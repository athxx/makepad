$ErrorActionPreference = "Stop"
$src = "C:\ai\makepad-work\libs\asset\ai\target\release\makepad-asset-ai.exe"
$destDir = "C:\ai\makepad-asset-ai"
$dest = Join-Path $destDir "makepad-asset-ai.exe"
$cache = "C:\ai\asset_node_cache"
$port = 8123
if (-not (Test-Path $src)) { Write-Output "MISSING $src"; exit 1 }
Get-Item $src | ForEach-Object { Write-Output "built $($_.FullName) $($_.Length) $($_.LastWriteTime)" }

Get-CimInstance Win32_Process |
    Where-Object { $_.Name -match "makepad-asset-ai|makepad-ai-content" } |
    ForEach-Object {
        Write-Output "stop pid=$($_.ProcessId) $($_.Name)"
        Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }
Start-Sleep -Seconds 3
Copy-Item -Force $src $dest
Get-Item $dest | ForEach-Object { Write-Output "installed $($_.FullName) $($_.Length) $($_.LastWriteTime)" }

$log = Join-Path $destDir "service.log"
$ps = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
# The service uses its EMBEDDED registry (the box-local
# aicontent-registry.json was a stale snapshot that hid newer entries).
$regArg = ""
$inner = @"
`$env:MAKEPAD_ASSET_AI_PORT = '$port'
`$env:MAKEPAD_ASSET_AI_FLEET = 'gen'
& '$dest' --port $port --host 0.0.0.0 --fleet gen --cache-dir '$cache' $regArg *>> '$log'
"@
Set-Content -Path (Join-Path $destDir "run-service.ps1") -Value $inner -Encoding ASCII
$proc = Start-Process -FilePath $ps -ArgumentList @("-NoProfile","-ExecutionPolicy","Bypass","-File",(Join-Path $destDir "run-service.ps1")) -WindowStyle Hidden -PassThru
Write-Output "started pid=$($proc.Id)"

$ok = $false
foreach ($i in 1..30) {
    Start-Sleep -Seconds 2
    try {
        $h = (Invoke-WebRequest -Uri "http://127.0.0.1:$port/health" -UseBasicParsing -TimeoutSec 3).Content
        Write-Output "health=$h"
        $ok = $true
        break
    } catch {
        Write-Output "wait $i"
    }
}
if (-not $ok) {
    Write-Output "NO HEALTH"
    if (Test-Path $log) { Get-Content $log -Tail 50 }
    exit 1
}
Write-Output "START_OK"
