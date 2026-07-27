//! Tauri entrypoint. Builds the app state (AniList client + SQLite cache), restores
//! any saved token, registers all commands, and starts the playback watcher.

mod anilist;
mod commands;
mod db;
mod library;
mod models;
mod playback;
mod recognize;
mod rss;
mod updater;

use commands::AppState;
use directories::ProjectDirs;
use parking_lot::Mutex;
use std::sync::Arc;
use tauri::Manager;

/// Resolve the app-data dir, migrate any pre-1.0 ProjectDirs DB, and open the
/// SQLite cache. String errors get surfaced in a startup dialog by the caller.
fn open_database(app: &tauri::App) -> Result<db::Db, String> {
    // All app data lives under Tauri's app-data dir (derived from the bundle
    // identifier), so backup/reset touches ONE path. Pre-1.0 builds kept the DB
    // under ProjectDirs (e.g. ~/.local/share/kurisu); migrate it over — but
    // never clobber a real DB already at the new path (an empty placeholder
    // left by an old WebKit run is fair game).
    let data_dir = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    // The DB holds the AniList token in plaintext. Keep the WHOLE data dir
    // owner-only: the -wal/-shm sidecars are created lazily at the first write
    // with the process umask, so chmodding the db file at open time misses
    // them for the entire session.
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
        let target_free = std::fs::metadata(&db_path).map(|m| m.len() == 0).unwrap_or(true);
        let legacy_has_data = std::fs::metadata(&legacy).map(|m| m.len() > 0).unwrap_or(false);
        if target_free && legacy_has_data {
            // Copy to a temp name and rename into place: a mid-copy crash must
            // not leave a truncated "real" DB (its presence would block every
            // future migration attempt). Sidecars ride along so writes sitting
            // in an un-checkpointed legacy WAL survive the move.
            let tmp = data_dir.join(".kurisu-migrate.tmp");
            // Stage EVERYTHING first, then publish the main database LAST. The
            // sidecars used to be renamed into place before it: a failure after
            // that point (or a crash) left a -wal/-shm pair belonging to a
            // database that was not there, and SQLite treats an orphaned WAL
            // beside a fresh DB as corruption.
            let mut staged: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
            let result = (|| -> std::io::Result<()> {
                std::fs::copy(&legacy, &tmp)?;
                for suffix in ["-wal", "-shm"] {
                    let mut side = legacy.as_os_str().to_os_string();
                    side.push(suffix);
                    let side = std::path::PathBuf::from(side);
                    if side.exists() {
                        let mut tmp_side = tmp.as_os_str().to_os_string();
                        tmp_side.push(suffix);
                        let mut dst_side = db_path.as_os_str().to_os_string();
                        dst_side.push(suffix);
                        let (tmp_side, dst_side) =
                            (std::path::PathBuf::from(tmp_side), std::path::PathBuf::from(dst_side));
                        std::fs::copy(&side, &tmp_side)?;
                        staged.push((tmp_side, dst_side));
                    }
                }
                // Publish the database first, then its sidecars. If a sidecar
                // rename fails now, the DB is still valid on its own: SQLite
                // rebuilds a missing -shm and an absent -wal just means the
                // un-checkpointed tail is lost, not that the file is unreadable.
                std::fs::rename(&tmp, &db_path)?;
                for (from, to) in &staged {
                    std::fs::rename(from, to)?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                log::warn!("legacy DB migration failed: {e}");
                let _ = std::fs::remove_file(&tmp);
                for (from, _) in &staged {
                    let _ = std::fs::remove_file(from);
                }
            }
        }
    }
    db::Db::open(&db_path).map_err(|e| format!("cannot open {}: {e}", db_path.display()))
}

/// Loopback address the running instance holds for its whole lifetime. A second
/// launch fails to bind it, which is how it learns it is the second launch.
/// Adjacent to the OAuth callback port (39417) so the two stay together.
const SINGLE_INSTANCE_ADDR: &str = "127.0.0.1:39418";

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

    // Single instance. Two Kurisu processes share ONE SQLite file with separate
    // in-memory state and separate playback watchers, so they overwrite each
    // other's writes and can double-push progress to AniList. Close-to-tray makes
    // a second launch easy: the first process is alive but has no window, so
    // clicking the launcher again looks like the app "didn't start".
    //
    // The guard is a loopback bind rather than a lock file (no stale lock to
    // clean up if the process is killed) and rather than a plugin (no new
    // dependency). Same shape as the OAuth callback listener already here.
    let instance_guard = match std::net::TcpListener::bind(SINGLE_INSTANCE_ADDR) {
        Ok(l) => l,
        Err(_) => {
            // Someone already holds it: poke them so their window comes back,
            // then exit quietly. A connect failure means the port belongs to an
            // unrelated process, in which case carrying on is better than
            // refusing to start at all.
            let _ = std::net::TcpStream::connect(SINGLE_INSTANCE_ADDR);
            eprintln!("kurisu: already running; raising the existing window");
            return;
        }
    };

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
            commands::bind_library_path,
            commands::unbind_library_media,
            commands::get_rss_feeds,
            commands::add_rss_feed,
            commands::remove_rss_feed,
            commands::fetch_torrents,
            commands::mark_torrents_seen,
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

            // Capture the exe path before anything can swap it: after a
            // successful Linux in-place update, /proc/self/exe follows the
            // renamed inode and a current_exe() at apply time would point at
            // the .kurisu-old backup.
            if let Err(e) = updater::init_install_path() {
                log::warn!("updater: {e}");
            }

            let db = match open_database(app) {
                Ok(db) => db,
                Err(e) => {
                    // No DB = no app. Tell the user WHY (a bare panic only shows
                    // on a console nobody watches), then exit cleanly.
                    eprintln!("kurisu: cannot start: {e}");
                    app.dialog()
                        .message(format!("Kurisu cannot start.\n\n{e}"))
                        .title("Kurisu — startup error")
                        .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                        .blocking_show();
                    std::process::exit(1);
                }
            };

            // Restore a saved token so the app starts logged in.
            let mut anilist = anilist::AniList::new();
            if let Ok(Some(token)) = db.get_setting("anilist_token") {
                if !token.is_empty() {
                    anilist.set_token(Some(token));
                }
            }
            // Seed the recognizer matcher cache from the just-opened DB.
            let matchers = recognize::build_matchers(&db);
            app.manage(AppState {
                anilist: Mutex::new(anilist),
                db,
                user: Mutex::new(None),
                entry_lock: tokio::sync::Mutex::new(()),
                matchers: Mutex::new(Arc::new(matchers)),
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
                            // unminimize FIRST: show() does not de-iconify, and
                            // set_focus() is a no-op on a minimized window, so a
                            // window minimized to the taskbar could not be
                            // brought back from the tray at all.
                            let _ = w.unminimize();
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
                            // A minimized window is still "visible" to Tauri, so
                            // toggling one used to HIDE it to the tray instead of
                            // restoring it: the click appeared to do nothing.
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

            // Hold the single-instance listener for the process lifetime and
            // answer later launches by raising this window. The OS drops the
            // binding when the process dies, so there is no stale state to clear.
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    for stream in instance_guard.incoming() {
                        drop(stream); // the connection itself is the whole message
                        if let Some(w) = handle.get_webview_window("main") {
                            let _ = w.unminimize();
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                });
            }

            // Background MPRIS2 playback watcher. Runs for the app's lifetime; every
            // tick swallows its own errors, so a flaky player can't crash detection.
            playback::spawn(app.handle().clone());

            // Startup update check — CI-stamped builds only (locally compiled
            // builds skip it: they report the base version and would nag about
            // every newer rolling build during development). Settings →
            // Updates can turn it off (default on); a manual check there works
            // on any build. Emits `kurisu://update-available` when a newer
            // release ships an asset this platform can install.
            if updater::is_ci_build() {
                use tauri::Emitter;
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // The leftover sweep is blocking filesystem I/O — keep it off
                    // the async worker threads.
                    let sweep_handle = handle.clone();
                    let update_failed = tokio::task::spawn_blocking(move || {
                        if let Ok(dir) = sweep_handle.path().app_local_data_dir() {
                            updater::sweep_update_leftovers(&dir);
                        }
                        // Sweep next to the exe and flag the doubly-failed-swap
                        // marker if one exists. The marker file stays on disk
                        // until the frontend acknowledges the notice via
                        // take_update_failed — the emit below is only a fast
                        // path and can fire before the webview listens.
                        updater::sweep_install_dir()
                    })
                    .await
                    .unwrap_or(false);
                    // Let the window settle before emitting / hitting the network.
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    if update_failed {
                        let _ = handle.emit(
                            "kurisu://update-failed",
                            serde_json::json!({
                                "message": updater::FAILED_MESSAGE
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
                            let payload = serde_json::json!({
                                "available": true,
                                "can_install": true,
                                "version": rel.version,
                                "tag": rel.tag,
                                "html_url": rel.html_url,
                                "body": rel.body,
                                "current": updater::current_version(),
                            });
                            // Stash it for the frontend's pull on mount
                            // (take_pending_update): the emit is a one-shot
                            // fast path a slow-booting webview can miss.
                            updater::set_pending_update(payload.clone());
                            let _ = handle.emit("kurisu://update-available", payload);
                        }
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running kurisu");
}
