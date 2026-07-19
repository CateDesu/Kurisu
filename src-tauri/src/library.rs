//! M3: library scan. Walks the user's configured folders for video files and runs
//! each basename through the recognizer (`recognize.rs`) against the cached list.
//!
//! Scan results are NOT persisted — a full walk of a typical anime folder takes
//! well under a second, so the Library page just re-scans on demand. Watched state
//! is derived from list progress (`episode <= progress` ⇒ watched), which is
//! retroactively correct for files watched before M3 existed; that's why the older
//! `watched_file` table stays unused.
//!
//! Folder list lives in the `settings` table as a JSON array (`library_folders`).

use anyhow::Result;

use crate::db::Db;
use crate::models::LibraryFile;
use crate::recognize::{basename, match_title, parse_episode_after, parse_episode_guess, Matcher};

const FOLDERS_KEY: &str = "library_folders";
/// Recursion cap — plenty for `Anime/Series/Season 2/file.mkv` layouts, and keeps
/// a symlink-ish loop from running away.
const MAX_DEPTH: usize = 8;
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv",
];

// ─────────────────────────── folder settings ───────────────────────────

pub fn get_folders(db: &Db) -> Vec<String> {
    db.get_setting(FOLDERS_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_folders(db: &Db, folders: &[String]) -> Result<()> {
    db.set_setting(FOLDERS_KEY, &serde_json::to_string(folders)?)
}

pub fn add_folder(db: &Db, path: &str) -> Result<Vec<String>> {
    let mut folders = get_folders(db);
    if !folders.iter().any(|f| f == path) {
        folders.push(path.to_string());
        save_folders(db, &folders)?;
    }
    Ok(folders)
}

pub fn remove_folder(db: &Db, path: &str) -> Result<Vec<String>> {
    let mut folders = get_folders(db);
    folders.retain(|f| f != path);
    save_folders(db, &folders)?;
    Ok(folders)
}

// ─────────────────────────── scan ───────────────────────────

/// Walk the given folders and recognize each video file against `matchers`
/// (pre-built from the cached list by the caller). Blocking — call from
/// `spawn_blocking`. Missing/unreadable folders are skipped (a disconnected drive
/// shouldn't fail the whole scan).
pub fn scan_paths(folders: &[String], matchers: &[Matcher]) -> Vec<LibraryFile> {
    let mut paths = Vec::new();
    for folder in folders {
        collect_videos(std::path::Path::new(folder), 0, &mut paths);
    }
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let base = basename(&path);
            let matched = match_title(matchers, "", &path);
            let episode = matched
                .and_then(|m| parse_episode_after(&base, &m.variants))
                .or_else(|| parse_episode_guess(&base));
            LibraryFile {
                path,
                media_id: matched.map(|m| m.media_id),
                matched: matched.map(|m| m.display.clone()),
                episode,
            }
        })
        .collect()
}

/// Recursively collect video files under `dir`, skipping hidden entries.
fn collect_videos(dir: &std::path::Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // metadata() (not DirEntry::metadata) follows symlinks, so symlinked
        // folders/files get scanned; a symlink loop just bottoms out at MAX_DEPTH.
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            collect_videos(&path, depth + 1, out);
        } else if meta.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false)
        {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}
