Unicode true
RequestExecutionLevel user
SetCompressor /SOLID lzma
!include "MUI2.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !error "Build with native/build.py --installer"
!endif

!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu"
!define PRODUCTKEY "Software\CateDesu\Kurisu"
!define CANDIDATEKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu Native"
!define MUI_ICON "kurisu.ico"
!define MUI_UNICON "kurisu.ico"
Name "Kurisu"
OutFile "${OUTPUT}"
InstallDir "$LOCALAPPDATA\Kurisu"
InstallDirRegKey HKCU "${PRODUCTKEY}" ""
VIProductVersion "${WINDOWS_VERSION}"
VIAddVersionKey /LANG=1033 "ProductName" "Kurisu"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "FileDescription" "Kurisu installer"
VIAddVersionKey /LANG=1033 "LegalCopyright" "CateDesu"

Var CandidateDir
Var StopAttempts

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\kurisu.exe"
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

; NSIS runs as x86, so PROCESSENTRY32W has a 36-byte header.
!macro ProcessGuard Prefix
Function ${Prefix}EnsureStopped
  StrCpy $StopAttempts 0
  retry:
  StrCpy $5 1
  System::Call 'kernel32::CreateToolhelp32Snapshot(i 2, i 0) p .r0'
  StrCmp $0 -1 done
  System::Alloc 556
  Pop $1
  StrCmp $1 0 close
  System::Call '*$1(i 556)'
  System::Call 'kernel32::Process32FirstW(p r0, p r1) i .r2'
  StrCmp $2 0 free
  loop:
    IntOp $3 $1 + 36
    System::Call '*$3(&w260 .r4)'
    StrCmp $4 "kurisu.exe" free
    StrCmp $4 "kurisu-native.exe" free
    System::Call 'kernel32::Process32NextW(p r0, p r1) i .r2 ?e'
    Pop $6
    StrCmp $2 0 finished loop
  finished:
    ${If} $6 == 18
      StrCpy $5 0
    ${EndIf}
  free:
    System::Free $1
  close:
    System::Call 'kernel32::CloseHandle(p r0)'
  done:
  ${If} $5 <> 0
    IntOp $StopAttempts $StopAttempts + 1
    ${If} $StopAttempts < 20
      Sleep 500
      Goto retry
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "Quit Kurisu and Kurisu Native, including their tray processes, then retry. No program or profile has been removed." /SD IDCANCEL IDRETRY retry
    Abort
  ${EndIf}
FunctionEnd
!macroend
!insertmacro ProcessGuard ""
!insertmacro ProcessGuard "un."

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_OK|MB_ICONSTOP "Kurisu requires 64-bit Windows."
    Abort
  ${EndIf}
  SetShellVarContext current
  SetRegView 64
  ReadRegStr $0 HKLM "${UNINSTKEY}" "UninstallString"
  ${If} $0 != ""
    MessageBox MB_OK|MB_ICONSTOP "An all-users Kurisu installation exists. Keep it and contact the maintainer before installing this per-user release."
    Abort
  ${EndIf}
  SetRegView 32
  ReadRegStr $0 HKLM "${UNINSTKEY}" "UninstallString"
  ${If} $0 != ""
    MessageBox MB_OK|MB_ICONSTOP "An all-users Kurisu installation exists. Keep it and contact the maintainer before installing this per-user release."
    Abort
  ${EndIf}
  ReadRegStr $CandidateDir HKCU "Software\Kurisu Native" "InstallDir"
  SetRegView 64
  ReadRegStr $0 HKCU "${PRODUCTKEY}" ""
  ${If} $0 != ""
    StrCpy $INSTDIR $0
  ${EndIf}
FunctionEnd

!macro CheckInstalledFile
  ${If} ${Errors}
    MessageBox MB_OK|MB_ICONSTOP "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. Installation is incomplete." /SD IDOK
    SetErrorLevel 1
    Quit
  ${EndIf}
!macroend

Section "Kurisu"
  Call EnsureStopped
  SetOutPath "$INSTDIR"
  ClearErrors
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ${If} ${Errors}
    MessageBox MB_OK|MB_ICONSTOP "Kurisu could not create its uninstaller. Close programs using this installation and check the folder permissions, then retry. The program and its registration have not been replaced." /SD IDOK
    SetErrorLevel 1
    Quit
  ${EndIf}
  ClearErrors
  File /oname=kurisu.exe "${BINARY}"
  !insertmacro CheckInstalledFile
  ClearErrors
  File "..\..\LICENSE"
  !insertmacro CheckInstalledFile
  ClearErrors
  File /oname=THIRD_PARTY_NOTICES.txt "${NOTICES}"
  !insertmacro CheckInstalledFile
  ClearErrors
  File /oname=PROFILE_TRANSFER.md "..\PROFILE_TRANSFER.md"
  !insertmacro CheckInstalledFile
  ClearErrors
  File /oname=kurisu.ico "kurisu.ico"
  !insertmacro CheckInstalledFile
  IfFileExists "$SMPROGRAMS\Kurisu.lnk" +2 0
    CreateShortcut "$SMPROGRAMS\Kurisu.lnk" "$INSTDIR\kurisu.exe" "" "$INSTDIR\kurisu.ico"
  WriteRegStr HKCU "${PRODUCTKEY}" "" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "" "Kurisu"
  WriteRegStr HKCU "${UNINSTKEY}" "MainBinaryName" "kurisu.exe"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "Kurisu"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "CateDesu"
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" '$\"$INSTDIR$\"'
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" '$\"$INSTDIR\kurisu.ico$\"'
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${If} $CandidateDir == "$LOCALAPPDATA\Programs\Kurisu Native"
    Delete "$CandidateDir\kurisu-native.exe"
    Delete "$CandidateDir\LICENSE"
    Delete "$CandidateDir\THIRD_PARTY_NOTICES.txt"
    Delete "$CandidateDir\migrate.py"
    Delete "$CandidateDir\PROFILE_TRANSFER.md"
    Delete "$CandidateDir\uninstall.exe"
    RMDir "$CandidateDir"
    Delete "$SMPROGRAMS\Kurisu Native.lnk"
    SetRegView 32
    DeleteRegKey HKCU "Software\Kurisu Native"
    DeleteRegKey HKCU "${CANDIDATEKEY}"
    SetRegView 64
  ${EndIf}
SectionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
FunctionEnd

Section "Uninstall"
  Call un.EnsureStopped
  ClearErrors
  Delete "$INSTDIR\kurisu.exe"
  ${If} ${Errors}
    MessageBox MB_OK|MB_ICONSTOP "Kurisu could not remove its executable. Close programs using the installation folder and check its permissions, then retry uninstalling. The uninstall entry and shortcuts have been kept." /SD IDOK
    SetErrorLevel 1
    Quit
  ${EndIf}
  Delete "$SMPROGRAMS\Kurisu.lnk"
  Delete "$DESKTOP\Kurisu.lnk"
  Delete "$INSTDIR\kurisu.ico"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.txt"
  Delete "$INSTDIR\PROFILE_TRANSFER.md"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKCU "${PRODUCTKEY}"
  DeleteRegKey HKCU "${UNINSTKEY}"
SectionEnd
