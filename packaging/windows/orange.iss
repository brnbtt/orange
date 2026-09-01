#ifndef AppVersion
#error AppVersion was not defined. Build through package.ps1, which passes /DAppVersion from the workspace version.
#endif

[Setup]
AppId={{3F4D13A1-E6A0-49BA-97D6-67DAF8938677}
AppName=orange
AppVersion={#AppVersion}
AppVerName=orange
AppPublisher=orange
AppPublisherURL=https://github.com/brnbtt/orange
DefaultDirName={localappdata}\Programs\orange
DefaultGroupName=orange
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=orange-setup-{#AppVersion}
SetupIconFile=..\..\crates\orange-tray\icon.ico
UninstallDisplayIcon={app}\orange-tray.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableWelcomePage=yes
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
DisableFinishedPage=yes
DisableStartupPrompt=yes
AllowCancelDuringInstall=no
SetupLogging=yes
CloseApplications=force
RestartApplications=no

[Files]
Source: "..\..\target\release\orange.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\orange-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\orange-updater.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\package\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion
; The media runtime travels with us. package.ps1 stages the slice orange loads
; and proves every element resolves from it. GStreamer locates its own plugins
; relative to bin\gstreamer-1.0-0.dll, so this layout needs no environment.
Source: "..\..\target\package\gstreamer\*"; DestDir: "{app}\gstreamer"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\orange"; Filename: "{app}\orange-tray.exe"

[Run]
Filename: "{app}\orange-tray.exe"; Description: "Open orange"; Flags: nowait skipifsilent

[Code]
const
  BN_CLICKED = 0;
  WM_COMMAND = $0111;
  CN_BASE = $BC00;
  CN_COMMAND = CN_BASE + WM_COMMAND;

procedure CurPageChanged(CurPageID: Integer);
var
  ClickNotification: Longint;
begin
  if (CurPageID = wpReady) and (not WizardSilent) then
  begin
    ClickNotification := BN_CLICKED shl 16;
    PostMessage(WizardForm.NextButton.Handle, CN_COMMAND, ClickNotification, 0);
  end;
end;
