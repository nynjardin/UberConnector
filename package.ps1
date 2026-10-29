# Builds the portable zip: dist\UberConnector-<version>.zip
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$et = Join-Path $root "vendor\easytier\easytier-windows-x86_64"
# Staged apart from dist\UberConnector, which may be in use (loaded WinDivert driver, uc-data).
$out = Join-Path $root "dist\staging\UberConnector"

cargo build --release -p uberconnector -p uc-app
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
# Diagnostic tools shipped alongside (NAT type check, punching test between two PCs).
cargo build --release -p uc-core --example nat_check --example punch_test
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory -Force "$out\easytier" | Out-Null
Copy-Item (Join-Path $root "target\release\UberConnector.exe") $out
Copy-Item (Join-Path $root "target\release\uc-helper.exe") $out
Copy-Item (Join-Path $root "target\release\examples\nat_check.exe") $out
Copy-Item (Join-Path $root "target\release\examples\punch_test.exe") $out
foreach ($f in "easytier-core.exe", "easytier-cli.exe", "wintun.dll", "Packet.dll", "WinDivert64.sys") {
    Copy-Item (Join-Path $et $f) "$out\easytier"
}
Copy-Item (Join-Path $root "README.txt") $out

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$zip = Join-Path $root "dist\UberConnector-$version.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $out -DestinationPath $zip
Write-Host "OK: $zip"
