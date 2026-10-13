# Builds the release artifacts into dist\:
#   Strela-Setup.exe   the installer (the same executable; it installs itself when it is named *Setup*.exe)
#   Strela.exe         the portable version
#   strela-cli.exe     the command-line tool
#   SHA256SUMS.txt     checksums of the three files
#
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1             # tests + build + dist\
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1 -Publish    # ... and `gh release create`
param([switch]$Publish, [switch]$SkipTests, [string]$NotesFile = "")

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path crates\app\Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$tag = "v$version"
Write-Host "Strela $tag"

if (-not $SkipTests) {
    cargo test --workspace --release
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }
    cargo clippy --workspace --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw "clippy failed" }
}
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$dist = Join-Path $root "dist"
if (Test-Path $dist) { Remove-Item -LiteralPath $dist -Recurse -Force }
New-Item -ItemType Directory $dist | Out-Null
Copy-Item target\release\Strela.exe (Join-Path $dist "Strela.exe")
Copy-Item target\release\Strela.exe (Join-Path $dist "Strela-Setup.exe")
Copy-Item target\release\strela-cli.exe (Join-Path $dist "strela-cli.exe")

$lines = foreach ($f in "Strela-Setup.exe", "Strela.exe", "strela-cli.exe") {
    $h = (Get-FileHash (Join-Path $dist $f) -Algorithm SHA256).Hash.ToLower()
    "$h  $f"
}
[IO.File]::WriteAllText((Join-Path $dist "SHA256SUMS.txt"), (($lines -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
Get-ChildItem $dist | Format-Table Name, Length
Get-Content (Join-Path $dist "SHA256SUMS.txt")

if ($Publish) {
    $assets = Get-ChildItem $dist | ForEach-Object { $_.FullName }
    if ($NotesFile -ne "") {
        gh release create $tag @assets --title "Strela $version" --notes-file $NotesFile
    } else {
        gh release create $tag @assets --title "Strela $version" --generate-notes
    }
    if ($LASTEXITCODE -ne 0) { throw "gh release failed" }
}
