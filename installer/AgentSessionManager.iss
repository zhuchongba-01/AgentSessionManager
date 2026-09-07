#define MyAppName "Agent会话管理器"
#define MyAppVersion "1.3.9"
#define MyAppExeName "Agent会话管理器.exe"

[Setup]
AppId={{7C1E5A93-9D4E-4F5C-B8A2-1E6D0C4B9F31}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
DefaultDirName={autopf}\Agent会话管理器
DefaultGroupName={#MyAppName}
UninstallDisplayName={#MyAppName}
OutputDir=Output
OutputBaseFilename=AgentSessionManager-Setup-v{#MyAppVersion}
SetupIconFile=..\src-tauri\icons\icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
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
