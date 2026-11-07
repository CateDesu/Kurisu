mod commands;
mod db;
mod profile_transfer;
#[cfg(target_os = "linux")]
mod rendering;
mod shutdown;
mod tracking;
mod updater;

use directories::ProjectDirs;
use kurisu_core::{Core, Event, EventKind, Platform, Services, UpdateChannel};
use tauri::Manager;

struct TauriPlatform(tauri::AppHandle);

impl Platform for TauriPlatform {
    fn open_url(&self, url: &str) -> Result<(), String> {
        use tauri_plugin_opener::OpenerExt;
        self.0
            .opener()
            .open_url(url, None::<&str>)
            .map_err(|e| e.to_string())
    }
    fn open_path(&self, path: &std::path::Path) -> Result<(), String> {
        use tauri_plugin_opener::OpenerExt;
        self.0
            .opener()
            .open_path(path.to_string_lossy().into_owned(), None::<&str>)
            .map_err(|e| e.to_string())
    }
    fn reveal_path(&self, path: &std::path::Path) -> Result<(), String> {
        self.open_path(path.parent().unwrap_or(path))
    }
}

struct LegacyServices {
    services: tokio::sync::Mutex<Option<Services>>,
    event_task: tokio::task::JoinHandle<()>,
}

impl Drop for LegacyServices {
    fn drop(&mut self) {
        self.services.get_mut().take();
        self.event_task.abort();
    }
}

fn emit_core_event(app: &tauri::AppHandle, event: Event) {
    use tauri::Emitter;
    let prompt = tracking::LegacyPrompt::from_event(&event);
    match event.kind {
        EventKind::AuthExpired => {
            let _ = app.emit("kurisu://auth-expired", ());
        }
        EventKind::NowPlaying(value) => {
            let _ = app.emit("kurisu://now-playing", value);
        }
        EventKind::TrackingPrompt(_) => {
            if let Some(prompt) = prompt {
                let _ = app.emit("kurisu://tracking-prompt", prompt);
            }
        }
        EventKind::TrackingAsk(_) => {
            if let Some(prompt) = prompt {
                let _ = app.emit("kurisu://tracking-ask", prompt);
            }
        }
        EventKind::EpisodeUpdated(value) => {
            let _ = app.emit("kurisu://episode-updated", value);
        }
        EventKind::PendingChanged(ids) => {
            if let Some(ids) = ids {
                let _ = app.emit(
                    "kurisu://pending-changed",
                    serde_json::json!({"media_ids": ids}),
                );
            } else {
                let _ = app.emit("kurisu://pending-changed", ());
            }
        }
        EventKind::WatchHistoryUpdated(value) => {
            let _ = app.emit("kurisu://watch-history-updated", value);
        }
        EventKind::UpdateAvailable(value) => {
            let _ = app.emit("kurisu://update-available", value);
        }
        EventKind::UpdateFailed(message) => {
            let _ = app.emit(
                "kurisu://update-failed",
                serde_json::json!({"message": message}),
            );
        }
    }
}

fn current_event(core: &Core, event: Event) -> Option<Event> {
    core.event_is_current(&event).then_some(event)
}

fn open_database(app: &tauri::App) -> Result<db::Db, String> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    // SQLite creates token sidecars lazily. Protect the whole directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700));
    }
    let db_path = data_dir.join("kurisu.db");
    if let Some(legacy) = ProjectDirs::from("com", "catedesu", "kurisu")
        .map(|p| p.data_local_dir().join("kurisu.db"))
        .filter(|p| p != &db_path)
    {
        let target_free = std::fs::metadata(&db_path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
        let legacy_has_data = std::fs::metadata(&legacy)
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if target_free && legacy_has_data {
            migrate_legacy_db(&legacy, &db_path)
                .map_err(|e| format!("cannot migrate {}: {e}", legacy.display()))?;
        }
    }
    db::Db::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))
}

fn migrate_legacy_db(legacy: &std::path::Path, db_path: &std::path::Path) -> std::io::Result<()> {
    // Publish only complete copies, including uncheckpointed WAL writes.
    let tmp = db_path.with_file_name(".kurisu-migrate.tmp");
    let mut staged: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let result = (|| -> std::io::Result<()> {
        std::fs::copy(legacy, &tmp)?;
        for suffix in ["-wal", "-shm"] {
            let mut side = legacy.as_os_str().to_os_string();
            side.push(suffix);
            let side = std::path::PathBuf::from(side);
            if side.exists() {
                let mut tmp_side = tmp.as_os_str().to_os_string();
                tmp_side.push(suffix);
                let mut dst_side = db_path.as_os_str().to_os_string();
                dst_side.push(suffix);
                let (tmp_side, dst_side) = (
                    std::path::PathBuf::from(tmp_side),
                    std::path::PathBuf::from(dst_side),
                );
                std::fs::copy(&side, &tmp_side)?;
                staged.push((tmp_side, dst_side));
            }
        }
        // Remove foreign sidecars before SQLite can replay them.
        for suffix in ["-wal", "-shm"] {
            let mut dst_side = db_path.as_os_str().to_os_string();
            dst_side.push(suffix);
            match std::fs::remove_file(std::path::PathBuf::from(dst_side)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        for (from, to) in &staged {
            std::fs::rename(from, to)?;
        }
        // A nonempty main file prevents retries on the next launch.
        std::fs::rename(&tmp, db_path)?;
        Ok(())
    })();
    if let Err(e) = &result {
        log::warn!("legacy DB migration failed: {e}");
        let _ = std::fs::remove_file(&tmp);
        for (from, _) in &staged {
            let _ = std::fs::remove_file(from);
        }
        // Leave the destination free to retry migration next launch.
        let _ = std::fs::remove_file(db_path);
        for suffix in ["-wal", "-shm"] {
            let mut dst_side = db_path.as_os_str().to_os_string();
            dst_side.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(dst_side));
        }
    }
    result
}

const SINGLE_INSTANCE_ADDR: &str = "127.0.0.1:39418";

/// Distinguish Kurisu from an unrelated service holding the port.
const SINGLE_INSTANCE_ACK: u8 = b'k';

static INSTANCE_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

fn poke_running_instance() -> bool {
    use std::io::Read;
    let Ok(mut stream) = std::net::TcpStream::connect(SINGLE_INSTANCE_ADDR) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let mut ack = [0_u8; 1];
    stream.read_exact(&mut ack).is_ok() && ack[0] == SINGLE_INSTANCE_ACK
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("kurisu_lib=debug,info"),
    )
    .init();

    let context = tauri::generate_context!();
    #[cfg(target_os = "linux")]
    rendering::configure();

    // One instance prevents duplicate trackers and competing database writes.
    let instance_guard = match std::net::TcpListener::bind(SINGLE_INSTANCE_ADDR) {
        Ok(l) => Some(l),
        Err(_) => {
            if poke_running_instance() {
                eprintln!("kurisu: already running; raising the existing window");
                return;
            }
            log::error!(
                "cannot reserve {SINGLE_INSTANCE_ADDR}; quit any other Kurisu instance and retry"
            );
            return;
        }
    };

    // Answer before database setup so slow startup still looks alive.
    if let Some(guard) = instance_guard {
        std::thread::spawn(move || {
            use std::io::Write;
            for stream in guard.incoming() {
                if let Ok(mut stream) = stream {
                    let _ = stream.write_all(&[SINGLE_INSTANCE_ACK]);
                }
                if let Some(handle) = INSTANCE_HANDLE.get() {
                    if let Some(w) = handle.get_webview_window("main") {
                        let _ = w.unminimize();
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
        });
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(shutdown::Shutdown::default())
        .invoke_handler(tauri::generate_handler![
            profile_transfer::legacy_profile_database_path,
            profile_transfer::export_profile_preferences,
            profile_transfer::mirror_profile_preferences,
            commands::acknowledge_tracking_prompt,
            shutdown::shutdown_ready,
            shutdown::finish_shutdown,
            commands::is_logged_in,
            commands::login_with_token,
            commands::login_oauth,
            commands::logout,
            commands::current_user,
            commands::search_anime_page,
            commands::get_notifications_page,
            commands::mark_notifications_read,
            commands::get_entry_details,
            commands::update_entry_details,
            commands::get_season,
            commands::get_recommendations,
            commands::get_media,
            commands::get_media_detail,
            commands::get_airing_schedule,
            commands::sync_my_list,
            commands::local_entries,
            commands::local_entry_updates,
            commands::get_entry,
            commands::update_entry,
            commands::set_progress,
            commands::delete_entry_cmd,
            commands::get_tracking_config,
            commands::set_tracking_config,
            commands::get_app_setting,
            commands::set_app_setting,
            commands::get_library_folders,
            commands::add_library_folder,
            commands::remove_library_folder,
            commands::scan_library,
            commands::bind_library_path,
            commands::library_binding_for,
            commands::library_binding_details,
            commands::get_watch_history,
            commands::get_pending_changes,
            commands::sync_pending_changes,
            commands::resolve_pending_change,
            commands::unbind_library_media,
            commands::get_rss_feeds,
            commands::add_rss_feed,
            commands::remove_rss_feed,
            commands::fetch_torrents,
            commands::mark_torrents_seen,
            commands::search_torrents,
            commands::find_show_torrents,
            commands::get_user_stats,
            commands::check_update,
            commands::install_update,
            commands::take_update_failed,
            commands::take_pending_update,
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
            use tauri_plugin_dialog::DialogExt;

            if let Err(e) = updater::init_install_path() {
                log::warn!("updater: {e}");
            }
            app.manage(profile_transfer::Handoff::new()?);
            INSTANCE_HANDLE
                .set(app.handle().clone())
                .expect("instance handle set once");

            let db = match open_database(app) {
                Ok(db) => db,
                Err(e) => {
                    eprintln!("kurisu: cannot start: {e}");
                    app.dialog()
                        .message(format!("Kurisu cannot start.\n\n{e}"))
                        .title("Kurisu — startup error")
                        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                        .blocking_show();
                    std::process::exit(1);
                }
            };

            let data_dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
            let platform = std::sync::Arc::new(TauriPlatform(app.handle().clone()));
            let runtime = tauri::async_runtime::handle().inner().clone();
            let core = Core::from_db(db, data_dir, runtime, platform, UpdateChannel::Legacy);
            let mut events = core.subscribe();
            let handle = app.handle().clone();
            let bridge_core = core.clone();
            let event_task = core.spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if let Some(kind) = current_event(&bridge_core, event) {
                                emit_core_event(&handle, kind);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            for event in bridge_core.recovery_events() {
                                if let Some(kind) = current_event(&bridge_core, event) {
                                    emit_core_event(&handle, kind);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            let services = core.start_services();
            app.manage(core);
            app.manage(LegacyServices {
                services: tokio::sync::Mutex::new(Some(services)),
                event_task,
            });

            let tray_result: Result<(), String> = (|| {
                let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))
                    .map_err(|e| e.to_string())?;
                let show = MenuItem::with_id(app, "show", "Show Kurisu", true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)
                    .map_err(|e| e.to_string())?;
                let menu = Menu::with_items(app, &[&show, &quit]).map_err(|e| e.to_string())?;

                let _tray = TrayIconBuilder::with_id("main")
                    .icon(icon)
                    .tooltip("Kurisu")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(|app, event| match event.id.as_ref() {
                        "show" => {
                            if let Some(w) = app.get_webview_window("main") {
                                // Minimized windows ignore focus requests.
                                let _ = w.unminimize();
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                        "quit" => app.exit(0),
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, event| {
                        if let TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let app = tray.app_handle();
                            if let Some(w) = app.get_webview_window("main") {
                                // Tauri still considers minimized windows visible.
                                let minimized = w.is_minimized().unwrap_or(false);
                                if w.is_visible().unwrap_or(false) && !minimized {
                                    let _ = w.hide();
                                } else {
                                    let _ = w.unminimize();
                                    let _ = w.show();
                                    let _ = w.set_focus();
                                }
                            }
                        }
                    })
                    .build(app)
                    .map_err(|e| e.to_string())?;
                Ok(())
            })();
            let tray_available = tray_result.is_ok();
            if let Err(e) = tray_result {
                log::warn!("tray unavailable, continuing without it: {e}");
            }

            if let Some(main_window) = app.get_webview_window("main") {
                let w = main_window.clone();
                main_window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        let close_to_tray = w
                            .state::<Core>()
                            .legacy_setting("close_to_tray")
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        if (tray_available && close_to_tray && w.hide().is_ok())
                            || shutdown::request(w.app_handle(), 0)
                        {
                            api.prevent_close();
                        }
                    }
                });
            }

            Ok(())
        })
        .build(context)
        .expect("error while building kurisu")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                profile_transfer::launch_after_exit(app);
            }
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if shutdown::request(app, code.unwrap_or(0)) {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{current_event, migrate_legacy_db};
    use kurisu_core::models::{TrackingPrompt, UpdateInfo};
    use kurisu_core::{Core, Event, EventKind, Platform, UpdateChannel};
    use std::path::Path;
    use std::sync::Arc;

    pub(crate) struct TestPlatform;
    impl Platform for TestPlatform {
        fn open_url(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn reveal_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn bridge_rejects_old_account_events_and_keeps_global_updates() {
        let dir = std::env::temp_dir().join(format!(
            "kurisu-bridge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let core = Core::open(
            dir.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Legacy,
        )
        .unwrap();
        let db = kurisu_core::Db::open(&dir.join("kurisu.db")).unwrap();
        db.set_setting("anilist_user_id", "8").unwrap();
        let prompt = TrackingPrompt {
            media_id: 1,
            episode: 2,
            title: "Old".into(),
            raw_title: "Old".into(),
            progress: 1,
        };
        let stale = Event {
            generation: core.generation(),
            account_id: Some(7),
            prompt_id: Some(0),
            kind: EventKind::TrackingPrompt(prompt.clone()),
        };
        assert!(current_event(&core, stale).is_none());
        let mut events = core.subscribe();
        core.emit(EventKind::TrackingPrompt(prompt));
        let active = events.recv().await.unwrap();
        assert!(matches!(
            current_event(&core, active.clone()),
            Some(Event {
                kind: EventKind::TrackingPrompt(_),
                ..
            })
        ));
        assert!(core
            .recovery_events()
            .iter()
            .any(|event| matches!(&event.kind, EventKind::TrackingPrompt(_))));
        assert!(core.acknowledge_tracking_prompt(active.clone()).await);
        assert!(current_event(&core, active).is_none());
        let update = UpdateInfo {
            available: true,
            can_install: false,
            restart_pending: false,
            version: "2.0.0".into(),
            tag: "v2.0.0".into(),
            html_url: String::new(),
            body: String::new(),
            current: "1.0.0".into(),
        };
        let global = Event {
            generation: 0,
            account_id: None,
            prompt_id: None,
            kind: EventKind::UpdateAvailable(update),
        };
        assert!(matches!(
            current_event(&core, global),
            Some(Event {
                kind: EventKind::UpdateAvailable(_),
                ..
            })
        ));
        drop(db);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn test_dirs(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let base =
            std::env::temp_dir().join(format!("kurisu-migrate-{name}-{}", std::process::id()));
        let legacy_dir = base.join("legacy");
        let dest_dir = base.join("dest");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::create_dir_all(&dest_dir).unwrap();
        (base, legacy_dir, dest_dir)
    }

    #[test]
    fn migration_replaces_foreign_sidecars() {
        let (base, legacy_dir, dest_dir) = test_dirs("foreign");
        let legacy = legacy_dir.join("kurisu.db");
        std::fs::write(&legacy, b"legacy data").unwrap();
        std::fs::write(legacy_dir.join("kurisu.db-wal"), b"legacy wal").unwrap();
        std::fs::write(legacy_dir.join("kurisu.db-shm"), b"legacy shm").unwrap();
        let db_path = dest_dir.join("kurisu.db");
        std::fs::write(&db_path, b"").unwrap();
        std::fs::write(dest_dir.join("kurisu.db-wal"), b"foreign wal").unwrap();
        std::fs::write(dest_dir.join("kurisu.db-shm"), b"foreign shm").unwrap();

        migrate_legacy_db(&legacy, &db_path).unwrap();

        assert_eq!(std::fs::read(&db_path).unwrap(), b"legacy data");
        assert_eq!(
            std::fs::read(dest_dir.join("kurisu.db-wal")).unwrap(),
            b"legacy wal"
        );
        assert_eq!(
            std::fs::read(dest_dir.join("kurisu.db-shm")).unwrap(),
            b"legacy shm"
        );
        assert!(!dest_dir.join(".kurisu-migrate.tmp").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn migration_drops_destination_sidecars_the_legacy_db_lacks() {
        let (base, legacy_dir, dest_dir) = test_dirs("orphan");
        let legacy = legacy_dir.join("kurisu.db");
        std::fs::write(&legacy, b"legacy data").unwrap();
        let db_path = dest_dir.join("kurisu.db");
        std::fs::write(dest_dir.join("kurisu.db-wal"), b"foreign wal").unwrap();
        std::fs::write(dest_dir.join("kurisu.db-shm"), b"foreign shm").unwrap();

        migrate_legacy_db(&legacy, &db_path).unwrap();

        assert_eq!(std::fs::read(&db_path).unwrap(), b"legacy data");
        assert!(!dest_dir.join("kurisu.db-wal").exists());
        assert!(!dest_dir.join("kurisu.db-shm").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn migration_preserves_uncheckpointed_writes() {
        let (base, legacy_dir, dest_dir) = test_dirs("wal");
        let legacy = legacy_dir.join("kurisu.db");
        let source = rusqlite::Connection::open(&legacy).unwrap();
        source
            .execute_batch(
                "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE saved_progress (episode INTEGER NOT NULL);
             INSERT INTO saved_progress VALUES (7);",
            )
            .unwrap();
        assert!(
            std::fs::metadata(legacy_dir.join("kurisu.db-wal"))
                .unwrap()
                .len()
                > 0
        );
        let destination = dest_dir.join("kurisu.db");

        migrate_legacy_db(&legacy, &destination).unwrap();

        let migrated = rusqlite::Connection::open(&destination).unwrap();
        let episode: i64 = migrated
            .query_row("SELECT episode FROM saved_progress", [], |row| row.get(0))
            .unwrap();
        assert_eq!(episode, 7);
        assert_eq!(
            migrated
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        drop(migrated);
        drop(source);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn failed_migration_leaves_a_free_target() {
        let (base, legacy_dir, dest_dir) = test_dirs("failure");
        let legacy = legacy_dir.join("kurisu.db");
        std::fs::write(&legacy, b"legacy data").unwrap();
        std::fs::write(legacy_dir.join("kurisu.db-wal"), b"legacy wal").unwrap();
        let db_path = dest_dir.join("kurisu.db");
        std::fs::create_dir(dest_dir.join("kurisu.db-wal")).unwrap();

        assert!(migrate_legacy_db(&legacy, &db_path).is_err());

        assert!(!db_path.exists());
        assert!(!dest_dir.join(".kurisu-migrate.tmp").exists());
        assert!(!dest_dir.join(".kurisu-migrate.tmp-wal").exists());
        std::fs::remove_dir_all(&base).ok();
    }
}
