; HatchZip-x64-setup.exe, built with Inno Setup 6 from a folder holding the
; files of the CLI release zip plus hatchzip-gui.exe and
; hatchzip_shell_ext.dll (see build-setup.ps1):
;
;   iscc /DPayload=C:\path\to\payload /DAppVersion=0.1.0 packaging\windows\hatchzip.iss
;
; It installs for the current user only and needs no administrator rights.
; It uses the same folder as install.ps1 and puts it on PATH the same way,
; so either one updates what the other installed.

#ifndef Payload
  #define Payload "..\..\dist\payload"
#endif
#ifndef AppVersion
  #define AppVersion GetStringFileInfo(Payload + "\hatchzip-gui.exe", "ProductVersion")
#endif
; The four numbers of the exe's version resource, such as 0.1.0.0. AppVersion
; can carry a suffix like -beta.1, which a version resource can't.
#ifndef NumVersion
  #define NumVersion GetVersionNumbersString(Payload + "\hatchzip-gui.exe")
#endif

[Setup]
; Never change: Windows finds the installed copy to update or remove by it.
AppId=io.github.xevrion.hatchzip
AppName=HatchZip
AppVersion={#AppVersion}
AppVerName=HatchZip {#AppVersion}
AppPublisher=Yash Bavadiya
AppPublisherURL=https://github.com/xevrion/hatchzip
AppSupportURL=https://github.com/xevrion/hatchzip/issues
AppUpdatesURL=https://github.com/xevrion/hatchzip/releases
AppComments=Extract huge archives in place
AppCopyright=Copyright (c) 2026 Yash Bavadiya
; The setup's own version resource, which Windows shows under Properties >
; Details and which antivirus engines read. Without these, Inno Setup fills
; in only part of it.
VersionInfoVersion={#NumVersion}
VersionInfoTextVersion={#AppVersion}
VersionInfoProductVersion={#NumVersion}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoCompany=Yash Bavadiya
VersionInfoDescription=HatchZip Setup
VersionInfoProductName=HatchZip
VersionInfoCopyright=Copyright (c) 2026 Yash Bavadiya
DefaultDirName={localappdata}\Programs\HatchZip
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
UsedUserAreasWarning=no
; ARM64 Windows runs the x64 build through its built-in emulation.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\..\dist
OutputBaseFilename=HatchZip-x64-setup
SetupIconFile=..\..\assets\icon\hatchzip.ico
WizardStyle=modern
UninstallDisplayIcon={app}\hatchzip-gui.exe
UninstallDisplayName=HatchZip
Compression=lzma2/max
SolidCompression=yes
; Tells Explorer and new terminals that PATH changed, on install and on
; uninstall.
ChangesEnvironment=yes
; Also lets Explorer pick up the "Open with" entries without a sign-out.
ChangesAssociations=yes
; Without *.dll, which is in the default. Explorer holds the Explorer menu
; DLL, and Restart Manager would offer to close Explorer for it, or close
; it unasked in a silent install. The [Code] section moves the DLL aside
; instead.
CloseApplicationsFilter=*.exe,*.chm

[Files]
Source: "{#Payload}\*"; DestDir: "{app}"; Excludes: "hatchzip_shell_ext.dll"; Flags: ignoreversion recursesubdirs createallsubdirs
; The Explorer menu's command handler, which hatchzip-gui finds next to
; itself when it registers the menu. A copy Explorer still has loaded was
; renamed out of the way at ssInstall, so this never meets a locked file.
Source: "{#Payload}\hatchzip_shell_ext.dll"; DestDir: "{app}"; Flags: ignoreversion
; The short name for the CLI. A copy, since Windows has no symlinks without
; developer mode; it's a few MB.
Source: "{#Payload}\hatchzip.exe"; DestDir: "{app}"; DestName: "hz.exe"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\HatchZip"; Filename: "{app}\hatchzip-gui.exe"; WorkingDir: "{userdocs}"; Comment: "Extract huge archives in place"

[Registry]
; "Open with" for each archive type, without taking over the default app.
Root: HKCU; Subkey: "Software\Classes\HatchZip.Archive"; ValueType: string; ValueData: "Archive"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\HatchZip.Archive\DefaultIcon"; ValueType: string; ValueData: """{app}\hatchzip-gui.exe"",0"
Root: HKCU; Subkey: "Software\Classes\HatchZip.Archive\shell\open\command"; ValueType: string; ValueData: """{app}\hatchzip-gui.exe"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\Applications\hatchzip-gui.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "HatchZip"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\.zip\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.7z\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.tar\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.tgz\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.gz\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.001\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.rar\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.r00\OpenWithProgids"; ValueType: string; ValueName: "HatchZip.Archive"; ValueData: ""; Flags: uninsdeletevalue

[Run]
; The right-click menu lives in the user's registry hive and is written by
; the app itself, so the installer and the app's settings switch agree.
Filename: "{app}\hatchzip-gui.exe"; Parameters: "--register-shell"; StatusMsg: "Adding HatchZip to the right-click menu..."; Flags: runhidden waituntilterminated
Filename: "{app}\hatchzip-gui.exe"; Description: "Open HatchZip"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\hatchzip-gui.exe"; Parameters: "--unregister-shell"; RunOnceId: "UnregisterShell"; Flags: runhidden waituntilterminated

[UninstallDelete]
Type: filesandordirs; Name: "{app}"

[Code]
const
  EnvKey = 'Environment';
  { The UpgradeCode in hatchzip.wxs. }
  MsiUpgradeCode = '{083ECD39-645A-4455-AC0F-1647AC24D57A}';
  { The Explorer menu's command handler, from crates/shell-ext. }
  HandlerDll = 'hatchzip_shell_ext.dll';

{ True if HatchZip-x64.msi is installed. It installs to the same folder, so
  the two must not be installed over each other; the MSI refuses the other
  way round. A per-user MSI has no key under HKCU\...\Uninstall, so this asks
  Windows Installer for products with the MSI's UpgradeCode. }
function MsiInstalled: Boolean;
var
  Installer, Products: Variant;
begin
  Result := False;
  try
    Installer := CreateOleObject('WindowsInstaller.Installer');
    Products := Installer.RelatedProducts(MsiUpgradeCode);
    Result := Products.Count > 0;
  except
    Log('Could not ask Windows Installer for HatchZip-x64.msi: ' + GetExceptionMessage);
  end;
end;

function InitializeSetup: Boolean;
begin
  Result := not MsiInstalled;
  if not Result then
    SuppressibleMsgBox('HatchZip is already installed by HatchZip-x64.msi. Uninstall it under Settings, Apps, then run this setup again.',
      mbError, MB_OK, IDOK);
end;

{ True if Dir is one of the entries of the user's PATH, ignoring case. }
function PathHas(Path, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') > 0;
end;

procedure AddToPath(Dir: String);
var
  Path: String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    Path := '';
  if PathHas(Path, Dir) then
    exit;
  if (Path <> '') and (Path[Length(Path)] <> ';') then
    Path := Path + ';';
  { Expandable, like the value Windows creates, so entries such as
    %USERPROFILE%\bin keep working. }
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Path + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Path: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    exit;
  Path := ';' + Path + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Path));
  if P = 0 then
    exit;
  Delete(Path, P, Length(Dir) + 1);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Copy(Path, 2, Length(Path) - 2));
end;

{ Explorer keeps hatchzip_shell_ext.dll loaded once it has shown the menu,
  often until the user signs out, and Windows refuses to overwrite or delete
  a loaded DLL. It does allow renaming one. Replacing a file at the next
  restart needs administrator rights, which this setup doesn't ask for, so a
  loaded copy is renamed out of the way and deleted by a later run, once
  nothing has it loaded. }

{ A name in Dir for a renamed copy of the DLL that no file has yet. }
function UnusedOldName(Dir: String): String;
var
  I: Integer;
begin
  I := 1;
  repeat
    Result := AddBackslash(Dir) + HandlerDll + '.' + IntToStr(I) + '.old';
    I := I + 1;
  until not FileExists(Result);
end;

{ The DLL and every copy of it renamed by an earlier run, in Dir. }
function HandlerFiles(Dir: String): TStringList;
var
  FindRec: TFindRec;
begin
  Result := TStringList.Create;
  if FindFirst(AddBackslash(Dir) + HandlerDll + '*', FindRec) then
  try
    repeat
      Result.Add(AddBackslash(Dir) + FindRec.Name);
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

{ Before the files are copied: deletes what it can, and renames the DLL
  itself if it is loaded, so the new one can take its name. }
procedure MoveHandlerAside(Dir: String);
var
  Files: TStringList;
  I: Integer;
begin
  Files := HandlerFiles(Dir);
  try
    for I := 0 to Files.Count - 1 do
      if not DeleteFile(Files[I]) and (CompareText(ExtractFileName(Files[I]), HandlerDll) = 0) then
        if not RenameFile(Files[I], UnusedOldName(Dir)) then
          Log('Could not delete or rename ' + Files[I]);
  finally
    Files.Free;
  end;
end;

{ Before uninstalling: anything still loaded goes to the user's temporary
  folder, so the install folder can be removed. It is deleted from there
  whenever Windows or the user clears that folder. }
procedure MoveHandlerOut(Dir: String);
var
  Files: TStringList;
  I: Integer;
begin
  Files := HandlerFiles(Dir);
  try
    for I := 0 to Files.Count - 1 do
      if not DeleteFile(Files[I]) then
        if not RenameFile(Files[I], UnusedOldName(GetTempDir)) then
          Log('Could not delete or move ' + Files[I] + '; the install folder stays until it is free');
  finally
    Files.Free;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    MoveHandlerAside(ExpandConstant('{app}'));
  if CurStep = ssPostInstall then
    AddToPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    MoveHandlerOut(ExpandConstant('{app}'));
  if CurUninstallStep = usPostUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
