; Per-user versioned installs preserve the binaries of every live Holder.
#ifndef Version
  #error Version is required
#endif
#ifndef Architecture
  #error Architecture is required
#endif
#ifndef Payload
  #error Payload is required
#endif
[Setup]
AppId=com.dirijor.diri
AppName=Diri
AppVersion={#Version}
VersionInfoVersion={#Version}.0
VersionInfoProductVersion={#Version}
VersionInfoProductName=Diri
DefaultDirName={localappdata}\Programs\Diri
DefaultGroupName=Diri
PrivilegesRequired=lowest
#if Architecture == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
MinVersion=10.0.22621
DisableProgramGroupPage=yes
CloseApplications=no
RestartApplications=no
UninstallDisplayIcon={app}\versions\{#Version}\diri.exe
OutputBaseFilename=diri-{#Version}-windows-{#Architecture}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
[Files]
Source: "{#Payload}\*"; DestDir: "{app}\versions\{#Version}"; Flags: ignoreversion recursesubdirs createallsubdirs
[Icons]
Name: "{autoprograms}\Diri"; Filename: "{app}\versions\{#Version}\diri.exe"; AppUserModelID: "com.dirijor.diri"
[Run]
Filename: "{app}\versions\{#Version}\diri.exe"; Description: "Open Diri"; Flags: nowait postinstall skipifsilent
Filename: "{app}\versions\{#Version}\diri.exe"; Flags: nowait; Check: RelaunchAfterUpdate
[Code]
function OpenProcess(Access: LongWord; Inherit: Boolean; Pid: LongWord): THandle;
  external 'OpenProcess@kernel32.dll stdcall';
function WaitForSingleObject(Handle: THandle; Milliseconds: LongWord): LongWord;
  external 'WaitForSingleObject@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';
function InitializeSetup(): Boolean;
var Pid: Integer; Process: THandle;
begin
  Result := True;
  Pid := StrToIntDef(ExpandConstant('{param:DiriWaitPID|0}'), 0);
  if Pid <= 0 then exit;
  Process := OpenProcess($00100000, False, Pid);
  if Process = 0 then exit;
  Result := WaitForSingleObject(Process, 60000) = 0;
  CloseHandle(Process);
end;
function RelaunchAfterUpdate(): Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:DiriRelaunch|0}') = '1');
end;
