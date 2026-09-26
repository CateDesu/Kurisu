use std::sync::Arc;

use parking_lot::Mutex;
use tauri::State;
use tauri_plugin_opener::OpenerExt;

use crate::anilist::{self, AniList};
use crate::db::Db;
use crate::library;
use crate::models::{
    AiringItem, FeedFailure, LibraryScan, ListEntry, ListStatus, Media, MediaDetail, Notification,
    TorrentFetch, TorrentItem, User, UserStats,
};
use crate::recognize;
use crate::rss;

const TOKEN_KEY: &str = "anilist_token";
const CLIENT_ID_KEY: &str = "anilist_client_id";
const REDIRECT_URI_KEY: &str = "anilist_redirect_uri";
const USERNAME_KEY: &str = "anilist_username";
const USER_ID_KEY: &str = "anilist_user_id";

const DEFAULT_CLIENT_ID: &str = "45266";
/// Must exactly match the redirect URI registered with AniList.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:39417/";
const ALLOWED_REDIRECT_URIS: &[&str] = &["http://127.0.0.1:39417/", "http://localhost:39417/"];

pub struct AppState {
    pub anilist: Mutex<AniList>,
    pub db: std::sync::Arc<Db>,
    pub user: Mutex<Option<User>>,
    /// Hold across entry reads and remote writes to serialize clicks, tracking and session changes.
    pub entry_lock: tokio::sync::Mutex<()>,
    /// Rebuild after list changes that affect recognition.
    pub matchers: Mutex<Arc<Vec<recognize::Matcher>>>,
}

impl AppState {
    pub fn refresh_matchers(&self) {
        *self.matchers.lock() = Arc::new(recognize::build_matchers(&self.db));
    }
}

static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

pub fn set_app_handle(handle: tauri::AppHandle) {
    let _ = APP_HANDLE.set(handle);
}

fn emit_auth_expired() {
    if let Some(handle) = APP_HANDLE.get() {
        use tauri::Emitter as _;
        let _ = handle.emit("kurisu://auth-expired", ());
    }
}

const SESSION_EXPIRED: &str = "Your AniList session expired or was revoked. Please sign in again.";

/// Caller must hold entry_lock to prevent writes restoring the cleared session.
fn clear_rejected_session(state: &AppState) {
    state.anilist.lock().set_token(None);
    *state.user.lock() = None;
    if let Err(e) = state.db.delete_setting(TOKEN_KEY) {
        log::warn!("failed to drop rejected token from db: {e}");
    }
    if let Err(e) = state.db.delete_setting(USERNAME_KEY) {
        log::warn!("failed to drop rejected username from db: {e}");
    }
    if let Err(e) = state.db.delete_setting(USER_ID_KEY) {
        log::warn!("failed to drop rejected user ID from db: {e}");
    }
    if let Err(e) = state.db.clear_entries() {
        log::warn!("failed to clear the cached list of the rejected session: {e}");
    }
    state.refresh_matchers();
    emit_auth_expired();
}

/// Caller must hold entry_lock because a rejected token clears the session.
fn write_err(state: &AppState, e: &anyhow::Error) -> String {
    if anilist::is_auth_rejection(e) {
        clear_rejected_session(state);
        SESSION_EXPIRED.to_string()
    } else {
        e.to_string()
    }
}

/// auto_ask and discord_enabled work independently of the tracking mode.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackingConfig {
    pub mode: String, // off, prompt or auto
    pub prompt_seconds: u64,
    pub auto_percent: u64,
    pub auto_ask: bool,
    /// Empty uses the default MPV socket paths.
    pub mpv_ipc_socket: String,
    pub discord_enabled: bool,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            mode: "off".into(),
            prompt_seconds: 120,
            auto_percent: 80,
            auto_ask: true,
            mpv_ipc_socket: String::new(),
            discord_enabled: true,
        }
    }
}

const TRACKING_MODE_KEY: &str = "tracking_mode";
const TRACKING_PROMPT_KEY: &str = "tracking_prompt_seconds";
const TRACKING_AUTO_KEY: &str = "tracking_auto_percent";
const TRACKING_AUTO_ASK_KEY: &str = "tracking_auto_ask";
const TRACKING_MPV_SOCKET_KEY: &str = "tracking_mpv_socket";
const TRACKING_DISCORD_KEY: &str = "tracking_discord";

impl TrackingConfig {
    pub fn load(db: &Db) -> Self {
        let kv = match db.get_settings_batch(&[
            TRACKING_MODE_KEY,
            TRACKING_PROMPT_KEY,
            TRACKING_AUTO_KEY,
            TRACKING_AUTO_ASK_KEY,
            TRACKING_MPV_SOCKET_KEY,
            TRACKING_DISCORD_KEY,
        ]) {
            Ok(kv) => kv,
            Err(e) => {
                log::warn!("tracking config read failed, using defaults: {e}");
                return Self::default();
            }
        };
        let mode = kv
            .get(TRACKING_MODE_KEY)
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| "off".to_string());
        let prompt_seconds = kv
            .get(TRACKING_PROMPT_KEY)
            .and_then(|s| s.parse().ok())
            .filter(|&s: &u64| s > 0)
            .unwrap_or(120);
        let auto_percent = kv
            .get(TRACKING_AUTO_KEY)
            .and_then(|s| s.parse().ok())
            .filter(|&p: &u64| (1..=100).contains(&p))
            .unwrap_or(80);
        let auto_ask = kv
            .get(TRACKING_AUTO_ASK_KEY)
            .map(|s| s != "0")
            .unwrap_or(true);
        let mpv_ipc_socket = kv.get(TRACKING_MPV_SOCKET_KEY).cloned().unwrap_or_default();
        let discord_enabled = kv
            .get(TRACKING_DISCORD_KEY)
            .map(|s| s != "0")
            .unwrap_or(true);
        Self {
            mode,
            prompt_seconds,
            auto_percent,
            auto_ask,
            mpv_ipc_socket,
            discord_enabled,
        }
    }

    pub fn save(&self, db: &Db) -> Result<(), String> {
        db.set_settings(&[
            (TRACKING_MODE_KEY, &self.mode),
            (TRACKING_PROMPT_KEY, &self.prompt_seconds.to_string()),
            (TRACKING_AUTO_KEY, &self.auto_percent.to_string()),
            (TRACKING_AUTO_ASK_KEY, if self.auto_ask { "1" } else { "0" }),
            (TRACKING_MPV_SOCKET_KEY, self.mpv_ipc_socket.trim()),
            (
                TRACKING_DISCORD_KEY,
                if self.discord_enabled { "1" } else { "0" },
            ),
        ])
        .map_err(|e| e.to_string())
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        matches!(self.mode.as_str(), "prompt" | "auto")
    }
}

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
    let id = id.trim();
    if id.is_empty() || id.len() > 16 || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err("client id must be 1 to 16 digits".to_string());
    }
    state
        .db
        .set_setting(CLIENT_ID_KEY, id)
        .map_err(|e| e.to_string())
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
    if !ALLOWED_REDIRECT_URIS.contains(&uri.as_str()) {
        return Err(format!(
            "redirect URI must be one of: {}",
            ALLOWED_REDIRECT_URIS.join(", ")
        ));
    }
    state
        .db
        .set_setting(REDIRECT_URI_KEY, &uri)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_tracking_config(state: State<'_, AppState>) -> TrackingConfig {
    TrackingConfig::load(&state.db)
}

#[tauri::command]
pub fn set_tracking_config(
    mode: String,
    prompt_seconds: u64,
    auto_percent: u64,
    auto_ask: bool,
    mpv_ipc_socket: String,
    discord_enabled: bool,
    state: State<'_, AppState>,
) -> Result<TrackingConfig, String> {
    let normalized_mode = match mode.as_str() {
        "prompt" | "auto" => mode,
        _ => "off".to_string(),
    };
    let mpv_ipc_socket = mpv_ipc_socket.trim().chars().take(512).collect::<String>();
    let cfg = TrackingConfig {
        mode: normalized_mode,
        prompt_seconds: prompt_seconds.clamp(1, 3_600),
        auto_percent: auto_percent.clamp(1, 100),
        auto_ask,
        mpv_ipc_socket,
        discord_enabled,
    };
    cfg.save(&state.db)?;
    Ok(cfg)
}

#[tauri::command]
pub fn is_logged_in(state: State<'_, AppState>) -> bool {
    state.anilist.lock().has_token()
}

/// Keep credentials out of generic settings access.
const APP_SETTING_KEYS: &[&str] = &["close_to_tray", "auto_update"];

fn check_app_setting_key(key: &str) -> Result<(), String> {
    if APP_SETTING_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!("not a UI setting key: {key}"))
    }
}

#[tauri::command]
pub fn get_app_setting(key: String, state: State<'_, AppState>) -> Result<Option<String>, String> {
    check_app_setting_key(&key)?;
    Ok(state.db.get_setting(&key).ok().flatten())
}

#[tauri::command]
pub fn set_app_setting(
    key: String,
    value: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    check_app_setting_key(&key)?;
    state
        .db
        .set_setting(&key, &value)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn login_with_token(token: String, state: State<'_, AppState>) -> Result<User, String> {
    // Keep the current token until the replacement passes verification.
    let mut probe = state.anilist.lock().clone();
    probe.set_token(Some(token.clone()));
    let user = probe.viewer().await.map_err(|e| e.to_string())?;
    let _write = state.entry_lock.lock().await;
    let account_changed = state
        .db
        .replace_account(&token, &user)
        .map_err(|e| e.to_string())?;
    state.anilist.lock().set_token(Some(token));
    *state.user.lock() = Some(user.clone());
    if account_changed {
        state.refresh_matchers();
    }
    Ok(user)
}

#[tauri::command]
pub async fn login_oauth(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<User, String> {
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
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())?;
    let token = tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "Timed out waiting for AniList to redirect.".to_string())?
        .map_err(|_| "OAuth callback channel closed.".to_string())?;
    login_with_token(token, state).await
}

#[tauri::command]
pub async fn logout(state: State<'_, AppState>) -> Result<(), String> {
    let _write = state.entry_lock.lock().await;
    state.anilist.lock().set_token(None);
    *state.user.lock() = None;
    let db = state.db.clone();
    let scrub = tokio::task::spawn_blocking(move || db.scrub_setting(TOKEN_KEY))
        .await
        .map_err(|e| e.to_string())?;
    let scrub_failure = scrub.err();
    if scrub_failure.is_some() {
        let _ = state.db.set_setting(TOKEN_KEY, "");
        state
            .db
            .delete_setting(TOKEN_KEY)
            .map_err(|e| e.to_string())?;
    }
    state.db.clear_entries().map_err(|e| e.to_string())?;
    state
        .db
        .delete_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?;
    state
        .db
        .delete_setting(USER_ID_KEY)
        .map_err(|e| e.to_string())?;
    state.refresh_matchers();
    if let Some(e) = scrub_failure {
        return Err(format!(
            "signed out, but the token could not be scrubbed from the local database: {e}"
        ));
    }
    Ok(())
}

#[tauri::command]
pub async fn current_user(state: State<'_, AppState>) -> Result<Option<User>, String> {
    let al = {
        let _write = state.entry_lock.lock().await;
        let al = state.anilist.lock().clone();
        if !al.has_token() {
            return Ok(None);
        }
        if let Some(u) = state.user.lock().clone() {
            return Ok(Some(u));
        }
        al
    };
    let token_used = al.token();
    let result = al.viewer().await;
    let _write = state.entry_lock.lock().await;
    if state.anilist.lock().token() != token_used {
        return Ok(state.user.lock().clone());
    }
    if let Some(user) = state.user.lock().clone() {
        return Ok(Some(user));
    }
    let u = match result {
        Ok(u) => {
            // AniList list queries use the username, which can change after login.
            if !u.name.is_empty()
                && state.db.get_setting(USERNAME_KEY).ok().flatten().as_deref()
                    != Some(u.name.as_str())
            {
                let _ = state.db.set_setting(USERNAME_KEY, &u.name);
            }
            u
        }
        // Keep cached data accessible during an outage.
        Err(e) => {
            if anilist::is_auth_rejection(&e) {
                clear_rejected_session(state.inner());
                return Ok(None);
            }
            let name = state
                .db
                .get_setting(USERNAME_KEY)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| e.to_string())?;
            // Do not cache the offline placeholder. Retry the real profile when connectivity returns.
            return Ok(Some(User {
                name,
                ..Default::default()
            }));
        }
    };
    *state.user.lock() = Some(u.clone());
    Ok(Some(u))
}

#[tauri::command]
pub async fn search_anime(query: String, state: State<'_, AppState>) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
    let media = al.search(&query, 25).await.map_err(|e| e.to_string())?;
    let _ = state.db.upsert_media_batch(&media);
    Ok(media)
}

#[tauri::command]
pub async fn get_season(
    season: String,
    year: i64,
    state: State<'_, AppState>,
) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
    let media = al
        .season_all(&season, year)
        .await
        .map_err(|e| e.to_string())?;
    let _ = state.db.upsert_media_batch(&media);
    Ok(media)
}

#[tauri::command]
pub async fn get_recommendations(
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
    let media = al
        .recommendations(media_id)
        .await
        .map_err(|e| e.to_string())?;
    let _ = state.db.upsert_media_batch(&media);
    Ok(media)
}

#[tauri::command]
pub async fn get_media(id: i64, state: State<'_, AppState>) -> Result<Media, String> {
    if let Some(m) = state.db.get_media(id).map_err(|e| e.to_string())? {
        return Ok(m);
    }
    let al = state.anilist.lock().clone();
    let v = match al.media_by_id(id).await {
        Ok(v) => v,
        Err(e) if anilist::media_not_found(&e) => return Err(MEDIA_GONE.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    state.db.upsert_media(&v).map_err(|e| e.to_string())?;
    Ok(v)
}

#[tauri::command]
pub async fn get_media_detail(id: i64, state: State<'_, AppState>) -> Result<MediaDetail, String> {
    let al = state.anilist.lock().clone();
    match al.media_detail(id).await {
        Ok((media, relations, characters, staff)) => {
            let _ = state.db.upsert_media_detail(&media);
            let _ = state.db.upsert_media_batch(
                &relations
                    .iter()
                    .map(|r| r.media.clone())
                    .collect::<Vec<_>>(),
            );
            Ok(MediaDetail {
                media,
                relations,
                characters,
                staff,
            })
        }
        Err(e) if anilist::media_not_found(&e) => {
            let _ = state.db.delete_entry(id);
            let _ = state.db.delete_media(id);
            state.refresh_matchers();
            Err(MEDIA_GONE.to_string())
        }
        Err(e) => match state.db.get_media(id).map_err(|e| e.to_string())? {
            Some(media) => Ok(MediaDetail {
                media,
                relations: vec![],
                characters: vec![],
                staff: vec![],
            }),
            None => Err(e.to_string()),
        },
    }
}

#[tauri::command]
pub async fn get_airing_schedule(
    start: i64,
    end: i64,
    state: State<'_, AppState>,
) -> Result<Vec<AiringItem>, String> {
    if end <= start || end - start > 15 * 86_400 {
        return Err("invalid schedule range".to_string());
    }
    let al = state.anilist.lock().clone();
    let items = al
        .airing_schedule(start, end)
        .await
        .map_err(|e| e.to_string())?;
    let on_list: std::collections::HashSet<i64> = state
        .db
        .entry_media_ids()
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    for item in &items {
        if on_list.contains(&item.media.id) {
            let _ = state.db.upsert_media(&item.media);
        }
    }
    Ok(items)
}

#[tauri::command]
pub async fn sync_my_list(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    let _write = state.entry_lock.lock().await;
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Err("not logged in".to_string());
    }
    let user_name = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;
    // Hold the lock through the fetch so an older snapshot cannot overwrite a newer save.
    let entries = match al.user_list(&user_name).await {
        Ok(v) => v,
        Err(e) => return Err(write_err(state.inner(), &e)),
    };
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || db.replace_list_snapshot(&entries))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            format!(
                "sync incomplete: the remote list could not be cached, remote deletions were not reconciled: {e}"
            )
        })?;
    let _ = state.db.prune_media_cache(30);
    state.refresh_matchers();
    state.db.entries_with_media().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn local_entries(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || db.entries_with_media())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_entry(media_id: i64, state: State<'_, AppState>) -> Result<Option<ListEntry>, String> {
    state.db.get_entry(media_id).map_err(|e| e.to_string())
}

const MEDIA_GONE: &str =
    "AniList no longer has this anime. It was likely merged into another entry. Search for it to re-add the correct one.";

/// Null fields stay unchanged on AniList. Completing fills progress when the total is known.
#[tauri::command]
pub async fn update_entry(
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    save_entry_inner(state.inner(), media_id, status, progress, score, repeat).await
}

pub async fn save_entry_inner(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
) -> Result<ListEntry, String> {
    let _write = state.entry_lock.lock().await;
    let st = status.as_deref().map(parse_status).transpose()?;
    if status.is_none() && progress.is_none() && score.is_none() && repeat.is_none() {
        return Err("nothing to update".to_string());
    }
    let before = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let start_rewatch = st == Some(ListStatus::Repeating) && progress.is_none();
    let progress = if start_rewatch && before.as_ref().is_some_and(|e| e.status == "COMPLETED") {
        Some(0)
    } else {
        progress
    };
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes);
    let mut progress = progress.map(|p| p.max(0));
    if let (Some(t), Some(p)) = (total, progress.as_mut()) {
        *p = (*p).min(t);
    }
    let mut filled_to_total = false;
    if st == Some(ListStatus::Completed) {
        if let Some(t) = total {
            progress = Some(progress.map_or(t, |p| p.max(t)));
            filled_to_total = true;
        }
    }
    let al = state.anilist.lock().clone();
    // A cache miss may already exist remotely. Preserve its progress, score and repeat count.
    if before.is_none() {
        match al.entry_by_media_id(media_id).await {
            Ok(Some(remote)) => {
                let remote_rewatch = start_rewatch && remote.status.as_deref() == Some("COMPLETED");
                return save_entry_unlocked(
                    state,
                    media_id,
                    status,
                    if remote_rewatch {
                        Some(0)
                    } else {
                        filled_to_total.then_some(progress).flatten()
                    },
                    None,
                    None,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) if anilist::media_not_found(&e) => {
                let _ = state.db.delete_media(media_id);
                state.refresh_matchers();
                return Err(MEDIA_GONE.to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    save_entry_unlocked(state, media_id, status, progress, score, repeat).await
}

/// Caller must hold entry_lock and send only changed fields.
async fn save_entry_unlocked(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
) -> Result<ListEntry, String> {
    let st = status.as_deref().map(parse_status).transpose()?;
    let al = state.anilist.lock().clone();
    let before = state.db.get_entry(media_id).ok().flatten();
    let saved = match al.save_entry(media_id, st, progress, score, repeat).await {
        Ok(s) => s,
        Err(e) if anilist::media_not_found(&e) => {
            let _ = state.db.delete_entry(media_id);
            let _ = state.db.delete_media(media_id);
            state.refresh_matchers();
            return Err(MEDIA_GONE.to_string());
        }
        Err(e) => return Err(write_err(state, &e)),
    };
    let entry = ListEntry {
        id: Some(saved.id),
        media_id,
        status: saved
            .status
            .or(status)
            .unwrap_or_else(|| ListStatus::Current.as_str().to_string()),
        progress: saved.progress.or(progress).unwrap_or(0),
        score: saved.score,
        repeat: saved.repeat.or(repeat).unwrap_or(0),
        updated_at: Some(chrono::Utc::now().timestamp()),
        media: state.db.get_media(media_id).map_err(|e| e.to_string())?,
    };
    state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
    if before.as_ref().map(|b| b.status.as_str()) != Some(entry.status.as_str()) {
        state.refresh_matchers();
    }
    Ok(entry)
}

/// If expected no longer matches, skip the write and return the current entry.
#[tauri::command]
pub async fn set_progress(
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    set_progress_inner(state.inner(), media_id, progress, expected).await
}

pub async fn set_progress_inner(
    state: &AppState,
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
) -> Result<ListEntry, String> {
    let _write = state.entry_lock.lock().await;
    if let Some(exp) = expected {
        let cur = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
        // A missing row was deleted. Never recreate it from a buffered edit.
        let Some(entry) = cur else {
            return Err("the entry is no longer on your list".to_string());
        };
        if entry.progress != exp {
            let mut entry = entry;
            entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
            return Ok(entry);
        }
    } else {
        if state
            .db
            .get_entry(media_id)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err("the entry is no longer on your list".to_string());
        }
    }
    let w = compute_set_progress(state, media_id, progress)?;
    save_entry_unlocked(
        state,
        media_id,
        w.send_status.then_some(w.status),
        Some(w.progress),
        None,
        w.send_repeat.then_some(w.repeat),
    )
    .await
}

pub async fn watcher_set_progress(
    state: &AppState,
    media_id: i64,
    episode: i64,
) -> Result<Option<ListEntry>, String> {
    let _write = state.entry_lock.lock().await;
    let Some(cur) = state.db.get_entry(media_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    if episode <= cur.progress {
        return Ok(None);
    }
    let w = compute_set_progress(state, media_id, episode)?;
    save_entry_unlocked(
        state,
        media_id,
        w.send_status.then_some(w.status),
        Some(w.progress),
        None,
        w.send_repeat.then_some(w.repeat),
    )
    .await
    .map(Some)
}

/// Send status and repeat only when changed to preserve edits made outside Kurisu.
struct ProgressWrite {
    status: String,
    progress: i64,
    repeat: i64,
    send_status: bool,
    send_repeat: bool,
}

fn at_last_episode(total: Option<i64>, progress: i64) -> bool {
    total == Some(progress) && progress > 0
}

/// Caller must hold entry_lock. Overshooting the total clamps progress without completing.
fn compute_set_progress(
    state: &AppState,
    media_id: i64,
    progress: i64,
) -> Result<ProgressWrite, String> {
    let cur = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes);
    let requested = progress.max(0);
    let overshot = total.is_some_and(|t| requested > t);
    let mut progress = requested;
    if let Some(t) = total {
        progress = progress.min(t);
    }
    let prev_status = cur.as_ref().map(|e| e.status.as_str()).unwrap_or("");
    let prev_repeat = cur.as_ref().map(|e| e.repeat).unwrap_or(0);
    let at_end = at_last_episode(total, progress);
    let (status, repeat) = if overshot {
        (prev_status, prev_repeat)
    } else if at_end && prev_status == "REPEATING" {
        (ListStatus::Completed.as_str(), prev_repeat + 1)
    } else if at_end {
        (ListStatus::Completed.as_str(), prev_repeat)
    } else if prev_status.is_empty()
        || prev_status == "COMPLETED"
        || (progress > 0 && prev_status == "PLANNING")
    {
        (ListStatus::Current.as_str(), prev_repeat)
    } else {
        (prev_status, prev_repeat)
    };
    let status = if status.is_empty() {
        ListStatus::Current.as_str()
    } else {
        status
    };
    Ok(ProgressWrite {
        status: status.to_string(),
        progress,
        repeat,
        send_status: status != prev_status,
        send_repeat: repeat != prev_repeat,
    })
}

#[tauri::command]
pub async fn delete_entry_cmd(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    let _write = state.entry_lock.lock().await;
    if let Some(entry) = state.db.get_entry(media_id).map_err(|e| e.to_string())? {
        if let Some(id) = entry.id {
            let al = state.anilist.lock().clone();
            let deleted = al
                .delete_entry(id)
                .await
                .map_err(|e| write_err(state.inner(), &e))?;
            if !deleted {
                // A deleted and re-added entry has a new ID. Find and delete the live copy too.
                match al.entry_by_media_id(media_id).await {
                    Ok(Some(live)) => {
                        al.delete_entry(live.id)
                            .await
                            .map_err(|e| write_err(state.inner(), &e))?;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(write_err(state.inner(), &e)),
                }
            }
        }
    }
    state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
    state.refresh_matchers();
    Ok(())
}

#[tauri::command]
pub fn get_library_folders(state: State<'_, AppState>) -> Vec<String> {
    library::get_folders(&state.db)
}

#[tauri::command]
pub fn add_library_folder(path: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    library::add_folder(&state.db, &path).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_library_folder(
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    library::remove_folder(&state.db, &path).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn scan_library(state: State<'_, AppState>) -> Result<LibraryScan, String> {
    let folders = library::get_folders(&state.db);
    let bindings = library::get_bindings(&state.db);
    let matchers = state.matchers.lock().clone();
    tokio::task::spawn_blocking(move || library::scan_paths(&folders, &matchers, &bindings))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn bind_library_path(
    path: String,
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if state
        .db
        .get_entry(media_id)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("only shows on your list can be linked".to_string());
    }
    library::bind_path(&state.db, &path, media_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn library_binding_for(path: String, state: State<'_, AppState>) -> Option<i64> {
    library::binding_for_exact(&state.db, &path)
}

#[tauri::command]
pub fn unbind_library_media(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    library::unbind_media(&state.db, media_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_rss_feeds(state: State<'_, AppState>) -> Vec<String> {
    rss::get_feeds(&state.db)
}

#[tauri::command]
pub fn add_rss_feed(url: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    rss::add_feed(&state.db, &url).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn remove_rss_feed(url: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    rss::remove_feed(&state.db, &url).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn fetch_torrents(state: State<'_, AppState>) -> Result<TorrentFetch, String> {
    let feeds = rss::get_feeds(&state.db);
    if feeds.is_empty() {
        return Ok(TorrentFetch::default());
    }
    let fetched = rss::fetch_all(&feeds).await.map_err(|e| e.to_string())?;
    let failures: Vec<FeedFailure> = fetched
        .failures
        .into_iter()
        .map(|f| FeedFailure {
            url: f.url,
            error: f.error,
        })
        .collect();
    let raw = fetched.items;
    let seen = state.db.rss_seen_set().map_err(|e| e.to_string())?;
    let matchers = state.matchers.lock().clone();
    let (progress_by_id, total_by_id): (
        std::collections::HashMap<i64, i64>,
        std::collections::HashMap<i64, Option<i64>>,
    ) = if matchers.is_empty() {
        (Default::default(), Default::default())
    } else {
        let list = state.db.entries_with_media().map_err(|e| e.to_string())?;
        let p = list.iter().map(|e| (e.media_id, e.progress)).collect();
        let t = list
            .into_iter()
            .map(|e| (e.media_id, e.media.as_ref().and_then(|m| m.episodes)))
            .collect();
        (p, t)
    };
    let mut items: Vec<TorrentItem> = raw
        .into_iter()
        .map(|r| {
            let matched = recognize::match_title(&matchers, &r.title, "");
            let episode = matched.and_then(|m| recognize::resolve_episode(m, &[r.title.as_str()]));
            let (progress, total) = match matched {
                Some(m) => (
                    progress_by_id.get(&m.media_id).copied(),
                    total_by_id.get(&m.media_id).copied().flatten(),
                ),
                None => (None, None),
            };
            let was_seen = seen.contains(&r.guid);
            // Episodes beyond the known total may belong to another season. Never flag them as new.
            let within_total = match (episode, total) {
                (Some(ep), Some(t)) => ep <= t,
                _ => true,
            };
            let is_new = !was_seen
                && within_total
                && matches!((episode, progress), (Some(ep), Some(p)) if ep > p);
            TorrentItem {
                magnet: r.info_hash.as_deref().map(|h| rss::magnet_for(h, &r.title)),
                title: r.title,
                link: r.link,
                guid: r.guid,
                size: r.size,
                seeders: r.seeders,
                leechers: r.leechers,
                category_id: r.category_id,
                category: r.category,
                trusted: r.trusted,
                remake: r.remake,
                published: r.published,
                media_id: matched.map(|m| m.media_id),
                matched: matched.map(|m| m.display.clone()),
                episode,
                is_new,
                seen: was_seen,
            }
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.published.unwrap_or(0)));
    // Keep seen marks for items still in the feed so old releases do not become new again.
    let carried: Vec<String> = items.iter().map(|i| i.guid.clone()).collect();
    let _ = state.db.prune_rss_seen_keeping(60, &carried);
    Ok(TorrentFetch { items, failures })
}

#[tauri::command]
pub fn mark_torrents_seen(guids: Vec<String>, state: State<'_, AppState>) -> Result<(), String> {
    state.db.mark_rss_seen(&guids).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn search_torrents(
    query: String,
    category: Option<String>,
    filter: Option<String>,
) -> Result<Vec<TorrentItem>, String> {
    let query = query.trim().to_string();
    if query.is_empty() || query.chars().count() > 200 {
        return Err("empty or overlong search query".to_string());
    }
    let raw = rss::search(
        &query,
        category.as_deref().unwrap_or("1_0"),
        filter.as_deref().unwrap_or("0"),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(raw
        .into_iter()
        .map(|r| TorrentItem {
            magnet: r.info_hash.as_deref().map(|h| rss::magnet_for(h, &r.title)),
            title: r.title,
            link: r.link,
            guid: r.guid,
            size: r.size,
            seeders: r.seeders,
            leechers: r.leechers,
            category_id: r.category_id,
            category: r.category,
            trusted: r.trusted,
            remake: r.remake,
            published: r.published,
            media_id: None,
            matched: None,
            episode: None,
            is_new: false,
            seen: false,
        })
        .collect())
}

#[tauri::command]
pub async fn get_user_stats(state: State<'_, AppState>) -> Result<UserStats, String> {
    let user_name = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;
    let al = state.anilist.lock().clone();
    al.user_statistics(&user_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_notifications(state: State<'_, AppState>) -> Result<Vec<Notification>, String> {
    let al = state.anilist.lock().clone();
    al.notifications().await.map_err(|e| e.to_string())
}

static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    let rel = crate::updater::fetch_latest_release().await?;
    let available = crate::updater::is_newer(&rel.version, crate::updater::current_version());
    let can_install = crate::updater::platform_asset(&rel).is_some();
    Ok(serde_json::json!({
        "available": available,
        "can_install": can_install,
        "restart_pending": crate::updater::update_applied(),
        "version": rel.version,
        "tag": rel.tag,
        "html_url": rel.html_url,
        "body": rel.body,
        "current": crate::updater::current_version(),
    }))
}

#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    // Concurrent installs would share and truncate the same scratch files.
    let _install = INSTALL_LOCK
        .try_lock()
        .map_err(|_| "an update is already in progress".to_string())?;
    // After a swap, the running executable is older than the installed one.
    if crate::updater::update_applied() {
        return Err("an update was already installed; restart Kurisu to finish".to_string());
    }
    let rel = crate::updater::fetch_latest_release().await?;
    // Recheck here because the release may have changed since the prompt.
    if !crate::updater::is_newer(&rel.version, crate::updater::current_version()) {
        return Err("the latest release is not newer than this build".to_string());
    }
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
        let dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
        let dir2 = dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir2))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".kurisu-update-{}-{asset}", std::process::id()));
        let sidecar = crate::updater::fetch_sidecar(&rel, &asset)
            .await
            .ok_or_else(|| {
                "no SHA-256 checksum available for this release; refusing to install unverified"
                    .to_string()
            })?;
        crate::updater::download(&url, &tmp).await?;

        // Verify and install from the same open handle so the file cannot be swapped between them.
        let verify = async {
            let tmp2 = tmp.clone();
            match tokio::task::spawn_blocking(move || {
                crate::updater::verify_and_open(&tmp2, &sidecar)
            })
            .await
            {
                Ok(Ok(Some(f))) => Ok(f),
                Ok(Ok(None)) => Err("update failed integrity check (SHA-256 mismatch)".to_string()),
                Ok(Err(e)) => Err(format!("could not verify the download: {e}")),
                Err(e) => Err(format!("could not verify the download: {e}")),
            }
        }
        .await;
        let verified = match verify {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };

        #[cfg(windows)]
        let outcome = (|| {
            // Keep the verified handle open through spawn to prevent installer replacement.
            if let Err(e) = std::process::Command::new(&tmp).spawn() {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("could not launch the installer: {e}"));
            }
            drop(verified);
            let handle = app.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                handle.exit(0);
            });
            Ok("restarting".to_string())
        })();
        #[cfg(target_os = "linux")]
        let outcome = {
            let mut verified = verified;
            let result = tokio::task::spawn_blocking(move || {
                crate::updater::apply_linux_update(&mut verified)
            })
            .await
            .map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        AppState {
            anilist: Mutex::new(AniList::new()),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).expect("in-memory db")),
            user: Mutex::new(None),
            entry_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
        }
    }

    fn seed(state: &AppState, episodes: Option<i64>, status: &str, progress: i64, repeat: i64) {
        state
            .db
            .upsert_media(&Media {
                id: 1,
                episodes,
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(10),
                media_id: 1,
                status: status.into(),
                progress,
                score: None,
                repeat,
                updated_at: None,
                media: None,
            })
            .unwrap();
    }

    #[test]
    fn finishing_a_rewatch_bumps_repeat() {
        let state = test_state();
        seed(&state, Some(12), "REPEATING", 11, 2);
        let w = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!(
            (w.status.as_str(), w.progress, w.repeat),
            ("COMPLETED", 12, 3)
        );
        assert!(w.send_status && w.send_repeat);
    }

    #[test]
    fn finishing_first_watch_keeps_repeat() {
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 11, 0);
        let w = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!(
            (w.status.as_str(), w.progress, w.repeat),
            ("COMPLETED", 12, 0)
        );
        assert!(w.send_status && !w.send_repeat);
    }

    #[test]
    fn rewinding_past_the_end_reopens_completed() {
        let state = test_state();
        seed(&state, Some(12), "COMPLETED", 12, 1);
        let w = compute_set_progress(&state, 1, 5).unwrap();
        assert_eq!((w.status.as_str(), w.repeat), ("CURRENT", 1));
        let state = test_state();
        seed(&state, Some(12), "REPEATING", 6, 2);
        let w = compute_set_progress(&state, 1, 4).unwrap();
        assert_eq!((w.status.as_str(), w.repeat), ("REPEATING", 2));
    }

    #[test]
    fn rewinding_completed_to_zero_reopens() {
        let state = test_state();
        seed(&state, Some(12), "COMPLETED", 12, 0);
        let w = compute_set_progress(&state, 1, 0).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("CURRENT", 0));
        assert!(w.send_status);
    }

    #[test]
    fn advancing_progress_moves_planning_to_current() {
        let state = test_state();
        seed(&state, Some(12), "PLANNING", 0, 0);
        let w = compute_set_progress(&state, 1, 1).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("CURRENT", 1));
        assert!(w.send_status);
        let w = compute_set_progress(&state, 1, 0).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("PLANNING", 0));
        assert!(!w.send_status);
    }

    #[test]
    fn an_overshoot_past_the_total_clamps_without_completing() {
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 5, 0);
        let w = compute_set_progress(&state, 1, 265).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("CURRENT", 12));
        assert!(!w.send_status && !w.send_repeat);
        let state = test_state();
        seed(&state, Some(12), "COMPLETED", 12, 0);
        let w = compute_set_progress(&state, 1, 13).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("COMPLETED", 12));
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 11, 0);
        let w = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("COMPLETED", 12));
    }

    #[test]
    fn a_plain_advance_sends_only_progress() {
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 5, 1);
        let w = compute_set_progress(&state, 1, 6).unwrap();
        assert_eq!((w.status.as_str(), w.progress, w.repeat), ("CURRENT", 6, 1));
        assert!(!w.send_status && !w.send_repeat);
    }

    #[test]
    fn zero_episode_show_does_not_auto_complete() {
        assert!(!at_last_episode(Some(0), 0));
        assert!(at_last_episode(Some(12), 12));
        assert!(!at_last_episode(Some(12), 11));
        assert!(!at_last_episode(None, 3));
    }
}
