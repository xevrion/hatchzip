# Reads the product name and version that Inno Setup wrote into a file it
# built (the setup or its uninstaller), checks them, and adds them to
# GITHUB_ENV as JSON strings for SignPath's parameters.
#
#   packaging\windows\inno-version-info.ps1 <file> <version>
#
# Inno Setup writes those strings over fixed-length placeholders, so they
# end in spaces ("HatchZip" and 52 of them), and SignPath compares them
# exactly. The trimmed values must be HatchZip and the release's version, or
# nothing is signed; SignPath then checks the file against the exact ones.

param(
    [Parameter(Mandatory)] [string] $File,
    [Parameter(Mandatory)] [string] $Version
)

$ErrorActionPreference = 'Stop'
$info = [Diagnostics.FileVersionInfo]::GetVersionInfo((Resolve-Path $File).Path)
$name = (Split-Path $File -Leaf)
if ($info.ProductName.TrimEnd() -ne 'HatchZip') { throw "$name has the product name '$($info.ProductName)', not HatchZip" }
if ($info.ProductVersion.TrimEnd() -ne $Version) { throw "$name has the product version '$($info.ProductVersion)', not $Version" }
Write-Host "$name`: '$($info.ProductName)' '$($info.ProductVersion)'"
"INNO_PRODUCT_NAME=$($info.ProductName | ConvertTo-Json)" >> $env:GITHUB_ENV
"INNO_PRODUCT_VERSION=$($info.ProductVersion | ConvertTo-Json)" >> $env:GITHUB_ENV
