; The tacoshell installer (Inno Setup 6). CI builds it with
;   iscc /DAppVersion=0.1.0 tacoshell\installer\tacoshell.iss
; after `cargo build --release`, and it lands in target\tacoshell-installer.
;
; It installs for the current user by default (no admin needed; the first
; page offers everyone instead), puts tacoshell on PATH so tacoterm and other
; terminals find it, and adds a Windows Terminal profile. tacoterm's own
; installer can ship tacoshell.exe beside tacoterm.exe instead: the two
; don't clash (separate app ids and folders).

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\target\release"
#endif

[Setup]
AppId={{6F1C2A0E-5B7D-4C1E-9A43-7A3E2C8D9B10}
AppName=tacoshell
AppVersion={#AppVersion}
AppVerName=tacoshell {#AppVersion}
AppPublisher=Tacoterm
AppPublisherURL=https://github.com/tacobellerontop-sudo/Tacoterm
DefaultDirName={autopf}\tacoshell
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesEnvironment=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\target\tacoshell-installer
OutputBaseFilename=tacoshell-setup-{#AppVersion}
UninstallDisplayIcon={app}\tacoshell.exe
UninstallDisplayName=tacoshell
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Tasks]
Name: path; Description: "Add tacoshell to PATH (so tacoterm and other terminals find it)"
Name: terminal; Description: "Add a tacoshell profile to Windows Terminal"
Name: startmenu; Description: "Add tacoshell to the Start menu"

[Files]
Source: "{#SourceDir}\tacoshell.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\tacoshell"; Filename: "{app}\tacoshell.exe"; WorkingDir: "{%USERPROFILE}"; Tasks: startmenu

[Run]
Filename: "{app}\tacoshell.exe"; Description: "Start tacoshell"; WorkingDir: "{%USERPROFILE}"; Flags: nowait postinstall skipifsilent unchecked

[UninstallDelete]
Type: filesandordirs; Name: "{code:FragmentDir}"

[Code]
// PATH lives in the user's environment, or the machine's when installing
// for everyone.
function EnvRoot: Integer;
begin
  if IsAdminInstallMode then
    Result := HKEY_LOCAL_MACHINE
  else
    Result := HKEY_CURRENT_USER;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then
    Result := 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
  else
    Result := 'Environment';
end;

procedure AddToPath(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Paths: String;
  At: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Paths) then
    exit;
  Paths := ';' + Paths + ';';
  At := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if At = 0 then
    exit;
  Delete(Paths, At, Length(Dir) + 1);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Copy(Paths, 2, Length(Paths) - 2));
end;

// Windows Terminal picks up profiles from "fragments" in this folder.
function FragmentDir(Param: String): String;
begin
  if IsAdminInstallMode then
    Result := ExpandConstant('{commonappdata}')
  else
    Result := ExpandConstant('{localappdata}');
  Result := Result + '\Microsoft\Windows Terminal\Fragments\tacoshell';
end;

procedure WriteTerminalProfile;
var
  Exe, Json: String;
begin
  Exe := ExpandConstant('{app}\tacoshell.exe');
  StringChangeEx(Exe, '\', '\\', True);
  Json :=
    '{' + #13#10 +
    '  "profiles": [' + #13#10 +
    '    {' + #13#10 +
    '      "guid": "{6f1c2a0e-5b7d-4c1e-9a43-7a3e2c8d9b11}",' + #13#10 +
    '      "name": "tacoshell",' + #13#10 +
    '      "commandline": "' + Exe + '",' + #13#10 +
    '      "icon": "' + Exe + '",' + #13#10 +
    '      "startingDirectory": "%USERPROFILE%"' + #13#10 +
    '    }' + #13#10 +
    '  ]' + #13#10 +
    '}' + #13#10;
  ForceDirectories(FragmentDir(''));
  SaveStringToFile(FragmentDir('') + '\tacoshell.json', Json, False);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('path') then
      AddToPath(ExpandConstant('{app}'));
    if WizardIsTaskSelected('terminal') then
      WriteTerminalProfile;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveFromPath(ExpandConstant('{app}'));
end;
