$ErrorActionPreference = 'Stop'

# The setup writes its uninstall entry under HKCU, as
# io.github.xevrion.hatchzip_is1, with the path to unins000.exe in quotes.
# Its uninstaller removes the right-click menu, the "Open with" entries and
# the PATH entry, then the install folder.
[array]$keys = Get-UninstallRegistryKey -SoftwareName 'HatchZip'

if ($keys.Count -eq 1) {
  $packageArgs = @{
    packageName    = $env:ChocolateyPackageName
    fileType       = 'exe'
    file           = $keys[0].UninstallString.Trim('"')
    silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART'
    validExitCodes = @(0)
  }
  Uninstall-ChocolateyPackage @packageArgs
}
elseif ($keys.Count -eq 0) {
  Write-Warning "HatchZip is not installed for this user; it may have been uninstalled from Settings, Apps already."
}
else {
  Write-Warning "$($keys.Count) programs named HatchZip are installed; none was uninstalled. Remove HatchZip from Settings, Apps."
  $keys | ForEach-Object { Write-Warning "- $($_.DisplayName) $($_.DisplayVersion)" }
}
