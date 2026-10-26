# M2 混合端口协议验收（Windows PowerShell 5.1+）
#
# 覆盖（同一监听端口 5500 上同时服务 SOCKS5 / SOCKS4 / HTTP）：
#   1) SOCKS5 回归（tokio-socks 客户端）
#   2) SOCKS4 标准 IP 目标 CONNECT → 00 5A + 回显
#   3) SOCKS4a 域名目标 CONNECT → 00 5A + 回显
#   4) SOCKS4 BIND 命令 → 00 5B
#   5) HTTP CONNECT → 200 Connection established + 回显
#   6) HTTP CONNECT + early data（请求头与数据粘包）→ 数据必须回放给目标
#   7) HTTP absolute-form 正向代理（curl 经代理）→ 目标收到 origin-form（/m2-forward）
#   8) HTTP 非 absolute-form 请求 → 400
#   9) disableSocks4=true 的实例 → SOCKS4 回 00 5B
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m2-smoke.ps1
# 注意：本文件必须保存为 UTF-8 **带 BOM**。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root "target\m2-logs"
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
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, http_target -ErrorAction SilentlyContinue |
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

function Run-Cmd([string]$cmdline) {
    $script:bgIndex++
    $bat = Join-Path $logs ("cmd-" + $script:bgIndex + ".bat")
    [System.IO.File]::WriteAllText($bat, "@echo off`r`n$cmdline`r`n", [System.Text.Encoding]::ASCII)
    cmd /c "`"$bat`"" | Out-Null
    return $LASTEXITCODE
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

function New-Conn([int]$port) {
    $c = [System.Net.Sockets.TcpClient]::new("127.0.0.1", $port)
    $c.ReceiveTimeout = 6000
    return $c
}

function Bytes-Ascii([string]$s) { return [System.Text.Encoding]::ASCII.GetBytes($s) }

function Read-Exact($stream, [int]$n) {
    $out = New-Object System.Collections.Generic.List[byte]
    $buf = New-Object byte[] 512
    while ($out.Count -lt $n) {
        $want = [Math]::Min(512, $n - $out.Count)
        $r = $stream.Read($buf, 0, $want)
        if ($r -le 0) { break }
        for ($i = 0; $i -lt $r; $i++) { $out.Add($buf[$i]) }
    }
    return $out.ToArray()
}

function Read-Until($stream, [string]$marker, [int]$maxBytes = 65536) {
    $out = New-Object System.Collections.Generic.List[byte]
    $buf = New-Object byte[] 512
    $text = ""
    while ($out.Count -lt $maxBytes) {
        $r = $stream.Read($buf, 0, 512)
        if ($r -le 0) { break }
        for ($i = 0; $i -lt $r; $i++) { $out.Add($buf[$i]) }
        $text = [System.Text.Encoding]::ASCII.GetString($out.ToArray())
        if ($text.Contains($marker)) { break }
    }
    return @{ Bytes = $out.ToArray(); Text = $text }
}

# 发送 SOCKS4/4a 请求并读 8 字节应答，返回 rep 字节（0x5A 成功）
function Test-Socks4([int]$listenPort, [byte]$cmd, [string]$targetIp, [int]$targetPort, [string]$userid, [string]$domain) {
    $c = New-Conn $listenPort
    try {
        $s = $c.GetStream()
        $ip = [System.Net.IPAddress]::Parse($targetIp).GetAddressBytes()
        $frame = New-Object System.Collections.Generic.List[byte]
        $frame.Add(0x04); $frame.Add($cmd)
        $frame.Add([byte](($targetPort -shr 8) -band 0xFF)); $frame.Add([byte]($targetPort -band 0xFF))
        foreach ($b in $ip) { $frame.Add($b) }
        foreach ($b in (Bytes-Ascii $userid)) { $frame.Add($b) }
        $frame.Add(0x00)
        if ($domain) {
            foreach ($b in (Bytes-Ascii $domain)) { $frame.Add($b) }
            $frame.Add(0x00)
        }
        $bytes = $frame.ToArray()
        $s.Write($bytes, 0, $bytes.Length)
        $reply = Read-Exact $s 8
        $rep = -1
        if ($reply.Length -ge 2) { $rep = [int]$reply[1] }
        return @{ Rep = $rep; Stream = $s; Client = $c }
    } catch {
        $c.Close()
        throw
    }
}

function Test-Socks4Echo([int]$listenPort, [string]$targetIp, [int]$targetPort, [string]$userid, [string]$domain, [string]$payload) {
    $r = Test-Socks4 $listenPort 0x01 $targetIp $targetPort $userid $domain
    try {
        if ($r.Rep -ne 0x5A) { return @{ Rep = $r.Rep; Echo = "" } }
        $p = Bytes-Ascii $payload
        $r.Stream.Write($p, 0, $p.Length)
        $echo = [System.Text.Encoding]::ASCII.GetString((Read-Exact $r.Stream $p.Length))
        return @{ Rep = $r.Rep; Echo = $echo }
    } finally {
        $r.Client.Close()
    }
}

function Test-HttpConnect([int]$listenPort, [string]$authority, [string]$payload, [bool]$pipeline) {
    $c = New-Conn $listenPort
    try {
        $s = $c.GetStream()
        $head = "CONNECT $authority HTTP/1.1`r`nHost: $authority`r`n`r`n"
        if ($pipeline) {
            $all = (Bytes-Ascii $head) + (Bytes-Ascii $payload)
            $s.Write($all, 0, $all.Length)
        } else {
            $h = Bytes-Ascii $head
            $s.Write($h, 0, $h.Length)
        }
        $resp = Read-Until $s "`r`n`r`n"
        $ok = $resp.Text -match "HTTP/1.1 200"
        if (-not $pipeline -and $ok) {
            $p = Bytes-Ascii $payload
            $s.Write($p, 0, $p.Length)
        }
        $echo = ""
        if ($ok) {
            $e = Read-Exact $s $payload.Length
            $echo = [System.Text.Encoding]::ASCII.GetString($e)
        }
        return @{ Ok = $ok; Echo = $echo; Response = $resp.Text }
    } finally {
        $c.Close()
    }
}

function Test-HttpForward([string]$proxy, [string]$url) {
    $script:bgIndex++
    $out = Join-Path $logs ("curl-" + $script:bgIndex + ".out")
    $err = Join-Path $logs ("curl-" + $script:bgIndex + ".err")
    $code = Run-Cmd "curl.exe -sS --proxy `"$proxy`" --max-time 10 `"$url`" > `"$out`" 2> `"$err`""
    return @{
        Code = $code
        Out  = (Get-Content $out -Encoding utf8 -Raw -ErrorAction SilentlyContinue)
        Err  = (Get-Content $err -Encoding utf8 -Raw -ErrorAction SilentlyContinue)
    }
}

try {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream, http_target -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300

    Write-Host "== [0/6] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) { Get-Content $buildLog -Encoding utf8 -Tail 40; throw "release 构建失败" }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"
    $httpTarget = Join-Path $root "target\release\examples\http_target.exe"
    $clientBin = Join-Path $root "target\release\examples\socks5_client.exe"

    Write-Host "== [1/6] 启动测试拓扑 =="
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    $procs += Start-Bg $httpTarget @("127.0.0.1:6101") (Join-Path $logs "http.out") (Join-Path $logs "http.err")
    $procs += Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock.out") (Join-Path $logs "mock.err")
    foreach ($p in 6100, 6101, 6200) { if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" } }
    Start-Sleep -Milliseconds 300
    $procs += Start-Bg $bin @("-c", "configs\m2-mixed.json") (Join-Path $logs "bal.out") (Join-Path $logs "bal.err")
    $procs += Start-Bg $bin @("-c", "configs\m2-disable-socks4.json") (Join-Path $logs "bal-nos4.out") (Join-Path $logs "bal-nos4.err")
    foreach ($p in 5500, 5501) { if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" } }
    Start-Sleep -Milliseconds 300

    Write-Host "== [2/6] SOCKS5 回归 =="
    $s5out = Join-Path $logs "s5.out"
    $code = Run-Cmd "`"$clientBin`" 127.0.0.1:5500 127.0.0.1:6100 `"s5-ok`" --auth alice:secret > `"$s5out`" 2>&1"
    $out = Get-Content $s5out -Encoding utf8 -Raw -ErrorAction SilentlyContinue
    Check "SOCKS5 经混合端口仍可用" ($code -eq 0 -and $out -match "OK s5-ok") ("out=$out")

    Write-Host "== [3/6] SOCKS4 / SOCKS4a =="
    $r4 = Test-Socks4Echo 5500 "127.0.0.1" 6100 "u" "" "hello-socks4"
    Check "SOCKS4 IP 目标 CONNECT" ($r4.Rep -eq 0x5A -and $r4.Echo -eq "hello-socks4") ("rep=0x{0:X2} echo={1}" -f $r4.Rep, $r4.Echo)

    $r4a = Test-Socks4Echo 5500 "0.0.0.1" 6100 "u" "localhost" "hello-socks4a"
    Check "SOCKS4a 域名目标 CONNECT" ($r4a.Rep -eq 0x5A -and $r4a.Echo -eq "hello-socks4a") ("rep=0x{0:X2} echo={1}" -f $r4a.Rep, $r4a.Echo)

    $rb = Test-Socks4 5500 0x02 "127.0.0.1" 6100 "u" ""
    $rb.Client.Close()
    Check "SOCKS4 BIND 命令回 5B" ($rb.Rep -eq 0x5B) ("rep=0x{0:X2}" -f $rb.Rep)

    Write-Host "== [4/6] HTTP CONNECT =="
    $hc = Test-HttpConnect 5500 "127.0.0.1:6100" "hello-http-connect" $false
    Check "HTTP CONNECT 200 + 回显" ($hc.Ok -and $hc.Echo -eq "hello-http-connect") ("echo=$($hc.Echo)")

    $hp = Test-HttpConnect 5500 "127.0.0.1:6100" "hello-early-data" $true
    Check "HTTP CONNECT 粘包 early data 回放" ($hp.Ok -and $hp.Echo -eq "hello-early-data") ("echo=$($hp.Echo)")

    Write-Host "== [5/6] HTTP absolute-form 正向代理 =="
    $fw = Test-HttpForward "http://127.0.0.1:5500" "http://127.0.0.1:6101/m2-forward"
    Check "forward 请求被改写为 origin-form" ($fw.Code -eq 0 -and $fw.Out -match "OK GET /m2-forward HTTP/1.1") ("code=$($fw.Code) out=$($fw.Out) err=$($fw.Err)")

    $c = New-Conn 5500
    $s = $c.GetStream()
    $req = Bytes-Ascii "GET /not-absolute HTTP/1.1`r`nHost: x`r`n`r`n"
    $s.Write($req, 0, $req.Length)
    $bad = Read-Until $s "`r`n`r`n"
    $c.Close()
    Check "非 absolute-form 回 400" ($bad.Text -match "HTTP/1.1 400") ("resp=$($bad.Text)")

    Write-Host "== [6/6] disableSocks4 生效 =="
    $r = Test-Socks4 5501 0x01 "127.0.0.1" 6100 "u" ""
    $r.Client.Close()
    Check "disableSocks4 回 5B" ($r.Rep -eq 0x5B) ("rep=0x{0:X2}" -f $r.Rep)

    Write-Host ""
    Write-Host "== 验收结论 =="
    Write-Host "  PASS=$script:pass FAIL=$script:fail"
    Write-Host "  日志目录: $logs"
    if ($script:fail -gt 0) {
        Get-Content (Join-Path $logs "bal.out") -Encoding utf8 -Tail 60
        exit 1
    }
    exit 0
} catch {
    Write-Host "验收失败: $_" -ForegroundColor Red
    Get-Content (Join-Path $logs "bal.out") -Encoding utf8 -Tail 60 -ErrorAction SilentlyContinue
    exit 1
} finally {
    Cleanup
}
