use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

const REPO: &str = "CateDesu/Kurisu";
const USER_AGENT: &str = "Kurisu";

/// Keep the failure marker until acknowledged.
pub const FAILED_MARKER: &str = ".kurisu-update-failed";

pub const FAILED_MESSAGE: &str = "The last update failed to install cleanly, so the previous version was kept. Nothing was lost — you can retry the update from Settings.";

/// Keep the original launch path after Linux replaces the running inode.
static EXE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Swaps require restart before another install.
static UPDATE_APPLIED: AtomicBool = AtomicBool::new(false);

static UPDATE_FAILED: AtomicBool = AtomicBool::new(false);

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

pub fn take_update_failed() -> Option<String> {
    if !UPDATE_FAILED.swap(false, Ordering::SeqCst) {
        return None;
    }
    if let Some(dir) = install_dir() {
        let _ = std::fs::remove_file(dir.join(FAILED_MARKER));
    }
    Some(FAILED_MESSAGE.to_string())
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

/// Stamp the full rolling version to prevent repeat update offers.
pub fn current_version() -> &'static str {
    match option_env!("KURISU_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

pub fn is_ci_build() -> bool {
    matches!(option_env!("KURISU_BUILD_VERSION"), Some(v) if !v.is_empty())
}

/// Unstamped debug builds skip automatic checks.
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

/// Native artifacts keep a distinct name from the frozen bridge.
pub fn platform_asset_channel(rel: &Release, channel: crate::UpdateChannel) -> Option<&str> {
    if update_applied() {
        return None;
    }
    channel_asset(rel, channel)
}

fn channel_asset(rel: &Release, channel: crate::UpdateChannel) -> Option<&str> {
    if channel == crate::UpdateChannel::Legacy {
        return legacy_asset(rel);
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::consts::ARCH != "x86_64" {
            return None;
        }
        rel.assets
            .get_key_value("kurisu-native")
            .map(|(name, _)| name.as_str())
    }
    #[cfg(target_os = "windows")]
    {
        if std::env::consts::ARCH != "x86_64" {
            return None;
        }
        native_windows_installer(rel.assets.keys(), std::env::consts::ARCH)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = rel;
        None
    }
}

pub fn update_info_channel(
    rel: &Release,
    channel: crate::UpdateChannel,
) -> crate::models::UpdateInfo {
    let mut info: crate::models::UpdateInfo =
        serde_json::from_value(update_info(rel)).expect("update payload shape");
    info.can_install = platform_asset_channel(rel, channel).is_some();
    if channel == crate::UpdateChannel::Native {
        info.available &= info.can_install;
    }
    info
}
pub async fn watch_updates(enabled: impl Fn() -> bool, notify: impl FnMut(Value)) {
    poll_releases(enabled, fetch_latest_release, notify).await;
}

pub async fn watch_updates_channel(
    enabled: impl Fn() -> bool,
    channel: crate::UpdateChannel,
    notify: impl FnMut(crate::models::UpdateInfo),
) {
    poll_releases_with(
        enabled,
        || fetch_latest_release_channel(crate::UpdateChannel::Native),
        |rel| {
            if channel == crate::UpdateChannel::Legacy {
                bridge_update_info(rel)
            } else {
                update_info_channel(rel, channel)
            }
        },
        notify,
    )
    .await;
}

async fn poll_releases<F, Fut>(enabled: impl Fn() -> bool, fetch: F, notify: impl FnMut(Value))
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Release, String>>,
{
    poll_releases_with(enabled, fetch, update_info, notify).await;
}

async fn poll_releases_with<F, Fut, T>(
    enabled: impl Fn() -> bool,
    fetch: F,
    select: impl Fn(&Release) -> T,
    mut notify: impl FnMut(T),
) where
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
            notify(select(&rel));
        }
    }
}

/// Compare versions because publication order can put older builds last.
pub async fn fetch_latest_release() -> Result<Release, String> {
    pick_latest_release(&fetch_release_listing().await?)
        .ok_or_else(|| "no published releases found".to_string())
}

pub async fn fetch_latest_release_channel(
    channel: crate::UpdateChannel,
) -> Result<Release, String> {
    let data = fetch_release_listing().await?;
    pick_latest_release_channel(&data, channel)
        .ok_or_else(|| "no published release for this update channel".to_string())
}

pub async fn fetch_bridge_release() -> Result<Release, String> {
    let data = fetch_release_listing().await?;
    pick_latest_release_channel(&data, crate::UpdateChannel::Native)
        .or_else(|| pick_latest_release(&data))
        .ok_or_else(|| "no published releases found".to_string())
}

pub fn bridge_update_info(rel: &Release) -> crate::models::UpdateInfo {
    let mut info = update_info_channel(rel, crate::UpdateChannel::Native);
    info.available = bridge_update_available(rel, current_version());
    info
}

fn bridge_update_available(rel: &Release, current: &str) -> bool {
    platform_asset_channel(rel, crate::UpdateChannel::Native).is_some()
        && !is_newer(current, &rel.version)
}

async fn fetch_release_listing() -> Result<Value, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    #[cfg(feature = "test-support")]
    let url = fixture_release_endpoint()?.map_or(url, |url| url.to_string());
    let data: Value = update_request(&url, Duration::from_secs(8))?
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(data)
}

fn update_request(url: &str, timeout: Duration) -> Result<reqwest::RequestBuilder, String> {
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout);
    #[cfg(feature = "test-support")]
    let builder = if let Some(origin) = fixture_release_endpoint()? {
        validate_fixture_update_url(url, &origin)?;
        builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
    } else {
        builder
    };
    Ok(builder.build().map_err(|e| e.to_string())?.get(url))
}

#[cfg(feature = "test-support")]
fn fixture_release_endpoint() -> Result<Option<reqwest::Url>, String> {
    fixture_release_value(std::env::var("KURISU_FIXTURE_RELEASES"))
}

#[cfg(feature = "test-support")]
fn fixture_release_value(
    value: Result<String, std::env::VarError>,
) -> Result<Option<reqwest::Url>, String> {
    match value {
        Ok(value) => crate::anilist::fixture_loopback_http_url(&value)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err("invalid update fixture URL".into()),
    }
}

#[cfg(feature = "test-support")]
fn validate_fixture_update_url(url: &str, origin: &reqwest::Url) -> Result<(), String> {
    let target =
        crate::anilist::fixture_loopback_http_url(url).map_err(|error| error.to_string())?;
    if target.origin() != origin.origin() {
        return Err("update fixture assets must use the release fixture origin".into());
    }
    Ok(())
}

fn pick_latest_release(data: &Value) -> Option<Release> {
    pick_release(data, |_| true)
}

fn pick_latest_release_channel(data: &Value, channel: crate::UpdateChannel) -> Option<Release> {
    pick_release(data, |rel| channel_asset(rel, channel).is_some())
}

fn pick_release(data: &Value, accepts: impl Fn(&Release) -> bool) -> Option<Release> {
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
        if !accepts(&rel) {
            continue;
        }
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
    let mut hits = names
        .filter(|n| n.starts_with("Kurisu_") && n.ends_with("-setup.exe") && n.contains(marker));
    match (hits.next(), hits.next()) {
        (Some(only), None) => Some(only.as_str()),
        _ => None,
    }
}

#[cfg(any(windows, test))]
fn native_windows_installer<'a>(
    names: impl Iterator<Item = &'a String>,
    arch: &str,
) -> Option<&'a str> {
    if arch != "x86_64" {
        return None;
    }
    let names: Vec<_> = names
        .filter(|name| name.starts_with("Kurisu-Native_"))
        .collect();
    for suffix in ["_x64-install.exe", "_x64-setup.exe"] {
        let mut hits = names.iter().filter(|name| name.ends_with(suffix));
        match (hits.next(), hits.next()) {
            (Some(name), None) => return Some(name.as_str()),
            (Some(_), Some(_)) => return None,
            _ => {}
        }
    }
    None
}

pub fn platform_asset(rel: &Release) -> Option<&str> {
    if update_applied() {
        return None;
    }
    legacy_asset(rel)
}

fn legacy_asset(rel: &Release) -> Option<&str> {
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
    let mut resp = update_request(url, Duration::from_secs(15))
        .ok()?
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
    let mut created = false;
    let res: Result<(), String> = async {
        let mut resp = update_request(url, Duration::from_secs(1800))?
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
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
            .map_err(|e| e.to_string())?;
        created = true;
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
    if res.is_err() && created {
        let _ = tokio::fs::remove_file(&part).await;
    }
    res
}

/// Return a rewound verified handle, or None on mismatch.
/// Linux copies from it. Windows holds it through installer launch.
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
    let _installation = lock_install_dir(dir)
        .map_err(|error| format!("could not lock the installation: {error}"))?;
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
    let mut created = false;
    let result = (|| -> io::Result<()> {
        let mut staged = std::fs::File::create_new(&staging)?;
        created = true;
        io::copy(new_bin, &mut staged)?;
        staged.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        // Flush before publishing so a crash cannot leave an empty executable.
        staged.sync_all()?;
        drop(staged);
        preserve_executable(&exe, &backup)?;
        std::fs::rename(&staging, &exe)?;
        sync_dir(dir);
        Ok(())
    })();
    if created {
        let _ = std::fs::remove_file(&staging);
    }
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
    let mut created = false;
    let result = (|| {
        if std::fs::hard_link(exe, &temporary).is_err() {
            let mut old = std::fs::File::open(exe)?;
            let mut copy = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            created = true;
            io::copy(&mut old, &mut copy)?;
            copy.set_permissions(old.metadata()?.permissions())?;
            copy.sync_all()?;
        } else {
            created = true;
        }
        std::fs::rename(&temporary, backup)?;
        sync_dir(dir);
        Ok(())
    })();
    if created {
        let _ = std::fs::remove_file(&temporary);
    }
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

/// Keep recent files that another process may still be downloading or verifying.
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
    #[cfg(target_os = "linux")]
    let Ok(_installation) = lock_install_dir(exe_dir) else {
        return;
    };
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

#[cfg(target_os = "linux")]
fn lock_install_dir(dir: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Keep the inode in place so every process locks the same installation.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join(".kurisu-install.lock"))?;
    if !lock.metadata()?.is_file() {
        return Err(io::Error::other("installation lock is not a regular file"));
    }
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => io::Error::other("an update is already in progress"),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-support")]
    #[test]
    fn fixture_release_configuration_fails_closed() {
        assert!(fixture_release_value(Err(std::env::VarError::NotPresent))
            .unwrap()
            .is_none());
        for value in [
            "",
            "https://127.0.0.1/releases",
            "http://localhost/releases",
            "http://example.com/releases",
            "http://127.0.0.1/releases?token=secret",
            "http://user@127.0.0.1/releases",
        ] {
            assert!(fixture_release_value(Ok(value.into())).is_err());
        }
        assert!(
            fixture_release_value(Err(std::env::VarError::NotUnicode("invalid".into()))).is_err()
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn fixture_update_assets_stay_on_the_release_origin() {
        let origin = fixture_release_value(Ok("http://127.0.0.1:39393/releases".into()))
            .unwrap()
            .unwrap();
        assert!(
            validate_fixture_update_url("http://127.0.0.1:39393/kurisu-native", &origin).is_ok()
        );
        for url in [
            "http://127.0.0.1:39394/kurisu-native",
            "http://127.0.0.2:39393/kurisu-native",
            "https://example.com/kurisu-native",
            "file:///tmp/kurisu-native",
        ] {
            assert!(validate_fixture_update_url(url, &origin).is_err());
        }
    }

    #[cfg(all(feature = "test-support", any(windows, target_os = "linux")))]
    #[tokio::test]
    async fn fixture_release_requests_bypass_proxy_and_refuse_redirects() {
        const CHILD: &str = "KURISU_TEST_RELEASE_FIXTURE_MODE";
        if let Ok(mode) = std::env::var(CHILD) {
            let result = fetch_latest_release().await;
            if mode == "ok" {
                assert_eq!(result.unwrap().tag, "v9999.0.0");
            } else {
                assert!(result.is_err());
            }
            return;
        }
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let body = r#"[{"tag_name":"v9999.0.0","draft":false,"prerelease":false,"assets":[]}]"#;
        for (mode, response) in [
            ("ok", format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())),
            ("redirect", format!("HTTP/1.1 302 Found\r\nLocation: http://{}/unexpected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap())),
        ] {
            let (url, server) = serve_download(response.into_bytes()).await;
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "updater::tests::fixture_release_requests_bypass_proxy_and_refuse_redirects", "--nocapture"])
                    .env(CHILD, mode)
                    .env("KURISU_FIXTURE_RELEASES", url)
                    .env("HTTP_PROXY", "http://127.0.0.1:1")
                    .env("ALL_PROXY", "http://127.0.0.1:1")
                    .env("NO_PROXY", "")
                    .output()
                    .unwrap()
            }).await.unwrap();
            assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            server.await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_update_staging_collisions_preserve_existing_paths() {
        use std::io::Read;
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        let exe = std::env::current_exe().unwrap();
        let child_name = "kurisu-update-staging-test";
        if exe.file_name().unwrap() != child_name {
            let dir = std::env::temp_dir().join(format!(
                "kurisu-update-staging-{}-{}",
                std::process::id(),
                scratch_suffix()
            ));
            std::fs::create_dir(&dir).unwrap();
            let child = dir.join(child_name);
            std::fs::copy(&exe, &child).unwrap();
            // Never replace Cargo's test executable or share updater globals.
            let result = std::process::Command::new(&child)
                .args([
                    "--exact",
                    "updater::tests::linux_update_staging_collisions_preserve_existing_paths",
                    "--nocapture",
                ])
                .output();
            std::fs::remove_dir_all(&dir).unwrap();
            let result = result.unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        init_install_path().unwrap();
        let dir = exe.parent().unwrap();
        let original = std::fs::metadata(&exe).unwrap();
        let digest_file = |path: &Path| {
            use sha2::{Digest, Sha256};
            let mut digest = Sha256::new();
            io::copy(&mut std::fs::File::open(path).unwrap(), &mut digest).unwrap();
            digest.finalize()
        };
        let original_digest = digest_file(&exe);
        let payload = dir.join("payload");
        // Reuse the running ELF header for a small publication fixture.
        let mut header = [0; 20];
        std::fs::File::open(&exe)
            .unwrap()
            .read_exact(&mut header)
            .unwrap();
        let mut replacement = header.to_vec();
        replacement.extend_from_slice(b"kurisu update payload");
        std::fs::write(&payload, &replacement).unwrap();
        let sentinel = dir.join("unrelated-data");
        std::fs::write(&sentinel, b"saved user data").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600)).unwrap();
        let absent = dir.join("absent-target");
        let backup = dir.join(format!("{child_name}.kurisu-old"));

        for backup_collision in [false, true] {
            for kind in ["symlink", "hardlink", "file", "dangling", "directory"] {
                let staging = dir.join(format!(
                    ".kurisu-new-{}-{}",
                    std::process::id(),
                    SCRATCH_SEQ.load(Ordering::Relaxed) + u64::from(backup_collision)
                ));
                match kind {
                    "symlink" => symlink(&sentinel, &staging).unwrap(),
                    "hardlink" => std::fs::hard_link(&sentinel, &staging).unwrap(),
                    "file" => std::fs::write(&staging, b"existing staging data").unwrap(),
                    "dangling" => symlink(&absent, &staging).unwrap(),
                    "directory" => std::fs::create_dir(&staging).unwrap(),
                    _ => unreachable!(),
                }
                let result = apply_linux_update(&mut std::fs::File::open(&payload).unwrap());
                assert!(
                    std::fs::read(&sentinel).unwrap() == b"saved user data",
                    "{kind}: unrelated data changed"
                );
                assert_eq!(
                    std::fs::metadata(&sentinel).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{kind}"
                );
                assert!(result.is_err(), "{kind}: {result:?}");
                let installed = std::fs::metadata(&exe).unwrap();
                assert_eq!(installed.ino(), original.ino(), "{kind}");
                assert_eq!(installed.len(), original.len(), "{kind}");
                assert!(!update_applied());
                assert!(!backup.exists());
                assert!(!absent.exists());
                assert!(
                    std::fs::symlink_metadata(&staging).is_ok(),
                    "{kind}: pre-existing staging path was removed, backup: {backup_collision}"
                );
                match kind {
                    "symlink" => assert_eq!(std::fs::read_link(&staging).unwrap(), sentinel),
                    "hardlink" => assert_eq!(std::fs::read(&staging).unwrap(), b"saved user data"),
                    "file" => {
                        assert_eq!(std::fs::read(&staging).unwrap(), b"existing staging data")
                    }
                    "dangling" => assert_eq!(std::fs::read_link(&staging).unwrap(), absent),
                    "directory" => assert!(staging.is_dir()),
                    _ => unreachable!(),
                }
                if kind == "directory" {
                    std::fs::remove_dir(&staging).unwrap();
                } else {
                    std::fs::remove_file(&staging).unwrap();
                }
            }
        }

        std::fs::create_dir(&backup).unwrap();
        assert!(apply_linux_update(&mut std::fs::File::open(&payload).unwrap()).is_err());
        assert_eq!(std::fs::metadata(&exe).unwrap().ino(), original.ino());
        assert!(!update_applied());
        assert!(std::fs::read_dir(dir).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".kurisu-new-")));
        std::fs::remove_dir(&backup).unwrap();

        apply_linux_update(&mut std::fs::File::open(&payload).unwrap()).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), replacement);
        assert_eq!(digest_file(&backup), original_digest);
        assert!(update_applied());
        assert!(apply_linux_update(&mut std::fs::File::open(&payload).unwrap()).is_err());
    }

    #[cfg(any(windows, target_os = "linux"))]
    async fn serve_download(response: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            socket.write_all(&response).await.unwrap();
        });
        (format!("http://{address}/update"), server)
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn download_staging_collisions_preserve_existing_paths() {
        const CHILD_DIR: &str = "KURISU_TEST_DOWNLOAD_COLLISIONS";
        let dir = match std::env::var_os(CHILD_DIR) {
            Some(dir) => PathBuf::from(dir),
            None => {
                let dir = std::env::temp_dir().join(format!(
                    "kurisu-download-collisions-{}-{}",
                    std::process::id(),
                    scratch_suffix()
                ));
                std::fs::create_dir(&dir).unwrap();
                // Isolate the scratch sequence from concurrently running tests.
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "updater::tests::download_staging_collisions_preserve_existing_paths",
                        "--nocapture",
                    ])
                    .env(CHILD_DIR, &dir)
                    .output();
                std::fs::remove_dir_all(&dir).unwrap();
                let output = output.unwrap();
                assert!(
                    output.status.success(),
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
        };
        let target = dir.join("payload.bin");
        let sentinel = dir.join("unrelated-data");
        let absent = dir.join("absent-target");
        std::fs::write(&target, b"previous download").unwrap();
        std::fs::write(&sentinel, b"saved user data").unwrap();
        #[cfg(unix)]
        let kinds = ["file", "directory", "symlink", "hardlink", "dangling"];
        #[cfg(windows)]
        let kinds = ["file", "directory"];
        for status in ["200 OK", "503 Unavailable"] {
            for kind in kinds {
                let part = target.with_file_name(format!(
                    "payload.bin.part-{}",
                    SCRATCH_SEQ.load(Ordering::Relaxed)
                ));
                match kind {
                    "file" => std::fs::write(&part, b"existing staging data").unwrap(),
                    "directory" => std::fs::create_dir(&part).unwrap(),
                    #[cfg(unix)]
                    "symlink" => std::os::unix::fs::symlink(&sentinel, &part).unwrap(),
                    #[cfg(unix)]
                    "dangling" => std::os::unix::fs::symlink(&absent, &part).unwrap(),
                    #[cfg(unix)]
                    "hardlink" => std::fs::hard_link(&sentinel, &part).unwrap(),
                    _ => unreachable!(),
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload"
                );
                let (url, server) = serve_download(response.into_bytes()).await;
                let result = tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
                    .await
                    .unwrap();
                server.await.unwrap();
                assert!(result.is_err(), "{kind}, {status}: {result:?}");
                assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"saved user data");
                assert!(!absent.exists());
                assert!(
                    std::fs::symlink_metadata(&part).is_ok(),
                    "{kind}, {status}: pre-existing staging path was removed"
                );
                match kind {
                    "file" => assert_eq!(std::fs::read(&part).unwrap(), b"existing staging data"),
                    "directory" => assert!(part.is_dir()),
                    #[cfg(unix)]
                    "symlink" => assert_eq!(std::fs::read_link(&part).unwrap(), sentinel),
                    #[cfg(unix)]
                    "dangling" => assert_eq!(std::fs::read_link(&part).unwrap(), absent),
                    #[cfg(unix)]
                    "hardlink" => assert_eq!(std::fs::read(&part).unwrap(), b"saved user data"),
                    _ => unreachable!(),
                }
                if kind == "directory" {
                    std::fs::remove_dir(&part).unwrap();
                } else {
                    std::fs::remove_file(&part).unwrap();
                }
            }
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn downloaded_payloads_are_published_complete_and_verify_from_disk() {
        use sha2::{Digest, Sha256};
        use std::io::Read;

        let payload = b"kurisu update payload";
        let digest = format!("{:x}", Sha256::digest(payload));
        for chunked in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "kurisu-download-{}-{}",
                std::process::id(),
                scratch_suffix()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join("payload.bin");
            std::fs::write(&target, b"previous download").unwrap();
            let mut response = if chunked {
                format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", payload.len())
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len())
            }
            .into_bytes();
            response.extend_from_slice(payload);
            if chunked {
                response.extend_from_slice(b"\r\n0\r\n\r\n");
            }
            let (url, server) = serve_download(response).await;
            tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
                .await
                .unwrap()
                .unwrap();
            server.await.unwrap();
            let mut verified = verify_and_open(&target, &digest).unwrap().unwrap();
            let mut received = Vec::new();
            verified.read_to_end(&mut received).unwrap();
            assert_eq!(received, payload);
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            drop(verified);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn failed_downloads_keep_the_previous_payload_and_remove_partial_files() {
        for response in [
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort body".into(),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n20\r\nshort body".into(),
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", MAX_DOWNLOAD_BYTES + 1),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "kurisu-download-failed-{}-{}",
                std::process::id(),
                scratch_suffix()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let target = dir.join("payload.bin");
            std::fs::write(&target, b"previous download").unwrap();
            let (url, server) = serve_download(response.into_bytes()).await;
            assert!(tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
                .await
                .unwrap()
                .is_err());
            server.await.unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(version_key("v0.3.1").0, vec![0, 3, 1]);
        assert_eq!(version_key("1.0.0.8").0, vec![1, 0, 0, 8]);
        assert_eq!(version_key("0.4-rc1").0, vec![0, 4]);
        assert_eq!(version_key("").0, vec![0]);
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
    fn legacy_windows_selector_ignores_native_installers() {
        let legacy = "Kurisu_1.0.0_x64-setup.exe".to_string();
        let native = "Kurisu-Native_1.0.0_x64-setup.exe".to_string();
        assert_eq!(
            windows_installer([&legacy].into_iter(), "x86_64"),
            Some(legacy.as_str())
        );
        assert_eq!(windows_installer([&native].into_iter(), "x86_64"), None);
        assert_eq!(
            windows_installer([&legacy, &native].into_iter(), "x86_64"),
            Some(legacy.as_str())
        );
        let second_legacy = "Kurisu_1.0.1_x64-setup.exe".to_string();
        assert_eq!(
            windows_installer([&legacy, &second_legacy, &native].into_iter(), "x86_64"),
            None
        );
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

#[cfg(test)]
mod native_selector_tests {
    use super::*;
    #[test]
    fn native_windows_names_preserve_the_original_published_selector() {
        let names = [
            "Kurisu_1.0.0_x64-setup.exe".to_string(),
            "Kurisu-Native_1.1.0_x64-install.exe".to_string(),
            "Kurisu-Native_1.1.0_x64-install.exe.sha256".to_string(),
        ];
        let original: Vec<_> = names
            .iter()
            .filter(|name| name.ends_with("-setup.exe") && name.contains("_x64-"))
            .collect();
        assert_eq!(original, vec![&names[0]]);
        assert_eq!(
            native_windows_installer(names.iter(), "x86_64"),
            Some(names[1].as_str())
        );
        assert_eq!(native_windows_installer(names.iter(), "aarch64"), None);
        let mut names = names.to_vec();
        names.push("Kurisu-Native_1.1.1_x64-install.exe".into());
        assert_eq!(native_windows_installer(names.iter(), "x86_64"), None);
    }

    #[test]
    fn native_windows_selector_accepts_old_candidates_only_as_a_fallback() {
        let old = "Kurisu-Native_1.0.0.77_x64-setup.exe".to_string();
        let new = "Kurisu-Native_1.1.0_x64-install.exe".to_string();
        assert_eq!(
            native_windows_installer([&old].into_iter(), "x86_64"),
            Some(old.as_str())
        );
        assert_eq!(
            native_windows_installer([&old, &new].into_iter(), "x86_64"),
            Some(new.as_str())
        );
        let other = "Kurisu-Native_1.0.0.78_x64-setup.exe".to_string();
        assert_eq!(
            native_windows_installer([&old, &other].into_iter(), "x86_64"),
            None
        );
    }

    #[test]
    fn skipped_bridge_handoff_installs_native_without_a_same_version_loop() {
        let mut release = Release {
            version: "1.1.0".into(),
            ..Release::default()
        };
        release
            .assets
            .insert("kurisu".into(), "frozen-bridge".into());
        release.assets.insert(
            "Kurisu_1.0.0_x64-setup.exe".into(),
            "frozen-bridge-windows".into(),
        );
        assert!(!bridge_update_available(&release, "1.0.0.78"));
        release
            .assets
            .insert("kurisu-native".into(), "native".into());
        release.assets.insert(
            "Kurisu-Native_1.1.0_x64-install.exe".into(),
            "native-windows".into(),
        );
        assert!(bridge_update_available(&release, "1.0.0.78"));
        assert!(bridge_update_available(&release, "1.1.0"));
        assert!(!bridge_update_available(&release, "1.2.0"));
        assert!(!is_newer(&release.version, "1.1.0"));
        assert_eq!(
            update_info_channel(&release, crate::UpdateChannel::Native).version,
            "1.1.0"
        );
    }

    #[test]
    fn native_release_selection_ignores_legacy_only_and_unpublished_versions() {
        let listing = serde_json::json!([
            {"tag_name": "v9.0.0", "assets": [{"name": "kurisu", "browser_download_url": "legacy"}]},
            {"tag_name": "v8.0.0", "draft": true, "assets": [
                {"name": "kurisu-native", "browser_download_url": "draft"},
                {"name": "Kurisu-Native_8.0.0_x64-install.exe", "browser_download_url": "draft"}
            ]},
            {"tag_name": "v1.1.0", "assets": [
                {"name": "kurisu", "browser_download_url": "bridge"},
                {"name": "Kurisu_1.0.0_x64-setup.exe", "browser_download_url": "bridge"},
                {"name": "kurisu-native", "browser_download_url": "native"},
                {"name": "Kurisu-Native_1.1.0_x64-install.exe", "browser_download_url": "native"}
            ]}
        ]);
        let release = pick_latest_release_channel(&listing, crate::UpdateChannel::Native).unwrap();
        assert_eq!(release.version, "1.1.0");
        assert_eq!(pick_latest_release(&listing).unwrap().version, "9.0.0");
    }

    #[test]
    fn native_channel_never_selects_legacy_artifacts() {
        let mut release = Release::default();
        release
            .assets
            .insert("kurisu".into(), "legacy-linux".into());
        release
            .assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "legacy-windows".into());
        assert!(platform_asset_channel(&release, crate::UpdateChannel::Native).is_none());
        #[cfg(target_os = "linux")]
        {
            release
                .assets
                .insert("kurisu-native".into(), "native-linux".into());
            assert_eq!(
                platform_asset_channel(&release, crate::UpdateChannel::Native),
                Some("kurisu-native")
            );
            assert_eq!(
                platform_asset_channel(&release, crate::UpdateChannel::Legacy),
                Some("kurisu")
            );
        }
        #[cfg(target_os = "windows")]
        {
            release.assets.insert(
                "Kurisu-Native_1.0.0_x64-install.exe".into(),
                "native-windows".into(),
            );
            assert_eq!(
                platform_asset_channel(&release, crate::UpdateChannel::Native),
                Some("Kurisu-Native_1.0.0_x64-install.exe")
            );
            release.assets.insert(
                "Kurisu-Native_1.0.1_x64-install.exe".into(),
                "other-native".into(),
            );
            assert!(platform_asset_channel(&release, crate::UpdateChannel::Native).is_none());
        }
    }
}
