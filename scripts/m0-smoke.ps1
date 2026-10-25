# M0 端到端冒烟验收脚本（Windows PowerShell 5.1+）
#
# 覆盖：
#   1) release 构建
#   2) 启动本地 echo 上游 + 负载均衡器
#   3) 主监听端口与 multiListen 端口的功能透传
#   4) 并发连接保持（默认 2000）+ 进程 RSS 采样
#   5) 连接上限与 bufferSize 配置生效（第二实例：bufferSize=2048, maxConnections=5）
#   6) 清理（taskkill /T 杀进程树）
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m0-smoke.ps1 [-Connections 2000] [-HoldSecs 5]
#
# 注意：本文件必须保存为 UTF-8 **带 BOM**——Windows PowerShell 5.1 只有在有 BOM 时才按 UTF-8 解析脚本，
# 否则中文会按 ANSI 解码导致语法错误。

param(
    [int]$Connections = 500,
    [int]$HoldSecs = 5
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$logs = Join-Path $root "target\m0-logs"
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$echoOut = Join-Path $logs "echo.out"
$balOut = Join-Path $logs "balancer.out"
$bal2Out = Join-Path $logs "balancer-limit.out"

$procs = @()

function Kill-Tree($p) {
    if ($p -and -not $p.HasExited) {
        & taskkill /T /F /PID $p.Id 2>&1 | Out-Null
    }
}

function Cleanup {
    foreach ($p in $procs) { Kill-Tree $p }
    # 兜底：按进程名清理本次测试可能残留的进程（不会影响 C++ 版 Socks5BalancerAsio）
    Get-Process -Name Socks5BalancerRust, echo_upstream, flood -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
}

# 每次启动前清理上一次异常中断留下的测试进程，避免端口占用导致误判
function Pre-Clean {
    Get-Process -Name Socks5BalancerRust, echo_upstream, flood -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300
}

# 用 .NET Process 启动（不用 Start-Process：PS5.1 在环境变量存在大小写重复键时会抛
# "Item has already been added. Key in dictionary"）。
# 通过临时 .bat 承载命令行与重定向，避免 cmd /c 的嵌套引号问题。
$script:bgIndex = 0
function Start-Bg([string]$exe, [string[]]$argList, [string]$outFile, [string]$errFile) {
    $script:bgIndex++
    $argLine = ($argList | ForEach-Object {
            if ($_ -match '[\s"]') { '"' + $_ + '"' } else { $_ }
        }) -join " "
    $bat = Join-Path $logs ("run-" + $script:bgIndex + ".bat")
    $content = "@echo off`r`n`"$exe`" $argLine > `"$outFile`" 2> `"$errFile`"`r`n"
    [System.IO.File]::WriteAllText($bat, $content, [System.Text.Encoding]::ASCII)

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
        try {
            $c = [System.Net.Sockets.TcpClient]::new()
            $c.Connect("127.0.0.1", $port)
            $c.Close()
            return $true
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    return $false
}

function Send-Echo([int]$port, [string]$payload) {
    $client = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
    try {
        $client.ReceiveTimeout = 5000
        $stream = $client.GetStream()
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($payload)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
        $buf = New-Object byte[] 4096
        $n = $stream.Read($buf, 0, $buf.Length)
        return [System.Text.Encoding]::UTF8.GetString($buf, 0, $n)
    } finally {
        $client.Close()
    }
}

try {
    Pre-Clean
    Write-Host "== [1/5] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Get-Content $buildLog -Encoding utf8 -Tail 40
        throw "release 构建失败"
    }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $floodBin = Join-Path $root "target\release\examples\flood.exe"

    Write-Host "== [2/5] 启动 echo 上游(6100) 与 balancer(5100/6610) =="
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") $echoOut (Join-Path $logs "echo.err")
    Start-Sleep -Milliseconds 300
    $procs += Start-Bg $bin @("-c", "config.json") $balOut (Join-Path $logs "balancer.err")

    if (-not (Wait-Port 5100)) { throw "balancer 未在 5100 监听" }
    if (-not (Wait-Port 6610)) { throw "balancer 未在 multiListen 6610 监听" }
    Start-Sleep -Milliseconds 300

    Write-Host "== [3/5] 功能透传（主端口 + multiListen） =="
    $r1 = Send-Echo 5100 "hello-m0-primary"
    $r2 = Send-Echo 6610 "hello-m0-multilisten"
    Write-Host "  5100 -> '$r1'"
    Write-Host "  6610 -> '$r2'"
    if ($r1 -ne "hello-m0-primary") { throw "主端口透传不正确: $r1" }
    if ($r2 -ne "hello-m0-multilisten") { throw "multiListen 透传不正确: $r2" }

    # 注意：Start-Bg 返回的是承载重定向的 cmd.exe；必须按进程名找真正的 balancer 来测 RSS。
    $balProc = Get-Process -Name Socks5BalancerRust -ErrorAction SilentlyContinue |
        Sort-Object StartTime | Select-Object -First 1
    if (-not $balProc) { throw "找不到 Socks5BalancerRust 进程" }
    $rssEmpty = [math]::Round($balProc.WorkingSet64 / 1MB, 2)
    $threads = $balProc.Threads.Count
    Write-Host "  空闲 RSS: $rssEmpty MB (线程数 $threads)"

    Write-Host "== [4/5] 并发连接保持 $Connections 条 / 保持 $HoldSecs 秒 =="
    $procs += Start-Bg $floodBin @("127.0.0.1:5100", "$Connections", "$HoldSecs", "0") `
        (Join-Path $logs "flood.out") (Join-Path $logs "flood.err")

    Start-Sleep -Seconds 4
    $balProc.Refresh()
    $rssBusy = [math]::Round($balProc.WorkingSet64 / 1MB, 2)
    $privateBusy = [math]::Round($balProc.PrivateMemorySize64 / 1MB, 2)
    $handles = $balProc.HandleCount
    $threadsBusy = $balProc.Threads.Count
    Write-Host "  并发峰值 RSS: $rssBusy MB (Private $privateBusy MB, HandleCount $handles, 线程数 $threadsBusy)"

    # 等待 flood 结束（最多等 hold+30s）
    $deadline = (Get-Date).AddSeconds($HoldSecs + 30)
    while ((Get-Date) -lt $deadline) {
        $floodLog = Get-Content (Join-Path $logs "flood.out") -Encoding utf8 -Raw -ErrorAction SilentlyContinue
        if ($floodLog -match "flood done") { break }
        Start-Sleep -Milliseconds 300
    }
    Start-Sleep -Milliseconds 500
    $floodText = Get-Content (Join-Path $logs "flood.out") -Encoding utf8 -Raw
    $floodText -split "`n" | Where-Object { $_.Trim() } | ForEach-Object { Write-Host "  $_" }
    $floodOk = 0; $floodFailed = 0
    if ($floodText -match "ok=(\d+) failed=(\d+)") { $floodOk = [int]$Matches[1]; $floodFailed = [int]$Matches[2] }
    $timeWait = (Get-NetTCPConnection -State TimeWait -ErrorAction SilentlyContinue | Measure-Object).Count
    Write-Host "  TIME_WAIT 端口数: $timeWait（本机动态端口 16384，压测规模受其限制）"
    if ($floodOk -lt [int]($Connections * 0.99)) { throw "并发连接成功率不足: ok=$floodOk failed=$floodFailed（多为本机 TIME_WAIT/端口耗尽，可减少 -Connections 或等待回收后重试）" }

    Start-Sleep -Seconds 5
    $balProc.Refresh()
    $rssAfter = [math]::Round($balProc.WorkingSet64 / 1MB, 2)
    Write-Host "  全部连接释放 5s 后 RSS: $rssAfter MB"

    Write-Host "== [5/5] 第二实例：workerThreads=2 + bufferSize=2048 + maxConnections=5 =="
    $limitCfg = Join-Path $logs "config-limit.json"
    @"
{
  "listenHost": "127.0.0.1",
  "listenPort": 5200,
  "traditionTcpRelay": true,
  "workerThreads": 2,
  "bufferSize": 2048,
  "maxConnections": 5,
  "logLevel": "debug",
  "upstream": [ { "name": "localEcho", "host": "127.0.0.1", "port": 6100 } ]
}
"@ | Set-Content -Path $limitCfg -Encoding utf8
    $procs += Start-Bg $bin @("-c", $limitCfg) $bal2Out (Join-Path $logs "balancer-limit.err")
    if (-not (Wait-Port 5200)) { throw "第二实例未在 5200 监听" }
    Start-Sleep -Milliseconds 400

    $r3 = Send-Echo 5200 "limit-check"
    if ($r3 -ne "limit-check") { throw "第二实例透传不正确: $r3" }

    # 打满 5 条连接后继续建连，期望日志出现"拒绝连接"
    $clients = @()
    for ($i = 0; $i -lt 12; $i++) {
        try {
            $c = [System.Net.Sockets.TcpClient]::new("127.0.0.1", 5200)
            $clients += $c
        } catch { }
    }
    Start-Sleep -Milliseconds 800
    foreach ($c in $clients) { $c.Close() }
    Start-Sleep -Milliseconds 400

    $limitLog = Get-Content $bal2Out -Encoding utf8 -Raw
    $workerOk = $limitLog -match "workers=2"
    $bal2Proc = Get-Process -Name Socks5BalancerRust -ErrorAction SilentlyContinue |
        Sort-Object StartTime -Descending | Select-Object -First 1
    $rss2 = if ($bal2Proc) { [math]::Round($bal2Proc.WorkingSet64 / 1MB, 2) } else { -1 }
    Write-Host "  日志确认 workerThreads=2: $workerOk（第二实例空闲 RSS $rss2 MB）"
    if (-not $workerOk) { throw "workerThreads=2 配置未生效" }
    $configOk = $limitLog -match "buffer_size=2048"
    $limitHit = $limitLog -match "拒绝连接"
    Write-Host "  日志确认 bufferSize=2048: $configOk"
    Write-Host "  日志确认 maxConnections 生效: $limitHit"
    if (-not $configOk) { throw "bufferSize 配置未生效" }
    if (-not $limitHit) { throw "maxConnections 上限未触发" }

    Write-Host ""
    Write-Host "== 验收结论 =="
    Write-Host "  功能透传:         PASS"
    Write-Host "  multiListen:      PASS"
    Write-Host "  并发连接保持:     PASS (ok=$floodOk/$Connections, TIME_WAIT=$timeWait)"
    Write-Host "  bufferSize 生效:  PASS"
    Write-Host "  maxConnections:   PASS"
    Write-Host "  线程数可配:       PASS"
    Write-Host "  空闲 RSS:         $rssEmpty MB"
    Write-Host "  并发峰值 RSS:     $rssBusy MB"
    Write-Host "  释放 5s 后 RSS:   $rssAfter MB"
    Write-Host "  2 线程实例 RSS:   $rss2 MB"
    Write-Host "  日志目录:         $logs"
} catch {
    Write-Host "验收失败: $_" -ForegroundColor Red
    Write-Host "---- balancer 日志尾部 ----"
    if (Test-Path $balOut) { Get-Content $balOut -Encoding utf8 -Tail 40 }
    if (Test-Path $bal2Out) { Get-Content $bal2Out -Encoding utf8 -Tail 20 }
    exit 1
} finally {
    Cleanup
}
