# M3 精简控制面验收（Windows PowerShell 5.1+）
#
# 覆盖（ADR-001：4 端点、无鉴权、默认回环、响应不含密码）：
#   1) GET /healthz → 200 ok
#   2) GET /api/v1/status → JSON（schema v1 / stats / health.total / listen）
#   3) GET /api/v1/upstreams → JSON 数组且不含密码/用户名/认证字段
#   4) 主服务转发可用（SOCKS5 经 5600）
#   5) POST control check_upstream → ok
#   6) POST control disable_upstream → ok，且此后客户端失败（无可用上游）
#   7) POST control enable_upstream → ok，客户端恢复
#   8) POST control set_rule(loop) → ok，status.selectRule 变为 loop
#   9) 未知命令（no_such_cmd）→ ok=false；close_all/close_conn（连接监测）→ ok=true
#  10) GET /nope → 404
#  11) 无 CORS 响应头
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m3-smoke.ps1
# 注意：本文件必须保存为 UTF-8 **带 BOM**。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
$logs = Join-Path $root "target\m3-logs"
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
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream -ErrorAction SilentlyContinue |
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

# 调控制面：返回 @{ Status; Head; Body }
function Invoke-Ctl([string]$method, [string]$path, [string]$json = "") {
    $script:bgIndex++
    $out = Join-Path $logs ("ctl-" + $script:bgIndex + ".out")
    $err = Join-Path $logs ("ctl-" + $script:bgIndex + ".err")
    $url = "http://127.0.0.1:5601$path"
    if ($json) {
        $jsonFile = Join-Path $logs ("req-" + $script:bgIndex + ".json")
        [System.IO.File]::WriteAllText($jsonFile, $json, [System.Text.UTF8Encoding]::new($false))
        $cmd = "curl.exe -sS -i --max-time 5 -X $method -H `"Content-Type: application/json`" --data-binary `"@$jsonFile`" `"$url`" > `"$out`" 2> `"$err`""
    } else {
        $cmd = "curl.exe -sS -i --max-time 5 -X $method `"$url`" > `"$out`" 2> `"$err`""
    }
    Run-Cmd $cmd | Out-Null
    $raw = Get-Content $out -Raw -Encoding utf8 -ErrorAction SilentlyContinue
    if (-not $raw) { $raw = "" }
    $parts = $raw -split "`r`n`r`n", 2
    $head = $parts[0]
    $body = if ($parts.Length -gt 1) { $parts[1] } else { "" }
    $status = 0
    if ($head -match "HTTP/1\.1 (\d{3})") { $status = [int]$Matches[1] }
    return @{ Status = $status; Head = $head; Body = $body.Trim() }
}

function Invoke-Client([int]$port, [string]$payload) {
    $script:bgIndex++
    $out = Join-Path $logs ("cli-" + $script:bgIndex + ".out")
    $err = Join-Path $logs ("cli-" + $script:bgIndex + ".err")
    $client = Join-Path $root "target\release\examples\socks5_client.exe"
    $cmd = "`"$client`" 127.0.0.1:$port 127.0.0.1:6100 `"$payload`" --auth alice:secret > `"$out`" 2> `"$err`""
    $code = Run-Cmd $cmd
    return @{
        Code = $code
        Out  = (Get-Content $out -Raw -Encoding utf8 -ErrorAction SilentlyContinue)
        Err  = (Get-Content $err -Raw -Encoding utf8 -ErrorAction SilentlyContinue)
    }
}

try {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300

    Write-Host "== [0/5] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) { Get-Content $buildLog -Encoding utf8 -Tail 40; throw "release 构建失败" }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"

    Write-Host "== [1/5] 启动 echo/mock/balancer =="
    $procs += Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    $procs += Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock.out") (Join-Path $logs "mock.err")
    foreach ($p in 6100, 6200) { if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" } }
    Start-Sleep -Milliseconds 300
    $procs += Start-Bg $bin @("-c", "configs\m3-control.json") (Join-Path $logs "bal.out") (Join-Path $logs "bal.err")
    foreach ($p in 5600, 5601) { if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" } }
    Start-Sleep -Milliseconds 500

    Write-Host "== [2/5] 只读端点 =="
    $hz = Invoke-Ctl "GET" "/healthz"
    Check "/healthz 返回 200 ok" ($hz.Status -eq 200 -and $hz.Body -eq "ok") ("status=$($hz.Status) body=$($hz.Body)")

    $st = Invoke-Ctl "GET" "/api/v1/status"
    $j = $null
    try { $j = $st.Body | ConvertFrom-Json } catch { }
    Check "/api/v1/status schema=v1" ($st.Status -eq 200 -and $j -and $j.schema -eq "v1") ("status=$($st.Status)")
    Check "status 含统计与健康摘要" ($j -and $null -ne $j.stats.accepted -and $j.health.total -ge 1) ("accepted=$($j.stats.accepted) total=$($j.health.total)")
    Check "status.listen 正确" ($j -and ($j.listen -contains "127.0.0.1:5600")) ("listen=$($j.listen -join ',')")

    $ups = Invoke-Ctl "GET" "/api/v1/upstreams"
    $arr = $null
    try { $arr = $ups.Body | ConvertFrom-Json } catch { }
    Check "/api/v1/upstreams 返回数组" ($ups.Status -eq 200 -and $arr -and $arr.Count -ge 1) ("status=$($ups.Status)")
    $upsText = $ups.Body
    Check "upstreams 不含密码/用户名/认证字段" (-not ($upsText -match "secret|alice|authUser|authPwd")) ("body=$upsText")

    Write-Host "== [3/5] 主服务转发 + 控制命令 =="
    $c1 = Invoke-Client 5600 "m3-before"
    Check "控制前主服务转发可用" ($c1.Code -eq 0 -and $c1.Out -match "OK m3-before") ("code=$($c1.Code)")

    $ck = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"check_upstream","args":{"index":0}}'
    Check "check_upstream ok" ($ck.Status -eq 200 -and ($ck.Body | ConvertFrom-Json).ok -eq $true) ("body=$($ck.Body)")

    $dis = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"disable_upstream","args":{"index":0}}'
    Check "disable_upstream ok" ($dis.Status -eq 200 -and ($dis.Body | ConvertFrom-Json).ok -eq $true) ("body=$($dis.Body)")
    Start-Sleep -Milliseconds 300
    $c2 = Invoke-Client 5600 "m3-disabled"
    Check "禁用后客户端失败（无可用上游）" ($c2.Code -ne 0) ("code=$($c2.Code) err=$($c2.Err)")

    $ena = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"enable_upstream","args":{"index":0}}'
    Check "enable_upstream ok" ($ena.Status -eq 200 -and ($ena.Body | ConvertFrom-Json).ok -eq $true) ("body=$($ena.Body)")
    Start-Sleep -Milliseconds 300
    $c3 = Invoke-Client 5600 "m3-resumed"
    Check "重新启用后转发恢复" ($c3.Code -eq 0 -and $c3.Out -match "OK m3-resumed") ("code=$($c3.Code)")

    $rule = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"set_rule","args":{"rule":"loop"}}'
    Check "set_rule ok" ($rule.Status -eq 200 -and ($rule.Body | ConvertFrom-Json).ok -eq $true) ("body=$($rule.Body)")
    $st2 = Invoke-Ctl "GET" "/api/v1/status"
    $j2 = $null
    try { $j2 = $st2.Body | ConvertFrom-Json } catch { }
    Check "status.selectRule 已切换为 loop" ($j2 -and $j2.selectRule -eq "loop") ("selectRule=$($j2.selectRule)")

    # 连接监测：明细/摘要端点 + close_all（默认开启明细）
    $conns = Invoke-Ctl "GET" "/api/v1/connections"
    $connsJson = $null
    try { $connsJson = $conns.Body | ConvertFrom-Json } catch { }
    Check "/api/v1/connections 返回 enabled/detail" ($conns.Status -eq 200 -and $connsJson.enabled -eq $true -and $connsJson.detail -eq $true) ("body=$($conns.Body)")
    $sum = Invoke-Ctl "GET" "/api/v1/connections/summary"
    $sumJson = $null
    try { $sumJson = $sum.Body | ConvertFrom-Json } catch { }
    Check "/api/v1/connections/summary 含三维度字段" ($sum.Status -eq 200 -and $null -ne $sumJson.total -and $null -ne $sumJson.byUpstream -and $null -ne $sumJson.byListen -and $null -ne $sumJson.byClient) ("body=$($sum.Body)")
    $ca = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"close_all"}'
    $caJson = $null
    try { $caJson = $ca.Body | ConvertFrom-Json } catch { }
    Check "close_all ok" ($ca.Status -eq 200 -and $caJson.ok -eq $true) ("body=$($ca.Body)")

    Write-Host "== [4/5] 错误路径与安全默认 =="
    $bad = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"no_such_cmd"}'
    $badJson = $null
    try { $badJson = $bad.Body | ConvertFrom-Json } catch { }
    Check "未知命令返回 ok=false" ($bad.Status -eq 200 -and $badJson.ok -eq $false) ("body=$($bad.Body)")

    $bad2 = Invoke-Ctl "POST" "/api/v1/control" '{"cmd":"close_conn"}'
    $bad2Json = $null
    try { $bad2Json = $bad2.Body | ConvertFrom-Json } catch { }
    Check "close_conn 缺参数返回 ok=false" ($bad2Json.ok -eq $false) ("body=$($bad2.Body)")

    $nf = Invoke-Ctl "GET" "/nope"
    Check "未知路径返回 404" ($nf.Status -eq 404) ("status=$($nf.Status)")

    $hd = Invoke-Ctl "GET" "/api/v1/status"
    Check "无 CORS 响应头" (-not ($hd.Head -match "(?i)access-control-")) ("head=$($hd.Head)")

    Write-Host "== [5/5] 审计日志 =="
    $balLog = Get-Content (Join-Path $logs "bal.out") -Raw -Encoding utf8
    Check "日志含控制面启动信息" ($balLog -match "控制面已就绪") ""
    Check "日志含控制命令审计" ($balLog -match "控制命令") ""

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
