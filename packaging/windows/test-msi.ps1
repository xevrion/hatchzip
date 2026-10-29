# Runs the MSI the way an administrator or winget would, silently: install,
# check what it left, run the installed CLI, uninstall, check that nothing is
# left. Given the setup too, it also checks that neither installs over the
# other. Used by the release workflow before anything is published.
#
#   packaging\windows\test-msi.ps1 dist\HatchZip-x64.msi [dist\HatchZip-x64-setup.exe]

param(
    [Parameter(Mandatory)] [string] $Msi,
    [string] $Setup
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'signature.ps1')
$Msi = (Resolve-Path $Msi).Path
$dir = Join-Path $env:LOCALAPPDATA 'Programs\HatchZip'
$link = Join-Path ([Environment]::GetFolderPath('Programs')) 'HatchZip.lnk'
# Written by `hatchzip-gui --register-shell` (crates/shell).
$menuKey = 'HKCU:\Software\Classes\SystemFileAssociations\.zip\shell\HatchZip'
$progId = 'HKCU:\Software\Classes\HatchZip.Archive'
$appKey = 'HKCU:\Software\Classes\Applications\hatchzip-gui.exe'
$extensions = 'zip', '7z', 'tar', 'tgz', 'gz', '001', 'rar', 'r00'
$upgradeCode = '{' + ([xml](Get-Content (Join-Path $PSScriptRoot 'hatchzip.wxs'))).Wix.Package.UpgradeCode + '}'
$tmp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$log = Join-Path $tmp 'hatchzip-msi.log'
$quiet = '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'

# Runs msiexec and fails unless it exits with the code expected. The log is
# printed when it doesn't.
function Msiexec($arguments, [int] $expect = 0) {
    $p = Start-Process -FilePath msiexec.exe -ArgumentList ($arguments + '/qn', '/norestart', '/l*v', "`"$log`"") -PassThru
    $null = $p.Handle # keeps the exit code readable after it exits
    if (-not $p.WaitForExit(300000)) { throw "msiexec $arguments is still running after 5 minutes" }
    if ($p.ExitCode -ne $expect) {
        Get-Content $log -ErrorAction SilentlyContinue | Select-Object -Last 80 | Out-Host
        throw "msiexec $arguments exited with $($p.ExitCode), expected $expect"
    }
}

function Run($file, $arguments) {
    $p = Start-Process -FilePath $file -ArgumentList $arguments -PassThru
    $null = $p.Handle
    if (-not $p.WaitForExit(300000)) { throw "$file is still running after 5 minutes" }
    $p.ExitCode
}

# Every failed check is collected, so one run shows all of them.
$problems = [Collections.Generic.List[string]]::new()
function Check([bool] $ok, [string] $problem) {
    if (-not $ok) { $problems.Add($problem); Write-Host "FAIL: $problem" }
}
function Done($stage) {
    if ($problems.Count) { throw "$stage`: $($problems -join '; ')" }
}

function UserPath {
    [Environment]::GetEnvironmentVariable('Path', 'User') -split ';' | Where-Object { $_ }
}

function PathKind {
    $key = Get-Item 'HKCU:\Environment'
    if ($key.GetValueNames() -contains 'Path') { $key.GetValueKind('Path') } else { 'none' }
}

function OpenWithHas($ext) {
    $key = "HKCU:\Software\Classes\.$ext\OpenWithProgids"
    (Test-Path $key) -and ((Get-ItemProperty $key).PSObject.Properties.Name -contains 'HatchZip.Archive')
}

# A per-user MSI has no key under HKCU\...\Uninstall: Windows Installer keeps
# its entry for Settings > Apps itself. So the installed product is asked of
# Windows Installer, by its UpgradeCode. The COM object has no type library,
# hence InvokeMember.
$installer = New-Object -ComObject WindowsInstaller.Installer
function ComProperty($object, $name, $params) {
    $object.GetType().InvokeMember($name, 'GetProperty', $null, $object, $params)
}
# RelatedProducts returns a StringList, which PowerShell enumerates as it
# leaves ComProperty: what arrives is nothing, one product code or several.
function Products {
    foreach ($code in (ComProperty $installer 'RelatedProducts' @($upgradeCode))) { [string] $code }
}
function ProductInfo($code, $attribute) {
    try { ComProperty $installer 'ProductInfo' @($code, $attribute) }
    catch { "(no ${attribute}: $($_.Exception.InnerException.Message))" }
}

$kindBefore = PathKind

Write-Host 'install'
Msiexec '/i', "`"$Msi`""
foreach ($f in 'hatchzip-gui.exe', 'hatchzip.exe', 'hz.exe', 'hatchzip-completion.ps1', 'README.md',
        'LICENSE-MIT.txt', 'LICENSE-APACHE.txt', 'LICENSE-UNRAR.txt') {
    Check (Test-Path (Join-Path $dir $f)) "$f is missing"
}
# A signed build installs only signed programs.
if ($env:SIGNER) {
    foreach ($f in 'hatchzip-gui.exe', 'hatchzip.exe', 'hz.exe') {
        try { Assert-Signed (Join-Path $dir $f) } catch { Check $false "$_" }
    }
}
Check (Test-Path $link) 'the Start menu shortcut is missing'
if (Test-Path $link) {
    $target = (New-Object -ComObject WScript.Shell).CreateShortcut($link).TargetPath
    Check ($target -eq (Join-Path $dir 'hatchzip-gui.exe')) "the Start menu shortcut points at $target"
}
$products = @(Products)
Check ($products.Count -eq 1) "expected one installed product with the UpgradeCode $upgradeCode, found $($products.Count)"
$shown = $null
if ($products.Count -eq 1) {
    $code = $products[0]
    $shown = ProductInfo $code 'VersionString'
    $name = ProductInfo $code 'InstalledProductName'
    $publisher = ProductInfo $code 'Publisher'
    $about = ProductInfo $code 'URLInfoAbout'
    $help = ProductInfo $code 'HelpLink'
    $context = ProductInfo $code 'AssignmentType' # 0: per user, 1: per machine
    Write-Host "Apps and features: $name $shown, $publisher, $about, $help, product code $code, assignment $context"
    Check ($name -eq 'HatchZip') "the product is named $name"
    Check ($publisher -eq 'Yash Bavadiya') "the publisher is $publisher"
    Check ($about -eq 'https://hatchzip.dev') "URLInfoAbout is $about"
    Check ($context -eq '0') "it was installed per machine (AssignmentType $context)"
    $arp = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$code"
    Write-Host "$arp exists: $(Test-Path $arp)"
}
Check ((UserPath) -contains $dir) "$dir is not on the user PATH: $([Environment]::GetEnvironmentVariable('Path', 'User'))"
Check ((PathKind) -eq $kindBefore) "the user PATH changed from $kindBefore to $(PathKind)"
Check (Test-Path $menuKey) 'the right-click menu was not registered'
foreach ($ext in $extensions) {
    Check (OpenWithHas $ext) "HatchZip is not in Open with for .$ext"
}
$command = if (Test-Path "$progId\shell\open\command") { (Get-ItemProperty "$progId\shell\open\command").'(default)' }
Check ($command -eq "`"$dir\hatchzip-gui.exe`" `"%1`"") "the Open with command is $command"
Check ((Test-Path $appKey) -and (Get-ItemProperty $appKey).FriendlyAppName -eq 'HatchZip') 'FriendlyAppName is not set'

foreach ($exe in 'hatchzip.exe', 'hatchzip-gui.exe') {
    $info = (Get-Item (Join-Path $dir $exe) -ErrorAction SilentlyContinue).VersionInfo
    Write-Host "${exe}: $($info.ProductName) $($info.ProductVersion), $($info.FileDescription), $($info.CompanyName)"
    Check ($info.ProductName -eq 'HatchZip' -and $info.FileVersion -and $info.CompanyName) "$exe has no complete version resource"
}

$version = & (Join-Path $dir 'hatchzip.exe') --version
Check (-not $LASTEXITCODE) 'the installed hatchzip.exe does not run'
Write-Host $version
& (Join-Path $dir 'hz.exe') --version | Out-Host
Check (-not $LASTEXITCODE) 'the installed hz.exe does not run'
# Windows Installer keeps three numbers: 0.2.0-beta.1 is shown as 0.2.0.
Check ($shown -eq (($version -split ' ')[1] -split '[-+]')[0]) "Apps and features shows $shown, the program $version"
python .github/scripts/smoke.py (Join-Path $dir 'hatchzip.exe')
Check (-not $LASTEXITCODE) 'the smoke test failed on the installed hatchzip.exe'
Done 'after installing'

if ($Setup) {
    Write-Host 'the setup refuses to install over the MSI'
    $code = Run $Setup $quiet
    Check ($code -ne 0) 'the setup installed over the MSI'
    Check (-not (Test-Path (Join-Path $dir 'unins000.exe'))) 'the setup left its uninstaller in the MSI install'
    Done 'installing the setup over the MSI'
}

Write-Host 'uninstall'
Msiexec '/x', "`"$Msi`""
Check (-not (Test-Path $dir)) "the install folder is still there: $((Get-ChildItem $dir -Recurse -ErrorAction SilentlyContinue).FullName -join ', ')"
Check (-not (Test-Path $link)) 'the Start menu shortcut is still there'
Check (@(Products).Count -eq 0) 'it is still installed'
Check ((UserPath) -notcontains $dir) "$dir is still on the user PATH"
Check ((PathKind) -eq $kindBefore) "the user PATH changed from $kindBefore to $(PathKind)"
Check (-not (Test-Path $menuKey)) 'the right-click menu is still there'
foreach ($ext in $extensions) {
    Check (-not (OpenWithHas $ext)) "HatchZip is still in Open with for .$ext"
}
Check (-not (Test-Path $progId)) "$progId is still there"
Check (-not (Test-Path $appKey)) "$appKey is still there"
Done 'after uninstalling'

if ($Setup) {
    Write-Host 'the MSI refuses to install over the setup'
    $code = Run $Setup $quiet
    if ($code) { throw "the setup exited with $code" }
    # 1603: the launch condition stopped it.
    Msiexec '/i', "`"$Msi`"" 1603
    Check (@(Products).Count -eq 0) 'the MSI installed over the setup'
    $code = Run (Join-Path $dir 'unins000.exe') $quiet
    Check ($code -eq 0) "uninstalling the setup exited with $code"
    # The uninstaller finishes from a copy of itself in TEMP.
    foreach ($i in 1..30) {
        if (-not (Test-Path $dir)) { break }
        Start-Sleep -Seconds 1
    }
    Check (-not (Test-Path $dir)) 'the setup left its install folder'
    Done 'installing the MSI over the setup'
}
Write-Host 'the MSI installs and uninstalls cleanly'
