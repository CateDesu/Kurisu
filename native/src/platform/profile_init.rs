use super::{File, Path, PathBuf, Write, path_present, validate_profile_path};
use std::sync::atomic::{AtomicU64, Ordering};

const READY: &str = ".kurisu-native-init";
const MOVING: &str = ".publish-by-move";
const MOVE_CONTENT: &[u8] = b"kurisu-native-init-move-v1\n";
const DATABASE: &str = "kurisu.db";
const MARKER: &str = ".kurisu-native-profile";
const CONFLICT: &str = "Existing profile data conflicts with initialization. Keep it for recovery";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn lock(path: &Path) -> Result<File, String> {
    let name = path.join(".kurisu-native-init.lock");
    validate_profile_path(&name)?;
    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(name).map_err(|error| error.to_string())?;
    if !lock
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("The profile initialization lock must be a regular file".into());
    }
    lock.lock().map_err(|error| error.to_string())?;
    Ok(lock)
}

pub(super) fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

struct Stage(PathBuf);

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Stage {
    fn new(path: &Path) -> Result<Self, String> {
        loop {
            let name = path.join(format!(
                ".kurisu-native-init-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&name) {
                Ok(()) => return Ok(Self(name)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

pub(super) fn create(path: &Path) -> Result<(), String> {
    let stage = Stage::new(path)?;
    let database = stage.0.join(DATABASE);
    drop(kurisu_core::Db::open(&database).map_err(|error| error.to_string())?);
    kurisu_core::db_validate::validate_database(&database).map_err(|error| error.to_string())?;
    File::options()
        .write(true)
        .open(database)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut marker = options
        .open(stage.0.join(MARKER))
        .map_err(|error| error.to_string())?;
    marker
        .write_all(b"kurisu-native-v1\n")
        .and_then(|()| marker.sync_all())
        .map_err(|error| error.to_string())?;
    drop(marker);
    sync_directory(&stage.0)?;
    // Only a complete stage gets the recovery name. Incomplete crash debris is ignored.
    let ready = path.join(READY);
    if path_present(&ready)? {
        return Err("Profile initialization staging already exists. Keep it for recovery".into());
    }
    rename_stage(&stage.0, &ready).map_err(|error| error.to_string())?;
    recover(path)
}

pub(super) fn recover(path: &Path) -> Result<(), String> {
    let ready = path.join(READY);
    if !path_present(&ready)? {
        return Ok(());
    }
    super::sync_parent(path)?;
    let moving = ready.join(MOVING);
    let by_move = path_present(&moving)?;
    if by_move {
        let metadata = std::fs::metadata(&moving).map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.len() != MOVE_CONTENT.len() as u64
            || std::fs::read(&moving).map_err(|error| error.to_string())? != MOVE_CONTENT
        {
            return Err(
                "Profile initialization publication record is damaged. Keep it for recovery".into(),
            );
        }
        // The record may be visible after its directory sync failed.
        sync_directory(&ready)?;
    } else if !super::has_native_profile_unlocked(&ready)? {
        return Err("Profile initialization staging is incomplete. Keep it for recovery".into());
    }
    for entry in std::fs::read_dir(&ready).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() != DATABASE
            && entry.file_name() != MARKER
            && entry.file_name() != MOVING
        {
            return Err(
                "Profile initialization staging contains unexpected data. Keep it for recovery"
                    .into(),
            );
        }
    }
    for name in ["kurisu.db-wal", "kurisu.db-shm", "kurisu.db-journal"] {
        if !path_present(&path.join(MARKER))? && path_present(&path.join(name))? {
            return Err(CONFLICT.into());
        }
    }
    // Existing pairs must share file identity, including when switching to moves.
    for name in [DATABASE, MARKER] {
        let source = ready.join(name);
        let destination = path.join(name);
        let source_present = path_present(&source)?;
        let destination_present = path_present(&destination)?;
        if source_present && destination_present {
            if !same_file::is_same_file(&source, &destination).map_err(|error| error.to_string())? {
                return Err(CONFLICT.into());
            }
        } else if !source_present && (!by_move || !destination_present) {
            return Err(
                "Profile initialization staging is incomplete. Keep it for recovery".into(),
            );
        }
    }
    let database = path.join(DATABASE);
    if !path_present(&database)? && path_present(&path.join(MARKER))? {
        return Err(
            "Existing native marker conflicts with initialization. Keep it for recovery".into(),
        );
    }
    if by_move {
        let marker = if path_present(&ready.join(MARKER))? {
            ready.join(MARKER)
        } else {
            if path_present(&ready.join(DATABASE))? {
                return Err(
                    "Profile initialization publication order is damaged. Keep it for recovery"
                        .into(),
                );
            }
            path.join(MARKER)
        };
        let metadata = std::fs::metadata(&marker).map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.len() != b"kurisu-native-v1\n".len() as u64
            || std::fs::read(&marker).map_err(|error| error.to_string())? != b"kurisu-native-v1\n"
        {
            return Err("Profile initialization marker is damaged. Keep it for recovery".into());
        }
    }
    if !path_present(&database)? || (by_move && !path_present(&path.join(MARKER))?) {
        let source = if path_present(&ready.join(DATABASE))? {
            ready.join(DATABASE)
        } else {
            database
        };
        kurisu_core::db_validate::validate_database_with_scratch(&source, path)
            .map_err(|error| error.to_string())?;
    }
    // Make the recovery name durable before publishing any files from it.
    sync_directory(path)?;
    for name in [DATABASE, MARKER] {
        let source = ready.join(name);
        let destination = path.join(name);
        if path_present(&destination)? {
            if path_present(&source)? {
                if !same_file::is_same_file(&source, &destination)
                    .map_err(|error| error.to_string())?
                {
                    return Err(CONFLICT.into());
                }
                if by_move {
                    std::fs::remove_file(&source).map_err(|error| error.to_string())?;
                }
            }
        } else if by_move {
            // A synced move record and an absent source identify an interrupted atomic move.
            rename_new(&source, &destination).map_err(|error| error.to_string())?;
        } else if let Err(error) = std::fs::hard_link(&source, &destination) {
            if !links_unavailable(&error) {
                return Err(error.to_string());
            }
            let stage = Stage::new(path)?;
            let temporary = stage.0.join(MOVING);
            let mut options = File::options();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut record = options
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            record
                .write_all(MOVE_CONTENT)
                .and_then(|()| record.sync_all())
                .map_err(|error| error.to_string())?;
            drop(record);
            rename_new(&temporary, &moving).map_err(|error| error.to_string())?;
            drop(stage);
            return recover(path);
        }
        sync_directory(path)?;
        if by_move {
            sync_directory(&ready)?;
        }
    }
    // Unlink the recovery name atomically before best effort removal of its owned files.
    loop {
        let cleanup = path.join(format!(
            ".kurisu-native-init-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if path_present(&cleanup)? {
            continue;
        }
        match rename_stage(&ready, &cleanup) {
            Ok(()) => {
                sync_directory(path)?;
                drop(Stage(cleanup));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn rename_stage(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", windows))]
    return rename_new(source, destination);
    #[cfg(not(any(target_os = "linux", windows)))]
    std::fs::rename(source, destination)
}

fn links_unavailable(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(target_os = "linux")]
    if error.raw_os_error() == Some(1) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(1 | 50)) {
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn rename_new(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn renameat2(
            old_dir: i32,
            old: *const std::ffi::c_char,
            new_dir: i32,
            new: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // RENAME_NOREPLACE preserves a destination created outside the initialization lock.
    if unsafe { renameat2(-100, source.as_ptr(), -100, destination.as_ptr(), 1) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_new(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // MOVEFILE_WRITE_THROUGH without replacement.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 8) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn rename_new(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::hard_link(source, destination)?;
    std::fs::remove_file(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempProfile(PathBuf);

    impl TempProfile {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "kurisu-profile-init-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempProfile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seed_ready_stage(profile: &Path) -> PathBuf {
        let ready = profile.join(READY);
        std::fs::create_dir(&ready).unwrap();
        let database = ready.join(DATABASE);
        drop(kurisu_core::Db::open(&database).unwrap());
        std::fs::write(ready.join(MARKER), b"kurisu-native-v1\n").unwrap();
        ready
    }

    #[test]
    fn recover_skips_occupied_cleanup_names_and_preserves_debris() {
        let profile = TempProfile::new("cleanup-collision");
        let _ready = seed_ready_stage(&profile.0);
        let start = SEQUENCE.load(Ordering::Relaxed);
        let debris: Vec<_> = (start..start + 32)
            .map(|seq| {
                let dir = profile
                    .0
                    .join(format!(".kurisu-native-init-{}-{seq}", std::process::id()));
                std::fs::create_dir(&dir).unwrap();
                std::fs::write(dir.join("keep"), b"preserved").unwrap();
                dir
            })
            .collect();

        recover(&profile.0).unwrap();

        assert!(
            !profile.0.join(READY).exists(),
            "recovery must unlink .kurisu-native-init even when earlier cleanup names are occupied"
        );
        assert!(profile.0.join(DATABASE).is_file());
        assert!(profile.0.join(MARKER).is_file());
        for dir in &debris {
            assert_eq!(std::fs::read(dir.join("keep")).unwrap(), b"preserved");
        }

        // Replacing the published database with a fresh inode must not conflict with stale staging.
        let copy = profile.0.join("kurisu.db.copy");
        std::fs::copy(profile.0.join(DATABASE), &copy).unwrap();
        std::fs::rename(&copy, profile.0.join(DATABASE)).unwrap();
        recover(&profile.0).unwrap();
    }

    #[test]
    fn recover_rejects_oversized_move_record_and_marker() {
        let profile = TempProfile::new("oversized-moving");
        let ready = seed_ready_stage(&profile.0);
        std::fs::write(ready.join(MOVING), [MOVE_CONTENT, b"extra"].concat()).unwrap();
        let error = recover(&profile.0).unwrap_err();
        assert!(error.contains("publication record is damaged"), "{error}");

        let profile = TempProfile::new("oversized-marker");
        let ready = seed_ready_stage(&profile.0);
        std::fs::write(ready.join(MOVING), MOVE_CONTENT).unwrap();
        std::fs::write(ready.join(MARKER), b"kurisu-native-v1\nextra").unwrap();
        let error = recover(&profile.0).unwrap_err();
        assert!(error.contains("marker is damaged"), "{error}");
    }
}
