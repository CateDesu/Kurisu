//! Self-update over the rolling GitHub release — same shape as NyaaTriggers'
//! updater: fetch `/releases/latest`, compare numeric version tuples, download
//! with a SHA-256 check that fails closed, then hand off. Kurisu ships an NSIS
//! installer on Windows, so the hand-off is "launch the verified installer and
//! quit", not a staged file swap. Only Windows builds can self-update: CI
//! publishes no Linux/macOS asset, so `install_update` is a Windows-only
//! command and `check_update` reports `can_install: false` elsewhere.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

const REPO: &str = "CateDesu/Kurisu";
const USER_AGENT: &str = "Kurisu";

// ── Version comparison (same semantics as NyaaTriggers' parse_version) ──────

/// `"v1.0.0.8" -> (1, 0, 0, 8)`. Each dot segment contributes only its leading
/// digits; no leading digit means 0. Handles the rolling 4-segment tags.
pub fn parse_version(s: &str) -> Vec<u64> {
    let trimmed = s.trim().trim_start_matches(['v', 'V']);
    let out: Vec<u64> = trimmed
        .split('.')
        .map(|seg| {
            let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect();
    if out.is_empty() {
        vec![0]
    } else {
        out
    }
}

/// True if `remote` is strictly newer than `current`.
pub fn is_newer(remote: &str, current: &str) -> bool {
    parse_version(remote) > parse_version(current)
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

/// The NSIS installer asset name in `rel` (`…-setup.exe`), if it ships one.
/// The `.sha256` sidecar ends in `-setup.exe.sha256`, so it can't match.
pub fn installer_asset(rel: &Release) -> Option<&str> {
    rel.assets
        .keys()
        .find(|n| n.ends_with("-setup.exe"))
        .map(String::as_str)
}

/// Fetch the `.sha256` sidecar text for an asset, if the release publishes one.
/// (Only the Windows install path calls this today.)
#[cfg_attr(not(windows), allow(dead_code))]
pub async fn fetch_sidecar(rel: &Release, asset_name: &str) -> Option<String> {
    let url = rel.assets.get(&format!("{asset_name}.sha256"))?;
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .ok()?;
    client.get(url).send().await.ok()?.text().await.ok()
}

// ── Download + integrity ────────────────────────────────────────────────────

/// Stream `url` to `dest`, writing to a `.part` and renaming on success so a
/// half-download is never mistaken for complete. Verifies Content-Length (a
/// clean early close is a short read with no error). `.part` removed on failure.
/// (Only the Windows install path calls this today.)
#[cfg_attr(not(windows), allow(dead_code))]
pub async fn download(url: &str, dest: &Path) -> Result<(), String> {
    use std::io::Write;
    let part = {
        let mut p = dest.as_os_str().to_os_string();
        p.push(".part");
        PathBuf::from(p)
    };
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let res: Result<(), String> = async {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(600))
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
        let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut got: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            got += chunk.len() as u64;
        }
        if total != 0 && got < total {
            return Err(format!("download incomplete: {got} of {total} bytes"));
        }
        std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
        Ok(())
    }
    .await;
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

/// Whether the `.sha256` sidecar text authorizes `file`. `Ok(true)` = digest
/// matches; `Ok(false)` = digest found but does NOT match; `Err(_)` = no
/// readable digest or unreadable file. Callers fail closed on anything but
/// `Ok(true)`. (Only the Windows install path calls this today.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn verify_sha256(file: &Path, sidecar_text: &str) -> io::Result<bool> {
    use sha2::{Digest, Sha256};
    let expected = sidecar_text
        .split_whitespace()
        .find(|tok| tok.len() == 64 && tok.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no sha256 digest in sidecar"))?
        .to_ascii_lowercase();
    let mut f = std::fs::File::open(file)?;
    let mut hasher = Sha256::new();
    io::copy(&mut f, &mut hasher)?;
    let got = hasher.finalize();
    let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    Ok(got_hex == expected)
}

/// Remove leftover `.kurisu-update-*` installer downloads in `dir` (a finished
/// or aborted update leaves the installer behind). Best-effort, every launch.
/// (Only the Windows startup task calls this today.)
#[cfg_attr(not(windows), allow(dead_code))]
pub fn sweep_update_leftovers(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".kurisu-update-")
            {
                let _ = std::fs::remove_file(entry.path());
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
    }

    #[test]
    fn installer_asset_picks_nsis_not_sidecar() {
        let mut rel = Release::default();
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe.sha256".into(), "u1".into());
        assert_eq!(installer_asset(&rel), None);
        rel.assets
            .insert("Kurisu_1.0.0_x64-setup.exe".into(), "u2".into());
        rel.assets.insert("kurisu.exe".into(), "u3".into());
        assert_eq!(installer_asset(&rel), Some("Kurisu_1.0.0_x64-setup.exe"));
    }
}
