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

use anyhow::{anyhow, Result};

use crate::db::Db;
use crate::models::{LibraryFile, LibraryScan, UnreadableFolder};
use crate::recognize::{basename, match_title, resolve_episode, Matcher};

const FOLDERS_KEY: &str = "library_folders";
const BINDINGS_KEY: &str = "library_bindings";
/// Recursion cap — plenty for `Anime/Series/Season 2/file.mkv` layouts, and keeps
/// a symlink-ish loop from running away.
const MAX_DEPTH: usize = 8;
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv",
];
/// The folder list and the bindings map are read-modify-write JSON values in the
/// settings table — serialize mutations so two concurrent calls can't lose one.
static FOLDERS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static BINDINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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
    let _guard = FOLDERS_LOCK.lock();
    let mut folders = get_folders(db);
    if folders.iter().any(|f| f == path) {
        return Ok(folders);
    }
    // Overlapping folders would double-scan every shared file (and the Library
    // page keys on path, so duplicates crash its render) — refuse them.
    if let Some(existing) = folders.iter().find(|f| folders_overlap(f, path)) {
        return Err(anyhow!("folder overlaps existing library folder: {existing}"));
    }
    folders.push(path.to_string());
    save_folders(db, &folders)?;
    Ok(folders)
}

/// True when either path is the other or contains it, comparing normalized
/// components: `/anime/` == `/anime`, `/anime/seasonal` nests under `/anime`,
/// but `/anime2` is unrelated to `/anime`.
fn folders_overlap(a: &str, b: &str) -> bool {
    let a = std::path::Path::new(a);
    let b = std::path::Path::new(b);
    a.starts_with(b) || b.starts_with(a)
}

pub fn remove_folder(db: &Db, path: &str) -> Result<Vec<String>> {
    let _guard = FOLDERS_LOCK.lock();
    let mut folders = get_folders(db);
    folders.retain(|f| f != path);
    save_folders(db, &folders)?;
    Ok(folders)
}

// ─────────────────────────── manual bindings ───────────────────────────
//
// A binding maps a file or directory path to a media_id — the user's explicit
// "this IS that show" for files the recognizer can't name. Stored as a JSON
// object (path → media_id) in the settings table, like the folder list.

pub fn get_bindings(db: &Db) -> std::collections::HashMap<String, i64> {
    db.get_setting(BINDINGS_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn bind_path(db: &Db, path: &str, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.insert(path.to_string(), media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

/// Drop every binding pointing at `media_id` (the group-level "unlink" action).
pub fn unbind_media(db: &Db, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.retain(|_, id| *id != media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

/// The media a path is manually bound to, if any: an exact file binding, else
/// the DEEPEST directory binding containing the path (so a nested `Specials/`
/// binding beats its parent's).
fn binding_for(bindings: &std::collections::HashMap<String, i64>, path: &str) -> Option<i64> {
    if let Some(id) = bindings.get(path) {
        return Some(*id);
    }
    bindings
        .iter()
        .filter(|(prefix, _)| {
            path.len() > prefix.len()
                && path.starts_with(prefix.as_str())
                && matches!(path.as_bytes()[prefix.len()], b'/' | b'\\')
        })
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, id)| *id)
}

// ─────────────────────────── scan ───────────────────────────

/// Walk the given folders and recognize each video file against `matchers`
/// (pre-built from the cached list by the caller). A manual binding wins over
/// the recognizer — it's the user's explicit statement. Blocking — call from
/// `spawn_blocking`. Missing/unreadable folders are skipped (a disconnected drive
/// shouldn't fail the whole scan).
pub fn scan_paths(
    folders: &[String],
    matchers: &[Matcher],
    bindings: &std::collections::HashMap<String, i64>,
) -> LibraryScan {
    let mut paths = Vec::new();
    // Roots that could not be read at all. Deeper subdirectories stay
    // best-effort, but a configured root vanishing (unmounted drive, dead
    // network mount, permissions changed) silently produced an empty scan that
    // looked exactly like "you have no files".
    let mut unreadable = Vec::new();
    for folder in folders {
        let root = std::path::Path::new(folder);
        if let Err(e) = std::fs::read_dir(root) {
            unreadable.push(UnreadableFolder { path: folder.clone(), error: e.to_string() });
            continue;
        }
        collect_videos(root, 0, &mut paths);
    }
    // Overlapping folders (added before the overlap check existed) can collect
    // the same file twice — sort+dedup so each path appears exactly once.
    paths.sort();
    paths.dedup();

    let files = paths
        .into_iter()
        .map(|path| {
            let base = basename(&path);
            // A binding to a show no longer on the list has no matcher (no
            // titles to show or parse against) — fall through to the recognizer.
            let bound = binding_for(bindings, &path)
                .and_then(|id| matchers.iter().find(|m| m.media_id == id));
            let matched = bound.or_else(|| match_title(matchers, "", &path));
            let episode = matched.and_then(|m| resolve_episode(m, &[base.as_str()]));
            LibraryFile {
                path,
                media_id: matched.map(|m| m.media_id),
                matched: matched.map(|m| m.display.clone()),
                episode,
                bound: bound.is_some(),
            }
        })
        .collect();
    LibraryScan { files, unreadable }
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

#[cfg(test)]
mod tests {
    use super::{add_folder, binding_for, folders_overlap, get_folders, scan_paths};
    use crate::db::Db;
    use std::collections::HashMap;

    #[test]
    fn binding_prefix_matching() {
        let mut b = HashMap::new();
        b.insert("/a/Show".to_string(), 1_i64);
        b.insert("/a/Show/Specials".to_string(), 2);
        b.insert("/a/file.mkv".to_string(), 3);
        // exact file binding
        assert_eq!(binding_for(&b, "/a/file.mkv"), Some(3));
        // dir binding covers files below it
        assert_eq!(binding_for(&b, "/a/Show/ep01.mkv"), Some(1));
        // deepest dir wins
        assert_eq!(binding_for(&b, "/a/Show/Specials/sp1.mkv"), Some(2));
        // "Show 2" is NOT under the "Show" binding (no separator boundary)
        assert_eq!(binding_for(&b, "/a/Show 2/ep01.mkv"), None);
        // Windows separators count as a boundary too
        assert_eq!(binding_for(&b, "/a/Show\\ep01.mkv"), Some(1));
        assert_eq!(binding_for(&b, "/other/x.mkv"), None);
    }

    #[test]
    fn folder_overlap_detection() {
        assert!(folders_overlap("/anime", "/anime"));
        // trailing slash still the same folder
        assert!(folders_overlap("/anime/", "/anime"));
        assert!(folders_overlap("/anime", "/anime/seasonal"));
        assert!(folders_overlap("/anime/seasonal", "/anime"));
        // shared string prefix without a component boundary is NOT overlap
        assert!(!folders_overlap("/anime", "/anime2"));
        assert!(!folders_overlap("/anime", "/other"));
    }

    #[test]
    fn add_folder_rejects_overlaps() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        add_folder(&db, "/anime").unwrap();
        // exact duplicate stays a silent no-op
        add_folder(&db, "/anime").unwrap();
        // nested either way is rejected, trailing slash included
        assert!(add_folder(&db, "/anime/seasonal").is_err());
        assert!(add_folder(&db, "/anime/").is_err());
        assert!(add_folder(&db, "/").is_err());
        // a sibling that merely shares a string prefix is fine
        add_folder(&db, "/anime2").unwrap();
        assert_eq!(get_folders(&db), vec!["/anime", "/anime2"]);
    }

    #[test]
    fn scan_dedups_overlapping_folders() {
        let dir = std::env::temp_dir().join(format!("kurisu-scan-test-{}", std::process::id()));
        let nested = dir.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("ep01.mkv"), []).unwrap();
        let folders = vec![
            dir.to_string_lossy().into_owned(),
            nested.to_string_lossy().into_owned(),
        ];
        let scan = scan_paths(&folders, &[], &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        // the file under the nested folder must appear exactly once
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("ep01.mkv"));
        assert!(scan.unreadable.is_empty());
    }

    /// A configured root that cannot be read is REPORTED, not silently skipped:
    /// an unmounted drive used to look identical to an empty library.
    #[test]
    fn scan_reports_unreadable_roots() {
        let dir = std::env::temp_dir().join(format!("kurisu-scan-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ep01.mkv"), []).unwrap();
        let gone = dir.join("not-mounted");
        let folders = vec![
            dir.to_string_lossy().into_owned(),
            gone.to_string_lossy().into_owned(),
        ];
        let scan = scan_paths(&folders, &[], &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        // The readable root still scans; the missing one is named.
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.unreadable.len(), 1);
        assert!(scan.unreadable[0].path.ends_with("not-mounted"));
        assert!(!scan.unreadable[0].error.is_empty());
    }
}
