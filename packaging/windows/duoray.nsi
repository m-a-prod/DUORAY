; DUORAY installer for Windows (NSIS 3).
;
; Installs the app with xray + Wintun, registers the TUN helper as a Windows
; service (the only admin step — connecting later needs no UAC prompt),
; creates shortcuts and an entry in "Apps & features".
;
; Build (from this directory): makensis duoray.nsi

Unicode true
SetCompressor /SOLID lzma

!define APP "DUORAY"
!ifndef VERSION
  !define VERSION "0.3.1"
!endif
; x64 (default) or x86. Wintun must match the OS bitness, so each build
; only installs on its own architecture.
!ifndef ARCH
  !define ARCH "x64"
!endif
!define STAGE "stage-${ARCH}"
!define SERVICE "DuorayHelper"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\DUORAY"

Name "${APP}"
OutFile "..\..\dist\DUORAY-Setup-${VERSION}-${ARCH}.exe"
!if "${ARCH}" == "x64"
  InstallDir "$PROGRAMFILES64\DUORAY"
!else
  InstallDir "$PROGRAMFILES\DUORAY"
!endif
RequestExecutionLevel admin
; Upgrades (also the silent ones the app starts) go where DUORAY already is.
InstallDirRegKey HKLM "${UNINST_KEY}" "InstallLocation"
BrandingText "DUORAY ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "DUORAY"
VIAddVersionKey "FileDescription" "DUORAY Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Dualizm"

!include "MUI2.nsh"
!include "x64.nsh"

!define MUI_ICON "duoray.ico"
!define MUI_UNICON "duoray.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\duoray.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Запустить DUORAY"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "Russian"

Function .onInit
!if "${ARCH}" == "x64"
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Это 64-битная версия DUORAY. Для 32-битной Windows скачайте установщик x86."
    Abort
  ${EndIf}
  SetRegView 64
!else
  ${If} ${RunningX64}
    MessageBox MB_ICONSTOP "Это 32-битная версия DUORAY, а Windows у вас 64-битная. Скачайте установщик x64 — драйвер туннеля должен совпадать с разрядностью системы."
    Abort
  ${EndIf}
  SetRegView 32
!endif
FunctionEnd

; Stops a running DUORAY and its service so the files can be replaced.
!macro StopRunning
  nsExec::Exec 'taskkill /IM duoray.exe /F'
  nsExec::Exec 'sc.exe stop ${SERVICE}'
  Sleep 2500
  nsExec::Exec 'taskkill /IM xray.exe /F'
!macroend

Section "DUORAY" SecMain
  SectionIn RO
  SetShellVarContext all
  !insertmacro StopRunning

  SetOutPath "$INSTDIR"
  File "${STAGE}\duoray.exe"
  File "${STAGE}\duoray-helper.exe"
  File "${STAGE}\xray.exe"
  File "${STAGE}\geoip.dat"
  File "${STAGE}\geosite.dat"
  File "${STAGE}\wintun.dll"
  File "${STAGE}\LICENSE-xray.txt"
  File "${STAGE}\LICENSE-wintun.txt"
  File "${STAGE}\LICENSE.txt"
  File "${STAGE}\LICENSE-EXCEPTION.md"
  File "${STAGE}\THIRD-PARTY-NOTICES.md"
  File "${STAGE}\THIRD-PARTY-CRATES.txt"
  File "duoray.ico"

  ; TUN helper service: recreate so an upgrade always points at the new binary.
  nsExec::Exec 'sc.exe delete ${SERVICE}'
  Sleep 1000
  nsExec::ExecToLog 'sc.exe create ${SERVICE} binPath= "\"$INSTDIR\duoray-helper.exe\" --service" start= auto DisplayName= "DUORAY TUN helper"'
  nsExec::ExecToLog 'sc.exe description ${SERVICE} "DUORAY: поднимает VPN-туннель (TUN) по запросу приложения"'
  nsExec::ExecToLog 'sc.exe failure ${SERVICE} reset= 60 actions= restart/2000/restart/5000/restart/10000'
  nsExec::ExecToLog 'sc.exe start ${SERVICE}'

  ; The icon lives inside duoray.exe; recreate shortcuts and tell Explorer to
  ; drop its cached icons, so an upgrade never shows the old one.
  Delete "$SMPROGRAMS\DUORAY.lnk"
  Delete "$DESKTOP\DUORAY.lnk"
  CreateShortcut "$SMPROGRAMS\DUORAY.lnk" "$INSTDIR\duoray.exe" "" "$INSTDIR\duoray.exe" 0
  CreateShortcut "$DESKTOP\DUORAY.lnk" "$INSTDIR\duoray.exe" "" "$INSTDIR\duoray.exe" 0
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  nsExec::Exec 'ie4uinit.exe -show'

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "DUORAY"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "Dualizm"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\duoray.exe,0"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "EstimatedSize" 92000

  ; A silent run is the in-app update: start the new version again. Through
  ; explorer.exe it runs as the user, not elevated like this installer.
  IfSilent 0 +2
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\duoray.exe"'
SectionEnd

Function un.onInit
!if "${ARCH}" == "x64"
  SetRegView 64
!else
  SetRegView 32
!endif
FunctionEnd

Section "Uninstall"
  SetShellVarContext all
  ; Stopping the service restores routes and DNS if a tunnel is up.
  nsExec::Exec 'taskkill /IM duoray.exe /F'
  nsExec::ExecToLog 'sc.exe stop ${SERVICE}'
  Sleep 3000
  nsExec::ExecToLog 'sc.exe delete ${SERVICE}'
  nsExec::Exec 'taskkill /IM xray.exe /F'
  ; Leftover firewall rule if the service died mid-session.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="DUORAY DNS guard"'

  Delete "$INSTDIR\duoray.exe"
  Delete "$INSTDIR\duoray-helper.exe"
  Delete "$INSTDIR\xray.exe"
  Delete "$INSTDIR\geoip.dat"
  Delete "$INSTDIR\geosite.dat"
  Delete "$INSTDIR\wintun.dll"
  Delete "$INSTDIR\LICENSE-xray.txt"
  Delete "$INSTDIR\LICENSE-wintun.txt"
  Delete "$INSTDIR\LICENSE.txt"
  Delete "$INSTDIR\LICENSE-EXCEPTION.md"
  Delete "$INSTDIR\THIRD-PARTY-NOTICES.md"
  Delete "$INSTDIR\THIRD-PARTY-CRATES.txt"
  Delete "$INSTDIR\duoray.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  Delete "$SMPROGRAMS\DUORAY.lnk"
  Delete "$DESKTOP\DUORAY.lnk"
  DeleteRegKey HKLM "${UNINST_KEY}"
SectionEnd
