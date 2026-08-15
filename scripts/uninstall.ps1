# thoth uninstaller for Windows.
# usage:  irm https://raw.githubusercontent.com/thoth-coder/thoth/main/scripts/uninstall.ps1 | iex
# purge:  $env:THOTH_PURGE = "1"; irm ... | iex   (also removes config and state dirs)
$ErrorActionPreference = "Stop"

$InstallDir = if ($env:THOTH_INSTALL_DIR) { $env:THOTH_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\thoth" }

if (Test-Path $InstallDir) {
    Remove-Item -Recurse -Force $InstallDir
    Write-Host "removed $InstallDir"
} else {
    Write-Host "thoth not found in $InstallDir (nothing to remove)"
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$newPath = (($userPath -split ";") | Where-Object { $_ -and $_ -ne $InstallDir }) -join ";"
if ($newPath -ne $userPath) {
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Host "removed $InstallDir from your user PATH"
}

if ($env:THOTH_PURGE -eq "1") {
    Remove-Item -Recurse -Force (Join-Path $HOME ".thoth") -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force (Join-Path $env:APPDATA "thoth") -ErrorAction SilentlyContinue
    Write-Host "removed ~/.thoth and %APPDATA%\thoth"
} else {
    Write-Host "kept config and state:"
    Write-Host "  $HOME\.thoth          (config.toml, editor state, session recaps)"
    Write-Host "  $env:APPDATA\thoth    (config.toml from thoth 0.2 and earlier)"
    Write-Host "set `$env:THOTH_PURGE = `"1`" before running to remove them too"
}
