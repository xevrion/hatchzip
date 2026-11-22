$ErrorActionPreference = 'Stop'

# HatchZip-x64-setup.exe is an Inno Setup installer that always installs for
# the current user (PrivilegesRequired=lowest in packaging/windows/hatchzip.iss).
# It runs on ARM64 Windows through its x64 emulation, so there is no 32-bit
# or ARM64 download.
$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'exe'
  url64bit       = 'https://github.com/xevrion/hatchzip/releases/download/v0.1.10/HatchZip-x64-setup.exe'
  checksum64     = '08f6129afb4281d734101c605a0aa40a0ac40bac0b288903d553ad742ca2a774'
  checksumType64 = 'sha256'
  softwareName   = 'HatchZip'
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
