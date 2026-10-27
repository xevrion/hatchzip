# Runs the setup the way a user would, silently: install, check what it
# left, run the installed CLI, uninstall, check that nothing is left. Used
# by the release workflow before anything is published.
#
#   packaging\windows\test-setup.ps1 dist\HatchZip-x64-setup.exe

param([Parameter(Mandatory)] [string] $Setup)

$ErrorActionPreference = 'Stop'
$Setup = (Resolve-Path $Setup).Path
$dir = Join-Path $env:LOCALAPPDATA 'Programs\HatchZip'
$link = Join-Path ([Environment]::GetFolderPath('Programs')) 'HatchZip.lnk'
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.xevrion.hatchzip_is1'
# Written by `hatchzip-gui --register-shell` (crates/shell).
$menuKey = 'HKCU:\Software\Classes\SystemFileAssociations\.zip\shell\HatchZip'
# The Explorer command handler's class, crates/shell/src/explorer.rs.
$clsid = '{CB3BCA83-08A0-4CC7-8219-C514FAC7D51C}'
$clsidKey = "HKCU:\Software\Classes\CLSID\$clsid"
$dll = Join-Path $dir 'hatchzip_shell_ext.dll'
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
foreach ($f in 'hatchzip-gui.exe', 'hatchzip.exe', 'hz.exe', 'hatchzip_shell_ext.dll', 'hatchzip-completion.ps1', 'unins000.exe') {
    if (-not (Test-Path (Join-Path $dir $f))) { throw "$f is missing" }
}
if (-not (Test-Path $link)) { throw 'the Start menu shortcut is missing' }
$entry = Get-ItemProperty $uninstallKey
Write-Host "Apps and features: $($entry.DisplayName) $($entry.DisplayVersion)"
if ((UserPath) -notcontains $dir) { throw "$dir is not on the user PATH" }
if (-not (Test-Path $menuKey)) { throw 'the right-click menu was not registered' }
function CheckHandler {
    $server = "$clsidKey\InprocServer32"
    if (-not (Test-Path $server)) { throw 'the Explorer command handler was not registered' }
    $registered = (Get-ItemProperty $server).'(default)'
    if ($registered -ne $dll) { throw "the Explorer command handler points at $registered" }
    if ((Get-ItemProperty $server).ThreadingModel -ne 'Apartment') { throw 'the command handler is not ThreadingModel=Apartment' }
    $handler = (Get-ItemProperty "$menuKey\shell\03extract-to").ExplorerCommandHandler
    if ($handler -ne $clsid) { throw "Extract to names the handler $handler" }
}
CheckHandler
if ((Get-ItemProperty $openWith).PSObject.Properties.Name -notcontains 'HatchZip.Archive') { throw 'HatchZip is not in Open with for .zip' }

$version = & (Join-Path $dir 'hatchzip.exe') --version
if ($LASTEXITCODE) { throw 'the installed hatchzip.exe does not run' }
Write-Host $version
& (Join-Path $dir 'hz.exe') --version | Out-Host
if ($LASTEXITCODE) { throw 'the installed hz.exe does not run' }
if ($entry.DisplayVersion -ne ($version -split ' ')[1]) { throw 'Apps and features shows a different version from the program' }
python .github/scripts/smoke.py (Join-Path $dir 'hatchzip.exe')
if ($LASTEXITCODE) { throw 'the smoke test failed on the installed hatchzip.exe' }

# Explorer keeps the handler DLL loaded once it has shown the menu, and
# Windows won't overwrite or delete a loaded DLL. Loading it here holds it
# the same way, through the update and the uninstall that follow.
Write-Host 'update while the Explorer DLL is loaded'
$loaded = [Runtime.InteropServices.NativeLibrary]::Load($dll)
Run $Setup $quiet
if (-not (Test-Path $dll)) { throw 'the update left no hatchzip_shell_ext.dll' }
if (-not (Test-Path "$dll.1.old")) { throw 'the loaded DLL was not renamed out of the way' }
CheckHandler

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
if (Test-Path $clsidKey) { throw 'the Explorer command handler is still registered' }
if ((Test-Path $openWith) -and ((Get-ItemProperty $openWith).PSObject.Properties.Name -contains 'HatchZip.Archive')) { throw 'HatchZip is still in Open with' }
# The copy still loaded went to the temporary folder.
[Runtime.InteropServices.NativeLibrary]::Free($loaded)
$moved = @(Get-ChildItem -Path ([IO.Path]::GetTempPath()) -Filter 'hatchzip_shell_ext.dll.*.old' -ErrorAction SilentlyContinue)
Write-Host "moved to the temporary folder: $($moved.Count) file(s)"
$moved | Remove-Item -Force -ErrorAction SilentlyContinue
Write-Host 'the setup installs and uninstalls cleanly'
