# thoth installer for Windows.
# usage:  irm https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/install.ps1 | iex
# env:    $env:THOTH_VERSION = "v0.1.0"   install a specific version (default: latest)
#         $env:THOTH_INSTALL_DIR = "..."  install location (default: %LOCALAPPDATA%\Programs\thoth)
$ErrorActionPreference = "Stop"

$Repo = "thoth-coder/thoth"
$InstallDir = if ($env:THOTH_INSTALL_DIR) { $env:THOTH_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\thoth" }
$Version = if ($env:THOTH_VERSION) { $env:THOTH_VERSION } else { "latest" }

if ($Version -eq "latest") {
    $tag = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
} else {
    $tag = $Version
}
if (-not $tag) { throw "could not resolve the latest release tag" }

if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    Write-Warning "no native ARM64 build yet, installing x86_64 (runs under emulation)"
}

$name = "thoth-$tag-x86_64-pc-windows-msvc"
$url = "https://github.com/$Repo/releases/download/$tag/$name.zip"
$tmp = Join-Path $env:TEMP "$name.zip"

Write-Host "downloading $url"
Invoke-WebRequest $url -OutFile $tmp -UseBasicParsing

# verify checksum when the file exists on the release
try {
    $expected = ((Invoke-RestMethod "$url.sha256") -split "\s+")[0].ToLower()
    $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch, aborting" }
} catch {
    if ($_.Exception.Message -eq "checksum mismatch, aborting") { throw }
    Write-Warning "no checksum file found, skipping verification"
}

New-Item -ItemType Directory -Force $InstallDir | Out-Null
Expand-Archive $tmp -DestinationPath $InstallDir -Force
Remove-Item $tmp -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
    Write-Host "added $InstallDir to your user PATH (restart the terminal to pick it up)"
}

Write-Host "installed thoth $tag -> $InstallDir\thoth.exe"
