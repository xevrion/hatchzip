$ErrorActionPreference = 'Stop'

# HatchZip-x64-setup.exe is an Inno Setup installer that always installs for
# the current user (PrivilegesRequired=lowest in packaging/windows/hatchzip.iss).
# It runs on ARM64 Windows through its x64 emulation, so there is no 32-bit
# or ARM64 download.
$packageArgs = @{
  packageName    = $env:ChocolateyPackageName
  fileType       = 'exe'
  url64bit       = 'https://github.com/xevrion/hatchzip/releases/download/v0.1.9/HatchZip-x64-setup.exe'
  checksum64     = 'c9799aef57eef6bc8c2c8431956d822c7fd5f317758cd578c58f236dcb3fb3f7'
  checksumType64 = 'sha256'
  softwareName   = 'HatchZip'
  silentArgs     = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-'
  validExitCodes = @(0)
}

Install-ChocolateyPackage @packageArgs
