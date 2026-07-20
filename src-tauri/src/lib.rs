//! Tauri entrypoint. Builds the app state (AniList client + SQLite cache), restores
//! any saved token, registers all commands, and starts the playback watcher.

mod anilist;
mod commands;
mod db;
mod library;
mod models;
mod playback;
mod recognize;
mod updater;

use commands::AppState;
use directories::ProjectDirs;
use std::sync::Mutex;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logger for the `log` facade calls (playback tick diagnostics): our crate
    // at debug, deps at info. Override with RUST_LOG; stderr lands in the
    // systemd user journal on most desktops.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("kurisu_lib=debug,info"))
        .init();

    // WebKit2GTK's DMA-BUF renderer crashes in Mesa/GBM teardown on exit on many
    // Wayland setups (SIGSEGV in dri_gbm.so during process exit). The long-standing
    // workaround is to disable it and fall back to the stable path — at the cost of
    // choppier scrolling (software raster). Set KURISU_DMABUF=1 to keep the
    // hardware renderer for smooth scrolling, if your Mesa no longer crashes on exit.
    if std::env::var_os("KURISU_DMABUF").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
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
            commands::sync_my_list,
            commands::local_entries,
            commands::get_entry,
            commands::update_entry,
            commands::increment_episode,
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
            commands::check_update,
            commands::install_update,
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            // All app data lives under Tauri's app-data dir (derived from the
            // bundle identifier), so backup/reset touches ONE path. Pre-1.0
            // builds kept the DB under ProjectDirs (e.g. ~/.local/share/kurisu);
            // migrate it over — but never clobber a real DB already at the new
            // path (an empty placeholder left by an old WebKit run is fair game).
            let data_dir = app.path().app_local_data_dir().expect("no app data dir");
            std::fs::create_dir_all(&data_dir).expect("cannot create app data dir");
            let db_path = data_dir.join("kurisu.db");
            if let Some(legacy) = ProjectDirs::from("com", "catedesu", "kurisu")
                .map(|p| p.data_local_dir().join("kurisu.db"))
                .filter(|p| p != &db_path)
            {
                let target_free = std::fs::metadata(&db_path).map(|m| m.len() == 0).unwrap_or(true);
                let legacy_has_data = std::fs::metadata(&legacy).map(|m| m.len() > 0).unwrap_or(false);
                if target_free && legacy_has_data {
                    let _ = std::fs::copy(&legacy, &db_path);
                }
            }
            let db = db::Db::open(&db_path).expect("failed to open kurisu.db");

            // Restore a saved token so the app starts logged in.
            let mut anilist = anilist::AniList::new();
            if let Ok(Some(token)) = db.get_setting("anilist_token") {
                if !token.is_empty() {
                    anilist.set_token(Some(token));
                }
            }
            app.manage(AppState {
                anilist: Mutex::new(anilist),
                db,
                user: Mutex::new(None),
            });

            let show = MenuItem::with_id(app, "show", "Show Kurisu", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(
                    app.default_window_icon()
                        .cloned()
                        .expect("default window icon missing"),
                )
                .tooltip("Kurisu")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Left-click toggles the window; right-click opens the menu.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            if w.is_visible().unwrap_or(false) {
                                let _ = w.hide();
                            } else {
                                let _ = w.show();
                                let _ = w.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // The window close button quits by default (this being the only window,
            // closing ends the app). The Settings toggle (`close_to_tray = 1`) makes
            // it hide to the tray instead — Quit then lives in the tray menu.
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
                        if close_to_tray {
                            api.prevent_close();
                            let _ = w.hide();
                        }
                    }
                });
            }

            // Background MPRIS2 playback watcher. Runs for the app's lifetime; every
            // tick swallows its own errors, so a flaky player can't crash detection.
            playback::spawn(app.handle().clone());

            // Startup update check (Settings → Updates can turn it off; default on).
            // Emits `kurisu://update-available` when a newer release ships an
            // asset this platform can install; the UI prompts from there.
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(dir) = handle.path().app_local_data_dir() {
                        updater::sweep_update_leftovers(&dir);
                    }
                    // A leftover failure marker means a previous update's swap
                    // AND its rollback both failed; the user only gets here by
                    // manually restoring the backup. Surface it once.
                    let mut update_failed = false;
                    if let Ok(exe) = std::env::current_exe() {
                        if let Some(dir) = exe.parent() {
                            updater::sweep_install_leftovers(dir);
                            let marker = dir.join(updater::FAILED_MARKER);
                            if marker.exists() {
                                let _ = std::fs::remove_file(&marker);
                                update_failed = true;
                            }
                        }
                    }
                    // Let the window settle before emitting / hitting the network.
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    if update_failed {
                        let _ = handle.emit(
                            "kurisu://update-failed",
                            serde_json::json!({
                                "message": "The last update failed to install cleanly, so the previous version was kept. Nothing was lost — you can retry the update from Settings."
                            }),
                        );
                    }
                    let enabled = handle
                        .state::<AppState>()
                        .db
                        .get_setting("auto_update")
                        .ok()
                        .flatten()
                        .map(|v| v != "0")
                        .unwrap_or(true);
                    if !enabled {
                        return;
                    }
                    if let Ok(rel) = updater::fetch_latest_release().await {
                        if updater::platform_asset(&rel).is_some()
                            && updater::is_newer(&rel.version, updater::current_version())
                        {
                            let _ = handle.emit(
                                "kurisu://update-available",
                                serde_json::json!({
                                    "available": true,
                                    "can_install": true,
                                    "version": rel.version,
                                    "tag": rel.tag,
                                    "html_url": rel.html_url,
                                    "body": rel.body,
                                    "current": updater::current_version(),
                                }),
                            );
                        }
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kurisu");
}
