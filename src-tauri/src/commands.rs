//! Tauri commands: the only surface the frontend can call. Each is a thin wrapper
//! over the AniList client + DB cache. State (AniList client, DB, current user) is
//! held behind std Mutex; async commands CLONE the AniList client out of the lock
//! before awaiting so the futures stay Send (Tauri's hard requirement).

use std::sync::Mutex;

use tauri::State;
use tauri_plugin_opener::OpenerExt;

use crate::anilist::{self, AniList};
use crate::db::Db;
use crate::library;
use crate::models::{LibraryFile, ListEntry, ListStatus, Media, Notification, User};
use crate::recognize;

const TOKEN_KEY: &str = "anilist_token";
const CLIENT_ID_KEY: &str = "anilist_client_id";
const REDIRECT_URI_KEY: &str = "anilist_redirect_uri";
const USERNAME_KEY: &str = "anilist_username";

/// The project's registered AniList client id (public OAuth identifier). Override
/// per-install in Settings if you register a different client.
const DEFAULT_CLIENT_ID: &str = "45266";
/// The redirect URI the callback server answers on. MUST byte-match the redirect
/// URI registered for the AniList client. The callback server always binds
/// 127.0.0.1:39417 and reads the token from the query of any path.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:39417/";

pub struct AppState {
    pub anilist: Mutex<AniList>,
    pub db: Db,
    pub user: Mutex<Option<User>>,
}

/// Playback tracking configuration. All three are stored in the settings table and
/// surfaced in the Settings tab. Mode defaults to `off` (opt-in) since tracking
/// edits the user's live AniList list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackingConfig {
    pub mode: String,            // "off" | "prompt" | "auto"
    pub prompt_seconds: u64,     // prompt mode: how long playback must run before asking
    pub auto_percent: u64,       // auto mode: watched percentage that triggers a +1
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self { mode: "off".into(), prompt_seconds: 120, auto_percent: 80 }
    }
}

const TRACKING_MODE_KEY: &str = "tracking_mode";
const TRACKING_PROMPT_KEY: &str = "tracking_prompt_seconds";
const TRACKING_AUTO_KEY: &str = "tracking_auto_percent";

impl TrackingConfig {
    pub fn load(db: &Db) -> Self {
        let mode = db
            .get_setting(TRACKING_MODE_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "off".to_string());
        let prompt_seconds = db
            .get_setting(TRACKING_PROMPT_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .filter(|&s: &u64| s > 0)
            .unwrap_or(120);
        let auto_percent = db
            .get_setting(TRACKING_AUTO_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .filter(|&p: &u64| (1..=100).contains(&p))
            .unwrap_or(80);
        Self { mode, prompt_seconds, auto_percent }
    }

    pub fn save(&self, db: &Db) -> Result<(), String> {
        db.set_setting(TRACKING_MODE_KEY, &self.mode).map_err(|e| e.to_string())?;
        db.set_setting(TRACKING_PROMPT_KEY, &self.prompt_seconds.to_string()).map_err(|e| e.to_string())?;
        db.set_setting(TRACKING_AUTO_KEY, &self.auto_percent.to_string()).map_err(|e| e.to_string())?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        matches!(self.mode.as_str(), "prompt" | "auto")
    }
}

// ─────────────────────────── settings / auth ───────────────────────────

#[tauri::command]
pub fn get_client_id(state: State<'_, AppState>) -> Option<String> {
    Some(
        state
            .db
            .get_setting(CLIENT_ID_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string()),
    )
}

#[tauri::command]
pub fn set_client_id(id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.db.set_setting(CLIENT_ID_KEY, &id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_redirect_uri(state: State<'_, AppState>) -> Option<String> {
    Some(
        state
            .db
            .get_setting(REDIRECT_URI_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string()),
    )
}

#[tauri::command]
pub fn set_redirect_uri(uri: String, state: State<'_, AppState>) -> Result<(), String> {
    state
        .db
        .set_setting(REDIRECT_URI_KEY, &uri)
        .map_err(|e| e.to_string())
}

// ─────────────────────────── tracking config ───────────────────────────

#[tauri::command]
pub fn get_tracking_config(state: State<'_, AppState>) -> TrackingConfig {
    TrackingConfig::load(&state.db)
}

#[tauri::command]
pub fn set_tracking_config(
    mode: String,
    prompt_seconds: u64,
    auto_percent: u64,
    state: State<'_, AppState>,
) -> Result<TrackingConfig, String> {
    let normalized_mode = match mode.as_str() {
        "prompt" | "auto" => mode,
        _ => "off".to_string(),
    };
    let cfg = TrackingConfig {
        mode: normalized_mode,
        prompt_seconds: prompt_seconds.clamp(1, 3_600),
        auto_percent: auto_percent.clamp(1, 100),
    };
    cfg.save(&state.db)?;
    Ok(cfg)
}

#[tauri::command]
pub fn is_logged_in(state: State<'_, AppState>) -> bool {
    state.anilist.lock().unwrap().has_token()
}

/// Generic key/value settings access for small UI toggles (close-to-tray, …).
/// No whitelist: the settings table is local-only and every key is namespaced by
/// the caller, same trust level as the existing tracking config.
#[tauri::command]
pub fn get_app_setting(key: String, state: State<'_, AppState>) -> Option<String> {
    state.db.get_setting(&key).ok().flatten()
}

#[tauri::command]
pub fn set_app_setting(key: String, value: String, state: State<'_, AppState>) -> Result<(), String> {
    state.db.set_setting(&key, &value).map_err(|e| e.to_string())
}

/// Manual token entry (fallback if the browser flow can't be used). Verifies the
/// token via Viewer, then persists it.
#[tauri::command]
pub async fn login_with_token(token: String, state: State<'_, AppState>) -> Result<User, String> {
    {
        let mut a = state.anilist.lock().unwrap();
        a.set_token(Some(token.clone()));
    }
    let al = state.anilist.lock().unwrap().clone();
    let user = match al.viewer().await {
        Ok(u) => u,
        Err(e) => {
            // Don't leave the rejected token in memory: is_logged_in would keep
            // reporting true and every API call would fail until restart.
            state.anilist.lock().unwrap().set_token(None);
            return Err(e.to_string());
        }
    };
    state.db.set_setting(TOKEN_KEY, &token).map_err(|e| e.to_string())?;
    state.db.set_setting(USERNAME_KEY, &user.name).map_err(|e| e.to_string())?;
    *state.user.lock().unwrap() = Some(user.clone());
    Ok(user)
}

/// Browser OAuth2 (implicit) flow. Starts the localhost callback server, opens the
/// AniList authorize page, waits (up to 5 min) for the redirect — the callback
/// yields the access token directly (no client_secret, no code exchange).
#[tauri::command]
pub async fn login_oauth(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<User, String> {
    let client_id = state
        .db
        .get_setting(CLIENT_ID_KEY)
        .map_err(|e| e.to_string())?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    let redirect_uri = state
        .db
        .get_setting(REDIRECT_URI_KEY)
        .map_err(|e| e.to_string())?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());
    let (oauth_state, rx) = anilist::start_callback_server().map_err(|e| e.to_string())?;
    let url = anilist::authorize_url(&client_id, &redirect_uri, &oauth_state);
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())?;
    // Implicit flow: the callback yields the access token itself (no exchange step).
    let token = tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "Timed out waiting for AniList to redirect.".to_string())?
        .map_err(|_| "OAuth callback channel closed.".to_string())?
        .map_err(|e| format!("AniList denied access: {}", e))?;
    login_with_token(token, state).await
}

#[tauri::command]
pub fn logout(state: State<'_, AppState>) {
    state.anilist.lock().unwrap().set_token(None);
    let _ = state.db.set_setting(TOKEN_KEY, "");
    *state.user.lock().unwrap() = None;
}

#[tauri::command]
pub async fn current_user(state: State<'_, AppState>) -> Result<Option<User>, String> {
    // Cached user wins: a name/avatar change on AniList only shows after
    // logout/login (no re-fetch per call). Acceptable for a single-user app.
    if let Some(u) = state.user.lock().unwrap().clone() {
        return Ok(Some(u));
    }
    let al = state.anilist.lock().unwrap().clone();
    if !al.has_token() {
        return Ok(None);
    }
    let u = al.viewer().await.map_err(|e| e.to_string())?;
    *state.user.lock().unwrap() = Some(u.clone());
    Ok(Some(u))
}

// ───────────────────────────── anime / list ─────────────────────────────

#[tauri::command]
pub async fn search_anime(query: String, state: State<'_, AppState>) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().unwrap().clone();
    let media = al.search(&query, 25).await.map_err(|e| e.to_string())?;
    for m in &media {
        let _ = state.db.upsert_media(m);
    }
    Ok(media)
}

/// One anime season (the `/seasons` browser). Results are cached like search hits.
#[tauri::command]
pub async fn get_season(
    season: String,
    year: i64,
    page: i64,
    state: State<'_, AppState>,
) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().unwrap().clone();
    let media = al.season(&season, year, page).await.map_err(|e| e.to_string())?;
    for m in &media {
        let _ = state.db.upsert_media(m);
    }
    Ok(media)
}

/// Community recommendations for a title (the edit modal's "also like" strip).
#[tauri::command]
pub async fn get_recommendations(media_id: i64, state: State<'_, AppState>) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().unwrap().clone();
    let media = al.recommendations(media_id).await.map_err(|e| e.to_string())?;
    for m in &media {
        let _ = state.db.upsert_media(m);
    }
    Ok(media)
}

#[tauri::command]
pub async fn get_media(id: i64, state: State<'_, AppState>) -> Result<Media, String> {
    if let Some(m) = state.db.get_media(id).map_err(|e| e.to_string())? {
        return Ok(m);
    }
    let al = state.anilist.lock().unwrap().clone();
    let v = al
        .search(&id.to_string(), 5)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| "media not found".to_string())?;
    state.db.upsert_media(&v).map_err(|e| e.to_string())?;
    Ok(v)
}

/// Sync the user's remote list into the local cache, then return the local view.
#[tauri::command]
pub async fn sync_my_list(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    let user_name = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;
    let al = state.anilist.lock().unwrap().clone();
    let entries = al.user_list(&user_name).await.map_err(|e| e.to_string())?;
    for e in &entries {
        if let Some(m) = &e.media {
            let _ = state.db.upsert_media(m);
        }
        let _ = state.db.upsert_entry(e);
    }
    state.db.entries_with_media().map_err(|e| e.to_string())
}

/// Offline/local view of the cached list (no network).
#[tauri::command]
pub fn local_entries(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    state.db.entries_with_media().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_entry(media_id: i64, state: State<'_, AppState>) -> Result<Option<ListEntry>, String> {
    state.db.get_entry(media_id).map_err(|e| e.to_string())
}

/// Add or update an entry. Pushes to AniList, mirrors to the local cache.
#[tauri::command]
pub async fn update_entry(
    media_id: i64,
    status: String,
    progress: i64,
    score: Option<f64>,
    repeat: i64,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    save_entry_inner(state.inner(), media_id, status, progress, score, repeat).await
}

/// Shared push-to-AniList + mirror-to-cache used by the `update_entry` command and
/// the playback watcher. Takes `&AppState` so it works outside the command layer.
pub async fn save_entry_inner(
    state: &AppState,
    media_id: i64,
    status: String,
    progress: i64,
    score: Option<f64>,
    repeat: i64,
) -> Result<ListEntry, String> {
    let st = parse_status(&status)?;
    let al = state.anilist.lock().unwrap().clone();
    let entry_id = al
        .save_entry(media_id, st, progress, score, repeat)
        .await
        .map_err(|e| e.to_string())?;
    let entry = ListEntry {
        id: Some(entry_id),
        media_id,
        status: status.clone(),
        progress,
        score,
        repeat,
        updated_at: Some(chrono::Utc::now().timestamp()),
        media: state.db.get_media(media_id).map_err(|e| e.to_string())?,
    };
    state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
    Ok(entry)
}

/// Increment progress by one (the "+1 episode" button). Skips past total if known.
#[tauri::command]
pub async fn increment_episode(
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    increment_inner(state.inner(), media_id).await
}

/// Shared +1 logic used by the `increment_episode` command and the watcher's
/// auto-increment. Clamps to the known episode count and auto-completes at the last
/// episode, mirroring the command's visible behavior exactly.
pub async fn increment_inner(state: &AppState, media_id: i64) -> Result<ListEntry, String> {
    let cur = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let mut progress = cur.as_ref().map(|e| e.progress).unwrap_or(0) + 1;
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes).unwrap_or(i64::MAX);
    if progress > total {
        progress = total;
    }
    let status = cur
        .as_ref()
        .map(|e| e.status.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ListStatus::Current.as_str().to_string());
    // auto-complete when we hit the last episode
    let final_status = if media.as_ref().and_then(|m| m.episodes) == Some(progress) {
        ListStatus::Completed.as_str().to_string()
    } else {
        status
    };
    let score = cur.as_ref().and_then(|e| e.score);
    let repeat = cur.as_ref().map(|e| e.repeat).unwrap_or(0);
    save_entry_inner(state, media_id, final_status, progress, score, repeat).await
}

/// Set absolute episode progress (the list's −/+ stepper). Preserves the current
/// status + score, clamps to the known episode count, auto-completes at the last
/// episode, and drops a Completed entry back to Current if you rewind past the end.
#[tauri::command]
pub async fn set_progress(
    media_id: i64,
    progress: i64,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    set_progress_inner(state.inner(), media_id, progress).await
}

pub async fn set_progress_inner(state: &AppState, media_id: i64, progress: i64) -> Result<ListEntry, String> {
    let cur = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes);
    let mut progress = progress.max(0);
    if let Some(t) = total {
        progress = progress.min(t);
    }
    let prev_status = cur.as_ref().map(|e| e.status.as_str()).unwrap_or("");
    let final_status = if total == Some(progress) && progress > 0 {
        ListStatus::Completed.as_str().to_string()
    } else if prev_status == "COMPLETED" || prev_status.is_empty() {
        ListStatus::Current.as_str().to_string()
    } else {
        prev_status.to_string()
    };
    let score = cur.as_ref().and_then(|e| e.score);
    let repeat = cur.as_ref().map(|e| e.repeat).unwrap_or(0);
    save_entry_inner(state, media_id, final_status, progress, score, repeat).await
}

#[tauri::command]
pub async fn delete_entry_cmd(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    if let Some(entry) = state.db.get_entry(media_id).map_err(|e| e.to_string())? {
        if let Some(id) = entry.id {
            let al = state.anilist.lock().unwrap().clone();
            let _ = al.delete_entry(id).await;
        }
    }
    state.db.delete_entry(media_id).map_err(|e| e.to_string())
}

// ───────────────────────────── library ─────────────────────────────

#[tauri::command]
pub fn get_library_folders(state: State<'_, AppState>) -> Vec<String> {
    library::get_folders(&state.db)
}

#[tauri::command]
pub fn add_library_folder(path: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    library::add_folder(&state.db, &path).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_library_folder(path: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    library::remove_folder(&state.db, &path).map_err(|e| e.to_string())
}

/// Scan the configured folders for video files and recognize them against the
/// cached list. Matchers/folders are read from the DB up front; the filesystem
/// walk itself runs on a blocking thread.
#[tauri::command]
pub async fn scan_library(state: State<'_, AppState>) -> Result<Vec<LibraryFile>, String> {
    let folders = library::get_folders(&state.db);
    let matchers = recognize::build_matchers(&state.db);
    tokio::task::spawn_blocking(move || library::scan_paths(&folders, &matchers))
        .await
        .map_err(|e| e.to_string())
}

// ───────────────────────────── notifications ─────────────────────────────

#[tauri::command]
pub async fn get_notifications(state: State<'_, AppState>) -> Result<Vec<Notification>, String> {
    let al = state.anilist.lock().unwrap().clone();
    al.notifications().await.map_err(|e| e.to_string())
}

// ───────────────────────────── self-update ─────────────────────────────

/// Check GitHub for a newer release. Returns `{available, can_install, version,
/// tag, html_url, body, current}`: `available` = a newer release exists;
/// `can_install` = the release ships an asset this platform can install
/// (NSIS installer on Windows, bare binary on Linux). Other platforms get
/// `can_install: false` and update manually from the release page.
#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    let rel = crate::updater::fetch_latest_release().await?;
    let available = crate::updater::is_newer(&rel.version, crate::updater::current_version());
    let can_install = crate::updater::platform_asset(&rel).is_some();
    Ok(serde_json::json!({
        "available": available,
        "can_install": can_install,
        "version": rel.version,
        "tag": rel.tag,
        "html_url": rel.html_url,
        "body": rel.body,
        "current": crate::updater::current_version(),
    }))
}

/// Download, verify, and install the latest release. Windows: the verified
/// NSIS installer is launched and the app quits ("restarting"). Linux: the
/// running binary is swapped in place ("installed"; the UI prompts a restart).
/// Fails closed on a checksum problem.
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    let rel = crate::updater::fetch_latest_release().await?;
    let asset = crate::updater::platform_asset(&rel)
        .ok_or_else(|| "the latest release has no build for this platform".to_string())?
        .to_string();
    let url = rel
        .assets
        .get(&asset)
        .cloned()
        .ok_or_else(|| "the latest release has no build for this platform".to_string())?;

    #[cfg(any(windows, target_os = "linux"))]
    {
        use tauri::Manager;
        // Download into the app-data dir (always user-writable) under a
        // pid-unique name; swept on the next launch.
        let dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".kurisu-update-{}-{asset}", std::process::id()));
        crate::updater::download(&url, &tmp).await?;

        // Verify against the published `.sha256` sidecar and FAIL CLOSED: an
        // unverifiable download is refused, never installed.
        let verify = async {
            let sidecar = crate::updater::fetch_sidecar(&rel, &asset)
                .await
                .ok_or_else(|| "no SHA-256 checksum available for this release; refusing to install unverified".to_string())?;
            match crate::updater::verify_sha256(&tmp, &sidecar) {
                Ok(true) => Ok(()),
                Ok(false) => Err("update failed integrity check (SHA-256 mismatch)".to_string()),
                Err(e) => Err(format!("could not verify the download: {e}")),
            }
        }
        .await;
        if let Err(e) = verify {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }

        #[cfg(windows)]
        let outcome = (|| {
            // Hand off: launch the installer, then quit so it can overwrite us.
            std::process::Command::new(&tmp)
                .spawn()
                .map_err(|e| format!("could not launch the installer: {e}"))?;
            let handle = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                handle.exit(0);
            });
            Ok("restarting".to_string())
        })();
        #[cfg(target_os = "linux")]
        let outcome = {
            let result = crate::updater::apply_linux_update(&tmp);
            let _ = std::fs::remove_file(&tmp);
            result.map(|_| "installed".to_string())
        };
        outcome
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (app, url);
        Err("in-app update is not supported on this platform".to_string())
    }
}

fn parse_status(s: &str) -> Result<ListStatus, String> {
    Ok(match s.to_uppercase().as_str() {
        "CURRENT" | "WATCHING" => ListStatus::Current,
        "PLANNING" | "PLAN_TO_WATCH" => ListStatus::Planning,
        "COMPLETED" => ListStatus::Completed,
        "PAUSED" => ListStatus::Paused,
        "DROPPED" => ListStatus::Dropped,
        "REPEATING" => ListStatus::Repeating,
        other => return Err(format!("unknown list status: {}", other)),
    })
}
