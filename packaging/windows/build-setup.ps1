# Builds dist\HatchZip-x64-setup.exe with Inno Setup 6 from a folder holding
# the files of the CLI release zip plus hatchzip-gui.exe and
# hatchzip_shell_ext.dll. GitHub's Windows runners have Inno Setup; anywhere
# else it's installed with Chocolatey.
#
#   packaging\windows\build-setup.ps1 dist\payload 0.1.0

param(
    [Parameter(Mandatory)] [string] $Payload,
    [Parameter(Mandatory)] [string] $Version
)

$ErrorActionPreference = 'Stop'
foreach ($f in 'hatchzip.exe', 'hatchzip-gui.exe', 'hatchzip_shell_ext.dll') {
    if (-not (Test-Path (Join-Path $Payload $f))) { throw "$f is missing from $Payload" }
}
$iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) {
    choco install innosetup --yes --no-progress | Out-Host
}
$payload = (Resolve-Path $Payload).Path
& $iscc /Qp "/DPayload=$payload" "/DAppVersion=$Version" (Join-Path $PSScriptRoot 'hatchzip.iss')
if ($LASTEXITCODE) { throw "iscc exited with $LASTEXITCODE" }
