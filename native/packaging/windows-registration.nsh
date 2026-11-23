Var RegFile
Var RegPath
Var RegKey
Var RegKeyBlocked
Var RegValue
Var RegHeader
Var RegData
Var RegCurrent
Var RegFailed

!define REG_EMPTY -1
!define REG_DATA_LIMIT 65536

!macro ProductRegistration Action
  !insertmacro ${Action} "" Str "$INSTDIR"
!macroend

!macro UninstallRegistration Action
  !insertmacro ${Action} "" Str "Kurisu"
  !insertmacro ${Action} "MainBinaryName" Str "kurisu.exe"
  !insertmacro ${Action} "DisplayName" Str "Kurisu"
  !insertmacro ${Action} "DisplayVersion" Str "${VERSION}"
  !insertmacro ${Action} "Publisher" Str "CateDesu"
  !insertmacro ${Action} "InstallLocation" Str '$\"$INSTDIR$\"'
  !insertmacro ${Action} "DisplayIcon" Str '$\"$INSTDIR\kurisu.ico$\"'
  !insertmacro ${Action} "UninstallString" Str '$\"$INSTDIR\uninstall.exe$\"'
  !insertmacro ${Action} "NoModify" DWORD 1
  !insertmacro ${Action} "NoRepair" DWORD 1
!macroend

!macro RegistrationReader Prefix
Function ${Prefix}OpenRegistrationKey
  StrCpy $RegKey 0
  System::Call 'advapi32::RegOpenKeyExW(p 0x80000001, w "$RegPath", i 0, i 0x101, *p .s) i .r0'
  Pop $RegKey
  ${If} $0 != 0
    StrCpy $RegKey 0
    ${If} $0 != 2
      StrCpy $RegFailed 1
    ${EndIf}
  ${EndIf}
FunctionEnd

Function ${Prefix}CloseRegistrationKey
  ${If} $RegKey != 0
    System::Call 'advapi32::RegCloseKey(p $RegKey)'
    StrCpy $RegKey 0
  ${EndIf}
FunctionEnd

Function ${Prefix}ReadRegistrationKeyCounts
  System::Call 'advapi32::RegQueryInfoKeyW(p $RegKey, p 0, p 0, p 0, *i .r1, p 0, p 0, *i .r2, p 0, p 0, p 0, p 0) i .r0'
  ${If} $0 != 0
    StrCpy $RegFailed 1
  ${EndIf}
FunctionEnd

Function ${Prefix}ReadRegistrationValue
  StrCpy $0 2
  ${If} $RegKey != 0
    System::Call '*$RegHeader(i ${REG_DATA_LIMIT})'
    System::Call 'advapi32::RegQueryValueExW(p $RegKey, w "$RegValue", p 0, *i .r1, p $RegData, p $RegHeader) i .r0'
    System::Call '*$RegHeader(i .r2)'
  ${EndIf}
  ${If} $0 == 2
    StrCpy $1 ${REG_EMPTY}
    StrCpy $2 0
  ${ElseIf} $0 != 0
    StrCpy $RegFailed 1
  ${EndIf}
FunctionEnd
!macroend
!insertmacro RegistrationReader ""
!insertmacro RegistrationReader "un."

!macro SnapshotValue Name Kind Value
  ${If} $RegFailed == 0
    StrCpy $RegValue "${Name}"
    Call SnapshotRegistrationValue
  ${EndIf}
!macroend

; Record the registry type and original bytes, including absent values.
Function SnapshotRegistrationValue
  Call ReadRegistrationValue
  ${If} $RegFailed != 0
    Return
  ${EndIf}
  System::Call '*$RegHeader(i r1, i r2)'
  System::Call 'kernel32::WriteFile(p $RegFile, p $RegHeader, i 8, *i .r3, p 0) i .r0'
  ${If} $0 == 0
  ${OrIf} $3 != 8
    StrCpy $RegFailed 1
    Return
  ${EndIf}
  ${If} $2 != 0
    System::Call 'kernel32::WriteFile(p $RegFile, p $RegData, i r2, *i .r3, p 0) i .r0'
    ${If} $0 == 0
    ${OrIf} $3 != $2
      StrCpy $RegFailed 1
    ${EndIf}
  ${EndIf}
FunctionEnd

!macro SnapshotRegistrationKey Path Values
  ${If} $RegFailed == 0
    StrCpy $RegPath "${Path}"
    Call OpenRegistrationKey
    ${If} $RegFailed == 0
      StrCpy $0 0
      ${If} $RegKey != 0
        StrCpy $0 1
      ${EndIf}
      System::Call '*$RegHeader(i r0)'
      System::Call 'kernel32::WriteFile(p $RegFile, p $RegHeader, i 4, *i .r3, p 0) i .r0'
      ${If} $0 == 0
      ${OrIf} $3 != 4
        StrCpy $RegFailed 1
      ${EndIf}
      !insertmacro ${Values} SnapshotValue
    ${EndIf}
    Call CloseRegistrationKey
  ${EndIf}
!macroend

Function SnapshotRegistration
  StrCpy $RegFailed 0
  ClearErrors
  FileOpen $RegFile "$INSTDIR\.kurisu-install.tmp\.registration" w
  ${If} ${Errors}
    StrCpy $RegFailed 1
    Return
  ${EndIf}
  System::Alloc 8
  Pop $RegHeader
  System::Alloc ${REG_DATA_LIMIT}
  Pop $RegData
  ${If} $RegHeader == 0
  ${OrIf} $RegData == 0
    StrCpy $RegFailed 1
  ${EndIf}
  !insertmacro SnapshotRegistrationKey "${UNINSTKEY}" UninstallRegistration
  !insertmacro SnapshotRegistrationKey "${PRODUCTKEY}" ProductRegistration
  System::Call 'kernel32::FlushFileBuffers(p $RegFile) i .r0'
  ${If} $0 == 0
    StrCpy $RegFailed 1
  ${EndIf}
  System::Free $RegData
  System::Free $RegHeader
  FileClose $RegFile
FunctionEnd

!macro RestoreValue Prefix Name
  ${If} $RegFailed == 0
    StrCpy $RegValue "${Name}"
    Call ${Prefix}RestoreRegistrationValue
  ${EndIf}
!macroend
!macro RestoreInstallerValue Name Kind Value
  !insertmacro RestoreValue "" "${Name}"
!macroend
!macro RestoreUninstallerValue Name Kind Value
  !insertmacro RestoreValue "un." "${Name}"
!macroend

!macro RestoreRegistrationKey Prefix Path Values Action
  ${If} $RegFailed == 0
    StrCpy $RegPath "${Path}"
    StrCpy $RegKeyBlocked 0
    System::Call 'kernel32::ReadFile(p $RegFile, p $RegHeader, i 4, *i .r3, p 0) i .r0'
    ${If} $0 == 0
    ${OrIf} $3 != 4
      StrCpy $RegFailed 1
    ${Else}
      System::Call '*$RegHeader(i .r9)'
      Call ${Prefix}OpenRegistrationKey
      ${If} $RegFailed != 0
        StrCpy $RegFailed 0
        StrCpy $RestoreFailed 1
        StrCpy $RegKeyBlocked 1
      ${ElseIf} $9 == 1
      ${AndIf} $RegKey == 0
        System::Call 'advapi32::RegCreateKeyExW(p 0x80000001, w "$RegPath", i 0, p 0, i 0, i 0x101, p 0, *p .s, p 0) i .r0'
        Pop $RegKey
        ${If} $0 != 0
          StrCpy $RegKey 0
          StrCpy $RestoreFailed 1
          StrCpy $RegKeyBlocked 1
        ${EndIf}
      ${EndIf}
      !insertmacro ${Values} ${Action}
      ${If} $RegKey != 0
      ${AndIf} $9 == 0
      ${AndIf} $RegFailed == 0
      ${AndIf} $RegKeyBlocked == 0
        Call ${Prefix}ReadRegistrationKeyCounts
        ${If} $RegFailed != 0
          StrCpy $RegFailed 0
          StrCpy $RestoreFailed 1
        ${ElseIf} $1 == 0
        ${AndIf} $2 == 0
          Call ${Prefix}CloseRegistrationKey
          ClearErrors
          DeleteRegKey /ifempty HKCU "${Path}"
          ${If} ${Errors}
            StrCpy $RestoreFailed 1
          ${EndIf}
        ${EndIf}
      ${EndIf}
      Call ${Prefix}CloseRegistrationKey
    ${EndIf}
  ${EndIf}
!macroend

!macro RegistrationRecovery Prefix Action
Function ${Prefix}RestoreRegistrationValue
  System::Call 'kernel32::ReadFile(p $RegFile, p $RegHeader, i 8, *i .r3, p 0) i .r0'
  ${If} $0 == 0
  ${OrIf} $3 != 8
    StrCpy $RegFailed 1
    Return
  ${EndIf}
  System::Call '*$RegHeader(i .r4, i .r5)'
  ${If} $5 < 0
  ${OrIf} $5 > ${REG_DATA_LIMIT}
    StrCpy $RegFailed 1
    Return
  ${EndIf}
  ${If} $5 != 0
    System::Call 'kernel32::ReadFile(p $RegFile, p $RegCurrent, i r5, *i .r3, p 0) i .r0'
    ${If} $0 == 0
    ${OrIf} $3 != $5
      StrCpy $RegFailed 1
      Return
    ${EndIf}
  ${EndIf}
  ${If} $RegKeyBlocked != 0
    Return
  ${EndIf}
  Call ${Prefix}ReadRegistrationValue
  ${If} $RegFailed != 0
    StrCpy $RegFailed 0
    StrCpy $RestoreFailed 1
    Return
  ${EndIf}
  ; The key that refused the first write may still be read only.
  ${If} $4 == $1
  ${AndIf} $5 == $2
    System::Call 'ntdll::RtlCompareMemory(p $RegCurrent, p $RegData, p r5) p .r0'
    ${If} $0 == $5
      Return
    ${EndIf}
  ${EndIf}
  System::Call 'advapi32::RegOpenKeyExW(p 0x80000001, w "$RegPath", i 0, i 0x102, *p .r3) i .r0'
  ${If} $0 == 0
    ${If} $4 == ${REG_EMPTY}
      System::Call 'advapi32::RegDeleteValueW(p r3, w "$RegValue") i .r0'
    ${Else}
      System::Call 'advapi32::RegSetValueExW(p r3, w "$RegValue", i 0, i r4, p $RegCurrent, i r5) i .r0'
    ${EndIf}
    System::Call 'advapi32::RegCloseKey(p r3)'
  ${EndIf}
  ${If} $0 != 0
    StrCpy $RestoreFailed 1
  ${EndIf}
FunctionEnd

Function ${Prefix}RestoreRegistration
  ${IfNot} ${FileExists} "$INSTDIR\.kurisu-install.tmp\.registering"
    Return
  ${EndIf}
  ClearErrors
  FileOpen $RegFile "$INSTDIR\.kurisu-install.tmp\.registering" r
  ${If} ${Errors}
    StrCpy $RestoreFailed 1
    Return
  ${EndIf}
  StrCpy $RegFailed 0
  System::Alloc 8
  Pop $RegHeader
  System::Alloc ${REG_DATA_LIMIT}
  Pop $RegData
  System::Alloc ${REG_DATA_LIMIT}
  Pop $RegCurrent
  ${If} $RegHeader == 0
  ${OrIf} $RegData == 0
  ${OrIf} $RegCurrent == 0
    StrCpy $RegFailed 1
  ${EndIf}
  ReadINIStr $0 "$INSTDIR\.kurisu-install.tmp\.swapping" "registration" "shortcut"
  ${If} $0 == 1
    ClearErrors
    Delete "$SMPROGRAMS\Kurisu.lnk"
    ${If} ${Errors}
    ${OrIf} ${FileExists} "$SMPROGRAMS\Kurisu.lnk"
      StrCpy $RestoreFailed 1
    ${EndIf}
  ${EndIf}
  !insertmacro RestoreRegistrationKey "${Prefix}" "${UNINSTKEY}" UninstallRegistration ${Action}
  !insertmacro RestoreRegistrationKey "${Prefix}" "${PRODUCTKEY}" ProductRegistration ${Action}
  System::Call 'advapi32::RegFlushKey(p 0x80000001) i .r0'
  ${If} $0 != 0
  ${OrIf} $RegFailed != 0
    StrCpy $RestoreFailed 1
  ${EndIf}
  System::Free $RegCurrent
  System::Free $RegData
  System::Free $RegHeader
  FileClose $RegFile
FunctionEnd
!macroend
!insertmacro RegistrationRecovery "" RestoreInstallerValue
!insertmacro RegistrationRecovery "un." RestoreUninstallerValue

!macro WriteRegistrationValue Name Kind Value
  ClearErrors
  WriteReg${Kind} HKCU "$RegPath" "${Name}" "${Value}"
  ${If} ${Errors}
    Call RollbackStagedSwap
  ${EndIf}
  ReadReg${Kind} $0 HKCU "$RegPath" "${Name}"
  ${If} ${Errors}
    Call RollbackStagedSwap
  ${EndIf}
  StrCmpS $0 "${Value}" +2
  Call RollbackStagedSwap
!macroend

Function RegisterInstallation
  System::Call 'kernel32::MoveFileExW(w "$INSTDIR\.kurisu-install.tmp\.registration", w "$INSTDIR\.kurisu-install.tmp\.registering", i 8) i .r0'
  ${If} $0 == 0
    Call RollbackStagedSwap
  ${EndIf}
  StrCpy $RegPath "${PRODUCTKEY}"
  !insertmacro ProductRegistration WriteRegistrationValue
  StrCpy $RegPath "${UNINSTKEY}"
  !insertmacro UninstallRegistration WriteRegistrationValue
  System::Call 'advapi32::RegFlushKey(p 0x80000001) i .r0'
  ${If} $0 != 0
    Call RollbackStagedSwap
  ${EndIf}

  System::Call 'kernel32::GetFileAttributesW(w "$SMPROGRAMS\Kurisu.lnk") i .r0 ?e'
  Pop $1
  ${If} $0 == -1
    ${If} $1 != 2
    ${AndIf} $1 != 3
      Call RollbackStagedSwap
    ${EndIf}
    ClearErrors
    WriteINIStr "$INSTDIR\.kurisu-install.tmp\.swapping" "registration" "shortcut" "1"
    ${If} ${Errors}
      Call RollbackStagedSwap
    ${EndIf}
    CreateShortcut "$SMPROGRAMS\Kurisu.lnk" "$INSTDIR\kurisu.exe" "" "$INSTDIR\kurisu.ico"
    ${If} ${Errors}
      Call RollbackStagedSwap
    ${EndIf}
  ${Else}
    IntOp $0 $0 & 0x10
    ${If} $0 != 0
      Call RollbackStagedSwap
    ${EndIf}
  ${EndIf}
FunctionEnd
