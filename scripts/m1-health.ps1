# M1.4 健康检查端到端验收（Windows PowerShell 5.1+）
#
# 验证 ADR-002 的 fail-fast / 快速切换 / DOWN 指数退避 / 恢复：
#   1) 上游在线 → 客户端正常
#   2) 停掉上游 → 探测失败判 DOWN → 客户端快速失败（REP 0x01/0x04，不挂起）
#   3) 重启上游 → 退避后重新探测成功 → 客户端恢复
#
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/m1-health.ps1
# 注意：本文件必须保存为 UTF-8 **带 BOM**。

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$logs = Join-Path $root "target\m1-health-logs"
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

function Run-Client([string]$target = "127.0.0.1:6100", [string]$payload = "h") {
    $script:bgIndex++
    $outFile = Join-Path $logs ("cli-" + $script:bgIndex + ".out")
    $errFile = Join-Path $logs ("cli-" + $script:bgIndex + ".err")
    $bat = Join-Path $logs ("cli-" + $script:bgIndex + ".bat")
    $exe = Join-Path $root "target\release\examples\socks5_client.exe"
    $content = "@echo off`r`n`"$exe`" 127.0.0.1:5300 $target `"$payload`" --auth alice:secret > `"$outFile`" 2> `"$errFile`"`r`n"
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

$echoProc = $null
$mockProc = $null

try {
    Get-Process -Name Socks5BalancerRust, echo_upstream, mock_socks5_upstream -ErrorAction SilentlyContinue |
        ForEach-Object { Kill-Tree $_ }
    Start-Sleep -Milliseconds 300

    Write-Host "== [0/4] release 构建 =="
    $buildLog = Join-Path $logs "build.log"
    cmd /c "cargo build --release --bins --examples > `"$buildLog`" 2>&1"
    if ($LASTEXITCODE -ne 0) {
        Get-Content $buildLog -Encoding utf8 -Tail 40
        throw "release 构建失败"
    }

    $bin = Join-Path $root "target\release\Socks5BalancerRust.exe"
    $echoBin = Join-Path $root "target\release\examples\echo_upstream.exe"
    $mockBin = Join-Path $root "target\release\examples\mock_socks5_upstream.exe"

    Write-Host "== [1/4] 启动 echo + mock 上游 + balancer =="
    $echoProc = Start-Bg $echoBin @("127.0.0.1:6100") (Join-Path $logs "echo.out") (Join-Path $logs "echo.err")
    $procs += $echoProc
    $mockProc = Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock.out") (Join-Path $logs "mock.err")
    $procs += $mockProc
    Start-Sleep -Milliseconds 400
    $procs += Start-Bg $bin @("-c", "configs\m1-health.json") (Join-Path $logs "bal.out") (Join-Path $logs "bal.err")
    foreach ($p in 6100, 6200, 5300) {
        if (-not (Wait-Port $p)) { throw "端口 $p 未就绪" }
    }
    Start-Sleep -Milliseconds 600

    Write-Host "== [2/4] 上游在线：应正常 =="
    $r = Run-Client "127.0.0.1:6100" "before-down"
    Check "健康时转发成功" ($r.Code -eq 0 -and $r.Out -match "OK before-down") ("code=$($r.Code) err=$($r.Err)")

    Write-Host "== [3/4] 停掉上游：应快速失败（判 DOWN） =="
    Kill-Tree $mockProc
    Start-Sleep -Milliseconds 1600
    $r = Run-Client "127.0.0.1:6100" "during-down"
    $fastFail = $r.Code -ne 0 -and ($r.Err -match "General SOCKS server failure" -or $r.Err -match "Host unreachable")
    Check "上游 DOWN 时快速返回错误码" $fastFail ("code=$($r.Code) err=$($r.Err)")

    $balLog = Get-Content (Join-Path $logs "bal.out") -Encoding utf8 -Raw
    Check "日志记录 fail-fast DOWN" ($balLog -match "TCP 探活失败|上游探活失败") ""
    Check "日志记录数据面/探测降级" ($balLog -match "标记 DOWN" -or $balLog -match "标记为不可用") ""

    Write-Host "== [4/4] 重启上游：退避后应自动恢复 =="
    $mockProc = Start-Bg $mockBin @("127.0.0.1:6200", "alice:secret") (Join-Path $logs "mock2.out") (Join-Path $logs "mock2.err")
    $procs += $mockProc
    if (-not (Wait-Port 6200)) { throw "重启后 6200 未就绪" }
    Start-Sleep -Milliseconds 4500
    $r = Run-Client "127.0.0.1:6100" "after-recover"
    Check "恢复后转发成功" ($r.Code -eq 0 -and $r.Out -match "OK after-recover") ("code=$($r.Code) err=$($r.Err)")
    $balLog = Get-Content (Join-Path $logs "bal.out") -Encoding utf8 -Raw
    Check "日志记录恢复可用" ($balLog -match "liveness 恢复|upstream readiness 恢复|上游恢复可用") ""

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
