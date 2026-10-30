$ErrorActionPreference = 'Stop'

# HatchZip-x64-setup.exe is an Inno Setup installer that always installs for
# the current user (PrivilegesRequired=lowest in packaging/windows/hatchzip.iss).
# It runs on ARM64 Windows through its x64 emulation, so there is no 32-bit
# or ARM64 download.
$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'exe'
  url64bit       = 'https://github.com/xevrion/hatchzip/releases/download/v0.1.8/HatchZip-x64-setup.exe'
  checksum64     = '8c137c59a49b5206dd4880d1b59a20b61309086a78b83e43a56ece3fcf427f2a'
  checksumType64 = 'sha256'
  softwareName   = 'HatchZip'
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
