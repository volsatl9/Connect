; The Windows installer, built by packaging/build.sh. It installs for the current user, without
; asking for administrator rights, because the app updates itself by replacing connect.exe in
; place, which it couldn't do under Program Files.

Target amd64-unicode
!include "MUI2.nsh"

!ifndef VERSION
  !error "VERSION isn't set"
!endif

!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Connect"

Name "Connect"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Programs\Connect"
InstallDirRegKey HKCU "${UNINSTALL_KEY}" "InstallLocation"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "Connect"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "Connect"
VIAddVersionKey "FileDescription" "Connect installer"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "MIT OR Apache-2.0"

!define MUI_ICON "..\..\app\assets\icon\connect.ico"
!define MUI_UNICON "..\..\app\assets\icon\connect.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\connect.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start Connect"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Connect"
  SetOutPath "$INSTDIR"
  File "..\..\target\dist\connect.exe"
  WriteUninstaller "$INSTDIR\Uninstall Connect.exe"
  CreateShortcut "$SMPROGRAMS\Connect.lnk" "$INSTDIR\connect.exe"
  CreateShortcut "$DESKTOP\Connect.lnk" "$INSTDIR\connect.exe"

  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "Connect"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\connect.exe"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "Connect"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\Uninstall Connect.exe"'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
SectionEnd

; Saved computers, the User-ID and settings stay, in case Connect is being reinstalled.
Section "Uninstall"
  Delete "$INSTDIR\connect.exe"
  Delete "$INSTDIR\connect.exe.old"
  Delete "$INSTDIR\connect.exe.download"
  Delete "$INSTDIR\Uninstall Connect.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\Connect.lnk"
  Delete "$DESKTOP\Connect.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
SectionEnd
