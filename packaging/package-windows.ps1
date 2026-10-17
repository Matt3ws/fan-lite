# Package the Windows release zip from the release build (run from the repo root after
# `cargo build --release`). Used by CI (to check it) and by the Release workflow.
param([Parameter(Mandatory)][string]$Version, [string]$TargetDir = "target")
$ErrorActionPreference = "Stop"
$name = "fan-lite-$Version-windows-x64"
if (Test-Path $name) { Remove-Item -Recurse -Force $name }
New-Item -ItemType Directory "$name\licenses\fonts", "$name\licenses\pawnio" | Out-Null
Copy-Item "$TargetDir\release\fan-lite.exe", README.md, LICENSE $name
Copy-Item third_party\fonts\*.txt, third_party\fonts\README.md "$name\licenses\fonts"
Copy-Item third_party\pawnio\COPYING, third_party\pawnio\README.md "$name\licenses\pawnio"
Compress-Archive -Path "$name\*" -DestinationPath "$name.zip" -Force
(Get-FileHash "$name.zip" -Algorithm SHA256).Hash.ToLower() + "  $name.zip" | Out-File "$name.zip.sha256" -Encoding ascii
Get-ChildItem "$name.zip", "$name.zip.sha256" | Format-Table Name, Length
