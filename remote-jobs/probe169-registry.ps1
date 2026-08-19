$ErrorActionPreference = "Continue"
$p = "C:\Users\playe\makepad\local\aicontent-registry.json"
if (Test-Path $p) {
  $j = Get-Content $p -Raw | ConvertFrom-Json
  Write-Output ("models=" + $j.models.Count)
  foreach ($m in $j.models) { Write-Output ("  " + $m.id + " " + $m.domain + " local_files=" + (($m.files | Where-Object { $_.local -eq $true }).Count) + "/" + $m.files.Count) }
  Get-Item $p | ForEach-Object { "mtime " + $_.LastWriteTime + " size " + $_.Length }
} else { Write-Output "NO LOCAL REGISTRY" }
