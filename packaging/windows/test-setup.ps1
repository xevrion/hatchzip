# Runs the setup the way a user would, silently: install, check what it
# left, run the installed CLI, uninstall, check that nothing is left. Used
# by the release workflow before anything is published.
#
#   packaging\windows\test-setup.ps1 dist\HatchZip-x64-setup.exe

param([Parameter(Mandatory)] [string] $Setup)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'signature.ps1')
$Setup = (Resolve-Path $Setup).Path
$dir = Join-Path $env:LOCALAPPDATA 'Programs\HatchZip'
$link = Join-Path ([Environment]::GetFolderPath('Programs')) 'HatchZip.lnk'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.xevrion.hatchzip_is1'
# Written by `hatchzip-gui --register-shell` (crates/shell).
$menuKey = 'HKCU:\Software\Classes\SystemFileAssociations\.zip\shell\HatchZip'
$openWith = 'HKCU:\Software\Classes\.zip\OpenWithProgids'
$quiet = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'

function Run($file, $arguments) {
    $p = Start-Process -FilePath $file -ArgumentList $arguments -PassThru
    $null = $p.Handle # keeps the exit code readable after it exits
    if (-not $p.WaitForExit(300000)) { throw "$file is still running after 5 minutes" }
    if ($p.ExitCode) { throw "$file exited with $($p.ExitCode)" }
}

function UserPath {
    [Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ }
}

$info = (Get-Item $Setup).VersionInfo
foreach ($field in 'CompanyName', 'FileDescription', 'ProductName', 'LegalCopyright', 'FileVersion', 'ProductVersion') {
    if (-not $info.$field) { throw "the setup's version info has no $field" }
}
Write-Host "version info: $($info.ProductName) $($info.ProductVersion) ($($info.FileVersion)), $($info.CompanyName), $($info.LegalCopyright)"

Write-Host 'install'
Run $Setup $quiet
foreach ($f in 'hatchzip-gui.exe', 'hatchzip.exe', 'hz.exe', 'hatchzip-completion.ps1', 'unins000.exe') {
    if (-not (Test-Path (Join-Path $dir $f))) { throw "$f is missing" }
}
# A signed build installs only signed programs, the uninstaller included
# (SignedUninstaller in hatchzip.iss).
if ($env:SIGNER) {
    foreach ($f in 'hatchzip-gui.exe', 'hatchzip.exe', 'hz.exe', 'unins000.exe') {
        Assert-Signed (Join-Path $dir $f)
    }
}
if (-not (Test-Path $link)) { throw 'the Start menu shortcut is missing' }
$entry = Get-ItemProperty $uninstallKey
Write-Host "Apps and features: $($entry.DisplayName) $($entry.DisplayVersion)"
if ((UserPath) -notcontains $dir) { throw "$dir is not on the user PATH" }
if (-not (Test-Path $menuKey)) { throw 'the right-click menu was not registered' }
if ((Get-ItemProperty $openWith).PSObject.Properties.Name -notcontains 'HatchZip.Archive') { throw 'HatchZip is not in Open with for .zip' }

$version = & (Join-Path $dir 'hatchzip.exe') --version
if ($LASTEXITCODE) { throw 'the installed hatchzip.exe does not run' }
Write-Host $version
& (Join-Path $dir 'hz.exe') --version | Out-Host
if ($LASTEXITCODE) { throw 'the installed hz.exe does not run' }
if ($entry.DisplayVersion -ne ($version -split ' ')[1]) { throw 'Apps and features shows a different version from the program' }
python .github/scripts/smoke.py (Join-Path $dir 'hatchzip.exe')
if ($LASTEXITCODE) { throw 'the smoke test failed on the installed hatchzip.exe' }

Write-Host 'uninstall'
Run (Join-Path $dir 'unins000.exe') $quiet
# The uninstaller finishes from a copy of itself in TEMP, so the folder can
# outlive the process that was waited for by a moment.
foreach ($i in 1..30) {
    if (-not (Test-Path $dir)) { break }
    Start-Sleep -Seconds 1
}
if (Test-Path $dir) { throw 'the install folder is still there' }
if (Test-Path $link) { throw 'the Start menu shortcut is still there' }
if (Test-Path $uninstallKey) { throw 'it is still in Apps and features' }
if ((UserPath) -contains $dir) { throw "$dir is still on the user PATH" }
if (Test-Path $menuKey) { throw 'the right-click menu is still there' }
if ((Test-Path $openWith) -and ((Get-ItemProperty $openWith).PSObject.Properties.Name -contains 'HatchZip.Archive')) { throw 'HatchZip is still in Open with' }
Write-Host 'the setup installs and uninstalls cleanly'
