# Detached: download the pinned RIFE v4.26 weights into the asset cache and run
# the makepad-ai-rife device-vs-reference parity gate on the box's GPU.
# Poll with remote-jobs/poll169-rife-parity.ps1.
$ErrorActionPreference = "Continue"
$log = "C:\ai\makepad-asset-ai\rife-parity.log"
$buildScript = @'
$ErrorActionPreference = "Continue"
Start-Transcript -Path "C:\ai\makepad-asset-ai\rife-parity.log" -Force
try {
  $w = "C:\ai\asset_node_cache\video\rife"
  New-Item -ItemType Directory -Force -Path $w | Out-Null
  $f = Join-Path $w "rife_v4.26.safetensors"
  if (-not (Test-Path $f)) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri "https://huggingface.co/Comfy-Org/frame_interpolation/resolve/main/frame_interpolation/rife_v4.26.safetensors" -OutFile $f -UseBasicParsing
  }
  Get-Item $f | ForEach-Object { Write-Host ("weights {0} {1}" -f $_.FullName, $_.Length) }
  $env:MAKEPAD_RIFE_WEIGHTS = $f
  $env:MAKEPAD_GGML_REQUIRE_CUDA = "1"
  $env:MAKEPAD_GGML_CUDA_ARCH = "120a"
  $env:CARGO_TERM_COLOR = "never"
  $env:CARGO_TERM_PROGRESS_WHEN = "never"
  Set-Location "C:\ai\makepad-work\libs\ai"
  cargo test --release -p makepad-ai-rife -- --nocapture device_matches
  Write-Host "test_exit=$LASTEXITCODE"
} catch { Write-Host "PARITY_FAIL $_" }
Stop-Transcript
'@
$scriptPath = "C:\ai\makepad-asset-ai\rife-parity.ps1"
Set-Content -Path $scriptPath -Value $buildScript -Encoding ASCII
$ps = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
$proc = Start-Process -FilePath $ps -ArgumentList @("-NoProfile","-ExecutionPolicy","Bypass","-File",$scriptPath) -WindowStyle Hidden -PassThru
Write-Output "spawned pid=$($proc.Id)"
