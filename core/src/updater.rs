use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

const REPO: &str = "CateDesu/Kurisu";
const USER_AGENT: &str = "Kurisu";

/// Taking the saved failure notice attempts to remove this marker.
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

fn install_exe() -> Option<PathBuf> {
    EXE_PATH
        .get()
        .cloned()
        .or_else(|| sane_exe_path(std::env::current_exe().ok()?).ok())
}

fn install_dir() -> Option<PathBuf> {
    install_exe()?.parent().map(Path::to_path_buf)
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

fn compare_version_keys(a: &(Vec<u64>, u8, u64), b: &(Vec<u64>, u8, u64)) -> std::cmp::Ordering {
    let len = a.0.len().max(b.0.len());
    for i in 0..len {
        let lhs = a.0.get(i).copied().unwrap_or(0);
        let rhs = b.0.get(i).copied().unwrap_or(0);
        match lhs.cmp(&rhs) {
            std::cmp::Ordering::Equal => {}
            ordering => return ordering,
        }
    }
    (a.1, a.2).cmp(&(b.1, b.2))
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    compare_version_keys(&version_key(remote), &version_key(current)).is_gt()
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

pub fn platform_asset(rel: &Release) -> Option<&str> {
    if update_applied() {
        return None;
    }
    native_asset(rel)
}

fn native_asset(rel: &Release) -> Option<&str> {
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

pub fn update_info(rel: &Release) -> crate::models::UpdateInfo {
    let can_install = platform_asset(rel).is_some();
    crate::models::UpdateInfo {
        available: is_newer(&rel.version, current_version()) && can_install,
        can_install,
        restart_pending: update_applied(),
        version: rel.version.clone(),
        tag: rel.tag.clone(),
        html_url: rel.html_url.clone(),
        body: rel.body.clone(),
        current: current_version().to_string(),
    }
}

pub async fn watch_updates(
    enabled: impl Fn() -> bool,
    notify: impl FnMut(crate::models::UpdateInfo),
) {
    poll_releases(enabled, fetch_latest_release, notify).await;
}

async fn poll_releases<F, Fut>(
    enabled: impl Fn() -> bool,
    fetch: F,
    notify: impl FnMut(crate::models::UpdateInfo),
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Release, String>>,
{
    poll_releases_with(enabled, fetch, update_info, |info| info.available, notify).await;
}

async fn poll_releases_with<F, Fut, T>(
    enabled: impl Fn() -> bool,
    fetch: F,
    select: impl Fn(&Release) -> T,
    available: impl Fn(&T) -> bool,
    mut notify: impl FnMut(T),
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Release, String>>,
{
    let mut interval = tokio::time::interval(UPDATE_CHECK_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut notified_tag: Option<String> = None;
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
        if !enabled() || update_applied() || notified_tag.as_deref() == Some(rel.tag.as_str()) {
            continue;
        }
        let info = select(&rel);
        if available(&info) {
            notified_tag = Some(rel.tag);
            notify(info);
        }
    }
}

/// Compare versions because publication order can put older builds last.
pub async fn fetch_latest_release() -> Result<Release, String> {
    pick_latest_release(&fetch_release_listing().await?)
        .ok_or_else(|| "no published release for this platform".to_string())
}

const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
const MAX_UPDATE_REDIRECTS: usize = 5;

fn host_ip(parsed: &reqwest::Url) -> Option<std::net::IpAddr> {
    parsed
        .host_str()?
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .ok()
}

fn is_loopback_host(parsed: &reqwest::Url) -> bool {
    parsed.host_str() == Some("localhost") || host_ip(parsed).is_some_and(|ip| ip.is_loopback())
}

fn is_non_public_ip(ip: std::net::IpAddr) -> bool {
    match ip.to_canonical() {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || segments[0..6] == [0; 6]
        }
    }
}

fn is_local_update_target(parsed: &reqwest::Url) -> bool {
    let host = parsed.host_str().unwrap_or("").trim_end_matches('.');
    host == "localhost"
        || host.ends_with(".localhost")
        || host_ip(parsed).is_some_and(is_non_public_ip)
}

fn validate_update_url(parsed: &reqwest::Url, allow_loopback_http: bool) -> Result<(), String> {
    if allow_loopback_http && parsed.scheme() == "http" && is_loopback_host(parsed) {
        return Ok(());
    }
    if parsed.scheme() != "https" {
        return Err("update URL must use https".into());
    }
    if is_local_update_target(parsed) {
        return Err("update URL cannot target a local address".into());
    }
    Ok(())
}

fn validate_update_redirect(
    previous: &[reqwest::Url],
    next: &reqwest::Url,
    allow_loopback_http: bool,
) -> Result<(), String> {
    validate_update_url(next, allow_loopback_http)?;
    if previous.iter().any(|url| url.scheme() == "https") && next.scheme() != "https" {
        return Err("https update cannot redirect to plain http".into());
    }
    if previous.iter().any(|url| !is_local_update_target(url)) && is_local_update_target(next) {
        return Err("remote update cannot redirect to a local address".into());
    }
    Ok(())
}

fn ensure_complete_success(resp: reqwest::Response) -> Result<reqwest::Response, String> {
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        return Err(format!("HTTP {status}"));
    }
    Ok(resp)
}

async fn fetch_release_listing() -> Result<Value, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
    #[cfg(feature = "test-support")]
    let url = fixture_release_endpoint()?.map_or(url, |url| url.to_string());
    fetch_release_listing_from(&url).await
}

async fn fetch_release_listing_from(url: &str) -> Result<Value, String> {
    let mut resp = ensure_complete_success(
        update_request(url, Duration::from_secs(8))?
            .send()
            .await
            .map_err(|e| e.to_string())?,
    )?;
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_LISTING_BYTES as u64)
    {
        return Err(format!(
            "release listing larger than {MAX_LISTING_BYTES} bytes"
        ));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if buf.len().saturating_add(chunk.len()) > MAX_LISTING_BYTES {
            return Err(format!(
                "release listing exceeded {MAX_LISTING_BYTES} bytes"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

fn update_request(url: &str, timeout: Duration) -> Result<reqwest::RequestBuilder, String> {
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout);
    #[cfg(feature = "test-support")]
    if let Some(origin) = fixture_release_endpoint()? {
        validate_fixture_update_url(url, &origin)?;
        let builder = builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none());
        return Ok(builder.build().map_err(|e| e.to_string())?.get(url));
    }
    let allow_loopback_http = cfg!(test);
    let parsed = reqwest::Url::parse(url).map_err(|e| e.to_string())?;
    validate_update_url(&parsed, allow_loopback_http)?;
    let builder = if cfg!(test) {
        builder.no_proxy()
    } else {
        builder
    };
    let builder = builder.redirect(reqwest::redirect::Policy::custom(move |attempt| {
        if let Err(error) =
            validate_update_redirect(attempt.previous(), attempt.url(), allow_loopback_http)
        {
            attempt.error(error)
        } else {
            reqwest::redirect::Policy::limited(MAX_UPDATE_REDIRECTS).redirect(attempt)
        }
    }));
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
    pick_release(data, |rel| native_asset(rel).is_some())
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
        if best_key
            .as_ref()
            .is_none_or(|k| compare_version_keys(&key, k).is_gt())
        {
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
fn native_windows_installer<'a>(
    names: impl Iterator<Item = &'a String>,
    arch: &str,
) -> Option<&'a str> {
    if arch != "x86_64" {
        return None;
    }
    let mut hits = names
        .filter(|name| name.starts_with("Kurisu-Native_") && name.ends_with("_x64-install.exe"));
    match (hits.next(), hits.next()) {
        (Some(name), None) => Some(name.as_str()),
        _ => None,
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub async fn fetch_sidecar(rel: &Release, asset_name: &str) -> Option<String> {
    const MAX_SIDECAR_BYTES: usize = 4096;
    let url = rel.assets.get(&format!("{asset_name}.sha256"))?;
    let resp = update_request(url, Duration::from_secs(15))
        .ok()?
        .send()
        .await
        .ok()?;
    let mut resp = ensure_complete_success(resp).ok()?;
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
                if buf.len().saturating_add(chunk.len()) > MAX_SIDECAR_BYTES {
                    return None;
                }
                buf.extend_from_slice(&chunk);
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
struct StagedPart {
    file: Option<std::fs::File>,
    path: PathBuf,
    armed: bool,
}

#[cfg(any(windows, target_os = "linux"))]
impl StagedPart {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl Drop for StagedPart {
    fn drop(&mut self) {
        // Close the file handle before unlinking so Windows can remove the staging file.
        drop(self.file.take());
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub async fn download(url: &str, dest: &Path) -> Result<(), String> {
    download_with_limit(url, dest, MAX_DOWNLOAD_BYTES).await
}

#[cfg(any(windows, target_os = "linux"))]
async fn download_with_limit(url: &str, dest: &Path, max_bytes: u64) -> Result<(), String> {
    use std::io::Write;
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
    let mut resp = ensure_complete_success(
        update_request(url, Duration::from_secs(1800))?
            .send()
            .await
            .map_err(|e| e.to_string())?,
    )?;
    let total = resp.content_length().unwrap_or(0);
    if total > max_bytes {
        return Err(format!(
            "update download is implausibly large ({total} bytes)"
        ));
    }
    let mut staged = Some(
        tokio::task::spawn_blocking(move || -> io::Result<StagedPart> {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&part)?;
            Ok(StagedPart {
                file: Some(file),
                path: part,
                armed: true,
            })
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?,
    );
    let mut got: u64 = 0;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                if let Some(staged) = staged.take() {
                    let _ = tokio::task::spawn_blocking(move || drop(staged)).await;
                }
                return Err(error.to_string());
            }
        };
        let next = got.saturating_add(chunk.len() as u64);
        if next > max_bytes {
            if let Some(staged) = staged.take() {
                let _ = tokio::task::spawn_blocking(move || drop(staged)).await;
            }
            return Err(format!("update download exceeded {max_bytes} bytes"));
        }
        let mut part = staged.take().expect("staged part active during download");
        let write = tokio::task::spawn_blocking(move || -> Result<StagedPart, String> {
            part.file
                .as_mut()
                .expect("staged file open during download")
                .write_all(&chunk)
                .map_err(|e| e.to_string())?;
            Ok(part)
        })
        .await
        .map_err(|e| e.to_string())?;
        staged = Some(write?);
        got = next;
    }
    if total != 0 && got < total {
        if let Some(staged) = staged.take() {
            let _ = tokio::task::spawn_blocking(move || drop(staged)).await;
        }
        return Err(format!("download incomplete: {got} of {total} bytes"));
    }
    let mut part = staged.take().expect("staged part active for publish");
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut file = part.file.take().expect("staged file open for publish");
        file.flush().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&part.path, &dest).map_err(|e| e.to_string())?;
        part.disarm();
        if let Some(parent) = dest.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
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
    // Another profile can update this installation while this process is running.
    let (running, installed) = std::fs::metadata("/proc/self/exe")
        .and_then(|running| std::fs::metadata(&exe).map(|installed| (running, installed)))
        .map_err(|error| format!("could not identify the installed executable: {error}"))?;
    if !same_linux_file(&running, &installed) {
        UPDATE_APPLIED.store(true, Ordering::SeqCst);
        return Err("the installed executable has changed; restart Kurisu before updating".into());
    }
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
    let result = (|| {
        let install_error = |error| format!("could not install the update: {error}");
        let mut staged = std::fs::File::create_new(&staging).map_err(install_error)?;
        created = true;
        io::copy(new_bin, &mut staged).map_err(install_error)?;
        staged
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(install_error)?;
        // Flush before publishing so a crash cannot leave an empty executable.
        staged.sync_all().map_err(install_error)?;
        drop(staged);
        preserve_executable(&exe, &installed, &backup)
            .map_err(|error| format!("could not secure the rollback executable: {error}"))?;
        std::fs::rename(&staging, &exe).map_err(install_error)?;
        created = false;
        UPDATE_APPLIED.store(true, Ordering::SeqCst);
        sync_dir(dir).map_err(|error| {
            format!(
                "the executable was replaced, but the installation could not be synced: {error}. The previous executable is saved at {}. Restart Kurisu before checking for updates again.",
                backup.display()
            )
        })
    })();
    if created {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

#[cfg(target_os = "linux")]
fn preserve_executable(exe: &Path, installed: &std::fs::Metadata, backup: &Path) -> io::Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| io::Error::other("no install directory"))?;
    match std::fs::symlink_metadata(backup) {
        Ok(saved) => {
            // The current executable must be durable before replacing an older rollback copy.
            sync_dir(dir)?;
            if same_linux_file(installed, &saved) {
                return Ok(());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
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
        created = false;
        sync_dir(dir)
    })();
    if created {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(target_os = "linux")]
fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
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
            }
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(exe) = install_exe().filter(|exe| exe.parent() == Some(exe_dir)) {
        let _ = sweep_install_backup(&exe);
    }
}

#[cfg(target_os = "linux")]
fn same_linux_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (left.dev(), left.ino()) == (right.dev(), right.ino())
}

#[cfg(target_os = "linux")]
fn sweep_install_backup(exe: &Path) -> io::Result<()> {
    use std::io::Read;

    let Some(name) = exe.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let backup = exe.with_file_name(format!("{name}.kurisu-old"));
    let saved = std::fs::symlink_metadata(&backup)?;
    let installed = std::fs::symlink_metadata(exe)?;
    let running = std::fs::metadata("/proc/self/exe")?;
    if !saved.is_file()
        || !same_linux_file(&running, &installed)
        || same_linux_file(&running, &saved)
    {
        return Ok(());
    }
    // A previous process may have published the replacement without syncing it.
    let dir = backup
        .parent()
        .ok_or_else(|| io::Error::other("no install directory"))?;
    sync_dir(dir)?;
    if running.len() != saved.len() {
        return std::fs::remove_file(backup);
    }

    // An interrupted install may have copied the running executable as its backup.
    let mut old = std::fs::File::open(&backup)?;
    let mut current = std::fs::File::open("/proc/self/exe")?;
    let mut old_bytes = [0; 8192];
    let mut current_bytes = [0; 8192];
    let mut remaining = running.len();
    while remaining > 0 {
        let count = remaining.min(old_bytes.len() as u64) as usize;
        old.read_exact(&mut old_bytes[..count])?;
        current.read_exact(&mut current_bytes[..count])?;
        if old_bytes[..count] != current_bytes[..count] {
            return std::fs::remove_file(backup);
        }
        remaining -= count as u64;
    }
    Ok(())
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

    #[cfg(any(windows, target_os = "linux"))]
    struct TempDir(PathBuf);

    #[cfg(any(windows, target_os = "linux"))]
    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                scratch_suffix()
            ));
            let dir = Self(path);
            std::fs::create_dir_all(&dir.0).unwrap();
            dir
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

        let other_update = dir.join("other-update");
        std::fs::write(&other_update, b"already installed by the other profile").unwrap();
        std::fs::hard_link(&exe, &backup).unwrap();
        std::fs::rename(other_update, &exe).unwrap();
        let installed = std::fs::metadata(&exe).unwrap();
        let error = apply_linux_update(&mut std::fs::File::open(&payload).unwrap()).unwrap_err();
        assert!(error.contains("restart Kurisu"), "{error}");
        assert_eq!(std::fs::metadata(&exe).unwrap().ino(), installed.ino());
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"already installed by the other profile"
        );
        assert_eq!(std::fs::metadata(&backup).unwrap().ino(), original.ino());
        assert!(update_applied());
        std::fs::rename(&backup, &exe).unwrap();
        UPDATE_APPLIED.store(false, Ordering::SeqCst);

        std::fs::hard_link(&exe, &backup).unwrap();
        apply_linux_update(&mut std::fs::File::open(&payload).unwrap()).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), replacement);
        assert_eq!(digest_file(&backup), original_digest);
        assert!(update_applied());
        assert!(std::fs::read_dir(dir).unwrap().flatten().all(|entry| !entry
            .file_name()
            .to_string_lossy()
            .starts_with(".kurisu-new-")));
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

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn cancelled_download_removes_partial_staging_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
        let (_hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                req.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\npartial-chunk")
                .await
                .unwrap();
            let _ = sent_tx.send(());
            let _ = hold_rx.await;
        });

        let dir = TempDir::new("kurisu-download-cancel");
        let target = dir.0.join("payload.bin");
        std::fs::write(&target, b"previous download").unwrap();
        let url = format!("http://{address}/update");
        let task = tokio::spawn({
            let target = target.clone();
            async move { download(&url, &target).await }
        });
        sent_rx.await.unwrap();
        // Read through an open handle because NTFS directory metadata lags behind writes until close.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !std::fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry.file_name() != "payload.bin"
                    && std::fs::read(entry.path()).is_ok_and(|bytes| bytes.len() >= 13)
            })
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "staging file was not written"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(_hold_tx);
        let _ = server.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while std::fs::read_dir(&dir.0).unwrap().count() > 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "staging file was not removed after cancellation"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
        let entries: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["payload.bin".to_string()]);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn staged_part_drop_closes_and_removes_only_when_armed() {
        let dir = TempDir::new("kurisu-staged-part");
        let armed_path = dir.0.join("armed.part");
        let armed_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&armed_path)
            .unwrap();
        drop(StagedPart {
            file: Some(armed_file),
            path: armed_path.clone(),
            armed: true,
        });
        assert!(!armed_path.exists());

        let kept_path = dir.0.join("kept.part");
        let kept_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&kept_path)
            .unwrap();
        drop(StagedPart {
            file: Some(kept_file),
            path: kept_path.clone(),
            armed: false,
        });
        assert!(kept_path.exists());
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn download_midstream_size_and_incomplete_errors_remove_staging_file() {
        let dir = TempDir::new("kurisu-download-midstream");
        let target = dir.0.join("payload.bin");
        std::fs::write(&target, b"previous download").unwrap();

        let oversize = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n6\r\nghijkl\r\n0\r\n\r\n".to_vec();
        let (url, server) = serve_download(oversize).await;
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            download_with_limit(&url, &target, 8),
        )
        .await
        .unwrap()
        .unwrap_err();
        let _ = server.await;
        assert!(err.contains("exceeded"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);

        let incomplete =
            b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\n\r\n0123456789"
                .to_vec();
        let (url, server) = serve_download(incomplete).await;
        let err = tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
            .await
            .unwrap()
            .unwrap_err();
        server.await.unwrap();
        assert!(
            err.contains("incomplete") || err.contains("decoding response body"),
            "{err}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn no_content_responses_are_rejected_across_updater_endpoints() {
        let response = b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec();
        let dir = TempDir::new("kurisu-download-204");
        let target = dir.0.join("payload.bin");
        std::fs::write(&target, b"previous download").unwrap();

        let (url, server) = serve_download(response.clone()).await;
        let err = tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
            .await
            .unwrap()
            .unwrap_err();
        server.await.unwrap();
        assert!(err.contains("204"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        drop(dir);

        let (sidecar_url, server) = serve_download(response.clone()).await;
        let mut rel = Release::default();
        rel.assets.insert("kurisu.sha256".into(), sidecar_url);
        let sidecar = fetch_sidecar(&rel, "kurisu").await;
        server.await.unwrap();
        assert!(sidecar.is_none());

        let (listing_url, server) = serve_download(response).await;
        let err = fetch_release_listing_from(&listing_url).await.unwrap_err();
        server.await.unwrap();
        assert!(err.contains("204"), "{err}");
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn live_update_requests_enforce_redirect_limit_and_disallowed_targets() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    while !req.ends_with(b"\r\n\r\n") {
                        let Ok(byte) = socket.read_u8().await else {
                            return;
                        };
                        req.push(byte);
                    }
                    let line = String::from_utf8_lossy(&req);
                    let path = line
                        .lines()
                        .next()
                        .and_then(|first| first.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let response = if let Some(rest) = path.strip_prefix("/limit/") {
                        let hop: usize = rest.parse().unwrap_or(0);
                        if hop < 5 {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: http://{address}/limit/{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                hop + 1
                            )
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
                                .into()
                        }
                    } else if let Some(rest) = path.strip_prefix("/too-many/") {
                        let hop: usize = rest.parse().unwrap_or(0);
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://{address}/too-many/{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            hop + 1
                        )
                    } else if path == "/bad-http" {
                        "HTTP/1.1 302 Found\r\nLocation: http://example.invalid/update\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    } else {
                        "HTTP/1.1 302 Found\r\nLocation: https://192.168.1.1/update\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into()
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        let dir = TempDir::new("kurisu-download-redirects");
        let target = dir.0.join("payload.bin");
        std::fs::write(&target, b"previous download").unwrap();

        let ok_url = format!("http://{address}/limit/0");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), fetch_release_listing_from(&ok_url))
                .await
                .unwrap()
                .unwrap(),
            serde_json::json!([])
        );
        tokio::time::timeout(Duration::from_secs(5), download(&ok_url, &target))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"[]");

        std::fs::write(&target, b"previous download").unwrap();
        for path in ["/too-many/0", "/bad-http", "/bad-lan"] {
            let url = format!("http://{address}{path}");
            assert!(
                tokio::time::timeout(Duration::from_secs(5), fetch_release_listing_from(&url))
                    .await
                    .unwrap()
                    .is_err(),
                "{path}: listing should fail"
            );
            assert!(
                tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
                    .await
                    .unwrap()
                    .is_err(),
                "{path}: download should fail"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"previous download");
            assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        }

        server.abort();
        let _ = server.await;
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn partial_content_responses_are_rejected_by_download_and_sidecar() {
        let dir = TempDir::new("kurisu-download-206");
        let target = dir.0.join("payload.bin");
        let response = b"HTTP/1.1 206 Partial Content\r\nContent-Length: 7\r\nConnection: close\r\n\r\npayload".to_vec();
        let (url, server) = serve_download(response.clone()).await;
        let err = tokio::time::timeout(Duration::from_secs(5), download(&url, &target))
            .await
            .unwrap()
            .unwrap_err();
        server.await.unwrap();
        assert!(err.contains("206"), "{err}");
        assert!(!target.exists());
        drop(dir);

        let (sidecar_url, server) = serve_download(response).await;
        let mut rel = Release::default();
        rel.assets.insert("kurisu.sha256".into(), sidecar_url);
        let sidecar = fetch_sidecar(&rel, "kurisu").await;
        server.await.unwrap();
        assert!(sidecar.is_none());
    }

    #[test]
    fn update_url_and_redirect_policy_blocks_downgrades_and_local_targets() {
        let remote = reqwest::Url::parse("https://github.com/CateDesu/Kurisu/releases").unwrap();
        let cdn = reqwest::Url::parse("https://objects.githubusercontent.com/asset").unwrap();
        assert!(validate_update_url(&remote, false).is_ok());
        assert!(validate_update_redirect(std::slice::from_ref(&remote), &cdn, false).is_ok());
        for allowed in [
            "https://1.1.1.1/update",
            "https://100.128.0.1/update",
            "https://[2606:4700:4700::1111]/update",
        ] {
            let parsed = reqwest::Url::parse(allowed).unwrap();
            assert!(
                validate_update_url(&parsed, false).is_ok(),
                "expected public URL acceptance for {allowed}"
            );
            assert!(
                validate_update_redirect(std::slice::from_ref(&remote), &parsed, false).is_ok(),
                "expected public redirect acceptance for {allowed}"
            );
        }

        for blocked in [
            "http://github.com/CateDesu/Kurisu/releases",
            "http://127.0.0.1:8080/update",
            "https://localhost/update",
            "https://localhost./update",
            "https://sub.localhost/update",
            "https://127.0.0.1/update",
            "https://[::1]/update",
            "https://[::ffff:127.0.0.1]/update",
            "https://0.0.0.0/update",
            "https://0.1.2.3/update",
            "https://10.0.0.1/update",
            "https://100.64.0.1/update",
            "https://100.127.255.254/update",
            "https://169.254.169.254/update",
            "https://172.16.0.1/update",
            "https://192.0.2.1/update",
            "https://192.168.1.1/update",
            "https://198.51.100.1/update",
            "https://203.0.113.1/update",
            "https://224.0.0.1/update",
            "https://240.0.0.1/update",
            "https://255.255.255.255/update",
            "https://[::]/update",
            "https://[::10.0.0.1]/update",
            "https://[::ffff:192.168.1.1]/update",
            "https://[::ffff:169.254.169.254]/update",
            "https://[fc00::1]/update",
            "https://[fd12::1]/update",
            "https://[fe80::1]/update",
            "https://[ff02::1]/update",
            "file:///tmp/kurisu",
        ] {
            let parsed = reqwest::Url::parse(blocked).unwrap();
            assert!(
                validate_update_url(&parsed, false).is_err(),
                "expected URL rejection for {blocked}"
            );
            assert!(
                validate_update_redirect(std::slice::from_ref(&remote), &parsed, false).is_err(),
                "expected redirect rejection for {blocked}"
            );
        }
        // Even with test loopback HTTP enabled, HTTPS must never downgrade or redirect locally.
        for blocked in [
            "http://127.0.0.1:8080/update",
            "http://localhost/update",
            "https://127.0.0.1/update",
        ] {
            let parsed = reqwest::Url::parse(blocked).unwrap();
            assert!(
                validate_update_redirect(std::slice::from_ref(&remote), &parsed, true).is_err(),
                "expected downgrade/local redirect rejection for {blocked}"
            );
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[tokio::test]
    async fn release_listing_rejects_oversized_and_partial_responses() {
        let partial =
            b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
                .to_vec();
        let (url, server) = serve_download(partial).await;
        let err = fetch_release_listing_from(&url).await.unwrap_err();
        server.await.unwrap();
        assert!(err.contains("206"), "{err}");

        let oversized_header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n[]",
            MAX_LISTING_BYTES + 1
        )
        .into_bytes();
        let (url, server) = serve_download(oversized_header).await;
        let err = fetch_release_listing_from(&url).await.unwrap_err();
        let _ = server.await;
        assert!(err.contains("larger than"), "{err}");

        let payload = vec![b' '; MAX_LISTING_BYTES + 16];
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            payload.len()
        )
        .into_bytes();
        chunked.extend_from_slice(&payload);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = serve_download(chunked).await;
        let err = fetch_release_listing_from(&url).await.unwrap_err();
        let _ = server.await;
        assert!(err.contains("exceeded"), "{err}");
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
        assert!(!is_newer("1.1.0.0", "1.1.0"));
        assert!(!is_newer("1.1.0", "1.1.0.0"));
        assert!(is_newer("1.1.0.1", "1.1.0"));
        assert!(!is_newer("1.1.0", "1.1.0.1"));
        assert!(is_newer("1.1.0", "1.1.0.0-rc1"));
        assert!(!is_newer("1.1.0.0-rc1", "1.1.0"));
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
                        let mut assets = HashMap::new();
                        assets.insert(
                            "kurisu-native".into(),
                            "https://example.invalid/linux".into(),
                        );
                        assets.insert(
                            format!("Kurisu-Native_{version}_x64-install.exe"),
                            "https://example.invalid/windows".into(),
                        );
                        Ok(Release {
                            tag: format!("v{version}"),
                            version,
                            body: "Release notes".into(),
                            assets,
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
        assert_eq!(notice.version, "9999.0.0");
        assert_eq!(notice.body, "Release notes");
        assert!(notice.available);
        assert!(notice.can_install);

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
        assert_eq!(rx.try_recv().unwrap().version, "9999.0.1");
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
                        let mut assets = HashMap::new();
                        assets.insert(
                            "kurisu-native".into(),
                            "https://example.invalid/linux".into(),
                        );
                        assets.insert(
                            "Kurisu-Native_9999.0.0_x64-install.exe".into(),
                            "https://example.invalid/windows".into(),
                        );
                        Ok(Release {
                            tag: "v9999.0.0".into(),
                            version: "9999.0.0".into(),
                            assets,
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
        let rel = pick_release(&listing, |_| true).expect("a winner");
        assert_eq!(rel.version, "1.1.0");
        assert_eq!(rel.tag, "v1.1.0");
        let listing = serde_json::json!([
            { "tag_name": "v1.1.0", "assets": [] },
            { "tag_name": "v1.1.0.2", "assets": [] }
        ]);
        let rel = pick_release(&listing, |_| true).expect("a winner");
        assert_eq!(rel.version, "1.1.0.2");
        assert!(pick_release(
            &serde_json::json!([{ "tag_name": "v9.9.9", "draft": true }]),
            |_| true,
        )
        .is_none());
        assert!(pick_release(&serde_json::json!([]), |_| true).is_none());
    }

    #[test]
    fn platform_asset_picks_this_platforms_asset() {
        let mut rel = Release::default();
        rel.assets.insert(
            "Kurisu-Native_1.0.0_x64-install.exe.sha256".into(),
            "u2".into(),
        );
        rel.assets.insert("kurisu-native.exe".into(), "u3".into());
        rel.assets
            .insert("kurisu-native.sha256".into(), "u4".into());
        assert_eq!(platform_asset(&rel), None);
        rel.assets
            .insert("Kurisu-Native_1.0.0_x64-install.exe".into(), "u1".into());
        rel.assets.insert("kurisu-native".into(), "u5".into());
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(platform_asset(&rel), Some("kurisu-native"));
        #[cfg(all(target_os = "linux", not(target_arch = "x86_64")))]
        assert_eq!(platform_asset(&rel), None);
        #[cfg(target_os = "windows")]
        assert_eq!(
            platform_asset(&rel),
            Some("Kurisu-Native_1.0.0_x64-install.exe")
        );
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

    #[cfg(windows)]
    #[test]
    fn verify_and_open_drop_allows_unlinking_on_windows() {
        let dir = TempDir::new("kurisu-verify-drop");
        let path = dir.0.join("installer.exe");
        std::fs::write(&path, b"kurisu installer bytes").unwrap();
        let digest = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(b"kurisu installer bytes"))
        };
        let verified = verify_and_open(&path, &digest).unwrap().unwrap();
        assert!(std::fs::remove_file(&path).is_err());
        drop(verified);
        assert!(std::fs::remove_file(&path).is_ok());
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
    fn native_windows_installer_requires_unique_x64_install_asset() {
        let names = [
            "Kurisu-Native_1.0.0_x64-setup.exe".to_string(),
            "Kurisu-Native_1.1.0_x64-install.exe".to_string(),
            "Kurisu-Native_1.1.0_x64-install.exe.sha256".to_string(),
        ];
        assert_eq!(
            native_windows_installer(names.iter(), "x86_64"),
            Some(names[1].as_str())
        );
        assert_eq!(native_windows_installer(names.iter(), "aarch64"), None);
        assert_eq!(
            native_windows_installer([&names[0]].into_iter(), "x86_64"),
            None
        );
        let mut names = names.to_vec();
        names.push("Kurisu-Native_1.1.1_x64-install.exe".into());
        assert_eq!(native_windows_installer(names.iter(), "x86_64"), None);
    }

    #[test]
    fn native_release_selection_ignores_incomplete_and_unpublished_versions() {
        let listing = serde_json::json!([
            {"tag_name": "v9.0.0", "assets": [{"name": "kurisu", "browser_download_url": "other"}]},
            {"tag_name": "v8.0.0", "draft": true, "assets": [
                {"name": "kurisu-native", "browser_download_url": "draft"},
                {"name": "Kurisu-Native_8.0.0_x64-install.exe", "browser_download_url": "draft"}
            ]},
            {"tag_name": "v1.1.0", "assets": [
                {"name": "kurisu-native", "browser_download_url": "native"},
                {"name": "Kurisu-Native_1.1.0_x64-install.exe", "browser_download_url": "native"}
            ]}
        ]);
        let release = pick_latest_release(&listing).unwrap();
        assert_eq!(release.version, "1.1.0");
    }

    #[test]
    fn platform_asset_ignores_non_native_artifacts() {
        let mut release = Release::default();
        release.assets.insert("kurisu".into(), "other-linux".into());
        release
            .assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "other-windows".into());
        assert!(platform_asset(&release).is_none());
        #[cfg(target_os = "linux")]
        {
            release
                .assets
                .insert("kurisu-native".into(), "native-linux".into());
            assert_eq!(platform_asset(&release), Some("kurisu-native"));
        }
        #[cfg(target_os = "windows")]
        {
            release.assets.insert(
                "Kurisu-Native_1.0.0_x64-install.exe".into(),
                "native-windows".into(),
            );
            assert_eq!(
                platform_asset(&release),
                Some("Kurisu-Native_1.0.0_x64-install.exe")
            );
            release.assets.insert(
                "Kurisu-Native_1.0.1_x64-install.exe".into(),
                "other-native".into(),
            );
            assert!(platform_asset(&release).is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_does_not_announce_same_version_release() {
        let mut release = Release {
            tag: format!("v{}", current_version()),
            version: current_version().to_string(),
            ..Release::default()
        };
        release.assets.insert(
            "kurisu-native".into(),
            "https://example.invalid/native".into(),
        );
        release.assets.insert(
            format!("Kurisu-Native_{}_x64-install.exe", current_version()),
            "https://example.invalid/native-win".into(),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(poll_releases(
            || true,
            move || {
                let release = release.clone();
                async move { Ok(release) }
            },
            move |info| tx.send(info).unwrap(),
        ));
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "watcher must not announce a same-version release"
        );
        task.abort();
    }
}
