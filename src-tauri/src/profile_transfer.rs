use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

use kurisu_core::profile_transfer::{check_path, PREFERENCES_FILE};

#[cfg(test)]
const FORMAT: &str = "kurisu-ui-preferences-v1";
static MIRROR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MIRROR_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub struct Handoff {
    lock: tokio::sync::Mutex<()>,
    pending: std::sync::atomic::AtomicBool,
    #[cfg(target_os = "linux")]
    launch_path: std::path::PathBuf,
}

impl Handoff {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            lock: tokio::sync::Mutex::new(()),
            pending: std::sync::atomic::AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            launch_path: std::env::current_exe().map_err(|error| error.to_string())?,
        })
    }
}

fn encode_preferences(values: &BTreeMap<String, Option<String>>) -> Result<Vec<u8>, String> {
    kurisu_core::profile_transfer::encode_preferences(values).map_err(|error| error.to_string())
}

fn save_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    check_path(path).map_err(|error| error.to_string())?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not create a new export file: {error}"))?;
    let result = file.write_all(bytes).and_then(|_| file.sync_all());
    if let Err(error) = result {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(format!("Could not save UI preferences: {error}"));
    }
    sync_directory(path.parent().ok_or("Export path has no parent")?)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn replace_mirror(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
        }
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0x1 | 0x8) };
        if result == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(from, to)
}

fn save_mirror(directory: &Path, bytes: &[u8]) -> Result<(), String> {
    let _lock = MIRROR_LOCK.lock();
    check_path(directory).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let path = directory.join(PREFERENCES_FILE);
    check_path(&path).map_err(|error| error.to_string())?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.is_file() {
            return Err("Preference mirror is not a regular file".into());
        }
    }
    let temporary = directory.join(format!(
        ".kurisu-ui-{}-{}.tmp",
        std::process::id(),
        MIRROR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    save_new(&temporary, bytes)?;
    let result = replace_mirror(&temporary, &path).map_err(|error| error.to_string());
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    sync_directory(directory)
}

fn validate_mirror(directory: &Path) -> Result<(), String> {
    let path = directory.join(PREFERENCES_FILE);
    check_path(&path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("Save UI preferences before moving to native Kurisu: {error}"))?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err("Invalid durable UI preferences".into());
    }
    let data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    if data.as_object().is_none_or(|object| object.len() != 2)
        || data["format"] != "kurisu-ui-preferences-v1"
    {
        return Err("Invalid durable UI preferences".into());
    }
    let values: BTreeMap<String, Option<String>> =
        serde_json::from_value(data["preferences"].clone()).map_err(|error| error.to_string())?;
    encode_preferences(&values)?;
    Ok(())
}

#[tauri::command]
pub async fn mirror_profile_preferences(
    app: tauri::AppHandle,
    preferences: BTreeMap<String, Option<String>>,
) -> Result<(), String> {
    let bytes = encode_preferences(&preferences)?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    tokio::task::spawn_blocking(move || save_mirror(&directory, &bytes))
        .await
        .map_err(|error| error.to_string())?
}

async fn install_stopped(
    core: &kurisu_core::Core,
    services: &tokio::sync::Mutex<Option<kurisu_core::Services>>,
    install: impl std::future::Future<Output = Result<String, String>>,
) -> Result<String, String> {
    let mut services = services.lock().await;
    let previous = services.take();
    let was_running = previous.is_some();
    if let Some(previous) = previous {
        previous.shutdown().await;
    }
    let result = install.await;
    if result.is_err() && was_running {
        *services = Some(core.start_services());
    }
    result
}

pub async fn handoff(app: tauri::AppHandle, core: kurisu_core::Core) -> Result<String, String> {
    let state = app.state::<Handoff>();
    let _install = state
        .lock
        .try_lock()
        .map_err(|_| "An update is already being installed".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    validate_mirror(&directory)?;
    check_path(&directory.join("kurisu.db")).map_err(|error| error.to_string())?;
    let services = app.state::<crate::LegacyServices>();
    let outcome = install_stopped(
        &core,
        &services.services,
        kurisu_core::install_bridge_update(directory),
    )
    .await?;
    if outcome == "installed" || outcome == "restarting" {
        state
            .pending
            .store(true, std::sync::atomic::Ordering::Release);
        app.exit(0);
        return Ok("restarting".into());
    }
    Ok(outcome)
}

async fn settle_services(
    core: &kurisu_core::Core,
    services: &tokio::sync::Mutex<Option<kurisu_core::Services>>,
    exiting: bool,
) {
    let mut services = services.lock().await;
    if exiting {
        if let Some(previous) = services.take() {
            previous.shutdown().await;
        }
    } else if services.is_none() {
        *services = Some(core.start_services());
    }
}

pub async fn settle_close(app: &tauri::AppHandle, exiting: bool) {
    if app
        .state::<Handoff>()
        .pending
        .load(std::sync::atomic::Ordering::Acquire)
    {
        let core = app.state::<kurisu_core::Core>();
        let services = app.state::<crate::LegacyServices>();
        settle_services(&core, &services.services, exiting).await;
    }
}

pub fn launch_after_exit(app: &tauri::AppHandle) {
    #[cfg(target_os = "linux")]
    {
        let state = app.state::<Handoff>();
        if state
            .pending
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            if let Err(error) = std::process::Command::new(&state.launch_path).spawn() {
                log::error!("Native Kurisu installed but could not restart: {error}");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

#[tauri::command]
pub fn legacy_profile_database_path(app: tauri::AppHandle) -> Result<String, String> {
    let directory = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
    Ok(directory.join("kurisu.db").to_string_lossy().into_owned())
}

#[tauri::command]
pub async fn export_profile_preferences(
    app: tauri::AppHandle,
    preferences: BTreeMap<String, Option<String>>,
) -> Result<Option<String>, String> {
    let bytes = encode_preferences(&preferences)?;
    tokio::task::spawn_blocking(move || {
        let selected = app
            .dialog()
            .file()
            .set_title("Export Kurisu UI preferences")
            .set_file_name("kurisu-ui-preferences.json")
            .add_filter("JSON", &["json"])
            .blocking_save_file();
        let Some(selected) = selected else {
            return Ok(None);
        };
        let path = selected.into_path().map_err(|error| error.to_string())?;
        save_new(&path, &bytes)?;
        Ok(Some(path.to_string_lossy().into_owned()))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurisu_core::profile_transfer::UI_PREFERENCE_KEYS as KEYS;

    fn values() -> BTreeMap<String, Option<String>> {
        KEYS.iter().map(|key| ((*key).into(), None)).collect()
    }

    #[test]
    fn export_contains_only_allowlisted_values_and_nulls() {
        let mut values = values();
        values.insert("kurisu.list.q".into(), Some("".into()));
        values.insert(
            "kurisu.torrents.filters".into(),
            Some("{\"category\":\"all\"}".into()),
        );
        let bytes = encode_preferences(&values).unwrap();
        let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(data["format"], FORMAT);
        assert_eq!(data["preferences"]["kurisu.list.q"], "");
        assert_eq!(
            data["preferences"]["kurisu.list.sort"],
            serde_json::Value::Null
        );
        assert_eq!(data["preferences"].as_object().unwrap().len(), 9);
        values.insert("anilist_token".into(), Some("synthetic".into()));
        assert!(encode_preferences(&values).is_err());
    }

    #[test]
    fn export_never_replaces_an_existing_file() {
        let directory = std::env::temp_dir().join(format!(
            "kurisu-transfer-export-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("preferences.json");
        std::fs::write(&path, b"existing").unwrap();
        assert!(save_new(&path, b"new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn export_rejects_a_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let directory = std::env::temp_dir().join(format!(
            "kurisu-transfer-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let real = directory.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = directory.join("link");
        symlink(&real, &link).unwrap();
        assert!(save_new(&link.join("preferences.json"), b"synthetic").is_err());
        assert!(!real.join("preferences.json").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kurisu-bridge-test-{}-{}",
            std::process::id(),
            MIRROR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn mirror_replaces_complete_private_snapshot_and_retries_after_failure() {
        let directory = directory();
        let path = directory.join(PREFERENCES_FILE);
        let mut preferences = values();
        preferences.insert("kurisu.list.q".into(), Some("before".into()));
        save_mirror(&directory, &encode_preferences(&preferences).unwrap()).unwrap();
        validate_mirror(&directory).unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(data["preferences"]["kurisu.list.q"], "before");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        preferences.insert("kurisu.list.q".into(), Some("latest".into()));
        assert!(save_mirror(&directory, &encode_preferences(&preferences).unwrap()).is_err());
        std::fs::remove_dir(&path).unwrap();
        save_mirror(&directory, &encode_preferences(&preferences).unwrap()).unwrap();
        validate_mirror(&directory).unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(data["preferences"]["kurisu.list.q"], "latest");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_or_missing_mirror_blocks_handoff() {
        let directory = directory();
        assert!(validate_mirror(&directory).is_err());
        let path = directory.join(PREFERENCES_FILE);
        for invalid in [b"{}".as_slice(), b"{\"format\":\"kurisu-ui-preferences-v1\",\"preferences\":{\"anilist_token\":\"synthetic\"}}".as_slice()] {
            std::fs::write(&path, invalid).unwrap();
            assert!(validate_mirror(&directory).is_err());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn failed_install_restarts_services_and_allows_a_stopped_retry() {
        let directory = directory();
        let core = kurisu_core::Core::open(
            directory.clone(),
            tokio::runtime::Handle::current(),
            std::sync::Arc::new(crate::tests::TestPlatform),
            kurisu_core::UpdateChannel::Legacy,
        )
        .unwrap();
        let services = tokio::sync::Mutex::new(Some(core.start_services()));
        let result = install_stopped(&core, &services, async {
            Err("synthetic failed download".into())
        })
        .await;
        assert_eq!(result.unwrap_err(), "synthetic failed download");
        assert!(services.lock().await.is_some());
        let result = install_stopped(&core, &services, async { Ok("installed".into()) }).await;
        assert_eq!(result.unwrap(), "installed");
        assert!(services.lock().await.is_none());
        settle_services(&core, &services, false).await;
        assert!(services.lock().await.is_some());
        settle_services(&core, &services, true).await;
        assert!(services.lock().await.is_none());
        settle_services(&core, &services, true).await;
        assert!(services.lock().await.is_none());
        drop(core);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
