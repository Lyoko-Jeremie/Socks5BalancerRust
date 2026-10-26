# M4 内存与并发基线实测（Windows PowerShell 5.1+）
#
# 目的：用真实数据校准 ADR-005 的内存模型（每连接 2×bufferSize + 固定开销），
#       验证 100MB 目标在 `bufferSize=2048` 下的可行口径。
#
# 方法：
#   对 bufferSize ∈ {2048, 4096, 8192} 各跑一轮：
#     1) 启动 balancer（listen 5700，首包超时 60s，保证测量窗口内连接不超时）
#     2) 采集"空闲 RSS"
#     3) 用 flood 建立 1000 条空闲连接并保持 30s
#     4) 轮询采集 balancer 工作集峰值（RSS）
#     5) 计算每连接增量 = (峰值 - 空闲) / 1000，与模型 2×bufferSize + 1.2~1.5KB 对比
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m4-memory.ps1
# 注意：本文件必须保存为 UTF-8 **带 BOM**。
#       Windows 单机动态端口约 16K，1000 连接可测；C10K 需 Linux（见 scripts/loadtest-linux.sh）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root "target\m4-logs"
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$summary = Join-Path $logs "memory-summary.txt"
"# M4 内存基线 $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Set-Content $summary -Encoding utf8
$procs = @()
$script:bgIndex = 0
$conns = 1000

function Kill-Tree($p) {
    if ($p -and -not $p.HasExited) { & taskkill /T /F /PID $p.Id 2>&1 | Out-Null }
}

function Cleanup {
    foreach ($p in $procs) { Kill-Tree $p }
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, flood -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
}

function Start-Bg([string]$exe, [string[]]$argList, [string]$outFile, [string]$errFile) {
    $script:bgIndex++
    $argLine = ($argList | ForEach-Object { if ($_ -match '[\s"]') { '"' + $_ + '"' } else { $_ } }) -join " "
    $bat = Join-Path $logs ("run-" + $script:bgIndex + ".bat")
    [System.IO.File]::WriteAllText($bat, "@echo off`r`n`"$exe`" $argLine > `"$outFile`" 2> `"$errFile`"`r`n",
        [System.Text.Encoding]::ASCII)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "cmd.exe"
    $psi.Arguments = '/c "' + $bat + '"'
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    return [System.Diagnostics.Process]::Start($psi)
}

function Wait-Port([int]$port, [int]$timeoutMs = 15000) {
    $deadline = (Get-Date).AddMilliseconds($timeoutMs)
    while ((Get-Date) -lt $deadline) {
        try { $c = [System.Net.Sockets.TcpClient]::new(); $c.Connect("127.0.0.1", $port); $c.Close(); return $true }
        catch { Start-Sleep -Milliseconds 100 }
    }
    return $false
}

function Get-Rss([int]$processId) {
    $p = Get-Process -Id $processId -ErrorAction SilentlyContinue
    if (-not $p) { return $null }
    return @{ WorkingSet = [int64]$p.WorkingSet64; Private = [int64]$p.PrivateMemorySize64; Handles = $p.HandleCount }
}

# Start-Bg 启动的是 cmd.exe 包装进程；真正的 balancer 是它的子进程。
# 注意 cmd 还会创建 conhost.exe 子进程，必须按进程名精确匹配，否则会采到 conhost。
function Find-Child([int]$parentPid) {
    $children = Get-CimInstance Win32_Process -Filter "ParentProcessId = $parentPid" -ErrorAction SilentlyContinue
    foreach ($c in $children) {
        if ($c.Name -eq "Socks5BalancerRust.exe") { return [int]$c.ProcessId }
    }
    $named = Get-Process -Name Socks5BalancerRust -ErrorAction SilentlyContinue |
        Sort-Object StartTime | Select-Object -Last 1
    if ($named) { return [int]$named.Id }
    return $parentPid
}

function MB([int64]$bytes) { return [math]::Round($bytes / 1MB, 2) }

try {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, flood -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300

    Write-Host "== [0/3] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) { Get-Content $buildLog -Encoding utf8 -Tail 40; throw "release 构建失败" }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"
    $floodBin = Join-Path $root "target\release\examples\flood.exe"

    Write-Host "== [1/3] 启动 echo + mock 上游 =="
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    $procs += Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock.out") (Join-Path $logs "mock.err")
    foreach ($p in 6100, 6200) { if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" } }

    $results = @()
    $index = 0
    foreach ($size in @(2048, 4096, 8192)) {
        $index++
        Write-Host "== [2/3] bufferSize=$size（$index/3）：建立 $conns 条空闲连接并采峰值 =="

        $cfgPath = Join-Path $logs ("mem-" + $size + ".json")
        $cfgObj = [ordered]@{
            listenHost         = "127.0.0.1"
            listenPort         = 5700
            stateServerPort    = 0
            traditionTcpRelay  = $false
            bufferSize         = $size
            maxConnections     = 20000
            firstPacketTimeout = 60000
            connectTimeout     = 2000
            handshakeTimeout   = 10000
            logLevel           = "info"
            upstream           = @(
                [ordered]@{ name = "mockAuth"; host = "127.0.0.1"; port = 6200; authUser = "alice"; authPwd = "secret" }
            )
            healthCheck        = [ordered]@{
                tcp         = [ordered]@{ period = 5000; timeout = 2000; startDelay = 1000 }
                downBackoff = [ordered]@{ initial = 30000; multiplier = 2.0; max = 600000; jitter = 0.2 }
                probeJitter = 0.2
            }
        }
        $cfg = $cfgObj | ConvertTo-Json -Depth 8
        [System.IO.File]::WriteAllText($cfgPath, $cfg, [System.Text.UTF8Encoding]::new($false))

        $bal = Start-Bg $bin @("-c", $cfgPath) (Join-Path $logs ("bal-" + $size + ".out")) (Join-Path $logs ("bal-" + $size + ".err"))
        $procs += $bal
        if (-not (Wait-Port 5700)) { throw "balancer 5700 未就绪（bufferSize=$size）" }
        $balPid = Find-Child $bal.Id
        Start-Sleep -Seconds 3

        $idle = Get-Rss $balPid
        if (-not $idle) { throw "无法读取 balancer 进程信息" }

        $floodLog = Join-Path $logs ("flood-" + $size + ".out")
        $floodProc = Start-Bg $floodBin @("127.0.0.1:5700", "$conns", "30") $floodLog (Join-Path $logs ("flood-" + $size + ".err"))
        $procs += $floodProc

        # 峰值采样：覆盖建连期 + 稳定期（30 × 500ms = 15s）
        $peakWs = $idle.WorkingSet
        $peakPrivate = $idle.Private
        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 500
            $now = Get-Rss $balPid
            if ($now) {
                if ($now.WorkingSet -gt $peakWs) { $peakWs = $now.WorkingSet }
                if ($now.Private -gt $peakPrivate) { $peakPrivate = $now.Private }
            }
        }

        # 等 flood 结束并释放连接
        [void]$floodProc.WaitForExit(120000)
        Start-Sleep -Seconds 4
        $after = Get-Rss $balPid
        if (-not $after) { $after = $idle }

        $perConnWs = [math]::Round(($peakWs - $idle.WorkingSet) / $conns, 2)
        $perConnPrivate = [math]::Round(($peakPrivate - $idle.Private) / $conns, 2)
        # Windows 的 WorkingSet 会被工作集修剪/内存压缩低估（8192 档尤其明显），
        # 100MB 口径取两者较大值作为保守估计。
        $perConnEffective = [math]::Max($perConnWs, $perConnPrivate)
        $modelMin = 2 * $size + 1229
        $modelMax = 2 * $size + 1536

        $r = [pscustomobject]@{
            BufferSize      = $size
            IdleRssMB       = MB $idle.WorkingSet
            PeakRssMB       = MB $peakWs
            AfterRssMB      = MB $after.WorkingSet
            PerConnRssByte  = $perConnWs
            PerConnPrivByte = $perConnPrivate
            PerConnEffByte  = $perConnEffective
            ModelMinByte    = $modelMin
            ModelMaxByte    = $modelMax
            HandlesIdle     = $idle.Handles
        }
        $results += $r

        Write-Host ("  空闲 RSS={0} MB  峰值 RSS={1} MB  每连接 RSS={2} B / Private={3} B（模型 {4}~{5} B）" -f
            $r.IdleRssMB, $r.PeakRssMB, $r.PerConnRssByte, $r.PerConnPrivByte, $r.ModelMinByte, $r.ModelMaxByte)
        Add-Content $summary ("bufferSize={0} idle={1}MB peak={2}MB after={3}MB perConnRss={4}B perConnPrivate={5}B perConnEffective={6}B model={7}~{8}B" -f
            $r.BufferSize, $r.IdleRssMB, $r.PeakRssMB, $r.AfterRssMB, $r.PerConnRssByte, $r.PerConnPrivByte, $r.PerConnEffByte, $r.ModelMinByte, $r.ModelMaxByte)

        Kill-Tree $bal
        Start-Sleep -Milliseconds 800
    }

    Write-Host ""
    Write-Host "== [3/3] 汇总（1000 并发空闲连接） =="
    $results | Format-Table BufferSize, IdleRssMB, PeakRssMB, AfterRssMB, PerConnRssByte, PerConnPrivByte, PerConnEffByte, ModelMinByte, ModelMaxByte -AutoSize | Out-String | Write-Host

    # 10K / 100MB 口径推算（用 RSS 与 Private 的较大值，保守）
    Write-Host "== 推算（10,000 连接 / 100MB 目标） =="
    foreach ($r in $results) {
        $est10k = $r.IdleRssMB + ($r.PerConnEffByte * 10000 / 1MB)
        $fit = if ($est10k -le 100) { "达标" } else { "超出" }
        Write-Host ("  bufferSize={0}: 10K 推算 ≈ {1} MB（{2}）" -f $r.BufferSize, [math]::Round($est10k, 1), $fit)
        Add-Content $summary ("  10K 推算 bufferSize={0}: {1} MB ({2})" -f $r.BufferSize, [math]::Round($est10k, 1), $fit)
    }
    Add-Content $summary "（Windows 单机动态端口约 16K，实测上限 1000；C10K 需 Linux，见 scripts/loadtest-linux.sh）"

    Write-Host ""
    Write-Host "  明细: $summary"
    exit 0
} catch {
    Write-Host "实测失败: $_" -ForegroundColor Red
    exit 1
} finally {
    Cleanup
}
