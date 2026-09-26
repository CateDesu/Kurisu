use anyhow::{anyhow, Result};

use crate::db::Db;
use crate::models::{LibraryFile, LibraryScan, UnreadableFolder};
use crate::recognize::{basename, match_title, resolve_episode, Matcher};

const FOLDERS_KEY: &str = "library_folders";
const BINDINGS_KEY: &str = "library_bindings";
const MAX_DEPTH: usize = 8;
/// Keep these extensions in sync with the opener scope.
const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv", "mpg", "mpeg", "m2ts",
    "vob", "ogv", "3gp", "rmvb", "asf", "divx",
];
static FOLDERS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
static BINDINGS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub fn get_folders(db: &Db) -> Vec<String> {
    match db.get_setting(FOLDERS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
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
    // Overlapping roots produce duplicate paths in the keyed file list.
    if let Some(existing) = folders.iter().find(|f| folders_overlap(f, path)) {
        return Err(anyhow!(
            "folder overlaps existing library folder: {existing}"
        ));
    }
    folders.push(path.to_string());
    save_folders(db, &folders)?;
    Ok(folders)
}

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

pub fn unbind_media(db: &Db, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.retain(|_, id| *id != media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

pub fn binding_for_exact(db: &Db, path: &str) -> Option<i64> {
    get_bindings(db).get(path).copied()
}

fn bound_live(
    bindings: &std::collections::HashMap<String, i64>,
    path: &str,
    live: impl Fn(i64) -> bool,
) -> Option<i64> {
    if let Some(id) = bindings.get(path) {
        if live(*id) {
            return Some(*id);
        }
    }
    let mut dirs: Vec<_> = bindings
        .iter()
        .filter(|(prefix, _)| {
            path.len() > prefix.len()
                && path.starts_with(prefix.as_str())
                && matches!(path.as_bytes()[prefix.len()], b'/' | b'\\')
        })
        .collect();
    dirs.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
    dirs.into_iter().map(|(_, id)| *id).find(|id| live(*id))
}

/// Call from spawn_blocking.
pub fn scan_paths(
    folders: &[String],
    matchers: &[Matcher],
    bindings: &std::collections::HashMap<String, i64>,
) -> LibraryScan {
    let mut paths = Vec::new();
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
    // Canonical paths deduplicate aliases and roots saved before overlap checks existed.
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
            let bound = bound_live(bindings, &path, |id| {
                matchers.iter().any(|m| m.media_id == id)
            })
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
        // Follow symlinks. MAX_DEPTH bounds cycles.
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
            // Lossy paths cannot be reopened reliably and may collide.
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
    use super::{add_folder, bound_live, folders_overlap, get_folders, scan_paths};
    use crate::db::Db;
    use std::collections::HashMap;

    #[test]
    fn binding_prefix_matching() {
        let mut b = HashMap::new();
        b.insert("/a/Show".to_string(), 1_i64);
        b.insert("/a/Show/Specials".to_string(), 2);
        b.insert("/a/file.mkv".to_string(), 3);
        let live = |_| true;
        assert_eq!(bound_live(&b, "/a/file.mkv", live), Some(3));
        assert_eq!(bound_live(&b, "/a/Show/ep01.mkv", live), Some(1));
        assert_eq!(bound_live(&b, "/a/Show/Specials/sp1.mkv", live), Some(2));
        assert_eq!(bound_live(&b, "/a/Show 2/ep01.mkv", live), None);
        assert_eq!(bound_live(&b, "/a/Show\\ep01.mkv", live), Some(1));
        assert_eq!(bound_live(&b, "/other/x.mkv", live), None);
    }

    #[test]
    fn dead_binding_shadows_nothing() {
        let mut b = HashMap::new();
        b.insert("/a/Show/ep01.mkv".to_string(), 9_i64);
        b.insert("/a/Show".to_string(), 1_i64);
        let live = |id: i64| id != 9;
        assert_eq!(bound_live(&b, "/a/Show/ep01.mkv", live), Some(1));
        assert_eq!(bound_live(&b, "/a/Show/ep01.mkv", |_| false), None);
    }

    #[test]
    fn folder_overlap_detection() {
        assert!(folders_overlap("/anime", "/anime"));
        assert!(folders_overlap("/anime/", "/anime"));
        assert!(folders_overlap("/anime", "/anime/seasonal"));
        assert!(folders_overlap("/anime/seasonal", "/anime"));
        assert!(!folders_overlap("/anime", "/anime2"));
        assert!(!folders_overlap("/anime", "/other"));
    }

    #[test]
    fn add_folder_rejects_overlaps() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        add_folder(&db, "/anime").unwrap();
        add_folder(&db, "/anime").unwrap();
        assert!(add_folder(&db, "/anime/seasonal").is_err());
        assert!(add_folder(&db, "/anime/").is_err());
        assert!(add_folder(&db, "/").is_err());
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
        assert_eq!(scan.files.len(), 1);
        assert!(scan.files[0].path.ends_with("ep01.mkv"));
        assert!(scan.unreadable.is_empty());
    }

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
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.unreadable.len(), 1);
        assert!(scan.unreadable[0].path.ends_with("not-mounted"));
        assert!(!scan.unreadable[0].error.is_empty());
    }

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
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dotted = dir.join("sub").join("..").join("real");
        assert!(add_folder(&db, &dotted.to_string_lossy()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

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
