; Inno Setup 6 script for the native XR MultiCast app. Built by packaging/native/build-windows.ps1:
;   ISCC /DAppVersion=2.0.0 /DSourceDir=<staged app folder> /DOutputDir=<dist> /DRepoRoot=<repo> xr-multicast.iss
; Installs per user (no admin) by default; the dialog offers an all-users install.

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif
#ifndef SourceDir
  #error Pass /DSourceDir=<staged app folder>
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef RepoRoot
  #define RepoRoot "..\.."
#endif

[Setup]
AppId={{6B1E5C9A-3D3F-4C59-9E0A-58D2A7C4F1B2}
AppName=XR MultiCast
AppVersion={#AppVersion}
AppVerName=XR MultiCast {#AppVersion}
AppPublisher=Dunk XR / Dunk Technologies
AppPublisherURL=https://github.com/smitdylan2001/xr-multicast
AppSupportURL=https://github.com/smitdylan2001/xr-multicast/issues
AppUpdatesURL=https://github.com/smitdylan2001/xr-multicast/releases
DefaultDirName={autopf}\XR MultiCast
DefaultGroupName=XR MultiCast
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=xr-multicast-{#AppVersion}-windows-x64-setup
SetupIconFile={#RepoRoot}\packaging\windows\xr-multicast.ico
UninstallDisplayIcon={app}\xr-multicast.exe
UninstallDisplayName=XR MultiCast
VersionInfoVersion={#AppVersion}
VersionInfoProductName=XR MultiCast
VersionInfoCompany=Dunk Technologies
LicenseFile={#SourceDir}\LICENSE.txt
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\XR MultiCast"; Filename: "{app}\xr-multicast.exe"; Comment: "Cast and monitor VR headset screens"
Name: "{autodesktop}\XR MultiCast"; Filename: "{app}\xr-multicast.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\xr-multicast.exe"; Description: "{cm:LaunchProgram,XR MultiCast}"; Flags: nowait postinstall skipifsilent
