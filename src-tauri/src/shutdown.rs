use parking_lot::Mutex;
use tauri::{Emitter, Manager};

#[derive(Default)]
pub struct Shutdown(Mutex<Status>);

#[derive(Default)]
struct Status {
    ready: bool,
    requested: bool,
    approved: bool,
    exit_code: i32,
}

#[tauri::command]
pub fn shutdown_ready(state: tauri::State<'_, Shutdown>) {
    state.0.lock().ready = true;
}

pub fn request(app: &tauri::AppHandle, exit_code: i32) -> bool {
    let state = app.state::<Shutdown>();
    let mut status = state.0.lock();
    if !status.ready || status.approved {
        return false;
    }
    if !status.requested {
        status.requested = true;
        status.exit_code = exit_code;
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
        }
        if let Err(error) = app.emit("kurisu://shutdown-requested", ()) {
            status.requested = false;
            log::error!("could not request pending saves before exit: {error}");
        }
    }
    true
}

#[tauri::command]
pub fn finish_shutdown(success: bool, app: tauri::AppHandle, state: tauri::State<'_, Shutdown>) {
    let mut status = state.0.lock();
    if !status.requested {
        return;
    }
    status.requested = false;
    status.approved = success;
    let code = status.exit_code;
    drop(status);
    if success {
        app.exit(code);
    }
}
