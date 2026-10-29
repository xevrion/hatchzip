# Builds dist\HatchZip-x64.msi with WiX 5 from the folder build-setup.ps1
# uses: the files of the CLI release zip plus hatchzip-gui.exe. WiX is a
# .NET tool; it's installed at a fixed version if it isn't there.
#
#   packaging\windows\build-msi.ps1 dist\payload 0.1.0

param(
    [Parameter(Mandatory)] [string] $Payload,
    [Parameter(Mandatory)] [string] $Version
)

$ErrorActionPreference = 'Stop'
$WixVersion = '5.0.2'

# hatchzip.wxs lists each file by name, unlike the setup, which takes the
# whole folder. A file added to the zip must be added there too.
$expected = 'hatchzip.exe', 'hatchzip-gui.exe', 'hatchzip-completion.ps1', 'README.md',
    'LICENSE-MIT.txt', 'LICENSE-APACHE.txt', 'LICENSE-UNRAR.txt'
$found = Get-ChildItem -Path $Payload -Recurse -File | ForEach-Object { $_.FullName.Substring((Resolve-Path $Payload).Path.Length + 1) }
$missing = $expected | Where-Object { $found -notcontains $_ }
$extra = $found | Where-Object { $expected -notcontains $_ }
if ($missing) { throw "missing from ${Payload}: $($missing -join ', ')" }
if ($extra) { throw "not in hatchzip.wxs: $($extra -join ', '). Add them there and to `$expected." }

# Windows Installer versions are three numbers; 0.2.0-beta.1 becomes 0.2.0.
$msiVersion = ($Version -split '[-+]')[0]
if ($msiVersion -notmatch '^\d+\.\d+\.\d+$') { throw "$Version is not a version like 0.1.0" }

$wix = Get-Command wix -ErrorAction SilentlyContinue
if (-not $wix -or ((& wix --version) -split '\+')[0] -ne $WixVersion) {
    dotnet tool install --global wix --version $WixVersion | Out-Host
    if ($LASTEXITCODE) { throw "installing WiX $WixVersion failed" }
    $env:PATH = "$env:USERPROFILE\.dotnet\tools;$env:PATH"
}

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$out = Join-Path $root 'dist\HatchZip-x64.msi'
& wix build -arch x64 `
    -d "Payload=$((Resolve-Path $Payload).Path)" `
    -d "Version=$msiVersion" `
    -o $out (Join-Path $PSScriptRoot 'hatchzip.wxs')
if ($LASTEXITCODE) { throw "wix build exited with $LASTEXITCODE" }
# Windows Installer's own consistency checks (ICE), which WiX 4 and later
# no longer run while building. They read the .wixpdb for source lines.
& wix msi validate $out
if ($LASTEXITCODE) { throw "the MSI failed validation (wix msi validate exited with $LASTEXITCODE)" }
# WiX leaves a .wixpdb with debug data next to the msi; it isn't a download.
Remove-Item -ErrorAction SilentlyContinue ([IO.Path]::ChangeExtension($out, '.wixpdb'))
Write-Host "built $out"
