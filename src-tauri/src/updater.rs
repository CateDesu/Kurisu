//! Self-update over the rolling GitHub release — same shape as NyaaTriggers'
//! updater: fetch `/releases/latest`, compare numeric version tuples, download
//! with a SHA-256 check that fails closed, then hand off per platform:
//!
//! - **Windows**: launch the verified NSIS installer and quit, so it can
//!   overwrite the install.
//! - **Linux**: swap the running binary for the verified one (two adjacent
//!   renames; a running Linux binary can be replaced, unlike Windows) and let
//!   the UI prompt a restart.
//!
//! Anything else reports `can_install: false` and updates by hand from the
//! release page.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

const REPO: &str = "CateDesu/Kurisu";
const USER_AGENT: &str = "Kurisu";

/// Dropped next to the exe when a swap fails so badly the rollback rename also
/// failed (exe missing, only the `.kurisu-old` backup remains). The next
/// launch — possible only after a manual restore — surfaces it and removes it.
pub const FAILED_MARKER: &str = ".kurisu-update-failed";

// ── Version comparison (same semantics as NyaaTriggers' parse_version) ──────

/// `"v1.0.0.8" -> (1, 0, 0, 8)`. Each dot segment contributes only its leading
/// digits; no leading digit means 0. Handles the rolling 4-segment tags.
/// (Comparisons go through `version_key`; this stays for the tests and as a
/// plain release-tuple view.)
#[allow(dead_code)]
pub fn parse_version(s: &str) -> Vec<u64> {
    version_key(s).0
}

/// Comparable version key: release segments, then a prerelease marker that sorts
/// ANY prerelease below the plain release of the same numbers
/// (`1.0.0-rc1 < 1.0.0 < 1.0.0.8`); the prerelease's first digit run breaks
/// rc1/rc2-style ties.
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

/// This build's version: the CI-stamped release version when present
/// (`KURISU_BUILD_VERSION` at build time, incl. the rolling 4th segment), else
/// the crate version. Without the stamp an installed rolling build would keep
/// reporting the X.Y.Z base and re-offer the same update forever.
pub fn current_version() -> &'static str {
    match option_env!("KURISU_BUILD_VERSION") {
        Some(v) if !v.is_empty() => v,
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// True only when this binary was stamped by CI. Unstamped (locally compiled)
/// builds never auto-check on startup: they report the X.Y.Z base version, so
/// the dev loop would get nagged by every newer rolling build — and an
/// accidental install would overwrite the working tree's binary. A manual
/// check from Settings still works on any build.
pub fn is_ci_build() -> bool {
    matches!(option_env!("KURISU_BUILD_VERSION"), Some(v) if !v.is_empty())
}

// ── Release lookup ──────────────────────────────────────────────────────────

/// A GitHub release, reduced to what the updater needs. `assets` maps asset
/// name -> browser download URL.
#[derive(Debug, Clone, Default)]
pub struct Release {
    pub tag: String,
    pub version: String,
    pub html_url: String,
    pub body: String,
    pub assets: HashMap<String, String>,
}

/// Fetch the latest full (non-prerelease, non-draft) release. The rolling
/// workflow prunes superseded rolling releases, so this is always the newest
/// main build — or the newest hand-cut milestone, if one is newer.
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

/// The updatable asset for THIS platform in `rel`: the NSIS installer
/// (`…-setup.exe`) on Windows, the bare `kurisu` binary on Linux. `None`
/// elsewhere. Never matches the `.sha256` sidecars.
pub fn platform_asset(rel: &Release) -> Option<&str> {
    #[cfg(target_os = "windows")]
    {
        rel.assets
            .keys()
            .find(|n| n.ends_with("-setup.exe"))
            .map(String::as_str)
    }
    #[cfg(target_os = "linux")]
    {
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

/// Fetch the `.sha256` sidecar text for an asset, if the release publishes one.
#[cfg(any(windows, target_os = "linux"))]
pub async fn fetch_sidecar(rel: &Release, asset_name: &str) -> Option<String> {
    let url = rel.assets.get(&format!("{asset_name}.sha256"))?;
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .ok()?;
    client.get(url).send().await.ok()?.text().await.ok()
}

// ── Download + integrity ────────────────────────────────────────────────────

/// Hard ceiling on an update download. The real assets are ~20 MB (Linux
/// binary) and ~150 MB (NSIS installer with the WebView2 bootstrapper); this
/// leaves room for an offline-installer future while bounding how much disk a
/// pathological or compromised asset can fill.
#[cfg(any(windows, target_os = "linux"))]
const MAX_DOWNLOAD_BYTES: u64 = 500 * 1024 * 1024;

/// Stream `url` to `dest`, writing to a `.part` and renaming on success so a
/// half-download is never mistaken for complete. Verifies Content-Length (a
/// clean early close is a short read with no error) and refuses anything past
/// `MAX_DOWNLOAD_BYTES`, header-claimed or streamed. `.part` removed on failure.
/// File I/O goes through tokio::fs so the writes stay off the async workers.
#[cfg(any(windows, target_os = "linux"))]
pub async fn download(url: &str, dest: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let part = {
        let mut p = dest.as_os_str().to_os_string();
        p.push(".part");
        PathBuf::from(p)
    };
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    let res: Result<(), String> = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // NSIS + embedded WebView2 bootstrapper is ~150 MB — at ~1 Mbps that
            // is a 20-minute pull, so give slow links 30 minutes before cutting
            // off (the half-download is deleted either way).
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
        drop(file);
        if total != 0 && got < total {
            return Err(format!("download incomplete: {got} of {total} bytes"));
        }
        tokio::fs::rename(&part, dest).await.map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    res
}

/// Whether the `.sha256` sidecar authorizes `path`. `Ok(Some(_))` = digest
/// matches; `Ok(None)` = digest found but does NOT match; `Err(_)` = no
/// readable digest or unreadable file. Callers fail closed on anything but
/// `Ok(Some(_))`.
///
/// The digest is computed from a freshly opened handle and that handle
/// (rewound) is returned on success, so the caller installs / executes THE
/// SAME BYTES it verified — a process that swaps the file between verify and
/// use (TOCTOU) gets nowhere:
///
/// - Linux: the apply step copies FROM this handle, never re-opening the path.
/// - Windows: the handle is opened with read-only sharing, so the file can't be
///   renamed or overwritten while it's held; the caller keeps it open until the
///   installer has been launched from the path.
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

/// Replace the running exe with the verified download: copy FROM the verified
/// handle (never re-open the download path — that keeps the verify→use chain on
/// the same bytes), stage it next to the live exe (same filesystem), then two
/// adjacent renames — live exe aside to `<name>.kurisu-old`, staged file in as
/// the exe. Rolls back if the second rename fails; the backup is swept on the
/// next launch. The caller prompts the user to restart.
#[cfg(target_os = "linux")]
pub fn apply_linux_update(new_bin: &mut std::fs::File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("cannot locate install dir")?;
    let name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("cannot locate install dir")?;
    let staging = dir.join(format!(".kurisu-new-{}", std::process::id()));
    let backup = dir.join(format!("{name}.kurisu-old"));
    let result = (|| -> io::Result<()> {
        let mut staged = std::fs::File::create(&staging)?;
        io::copy(new_bin, &mut staged)?;
        drop(staged);
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&exe, &backup)?;
        if let Err(e) = std::fs::rename(&staging, &exe) {
            // Roll the exe swap back. If even that fails, the install is left
            // with no working exe (only the backup): drop a marker the next
            // launch surfaces, and sweep_install_leftovers keeps the orphaned
            // backup, so a manual restore is always possible.
            if std::fs::rename(&backup, &exe).is_err() {
                let _ = std::fs::write(dir.join(FAILED_MARKER), "");
            }
            return Err(e);
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&staging);
    result.map_err(|e| format!("could not install the update: {e}"))
}

// ── Leftover sweeps ─────────────────────────────────────────────────────────

/// Remove leftover `.kurisu-update-*` downloads in `dir` (a finished or
/// aborted update leaves the download behind). Best-effort, every launch.
/// Files younger than an hour are left alone: without a single-instance guard
/// a second Kurisu could be mid-download (chunk writes keep the mtime fresh)
/// or in the verify gap, and unlinking its file out from under it aborts that
/// update. (The download timeout is 30 minutes, so an hour means certainly dead.)
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

/// Remove update leftovers next to the installed exe: `.kurisu-new-*` staging
/// files from an interrupted swap and `<name>.kurisu-old` backups (a launched
/// build no longer needs its rollback copy — the swap already proved itself
/// by running). A backup whose exe is MISSING is kept: after a doubly-failed
/// swap it's the only working copy, and deleting it would brick the install.
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
        // Prereleases sort BELOW the plain release of the same numbers; digit
        // runs order rc1 < rc2. A rolling 4th segment still beats any rc.
        assert!(is_newer("1.0.0", "1.0.0-rc1"));
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(is_newer("1.0.0-rc2", "1.0.0-rc1"));
        assert!(is_newer("1.0.0.1", "1.0.0-rc9"));
    }

    #[test]
    fn platform_asset_picks_this_platforms_asset() {
        let mut rel = Release::default();
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "u1".into());
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe.sha256".into(), "u2".into());
        rel.assets.insert("kurisu.exe".into(), "u3".into());
        rel.assets.insert("kurisu.sha256".into(), "u4".into());
        // Sidecars and the other platform's assets never match.
        assert_eq!(platform_asset(&rel), None);
        rel.assets.insert("kurisu".into(), "u5".into());
        #[cfg(target_os = "linux")]
        assert_eq!(platform_asset(&rel), Some("kurisu"));
        #[cfg(target_os = "windows")]
        assert_eq!(platform_asset(&rel), Some("Kurisu_1.0.0_x64-setup.exe"));
    }
}
