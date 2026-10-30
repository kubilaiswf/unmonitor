# Name, Publisher and PublisherName come from Partner Center > Product identity.
param(
    [string]$Name = "Unmonitor",
    [string]$Publisher = "CN=Unmonitor",
    [string]$PublisherName = "Kubilay Karacar",
    [string]$Version = "0.1.0.0"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot

cargo build --release --manifest-path "$root\Cargo.toml"
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$layout = "$root\target\msix"
Remove-Item $layout -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory "$layout\Images" | Out-Null
Copy-Item "$root\target\release\unmonitor.exe" $layout
Copy-Item "$PSScriptRoot\Images\*" "$layout\Images"
(Get-Content "$PSScriptRoot\AppxManifest.xml" -Raw) `
    -replace '__NAME__', $Name `
    -replace '__PUBLISHER__', $Publisher `
    -replace '__PUBLISHER_NAME__', $PublisherName `
    -replace '__VERSION__', $Version |
    Set-Content -Encoding utf8 "$layout\AppxManifest.xml"

$makeappx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" |
    Sort-Object FullName | Select-Object -Last 1
& $makeappx.FullName pack /d $layout /p "$root\target\unmonitor-$Version.msix" /o
exit $LASTEXITCODE
