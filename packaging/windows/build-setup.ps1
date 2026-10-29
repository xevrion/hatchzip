# Builds dist\HatchZip-x64-setup.exe with Inno Setup 6 from a folder holding
# the files of the CLI release zip plus hatchzip-gui.exe. GitHub's Windows
# runners have Inno Setup; anywhere else it's installed with Chocolatey.
#
#   packaging\windows\build-setup.ps1 dist\payload 0.1.0
#
# A signed setup also needs a signed uninstaller, which the compiler takes
# from a folder. With -Prepare it only writes the unsigned uninstaller there,
# as uninst-<Inno version>-<hash>.e32, for the signing service; once that file
# is signed in place, the same command without -Prepare builds the setup:
#
#   packaging\windows\build-setup.ps1 dist\payload 0.1.0 -SignedUninstallerDir dist\uninstaller -Prepare
#   (sign dist\uninstaller\uninst-*.e32)
#   packaging\windows\build-setup.ps1 dist\payload 0.1.0 -SignedUninstallerDir dist\uninstaller

param(
    [Parameter(Mandatory)] [string] $Payload,
    [Parameter(Mandatory)] [string] $Version,
    [string] $SignedUninstallerDir,
    [switch] $Prepare
)

$ErrorActionPreference = 'Stop'
# Exit codes are checked by hand, since -Prepare expects a failing one.
$PSNativeCommandUseErrorActionPreference = $false
if ($Prepare -and -not $SignedUninstallerDir) { throw '-Prepare needs -SignedUninstallerDir' }
foreach ($f in 'hatchzip.exe', 'hatchzip-gui.exe') {
    if (-not (Test-Path (Join-Path $Payload $f))) { throw "$f is missing from $Payload" }
}
$iscc = Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe'
if (-not (Test-Path $iscc)) {
    choco install innosetup --yes --no-progress | Out-Host
}
$payload = (Resolve-Path $Payload).Path
$arguments = @("/DPayload=$payload", "/DAppVersion=$Version")
if ($SignedUninstallerDir) {
    New-Item -ItemType Directory -Force $SignedUninstallerDir | Out-Null
    $uninstallers = (Resolve-Path $SignedUninstallerDir).Path
    $arguments += "/DSignedUninstallerDir=$uninstallers"
}
if ($Prepare) {
    # The setup this would build is never used, so it goes to TEMP.
    $out = Join-Path ([IO.Path]::GetTempPath()) "hatchzip-setup-prepare"
    & $iscc /Qp "/O$out" @arguments (Join-Path $PSScriptRoot 'hatchzip.iss') | Out-Host
    # The compiler stops once it has written the unsigned uninstaller, so
    # failing is expected; finding no uninstaller is not.
    $found = @(Get-ChildItem $uninstallers -Filter 'uninst-*.e32')
    if ($found.Count -ne 1) { throw "expected one uninstaller in $uninstallers, found $($found.Count)" }
    Write-Host "uninstaller to sign: $($found[0].FullName)"
    exit 0
}
& $iscc /Qp @arguments (Join-Path $PSScriptRoot 'hatchzip.iss')
if ($LASTEXITCODE) { throw "iscc exited with $LASTEXITCODE" }
