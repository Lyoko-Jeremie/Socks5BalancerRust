# M1.5 TLS + HTTP readiness 探测验收（Windows PowerShell 5.1+）
#
# 使用**本地**自签 HTTPS 服务作为探测目标，不依赖外网、可复现：
#   1) 本地 TLS 目标（127.0.0.1:18443，自签证书，客户端通过 caFile 信任）→ readiness 成功
#   2) 客户端经该 balancer 正常转发
#   3) 目标不可解析（no-such-host.invalid）→ liveness 通但 readiness 失败 → 上游不可用（拒绝 CONNECT）
#   4) 停掉上游 → TCP 探测失败 → 快速失败；重启后恢复
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-tls.ps1
# 注意：本文件必须保存为 UTF-8 **带 BOM**。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root "target\m1-tls-logs"
New-Item -ItemType Directory -Force -Path $logs | Out-Null
$procs = @()
$script:bgIndex = 0
$script:pass = 0
$script:fail = 0

function Kill-Tree($p) {
    if ($p -and -not $p.HasExited) { & taskkill /T /F /PID $p.Id 2>&1 | Out-Null }
}

function Cleanup {
    foreach ($p in $procs) { Kill-Tree $p }
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, tls_echo_server -ErrorAction SilentlyContinue |
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

function Run-Client([int]$port, [string]$payload) {
    $script:bgIndex++
    $outFile = Join-Path $logs ("cli-" + $script:bgIndex + ".out")
    $errFile = Join-Path $logs ("cli-" + $script:bgIndex + ".err")
    $bat = Join-Path $logs ("cli-" + $script:bgIndex + ".bat")
    $exe = Join-Path $root "target\release\examples\socks5_client.exe"
    [System.IO.File]::WriteAllText($bat,
        "@echo off`r`n`"$exe`" 127.0.0.1:$port 127.0.0.1:6100 `"$payload`" --auth alice:secret > `"$outFile`" 2> `"$errFile`"`r`n",
        [System.Text.Encoding]::ASCII)
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
        try { $c = [System.Net.Sockets.TcpClient]::new(); $c.Connect("127.0.0.1", $port); $c.Close(); return $true }
        catch { Start-Sleep -Milliseconds 100 }
    }
    return $false
}

function Check([string]$name, [bool]$ok, [string]$detail = "") {
    if ($ok) { $script:pass++; Write-Host ("  [PASS] " + $name + $(if ($detail) { "  ($detail)" } else { "" })) }
    else { $script:fail++; Write-Host ("  [FAIL] " + $name + "  $detail") -ForegroundColor Red }
}

function Wait-ClientOk([int]$port, [string]$payload, [int]$timeoutSec = 20) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    $last = $null
    while ((Get-Date) -lt $deadline) {
        $r = Run-Client $port $payload
        $last = $r
        if ($r.Code -eq 0 -and $r.Out -match "OK $payload") { return $r }
        Start-Sleep -Milliseconds 700
    }
    return $last
}

$mockProc = $null
try {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, tls_echo_server -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300

    Write-Host "== [0/5] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) { Get-Content $buildLog -Encoding utf8 -Tail 40; throw "release 构建失败" }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"
    $tlsBin = Join-Path $root "target\release\examples\tls_echo_server.exe"
    $certPath = Join-Path $logs "cert.pem"
    if (Test-Path $certPath) { Remove-Item $certPath -Force }

    Write-Host "== [1/5] 启动本地 TLS 目标 + echo + mock 上游 + 两个 balancer =="
    # 本地 HTTPS 目标：生成自签证书并监听 18443
    $procs += Start-Bg $tlsBin @("127.0.0.1:18443", $certPath) (Join-Path $logs "tls.out") (Join-Path $logs "tls.err")
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    $mockProc = Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock.out") (Join-Path $logs "mock.err")
    $procs += $mockProc
    foreach ($p in 18443, 6100, 6200) {
        if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" }
    }
    if (-not (Test-Path $certPath)) { throw "自签证书未生成: $certPath" }
    Start-Sleep -Milliseconds 300
    # 5400：目标为本地 TLS 服务（caFile 信任自签证书）；5401：目标不可解析
    $procs += Start-Bg $bin @("-c", "configs\m1-tls-local.json") (Join-Path $logs "bal-good.out") (Join-Path $logs "bal-good.err")
    $procs += Start-Bg $bin @("-c", "configs\m1-tls-bad-target.json") (Join-Path $logs "bal-bad.out") (Join-Path $logs "bal-bad.err")
    foreach ($p in 5400, 5401) {
        if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" }
    }

    Write-Host "== [2/5] 本地 TLS 目标：readiness 探测应成功 =="
    $r = Wait-ClientOk 5400 "tls-local"
    Check "TLS+HTTP 探测通过后转发成功" ($r.Code -eq 0 -and $r.Out -match "OK tls-local") ("code=$($r.Code) err=$($r.Err)")
    $goodLog = Get-Content (Join-Path $logs "bal-good.out") -Encoding utf8 -Raw
    Check "日志记录 TLS/HTTP 探测启用" ($goodLog -match "TLS/HTTP readiness 探测已启用") ""
    Check "日志记录 TLS/HTTP 探测成功" ($goodLog -match "TLS/HTTP 探测成功") ""
    $tlsLog = Get-Content (Join-Path $logs "tls.out") -Encoding utf8 -Raw
    Check "本地 TLS 服务收到探测请求" ($tlsLog -match "GET / HTTP") ""

    Write-Host "== [3/5] 坏目标：liveness 通但 readiness 应失败 =="
    $badOk = $false
    $r = $null
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline) {
        $badLog = Get-Content (Join-Path $logs "bal-bad.out") -Encoding utf8 -Raw
        $r = Run-Client 5401 "tls-bad"
        if ($r.Code -ne 0 -and ($r.Err -match "General SOCKS server failure" -or $r.Err -match "Host unreachable")) { $badOk = $true }
        if ($badLog -match "readiness 探测失败") { break }
        Start-Sleep -Milliseconds 700
    }
    $badLog = Get-Content (Join-Path $logs "bal-bad.out") -Encoding utf8 -Raw
    Check "坏目标导致 readiness 失败日志" ($badLog -match "readiness 探测失败") ""
    Check "坏目标时上游不可用（拒绝 CONNECT）" $badOk ("code=$($r.Code) err=$($r.Err)")

    Write-Host "== [4/5] 停掉上游：TCP 探测失败 → 快速失败 =="
    Kill-Tree $mockProc
    Start-Sleep -Milliseconds 1600
    $r = Run-Client 5400 "tls-down"
    Check "上游 DOWN 时快速失败" ($r.Code -ne 0) ("code=$($r.Code) err=$($r.Err)")
    $goodLog = Get-Content (Join-Path $logs "bal-good.out") -Encoding utf8 -Raw
    Check "日志记录 TCP 探活失败" ($goodLog -match "TCP 探活失败") ""

    Write-Host "== [5/5] 重启上游：应自动恢复 =="
    $mockProc = Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock2.out") (Join-Path $logs "mock2.err")
    $procs += $mockProc
    if (-not (Wait-Port 6200)) { throw "重启后 6200 未就绪" }
    $r = Wait-ClientOk 5400 "tls-recover" 30
    Check "恢复后转发成功" ($r.Code -eq 0 -and $r.Out -match "OK tls-recover") ("code=$($r.Code) err=$($r.Err)")

    Write-Host ""
    Write-Host "== 验收结论 =="
    Write-Host "  PASS=$script:pass FAIL=$script:fail"
    Write-Host "  日志目录: $logs"
    if ($script:fail -gt 0) {
        Get-Content (Join-Path $logs "bal-good.out") -Encoding utf8 -Tail 60
        Get-Content (Join-Path $logs "bal-bad.out") -Encoding utf8 -Tail 60
        exit 1
    }
    exit 0
} catch {
    Write-Host "验收失败: $_" -ForegroundColor Red
    Get-Content (Join-Path $logs "bal-good.out") -Encoding utf8 -Tail 60 -ErrorAction SilentlyContinue
    Get-Content (Join-Path $logs "bal-bad.out") -Encoding utf8 -Tail 60 -ErrorAction SilentlyContinue
    exit 1
} finally {
    Cleanup
}
