//! Self-update over the rolling GitHub release. Same shape as NyaaTriggers'
//! updater. Fetch releases/latest, compare numeric version tuples, download
//! with a SHA-256 check that fails closed, then hand off per platform.
//!
//! - Windows: launch the verified NSIS installer and quit, so it can
//!   overwrite the install.
//! - Linux: swap the running binary for the verified one via two adjacent
//!   renames. A running Linux binary can be replaced, unlike Windows. Let
//!   the UI prompt a restart.
//!
//! Anything else reports can_install: false and updates by hand from the
//! release page.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;

const REPO: &str = "CateDesu/Kurisu";
const USER_AGENT: &str = "Kurisu";

/// Dropped next to the exe when a swap fails so badly the rollback rename also
/// failed. exe missing, only the .kurisu-old backup remains. The next launch,
/// possible only after a manual restore, surfaces it. The file is removed only
/// once the frontend acknowledges the notice.
pub const FAILED_MARKER: &str = ".kurisu-update-failed";

/// Notice text for the doubly failed swap state the marker records. Shared by
/// the emit fast path and the take_update_failed pull so both say the same
/// thing.
pub const FAILED_MESSAGE: &str = "The last update failed to install cleanly, so the previous version was kept. Nothing was lost — you can retry the update from Settings.";

// ── Process-wide updater state ──────────────────────────────────────────────

/// The exe path captured ONCE at process start, via init_install_path from app
/// setup. After a successful Linux swap, /proc/self/exe follows the renamed
/// inode, so a current_exe() at apply time would return the .kurisu-old
/// backup and the install would target the wrong path.
static EXE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Set after a successful in-place swap: the on-disk binary is then newer than
/// the still-running process, whose current_version() is a compile-time
/// constant, so no further install may be offered or applied until restart.
static UPDATE_APPLIED: AtomicBool = AtomicBool::new(false);

/// Set when startup found the doubly failed swap marker. Cleared, and the
/// marker file removed, only when the frontend acknowledges the notice via
/// take_update_failed.
static UPDATE_FAILED: AtomicBool = AtomicBool::new(false);

/// The startup check's update-available payload, stashed so a webview that
/// booted after the one-shot emit can still pull it on mount.
static PENDING_UPDATE: Mutex<Option<Value>> = Mutex::new(None);

/// Per-invocation counter for scratch paths. A bare pid is shared by every
/// concurrent install in the same process.
#[cfg(any(windows, target_os = "linux"))]
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique per-invocation scratch suffix. See SCRATCH_SEQ.
#[cfg(any(windows, target_os = "linux"))]
fn scratch_suffix() -> u64 {
    SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Capture the exe path at process start, before any update can have swapped
/// it. See EXE_PATH. Repeat calls keep the first capture.
pub fn init_install_path() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = EXE_PATH.set(sane_exe_path(exe)?);
    Ok(())
}

/// Backstop sanity check on an install target. Never a .kurisu-old backup,
/// never an unlinked but running inode, flagged by a ` (deleted)` suffix on Linux.
fn sane_exe_path(exe: PathBuf) -> Result<PathBuf, String> {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".kurisu-old") || name.contains(" (deleted)") {
        return Err(format!("refusing to update the install at {name}"));
    }
    Ok(exe)
}

/// The install dir. Parent of the startup-captured exe path, falling back to a
/// fresh current_exe() when init_install_path never ran. Unit tests use the fallback.
fn install_dir() -> Option<PathBuf> {
    let exe = EXE_PATH
        .get()
        .cloned()
        .or_else(|| std::env::current_exe().ok())?;
    exe.parent().map(Path::to_path_buf)
}

/// Startup sweep next to the exe. Leftovers first, then the doubly failed swap
/// marker. Returns true when a marker was found, meaning a previous update's
/// swap AND its rollback both failed, and the user only got here by manually
/// restoring the backup. The marker file is NOT removed here. It stays until
/// the frontend acknowledges the notice via take_update_failed, because the
/// one-shot emit can race a slow webview boot and drop, losing the warning.
pub fn sweep_install_dir() -> bool {
    let Some(dir) = install_dir() else {
        return false;
    };
    sweep_install_leftovers(&dir);
    if dir.join(FAILED_MARKER).exists() {
        UPDATE_FAILED.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

/// The frontend's pull half of the failed-update notice. Returns the notice
/// text once per marker and removes the marker file only now.
#[tauri::command]
pub fn take_update_failed() -> Option<String> {
    if !UPDATE_FAILED.swap(false, Ordering::SeqCst) {
        return None;
    }
    if let Some(dir) = install_dir() {
        let _ = std::fs::remove_file(dir.join(FAILED_MARKER));
    }
    Some(FAILED_MESSAGE.to_string())
}

/// Stash the startup update-available payload for the pull path.
pub fn set_pending_update(payload: Value) {
    *PENDING_UPDATE.lock() = Some(payload);
}

/// The stashed update-available payload, handed out once. The emit carrying
/// the same payload can fire before a slow-booting webview has its listener
/// registered, so pulling on mount is the reliable path.
#[tauri::command]
pub fn take_pending_update() -> Option<Value> {
    PENDING_UPDATE.lock().take()
}

// ── Version comparison (same semantics as NyaaTriggers' parse_version) ──────

/// `v1.0.0.8` becomes `(1, 0, 0, 8)`. Each dot segment contributes only its
/// leading digits. No leading digit means 0. Handles the rolling 4-segment tags.
/// Comparisons go through version_key. This stays for the tests and as a
/// plain release-tuple view.
#[allow(dead_code)]
pub fn parse_version(s: &str) -> Vec<u64> {
    version_key(s).0
}

/// Comparable version key. Release segments first, then a prerelease marker
/// that sorts ANY prerelease below the plain release of the same numbers,
/// so 1.0.0-rc1 < 1.0.0 < 1.0.0.8. The prerelease's first digit run breaks
/// rc1 and rc2 style ties.
fn version_key(s: &str) -> (Vec<u64>, u8, u64) {
    let trimmed = s.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match trimmed.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (trimmed, None),
    };
    let release: Vec<u64> = core
        .split('.')
        .map(|seg| {
            let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect();
    let release = if release.is_empty() { vec![0] } else { release };
    let (pre_rank, pre_num) = match pre {
        // No prerelease sorts above any prerelease of the same release numbers.
        None => (1, 0),
        Some(p) => {
            let digits: String = p
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            (0, digits.parse().unwrap_or(0))
        }
    };
    (release, pre_rank, pre_num)
}

/// True if `remote` is strictly newer than `current`.
pub fn is_newer(remote: &str, current: &str) -> bool {
    version_key(remote) > version_key(current)
}

/// This build's version. The CI-stamped release version when present,
/// KURISU_BUILD_VERSION at build time, including the rolling 4th segment, else
/// the crate version. Without the stamp an installed rolling build would keep
/// reporting the X.Y.Z base and re-offer the same update forever.
pub fn current_version() -> &'static str {
    match option_env!("KURISU_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// True only when this binary was stamped by CI. Unstamped, locally compiled,
/// builds never auto-check on startup. They report the X.Y.Z base version, so
/// the dev loop would get nagged by every newer rolling build, and an
/// accidental install would overwrite the working tree's binary. A manual
/// check from Settings still works on any build.
pub fn is_ci_build() -> bool {
    matches!(option_env!("KURISU_BUILD_VERSION"), Some(v) if !v.is_empty())
}

// ── Release lookup ──────────────────────────────────────────────────────────

/// A GitHub release, reduced to what the updater needs. assets maps asset
/// name to browser download URL.
#[derive(Debug, Clone, Default)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub html_url: String,
    pub body: String,
    pub assets: HashMap<String, String>,
}

/// Fetch the latest full, non-prerelease, non-draft release. The rolling
/// workflow prunes superseded rolling releases, so this is always the newest
/// main build, or the newest hand-cut milestone if one is newer.
pub async fn fetch_latest_release() -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let data: Value = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse_release(&data))
}

fn parse_release(data: &Value) -> Release {
    let tag = data
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut assets = HashMap::new();
    if let Some(arr) = data.get("assets").and_then(Value::as_array) {
        for a in arr {
            let name = a.get("name").and_then(Value::as_str).unwrap_or("");
            let url = a
                .get("browser_download_url")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !name.is_empty() && !url.is_empty() {
                assets.insert(name.to_string(), url.to_string());
            }
        }
    }
    Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        tag,
        html_url: data
            .get("html_url")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&format!("https://github.com/{REPO}/releases/latest"))
            .to_string(),
        body: data.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        assets,
    }
}

/// The updatable asset for THIS platform in `rel`. The NSIS installer,
/// name ending in -setup.exe, on Windows. The bare `kurisu` binary on Linux.
/// None elsewhere, and None once an update was applied this session. The
/// running process is then older than the on-disk binary, so the version
/// comparison would keep re-offering the install until restart. Never matches
/// the .sha256 sidecars. Linux CI publishes x86-64 only, so other arches get
/// None, can_install: false, rather than a binary their kernel cannot exec.
/// Whether an update was already applied this session. The running process is
/// then older than the on-disk binary, so no further install should be offered
/// until a restart.
pub fn update_applied() -> bool {
    UPDATE_APPLIED.load(Ordering::SeqCst)
}

pub fn platform_asset(rel: &Release) -> Option<&str> {
    if update_applied() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        rel.assets
            .keys()
            .find(|n| n.ends_with("-setup.exe"))
            .map(String::as_str)
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::consts::ARCH != "x86_64" {
            return None;
        }
        rel.assets
            .keys()
            .find(|n| n.as_str() == "kurisu")
            .map(String::as_str)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = rel;
        None
    }
}

/// Fetch the .sha256 sidecar text for an asset, if the release publishes one.
/// Bounded like the module's other fetches. A sidecar is under 100 bytes, so a
/// 15 s deadline, HTTP error statuses don't count as sidecar text, and the
/// body is capped rather than read unbounded.
#[cfg(any(windows, target_os = "linux"))]
pub async fn fetch_sidecar(rel: &Release, asset_name: &str) -> Option<String> {
    const MAX_SIDECAR_BYTES: usize = 4096;
    let url = rel.assets.get(&format!("{asset_name}.sha256"))?;
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_SIDECAR_BYTES as u64)
    {
        return None;
    }
    let mut buf = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if buf.len() > MAX_SIDECAR_BYTES {
                    return None;
                }
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

// ── Download + integrity ────────────────────────────────────────────────────

/// Hard ceiling on an update download. The real assets are around 20 MB for
/// the Linux binary and 150 MB for the NSIS installer with the WebView2
/// bootstrapper. This leaves room for an offline-installer future while
/// bounding how much disk a pathological or compromised asset can fill.
#[cfg(any(windows, target_os = "linux"))]
const MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// Stream `url` to `dest`. Writes to a unique per-invocation .part-N sibling
/// so concurrent installs in one process never share a scratch file, and
/// renames on success so a half-download is never mistaken for complete.
/// Verifies Content-Length, since a clean early close is a short read with no
/// error, and refuses anything past MAX_DOWNLOAD_BYTES, header-claimed or
/// streamed. The .part is removed on failure. The staged bytes and the
/// directory entry are fsync'd before returning, so a crash in the writeback
/// window can't leave an unflushed file at `dest`. File I/O goes through
/// tokio::fs so the writes stay off the async workers.
#[cfg(any(windows, target_os = "linux"))]
pub async fn download(url: &str, dest: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let part = {
        let mut p = dest.as_os_str().to_os_string();
        p.push(format!(".part-{}", scratch_suffix()));
        PathBuf::from(p)
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    let res: Result<(), String> = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // NSIS plus the embedded WebView2 bootstrapper is about 150 MB.
            // At roughly 1 Mbps that is a 20-minute pull, so give slow links
            // 30 minutes before cutting off. The half-download is deleted
            // either way.
            .timeout(Duration::from_secs(1800))
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        let total = resp.content_length().unwrap_or(0);
        if total > MAX_DOWNLOAD_BYTES {
            return Err(format!("update download is implausibly large ({total} bytes)"));
        }
        let mut file = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
        let mut got: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
            // The header can lie (or be absent): enforce the cap on the stream too.
            if got > MAX_DOWNLOAD_BYTES {
                return Err(format!("update download exceeded {MAX_DOWNLOAD_BYTES} bytes"));
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        if total != 0 && got < total {
            return Err(format!("download incomplete: {got} of {total} bytes"));
        }
        tokio::fs::rename(&part, dest).await.map_err(|e| e.to_string())?;
        // The caller verifies and then installs or executes these bytes. Make
        // the rename itself durable too, best effort. Windows can't fsync a
        // directory handle opened this way, for example.
        if let Some(parent) = dest.parent() {
            if let Ok(d) = tokio::fs::File::open(parent).await {
                let _ = d.sync_all().await;
            }
        }
        Ok(())
    }
    .await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    res
}

/// Whether the .sha256 sidecar authorizes `path`. Ok(Some(_)) means the digest
/// matches. Ok(None) means a digest was found but does NOT match. Err(_) means
/// no readable digest or unreadable file. Callers fail closed on anything but
/// Ok(Some(_)).
///
/// The digest is computed from a freshly opened handle and that handle,
/// rewound, is returned on success, so the caller installs or executes THE
/// SAME BYTES it verified. A process that swaps the file between verify and
/// use, a TOCTOU, gets nowhere:
///
/// - Linux: the apply step copies FROM this handle, never re-opening the path.
/// - Windows: the handle is opened with read-only sharing, so the file can't
///   be renamed or overwritten while it's held. The caller keeps it open
///   until the installer has been launched from the path.
#[cfg(any(windows, target_os = "linux"))]
pub fn verify_and_open(path: &Path, sidecar_text: &str) -> io::Result<Option<std::fs::File>> {
    use sha2::{Digest, Sha256};
    let expected = sidecar_text
        .split_whitespace()
        .find(|tok| tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no sha256 digest in sidecar"))?
        .to_ascii_lowercase();
    #[cfg(windows)]
    let mut f = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0001;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)?
    };
    #[cfg(target_os = "linux")]
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut f, &mut hasher)?;
    let got = hasher.finalize();
    let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    if got_hex != expected {
        return Ok(None);
    }
    use std::io::Seek;
    f.seek(io::SeekFrom::Start(0))?;
    Ok(Some(f))
}

// ── Apply: Linux in-place binary swap ───────────────────────────────────────

/// Replace the running exe with the verified download. Refuse when an update
/// already went in this session, since the running process is older than the
/// on-disk binary now and needs a restart first. Refuse bytes that are not an
/// ELF for this architecture. Then copy FROM the verified handle, never
/// re-open the download path, to keep the verify and use chain on the same
/// bytes. fsync, stage next to the live exe on the same filesystem under a
/// unique per-invocation name, and do two adjacent renames. The live exe goes
/// aside to <name>.kurisu-old, the staged file goes in as the exe, followed
/// by an fsync of the install dir so the rename metadata is durable. ext4's
/// rename-onto-existing flush heuristic can't fire here because the target is
/// renamed away FIRST. Rolls back if the second rename fails. The backup is
/// swept on the next launch. The install path is the one captured at process
/// start, not a fresh current_exe(). After a swap the latter points at the
/// .kurisu-old backup. The caller prompts the user to restart.
#[cfg(target_os = "linux")]
pub fn apply_linux_update(new_bin: &mut std::fs::File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if UPDATE_APPLIED.load(Ordering::SeqCst) {
        return Err("an update was already installed; restart Kurisu to finish it".to_string());
    }
    // CI ships exactly one Linux build (x86-64): never swap in bytes this
    // kernel cannot exec.
    if !elf_file_matches_arch(new_bin)
        .map_err(|e| format!("could not read the downloaded update: {e}"))?
    {
        return Err("the downloaded update is not built for this machine's architecture".to_string());
    }
    let exe = match EXE_PATH.get() {
        Some(p) => p.clone(),
        None => sane_exe_path(std::env::current_exe().map_err(|e| e.to_string())?)?,
    };
    let dir = exe.parent().ok_or("cannot locate install dir")?;
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("cannot locate install dir")?;
    let staging = dir.join(format!(
        ".kurisu-new-{}-{}",
        std::process::id(),
        scratch_suffix()
    ));
    let backup = dir.join(format!("{name}.kurisu-old"));
    let result = (|| -> io::Result<()> {
        let mut staged = std::fs::File::create(&staging)?;
        io::copy(new_bin, &mut staged)?;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
        // Flush data AND metadata before the renames commit the path. Without
        // this a crash in the writeback window can leave a zero-length exe.
        staged.sync_all()?;
        drop(staged);
        std::fs::rename(&exe, &backup)?;
        if let Err(e) = std::fs::rename(&staging, &exe) {
            // Roll the exe swap back. If even that fails, the install is left
            // with no working exe, only the backup. Drop a marker the next
            // launch surfaces, and sweep_install_leftovers keeps the orphaned
            // backup, so a manual restore is always possible.
            if std::fs::rename(&backup, &exe).is_err() {
                let _ = std::fs::write(dir.join(FAILED_MARKER), "");
            }
            sync_dir(dir);
            return Err(e);
        }
        sync_dir(dir);
        Ok(())
    })();
    let _ = std::fs::remove_file(&staging);
    if result.is_ok() {
        UPDATE_APPLIED.store(true, Ordering::SeqCst);
    }
    result.map_err(|e| format!("could not install the update: {e}"))
}

/// fsync a directory so rename metadata inside it survives a crash. Best
/// effort. The swap itself already succeeded, or already failed.
#[cfg(target_os = "linux")]
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// The e_machine value an ELF must carry for this process's architecture, when
/// known. None means we cannot check, so we do not refuse.
#[cfg(target_os = "linux")]
fn expected_elf_machine() -> Option<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Some(62),   // EM_X86_64
        "aarch64" => Some(183), // EM_AARCH64
        _ => None,
    }
}

/// True when a 20-byte ELF prefix, e_ident plus e_type plus e_machine, matches
/// the running architecture. e_machine sits at offset 18 for both ELF classes.
/// EI_DATA, byte 5, picks its endianness. Anything unparseable is NOT a match.
#[cfg(target_os = "linux")]
fn elf_header_matches_arch(ident: &[u8; 20]) -> bool {
    let Some(want) = expected_elf_machine() else {
        return true;
    };
    if ident[0..4] != [0x7f, b'E', b'L', b'F'] {
        return false;
    }
    let machine = match ident[5] {
        1 => u16::from_le_bytes([ident[18], ident[19]]),
        2 => u16::from_be_bytes([ident[18], ident[19]]),
        _ => return false,
    };
    machine == want
}

/// Read the ELF header from the verified handle, then rewind. The install
/// copies from this same handle, so it must be left back at offset 0.
#[cfg(target_os = "linux")]
fn elf_file_matches_arch(file: &mut std::fs::File) -> io::Result<bool> {
    use std::io::{Read, Seek};
    let mut ident = [0u8; 20];
    file.read_exact(&mut ident)?;
    file.seek(io::SeekFrom::Start(0))?;
    Ok(elf_header_matches_arch(&ident))
}

// ── Leftover sweeps ─────────────────────────────────────────────────────────

/// Remove leftover .kurisu-update-* downloads in `dir`. A finished or aborted
/// update leaves the download behind. Best-effort, every launch. Files younger
/// than an hour are left alone. Without a single-instance guard a second
/// Kurisu could be mid-download, chunk writes keep the mtime fresh, or in the
/// verify gap, and unlinking its file out from under it aborts that update.
/// The download timeout is 30 minutes, so an hour means certainly dead.
pub fn sweep_update_leftovers(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".kurisu-update-")
            {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|age| age > Duration::from_secs(3600)).unwrap_or(true))
                .unwrap_or(true);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Remove update leftovers next to the installed exe. .kurisu-new-* staging
/// files from an interrupted swap, and <name>.kurisu-old backups. A launched
/// build no longer needs its rollback copy, since the swap already proved
/// itself by running. A backup whose exe is MISSING is kept. After a doubly
/// failed swap it's the only working copy, and deleting it would brick the
/// install.
pub fn sweep_install_leftovers(exe_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(exe_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".kurisu-new-") {
                let _ = std::fs::remove_file(entry.path());
            } else if let Some(exe_name) = name.strip_suffix(".kurisu-old") {
                if exe_dir.join(exe_name).exists() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("v0.3.1"), vec![0, 3, 1]);
        assert_eq!(parse_version("1.0.0.8"), vec![1, 0, 0, 8]);
        assert_eq!(parse_version("0.4-rc1"), vec![0, 4]); // leading digits only
        assert_eq!(parse_version(""), vec![0]);
        // Rolling tags sort above their X.Y.Z base and increase per build.
        assert!(is_newer("1.0.0.8", "1.0.0"));
        assert!(is_newer("1.0.0.8", "1.0.0.7"));
        assert!(is_newer("1.1.0", "1.0.0.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0.7", "1.0.0.8"));
        // Prereleases sort BELOW the plain release of the same numbers. Digit
        // runs order rc1 < rc2. A rolling 4th segment still beats any rc.
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(is_newer("1.0.0-rc2", "1.0.0-rc1"));
        assert!(is_newer("1.0.0.1", "1.0.0-rc9"));
    }

    #[test]
    fn platform_asset_picks_this_platforms_asset() {
        let mut rel = Release::default();
        // A release carrying ONLY sidecars and near-miss names has nothing
        // this platform can install. The Windows installer is deliberately
        // absent here. Asserting None while it was present made this test
        // impossible to pass on Windows, since platform_asset would rightly
        // have found it.
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe.sha256".into(), "u2".into());
        rel.assets.insert("kurisu.exe".into(), "u3".into());
        rel.assets.insert("kurisu.sha256".into(), "u4".into());
        assert_eq!(platform_asset(&rel), None);
        // Now publish both platforms' real assets.
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "u1".into());
        rel.assets.insert("kurisu".into(), "u5".into());
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(platform_asset(&rel), Some("kurisu"));
        // CI publishes x86-64 only. Other arches get no installable asset.
        #[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
        assert_eq!(platform_asset(&rel), None);
        #[cfg(target_os = "windows")]
        assert_eq!(platform_asset(&rel), Some("Kurisu_1.0.0_x64-setup.exe"));
    }

    #[test]
    fn exe_path_backstop_rejects_backup_and_deleted_names() {
        assert!(sane_exe_path(PathBuf::from("/usr/bin/kurisu")).is_ok());
        assert!(sane_exe_path(PathBuf::from("/usr/bin/kurisu.kurisu-old")).is_err());
        assert!(sane_exe_path(PathBuf::from("/usr/bin/kurisu (deleted)")).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn elf_header_matches_running_arch() {
        let Some(machine) = expected_elf_machine() else {
            return; // an arch with no known e_machine, nothing to assert here
        };
        let mut ident = [0u8; 20];
        ident[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        ident[5] = 1; // little-endian
        ident[18..20].copy_from_slice(&machine.to_le_bytes());
        assert!(elf_header_matches_arch(&ident));
        // The other supported architecture's machine id is refused.
        let other: u16 = if machine == 62 { 183 } else { 62 };
        ident[18..20].copy_from_slice(&other.to_le_bytes());
        assert!(!elf_header_matches_arch(&ident));
        // Big-endian encoding of the right machine is honored.
        ident[5] = 2;
        ident[18..20].copy_from_slice(&machine.to_be_bytes());
        assert!(elf_header_matches_arch(&ident));
        // No ELF magic, no match.
        ident[0] = 0;
        assert!(!elf_header_matches_arch(&ident));
    }
}
