# Checks that each file the signing service returned carries a valid
# Authenticode signature (see signature.ps1), then copies it over the
# unsigned one. Used by the release workflow after each signing step.
#
#   packaging\windows\check-signed.ps1 <signed folder> <destination> <file>...

param(
    [Parameter(Mandatory)] [string] $From,
    [Parameter(Mandatory)] [string] $To,
    [Parameter(Mandatory, ValueFromRemainingArguments)] [string[]] $Files
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'signature.ps1')
foreach ($f in $Files) {
    $path = Join-Path $From $f
    if (-not (Test-Path $path)) { throw "$f did not come back from signing" }
    Assert-Signed $path
    Copy-Item $path (Join-Path $To $f) -Force
}
