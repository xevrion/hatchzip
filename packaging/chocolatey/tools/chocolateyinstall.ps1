$ErrorActionPreference = 'Stop'

# HatchZip-x64-setup.exe is an Inno Setup installer that always installs for
# the current user (PrivilegesRequired=lowest in packaging/windows/hatchzip.iss).
# It runs on ARM64 Windows through its x64 emulation, so there is no 32-bit
# or ARM64 download.
$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'exe'
  url64bit       = 'https://github.com/xevrion/hatchzip/releases/download/v0.1.11/HatchZip-x64-setup.exe'
  checksum64     = '8d7dbb385ce9b5d5ec1bb87c33b60ae5c7c2ff20dcc8feafc4298321457c02a9'
  checksumType64 = 'sha256'
  softwareName   = 'HatchZip'
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
