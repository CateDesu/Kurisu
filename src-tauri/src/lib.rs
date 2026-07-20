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
    // WebKit2GTK's DMA-BUF renderer crashes in Mesa/GBM teardown on exit on many
    // Wayland setups (SIGSEGV in dri_gbm.so during process exit). The long-standing
    // workaround is to disable it and fall back to the stable path — at the cost of
    // choppier scrolling (software raster). Set KURISU_DMABUF=1 to keep the
    // hardware renderer for smooth scrolling, if your Mesa no longer crashes on exit.
    if std::env::var_os("KURISU_DMABUF").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let proj = ProjectDirs::from("com", "catedesu", "kurisu")
        .expect("no home directory for app data");
    let db_path = proj.data_local_dir().join("kurisu.db");
    let db = db::Db::open(&db_path).expect("failed to open kurisu.db");

    // Restore a saved token so the app starts logged in.
    let mut anilist = anilist::AniList::new();
    if let Ok(Some(token)) = db.get_setting("anilist_token") {
        if !token.is_empty() {
            anilist.set_token(Some(token));
        }
    }

    let state = AppState {
        anilist: Mutex::new(anilist),
        db,
        user: Mutex::new(None),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
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
            // Windows only: CI ships no Linux/macOS asset, so elsewhere the check
            // would only nag. Emits `kurisu://update-available` when a newer
            // installer is published; the UI prompts from there.
            #[cfg(target_os = "windows")]
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok(dir) = handle.path().app_local_data_dir() {
                        updater::sweep_update_leftovers(&dir);
                    }
                    // Let the window settle before hitting the network.
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                        if updater::installer_asset(&rel).is_some()
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
