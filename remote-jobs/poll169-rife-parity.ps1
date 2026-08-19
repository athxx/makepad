$ErrorActionPreference = "Continue"
$log = "C:\ai\makepad-asset-ai\rife-parity.log"
if (Test-Path $log) { Get-Content $log -Tail 30 } else { Write-Output "NO_LOG" }
