use anyhow::{anyhow, Result};

use crate::db::Db;
use crate::models::{LibraryFile, LibraryScan, UnreadableFolder};
use crate::recognize::{basename, match_title, resolve_episode, Matcher, VIDEO_EXTS};

const FOLDERS_KEY: &str = "library_folders";
const BINDINGS_KEY: &str = "library_bindings";
const MAX_DEPTH: usize = 8;
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
    let resolved = resolved_path(path);
    bindings.retain(|saved, _| resolved_path(saved) != resolved);
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
    let resolved = resolved_path(path);
    let local = local_path(path);
    binding_paths(&get_bindings(db))
        .into_iter()
        .filter(|(_, saved, _)| *saved == resolved)
        .max_by_key(|(saved, _, _)| (local_path(saved) == local, std::cmp::Reverse(saved.clone())))
        .map(|(_, _, id)| id)
}

type BindingPath = (String, std::path::PathBuf, i64);

fn resolved_path(path: &str) -> std::path::PathBuf {
    let path = local_path(path);
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn local_path(path: &str) -> std::path::PathBuf {
    reqwest::Url::parse(path)
        .ok()
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok())
        .unwrap_or_else(|| std::path::PathBuf::from(path))
}

fn binding_paths(bindings: &std::collections::HashMap<String, i64>) -> Vec<BindingPath> {
    bindings
        .iter()
        .map(|(path, id)| (path.clone(), resolved_path(path), *id))
        .collect()
}

#[cfg(test)]
fn bound_live(
    bindings: &std::collections::HashMap<String, i64>,
    path: &str,
    live: impl Fn(i64) -> bool,
) -> Option<i64> {
    bound_path_id(&binding_paths(bindings), path, live)
}

fn bound_path_id(bindings: &[BindingPath], path: &str, live: impl Fn(i64) -> bool) -> Option<i64> {
    let resolved = resolved_path(path);
    let local = local_path(path);
    bindings
        .iter()
        .filter(|(_, prefix, id)| resolved.starts_with(prefix) && live(*id))
        .max_by_key(|(source, prefix, _)| {
            (
                prefix.components().count(),
                local.starts_with(local_path(source)),
                std::cmp::Reverse(source.as_str()),
            )
        })
        .map(|(_, _, id)| *id)
}

pub(crate) fn bound_match<'a>(
    matchers: &'a [Matcher],
    bindings: &std::collections::HashMap<String, i64>,
    path: &str,
) -> Option<&'a Matcher> {
    match_bound_path(matchers, &binding_paths(bindings), path)
}

fn match_bound_path<'a>(
    matchers: &'a [Matcher],
    bindings: &[BindingPath],
    path: &str,
) -> Option<&'a Matcher> {
    bound_path_id(bindings, path, |id| {
        matchers.iter().any(|m| m.media_id == id)
    })
    .and_then(|id| matchers.iter().find(|m| m.media_id == id))
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
    let bindings = binding_paths(bindings);

    let files = paths
        .into_iter()
        .map(|path| {
            let base = basename(&path);
            let bound = match_bound_path(matchers, &bindings, &path);
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
    let mut pending = std::collections::VecDeque::from([(dir.to_path_buf(), depth)]);
    let mut visited = std::collections::HashSet::new();
    while let Some((dir, depth)) = pending.pop_front() {
        if depth > MAX_DEPTH {
            continue;
        }
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries = read.flatten().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                pending.push_back((path, depth + 1));
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
        #[cfg(windows)]
        assert_eq!(bound_live(&b, "/a/Show\\ep01.mkv", live), Some(1));
        #[cfg(unix)]
        assert_eq!(bound_live(&b, "/a/Show\\ep01.mkv", live), None);
        assert_eq!(bound_live(&b, "/other/x.mkv", live), None);
    }

    #[test]
    fn playback_bindings_accept_file_urls_and_plain_paths() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let matchers = crate::recognize::build_matchers(&db);
        let folder = std::env::temp_dir().join("kurisu-bound-show");
        let file = folder.join("ep05.mkv");
        let url = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let bindings = HashMap::from([(folder.to_str().unwrap().to_string(), 1)]);
        for path in [file.to_str().unwrap(), url.as_str()] {
            let matched = super::bound_match(&matchers, &bindings, path).unwrap();
            assert_eq!(matched.media_id, 1);
            assert_eq!(
                crate::recognize::resolve_episode(matched, &[&crate::recognize::basename(path)]),
                Some(5)
            );
        }
        assert!(super::bound_match(&[], &bindings, &url).is_none());
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
    fn bindings_follow_directory_aliases() {
        let dir = std::env::temp_dir().join(format!("kurisu-binding-alias-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("ep05.mkv"), []).unwrap();
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let bindings = HashMap::from([(real.to_str().unwrap().to_string(), 1)]);
        let found = bound_live(&bindings, alias.join("ep05.mkv").to_str().unwrap(), |_| {
            true
        });
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_bindings_keep_file_precedence_and_replace_aliases() {
        let dir =
            std::env::temp_dir().join(format!("kurisu-binding-replace-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let file = real.join("ep05.mkv");
        std::fs::write(&file, []).unwrap();
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let alias_file = alias.join("ep05.mkv");
        let text = |path: &std::path::Path| path.to_str().unwrap().to_string();
        let bindings = HashMap::from([(text(&alias), 1), (text(&file), 2)]);
        assert_eq!(bound_live(&bindings, &text(&alias_file), |_| true), Some(2));

        let legacy = HashMap::from([(text(&file), 2), (text(&alias_file), 3)]);
        assert_eq!(bound_live(&legacy, &text(&file), |_| true), Some(2));
        assert_eq!(bound_live(&legacy, &text(&alias_file), |_| true), Some(3));
        let url = reqwest::Url::from_file_path(&file).unwrap().to_string();
        assert_eq!(bound_live(&legacy, &url, |_| true), Some(2));

        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        super::bind_path(&db, &text(&real), 1).unwrap();
        super::bind_path(&db, &text(&file), 2).unwrap();
        assert_eq!(super::binding_for_exact(&db, &text(&alias_file)), Some(2));
        super::bind_path(&db, &text(&alias_file), 3).unwrap();
        assert_eq!(super::binding_for_exact(&db, &text(&file)), Some(3));
        assert_eq!(super::get_bindings(&db).len(), 2);
        super::bind_path(&db, &text(&alias), 4).unwrap();
        assert_eq!(super::binding_for_exact(&db, &text(&real)), Some(4));
        assert_eq!(super::get_bindings(&db).len(), 2);
        assert_eq!(
            bound_live(&super::get_bindings(&db), &text(&file), |_| true),
            Some(3)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn directory_cycles_are_skipped_before_collecting_files() {
        let dir = std::env::temp_dir().join(format!("kurisu-scan-cycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ep05.mkv"), []).unwrap();
        std::os::unix::fs::symlink(&dir, dir.join("loop")).unwrap();
        let mut paths = Vec::new();
        super::collect_videos(&dir, 0, &mut paths);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            paths,
            vec![dir.join("ep05.mkv").to_str().unwrap().to_string()]
        );
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
