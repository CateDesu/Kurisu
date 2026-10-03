use anyhow::{anyhow, Result};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use crate::db::Db;
use crate::models::{LibraryFile, LibraryScan, UnreadableFolder};
use crate::recognize::{basename, match_title, resolve_playback_episode, Matcher, VIDEO_EXTS};

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

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct LibraryBinding {
    pub media_id: i64,
    pub episode_offset: i64,
}

impl<'de> Deserialize<'de> for LibraryBinding {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SavedBinding {
            Legacy(i64),
            Current {
                media_id: i64,
                #[serde(default)]
                episode_offset: i64,
            },
        }
        Ok(match SavedBinding::deserialize(deserializer)? {
            SavedBinding::Legacy(media_id) => Self {
                media_id,
                episode_offset: 0,
            },
            SavedBinding::Current {
                media_id,
                episode_offset,
            } => Self {
                media_id,
                episode_offset,
            },
        })
    }
}

impl From<i64> for LibraryBinding {
    fn from(media_id: i64) -> Self {
        Self {
            media_id,
            episode_offset: 0,
        }
    }
}

pub fn get_bindings(db: &Db) -> std::collections::HashMap<String, LibraryBinding> {
    match db.get_setting(BINDINGS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("corrupt library_bindings setting, starting from empty: {e}");
            std::collections::HashMap::new()
        }),
        None => std::collections::HashMap::new(),
    }
}

pub fn bind_path(db: &Db, path: &str, media_id: i64, episode_offset: i64) -> Result<()> {
    if !(-9999..=9999).contains(&episode_offset) {
        return Err(anyhow!("episode offset must be between -9999 and 9999"));
    }
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    let resolved = resolved_path(path);
    bindings.retain(|saved, _| resolved_path(saved) != resolved);
    bindings.insert(
        path.to_string(),
        LibraryBinding {
            media_id,
            episode_offset,
        },
    );
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

pub fn unbind_media(db: &Db, media_id: i64) -> Result<()> {
    let _guard = BINDINGS_LOCK.lock();
    let mut bindings = get_bindings(db);
    bindings.retain(|_, binding| binding.media_id != media_id);
    db.set_setting(BINDINGS_KEY, &serde_json::to_string(&bindings)?)
}

pub fn binding_for_exact(db: &Db, path: &str) -> Option<LibraryBinding> {
    let resolved = resolved_path(path);
    let local = local_path(path);
    binding_paths(&get_bindings(db))
        .into_iter()
        .filter(|(_, saved, _)| *saved == resolved)
        .max_by_key(|(saved, _, _)| (local_path(saved) == local, std::cmp::Reverse(saved.clone())))
        .map(|(_, _, id)| id)
}

pub fn effective_binding(db: &Db, path: &str) -> Option<LibraryBinding> {
    bound_path_id(&binding_paths(&get_bindings(db)), path, |id| {
        db.get_entry(id).ok().flatten().is_some()
    })
}

type BindingPath = (String, std::path::PathBuf, LibraryBinding);

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

fn binding_paths(bindings: &std::collections::HashMap<String, LibraryBinding>) -> Vec<BindingPath> {
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
    let bindings = bindings
        .iter()
        .map(|(path, id)| (path.clone(), (*id).into()))
        .collect();
    bound_path_id(&binding_paths(&bindings), path, live).map(|b| b.media_id)
}

fn bound_path_id(
    bindings: &[BindingPath],
    path: &str,
    live: impl Fn(i64) -> bool,
) -> Option<LibraryBinding> {
    let resolved = resolved_path(path);
    bound_resolved_path_id(bindings, path, &resolved, live)
}

fn bound_resolved_path_id(
    bindings: &[BindingPath],
    path: &str,
    resolved: &Path,
    live: impl Fn(i64) -> bool,
) -> Option<LibraryBinding> {
    let local = local_path(path);
    bindings
        .iter()
        .filter(|(_, prefix, binding)| resolved.starts_with(prefix) && live(binding.media_id))
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
    bindings: &std::collections::HashMap<String, LibraryBinding>,
    path: &str,
) -> Option<(&'a Matcher, LibraryBinding)> {
    match_bound_path(
        matchers,
        &binding_paths(bindings),
        path,
        &resolved_path(path),
    )
}

fn match_bound_path<'a>(
    matchers: &'a [Matcher],
    bindings: &[BindingPath],
    path: &str,
    resolved: &Path,
) -> Option<(&'a Matcher, LibraryBinding)> {
    let binding = bound_resolved_path_id(bindings, path, resolved, |id| {
        matchers.iter().any(|m| m.media_id == id)
    })?;
    let matched = matchers.iter().find(|m| m.media_id == binding.media_id)?;
    Some((matched, binding))
}

/// Bound candidates start with the filename used by Library.
pub(crate) fn resolve_bound_episode(
    matched: &Matcher,
    candidates: &[&str],
    binding: Option<LibraryBinding>,
) -> Option<i64> {
    let episode = if binding.is_some() && matched.episodes != Some(1) {
        if candidates.first().is_some_and(|candidate| {
            crate::recognize::excluded_playback_release(matched, candidate)
        }) {
            return None;
        }
        candidates
            .iter()
            .find_map(|candidate| resolve_playback_episode(matched, &[*candidate], true))
    } else {
        resolve_playback_episode(matched, candidates, binding.is_some())
    };
    episode?
        .checked_add(binding.map_or(0, |b| b.episode_offset))
        .filter(|episode| {
            *episode > 0
                && matched
                    .episodes
                    .filter(|total| *total > 0)
                    .is_none_or(|total| *episode <= total)
        })
}

struct CachedMatch {
    resolved: PathBuf,
    media_id: Option<i64>,
    matched: Option<String>,
    episode: Option<i64>,
    bound: bool,
}

#[derive(Default)]
pub struct ScanCache {
    matchers: Weak<Vec<Matcher>>,
    bindings: Vec<BindingPath>,
    year: i32,
    files: HashMap<String, CachedMatch>,
    #[cfg(test)]
    recognized: usize,
}

impl ScanCache {
    /// Call from spawn_blocking.
    pub fn scan(
        &mut self,
        folders: &[String],
        matchers: &Arc<Vec<Matcher>>,
        bindings: &HashMap<String, LibraryBinding>,
    ) -> LibraryScan {
        let mut bindings = binding_paths(bindings);
        bindings.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let same_matchers = self
            .matchers
            .upgrade()
            .is_some_and(|previous| Arc::ptr_eq(&previous, matchers));
        // Recent years are release metadata.
        let year = chrono::Utc::now().year();
        if !same_matchers || self.bindings != bindings || self.year != year {
            self.files.clear();
        }
        self.matchers = Arc::downgrade(matchers);
        self.bindings = bindings;
        self.year = year;
        self.scan_paths(folders, matchers)
    }

    fn scan_paths(&mut self, folders: &[String], matchers: &[Matcher]) -> LibraryScan {
        let mut paths = Vec::new();
        let mut unreadable = Vec::new();
        for folder in folders {
            collect_videos(Path::new(folder), 0, &mut paths, &mut unreadable);
        }
        // Canonical paths deduplicate aliases and roots saved before overlap checks existed.
        let mut seen = std::collections::HashSet::new();
        let mut paths: Vec<_> = paths
            .into_iter()
            .filter_map(|path| {
                let resolved =
                    std::fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
                seen.insert(resolved.clone()).then_some((path, resolved))
            })
            .collect();
        paths.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut files = Vec::with_capacity(paths.len());
        let mut retained = HashMap::with_capacity(paths.len());
        for (path, resolved) in paths {
            let cached = self
                .files
                .remove(&path)
                .filter(|cached| cached.resolved == resolved);
            let recognized = cached.unwrap_or_else(|| {
                #[cfg(test)]
                {
                    self.recognized += 1;
                }
                let base = basename(&path);
                let bound = match_bound_path(matchers, &self.bindings, &path, &resolved);
                let matched = bound
                    .map(|(m, _)| m)
                    .or_else(|| match_title(matchers, "", &path));
                let episode = matched.and_then(|m| {
                    resolve_bound_episode(m, &[base.as_str()], bound.map(|(_, b)| b))
                });
                CachedMatch {
                    resolved,
                    media_id: matched.map(|m| m.media_id),
                    matched: matched.map(|m| m.display.clone()),
                    episode,
                    bound: bound.is_some(),
                }
            });
            files.push(LibraryFile {
                path: path.clone(),
                media_id: recognized.media_id,
                matched: recognized.matched.clone(),
                episode: recognized.episode,
                bound: recognized.bound,
            });
            retained.insert(path, recognized);
        }
        self.files = retained;
        LibraryScan { files, unreadable }
    }
}

#[cfg(test)]
fn scan_paths(
    folders: &[String],
    matchers: &[Matcher],
    bindings: &HashMap<String, LibraryBinding>,
) -> LibraryScan {
    ScanCache {
        bindings: binding_paths(bindings),
        ..Default::default()
    }
    .scan_paths(folders, matchers)
}

fn collect_videos(
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
    unreadable: &mut Vec<UnreadableFolder>,
) {
    let mut pending = std::collections::VecDeque::from([(dir.to_path_buf(), depth)]);
    let mut visited = std::collections::HashSet::new();
    while let Some((dir, depth)) = pending.pop_front() {
        if depth > MAX_DEPTH {
            unreadable.push(UnreadableFolder {
                path: dir.to_string_lossy().into_owned(),
                error: format!("scan stopped at the maximum depth of {MAX_DEPTH} folders"),
            });
            continue;
        }
        let canonical = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !visited.insert(canonical) {
            continue;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) => {
                unreadable.push(UnreadableFolder {
                    path: dir.to_string_lossy().into_owned(),
                    error: error.to_string(),
                });
                continue;
            }
        };
        let mut entries = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => entries.push(entry),
                Err(error) => unreadable.push(UnreadableFolder {
                    path: dir.to_string_lossy().into_owned(),
                    error: error.to_string(),
                }),
            }
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    unreadable.push(UnreadableFolder {
                        path: path.to_string_lossy().into_owned(),
                        error: error.to_string(),
                    });
                    continue;
                }
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

    fn cache_folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kurisu-cache-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_cached_show(db: &Db, id: i64, title: &str, episodes: i64, status: &str) {
        db.upsert_media(&crate::models::Media {
            id,
            title_english: Some(title.into()),
            episodes: Some(episodes),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: id,
            status: status.into(),
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn scan_cache_reuses_matches_while_discovering_and_removing_files() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_cached_show(&db, 1, "Some Show", 12, "CURRENT");
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let dir = cache_folder("files");
        let old = dir.join("Some Show - 01.mkv");
        let unmatched = dir.join("Unknown - 01.mkv");
        std::fs::write(&old, []).unwrap();
        std::fs::write(&unmatched, []).unwrap();
        let folders = vec![dir.to_str().unwrap().to_string()];
        let mut cache = super::ScanCache::default();
        let bindings = HashMap::new();

        let first = cache.scan(&folders, &matchers, &bindings);
        let second = cache.scan(&folders, &matchers, &bindings);
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        assert_eq!(cache.recognized, 2);

        std::fs::rename(&old, dir.join("Some Show - 02.mkv")).unwrap();
        let scan = cache.scan(&folders, &matchers, &bindings);
        assert_eq!(scan.files.len(), 2);
        assert_eq!(scan.files[0].episode, Some(2));
        assert_eq!(cache.recognized, 3);
        assert_eq!(cache.files.len(), 2);

        std::fs::remove_file(&unmatched).unwrap();
        assert_eq!(cache.scan(&folders, &matchers, &bindings).files.len(), 1);
        assert_eq!(cache.files.len(), 1);
        assert_eq!(cache.recognized, 3);

        std::fs::remove_dir_all(&dir).unwrap();
        let missing = cache.scan(&folders, &matchers, &bindings);
        assert!(missing.files.is_empty());
        assert_eq!(missing.unreadable.len(), 1);
        assert!(cache.files.is_empty());
    }

    #[test]
    fn scan_cache_follows_title_total_status_and_account_changes() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_cached_show(&db, 1, "Old Title", 12, "CURRENT");
        let dir = cache_folder("matchers");
        std::fs::write(dir.join("Some Show - 03.mkv"), []).unwrap();
        let folders = vec![dir.to_str().unwrap().to_string()];
        let mut cache = super::ScanCache::default();
        let mut snapshots = Vec::new();
        let mut scan = || {
            let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
            snapshots.push(matchers);
            cache
                .scan(&folders, snapshots.last().unwrap(), &HashMap::new())
                .files
                .remove(0)
        };
        assert_eq!(scan().media_id, None);

        seed_cached_show(&db, 1, "Some Show", 12, "CURRENT");
        let renamed = scan();
        assert_eq!((renamed.media_id, renamed.episode), (Some(1), Some(3)));
        seed_cached_show(&db, 1, "Some Show", 2, "CURRENT");
        assert_eq!(scan().episode, None);

        seed_cached_show(&db, 1, "Some Show", 12, "PAUSED");
        seed_cached_show(&db, 2, "Some Show", 12, "CURRENT");
        assert_eq!(scan().media_id, Some(2));
        db.clear_entries().unwrap();
        assert_eq!(scan().media_id, None);
        seed_cached_show(&db, 3, "Some Show", 12, "CURRENT");
        assert_eq!(scan().media_id, Some(3));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scan_cache_follows_binding_offsets_and_unlinks() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_cached_show(&db, 1, "Some Show", 12, "CURRENT");
        seed_cached_show(&db, 2, "Another Show", 12, "CURRENT");
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let dir = cache_folder("bindings");
        let path = dir.join("ep13.mkv").to_str().unwrap().to_string();
        std::fs::write(&path, []).unwrap();
        let folders = vec![dir.to_str().unwrap().to_string()];
        let mut cache = super::ScanCache::default();
        let mut bindings = HashMap::new();
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].media_id,
            None
        );

        bindings.insert(path.clone(), super::LibraryBinding::from(1));
        let linked = cache.scan(&folders, &matchers, &bindings).files.remove(0);
        assert!(linked.bound);
        assert_eq!((linked.media_id, linked.episode), (Some(1), None));
        bindings.get_mut(&path).unwrap().episode_offset = -12;
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].episode,
            Some(1)
        );
        bindings.get_mut(&path).unwrap().media_id = 2;
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].media_id,
            Some(2)
        );
        bindings.clear();
        let unlinked = cache.scan(&folders, &matchers, &bindings).files.remove(0);
        assert!(!unlinked.bound);
        assert_eq!(unlinked.media_id, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_cache_follows_file_and_binding_symlink_targets() {
        use std::os::unix::fs::symlink;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_cached_show(&db, 1, "Some Show", 12, "CURRENT");
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let dir = cache_folder("symlinks");
        let root = dir.join("scan");
        std::fs::create_dir(&root).unwrap();
        let first = dir.join("first.mkv");
        let second = dir.join("second.mkv");
        std::fs::write(&first, []).unwrap();
        std::fs::write(&second, []).unwrap();
        let path = root.join("ep01.mkv");
        let binding = dir.join("binding.mkv");
        symlink(&first, &path).unwrap();
        symlink(&first, &binding).unwrap();
        let folders = vec![root.to_str().unwrap().to_string()];
        let bindings = HashMap::from([(
            binding.to_str().unwrap().to_string(),
            super::LibraryBinding::from(1),
        )]);
        let mut cache = super::ScanCache::default();
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].media_id,
            Some(1)
        );

        std::fs::remove_file(&path).unwrap();
        symlink(&second, &path).unwrap();
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].media_id,
            None
        );
        std::fs::remove_file(&binding).unwrap();
        symlink(&second, &binding).unwrap();
        assert_eq!(
            cache.scan(&folders, &matchers, &bindings).files[0].media_id,
            Some(1)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adversarial_movie_bonus_scan_preserves_the_feature_and_real_ova() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, title) in [(1, "Some Movie"), (2, "Some Show OVA")] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(1),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let dir = std::env::temp_dir().join(format!("kurisu-bonus-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bindings = HashMap::new();
        for (filename, id) in [
            ("Some Movie.mkv", 1),
            ("Some Movie [OVA].mkv", 1),
            ("Some Movie_OAD1.mkv", 1),
            ("Some Movie [SP01].mkv", 1),
            ("OP1.mkv", 1),
            ("Some Movie [ED1].mkv", 1),
            ("Some Movie [OST].mkv", 1),
            ("Some Show OVA.mkv", 2),
        ] {
            let path = dir.join(filename);
            std::fs::write(&path, []).unwrap();
            bindings.insert(path.to_str().unwrap().to_string(), id.into());
        }
        let scan = scan_paths(
            &[dir.to_str().unwrap().into()],
            &crate::recognize::build_matchers(&db),
            &bindings,
        );
        std::fs::remove_dir_all(&dir).unwrap();
        println!(
            "bonus scan evidence: {}",
            serde_json::to_string(&scan).unwrap()
        );
        assert!(scan.unreadable.is_empty());
        assert_eq!(scan.files.len(), 8);
        for file in &scan.files {
            let feature =
                file.path.ends_with("Some Movie.mkv") || file.path.ends_with("Some Show OVA.mkv");
            assert_eq!(file.episode, feature.then_some(1), "{}", file.path);
        }
    }

    #[test]
    fn adversarial_bound_packs_cannot_borrow_an_embedded_single_episode() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
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
        for filename in ["Some Show - 01-12.mkv", "[01-12].mkv", "E01-E12.mkv"] {
            assert_eq!(
                super::resolve_bound_episode(
                    &matchers[0],
                    &[filename, "Some Show - Episode 1"],
                    Some(1.into())
                ),
                None,
                "{filename} inherited an episode from embedded metadata"
            );
        }
        assert_eq!(
            super::resolve_bound_episode(
                &matchers[0],
                &["video.mkv", "Some Show - Episode 1"],
                Some(1.into())
            ),
            Some(1)
        );
        assert_eq!(
            super::resolve_bound_episode(
                &matchers[0],
                &["Some Show E03 - 1-2-3 Go!.mkv", "Some Show - Episode 1"],
                Some(1.into())
            ),
            Some(3)
        );
    }

    #[test]
    fn adversarial_library_scan_leaves_packs_and_fractional_movies_unnumbered() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, title, total) in [(1, "Some Show", 12), (2, "Some Movie", 1)] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(total),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let dir =
            std::env::temp_dir().join(format!("kurisu-release-numbering-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bindings = HashMap::new();
        for (name, id) in [
            ("Some Show - 01-12.mkv", 1),
            ("Some Show - 01.mkv", 1),
            ("Some Movie - 12.5.mkv", 2),
            ("Some Movie.mkv", 2),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, []).unwrap();
            bindings.insert(path.to_str().unwrap().to_string(), id.into());
        }
        let scan = scan_paths(
            &[dir.to_str().unwrap().into()],
            &crate::recognize::build_matchers(&db),
            &bindings,
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(scan.unreadable.is_empty());
        assert_eq!(scan.files.len(), 4);
        for file in &scan.files {
            let numbered =
                file.path.ends_with("Some Movie.mkv") || file.path.ends_with("Some Show - 01.mkv");
            assert_eq!(file.episode, numbered.then_some(1), "{}", file.path);
        }
        println!(
            "library scan evidence: {}",
            serde_json::to_string(&scan).unwrap()
        );
    }

    #[test]
    fn numbering_details_preserve_inherited_offsets_and_file_overrides() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let root = std::env::temp_dir().join("kurisu-inherited-numbering");
        let folder = root.join("season");
        let file = folder.join("episode13.mkv");
        super::bind_path(&db, root.to_str().unwrap(), 1, -12).unwrap();
        assert_eq!(
            super::effective_binding(&db, file.to_str().unwrap())
                .unwrap()
                .episode_offset,
            -12
        );
        super::bind_path(&db, folder.to_str().unwrap(), 1, -24).unwrap();
        assert_eq!(
            super::effective_binding(&db, file.to_str().unwrap())
                .unwrap()
                .episode_offset,
            -24
        );
        super::bind_path(&db, file.to_str().unwrap(), 1, -36).unwrap();
        let url = reqwest::Url::from_file_path(&file).unwrap();
        assert_eq!(
            super::effective_binding(&db, url.as_str())
                .unwrap()
                .episode_offset,
            -36
        );
        db.delete_entry(1).unwrap();
        assert_eq!(super::effective_binding(&db, file.to_str().unwrap()), None);
    }

    #[test]
    fn saved_bindings_accept_legacy_ids_and_persist_episode_offsets() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.set_setting(
            super::BINDINGS_KEY,
            r#"{"/anime/first":1,"/anime/second":{"media_id":2,"episode_offset":-12}}"#,
        )
        .unwrap();
        let bindings = super::get_bindings(&db);
        assert_eq!(bindings["/anime/first"], 1.into());
        assert_eq!(bindings["/anime/second"].episode_offset, -12);
        super::bind_path(&db, "/anime/first", 1, 3).unwrap();
        assert_eq!(
            super::binding_for_exact(&db, "/anime/first")
                .unwrap()
                .episode_offset,
            3
        );
        assert_eq!(
            super::get_bindings(&db)["/anime/second"].episode_offset,
            -12
        );
        assert!(super::bind_path(&db, "/anime/first", 1, i64::MAX).is_err());
        super::unbind_media(&db, 1).unwrap();
        assert_eq!(super::get_bindings(&db).len(), 1);
    }

    #[test]
    fn bound_episode_offsets_agree_for_library_files_and_playback_urls() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 2,
            title_english: Some("Some Show 2nd Season".into()),
            episodes: Some(12),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 2,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let matchers = crate::recognize::build_matchers(&db);
        let dir = std::env::temp_dir().join(format!("kurisu-offset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for episode in [13, 24, 25] {
            std::fs::write(dir.join(format!("Some Show - {episode}.mkv")), []).unwrap();
        }
        super::bind_path(&db, dir.to_str().unwrap(), 2, -12).unwrap();
        let bindings = super::get_bindings(&db);
        let scan = scan_paths(&[dir.to_str().unwrap().into()], &matchers, &bindings);
        assert_eq!(
            scan.files.iter().map(|f| f.episode).collect::<Vec<_>>(),
            vec![Some(1), Some(12), None]
        );
        let file = dir.join("Some Show - 13.mkv");
        let url = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let (matched, binding) = super::bound_match(&matchers, &bindings, &url).unwrap();
        assert_eq!(
            super::resolve_bound_episode(matched, &["Some Show - 13.mkv"], Some(binding)),
            Some(1)
        );
        assert_eq!(
            super::resolve_bound_episode(matched, &["Some Show - 13.mkv"], None),
            None
        );
        super::bind_path(&db, file.to_str().unwrap(), 2, -1).unwrap();
        let bindings = super::get_bindings(&db);
        let (matched, binding) = super::bound_match(&matchers, &bindings, &url).unwrap();
        assert_eq!(
            super::resolve_bound_episode(matched, &["Some Show - 13.mkv"], Some(binding)),
            Some(12)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bound_numbering_prefers_the_filename_over_embedded_episode_titles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
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
        let binding = Some(super::LibraryBinding {
            media_id: 1,
            episode_offset: -12,
        });
        for filename in ["Some Show - 13.mkv", "ep13.mkv"] {
            assert_eq!(
                super::resolve_bound_episode(&matchers[0], &[filename], binding),
                Some(1)
            );
            assert_eq!(
                super::resolve_bound_episode(
                    &matchers[0],
                    &[filename, "Some Show - Episode 24"],
                    binding
                ),
                Some(1),
                "{filename}"
            );
        }
        assert_eq!(
            super::resolve_bound_episode(
                &matchers[0],
                &["video.mkv", "Some Show - Episode 13"],
                binding
            ),
            Some(1)
        );
        assert_eq!(
            super::resolve_bound_episode(
                &matchers[0],
                &["ep25.mkv", "Some Show - Episode 13"],
                binding
            ),
            None
        );
    }

    #[test]
    fn deeply_nested_files_are_reported_when_the_scan_limit_is_reached() {
        let dir = std::env::temp_dir().join(format!("kurisu-scan-depth-{}", std::process::id()));
        let mut nested = dir.clone();
        for _ in 0..=super::MAX_DEPTH {
            nested = nested.join("nested");
        }
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("ep01.mkv"), []).unwrap();
        std::fs::write(dir.join("ep02.mkv"), []).unwrap();
        let scan = scan_paths(&[dir.to_str().unwrap().into()], &[], &HashMap::new());
        assert_eq!(scan.files.len(), 1);
        assert!(scan
            .unreadable
            .iter()
            .any(|warning| warning.path == nested.to_str().unwrap()
                && warning.error.contains("maximum depth")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nested_filesystem_errors_do_not_hide_successful_scan_results() {
        let dir =
            std::env::temp_dir().join(format!("kurisu-scan-nested-error-{}", std::process::id()));
        let nested = dir.join("season");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("ep01.mkv"), []).unwrap();
        let broken = nested.join("unmounted");
        std::os::unix::fs::symlink(nested.join("missing"), &broken).unwrap();
        let scan = scan_paths(&[dir.to_str().unwrap().into()], &[], &HashMap::new());
        assert_eq!(scan.files.len(), 1);
        assert!(scan
            .unreadable
            .iter()
            .any(|warning| warning.path == broken.to_str().unwrap() && !warning.error.is_empty()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
        let bindings = HashMap::from([(
            folder.to_str().unwrap().to_string(),
            super::LibraryBinding::from(1),
        )]);
        for path in [file.to_str().unwrap(), url.as_str()] {
            let (matched, _) = super::bound_match(&matchers, &bindings, path).unwrap();
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
        super::bind_path(&db, &text(&real), 1, 0).unwrap();
        super::bind_path(&db, &text(&file), 2, 0).unwrap();
        assert_eq!(
            super::binding_for_exact(&db, &text(&alias_file)),
            Some(2.into())
        );
        super::bind_path(&db, &text(&alias_file), 3, 0).unwrap();
        assert_eq!(super::binding_for_exact(&db, &text(&file)), Some(3.into()));
        assert_eq!(super::get_bindings(&db).len(), 2);
        super::bind_path(&db, &text(&alias), 4, 0).unwrap();
        assert_eq!(super::binding_for_exact(&db, &text(&real)), Some(4.into()));
        assert_eq!(super::get_bindings(&db).len(), 2);
        assert_eq!(
            bound_live(
                &super::get_bindings(&db)
                    .into_iter()
                    .map(|(path, b)| (path, b.media_id))
                    .collect(),
                &text(&file),
                |_| true
            ),
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
        super::collect_videos(&dir, 0, &mut paths, &mut Vec::new());
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
