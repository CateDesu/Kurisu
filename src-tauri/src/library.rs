//! M3 library scan. Walks the user's configured folders for video files and
//! runs each basename through the recognizer in recognize.rs against the
//! cached list.
//!
//! Scan results are NOT persisted. A full walk of a typical anime folder
//! takes well under a second, so the Library page just re-scans on demand.
//! Watched state is derived from list progress where episode <= progress
//! means watched. That is retroactively correct for files watched before
//! M3 existed, which is why the older watched_file table stays unused.
//!
//! Folder list lives in the settings table as a JSON array, library_folders.

use anyhow::{anyhow, Result};

use crate::db::Db;
use crate::models::{LibraryFile, LibraryScan, UnreadableFolder};
use crate::recognize::{basename, match_title, resolve_episode, Matcher};

const FOLDERS_KEY: &str = "library_folders";
const BINDINGS_KEY: &str = "library_bindings";
/// Recursion cap. Plenty for Anime/Series/Season 2/file.mkv layouts.
/// Keeps a symlink loop from running away.
const MAX_DEPTH: usize = 8;
/// Every extension the opener scope allows. The two lists must agree: the
/// scanner lowercases before checking, so an uppercase file listed here but
/// missing there gets a Play button the scope refuses, and one allowed by
/// the scope but not scanned never shows up at all.
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv", "mpg", "mpeg", "m2ts",
    "vob", "ogv", "3gp", "rmvb", "asf", "divx",
];
/// The folder list and bindings map are JSON values in the settings table,
/// read then modified then written back. Serialize mutations so two
/// concurrent calls can not lose one.
static FOLDERS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static BINDINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

// ─────────────────────────── folder settings ───────────────────────────

pub fn get_folders(db: &Db) -> Vec<String> {
    match db.get_setting(FOLDERS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            // A corrupt row used to look exactly like "no folders configured".
            // The next add_folder then persisted the empty list, destroying
            // the previous configuration for good. Log so it is discoverable.
            log::warn!("corrupt library_folders setting, starting from empty: {e}");
            Vec::new()
        }),
        None => Vec::new(),
    }
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
    // Overlapping folders would scan every shared file twice. The Library
    // page keys on path, so duplicates crash its render. Refuse them.
    if let Some(existing) = folders.iter().find(|f| folders_overlap(f, path)) {
        return Err(anyhow!(
            "folder overlaps existing library folder: {existing}"
        ));
    }
    folders.push(path.to_string());
    save_folders(db, &folders)?;
    Ok(folders)
}

/// True when either path is the other or contains it, comparing
/// canonicalized components so symlinks and .. segments naming the same
/// directory still count as overlap. Paths that do not resolve fall back
/// to the raw configured string. /anime/ == /anime. /anime/seasonal
/// nests under /anime. /anime2 is unrelated to /anime.
fn folders_overlap(a: &str, b: &str) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| std::path::PathBuf::from(a));
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| std::path::PathBuf::from(b));
    a.starts_with(&b) || b.starts_with(&a)
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
// A binding maps a file or directory path to a media_id. It is the user's
// explicit "this IS that show" for files the recognizer can not name.
// Stored as a JSON object, path to media_id, in the settings table, like
// the folder list.

pub fn get_bindings(db: &Db) -> std::collections::HashMap<String, i64> {
    match db.get_setting(BINDINGS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("corrupt library_bindings setting, starting from empty: {e}");
            std::collections::HashMap::new()
        }),
        None => std::collections::HashMap::new(),
    }
}

pub fn bind_path(db: &Db, path: &str, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.insert(path.to_string(), media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

/// Drop every binding pointing at media_id. The group level unlink action.
pub fn unbind_media(db: &Db, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.retain(|_, id| *id != media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

/// The media a path is manually bound to, if any. An exact file binding,
/// else the DEEPEST directory binding containing the path, so a nested
/// Specials/ binding beats its parent's.
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

/// Walk the given folders and recognize each video file against matchers,
/// built from the cached list by the caller. A manual binding wins over
/// the recognizer, it is the user's explicit statement. Blocking. Call
/// from spawn_blocking. Missing or unreadable folders are skipped. A
/// disconnected drive should not fail the whole scan.
pub fn scan_paths(
    folders: &[String],
    matchers: &[Matcher],
    bindings: &std::collections::HashMap<String, i64>,
) -> LibraryScan {
    let mut paths = Vec::new();
    // Roots that could not be read at all. Deeper subdirectories stay
    // best effort, but a configured root vanishing, unmounted drive, dead
    // network mount, permissions changed, silently produced an empty scan
    // that looked exactly like "you have no files".
    let mut unreadable = Vec::new();
    for folder in folders {
        let root = std::path::Path::new(folder);
        if let Err(e) = std::fs::read_dir(root) {
            unreadable.push(UnreadableFolder {
                path: folder.clone(),
                error: e.to_string(),
            });
            continue;
        }
        collect_videos(root, 0, &mut paths);
    }
    // Overlapping folders added before the overlap check existed, and
    // aliased roots that slipped past it, symlinks or .. segments naming
    // one directory twice, can collect the same file under different
    // strings. Dedup on the canonical path so each real file appears
    // exactly once, then sort for a stable order.
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        seen.insert(key)
    });
    paths.sort();

    let files = paths
        .into_iter()
        .map(|path| {
            let base = basename(&path);
            // A binding to a show no longer on the list has no matcher,
            // no titles to show or parse against. Fall through to the
            // recognizer.
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

/// Recursively collect video files under dir, skipping hidden entries.
fn collect_videos(dir: &std::path::Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // metadata(), not DirEntry::metadata, follows symlinks, so symlinked
        // folders and files get scanned. A symlink loop just bottoms out
        // at MAX_DEPTH.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_videos(&path, depth + 1, out);
        } else if meta.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| VIDEO_EXTS.contains(&e.to_lowercase().as_str()))
                .unwrap_or(false)
        {
            // A name that is not valid UTF-8 would be stored mangled by
            // to_string_lossy. Open and reveal would target a path that
            // does not exist, and two files differing only in the
            // invalid bytes would collapse into one at dedup. Skip the
            // file instead of corrupting the key the rest of the
            // pipeline uses.
            match path.to_str() {
                Some(p) => out.push(p.to_owned()),
                None => log::warn!(
                    "library scan skipping non UTF-8 file: {}",
                    path.to_string_lossy()
                ),
            }
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
        // "Show 2" is NOT under the "Show" binding, no separator boundary
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

    /// A configured root that cannot be read is REPORTED, not silently
    /// skipped. An unmounted drive used to look identical to an empty
    /// library.
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
        // The readable root still scans. The missing one is named.
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.unreadable.len(), 1);
        assert!(scan.unreadable[0].path.ends_with("not-mounted"));
        assert!(!scan.unreadable[0].error.is_empty());
    }

    /// Two roots naming one directory, here a real path and a symlink to
    /// it, must not duplicate every shared file.
    #[cfg(unix)]
    #[test]
    fn scan_dedups_aliased_roots() {
        let dir = std::env::temp_dir().join(format!("kurisu-scan-alias-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("ep01.mkv"), []).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let folders = vec![
            real.to_string_lossy().into_owned(),
            link.to_string_lossy().into_owned(),
        ];
        let scan = scan_paths(&folders, &[], &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("ep01.mkv"));
        assert!(scan.unreadable.is_empty());
    }

    /// The overlap check must see through a symlink alias, not just
    /// compare configured strings.
    #[cfg(unix)]
    #[test]
    fn add_folder_rejects_symlink_alias() {
        let dir = std::env::temp_dir().join(format!("kurisu-add-alias-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        add_folder(&db, &real.to_string_lossy()).unwrap();
        assert!(add_folder(&db, &link.to_string_lossy()).is_err());
        // A .. segment naming the same directory is rejected too. The
        // intermediate directory must exist for canonicalize to resolve
        // through it.
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dotted = dir.join("sub").join("..").join("real");
        assert!(add_folder(&db, &dotted.to_string_lossy()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file whose name is not valid UTF-8 is skipped with a log line,
    /// not stored as a mangled lossy path that open and reveal can
    /// never find.
    #[cfg(unix)]
    #[test]
    fn scan_skips_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("kurisu-scan-nonutf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join(std::ffi::OsStr::from_bytes(b"ep01-\xff.mkv"));
        std::fs::write(&bad, []).unwrap();
        std::fs::write(dir.join("ep02.mkv"), []).unwrap();
        let folders = vec![dir.to_string_lossy().into_owned()];
        let scan = scan_paths(&folders, &[], &HashMap::new());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("ep02.mkv"));
        assert!(scan.unreadable.is_empty());
    }
}
