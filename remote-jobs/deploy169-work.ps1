$ErrorActionPreference = "Continue"
# 1) sync C:\ai\makepad-work to origin/work  2) spawn a detached release build
#    of libs/asset/ai that installs the exe (does NOT restart the service).
$overlay = "C:\Users\playe\makepad"
$dest = "C:\ai\makepad-work"
git -C $overlay fetch --no-tags origin work 2>&1 | Out-Null
git -C $dest fetch --no-tags origin work 2>&1 | Out-Null
git -C $dest checkout --detach origin/work 2>&1 | Out-Null
Write-Output "HEAD=$(git -C $dest rev-parse HEAD)"
Write-Output "log=$(git -C $dest log -1 --oneline)"

$buildScript = @'
$ErrorActionPreference = "Continue"
$outDir = "C:\ai\makepad-asset-ai"
$src = "C:\ai\makepad-work"
$crate = Join-Path $src "libs\asset\ai"
$log = Join-Path $outDir "build-work.log"
Start-Transcript -Path $log -Force
try {
    $env:MAKEPAD_GGML_REQUIRE_CUDA = "1"
    $env:MAKEPAD_GGML_CUDA_ARCH = "120a"
    $env:CARGO_TERM_COLOR = "never"
    $env:CARGO_TERM_PROGRESS_WHEN = "never"
    Write-Host "HEAD=$(git -C $src rev-parse HEAD)"
    Set-Location $crate
    cargo build --release --bin makepad-asset-ai
    $code = $LASTEXITCODE
    Write-Host "build_exit=$code"
    if ($code -ne 0) { throw "cargo build failed: $code" }
    $exe = Join-Path $crate "target\release\makepad-asset-ai.exe"
    Copy-Item -Force $exe (Join-Path $outDir "makepad-asset-ai.exe")
    Get-Item (Join-Path $outDir "makepad-asset-ai.exe") | ForEach-Object { Write-Host "installed $($_.FullName) $($_.Length) $($_.LastWriteTime)" }
    Write-Host "BUILD_OK"
} catch {
    Write-Host "BUILD_FAIL $_"
} finally {
    Stop-Transcript
}
'@
$scriptPath = "C:\ai\makepad-asset-ai\build-work.ps1"
Set-Content -Path $scriptPath -Value $buildScript -Encoding ASCII
$ps = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
$proc = Start-Process -FilePath $ps -ArgumentList @("-NoProfile","-ExecutionPolicy","Bypass","-File",$scriptPath) -WindowStyle Hidden -PassThru
Write-Output "spawned pid=$($proc.Id)"
Start-Sleep -Seconds 3
if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) { Write-Output "alive=1" } else { Write-Output "alive=0" }
