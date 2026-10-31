# Builds the EVB IDE portable archive for Windows x64:
#   target\evb-bundle\EVB-IDE-<version>-windows-x64.zip
# The archive holds an "EVB IDE" folder: evb-ide.exe, the command line tool as bin\evb-ide.exe,
# the ConPTY terminal host the build downloads (conpty.dll, x64\OpenConsole.exe) and the
# license notices. Based on upstream script/bundle-windows.ps1, without its installer.
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$version = (Get-Content EVB_VERSION -Raw).Trim()
if (Test-Path SOURCE_COMMIT) {
    $commit = (Get-Content SOURCE_COMMIT -Raw).Trim()
} else {
    $commit = (git rev-parse HEAD).Trim()
}
$env:ZED_RELEASE_CHANNEL = 'stable'
$env:ZED_COMMIT_SHA = $commit
$env:ZED_BUNDLE = 'true'
$env:RELEASE_VERSION = $version
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repoRoot 'target' }

Write-Output "Building EVB IDE $version ($($commit.Substring(0, 10))) for windows-x64"
. "$PSScriptRoot/generate-licenses.ps1"
# Release builds embed assets at compile time; Cargo does not notice newly added files.
foreach ($embedding in 'crates/assets/src/assets.rs', 'crates/settings/src/settings.rs') {
    (Get-Item $embedding).LastWriteTime = Get-Date
}
cargo --config .cargo/bundle-config.toml build --release --package zed --bin evb-ide
cargo --config .cargo/bundle-config.toml build --release --package cli

$release = Join-Path $targetDir 'release'
$outDir = Join-Path $repoRoot 'target\evb-bundle'
$appDir = Join-Path $outDir 'EVB IDE'
if (Test-Path $appDir) { Remove-Item $appDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path "$appDir\bin", "$appDir\x64", "$appDir\licenses" | Out-Null
Copy-Item (Join-Path $release 'evb-ide.exe') "$appDir\evb-ide.exe"
Copy-Item (Join-Path $release 'cli.exe') "$appDir\bin\evb-ide.exe"
# The terminal uses this ConPTY instead of the one in Windows; Zed's build.rs downloads it.
Copy-Item (Join-Path $release 'conpty.dll') "$appDir\conpty.dll"
Copy-Item (Join-Path $release 'OpenConsole.exe') "$appDir\x64\OpenConsole.exe"
Copy-Item LICENSE-GPL, LICENSE-APACHE, NOTICE.md, assets\licenses.md, NATIVE-SOURCES.md, assets\WEBRTC-LICENSE.md "$appDir\licenses"
@"
EVB IDE $version, built from commit $commit.
Corresponding source, dependency sources, and build instructions:
https://github.com/evb0110/evb-ide/releases/tag/v$version
Source repository: https://github.com/evb0110/evb-ide
License: GPL-3.0-or-later, with Apache-2.0 components where marked.
"@ | Set-Content "$appDir\licenses\SOURCE.txt"

$archive = Join-Path $outDir "EVB-IDE-$version-windows-x64.zip"
if (Test-Path $archive) { Remove-Item $archive -Force }
Compress-Archive -Path $appDir -DestinationPath $archive
Write-Output "Assembled $archive"
