# M1 SOCKS5 端到端验收脚本（Windows PowerShell 5.1+）
#
# 覆盖：
#   1) 无认证入站 + 带认证上游（alice:secret）→ 回显正确
#   2) multiListen 端口透传
#   3) 域名目标（由上游解析）
#   4) 入站认证成功
#   5) 入站认证失败（密码错误 → 01 01）
#   6) 客户端未提供认证方法（→ 05 FF）
#   7) BIND 命令 → 05 07
#   8) 非 SOCKS5 首包（HTTP）→ 服务端关闭
#   9) 首包超时（firstPacketTimeout=5000）→ 服务端关闭
#  10) 上游不可达 → 客户端收到 Host unreachable（REP 0x04）
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-smoke.ps1
#
# 注意：本文件必须保存为 UTF-8 **带 BOM**（Windows PowerShell 5.1 的编码要求）。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$logs = Join-Path $root "target\m1-logs"
New-Item -ItemType Directory -Force -Path $logs | Out-Null

$procs = @()
$script:bgIndex = 0
$script:pass = 0
$script:fail = 0

function Kill-Tree($p) {
    if ($p -and -not $p.HasExited) {
        & taskkill /T /F /PID $p.Id 2>&1 | Out-Null
    }
}

function Cleanup {
    foreach ($p in $procs) { Kill-Tree $p }
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
}

function Pre-Clean {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 400
}

# 异步启动（.NET Process + .bat，避免 PS5.1 的 Start-Process 环境变量大小写重复 bug）
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

# 同步运行命令，返回 @{ Code; Out; Err }
function Run-Sync([string]$exe, [string[]]$argList) {
    $script:bgIndex++
    $argLine = ($argList | ForEach-Object {
            if ($_ -match '[\s"]') { '"' + $_ + '"' } else { $_ }
        }) -join " "
    $outFile = Join-Path $logs ("sync-" + $script:bgIndex + ".out")
    $errFile = Join-Path $logs ("sync-" + $script:bgIndex + ".err")
    $bat = Join-Path $logs ("sync-" + $script:bgIndex + ".bat")
    $content = "@echo off`r`n`"$exe`" $argLine > `"$outFile`" 2> `"$errFile`"`r`n"
    [System.IO.File]::WriteAllText($bat, $content, [System.Text.Encoding]::ASCII)
    cmd /c "`"$bat`"" | Out-Null
    return @{
        Code = $LASTEXITCODE
        Out  = (Get-Content $outFile -Encoding utf8 -Raw -ErrorAction SilentlyContinue)
        Err  = (Get-Content $errFile -Encoding utf8 -Raw -ErrorAction SilentlyContinue)
    }
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

function Check([string]$name, [bool]$ok, [string]$detail = "") {
    if ($ok) {
        $script:pass++
        Write-Host ("  [PASS] " + $name + $(if ($detail) { "  ($detail)" } else { "" }))
    } else {
        $script:fail++
        Write-Host ("  [FAIL] " + $name + "  $detail") -ForegroundColor Red
    }
}

function Socks-Bind-Probe([int]$port) {
    $c = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
    try {
        $c.ReceiveTimeout = 4000
        $s = $c.GetStream()
        $s.Write([byte[]](0x05, 0x01, 0x00), 0, 3)
        $m = New-Object byte[] 2
        [void]$s.Read($m, 0, 2)
        if ($m[0] -ne 0x05 -or $m[1] -ne 0x00) { return "bad-method" }
        $s.Write([byte[]](0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0, 0), 0, 10)
        $r = New-Object byte[] 10
        $got = 0
        try {
            while ($got -lt 10) {
                $n = $s.Read($r, $got, 10 - $got)
                if ($n -le 0) { break }
                $got += $n
            }
        } catch {
            # 服务端写回应答后立即关闭可能触发 RST，已读到的字节仍然有效
        }
        if ($got -lt 2) { return "short-reply" }
        return ("05-{0:X2}" -f $r[1])
    } finally {
        $c.Close()
    }
}

function Closed-Probe([int]$port, [byte[]]$payload, [int]$timeoutMs = 8000) {
    $c = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
    try {
        $c.ReceiveTimeout = $timeoutMs
        $s = $c.GetStream()
        $s.Write($payload, 0, $payload.Length)
        $buf = New-Object byte[] 16
        try {
            $n = $s.Read($buf, 0, 16)
            return ($n -eq 0)
        } catch {
            return $true  # 被 RST 关闭也算"服务端已断开"
        }
    } finally {
        $c.Close()
    }
}

try {
    Pre-Clean
    Write-Host "== [0/3] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Get-Content $buildLog -Encoding utf8 -Tail 40
        throw "release 构建失败"
    }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"
    $clientBin = Join-Path $root "target\release\examples\socks5_client.exe"

    Write-Host "== [1/3] 启动测试拓扑 =="
    # echo 目标
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    # mock 上游：6200 要求认证，6210 无认证
    $procs += Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock-auth.out") (Join-Path $logs "mock-auth.err")
    $procs += Start-Bg $mockBin @("127.0.0.1:6210") (Join-Path $logs "mock-noauth.out") (Join-Path $logs "mock-noauth.err")
    Start-Sleep -Milliseconds 400
    # balancer：5100 入站无认证（上游 6200 需要认证）；5200 入站认证（上游 6210 无认证）
    $procs += Start-Bg $bin @("-c", "configs\m1-inbound-noauth.json") (Join-Path $logs "bal-noauth.out") (Join-Path $logs "bal-noauth.err")
    $procs += Start-Bg $bin @("-c", "configs\m1-inbound-auth.json") (Join-Path $logs "bal-auth.out") (Join-Path $logs "bal-auth.err")

    foreach ($p in 6100, 6200, 6210, 5100, 6610, 5200) {
        if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" }
    }
    Start-Sleep -Milliseconds 400
    Write-Host "  拓扑就绪：echo=6100 mock-auth=6200 mock-noauth=6210 balancer=5100/6610/5200"

    Write-Host "== [2/3] 正向链路 =="
    $r = Run-Sync $clientBin @("127.0.0.1:5100", "127.0.0.1:6100", "hello-m1-noauth")
    Check "无认证入站 + 带认证上游" ($r.Code -eq 0 -and $r.Out -match "OK hello-m1-noauth") ("exit=$($r.Code) out=$($r.Out) err=$($r.Err)")

    $r = Run-Sync $clientBin @("127.0.0.1:6610", "127.0.0.1:6100", "hello-m1-multilisten")
    Check "multiListen 透传" ($r.Code -eq 0 -and $r.Out -match "OK hello-m1-multilisten") ("exit=$($r.Code)")

    $r = Run-Sync $clientBin @("127.0.0.1:5100", "localhost:6100", "hello-m1-domain")
    Check "域名目标（上游解析）" ($r.Code -eq 0 -and $r.Out -match "OK hello-m1-domain") ("exit=$($r.Code) err=$($r.Err)")

    $r = Run-Sync $clientBin @("127.0.0.1:5200", "127.0.0.1:6100", "hello-m1-auth", "--auth", "alice:secret")
    Check "入站认证成功" ($r.Code -eq 0 -and $r.Out -match "OK hello-m1-auth") ("exit=$($r.Code) err=$($r.Err)")

    Write-Host "== [3/3] 失败路径与协议边界 =="
    $r = Run-Sync $clientBin @("127.0.0.1:5200", "127.0.0.1:6100", "x", "--auth", "alice:wrong")
    Check "入站认证失败（01 01）" ($r.Code -ne 0 -and $r.Err -match "Password auth failure") ("code=$($r.Code) err=$($r.Err)")

    $r = Run-Sync $clientBin @("127.0.0.1:5200", "127.0.0.1:6100", "x")
    Check "无可用认证方法（05 FF）" ($r.Code -ne 0 -and $r.Err -match "No acceptable auth methods") ("code=$($r.Code) err=$($r.Err)")

    $bind = Socks-Bind-Probe 5100
    Check "BIND 命令回 05 07" ($bind -eq "05-07") ("实际 $bind")

    $closed = Closed-Probe 5100 ([System.Text.Encoding]::ASCII.GetBytes("GET / HTTP/1.1`r`nHost: x`r`n`r`n"))
    Check "非 SOCKS5 首包被断开" $closed ""

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $closed = Closed-Probe 5100 ([byte[]]@()) 15000
    $sw.Stop()
    $elapsed = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    Check "首包超时断开（约 5s）" ($closed -and $elapsed -ge 4.0 -and $elapsed -le 8.0) ("耗时 ${elapsed}s")

    # 上游不可达：停掉 6210 后 5200 应回 Host unreachable
    $noauthProc = Get-Process -Name mock_socks5_upstream -ErrorAction SilentlyContinue |
        Where-Object { $_.Id } | Sort-Object StartTime | Select-Object -Last 1
    if ($noauthProc) { Kill-Tree $noauthProc; Start-Sleep -Milliseconds 400 }
    $r = Run-Sync $clientBin @("127.0.0.1:5200", "127.0.0.1:6100", "x", "--auth", "alice:secret")
    Check "上游不可达回 Host unreachable（REP 0x04）" ($r.Code -ne 0 -and $r.Err -match "Host unreachable") ("code=$($r.Code) err=$($r.Err)")

    Write-Host ""
    Write-Host "== 验收结论 =="
    Write-Host "  PASS=$script:pass FAIL=$script:fail"
    Write-Host "  日志目录: $logs"
    if ($script:fail -gt 0) {
        Get-Content (Join-Path $logs "bal-noauth.out") -Encoding utf8 -Tail 30
        Get-Content (Join-Path $logs "bal-auth.out") -Encoding utf8 -Tail 30
        exit 1
    }
    exit 0
} catch {
    Write-Host "验收失败: $_" -ForegroundColor Red
    Get-Content (Join-Path $logs "bal-noauth.out") -Encoding utf8 -Tail 40 -ErrorAction SilentlyContinue
    Get-Content (Join-Path $logs "bal-auth.out") -Encoding utf8 -Tail 40 -ErrorAction SilentlyContinue
    exit 1
} finally {
    Cleanup
}
