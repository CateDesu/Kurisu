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
$bridge = @(Get-ChildItem src-tauri/target/release/bundle/nsis/*-setup.exe -ErrorAction SilentlyContinue)
if ((Test-Path src-tauri/Cargo.toml) -and $bridge.Count -ne 1) { throw 'Expected one built bridge installer' }
if ($bridge.Count -eq 1) {
    $legacy = Start-Process -FilePath $bridge[0].FullName -ArgumentList "/S /D=$installDirectory" -Wait -PassThru
    if ($legacy.ExitCode -ne 0) { throw "Bridge installer exited $($legacy.ExitCode)" }
    if ((Get-Item $key).GetValue('') -ne $installDirectory) { throw 'Bridge installation identity differs' }
    if (-not (Test-Path (Join-Path $installDirectory 'kurisu.exe'))) { throw 'Bridge executable missing' }
} else {
    New-Item -Path $key -Force | Out-Null
    Set-Item -Path $key -Value $installDirectory
    New-Item -ItemType Directory -Path $installDirectory -Force | Out-Null
    Set-Content -Path (Join-Path $installDirectory 'kurisu.exe') -Value 'stopped legacy executable'
}
$uninstallKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Kurisu'
if (-not (Test-Path $uninstallKey)) {
    New-Item -Path $uninstallKey -Force | Out-Null
    New-ItemProperty -Path $uninstallKey -Name DisplayVersion -Value 'retained version' | Out-Null
}
$registrationBefore = (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress)
$binaryBefore = (Get-FileHash (Join-Path $installDirectory 'kurisu.exe')).Hash
$uninstaller = Join-Path $installDirectory 'uninstall.exe'
if (Test-Path $uninstaller) { Remove-Item $uninstaller }
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
function Assert-RejectedInstall {
    param([switch]$BinaryIsDirectory)
    $failed = Start-Process -FilePath $installer[0].FullName -ArgumentList '/S' -Wait -PassThru
    if ($failed.ExitCode -eq 0) { throw 'Installer accepted a required file write failure' }
    if ($BinaryIsDirectory) {
        if (-not (Test-Path (Join-Path $installDirectory 'kurisu.exe') -PathType Container)) {
            throw 'Rejected installation changed the obstruction'
        }
    } elseif ((Get-FileHash (Join-Path $installDirectory 'kurisu.exe')).Hash -ne $binaryBefore) {
        throw 'Rejected installation replaced the existing executable'
    }
    if ((Get-Item $key).GetValue('') -ne $installDirectory -or
        (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress) -ne $registrationBefore) {
        throw 'Rejected installation changed registration'
    }
    if ((Get-FileHash $candidateBinary).Hash -ne $candidateBefore) {
        throw 'Rejected installation removed the candidate'
    }
    Assert-RetainedProfiles
}
New-Item -ItemType Directory -Path $uninstaller | Out-Null
Assert-RejectedInstall
Remove-Item $uninstaller
Set-Content $uninstaller 'retained uninstaller'
$uninstallerBefore = (Get-FileHash $uninstaller).Hash
$held = [IO.File]::Open($uninstaller, 'Open', 'Read', 'Read')
try {
    Assert-RejectedInstall
    if ((Get-FileHash $uninstaller).Hash -ne $uninstallerBefore) {
        throw 'Rejected installation changed the locked uninstaller'
    }
} finally { $held.Dispose() }
$binary = Join-Path $installDirectory 'kurisu.exe'
$savedBinary = Join-Path $env:RUNNER_TEMP 'kurisu-retained.exe'
Move-Item $binary $savedBinary
New-Item -ItemType Directory $binary | Out-Null
Assert-RejectedInstall -BinaryIsDirectory
Remove-Item $binary
Move-Item $savedBinary $binary
$held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
try { Assert-RejectedInstall } finally { $held.Dispose() }
$process = Start-Process -FilePath $installer[0].FullName -ArgumentList '/S' -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Installer exited $($process.ExitCode)" }
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
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'Kurisu.lnk'
$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
if ($link.TargetPath -ne $binary) { throw 'Kurisu shortcut does not target the normal executable' }
foreach ($name in 'LICENSE', 'THIRD_PARTY_NOTICES.txt', 'PROFILE_TRANSFER.md', 'kurisu.ico') {
    if (-not (Test-Path (Join-Path $installDirectory $name))) { throw "Missing $name" }
}
if (Test-Path (Join-Path $installDirectory 'migrate.py')) { throw 'Ordinary installation must not need Python' }
$uninstaller = Join-Path $installDirectory 'uninstall.exe'
$registrationBefore = Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress
$retainedFiles = @{}
foreach ($path in $binary, $uninstaller, $shortcut) {
    $retainedFiles[$path] = (Get-FileHash $path).Hash
}
$held = [IO.File]::Open($binary, 'Open', 'Read', 'Read')
try {
    $failed = Start-Process -FilePath $uninstaller -ArgumentList "/S _?=$installDirectory" -Wait -PassThru
    if ($failed.ExitCode -eq 0) { throw 'Uninstaller accepted a failed executable deletion' }
    if ((Get-Item $key).GetValue('') -ne $installDirectory -or
        (Get-ItemProperty $uninstallKey | ConvertTo-Json -Compress) -ne $registrationBefore) {
        throw 'Failed uninstall removed its retry registration'
    }
    foreach ($path in $retainedFiles.Keys) {
        if ((Get-FileHash $path).Hash -ne $retainedFiles[$path]) {
            throw "Failed uninstall changed $path"
        }
    }
    Assert-RetainedProfiles
} finally { $held.Dispose() }
$process = Start-Process -FilePath $uninstaller -ArgumentList "/S _?=$installDirectory" -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Uninstaller exited $($process.ExitCode)" }
if (Test-Path $binary) { throw 'Uninstaller left the program executable' }
Assert-RetainedProfiles
Write-Output 'Native install, legacy replacement, shortcut, identity, notices and default profile files passed'
