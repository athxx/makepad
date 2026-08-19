$ErrorActionPreference = "Continue"
$outDir = "C:\ai\makepad-asset-ai"
$src = "C:\ai\makepad-work"
$crate = Join-Path $src "libs\asset\ai"
$log = Join-Path $outDir "build-work.log"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
Start-Transcript -Path $log -Force
try {
    $env:MAKEPAD_GGML_REQUIRE_CUDA = "1"
    $env:MAKEPAD_GGML_CUDA_ARCH = "120a"
    $env:CARGO_TERM_COLOR = "never"
    $env:CARGO_TERM_PROGRESS_WHEN = "never"
    Write-Host "HEAD=$(git -C $src rev-parse HEAD)"
    Write-Host "log=$(git -C $src log -1 --oneline)"
    Write-Host "crate=$crate arch=$env:MAKEPAD_GGML_CUDA_ARCH"
    if (-not (Test-Path (Join-Path $crate "Cargo.toml"))) { throw "missing $crate" }
    Set-Location $crate
    # .169 is the box with the FlashWorld / DA3 / rig+motion oracle venvs
    # provisioned, so it opts into the python reference backends (the
    # default feature set is native-only; see libs/asset/ai/Cargo.toml).
    cargo build --release --bin makepad-asset-ai --features python-backends
    $code = $LASTEXITCODE
    Write-Host "build_exit=$code"
    if ($code -ne 0) { throw "cargo build failed: $code" }
    $exe = Join-Path $crate "target\release\makepad-asset-ai.exe"
    $dest = Join-Path $outDir "makepad-asset-ai.exe"
    Copy-Item -Force $exe $dest
    Get-Item $dest | ForEach-Object { Write-Host "installed $($_.FullName) $($_.Length) $($_.LastWriteTime)" }
    Write-Host "BUILD_OK"
} catch {
    Write-Host "BUILD_FAIL $_"
    exit 1
} finally {
    Stop-Transcript
}
