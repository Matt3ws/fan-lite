# Package the Windows release zips from the release build (run from the repo root after
# `cargo build --release`). Used by CI (to check it) and by the Release workflow.
#
# FanLite-<version>-windows-x64.zip holds FanLite.exe. Until older installs have moved over,
# fan-lite-<version>-windows-x64.zip holds the same program as fan-lite.exe, which is what the
# updater in v0.2.1-v0.2.6 downloads and looks for.
param([Parameter(Mandatory)][string]$Version, [string]$TargetDir = "target")
$ErrorActionPreference = "Stop"

function Package([string]$name, [string]$exe) {
    if (Test-Path $name) { Remove-Item -Recurse -Force $name }
    New-Item -ItemType Directory "$name\licenses\fonts", "$name\licenses\pawnio" | Out-Null
    Copy-Item "$TargetDir\release\FanLite.exe" "$name\$exe"
    Copy-Item README.md, LICENSE $name
    Copy-Item third_party\fonts\*.txt, third_party\fonts\README.md "$name\licenses\fonts"
    Copy-Item third_party\pawnio\COPYING, third_party\pawnio\README.md "$name\licenses\pawnio"
    Compress-Archive -Path "$name\*" -DestinationPath "$name.zip" -Force
    # LF line ending, so `sha256sum -c` also works on Linux and in Git Bash.
    $hash = (Get-FileHash "$name.zip" -Algorithm SHA256).Hash.ToLower()
    [IO.File]::WriteAllText("$PWD\$name.zip.sha256", "$hash  $name.zip`n")
    Get-ChildItem "$name.zip", "$name.zip.sha256" | Format-Table Name, Length
}

Package "FanLite-$Version-windows-x64" "FanLite.exe"
Package "fan-lite-$Version-windows-x64" "fan-lite.exe"
