# Package a Windows build: binary, README and LICENSE in a .zip.
#
#   pwsh -File scripts/package-windows.ps1
#   pwsh -File scripts/package-windows.ps1 -Out C:\temp\rel

param(
    [string]$Out = (Join-Path (Split-Path -Parent $PSScriptRoot) "dist")
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
Set-Location $Root

$Version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$Name = "viberadio-$Version-windows-x86_64"
$Stage = Join-Path $Out $Name

if (-not $env:CARGO_BUILD_JOBS) {
    $env:CARGO_BUILD_JOBS = "2"
}

Write-Host "==> cargo build --release --locked (jobs: $env:CARGO_BUILD_JOBS)"
cargo build --release --locked
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item "target/release/viberadio.exe" (Join-Path $Stage "viberadio.exe") -Force
Copy-Item "README.md" (Join-Path $Stage "README.md") -Force
Copy-Item "LICENSE" (Join-Path $Stage "LICENSE") -Force

$Zip = Join-Path $Out "$Name.zip"
if (Test-Path $Zip) {
    Remove-Item $Zip -Force
}
Compress-Archive -Path (Join-Path $Stage "*") -DestinationPath $Zip -Force

Write-Host "==> packaging $Zip"
Get-Item $Zip | Format-Table Name, Length
