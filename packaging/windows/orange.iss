#ifndef AppVersion
#define AppVersion "0.1.0"
#endif

#define GStreamerVersion "1.28.6"
#define GStreamerFile "gstreamer-1.0-msvc-x86_64-" + GStreamerVersion + ".exe"
#define GStreamerUrl "https://gstreamer.freedesktop.org/data/pkg/windows/" + GStreamerVersion + "/msvc/" + GStreamerFile
#define GStreamerSha256 "059251444d1267b486eba390b18d25fed87e10315e72f757ec6c7e912fa746b5"

[Setup]
AppId={{3F4D13A1-E6A0-49BA-97D6-67DAF8938677}
AppName=orange
AppVersion={#AppVersion}
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
Source: "..\..\target\package\vc_redist.x64.exe"; Flags: dontcopy

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

var
  DownloadPage: TDownloadWizardPage;

function HasGStreamerAt(const Root: String): Boolean;
begin
  Result :=
    FileExists(AddBackslash(Root) + 'bin\gstreamer-1.0-0.dll') and
    FileExists(AddBackslash(Root) + 'lib\gstreamer-1.0\gstd3d11.dll') and
    FileExists(AddBackslash(Root) + 'lib\gstreamer-1.0\gstwebrtc.dll') and
    FileExists(AddBackslash(Root) + 'lib\gstreamer-1.0\gstnvcodec.dll');
end;

function GStreamerReady: Boolean;
begin
  Result :=
    HasGStreamerAt(GetEnv('GSTREAMER_1_0_ROOT_MSVC_X86_64')) or
    HasGStreamerAt(ExpandConstant('{localappdata}\Programs\gstreamer\1.0\msvc_x86_64')) or
    HasGStreamerAt(ExpandConstant('{pf}\gstreamer\1.0\msvc_x86_64')) or
    HasGStreamerAt('C:\gstreamer\1.0\msvc_x86_64');
end;

function VCRuntimeReady: Boolean;
var
  Installed: Cardinal;
begin
  Result :=
    RegQueryDWordValue(
      HKLM64,
      'SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64',
      'Installed',
      Installed) and
    (Installed = 1);
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(
    'Preparing the media runtime',
    'orange needs the official GStreamer runtime for video and audio.',
    nil);
  DownloadPage.ShowBaseNameInsteadOfUrl := True;
end;

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

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  if not VCRuntimeReady then
  begin
    ExtractTemporaryFile('vc_redist.x64.exe');
    if not Exec(
      ExpandConstant('{tmp}\vc_redist.x64.exe'),
      '/install /quiet /norestart',
      '', SW_SHOW, ewWaitUntilTerminated, ResultCode) then
    begin
      Result := 'Could not start the Microsoft Visual C++ runtime installer.';
      exit;
    end;

    if not ((ResultCode = 0) or (ResultCode = 1638) or (ResultCode = 3010)) then
    begin
      Result := Format('The Microsoft Visual C++ runtime installer failed with code %d.', [ResultCode]);
      exit;
    end;
    if ResultCode = 3010 then
      NeedsRestart := True;
  end;

  if GStreamerReady then
    exit;

  DownloadPage.Clear;
  DownloadPage.Add('{#GStreamerUrl}', '{#GStreamerFile}', '{#GStreamerSha256}');
  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
    except
      Result := 'Could not download the GStreamer runtime: ' + GetExceptionMessage;
      exit;
    end;
  finally
    DownloadPage.Hide;
  end;

  if not Exec(
    ExpandConstant('{tmp}\{#GStreamerFile}'),
    '/TYPE=runtime /CURRENTUSER /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-',
    '', SW_SHOW, ewWaitUntilTerminated, ResultCode) then
  begin
    Result := 'Could not start the GStreamer runtime installer.';
    exit;
  end;

  if ResultCode <> 0 then
  begin
    Result := Format('The GStreamer runtime installer failed with code %d.', [ResultCode]);
    exit;
  end;

  if not GStreamerReady then
    Result := 'GStreamer finished installing, but orange could not find the required media plugins.';
end;
