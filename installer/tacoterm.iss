; Inno Setup script for the tacoterm installer.
;
; Build (scripts/build-installer.ps1 does all of this):
;   iscc /DAppVersion=0.1.0 /DSourceDir=C:\src\Tacoterm\target\release installer\tacoterm.iss
;
; SourceDir (an absolute path) must hold tacoterm.exe, conpty.dll and
; OpenConsole.exe; tacoshell.exe goes in too when it is there.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#ifndef SourceDir
  #define SourceDir AddBackslash(SourcePath) + "..\target\release"
#endif
#ifndef OutputDir
  #define OutputDir AddBackslash(SourcePath) + "..\target\installer"
#endif
#define HaveTacoshell FileExists(SourceDir + "\tacoshell.exe")

[Setup]
AppId={{6B1E0C59-3F0E-4C61-9A0A-7A1C0FFEE7AC}
AppName=tacoterm
AppVersion={#AppVersion}
AppVerName=tacoterm {#AppVersion}
AppPublisher=tacoterm
AppPublisherURL=https://github.com/tacobellerontop-sudo/Tacoterm
DefaultDirName={autopf}\tacoterm
DefaultGroupName=tacoterm
DisableProgramGroupPage=yes
; Installs for the current user without asking for admin; the first page
; offers "for all users" too.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
OutputDir={#OutputDir}
OutputBaseFilename=tacoterm-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
ChangesAssociations=yes
UninstallDisplayIcon={app}\tacoterm.exe
LicenseFile=..\LICENSE
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "explorer"; Description: "Add ""Open in tacoterm"" to folder right-click menus"; GroupDescription: "Explorer:"
Name: "path"; Description: "Add tacoterm to PATH (run it from any console)"; GroupDescription: "Command line:"
#if HaveTacoshell
Name: "tacoshelldefault"; Description: "Use tacoshell as the default shell"; GroupDescription: "Shell:"; Flags: unchecked
#endif

[Files]
Source: "{#SourceDir}\tacoterm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion
#if HaveTacoshell
Source: "{#SourceDir}\tacoshell.exe"; DestDir: "{app}"; Flags: ignoreversion
#endif
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\tacoterm"; Filename: "{app}\tacoterm.exe"; WorkingDir: "{%USERPROFILE}"
Name: "{autodesktop}\tacoterm"; Filename: "{app}\tacoterm.exe"; WorkingDir: "{%USERPROFILE}"; Tasks: desktopicon

[Registry]
; "Open in tacoterm" on folders, folder backgrounds and drives.
Root: HKA; Subkey: "Software\Classes\Directory\shell\tacoterm"; ValueType: string; ValueName: ""; ValueData: "Open in tacoterm"; Flags: uninsdeletekey; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Directory\shell\tacoterm"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\tacoterm.exe"; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Directory\shell\tacoterm\command"; ValueType: string; ValueName: ""; ValueData: """{app}\tacoterm.exe"" --cwd ""%V\."""; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\tacoterm"; ValueType: string; ValueName: ""; ValueData: "Open in tacoterm"; Flags: uninsdeletekey; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\tacoterm"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\tacoterm.exe"; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Directory\Background\shell\tacoterm\command"; ValueType: string; ValueName: ""; ValueData: """{app}\tacoterm.exe"" --cwd ""%V\."""; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Drive\shell\tacoterm"; ValueType: string; ValueName: ""; ValueData: "Open in tacoterm"; Flags: uninsdeletekey; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Drive\shell\tacoterm"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\tacoterm.exe"; Tasks: explorer
Root: HKA; Subkey: "Software\Classes\Drive\shell\tacoterm\command"; ValueType: string; ValueName: ""; ValueData: """{app}\tacoterm.exe"" --cwd ""%V\."""; Tasks: explorer
; Found by "App Paths", so Win+R → tacoterm works even without PATH.
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\tacoterm.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\tacoterm.exe"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\tacoterm.exe"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

[Run]
#if HaveTacoshell
Filename: "{app}\tacoterm.exe"; Parameters: "--use-shell ""{app}\tacoshell.exe"""; StatusMsg: "Making tacoshell the default shell..."; Flags: runasoriginaluser waituntilterminated; Tasks: tacoshelldefault
#endif
Filename: "{app}\tacoterm.exe"; Description: "Start tacoterm"; WorkingDir: "{%USERPROFILE}"; Flags: nowait postinstall skipifsilent

[Code]
const
  UserEnv = 'Environment';
  SystemEnv = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';

function EnvRoot: Integer;
begin
  if IsAdminInstallMode then Result := HKEY_LOCAL_MACHINE else Result := HKEY_CURRENT_USER;
end;

function EnvKey: String;
begin
  if IsAdminInstallMode then Result := SystemEnv else Result := UserEnv;
end;

function PathHas(Paths, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToPath(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Paths) then Paths := '';
  if PathHas(Paths, Dir) then exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then Paths := Paths + ';';
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Paths + Dir);
end;

procedure RemoveFromPath(Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(EnvRoot, EnvKey, 'Path', Paths) then exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(EnvRoot, EnvKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('path') then AddToPath(ExpandConstant('{app}'));
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then RemoveFromPath(ExpandConstant('{app}'));
end;
