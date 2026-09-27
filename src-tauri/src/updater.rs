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

/// Legacy failure marker. Keep until acknowledged.
pub const FAILED_MARKER: &str = ".kurisu-update-failed";

pub const FAILED_MESSAGE: &str = "The last update failed to install cleanly, so the previous version was kept. Nothing was lost — you can retry the update from Settings.";

/// Keep the original launch path after Linux replaces the running inode.
static EXE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// After a swap, require restart before offering another install.
static UPDATE_APPLIED: AtomicBool = AtomicBool::new(false);

static UPDATE_FAILED: AtomicBool = AtomicBool::new(false);

/// Retain the latest result for a webview that misses the event.
static PENDING_UPDATE: Mutex<Option<Value>> = Mutex::new(None);

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[cfg(any(windows, target_os = "linux"))]
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(any(windows, target_os = "linux"))]
fn scratch_suffix() -> u64 {
    SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
}

pub fn init_install_path() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = EXE_PATH.set(sane_exe_path(exe)?);
    Ok(())
}

fn sane_exe_path(exe: PathBuf) -> Result<PathBuf, String> {
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".kurisu-old") || name.contains(" (deleted)") {
        return Err(format!("refusing to update the install at {name}"));
    }
    Ok(exe)
}

fn install_dir() -> Option<PathBuf> {
    let exe = EXE_PATH
        .get()
        .cloned()
        .or_else(|| std::env::current_exe().ok())?;
    exe.parent().map(Path::to_path_buf)
}

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

pub fn set_pending_update(payload: Value) {
    *PENDING_UPDATE.lock() = Some(payload);
}

#[tauri::command]
pub fn take_pending_update() -> Option<Value> {
    PENDING_UPDATE.lock().take()
}

#[allow(dead_code)]
pub fn parse_version(s: &str) -> Vec<u64> {
    version_key(s).0
}

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

pub fn is_newer(remote: &str, current: &str) -> bool {
    version_key(remote) > version_key(current)
}

/// Stamp the full rolling version to avoid offering the installed release again.
pub fn current_version() -> &'static str {
    match option_env!("KURISU_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

pub fn is_ci_build() -> bool {
    matches!(option_env!("KURISU_BUILD_VERSION"), Some(v) if !v.is_empty())
}

/// Release builds check on every platform. Debug builds stay quiet unless stamped.
pub fn auto_check_eligible() -> bool {
    is_ci_build() || !cfg!(debug_assertions)
}

#[derive(Debug, Clone, Default)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub html_url: String,
    pub body: String,
    pub assets: HashMap<String, String>,
}

pub fn update_info(rel: &Release) -> Value {
    serde_json::json!({
        "available": is_newer(&rel.version, current_version()),
        "can_install": platform_asset(rel).is_some(),
        "restart_pending": update_applied(),
        "version": rel.version,
        "tag": rel.tag,
        "html_url": rel.html_url,
        "body": rel.body,
        "current": current_version(),
    })
}

pub async fn watch_updates(enabled: impl Fn() -> bool, notify: impl FnMut(Value)) {
    poll_releases(enabled, fetch_latest_release, notify).await;
}

async fn poll_releases<F, Fut>(enabled: impl Fn() -> bool, fetch: F, mut notify: impl FnMut(Value))
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Release, String>>,
{
    let mut interval = tokio::time::interval(UPDATE_CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut notified_tag = String::new();
    loop {
        interval.tick().await;
        if !enabled() || update_applied() {
            continue;
        }
        let rel = match fetch().await {
            Ok(rel) => rel,
            Err(error) => {
                log::debug!("update check failed: {error}");
                continue;
            }
        };
        if !enabled() || update_applied() || rel.tag == notified_tag {
            continue;
        }
        if is_newer(&rel.version, current_version()) {
            notified_tag.clone_from(&rel.tag);
            notify(update_info(&rel));
        }
    }
}

/// Choose by version. Publication order can put an older rolling build after a milestone.
pub async fn fetch_latest_release() -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
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
    pick_latest_release(&data).ok_or_else(|| "no published releases found".to_string())
}

fn pick_latest_release(data: &Value) -> Option<Release> {
    let arr = data.as_array()?;
    let mut best: Option<Release> = None;
    let mut best_key: Option<(Vec<u64>, u8, u64)> = None;
    for entry in arr {
        if entry.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if entry.get("prerelease").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let rel = parse_release(entry);
        let key = version_key(&rel.version);
        if best_key.as_ref().is_none_or(|k| key > *k) {
            best_key = Some(key);
            best = Some(rel);
        }
    }
    best
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
        body: data
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        assets,
    }
}

pub fn update_applied() -> bool {
    UPDATE_APPLIED.load(Ordering::SeqCst)
}

#[cfg(any(windows, test))]
fn windows_installer<'a>(names: impl Iterator<Item = &'a String>, arch: &str) -> Option<&'a str> {
    let marker = match arch {
        "x86_64" => "_x64-",
        _ => return None,
    };
    let mut hits = names.filter(|n| n.ends_with("-setup.exe") && n.contains(marker));
    match (hits.next(), hits.next()) {
        (Some(only), None) => Some(only.as_str()),
        _ => None,
    }
}

pub fn platform_asset(rel: &Release) -> Option<&str> {
    if update_applied() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        windows_installer(rel.assets.keys(), std::env::consts::ARCH)
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

#[cfg(any(windows, target_os = "linux"))]
pub async fn fetch_sidecar(rel: &Release, asset_name: &str) -> Option<String> {
    const MAX_SIDECAR_BYTES: usize = 4096;
    let url = rel.assets.get(&format!("{asset_name}.sha256"))?;
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let mut resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
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

#[cfg(any(windows, target_os = "linux"))]
const MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024;

#[cfg(any(windows, target_os = "linux"))]
pub async fn download(url: &str, dest: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let part = {
        let mut p = dest.as_os_str().to_os_string();
        p.push(format!(".part-{}", scratch_suffix()));
        PathBuf::from(p)
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let res: Result<(), String> = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // Allow slow links enough time to download the Windows installer.
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
            return Err(format!(
                "update download is implausibly large ({total} bytes)"
            ));
        }
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| e.to_string())?;
        let mut got: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
            if got > MAX_DOWNLOAD_BYTES {
                return Err(format!(
                    "update download exceeded {MAX_DOWNLOAD_BYTES} bytes"
                ));
            }
        }
        file.flush().await.map_err(|e| e.to_string())?;
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        if total != 0 && got < total {
            return Err(format!("download incomplete: {got} of {total} bytes"));
        }
        tokio::fs::rename(&part, dest)
            .await
            .map_err(|e| e.to_string())?;
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

/// Return the verified handle, rewound. None is a mismatch and errors also refuse installation.
/// Linux copies from this handle. Windows must keep it open through installer launch.
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

#[cfg(target_os = "linux")]
pub fn apply_linux_update(new_bin: &mut std::fs::File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if UPDATE_APPLIED.load(Ordering::SeqCst) {
        return Err("an update was already installed; restart Kurisu to finish it".to_string());
    }
    if !elf_file_matches_arch(new_bin)
        .map_err(|e| format!("could not read the downloaded update: {e}"))?
    {
        return Err(
            "the downloaded update is not built for this machine's architecture".to_string(),
        );
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
        // Flush before publishing so a crash cannot leave an empty executable.
        staged.sync_all()?;
        drop(staged);
        preserve_executable(&exe, &backup)?;
        std::fs::rename(&staging, &exe)?;
        sync_dir(dir);
        Ok(())
    })();
    let _ = std::fs::remove_file(&staging);
    if result.is_ok() {
        UPDATE_APPLIED.store(true, Ordering::SeqCst);
    }
    result.map_err(|e| format!("could not install the update: {e}"))
}

#[cfg(target_os = "linux")]
fn preserve_executable(exe: &Path, backup: &Path) -> io::Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("no install directory"))?;
    let temporary = dir.join(format!(
        ".kurisu-new-{}-{}",
        std::process::id(),
        scratch_suffix()
    ));
    let result = (|| {
        if std::fs::hard_link(exe, &temporary).is_err() {
            let mut old = std::fs::File::open(exe)?;
            let mut copy = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            io::copy(&mut old, &mut copy)?;
            copy.set_permissions(old.metadata()?.permissions())?;
            copy.sync_all()?;
        }
        std::fs::rename(&temporary, backup)?;
        sync_dir(dir);
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

#[cfg(target_os = "linux")]
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

#[cfg(target_os = "linux")]
fn expected_elf_machine() -> Option<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Some(62),   // EM_X86_64
        "aarch64" => Some(183), // EM_AARCH64
        _ => None,
    }
}

/// ELF byte 5 selects endianness. The machine ID starts at byte 18 in both classes.
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

#[cfg(target_os = "linux")]
fn elf_file_matches_arch(file: &mut std::fs::File) -> io::Result<bool> {
    use std::io::{Read, Seek};
    let mut ident = [0u8; 20];
    file.read_exact(&mut ident)?;
    file.seek(io::SeekFrom::Start(0))?;
    Ok(elf_header_matches_arch(&ident))
}

/// Leave files younger than an hour in case another process is still downloading or verifying.
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
                // A future mtime means a clock change. Treat it as fresh.
                .map(|t| {
                    t.elapsed()
                        .map(|age| age > Duration::from_secs(3600))
                        .unwrap_or(false)
                })
                .unwrap_or(true);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Keep backups whose executable is missing. They may be the only working copy.
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
        assert_eq!(parse_version("0.4-rc1"), vec![0, 4]);
        assert_eq!(parse_version(""), vec![0]);
        assert!(is_newer("1.0.0.8", "1.0.0"));
        assert!(is_newer("1.0.0.8", "1.0.0.7"));
        assert!(is_newer("1.1.0", "1.0.0.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0.7", "1.0.0.8"));
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(is_newer("1.0.0-rc2", "1.0.0-rc1"));
        assert!(is_newer("1.0.0.1", "1.0.0-rc9"));
    }

    #[test]
    fn auto_check_eligibility_covers_every_build_kind() {
        if is_ci_build() {
            assert!(auto_check_eligible());
        } else if cfg!(debug_assertions) {
            assert!(!auto_check_eligible());
        } else {
            assert!(auto_check_eligible());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_checks_retry_offline_starts_and_respect_the_setting() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let enabled = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let revision = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_releases(
            {
                let enabled = enabled.clone();
                move || enabled.load(Ordering::SeqCst)
            },
            {
                let attempts = attempts.clone();
                let revision = revision.clone();
                move || {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    let version = format!("9999.0.{}", revision.load(Ordering::SeqCst));
                    async move {
                        if attempt == 0 {
                            return Err("offline".into());
                        }
                        Ok(Release {
                            tag: format!("v{version}"),
                            version,
                            body: "Release notes".into(),
                            ..Release::default()
                        })
                    }
                }
            },
            move |payload| tx.send(payload).unwrap(),
        ));
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err());

        tokio::time::advance(UPDATE_CHECK_INTERVAL).await;
        tokio::task::yield_now().await;
        let notice = rx.try_recv().expect("retry should announce the release");
        assert_eq!(notice["version"], "9999.0.0");
        assert_eq!(notice["body"], "Release notes");
        assert_eq!(notice["available"], true);
        assert_eq!(notice["can_install"], false);

        tokio::time::advance(UPDATE_CHECK_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(
            rx.try_recv().is_err(),
            "do not announce the same release twice"
        );

        enabled.store(false, Ordering::SeqCst);
        tokio::time::advance(UPDATE_CHECK_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(rx.try_recv().is_err());

        enabled.store(true, Ordering::SeqCst);
        revision.store(1, Ordering::SeqCst);
        tokio::time::advance(UPDATE_CHECK_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(rx.try_recv().unwrap()["version"], "9999.0.1");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_checks_while_fetching_suppresses_the_notice() {
        use std::sync::Arc;

        let enabled = Arc::new(AtomicBool::new(true));
        let response = Arc::new(tokio::sync::Notify::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_releases(
            {
                let enabled = enabled.clone();
                move || enabled.load(Ordering::SeqCst)
            },
            {
                let response = response.clone();
                move || {
                    let response = response.clone();
                    async move {
                        response.notified().await;
                        Ok(Release {
                            tag: "v9999.0.0".into(),
                            version: "9999.0.0".into(),
                            ..Release::default()
                        })
                    }
                }
            },
            move |payload| tx.send(payload).unwrap(),
        ));
        tokio::task::yield_now().await;
        enabled.store(false, Ordering::SeqCst);
        response.notify_one();
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn release_walk_picks_the_highest_version_not_the_newest_published() {
        let listing = serde_json::json!([
            { "tag_name": "v1.0.0.412", "html_url": "https://x/412", "assets": [] },
            { "tag_name": "v1.1.0", "html_url": "https://x/110", "assets": [] },
            { "tag_name": "v1.0.0.413", "html_url": "https://x/413", "assets": [] },
            { "tag_name": "v9.9.9", "draft": true, "assets": [] },
            { "tag_name": "v8.8.8", "prerelease": true, "assets": [] }
        ]);
        let rel = pick_latest_release(&listing).expect("a winner");
        assert_eq!(rel.version, "1.1.0");
        assert_eq!(rel.tag, "v1.1.0");
        let listing = serde_json::json!([
            { "tag_name": "v1.1.0", "assets": [] },
            { "tag_name": "v1.1.0.2", "assets": [] }
        ]);
        let rel = pick_latest_release(&listing).expect("a winner");
        assert_eq!(rel.version, "1.1.0.2");
        assert!(pick_latest_release(&serde_json::json!([
            { "tag_name": "v9.9.9", "draft": true }
        ]))
        .is_none());
        assert!(pick_latest_release(&serde_json::json!([])).is_none());
    }

    #[test]
    fn windows_installer_requires_arch_marker_and_a_unique_match() {
        let mut rel = Release::default();
        rel.assets
            .insert("Kurisu_1.0.0_arm64-setup.exe".into(), "u1".into());
        assert_eq!(windows_installer(rel.assets.keys(), "x86_64"), None);
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "u2".into());
        assert_eq!(
            windows_installer(rel.assets.keys(), "x86_64"),
            Some("Kurisu_1.0.0_x64-setup.exe")
        );
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe.sha256".into(), "u3".into());
        assert_eq!(
            windows_installer(rel.assets.keys(), "x86_64"),
            Some("Kurisu_1.0.0_x64-setup.exe")
        );
        rel.assets
            .insert("Kurisu_1.0.0_x64-debug-setup.exe".into(), "u4".into());
        assert_eq!(windows_installer(rel.assets.keys(), "x86_64"), None);
        assert_eq!(windows_installer(rel.assets.keys(), "aarch64"), None);
    }

    #[test]
    fn platform_asset_picks_this_platforms_asset() {
        let mut rel = Release::default();
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe.sha256".into(), "u2".into());
        rel.assets.insert("kurisu.exe".into(), "u3".into());
        rel.assets.insert("kurisu.sha256".into(), "u4".into());
        assert_eq!(platform_asset(&rel), None);
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "u1".into());
        rel.assets.insert("kurisu".into(), "u5".into());
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(platform_asset(&rel), Some("kurisu"));
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

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn verify_and_open_accepts_only_a_matching_digest() {
        let dir = std::env::temp_dir().join(format!("kurisu-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.bin");
        std::fs::write(&path, b"kurisu update payload").unwrap();
        let digest = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"kurisu update payload");
            let d = h.finalize();
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let sidecars = [
            digest.clone(),
            format!("{digest}  payload.bin"),
            format!("SHA256 (payload.bin) = {digest}"),
            digest.to_uppercase(),
        ];
        for sc in &sidecars {
            let mut f = verify_and_open(&path, sc)
                .expect("sidecar readable")
                .expect("digest matched");
            let mut buf = String::new();
            use std::io::Read;
            f.read_to_string(&mut buf).unwrap();
            assert_eq!(
                buf, "kurisu update payload",
                "handle must be rewound, sidecar {sc}"
            );
        }
        let mut wrong = digest.clone();
        wrong.replace_range(0..1, if wrong.starts_with('0') { "1" } else { "0" });
        assert!(matches!(verify_and_open(&path, &wrong), Ok(None)));
        assert!(verify_and_open(&path, "not a digest").is_err());
        assert!(verify_and_open(&path, "").is_err());
        assert!(verify_and_open(&dir.join("absent.bin"), &digest).is_err());
        let short = dir.join("short.bin");
        std::fs::write(&short, b"xy").unwrap();
        assert!(matches!(verify_and_open(&short, &digest), Ok(None)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn elf_header_matches_running_arch() {
        let Some(machine) = expected_elf_machine() else {
            return;
        };
        let mut ident = [0u8; 20];
        ident[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        ident[5] = 1;
        ident[18..20].copy_from_slice(&machine.to_le_bytes());
        assert!(elf_header_matches_arch(&ident));
        let other: u16 = if machine == 62 { 183 } else { 62 };
        ident[18..20].copy_from_slice(&other.to_le_bytes());
        assert!(!elf_header_matches_arch(&ident));
        ident[5] = 2;
        ident[18..20].copy_from_slice(&machine.to_be_bytes());
        assert!(elf_header_matches_arch(&ident));
        ident[0] = 0;
        assert!(!elf_header_matches_arch(&ident));
    }
}
