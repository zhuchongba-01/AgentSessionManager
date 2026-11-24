#define MyAppName "Agent会话管理"
#define MyAppVersion "1.6.0"
#define MyAppExeName "Agent会话管理.exe"

[Setup]
AppId={{7C1E5A93-9D4E-4F5C-B8A2-1E6D0C4B9F31}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppMutex=com.agent-session-manager.desktop-sim
DefaultDirName={autopf}\Agent会话管理
DefaultGroupName={#MyAppName}
UninstallDisplayName={#MyAppName}
OutputDir=Output
OutputBaseFilename=AgentSessionManager-Setup-v{#MyAppVersion}
SetupIconFile=..\src-tauri\icons\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DisableProgramGroupPage=yes
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务:"

[Files]
Source: "..\src-tauri\target\release\agent-session-manager.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "立即运行 {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\com.agent-session-manager.desktop\EBWebView"

[Code]
procedure StopRunningInstance();
var
  AppExe: String;
  ResultCode: Integer;
  ElapsedMs: Integer;
begin
  if not CheckForMutexes('com.agent-session-manager.desktop-sim') then
    Exit;

  AppExe := ExpandConstant('{app}\{#MyAppExeName}');
  if FileExists(AppExe) then
  begin
    if Exec(AppExe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    begin
      ElapsedMs := 0;
      while CheckForMutexes('com.agent-session-manager.desktop-sim') and (ElapsedMs < 3000) do
      begin
        Sleep(100);
        ElapsedMs := ElapsedMs + 100;
      end;
      Sleep(400);
    end;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  StopRunningInstance();
end;

function InitializeUninstall(): Boolean;
begin
  StopRunningInstance();
  Result := True;
end;
