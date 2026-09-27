mod anilist;
mod commands;
mod db;
mod discord;
mod library;
mod models;
mod mpvipc;
mod playback;
mod recognize;
mod rss;
mod updater;

use commands::AppState;
use directories::ProjectDirs;
use parking_lot::Mutex;
use std::sync::Arc;
use tauri::Manager;

fn open_database(app: &tauri::App) -> Result<db::Db, String> {
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    // Protect the whole directory because SQLite creates token-bearing sidecars lazily.
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
        // Remove foreign sidecars before SQLite can replay them over the migrated database.
        for suffix in ["-wal", "-shm"] {
            let mut dst_side = db_path.as_os_str().to_os_string();
            dst_side.push(suffix);
            match std::fs::remove_file(std::path::PathBuf::from(dst_side)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        std::fs::rename(&tmp, db_path)?;
        for (from, to) in &staged {
            std::fs::rename(from, to)?;
        }
        Ok(())
    })();
    if let Err(e) = &result {
        log::warn!("legacy DB migration failed: {e}");
        let _ = std::fs::remove_file(&tmp);
        for (from, _) in &staged {
            let _ = std::fs::remove_file(from);
        }
        // Leave the destination free so a failed migration can retry next launch.
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

    // Disable DMA-BUF to avoid Mesa crashes on exit. KURISU_DMABUF opts back in.
    if std::env::var_os("KURISU_DMABUF").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // One instance prevents duplicate trackers and competing writes to the same database.
    let instance_guard = match std::net::TcpListener::bind(SINGLE_INSTANCE_ADDR) {
        Ok(l) => Some(l),
        Err(_) => {
            if poke_running_instance() {
                eprintln!("kurisu: already running; raising the existing window");
                return;
            }
            log::warn!(
                "single instance port {SINGLE_INSTANCE_ADDR} held by a process that is not Kurisu; starting without the guard"
            );
            None
        }
    };

    // Answer before database setup so another launch cannot mistake slow startup for a dead listener.
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
        .invoke_handler(tauri::generate_handler![
            commands::get_client_id,
            commands::set_client_id,
            commands::get_redirect_uri,
            commands::set_redirect_uri,
            commands::is_logged_in,
            commands::login_with_token,
            commands::login_oauth,
            commands::logout,
            commands::current_user,
            commands::search_anime,
            commands::get_season,
            commands::get_recommendations,
            commands::get_media,
            commands::get_media_detail,
            commands::get_airing_schedule,
            commands::sync_my_list,
            commands::local_entries,
            commands::get_entry,
            commands::update_entry,
            commands::set_progress,
            commands::delete_entry_cmd,
            commands::get_notifications,
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
            commands::unbind_library_media,
            commands::get_rss_feeds,
            commands::add_rss_feed,
            commands::remove_rss_feed,
            commands::fetch_torrents,
            commands::mark_torrents_seen,
            commands::search_torrents,
            commands::get_user_stats,
            commands::check_update,
            commands::install_update,
            updater::take_update_failed,
            updater::take_pending_update,
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
            use tauri_plugin_dialog::DialogExt;

            if let Err(e) = updater::init_install_path() {
                log::warn!("updater: {e}");
            }
            INSTANCE_HANDLE
                .set(app.handle().clone())
                .expect("instance handle set once");
            commands::set_app_handle(app.handle().clone());

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

            let mut anilist = anilist::AniList::new();
            if let Ok(Some(token)) = db.get_setting("anilist_token") {
                if !token.is_empty() {
                    anilist.set_token(Some(token));
                }
            }
            let matchers = recognize::build_matchers(&db);
            app.manage(AppState {
                anilist: Mutex::new(anilist),
                db: std::sync::Arc::new(db),
                user: Mutex::new(None),
                auth_intent: tokio::sync::watch::channel(0).0,
                entry_lock: tokio::sync::Mutex::new(()),
                matchers: Mutex::new(Arc::new(matchers)),
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
                                // Restore before focusing. A minimized window ignores focus requests.
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
                            .state::<AppState>()
                            .db
                            .get_setting("close_to_tray")
                            .ok()
                            .flatten()
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        if tray_available && close_to_tray && w.hide().is_ok() {
                            api.prevent_close();
                        }
                    }
                });
            }

            playback::spawn(app.handle().clone());

            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    let sweep_handle = handle.clone();
                    let update_failed = tokio::task::spawn_blocking(move || {
                        if let Ok(dir) = sweep_handle.path().app_local_data_dir() {
                            updater::sweep_update_leftovers(&dir);
                        }
                        updater::sweep_install_dir()
                    })
                    .await
                    .unwrap_or(false);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    if update_failed {
                        let _ = handle.emit(
                            "kurisu://update-failed",
                            serde_json::json!({
                                "message": updater::FAILED_MESSAGE
                            }),
                        );
                    }
                    if !updater::auto_check_eligible() {
                        return;
                    }
                    let enabled = || {
                        handle
                            .state::<AppState>()
                            .db
                            .get_setting("auto_update")
                            .ok()
                            .flatten()
                            .map(|v| v != "0")
                            .unwrap_or(true)
                    };
                    updater::watch_updates(enabled, |payload| {
                        updater::set_pending_update(payload.clone());
                        let _ = handle.emit("kurisu://update-available", payload);
                    })
                    .await;
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kurisu");
}

#[cfg(test)]
mod tests {
    use super::migrate_legacy_db;

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
