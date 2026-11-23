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
Var InstallerMutex
Var RestoreFailed
Var SwapFailure
Var OwnsRegistration
Var KeyOwned
Var BackedUpExe
Var PlacedExe
Var BackedUpUninstaller
Var PlacedUninstaller
Var BackedUpLicense
Var PlacedLicense
Var BackedUpNotices
Var PlacedNotices
Var BackedUpTransfer
Var PlacedTransfer
Var BackedUpIcon
Var PlacedIcon

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
    Call ${Prefix}CloseInstallerMutex
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
FunctionEnd

!macro FailWithMessage Prefix Message
  Call ${Prefix}CloseInstallerMutex
  MessageBox MB_OK|MB_ICONSTOP "${Message}" /SD IDOK
  Call ${Prefix}FailInstall
!macroend

!macro MutexGuard Prefix
Function ${Prefix}AcquireInstallerMutex
  System::Call 'kernel32::CreateMutexW(p 0, i 0, w "Local\Kurisu-Installer-Lock") p .r0 ?e'
  Pop $1
  StrCpy $InstallerMutex $0
  ${If} $0 == 0
  ${OrIf} $1 == 183
    !insertmacro FailWithMessage "${Prefix}" "Kurisu could not lock the installation. Close other Kurisu installers and try again. The program and its registration have not been replaced."
  ${EndIf}
FunctionEnd

Function ${Prefix}CloseInstallerMutex
  ${If} $InstallerMutex != ""
  ${AndIf} $InstallerMutex != 0
    System::Call 'kernel32::CloseHandle(p $InstallerMutex)'
    StrCpy $InstallerMutex 0
  ${EndIf}
FunctionEnd

Function ${Prefix}FailInstall
  Call ${Prefix}CloseInstallerMutex
  SetErrorLevel 1
  Quit
FunctionEnd
!macroend
!insertmacro MutexGuard ""
!insertmacro MutexGuard "un."

!include "windows-registration.nsh"

!macro RecoverBackup Name
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp\${Name}.bak"
    System::Call 'kernel32::MoveFileExW(w "$INSTDIR\.kurisu-install.tmp\${Name}.bak", w "$INSTDIR\${Name}", i 1) i .r0'
    ${If} $0 == 0
      StrCpy $RestoreFailed 1
    ${EndIf}
  ${Else}
    ReadINIStr $1 "$INSTDIR\.kurisu-install.tmp\.swapping" "absent" "${Name}"
    ${If} $1 == 1
    ${AndIfNot} ${FileExists} "$INSTDIR\.kurisu-install.tmp\${Name}"
    ${AndIf} ${FileExists} "$INSTDIR\${Name}"
      ClearErrors
      Delete "$INSTDIR\${Name}"
      ${If} ${Errors}
        StrCpy $RestoreFailed 1
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

!macro SwapRecovery Prefix RetryAction
Function ${Prefix}RemoveSwapMarker
  StrCpy $2 0
  retry:
  Delete "$INSTDIR\.kurisu-install.tmp\.swapping"
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
    IntOp $2 $2 + 1
    ${If} $2 < 10
      Sleep 100
      Goto retry
    ${EndIf}
  ${EndIf}
FunctionEnd

Function ${Prefix}RecoverInterruptedSwap
  ${IfNot} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
    Return
  ${EndIf}
  StrCpy $RestoreFailed 0
  !insertmacro RecoverBackup "kurisu.exe"
  !if "${Prefix}" == "un."
    ; Keep the registered uninstaller usable until uninstall succeeds.
    ${IfNot} ${FileExists} "$INSTDIR\uninstall.exe"
      !insertmacro RecoverBackup "uninstall.exe"
    ${ElseIf} ${FileExists} "$INSTDIR\.kurisu-install.tmp\uninstall.exe.bak"
      ; Retain the swap marker if the backup cannot be removed.
      System::Call 'kernel32::CreateFileW(w "$INSTDIR\.kurisu-install.tmp\uninstall.exe.bak", i 0x10000, i 7, p 0, i 3, i 0, p 0) p .r0'
      ${If} $0 == -1
        StrCpy $RestoreFailed 1
      ${Else}
        System::Call 'kernel32::CloseHandle(p r0)'
      ${EndIf}
    ${EndIf}
  !else
    !insertmacro RecoverBackup "uninstall.exe"
  !endif
  !insertmacro RecoverBackup "LICENSE"
  !insertmacro RecoverBackup "THIRD_PARTY_NOTICES.txt"
  !insertmacro RecoverBackup "PROFILE_TRANSFER.md"
  !insertmacro RecoverBackup "kurisu.ico"
  ${If} $RestoreFailed == 0
    Call ${Prefix}RestoreRegistration
  ${EndIf}
  ${If} $RestoreFailed == 0
    Call ${Prefix}RemoveSwapMarker
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
    !insertmacro FailWithMessage "${Prefix}" "Kurisu could not restore its previous installation. Recovery files are in $INSTDIR\.kurisu-install.tmp. Check folder and registry permissions and ${RetryAction}."
  ${EndIf}
FunctionEnd
!macroend
!insertmacro SwapRecovery "" "run the installer again"
!insertmacro SwapRecovery "un." "retry uninstalling"

!macro CleanStagingDir Prefix
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp"
  ${AndIfNot} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
    ClearErrors
    Delete "$INSTDIR\.kurisu-install.tmp\kurisu.exe"
    Delete "$INSTDIR\.kurisu-install.tmp\uninstall.exe"
    Delete "$INSTDIR\.kurisu-install.tmp\LICENSE"
    Delete "$INSTDIR\.kurisu-install.tmp\THIRD_PARTY_NOTICES.txt"
    Delete "$INSTDIR\.kurisu-install.tmp\PROFILE_TRANSFER.md"
    Delete "$INSTDIR\.kurisu-install.tmp\kurisu.ico"
    Delete "$INSTDIR\.kurisu-install.tmp\kurisu.exe.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\uninstall.exe.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\LICENSE.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\THIRD_PARTY_NOTICES.txt.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\PROFILE_TRANSFER.md.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\kurisu.ico.bak"
    Delete "$INSTDIR\.kurisu-install.tmp\.registration"
    Delete "$INSTDIR\.kurisu-install.tmp\.registering"
    !if "${Prefix}" == "un."
      ${If} ${Errors}
        !insertmacro FailWithMessage "un." "Kurisu could not remove its staged files. Close programs using $INSTDIR\.kurisu-install.tmp and retry uninstalling."
      ${EndIf}
    !endif
    RMDir "$INSTDIR\.kurisu-install.tmp"
  ${EndIf}
!macroend

!macro CheckStagedFile
  ${If} ${Errors}
    SetOutPath "$INSTDIR"
    !insertmacro CleanStagingDir ""
    !insertmacro FailWithMessage "" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  ${EndIf}
!macroend

!macro RollbackSingleFile Name BackedUpVar PlacedVar
  ${If} ${BackedUpVar} == 1
    System::Call 'kernel32::MoveFileExW(w "$INSTDIR\.kurisu-install.tmp\${Name}.bak", w "$INSTDIR\${Name}", i 1) i .r0'
    ${If} $0 == 0
      StrCpy $RestoreFailed 1
    ${EndIf}
  ${ElseIf} ${PlacedVar} == 1
    ClearErrors
    Delete "$INSTDIR\${Name}"
    ${If} ${Errors}
      StrCpy $RestoreFailed 1
    ${EndIf}
  ${EndIf}
!macroend

Function RollbackStagedSwap
  !insertmacro RollbackSingleFile "kurisu.ico" $BackedUpIcon $PlacedIcon
  !insertmacro RollbackSingleFile "PROFILE_TRANSFER.md" $BackedUpTransfer $PlacedTransfer
  !insertmacro RollbackSingleFile "THIRD_PARTY_NOTICES.txt" $BackedUpNotices $PlacedNotices
  !insertmacro RollbackSingleFile "LICENSE" $BackedUpLicense $PlacedLicense
  ${If} $PlacedUninstaller == 1
    !insertmacro RollbackSingleFile "uninstall.exe" $BackedUpUninstaller $PlacedUninstaller
  ${EndIf}
  !insertmacro RollbackSingleFile "kurisu.exe" $BackedUpExe $PlacedExe
  ${If} $RestoreFailed == 0
    Call RestoreRegistration
  ${EndIf}
  ${If} $RestoreFailed == 0
    Call RemoveSwapMarker
    ${IfNot} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
      !insertmacro CleanStagingDir ""
      !insertmacro FailWithMessage "" "Kurisu could not $SwapFailure. Its previous files and registration have been restored. Check folder and registry permissions, then run the installer again."
    ${EndIf}
  ${EndIf}
  !insertmacro FailWithMessage "" "Kurisu could not finish restoring its previous installation. Recovery files are in $INSTDIR\.kurisu-install.tmp. Check folder and registry permissions, then run the installer again."
FunctionEnd

!macro RecordAbsentFile Name
  ClearErrors
  WriteINIStr "$INSTDIR\.kurisu-install.tmp\.swapping" "absent" "${Name}" "1"
  ${If} ${Errors}
    Call RollbackStagedSwap
  ${EndIf}
!macroend

!macro PlaceStagedFile Name PlacedVar
  System::Call 'kernel32::MoveFileExW(w "$INSTDIR\.kurisu-install.tmp\${Name}", w "$INSTDIR\${Name}", i 1) i .r0'
  ${If} $0 == 0
    Call RollbackStagedSwap
  ${EndIf}
  StrCpy ${PlacedVar} 1
!macroend

!macro SwapStagedFile Name BackedUpVar PlacedVar
  ${If} ${FileExists} "$INSTDIR\${Name}"
    System::Call 'kernel32::MoveFileExW(w "$INSTDIR\${Name}", w "$INSTDIR\.kurisu-install.tmp\${Name}.bak", i 1) i .r0'
    ${If} $0 == 0
      Call RollbackStagedSwap
    ${EndIf}
    StrCpy ${BackedUpVar} 1
  ${Else}
    !insertmacro RecordAbsentFile "${Name}"
  ${EndIf}
  !insertmacro PlaceStagedFile "${Name}" ${PlacedVar}
!macroend

; Probe targets with FILE_SHARE_READ before staging; same-volume MoveFileExW swaps staged payloads only after extraction completes.
!macro CheckWritableTarget Path Message
  System::Call 'kernel32::CreateFileW(w "${Path}", i 0x40000000, i 1, p 0, i 3, i 0x80, p 0) p .r0 ?e'
  Pop $1
  ${If} $0 == -1
    ${If} $1 <> 2
      !insertmacro FailWithMessage "" "${Message}"
    ${EndIf}
  ${Else}
    System::Call 'kernel32::CloseHandle(p r0)'
  ${EndIf}
!macroend

Section "Kurisu"
  Call EnsureStopped
  Call AcquireInstallerMutex
  ReadRegStr $0 HKCU "${PRODUCTKEY}" ""
  ${If} $0 != ""
    StrCpy $INSTDIR $0
  ${EndIf}
  ClearErrors
  SetOutPath "$INSTDIR"
  ${If} ${Errors}
    !insertmacro FailWithMessage "" "Kurisu could not prepare its installation folder. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  ${EndIf}
  Call RecoverInterruptedSwap
  !insertmacro CheckWritableTarget "$INSTDIR\uninstall.exe" "Kurisu could not create its uninstaller. Close programs using this installation and check the folder permissions, then retry. The program and its registration have not been replaced."
  !insertmacro CheckWritableTarget "$INSTDIR\kurisu.exe" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  !insertmacro CheckWritableTarget "$INSTDIR\LICENSE" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  !insertmacro CheckWritableTarget "$INSTDIR\THIRD_PARTY_NOTICES.txt" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  !insertmacro CheckWritableTarget "$INSTDIR\PROFILE_TRANSFER.md" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  !insertmacro CheckWritableTarget "$INSTDIR\kurisu.ico" "Kurisu could not install a required program file. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  !insertmacro CleanStagingDir ""
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp"
    !insertmacro FailWithMessage "" "Kurisu could not clear its staging folder. Close programs using $INSTDIR\.kurisu-install.tmp and retry."
  ${EndIf}
  ClearErrors
  SetOutPath "$INSTDIR\.kurisu-install.tmp"
  ${If} ${Errors}
    !insertmacro FailWithMessage "" "Kurisu could not prepare its installation folder. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  ${EndIf}
  ClearErrors
  File /oname=kurisu.exe "${BINARY}"
  !insertmacro CheckStagedFile
  ClearErrors
  File "..\..\LICENSE"
  !insertmacro CheckStagedFile
  ClearErrors
  File /oname=THIRD_PARTY_NOTICES.txt "${NOTICES}"
  !insertmacro CheckStagedFile
  ClearErrors
  File /oname=PROFILE_TRANSFER.md "..\PROFILE_TRANSFER.md"
  !insertmacro CheckStagedFile
  ClearErrors
  File /oname=kurisu.ico "kurisu.ico"
  !insertmacro CheckStagedFile
  ClearErrors
  WriteUninstaller "$INSTDIR\.kurisu-install.tmp\uninstall.exe"
  ${If} ${Errors}
    SetOutPath "$INSTDIR"
    !insertmacro CleanStagingDir ""
    !insertmacro FailWithMessage "" "Kurisu could not create its uninstaller. Close programs using this installation and check the folder permissions, then retry. The program and its registration have not been replaced."
  ${EndIf}
  SetOutPath "$INSTDIR"
  Call SnapshotRegistration
  ${If} $RegFailed != 0
    !insertmacro CleanStagingDir ""
    !insertmacro FailWithMessage "" "Kurisu could not back up its registration. Check folder and registry permissions, then run the installer again."
  ${EndIf}
  StrCpy $SwapFailure "replace its installed files"
  StrCpy $RestoreFailed 0
  StrCpy $BackedUpExe 0
  StrCpy $PlacedExe 0
  StrCpy $BackedUpUninstaller 0
  StrCpy $PlacedUninstaller 0
  StrCpy $BackedUpLicense 0
  StrCpy $PlacedLicense 0
  StrCpy $BackedUpNotices 0
  StrCpy $PlacedNotices 0
  StrCpy $BackedUpTransfer 0
  StrCpy $PlacedTransfer 0
  StrCpy $BackedUpIcon 0
  StrCpy $PlacedIcon 0
  ; Finish the backup before the swap marker makes it eligible for recovery.
  ${If} ${FileExists} "$INSTDIR\uninstall.exe"
    System::Call 'kernel32::CopyFileW(w "$INSTDIR\uninstall.exe", w "$INSTDIR\.kurisu-install.tmp\uninstall.exe.bak", i 1) i .r0'
    ${If} $0 == 0
      !insertmacro CleanStagingDir ""
      !insertmacro FailWithMessage "" "Kurisu could not back up its uninstaller. Close programs using this installation and check the folder permissions, then retry. The program and its registration have not been replaced."
    ${EndIf}
    StrCpy $BackedUpUninstaller 1
  ${EndIf}
  ClearErrors
  FileOpen $0 "$INSTDIR\.kurisu-install.tmp\.swapping" w
  ${If} ${Errors}
    !insertmacro CleanStagingDir ""
    !insertmacro FailWithMessage "" "Kurisu could not prepare its installation folder. Close programs using the installation folder and check its permissions, then run the installer again. The program and its registration have not been replaced."
  ${EndIf}
  FileClose $0
  !insertmacro SwapStagedFile "kurisu.exe" $BackedUpExe $PlacedExe
  ${If} $BackedUpUninstaller == 0
    !insertmacro RecordAbsentFile "uninstall.exe"
  ${EndIf}
  !insertmacro PlaceStagedFile "uninstall.exe" $PlacedUninstaller
  !insertmacro SwapStagedFile "LICENSE" $BackedUpLicense $PlacedLicense
  !insertmacro SwapStagedFile "THIRD_PARTY_NOTICES.txt" $BackedUpNotices $PlacedNotices
  !insertmacro SwapStagedFile "PROFILE_TRANSFER.md" $BackedUpTransfer $PlacedTransfer
  !insertmacro SwapStagedFile "kurisu.ico" $BackedUpIcon $PlacedIcon
  StrCpy $SwapFailure "register the installation"
  Call RegisterInstallation
  StrCpy $SwapFailure "commit the installation"
  ; The marker keeps files and registration recoverable until both are ready.
  Call RemoveSwapMarker
  ${If} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.swapping"
    Call RollbackStagedSwap
  ${EndIf}
  !insertmacro CleanStagingDir ""
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
  Call CloseInstallerMutex
SectionEnd

Function un.onInit
  SetShellVarContext current
  SetRegView 64
FunctionEnd

!macro CheckRegisteredPath Name Suffix
  ${If} $RegKey != 0
    ClearErrors
    ReadRegStr $0 HKCU "$RegPath" "${Name}"
    ${If} ${Errors}
      System::Call 'advapi32::RegQueryValueExW(p $RegKey, w "${Name}", p 0, p 0, p 0, p 0) i .r0'
      ${If} $0 != 2
        StrCpy $RegFailed 1
      ${EndIf}
    ${ElseIf} $0 != ""
      StrCpy $1 $0 1
      StrCpy $2 $0 1 -1
      ${If} $1 == '$\"'
      ${AndIf} $2 == '$\"'
        StrCpy $0 $0 -1 1
      ${EndIf}
      StrCpy $0 "$0${Suffix}"
      Call un.RequireRegisteredPath
      StrCpy $KeyOwned 1
      StrCpy $OwnsRegistration 1
    ${EndIf}
  ${EndIf}
!macroend

Function un.RequireRegisteredPath
  GetFullPathName $0 "$0"
  GetFullPathName $1 "$INSTDIR\uninstall.exe"
  ${If} $0 != ""
  ${AndIf} $0 == $1
    Return
  ${EndIf}
  GetFullPathName /SHORT $0 "$0"
  GetFullPathName /SHORT $1 "$1"
  ${If} $0 == ""
  ${OrIf} $0 != $1
    !insertmacro FailWithMessage "un." "This uninstaller belongs to a different directory. Run the uninstaller from the currently registered Kurisu installation."
  ${EndIf}
FunctionEnd

Function un.FinishRegistrationCheck
  ${If} $RegKey != 0
  ${AndIf} $KeyOwned == 0
    Call un.ReadRegistrationKeyCounts
    ${If} $1 != 0
    ${OrIf} $2 != 0
      ; A populated key with no location cannot establish ownership.
      StrCpy $RegFailed 1
    ${EndIf}
  ${EndIf}
  Call un.CloseRegistrationKey
FunctionEnd

Function un.CheckRegistrationOwner
  StrCpy $OwnsRegistration 0
  StrCpy $RegFailed 0
  StrCpy $KeyOwned 0
  StrCpy $RegPath "${PRODUCTKEY}"
  Call un.OpenRegistrationKey
  !insertmacro CheckRegisteredPath "" "\uninstall.exe"
  Call un.FinishRegistrationCheck
  StrCpy $KeyOwned 0
  StrCpy $RegPath "${UNINSTKEY}"
  Call un.OpenRegistrationKey
  !insertmacro CheckRegisteredPath "InstallLocation" "\uninstall.exe"
  !insertmacro CheckRegisteredPath "UninstallString" ""
  Call un.FinishRegistrationCheck
  ${If} $RegFailed != 0
    !insertmacro FailWithMessage "un." "Kurisu could not verify its uninstall registration. Check registry permissions and retry uninstalling."
  ${EndIf}
FunctionEnd

; FILE_ID_INFO also identifies an in-place uninstaller reached through an alias.
!macro UninstallFileIdentity Path Result
  StrCpy ${Result} ""
  System::Call 'kernel32::CreateFileW(w "${Path}", i 0, i 7, p 0, i 3, i 0, p 0) p .r0'
  ${If} $0 != -1
    System::Alloc 24
    Pop $1
    System::Call 'kernel32::GetFileInformationByHandleEx(p r0, i 18, p r1, i 24) i .r2'
    ${If} $2 != 0
      System::Call '*$1(l .r2, l .r3, l .r4)'
      ${If} $3 != 0
      ${OrIf} $4 != 0
        StrCpy ${Result} "$2/$3/$4"
      ${EndIf}
    ${EndIf}
    System::Free $1
    System::Call 'kernel32::CloseHandle(p r0)'
  ${EndIf}
!macroend

Function un.RequireInPlaceUninstaller
  GetFullPathName /SHORT $5 "$EXEPATH"
  GetFullPathName /SHORT $6 "$INSTDIR\uninstall.exe"
  ${If} $5 == ""
  ${OrIf} $5 != $6
    !insertmacro UninstallFileIdentity "$EXEPATH" $5
    !insertmacro UninstallFileIdentity "$INSTDIR\uninstall.exe" $6
    ${If} $5 == ""
    ${OrIf} $5 != $6
      !insertmacro FailWithMessage "un." "Kurisu could not remove its uninstaller. Close programs using the installation folder and retry uninstalling."
    ${EndIf}
  ${EndIf}
FunctionEnd

!macro CheckRemovableFile Name
  System::Call 'kernel32::CreateFileW(w "$INSTDIR\${Name}", i 0x10000, i 7, p 0, i 3, i 0, p 0) p .r0 ?e'
  Pop $1
  ${If} $0 != -1
    System::Call 'kernel32::CloseHandle(p r0)'
  ${ElseIf} $1 != 2
  ${AndIf} $1 != 3
    !if "${Name}" == "uninstall.exe"
      Call un.RequireInPlaceUninstaller
    !else
      !insertmacro FailWithMessage "un." "Kurisu could not remove $INSTDIR\${Name}. Close programs using the installation folder and retry uninstalling."
    !endif
  ${EndIf}
!macroend

Section "Uninstall"
  Call un.AcquireInstallerMutex
  Call un.CheckRegistrationOwner
  Call un.EnsureStopped
  Call un.RecoverInterruptedSwap
  Call un.CheckRegistrationOwner
  !insertmacro CleanStagingDir "un."
  !insertmacro CheckRemovableFile "kurisu.exe"
  !insertmacro CheckRemovableFile "kurisu.ico"
  !insertmacro CheckRemovableFile "LICENSE"
  !insertmacro CheckRemovableFile "THIRD_PARTY_NOTICES.txt"
  !insertmacro CheckRemovableFile "PROFILE_TRANSFER.md"
  !insertmacro CheckRemovableFile "uninstall.exe"
  ClearErrors
  Delete "$INSTDIR\kurisu.exe"
  ${If} ${Errors}
    !insertmacro FailWithMessage "un." "Kurisu could not remove its executable. Close programs using the installation folder and check its permissions, then retry uninstalling. The uninstall entry and shortcuts have been kept."
  ${EndIf}
  Delete "$INSTDIR\kurisu.ico"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\THIRD_PARTY_NOTICES.txt"
  Delete "$INSTDIR\PROFILE_TRANSFER.md"
  ${If} ${Errors}
    !insertmacro FailWithMessage "un." "Kurisu could not remove all program files. Close programs using the installation folder and retry uninstalling."
  ${EndIf}
  Delete "$INSTDIR\uninstall.exe"
  ${If} ${Errors}
    Call un.RequireInPlaceUninstaller
  ${EndIf}
  ${If} $OwnsRegistration == 1
    Delete "$SMPROGRAMS\Kurisu.lnk"
    Delete "$DESKTOP\Kurisu.lnk"
    DeleteRegKey HKCU "${PRODUCTKEY}"
    DeleteRegKey HKCU "${UNINSTKEY}"
  ${EndIf}
  RMDir "$INSTDIR"
  Call un.CloseInstallerMutex
SectionEnd
