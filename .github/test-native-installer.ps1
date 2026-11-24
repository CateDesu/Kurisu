param(
    [Parameter(Mandatory)]
    [string]$BridgeInstaller
)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Installer proof runs only on a disposable GitHub hosted Windows runner'
}
$installer = @(Get-ChildItem native/target/release/bundle/Kurisu-Native_*-install.exe)
if ($installer.Count -ne 1) { throw 'Expected exactly one native installer' }
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
$dumpbin = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
if (-not $dumpbin) { throw 'MSVC dependency inspector is unavailable' }
$dependencies = & $dumpbin /dependents native/target/release/kurisu-native.exe
if ($LASTEXITCODE -ne 0) { throw 'Could not inspect native runtime dependencies' }
if ($dependencies -match '(?i)\b(?:vcruntime|msvcp|concrt)\d[^\s]*\.dll\b') {
    throw 'The native executable requires a separate Visual C++ runtime installation'
}
$installDirectory = Join-Path $env:RUNNER_TEMP 'Kurisu installer proof'
$profiles = @(
    (Join-Path $env:LOCALAPPDATA 'com.catedesu.kurisu'),
    (Join-Path $env:LOCALAPPDATA 'CateDesu/kurisu/data'),
    (Join-Path $env:LOCALAPPDATA 'CateDesu/kurisu-native/data'),
    (Join-Path $env:APPDATA 'CateDesu/kurisu-native/data')
)
@'
import pathlib, sqlite3, sys
for value in sys.argv[1:]:
    root = pathlib.Path(value)
    root.mkdir(parents=True, exist_ok=False)
    db = sqlite3.connect(root / "kurisu.db")
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("PRAGMA wal_autocheckpoint=0")
    db.execute("CREATE TABLE pending_change (account_id INTEGER, media_id INTEGER, progress INTEGER)")
    db.commit()
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    db.execute("INSERT INTO pending_change VALUES (42, 7, 3)")
    db.commit()
    saved = {path: path.read_bytes() for path in root.iterdir()}
    db.close()
    for path, content in saved.items():
        path.write_bytes(content)
    (root / "kurisu-ui-preferences.json").write_text('{"synthetic":true}', encoding="utf-8")
    if "kurisu-native" in root.parts:
        (root / ".kurisu-native-profile").write_text("kurisu-native-v1\n", encoding="utf-8")
'@ | python - @profiles
if ($LASTEXITCODE -ne 0) { throw 'Could not prepare disposable profile data' }
$profileFiles = @{}
foreach ($profile in $profiles) {
    foreach ($file in Get-ChildItem $profile -File) {
        $profileFiles[$file.FullName] = (Get-FileHash $file.FullName).Hash
    }
}
function Assert-RetainedProfiles {
    foreach ($path in $profileFiles.Keys) {
        if (-not (Test-Path $path) -or (Get-FileHash $path).Hash -ne $profileFiles[$path]) {
            throw "Installer changed retained profile file: $path"
        }
    }
}
$key = 'HKCU:\Software\CateDesu\Kurisu'
function Get-ProductDirectory {
    [Microsoft.Win32.Registry]::GetValue('HKEY_CURRENT_USER\Software\CateDesu\Kurisu', '', $null)
}
$transition = Get-Content .github/release-transition.json -Raw | ConvertFrom-Json
$expectedBridgeHash = [string]$transition.bridgeInstallerSha256
if ($expectedBridgeHash -cnotmatch '^[0-9a-f]{64}$') {
    throw 'Pinned bridge installer SHA-256 is missing or invalid'
}
$bridgeSidecarPath = "$BridgeInstaller.sha256"
if (-not (Test-Path -LiteralPath $bridgeSidecarPath -PathType Leaf)) {
    throw 'Bridge installer checksum sidecar is missing'
}
$bridgeName = [Regex]::Escape((Split-Path -Path $BridgeInstaller -Leaf))
$bridgeSidecar = (Get-Content -LiteralPath $bridgeSidecarPath -Raw).Trim()
if ($bridgeSidecar -cnotmatch "^([0-9a-f]{64})  $bridgeName$") {
    throw 'Bridge installer checksum sidecar format is invalid'
}
$actualBridgeHash = (Get-FileHash -LiteralPath $BridgeInstaller -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualBridgeHash -cne $expectedBridgeHash -or $actualBridgeHash -cne $Matches[1]) {
    throw 'Bridge installer SHA-256 verification failed'
}
$legacy = Start-Process -FilePath $BridgeInstaller -ArgumentList "/S /D=$installDirectory" -Wait -PassThru
if ($legacy.ExitCode -ne 0) { throw "Bridge installer exited $($legacy.ExitCode)" }
if ((Get-ProductDirectory) -ne $installDirectory) { throw 'Bridge installation identity differs' }
if (-not (Test-Path (Join-Path $installDirectory 'kurisu.exe'))) { throw 'Bridge executable missing' }
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Kurisu.lnk'
$shortcutBefore = (Get-FileHash -LiteralPath $shortcut).Hash
$uninstallSubKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu'
$uninstallKey = "HKCU:\$uninstallSubKey"
$registrationBefore = (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress)
$binaryBefore = (Get-FileHash (Join-Path $installDirectory 'kurisu.exe')).Hash
$uninstaller = Join-Path $installDirectory 'uninstall.exe'
$uninstallerBefore = (Get-FileHash $uninstaller).Hash
$savedUninstaller = Join-Path $env:RUNNER_TEMP 'kurisu-bridge-uninstaller.exe'
Move-Item $uninstaller $savedUninstaller
$candidate = Join-Path $env:LOCALAPPDATA 'Programs/Kurisu Native'
New-Item -ItemType Directory -Path $candidate -Force | Out-Null
$candidateBinary = Join-Path $candidate 'kurisu-native.exe'
Set-Content $candidateBinary 'retained candidate executable'
$candidateBefore = (Get-FileHash $candidateBinary).Hash
$registry32 = [Microsoft.Win32.RegistryKey]::OpenBaseKey('CurrentUser', 'Registry32')
try {
    $candidateKey = $registry32.CreateSubKey('Software\Kurisu Native')
    try { $candidateKey.SetValue('InstallDir', $candidate) } finally { $candidateKey.Dispose() }
} finally { $registry32.Dispose() }
function Invoke-RejectedInstall {
    $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList '/S' -Wait -PassThru
    if ($failed.ExitCode -eq 0) { throw 'Installer unexpectedly succeeded' }
}
function Assert-UnchangedInstall {
    param(
        [switch]$BinaryIsDirectory,
        [switch]$UninstallerIsDirectory
    )
    if ($BinaryIsDirectory) {
        if (-not (Test-Path (Join-Path $installDirectory 'kurisu.exe') -PathType Container)) {
            throw 'Rejected installation changed the obstruction'
        }
    } elseif ((Get-FileHash (Join-Path $installDirectory 'kurisu.exe')).Hash -ne $binaryBefore) {
        throw 'Rejected installation replaced the existing executable'
    }
    if ($UninstallerIsDirectory) {
        if (-not (Test-Path $uninstaller -PathType Container)) {
            throw 'Rejected installation changed the uninstaller obstruction'
        }
    } elseif ((Get-FileHash $uninstaller).Hash -ne $uninstallerBefore) {
        throw 'Rejected installation replaced the existing uninstaller'
    }
    if (Test-Path -LiteralPath (Join-Path $installDirectory '.kurisu-install.tmp')) {
        throw 'Rejected installation left staging files behind'
    }
    if ((Get-ProductDirectory) -ne $installDirectory -or
        (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress) -ne $registrationBefore) {
        throw 'Rejected installation changed registration'
    }
    if ((Get-FileHash -LiteralPath $shortcut).Hash -ne $shortcutBefore) {
        throw 'Rejected installation changed the Start menu shortcut'
    }
    if ((Get-FileHash $candidateBinary).Hash -ne $candidateBefore) {
        throw 'Rejected installation removed the candidate'
    }
    Assert-RetainedProfiles
}
function Assert-RejectedInstall {
    param(
        [switch]$BinaryIsDirectory,
        [switch]$UninstallerIsDirectory
    )
    Invoke-RejectedInstall
    Assert-UnchangedInstall @PSBoundParameters
}
function Invoke-WithUninstallWriteDenied {
    param([scriptblock]$Check)
    $rights = [Security.AccessControl.RegistryRights]::ReadPermissions -bor
        [Security.AccessControl.RegistryRights]::ChangePermissions
    $registration = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(
        $uninstallSubKey, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights
    )
    if ($null -eq $registration) { throw 'Uninstall registration is missing' }
    try {
        $section = [Security.AccessControl.AccessControlSections]::Access
        $acl = [Microsoft.Win32.RegistryAclExtensions]::GetAccessControl($registration, $section)
        $original = $acl.GetSecurityDescriptorBinaryForm()
        $acl.AddAccessRule([Security.AccessControl.RegistryAccessRule]::new(
            [Security.Principal.WindowsIdentity]::GetCurrent().User,
            [Security.AccessControl.RegistryRights]::SetValue,
            [Security.AccessControl.AccessControlType]::Deny
        ))
        [Microsoft.Win32.RegistryAclExtensions]::SetAccessControl($registration, $acl)
        try { & $Check } finally {
            $acl.SetSecurityDescriptorBinaryForm($original, $section)
            [Microsoft.Win32.RegistryAclExtensions]::SetAccessControl($registration, $acl)
        }
    } finally { $registration.Dispose() }
}
New-Item -ItemType Directory -Path $uninstaller | Out-Null
Assert-RejectedInstall -UninstallerIsDirectory
Remove-Item $uninstaller
Copy-Item $savedUninstaller $uninstaller
$mutexName = 'Local\Kurisu-Installer-Lock'
$mutex = [Threading.Mutex]::new($true, $mutexName)
try { Assert-RejectedInstall } finally { $mutex.ReleaseMutex(); $mutex.Dispose() }
foreach ($sharing in 'Read', 'ReadWrite') {
    # ReadWrite allows the write probe but blocks replacement at the rename.
    $held = [IO.File]::Open($uninstaller, 'Open', 'Read', $sharing)
    try { Invoke-RejectedInstall } finally { $held.Dispose() }
    Assert-UnchangedInstall
}
$binary = Join-Path $installDirectory 'kurisu.exe'
$savedBinary = Join-Path $env:RUNNER_TEMP 'kurisu-retained.exe'
Move-Item $binary $savedBinary
New-Item -ItemType Directory $binary | Out-Null
Assert-RejectedInstall -BinaryIsDirectory
Remove-Item $binary
Move-Item $savedBinary $binary
$held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
try { Invoke-RejectedInstall } finally { $held.Dispose() }
Assert-UnchangedInstall
$held = [IO.File]::Open($binary, 'Open', 'ReadWrite', 'ReadWrite')
try { Invoke-RejectedInstall } finally { $held.Dispose() }
Assert-UnchangedInstall
$notices = Join-Path $installDirectory 'THIRD_PARTY_NOTICES.txt'
$savedNotices = Join-Path $env:RUNNER_TEMP 'kurisu-retained-notices.txt'
$hadNotices = Test-Path -LiteralPath $notices -PathType Leaf
if ($hadNotices) { Move-Item -LiteralPath $notices -Destination $savedNotices }
New-Item -ItemType Directory -Path $notices | Out-Null
try {
    Assert-RejectedInstall
    if (-not (Test-Path -LiteralPath $notices -PathType Container)) {
        throw 'Rejected installation changed the notices obstruction'
    }
} finally {
    if (Test-Path -LiteralPath $notices -PathType Container) { Remove-Item -LiteralPath $notices }
    if ($hadNotices) { Move-Item -LiteralPath $savedNotices -Destination $notices }
}
$stage = New-Item -ItemType Directory -Path (Join-Path $installDirectory '.kurisu-install.tmp')
$staleBackup = Join-Path $stage.FullName 'PROFILE_TRANSFER.md.bak'
Set-Content -LiteralPath $staleBackup 'Stale backup'
$held = [IO.File]::Open($staleBackup, 'Open', 'Read', 'ReadWrite')
try {
    Invoke-RejectedInstall
    if (Test-Path -LiteralPath (Join-Path $stage.FullName '.swapping')) {
        throw 'Installer reused a staging folder containing a locked backup'
    }
} finally {
    $held.Dispose()
    Remove-Item -LiteralPath $staleBackup
    Remove-Item -LiteralPath $stage.FullName
}
Assert-UnchangedInstall
$rollbackTarget = Join-Path $installDirectory 'LICENSE'
$seededLicense = -not (Test-Path -LiteralPath $rollbackTarget -PathType Leaf)
if ($seededLicense) { Set-Content -LiteralPath $rollbackTarget 'Retained license' }
$placedFile = Join-Path $installDirectory 'PROFILE_TRANSFER.md'
$savedTransfer = Join-Path $env:RUNNER_TEMP 'kurisu-bridge-transfer.md'
$hadTransfer = Test-Path -LiteralPath $placedFile
if ($hadTransfer) { Move-Item -LiteralPath $placedFile -Destination $savedTransfer }
try {
    $licenseBefore = (Get-FileHash -LiteralPath $rollbackTarget).Hash
    # Simulate an interrupted swap that placed a previously absent file.
    $stage = New-Item -ItemType Directory -Path (Join-Path $installDirectory '.kurisu-install.tmp')
    Set-Content -LiteralPath (Join-Path $stage.FullName '.swapping') -Encoding Ascii -Value '[absent]', 'PROFILE_TRANSFER.md=1'
    Set-Content -LiteralPath $placedFile 'Interrupted candidate'
    $backup = Join-Path $stage.FullName 'kurisu.exe.bak'
    Move-Item -LiteralPath $binary -Destination $backup
    Set-Content -LiteralPath $binary 'Interrupted executable'
    $uninstallerBackup = Join-Path $stage.FullName 'uninstall.exe.bak'
    Copy-Item -LiteralPath $savedUninstaller -Destination $uninstallerBackup
    Set-Content -LiteralPath $uninstaller 'Interrupted uninstaller'
    $held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
    try { Invoke-RejectedInstall } finally { $held.Dispose() }
    if ((Get-FileHash -LiteralPath $backup).Hash -ne $binaryBefore) {
        throw 'Failed recovery lost the original executable backup'
    }
    # Writes can open this handle's file, but renames require delete sharing.
    $held = [IO.File]::Open($rollbackTarget, 'Open', 'Read', 'ReadWrite')
    try {
        $probe = [IO.File]::Open($rollbackTarget, 'Open', 'Write', 'Read')
        $probe.Dispose()
        Invoke-RejectedInstall
    } finally { $held.Dispose() }
    if ((Get-FileHash -LiteralPath $rollbackTarget).Hash -ne $licenseBefore) {
        throw 'Rejected installation replaced the existing license'
    }
    if (Test-Path -LiteralPath $placedFile) {
        throw 'Interrupted installation retained a newly placed file after rollback'
    }
} finally {
    if ($hadTransfer) { Move-Item -LiteralPath $savedTransfer -Destination $placedFile -Force }
    if ($seededLicense) { Remove-Item -LiteralPath $rollbackTarget }
}
Assert-UnchangedInstall
$product = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\CateDesu\Kurisu', $true)
try {
    $product.SetValue('', $installDirectory, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    Invoke-WithUninstallWriteDenied {
        Assert-RejectedInstall
        if ($product.GetValueKind('') -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
            throw 'Failed registration changed the registry value type'
        }
    }
} finally {
    $product.SetValue('', $installDirectory, [Microsoft.Win32.RegistryValueKind]::String)
    $product.Dispose()
}
$alternateDirectory = Join-Path $env:RUNNER_TEMP 'Alternate Kurisu'
$process = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)" }
if ((Get-ProductDirectory) -ne $installDirectory -or (Test-Path -LiteralPath $alternateDirectory)) {
    throw 'Upgrade with /D did not use the registered directory'
}
if (Test-Path -LiteralPath (Join-Path $installDirectory '.kurisu-install.tmp')) {
    throw 'Installer left staging files behind'
}
if (Test-Path -LiteralPath $candidateBinary) { throw 'Verified upgrade left the candidate executable behind' }
Assert-RetainedProfiles
$binary = Join-Path $installDirectory 'kurisu.exe'
@'
import re, subprocess, sys
result = subprocess.run([sys.argv[1], "--version"], check=True,
                        capture_output=True, encoding="utf-8", timeout=15)
if not re.fullmatch(r"kurisu-native [0-9.]+", result.stdout.strip()):
    raise SystemExit("Installed executable did not report a native version")
print(result.stdout.strip())
'@ | python - $binary
if ($LASTEXITCODE -ne 0) { throw 'Installed native version check failed' }
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class KurisuResources {
    [DllImport("kernel32", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr LoadLibraryEx(string path, IntPtr file, uint flags);
    [DllImport("kernel32", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindResource(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32")]
    public static extern bool FreeLibrary(IntPtr module);
}
'@
$resources = [KurisuResources]::LoadLibraryEx($binary, [IntPtr]::Zero, 2)
if ($resources -eq [IntPtr]::Zero) { throw 'Could not load the executable resources' }
try {
    if ([KurisuResources]::FindResource($resources, [IntPtr]1, [IntPtr]14) -eq [IntPtr]::Zero) {
        throw 'The executable is missing the Kurisu window and shortcut icon'
    }
} finally {
    [KurisuResources]::FreeLibrary($resources) | Out-Null
}
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu'
$registration = Get-ItemProperty $uninstallKey
if ($registration.DisplayName -ne 'Kurisu' -or $registration.MainBinaryName -ne 'kurisu.exe') {
    throw 'Normal Kurisu installation identity was not retained'
}
if ((Get-FileHash -LiteralPath $shortcut).Hash -ne $shortcutBefore) {
    throw 'Upgrade changed the Start menu shortcut'
}
$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
if ($link.TargetPath -ne $binary) { throw 'Kurisu shortcut does not target the normal executable' }
$supportFiles = foreach ($name in 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'PROFILE_TRANSFER.md', 'kurisu.ico') {
    $path = Join-Path $installDirectory $name
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing $name" }
    $path
}
if (Test-Path (Join-Path $installDirectory 'migrate.py')) { throw 'Ordinary installation must not need Python' }
$uninstaller = Join-Path $installDirectory 'uninstall.exe'
if ($registration.UninstallString -ne ('"{0}"' -f $uninstaller)) {
    throw "Unexpected registered uninstall command: $($registration.UninstallString)"
}
$registeredUninstaller = $registration.UninstallString.Trim('"')
$registrationBefore = Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress
$retainedFiles = @{}
foreach ($path in @($binary, $uninstaller, $shortcut) + @($supportFiles)) {
    $retainedFiles[$path] = (Get-FileHash $path).Hash
}
function Assert-RejectedUninstall {
    param([string]$Executable = $uninstaller, [switch]$Registered)
    $arguments = "/S _?=$installDirectory"
    $expectedStatus = 1
    if ($Registered) {
        # The stock launcher returns its own status. Start-Process -Wait also waits for its child.
        $Executable = $registeredUninstaller
        $arguments = '/S'
        $expectedStatus = 0
    }
    $failed = Start-Process -FilePath $Executable -ArgumentList $arguments -Wait -PassThru
    if ($failed.ExitCode -ne $expectedStatus) { throw "Expected uninstall status $expectedStatus, got $($failed.ExitCode)" }
    if ((Get-ProductDirectory) -ne $installDirectory -or
        (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress) -ne $registrationBefore) {
        throw 'Failed uninstall removed its retry registration'
    }
    foreach ($path in $retainedFiles.Keys) {
        if ((Get-FileHash $path).Hash -ne $retainedFiles[$path]) {
            throw "Failed uninstall changed $path"
        }
    }
    Assert-RetainedProfiles
}
$mutex = [Threading.Mutex]::new($true, $mutexName)
try {
    Assert-RejectedUninstall
    Assert-RejectedUninstall -Registered
} finally { $mutex.ReleaseMutex(); $mutex.Dispose() }
$held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
try {
    Assert-RejectedUninstall
    Assert-RejectedUninstall -Registered
} finally { $held.Dispose() }
$stagePath = Join-Path $installDirectory '.kurisu-install.tmp'
$copiedUninstaller = Join-Path $env:RUNNER_TEMP 'kurisu-copied-uninstaller.exe'
foreach ($mode in 'backup', 'absent') {
    if ($mode -eq 'absent') {
        $process = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$installDirectory" -Wait -PassThru
        if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)" }
        $registrationBefore = Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress
        foreach ($path in @($retainedFiles.Keys)) {
            $retainedFiles[$path] = (Get-FileHash $path).Hash
        }
    }
    New-Item -ItemType Directory -Path $stagePath | Out-Null
    $swapMarker = Join-Path $stagePath '.swapping'
    $binaryBackup = Join-Path $stagePath 'kurisu.exe.bak'
    $uninstallerBackup = Join-Path $stagePath 'uninstall.exe.bak'
    Copy-Item -LiteralPath $uninstaller -Destination $copiedUninstaller -Force
    Copy-Item -LiteralPath $binary -Destination $binaryBackup
    if ($mode -eq 'backup') {
        Copy-Item -LiteralPath $savedUninstaller -Destination $uninstallerBackup
        New-Item -ItemType File -Path $swapMarker | Out-Null
    } else {
        Set-Content -LiteralPath $swapMarker -Encoding Ascii -Value '[absent]', 'uninstall.exe=1'
    }
    $held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
    try {
        Assert-RejectedUninstall
        Assert-RejectedUninstall -Executable $copiedUninstaller
    } finally { $held.Dispose() }
    if (-not (Test-Path -LiteralPath $swapMarker) -or
        (Get-FileHash -LiteralPath $binaryBackup).Hash -ne $retainedFiles[$binary] -or
        ($mode -eq 'backup' -and (Get-FileHash -LiteralPath $uninstallerBackup).Hash -ne $uninstallerBefore)) {
        throw "Failed $mode recovery lost retry files"
    }
    $held = [IO.File]::Open($swapMarker, 'Open', 'Read', 'ReadWrite')
    try { Assert-RejectedUninstall } finally { $held.Dispose() }
    if ($mode -eq 'backup') {
        $held = [IO.File]::Open($uninstallerBackup, 'Open', 'Read', 'Read')
        try { Assert-RejectedUninstall } finally { $held.Dispose() }
        if (-not (Test-Path -LiteralPath $swapMarker)) { throw 'Locked backup lost its recovery marker' }
    } else {
        foreach ($name in 'LICENSE', 'uninstall.exe') {
            $held = [IO.File]::Open((Join-Path $installDirectory $name), 'Open', 'Read', 'Read')
            try {
                Assert-RejectedUninstall -Executable $copiedUninstaller
                Assert-RejectedUninstall -Registered
            } finally { $held.Dispose() }
        }
    }
    if ($mode -eq 'absent') {
        $alias = Join-Path $installDirectory 'kurisu-test-uninstall-link.exe'
        New-Item -ItemType HardLink -Path $alias -Target $uninstaller | Out-Null
        $held = [IO.File]::Open($uninstaller, 'Open', 'Read', 'Read')
        try {
            $process = Start-Process -FilePath $alias -ArgumentList "/S _?=$installDirectory" -Wait -PassThru
        } finally {
            $held.Dispose()
            Remove-Item -LiteralPath $alias
        }
    } else {
        $process = Start-Process -FilePath $registeredUninstaller -ArgumentList '/S' -Wait -PassThru
        if (Test-Path -LiteralPath $uninstaller) { throw 'Registered uninstall left its executable' }
    }
    if ($process.ExitCode -ne 0) { throw "Uninstaller exited $($process.ExitCode) after $mode recovery" }
    if ((Test-Path $binary) -or (Test-Path $stagePath) -or (Test-Path $shortcut) -or
        (Test-Path $key) -or (Test-Path $uninstallKey)) {
        throw "Uninstaller left program files or registration after $mode recovery"
    }
    Assert-RetainedProfiles
}
$staleStage = New-Item -ItemType Directory -Path $stagePath -Force
$staleSwap = Join-Path $staleStage.FullName '.swapping'
New-Item -ItemType File -Path $staleSwap | Out-Null
$staleBackup = Join-Path $staleStage.FullName 'kurisu.exe.bak'
Set-Content -LiteralPath $staleBackup 'Retained older executable'
Copy-Item -LiteralPath $copiedUninstaller -Destination $uninstaller
$newUninstaller = Join-Path $alternateDirectory 'uninstall.exe'
$newBinary = Join-Path $alternateDirectory 'kurisu.exe'
function Write-InterruptedJournal {
    param([string]$StageDir)
    @'
import pathlib, struct, sys
missing = (-1, b"")
arp = [missing] * 10
arp[3] = (1, b"\0\0")  # Empty DisplayVersion.
arp[4] = (3, bytes.fromhex("ff001100"))  # Binary Publisher.
arp[8] = (4, struct.pack("<I", 7))  # NoModify.
with (pathlib.Path(sys.argv[1]) / ".registering").open("wb") as journal:
    for existed, values in [(1, arp), (0, [missing])]:
        journal.write(struct.pack("<I", existed))
        for kind, data in values:
            journal.write(struct.pack("<iI", kind, len(data)) + data)
'@ | python - $StageDir
    if ($LASTEXITCODE -ne 0) { throw 'Could not seed interrupted registration' }
}
New-Item -ItemType Directory -Path $shortcut | Out-Null
try {
    $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
    if ($failed.ExitCode -ne 1 -or (Test-Path $key) -or (Test-Path $uninstallKey) -or
        (Test-Path $newUninstaller) -or (Test-Path $newBinary) -or
        (Test-Path (Join-Path $alternateDirectory '.kurisu-install.tmp'))) {
        throw 'Failed shortcut creation left a new installation or registration behind'
    }
    if (-not (Test-Path -LiteralPath $shortcut -PathType Container)) {
        throw 'Failed registration removed the shortcut obstruction'
    }

    $interruptedStage = Join-Path $alternateDirectory '.kurisu-install.tmp'
    New-Item -ItemType Directory -Path $interruptedStage | Out-Null
    New-Item -ItemType File -Path (Join-Path $interruptedStage '.swapping') | Out-Null
    # The old ARP key disappeared. An unrelated value was added to the new product key.
    $product = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\CateDesu\Kurisu')
    try {
        $product.SetValue('', $alternateDirectory)
        $product.SetValue('Retain', 'unrelated')
    } finally { $product.Dispose() }
    Write-InterruptedJournal $interruptedStage
    Invoke-RejectedInstall
    $arp = Get-Item -LiteralPath $uninstallKey
    $product = Get-Item -LiteralPath $key
    try {
        if ((Test-Path -LiteralPath $interruptedStage) -or
            (Test-Path -LiteralPath $newBinary) -or (Test-Path -LiteralPath $newUninstaller) -or
            $arp.ValueCount -ne 3 -or
            $arp.GetValueKind('DisplayVersion') -ne 'String' -or $arp.GetValue('DisplayVersion') -cne '' -or
            $arp.GetValueKind('Publisher') -ne 'Binary' -or
            [Convert]::ToHexString($arp.GetValue('Publisher')) -cne 'FF001100' -or
            $arp.GetValueKind('NoModify') -ne 'DWord' -or $arp.GetValue('NoModify') -ne 7 -or
            $product.ValueCount -ne 1 -or $product.GetValue('Retain') -cne 'unrelated') {
            throw 'Resumed rollback changed previous registry values or lost unrelated data'
        }
    } finally {
        $arp.Dispose()
        $product.Dispose()
    }

    # Shortcut delete and ARP write failures must still restore later registry keys while keeping retry state.
    New-Item -ItemType Directory -Path $interruptedStage | Out-Null
    Set-Content -LiteralPath (Join-Path $interruptedStage '.swapping') -Encoding Ascii -Value '[registration]', 'shortcut=1'
    Write-InterruptedJournal $interruptedStage
    $product = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\CateDesu\Kurisu', $true)
    try { $product.SetValue('', $alternateDirectory) } finally { $product.Dispose() }
    $arp = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($uninstallSubKey, $true)
    try { $arp.SetValue('DisplayName', 'Interrupted') } finally { $arp.Dispose() }
    Invoke-WithUninstallWriteDenied {
        $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
        if ($failed.ExitCode -ne 1 -or
            -not (Test-Path -LiteralPath (Join-Path $interruptedStage '.swapping'))) {
            throw 'Partial registration recovery did not retain its retry marker'
        }
        $product = Get-Item -LiteralPath $key
        try {
            if ($product.ValueCount -ne 1 -or $null -ne $product.GetValue('') -or
                $product.GetValue('Retain') -cne 'unrelated') {
                throw 'Shortcut or ARP write failure skipped restoring the product key'
            }
        } finally { $product.Dispose() }
    }
    Clear-Content -LiteralPath (Join-Path $interruptedStage '.swapping')
    $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
    if ($failed.ExitCode -ne 1) { throw 'Installer unexpectedly succeeded with shortcut obstruction' }
    $arp = Get-Item -LiteralPath $uninstallKey
    $product = Get-Item -LiteralPath $key
    try {
        if ((Test-Path -LiteralPath $interruptedStage) -or
            $arp.ValueCount -ne 3 -or $null -ne $arp.GetValue('DisplayName') -or
            $arp.GetValueKind('DisplayVersion') -ne 'String' -or $arp.GetValue('DisplayVersion') -cne '' -or
            $arp.GetValueKind('Publisher') -ne 'Binary' -or
            [Convert]::ToHexString($arp.GetValue('Publisher')) -cne 'FF001100' -or
            $arp.GetValueKind('NoModify') -ne 'DWord' -or $arp.GetValue('NoModify') -ne 7 -or
            $product.ValueCount -ne 1 -or $product.GetValue('Retain') -cne 'unrelated') {
            throw 'Follow-up recovery did not finish restoring the ARP key'
        }
    } finally {
        $arp.Dispose()
        $product.Dispose()
    }
    Remove-Item -LiteralPath $key, $uninstallKey
} finally {
    Remove-Item -LiteralPath $shortcut
}
# A locked shortcut file must still restore both registry keys and finish cleanup once released.
New-Item -ItemType Directory -Path $interruptedStage | Out-Null
Set-Content -LiteralPath (Join-Path $interruptedStage '.swapping') -Encoding Ascii -Value '[registration]', 'shortcut=1'
Write-InterruptedJournal $interruptedStage
Set-Content -LiteralPath $shortcut 'Interrupted shortcut'
$product = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\CateDesu\Kurisu')
try {
    $product.SetValue('', $alternateDirectory)
    $product.SetValue('Retain', 'unrelated')
} finally { $product.Dispose() }
$arp = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu')
try {
    $arp.SetValue('DisplayName', 'Interrupted')
    $arp.SetValue('DisplayVersion', '')
    $arp.SetValue('Publisher', [byte[]]@(0xff, 0x00, 0x11, 0x00), [Microsoft.Win32.RegistryValueKind]::Binary)
    $arp.SetValue('NoModify', 7, [Microsoft.Win32.RegistryValueKind]::DWord)
} finally { $arp.Dispose() }
try {
    $heldShortcut = [IO.File]::Open($shortcut, 'Open', 'Read', 'Read')
    try {
        $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
        if ($failed.ExitCode -ne 1 -or
            -not (Test-Path -LiteralPath (Join-Path $interruptedStage '.swapping')) -or
            -not (Test-Path -LiteralPath $shortcut -PathType Leaf)) {
            throw 'Locked shortcut recovery did not retain its retry marker'
        }
        $arp = Get-Item -LiteralPath $uninstallKey
        $product = Get-Item -LiteralPath $key
        try {
            if ($arp.ValueCount -ne 3 -or $null -ne $arp.GetValue('DisplayName') -or
                $arp.GetValueKind('DisplayVersion') -ne 'String' -or $arp.GetValue('DisplayVersion') -cne '' -or
                $arp.GetValueKind('Publisher') -ne 'Binary' -or
                [Convert]::ToHexString($arp.GetValue('Publisher')) -cne 'FF001100' -or
                $arp.GetValueKind('NoModify') -ne 'DWord' -or $arp.GetValue('NoModify') -ne 7 -or
                $product.ValueCount -ne 1 -or $null -ne $product.GetValue('') -or
                $product.GetValue('Retain') -cne 'unrelated') {
                throw 'Locked shortcut delete failure skipped restoring registry keys'
            }
        } finally {
            $arp.Dispose()
            $product.Dispose()
        }
    } finally {
        $heldShortcut.Dispose()
    }
    New-Item -ItemType Directory -Path $newBinary | Out-Null
    try {
        $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
        if ($failed.ExitCode -ne 1 -or
            (Test-Path -LiteralPath $shortcut) -or
            (Test-Path -LiteralPath (Join-Path $interruptedStage '.swapping'))) {
            throw 'Follow-up recovery did not remove the unlocked shortcut'
        }
    } finally {
        if (Test-Path -LiteralPath $newBinary -PathType Container) { Remove-Item -LiteralPath $newBinary }
    }
} finally {
    if (Test-Path -LiteralPath $shortcut) { Remove-Item -LiteralPath $shortcut }
    if (Test-Path -LiteralPath $interruptedStage) { Remove-Item -LiteralPath $interruptedStage -Recurse }
    if (Test-Path -LiteralPath $key) { Remove-Item -LiteralPath $key }
    if (Test-Path -LiteralPath $uninstallKey) { Remove-Item -LiteralPath $uninstallKey }
}
$process = Start-Process -FilePath $installer[0].FullName -ArgumentList "/S /D=$alternateDirectory" -Wait -PassThru
if ($process.ExitCode -ne 0 -or (Get-ProductDirectory) -ne $alternateDirectory) {
    throw 'Installer did not recover after registration failure'
}
$desktopShortcut = Join-Path ([Environment]::GetFolderPath('DesktopDirectory')) 'Kurisu.lnk'
$link = (New-Object -ComObject WScript.Shell).CreateShortcut($desktopShortcut)
$link.TargetPath = $newBinary
$link.Save()
$newFiles = @{}
foreach ($path in $newBinary, $newUninstaller, $shortcut, $desktopShortcut, $staleSwap, $staleBackup, $uninstaller) {
    $newFiles[$path] = (Get-FileHash -LiteralPath $path).Hash
}
$arp = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu', $true)
try {
    $location = $arp.GetValue('InstallLocation')
    $command = $arp.GetValue('UninstallString')
    foreach ($mode in 'complete', 'conflicting', 'arp-only', 'command-only', 'no-location', 'product-owned-arp-no-location', 'arp-owned-product-no-location') {
        switch ($mode) {
            'conflicting' { Set-Item -LiteralPath $key -Value $installDirectory }
            'arp-only' { Remove-Item -LiteralPath $key }
            'command-only' { $arp.DeleteValue('InstallLocation') }
            'no-location' { $arp.DeleteValue('UninstallString') }
            'product-owned-arp-no-location' {
                $product = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\CateDesu\Kurisu')
                try { $product.SetValue('', $installDirectory) } finally { $product.Dispose() }
            }
            'arp-owned-product-no-location' {
                Remove-Item -LiteralPath $key
                $product = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\CateDesu\Kurisu')
                try { $product.SetValue('Retain', 'unrelated') } finally { $product.Dispose() }
                $arp.SetValue('InstallLocation', ('"{0}"' -f $installDirectory))
                $arp.SetValue('UninstallString', ('"{0}"' -f $uninstaller))
            }
        }
        $expectedProductExists = Test-Path $key
        $expectedDirectory = if ($expectedProductExists) { Get-ProductDirectory }
        $expectedProduct = if ($expectedProductExists) { Get-ItemProperty $key | ConvertTo-Json -Compress }
        $expectedRegistration = Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress
        if ($mode -eq 'complete') {
            $launcher = Start-Process -FilePath $uninstaller -ArgumentList '/S' -Wait -PassThru
            if ($launcher.ExitCode -ne 0) { throw 'Stale uninstall launcher failed to start its child' }
        }
        $failed = Start-Process -FilePath $uninstaller -ArgumentList "/S _?=$installDirectory" -Wait -PassThru
        if ($failed.ExitCode -ne 1 -or
            (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress) -ne $expectedRegistration) {
            throw "Stale uninstall affected the $mode registration"
        }
        if ($expectedProductExists) {
            if (-not (Test-Path $key) -or
                (Get-ProductDirectory) -ne $expectedDirectory -or
                (Get-ItemProperty $key | ConvertTo-Json -Compress) -ne $expectedProduct) {
                throw "Stale uninstall affected the $mode product registration"
            }
        } elseif (Test-Path $key) {
            throw 'Stale uninstall recreated the product registration'
        }
        foreach ($path in $newFiles.Keys) {
            if ((Get-FileHash -LiteralPath $path).Hash -ne $newFiles[$path]) {
                throw "Stale uninstall changed $path"
            }
        }
    }
} finally {
    $arp.SetValue('InstallLocation', $location)
    $arp.SetValue('UninstallString', $command)
    $arp.Dispose()
    if (Test-Path $key) { Remove-Item -LiteralPath $key }
    $product = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Software\CateDesu\Kurisu')
    try { $product.SetValue('', $alternateDirectory) } finally { $product.Dispose() }
}
$process = Start-Process -FilePath $newUninstaller -ArgumentList '/S' -Wait -PassThru
if ($process.ExitCode -ne 0 -or (Test-Path $newBinary) -or (Test-Path $key) -or
    (Test-Path $uninstallKey) -or (Test-Path $shortcut) -or (Test-Path $desktopShortcut)) {
    throw 'The newer installation could not be removed through its registered uninstaller'
}
Set-Content -LiteralPath $shortcut 'Unowned shortcut'
try {
    $process = Start-Process -FilePath $copiedUninstaller -ArgumentList "/S _?=$installDirectory" -Wait -PassThru
    if ($process.ExitCode -ne 0 -or (Test-Path $installDirectory)) {
        throw 'Unregistered uninstall failed to recover and remove its own program files'
    }
    if ((Get-Content -LiteralPath $shortcut) -ne 'Unowned shortcut') {
        throw 'Unregistered uninstall changed an unowned shortcut'
    }
} finally {
    if (Test-Path -LiteralPath $shortcut) { Remove-Item -LiteralPath $shortcut }
}
Assert-RetainedProfiles
Write-Output 'Native installer, legacy replacement, rollback, uninstall ownership and retained profile checks passed'
