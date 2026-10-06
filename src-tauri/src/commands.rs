use std::sync::Arc;

use parking_lot::Mutex;
use tauri::State;
use tauri_plugin_opener::OpenerExt;

use crate::anilist::{self, AniList};
use crate::db::Db;
use crate::library;
use crate::models::{
    AiringItem, EntryDetails, FuzzyDate, LibraryScan, ListEntry, ListStatus, Media, MediaDetail,
    NotificationPage, SearchPage, ShowTorrents, TorrentFetch, TorrentItem, User, UserStats,
};
use crate::recognize;
use crate::rss;

const TOKEN_KEY: &str = "anilist_token";
const CLIENT_ID_KEY: &str = "anilist_client_id";
const REDIRECT_URI_KEY: &str = "anilist_redirect_uri";
const USERNAME_KEY: &str = "anilist_username";
const USER_ID_KEY: &str = "anilist_user_id";
const PROFILE_KEY: &str = "anilist_profile";

const DEFAULT_CLIENT_ID: &str = "45266";
/// Must exactly match the redirect URI registered with AniList.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:39417/";

pub struct AppState {
    pub anilist: Mutex<AniList>,
    pub db: std::sync::Arc<Db>,
    pub user: Mutex<Option<User>>,
    pub auth_intent: tokio::sync::watch::Sender<u64>,
    /// Protect local entry and account transitions. Never hold during network I/O.
    pub entry_lock: tokio::sync::Mutex<()>,
    /// Serialize remote mutations, snapshots and account replacement.
    pub remote_lock: tokio::sync::Mutex<()>,
    /// Rebuild after list changes that affect recognition.
    pub matchers: Mutex<Arc<Vec<recognize::Matcher>>>,
    pub library_cache: Arc<Mutex<library::ScanCache>>,
}

impl AppState {
    pub fn refresh_matchers(&self) {
        *self.matchers.lock() = Arc::new(recognize::build_matchers(&self.db));
    }

    fn begin_auth_intent(&self) -> u64 {
        let mut intent = 0;
        self.auth_intent.send_modify(|current| {
            *current = current.wrapping_add(1);
            intent = *current;
        });
        intent
    }

    fn check_auth_intent(&self, intent: u64) -> Result<(), String> {
        if *self.auth_intent.borrow() == intent {
            Ok(())
        } else {
            Err(AUTH_REPLACED.into())
        }
    }

    async fn auth_intent_changed(&self, intent: u64) {
        let mut changes = self.auth_intent.subscribe();
        let _ = changes.wait_for(|current| *current != intent).await;
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
const AUTH_REPLACED: &str = "A newer sign-in or sign-out replaced this attempt.";

fn check_write_session(state: &AppState, expected_token: &Option<String>) -> Result<(), String> {
    if state.anilist.lock().token() != *expected_token {
        return Err("Your AniList account changed. Try again.".into());
    }
    Ok(())
}

/// Caller holds remote_lock and entry_lock so in-flight writes cannot restore the session.
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
    if let Err(e) = state.db.delete_setting(PROFILE_KEY) {
        log::warn!("failed to drop rejected profile from db: {e}");
    }
    if let Err(e) = state.db.clear_entries() {
        log::warn!("failed to clear the cached list of the rejected session: {e}");
    }
    state.refresh_matchers();
    emit_auth_expired();
}

/// Caller holds remote_lock and entry_lock when handling an auth rejection.
pub(crate) fn write_err(state: &AppState, e: &anyhow::Error) -> String {
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
const APP_SETTING_KEYS: &[&str] = &["close_to_tray", "auto_update", "hardware_acceleration"];

fn check_app_setting_key(key: &str) -> Result<(), String> {
    if APP_SETTING_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!("not a UI setting key: {key}"))
    }
}

#[tauri::command]
pub fn get_app_setting(key: String, state: State<'_, AppState>) -> Result<Option<String>, String> {
    read_app_setting(&state.db, &key)
}

fn read_app_setting(db: &Db, key: &str) -> Result<Option<String>, String> {
    check_app_setting_key(key)?;
    db.get_setting(key).map_err(|e| e.to_string())
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
    let intent = state.begin_auth_intent();
    login_with_token_inner(token, state.inner(), intent).await
}

async fn login_with_token_inner(
    token: String,
    state: &AppState,
    intent: u64,
) -> Result<User, String> {
    state.check_auth_intent(intent)?;
    // Keep the current token until the replacement passes verification.
    let mut probe = state.anilist.lock().clone();
    probe.set_token(Some(token.clone()));
    let user = tokio::select! {
        result = probe.viewer() => result.map_err(|e| e.to_string())?,
        _ = state.auth_intent_changed(intent) => return Err(AUTH_REPLACED.into()),
    };
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    let current_intent = state.auth_intent.borrow();
    if *current_intent != intent {
        return Err(AUTH_REPLACED.into());
    }
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
    let intent = state.begin_auth_intent();
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
    let (oauth_state, _, rx) =
        start_oauth_callback(state.inner(), intent, anilist::OAUTH_PORT).await?;
    let url = anilist::authorize_url(&client_id, &redirect_uri, &oauth_state);
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())?;
    let token = wait_for_oauth_token(state.inner(), intent, rx).await?;
    login_with_token_inner(token, state.inner(), intent).await
}

async fn start_oauth_callback(
    state: &AppState,
    intent: u64,
    port: u16,
) -> Result<(String, u16, anilist::OAuthReceiver), String> {
    for attempt in 0..20 {
        state.check_auth_intent(intent)?;
        match anilist::start_callback_server_on(port) {
            Ok(listener) => return Ok(listener),
            Err(error)
                if attempt < 19
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse) =>
            {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
                    _ = state.auth_intent_changed(intent) => return Err(AUTH_REPLACED.into()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!()
}

async fn wait_for_oauth_token(
    state: &AppState,
    intent: u64,
    receiver: anilist::OAuthReceiver,
) -> Result<String, String> {
    state.check_auth_intent(intent)?;
    let token = tokio::select! {
        result = tokio::time::timeout(std::time::Duration::from_secs(300), receiver) => result
            .map_err(|_| "Timed out waiting for AniList to redirect.".to_string())?
            .map_err(|_| "OAuth callback channel closed.".to_string())?,
        _ = state.auth_intent_changed(intent) => Err(AUTH_REPLACED.into()),
    }?;
    state.check_auth_intent(intent)?;
    Ok(token)
}

#[tauri::command]
pub async fn logout(state: State<'_, AppState>) -> Result<(), String> {
    let intent = state.begin_auth_intent();
    logout_inner(state.inner(), intent).await
}

async fn logout_inner(state: &AppState, intent: u64) -> Result<(), String> {
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    state.check_auth_intent(intent)?;
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
    state
        .db
        .delete_setting(PROFILE_KEY)
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
    current_user_inner(state.inner()).await
}

async fn current_user_inner(state: &AppState) -> Result<Option<User>, String> {
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
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    if state.anilist.lock().token() != token_used {
        return Ok(state.user.lock().clone());
    }
    if let Some(user) = state.user.lock().clone() {
        return Ok(Some(user));
    }
    let u = match result {
        Ok(u) => {
            if state
                .db
                .replace_account(token_used.as_deref().unwrap(), &u)
                .map_err(|e| e.to_string())?
            {
                state.refresh_matchers();
            }
            u
        }
        // Keep cached data accessible during an outage.
        Err(e) => {
            if anilist::is_auth_rejection(&e) {
                clear_rejected_session(state);
                return Ok(None);
            }
            let name = state
                .db
                .get_setting(USERNAME_KEY)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| e.to_string())?;
            let mut user = state
                .db
                .cached_user()
                .map_err(|e| e.to_string())?
                .unwrap_or(User {
                    name,
                    ..Default::default()
                });
            user.offline = true;
            return Ok(Some(user));
        }
    };
    *state.user.lock() = Some(u.clone());
    Ok(Some(u))
}

#[tauri::command]
pub async fn search_anime_page(
    query: String,
    page: i64,
    state: State<'_, AppState>,
) -> Result<SearchPage, String> {
    if query.trim().is_empty() || !(1..=10000).contains(&page) {
        return Err("invalid search or page".into());
    }
    let al = state.anilist.lock().clone();
    let result = al
        .search_page(query.trim(), page, 25)
        .await
        .map_err(|e| e.to_string())?;
    let _write = state.entry_lock.lock().await;
    let _ = state.db.upsert_media_batch(&result.items);
    state.refresh_matchers();
    Ok(result)
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
    let _write = state.entry_lock.lock().await;
    let _ = state.db.upsert_media_batch(&media);
    state.refresh_matchers();
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
    let _write = state.entry_lock.lock().await;
    let _ = state.db.upsert_media_batch(&media);
    state.refresh_matchers();
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
    let _write = state.entry_lock.lock().await;
    state.db.upsert_media(&v).map_err(|e| e.to_string())?;
    state.refresh_matchers();
    Ok(v)
}

#[tauri::command]
pub async fn get_media_detail(id: i64, state: State<'_, AppState>) -> Result<MediaDetail, String> {
    get_media_detail_inner(id, state.inner()).await
}

async fn get_media_detail_inner(id: i64, state: &AppState) -> Result<MediaDetail, String> {
    let al = state.anilist.lock().clone();
    let expected_token = al.token();
    let key = format!("media_detail:{id}");
    let cached = state
        .db
        .get_setting(&key)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<MediaDetail>(&v).ok());
    match al.media_detail(id).await {
        Ok(mut detail) => {
            let _write = state.entry_lock.lock().await;
            let _ = state.db.upsert_media_detail(&detail.media);
            let _ = state.db.upsert_media_batch(
                &detail
                    .relations
                    .iter()
                    .map(|r| r.media.clone())
                    .collect::<Vec<_>>(),
            );
            state.refresh_matchers();
            if let Some(old) = cached {
                for section in &detail.unavailable_sections {
                    match section.as_str() {
                        "relations" => detail.relations = old.relations.clone(),
                        "characters" => detail.characters = old.characters.clone(),
                        "staff" => detail.staff = old.staff.clone(),
                        _ => {}
                    }
                }
            }
            detail.cached_at = Some(chrono::Utc::now().timestamp());
            if let Ok(value) = serde_json::to_string(&detail) {
                let _ = state.db.set_setting(&key, &value);
            }
            Ok(detail)
        }
        Err(e) if anilist::is_auth_rejection(&e) => {
            let _remote = state.remote_lock.lock().await;
            let _guard = state.entry_lock.lock().await;
            check_write_session(state, &expected_token)?;
            Err(write_err(state, &e))
        }
        Err(e) if anilist::media_not_found(&e) => {
            let _remote = state.remote_lock.lock().await;
            let _guard = state.entry_lock.lock().await;
            check_write_session(state, &expected_token)?;
            crate::sync_queue::mark_media_missing(state, id).map_err(|e| e.to_string())?;
            let _ = state.db.delete_entry(id);
            let _ = state.db.delete_media(id);
            let _ = state.db.set_setting(&key, "null");
            state.refresh_matchers();
            Err(MEDIA_GONE.to_string())
        }
        Err(e) => {
            if let Some(mut detail) = cached {
                detail.warning = Some(e.to_string());
                return Ok(detail);
            }
            match state.db.get_media(id).map_err(|e| e.to_string())? {
                Some(media) => Ok(MediaDetail {
                    media,
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: Some(e.to_string()),
                    unavailable_sections: vec![
                        "relations".into(),
                        "characters".into(),
                        "staff".into(),
                    ],
                }),
                None => Err(e.to_string()),
            }
        }
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
    let _write = state.entry_lock.lock().await;
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
    state.refresh_matchers();
    Ok(items)
}

#[tauri::command]
pub async fn sync_my_list(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    sync_my_list_inner(state.inner()).await
}

async fn sync_my_list_inner(state: &AppState) -> Result<Vec<ListEntry>, String> {
    let _remote = state.remote_lock.lock().await;
    crate::sync_queue::flush_unlocked(state).await?;
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Err("not logged in".to_string());
    }
    let user_name = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;
    let result = al.user_list(&user_name).await;
    let _write = state.entry_lock.lock().await;
    let entries = match result {
        Ok(v) => v,
        Err(e) => return Err(write_err(state, &e)),
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
    crate::sync_queue::overlay(state).map_err(|e| e.to_string())?;
    let _ = state.db.prune_media_cache(30);
    state.refresh_matchers();
    state.db.entries_with_media().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn local_entries(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    local_entries_inner(state.inner()).await
}

async fn local_entries_inner(state: &AppState) -> Result<Vec<ListEntry>, String> {
    let _write = state.entry_lock.lock().await;
    crate::sync_queue::overlay(state).map_err(|e| e.to_string())?;
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
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
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
    if st == Some(ListStatus::Completed) {
        if let Some(t) = total {
            progress = Some(progress.map_or(t, |p| p.max(t)));
        }
    }
    validate_entry_values(progress, score, repeat)?;
    let al = state.anilist.lock().clone();
    // A cache miss may already exist remotely.
    if before.is_none() {
        drop(_write);
        let result = al.entry_by_media_id(media_id).await;
        let _write = state.entry_lock.lock().await;
        match result {
            Ok(Some(remote)) => {
                let remote_rewatch = start_rewatch && remote.status == "COMPLETED";
                state
                    .db
                    .upsert_entry(&ListEntry {
                        id: Some(remote.id),
                        media_id,
                        status: remote.status,
                        progress: remote.progress,
                        score: remote.score,
                        repeat: remote.repeat,
                        ..Default::default()
                    })
                    .map_err(|e| e.to_string())?;
                state.refresh_matchers();
                drop(_write);
                return save_entry_unlocked(
                    state,
                    media_id,
                    status,
                    if remote_rewatch { Some(0) } else { progress },
                    score,
                    repeat,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) if anilist::media_not_found(&e) => {
                let _ = state.db.delete_media(media_id);
                state.refresh_matchers();
                return Err(MEDIA_GONE.to_string());
            }
            Err(e) if anilist::is_retryable(&e) => {
                if let Some((account, mut pending, _)) = crate::sync_queue::stage(
                    state,
                    media_id,
                    crate::sync_queue::Patch {
                        status,
                        progress,
                        score,
                        repeat,
                    },
                )
                .map_err(|e| e.to_string())?
                {
                    pending.error = Some(e.to_string());
                    state
                        .db
                        .put_pending(account, &pending)
                        .map_err(|e| e.to_string())?;
                    return crate::sync_queue::local_entry(state, &pending)
                        .map_err(|e| e.to_string());
                }
                return Err(e.to_string());
            }
            Err(e) => return Err(write_err(state, &e)),
        }
    } else {
        drop(_write);
    }
    save_entry_unlocked(state, media_id, status, progress, score, repeat).await
}

/// Caller holds remote_lock and sends only changed fields.
async fn save_entry_unlocked(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
) -> Result<ListEntry, String> {
    let _write = state.entry_lock.lock().await;
    validate_entry_values(progress, score, repeat)?;
    let st = status.as_deref().map(parse_status).transpose()?;
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Err("not logged in".into());
    }
    let before = state.db.get_entry(media_id).ok().flatten();
    let staged = crate::sync_queue::stage(
        state,
        media_id,
        crate::sync_queue::Patch {
            status: status.clone(),
            progress,
            score,
            repeat,
        },
    )
    .map_err(|e| e.to_string())?;
    if let Some((_, pending, true)) = &staged {
        return crate::sync_queue::local_entry(state, pending).map_err(|e| e.to_string());
    }
    let entry_id = before.as_ref().and_then(|entry| entry.id);
    drop(_write);
    let saved = match entry_id {
        Some(id) => al.save_entry_by_id(id, st, progress, score, repeat).await,
        None => al.save_entry(media_id, st, progress, score, repeat).await,
    };
    let _write = state.entry_lock.lock().await;
    let saved = match saved {
        Ok(s) => s,
        Err(e) if entry_id.is_some() && anilist::media_not_found(&e) => {
            let error = "Your AniList entry changed or was removed. Choose which version to keep.";
            if let Some((account, mut pending, _)) = staged {
                pending.conflict = true;
                pending.error = Some(error.into());
                state
                    .db
                    .put_pending(account, &pending)
                    .map_err(|e| e.to_string())?;
                return crate::sync_queue::local_entry(state, &pending).map_err(|e| e.to_string());
            }
            return Err(error.into());
        }
        Err(e) if anilist::media_not_found(&e) => {
            if let Some((account, _, _)) = &staged {
                let _ = state.db.remove_pending(*account, media_id);
            }
            let _ = state.db.delete_entry(media_id);
            let _ = state.db.delete_media(media_id);
            state.refresh_matchers();
            return Err(MEDIA_GONE.to_string());
        }
        Err(e) => {
            if let Some((account, mut pending, _)) = staged {
                if anilist::is_retryable(&e) {
                    pending.error = Some(e.to_string());
                    state
                        .db
                        .put_pending(account, &pending)
                        .map_err(|e| e.to_string())?;
                    return crate::sync_queue::local_entry(state, &pending)
                        .map_err(|e| e.to_string());
                }
                // A concurrent cached read may already have applied the staged patch.
                if let Some(base) = &pending.base {
                    state.db.upsert_entry(base).map_err(|e| e.to_string())?;
                } else {
                    state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
                }
                state
                    .db
                    .remove_pending(account, media_id)
                    .map_err(|e| e.to_string())?;
                state.refresh_matchers();
                crate::sync_queue::changed();
            }
            return Err(write_err(state, &e));
        }
    };
    let entry = ListEntry {
        id: Some(saved.id),
        media_id,
        status: saved.status,
        progress: saved.progress,
        score: saved.score,
        repeat: saved.repeat,
        updated_at: Some(chrono::Utc::now().timestamp()),
        media: state.db.get_media(media_id).map_err(|e| e.to_string())?,
    };
    state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
    if let Some((account, _, _)) = staged {
        state
            .db
            .remove_pending(account, media_id)
            .map_err(|e| e.to_string())?;
    }
    if before.as_ref().map(|b| b.status.as_str()) != Some(entry.status.as_str()) {
        state.refresh_matchers();
    }
    Ok(entry)
}

fn validate_entry_values(
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
) -> Result<(), String> {
    if progress.is_some_and(|value| !(0..=i32::MAX as i64).contains(&value)) {
        return Err("Progress must be between 0 and 2147483647".into());
    }
    if score.is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value)) {
        return Err("Score must be between 0 and 100".into());
    }
    if repeat.is_some_and(|value| !(0..=1000).contains(&value)) {
        return Err("Rewatches must be between 0 and 1000".into());
    }
    Ok(())
}

/// If expected no longer matches, skip the write and return the current entry.
#[tauri::command]
pub async fn set_progress(
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    let entry = set_progress_inner(state.inner(), media_id, progress, expected).await?;
    crate::sync_queue::publish(&entry);
    Ok(entry)
}

pub async fn set_progress_inner(
    state: &AppState,
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
) -> Result<ListEntry, String> {
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
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
    drop(_write);
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
    expected_token: Option<&str>,
) -> Result<Option<ListEntry>, String> {
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    if expected_token.is_none() || state.anilist.lock().token().as_deref() != expected_token {
        return Ok(None);
    }
    let Some(cur) = state.db.get_entry(media_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    if episode <= cur.progress {
        return Ok(None);
    }
    if state
        .db
        .get_media(media_id)
        .map_err(|e| e.to_string())?
        .and_then(|m| m.episodes)
        .is_some_and(|total| episode > total)
    {
        return Err(
            "Detected episode exceeds this show’s total. Adjust its episode offset in Library."
                .into(),
        );
    }
    let w = compute_set_progress(state, media_id, episode)?;
    drop(_write);
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
    delete_entry_inner(media_id, state.inner()).await
}

async fn delete_entry_inner(media_id: i64, state: &AppState) -> Result<(), String> {
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let entry = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let account = crate::sync_queue::account_id(&state.db);
    let queued = if let Some(account) = account {
        state
            .db
            .pending(account)
            .map_err(|e| e.to_string())?
            .iter()
            .any(|p| p.media_id == media_id)
    } else {
        false
    };
    let al = state.anilist.lock().clone();
    drop(_write);
    let id = if let Some(id) = entry.as_ref().and_then(|e| e.id) {
        Some(id)
    } else if entry.is_some() || queued {
        let result = al.entry_by_media_id(media_id).await;
        let _write = state.entry_lock.lock().await;
        match result {
            Ok(remote) => remote.map(|entry| entry.id),
            Err(e) if anilist::media_not_found(&e) => None,
            Err(e) => return Err(write_err(state, &e)),
        }
    } else {
        None
    };
    if let Some(id) = id {
        let result = al.delete_entry(id).await;
        let deleted = {
            let _write = state.entry_lock.lock().await;
            result.map_err(|e| write_err(state, &e))?
        };
        if !deleted {
            let result = al.entry_by_media_id(media_id).await;
            let _write = state.entry_lock.lock().await;
            match result {
                Ok(Some(live)) => {
                    drop(_write);
                    let result = al.delete_entry(live.id).await;
                    let _write = state.entry_lock.lock().await;
                    result.map_err(|e| write_err(state, &e))?;
                }
                Ok(None) => {}
                Err(e) if anilist::media_not_found(&e) => {}
                Err(e) => return Err(write_err(state, &e)),
            }
        }
    }
    let _write = state.entry_lock.lock().await;
    state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
    if let Some(account) = account {
        state
            .db
            .remove_pending(account, media_id)
            .map_err(|e| e.to_string())?;
    }
    state.refresh_matchers();
    Ok(())
}

#[tauri::command]
pub fn get_library_folders(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    library::get_folders(&state.db).map_err(|e| e.to_string())
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
    let folders = library::get_folders(&state.db).map_err(|e| e.to_string())?;
    let bindings = library::get_bindings(&state.db).map_err(|e| e.to_string())?;
    let matchers = state.matchers.lock().clone();
    let cache = state.library_cache.clone();
    tokio::task::spawn_blocking(move || cache.lock().scan(&folders, &matchers, &bindings))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn bind_library_path(
    path: String,
    media_id: i64,
    episode_offset: Option<i64>,
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
    library::bind_path(&state.db, &path, media_id, episode_offset.unwrap_or(0))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn library_binding_for(
    path: String,
    state: State<'_, AppState>,
) -> Result<Option<i64>, String> {
    library::binding_for_exact(&state.db, &path)
        .map(|binding| binding.map(|b| b.media_id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn library_binding_details(
    path: String,
    state: State<'_, AppState>,
) -> Result<Option<library::LibraryBinding>, String> {
    library::effective_binding(&state.db, &path).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_watch_history(
    limit: Option<i64>,
    before: Option<i64>,
    state: State<'_, AppState>,
) -> Result<Vec<crate::sync_queue::WatchHistoryItem>, String> {
    let Some(account) = crate::sync_queue::account_id(&state.db) else {
        return Ok(vec![]);
    };
    state
        .db
        .watch_history(account, limit.unwrap_or(50).clamp(1, 100), before)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn unbind_library_media(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    library::unbind_media(&state.db, media_id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_rss_feeds(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    rss::get_feeds(&state.db).map_err(|e| e.to_string())
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
    let feeds = rss::get_feeds(&state.db).map_err(|e| e.to_string())?;
    if feeds.is_empty() {
        return Ok(TorrentFetch::default());
    }
    let fetched = rss::fetch_all(&feeds).await.map_err(|e| e.to_string())?;
    let failures = fetched.failures;
    let raw = fetched.items;
    let mut carried: Vec<String> = raw.iter().map(crate::show_torrents::stable_guid).collect();
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
            let (matched, episode) = crate::show_torrents::match_feed(&matchers, &r.title);
            let (progress, total) = match matched {
                Some(m) => (
                    progress_by_id.get(&m.media_id).copied(),
                    total_by_id.get(&m.media_id).copied().flatten(),
                ),
                None => (None, None),
            };
            let was_seen = crate::show_torrents::was_seen(&r, &seen);
            // Episodes beyond the known total may belong to another season. Never flag them as new.
            let within_total = match (episode, total) {
                (Some(ep), Some(t)) => ep <= t,
                _ => true,
            };
            let is_new = !was_seen
                && within_total
                && matches!((episode, progress), (Some(ep), Some(p)) if ep > p);
            TorrentItem {
                media_id: matched.map(|m| m.media_id),
                matched: matched.map(|m| m.display.clone()),
                episode,
                is_new,
                seen: was_seen,
                ..crate::show_torrents::torrent_item(r)
            }
        })
        .collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.published.unwrap_or(0)));
    // Keep seen marks for items still in the feed so old releases do not become new again.
    carried.extend(items.iter().map(|i| i.guid.clone()));
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
    state: State<'_, AppState>,
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
    let seen = state.db.rss_seen_set().map_err(|e| e.to_string())?;
    Ok(crate::show_torrents::search_results(raw, &seen))
}

#[tauri::command]
pub async fn find_show_torrents(
    media_id: i64,
    category: Option<String>,
    filter: Option<String>,
    state: State<'_, AppState>,
) -> Result<ShowTorrents, String> {
    let entry = state
        .db
        .get_entry(media_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "This show is no longer on your list".to_string())?;
    let matchers = state.matchers.lock().clone();
    let mut result = crate::show_torrents::find(
        &entry,
        &matchers,
        category.as_deref().unwrap_or("1_0"),
        filter.as_deref().unwrap_or("0"),
    )
    .await
    .map_err(|e| e.to_string())?;
    let seen = state.db.rss_seen_set().map_err(|e| e.to_string())?;
    crate::show_torrents::restore_seen(&mut result, &seen);
    Ok(result)
}

fn cached_feature_user(state: &AppState) -> Result<User, String> {
    if !state.anilist.lock().has_token() {
        return Err("not logged in".into());
    }
    state
        .user
        .lock()
        .clone()
        .or_else(|| state.db.cached_user().ok().flatten())
        .ok_or_else(|| "Reconnect to AniList to load your account profile".into())
}

#[tauri::command]
pub async fn get_user_stats(state: State<'_, AppState>) -> Result<UserStats, String> {
    get_user_stats_inner(&state).await
}

async fn get_user_stats_inner(state: &AppState) -> Result<UserStats, String> {
    let expected_token = state.anilist.lock().token();
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let user = cached_feature_user(state)?;
    let key = format!("user_stats:{}", user.id);
    let al = state.anilist.lock().clone();
    let intent = *state.auth_intent.borrow();
    drop(_guard);
    let result = al.user_statistics(&user.name).await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    state.check_auth_intent(intent)?;
    match result {
        Ok(mut stats) => {
            stats.cached_at = Some(chrono::Utc::now().timestamp());
            if let Ok(value) = serde_json::to_string(&stats) {
                let _ = state.db.set_setting(&key, &value);
            }
            Ok(stats)
        }
        Err(e) if anilist::is_auth_rejection(&e) => Err(write_err(state, &e)),
        Err(e) => {
            let cached = state
                .db
                .get_setting(&key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str::<UserStats>(&v).ok());
            match cached {
                Some(mut stats) => {
                    stats.warning = Some(e.to_string());
                    Ok(stats)
                }
                None => Err(e.to_string()),
            }
        }
    }
}

#[tauri::command]
pub async fn get_notifications_page(
    page: i64,
    state: State<'_, AppState>,
) -> Result<NotificationPage, String> {
    get_notifications_page_inner(page, &state).await
}

async fn get_notifications_page_inner(
    page: i64,
    state: &AppState,
) -> Result<NotificationPage, String> {
    if !(1..=10000).contains(&page) {
        return Err("invalid page".into());
    }
    let expected_token = state.anilist.lock().token();
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let al = state.anilist.lock().clone();
    let intent = *state.auth_intent.borrow();
    drop(_guard);
    let result = al.notifications_page(page).await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    state.check_auth_intent(intent)?;
    result.map_err(|e| write_err(state, &e))
}

#[tauri::command]
pub async fn mark_notifications_read(state: State<'_, AppState>) -> Result<(), String> {
    mark_notifications_read_inner(&state).await
}

async fn mark_notifications_read_inner(state: &AppState) -> Result<(), String> {
    let expected_token = state.anilist.lock().token();
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let al = state.anilist.lock().clone();
    let intent = *state.auth_intent.borrow();
    drop(_guard);
    let result = al.mark_notifications_read().await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    state.check_auth_intent(intent)?;
    result.map_err(|e| write_err(state, &e))
}

#[tauri::command]
pub async fn get_entry_details(
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<EntryDetails, String> {
    get_entry_details_inner(media_id, &state).await
}

async fn get_entry_details_inner(media_id: i64, state: &AppState) -> Result<EntryDetails, String> {
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let user = cached_feature_user(state)?;
    let key = format!("entry_details:{}:{media_id}", user.id);
    let al = state.anilist.lock().clone();
    let intent = *state.auth_intent.borrow();
    drop(_guard);
    let result = al.entry_details(media_id).await;
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    state.check_auth_intent(intent)?;
    match result {
        Ok(mut details) => {
            details.cached_at = Some(chrono::Utc::now().timestamp());
            if let Ok(value) = serde_json::to_string(&details) {
                let _ = state.db.set_setting(&key, &value);
            }
            Ok(details)
        }
        Err(e) if anilist::is_auth_rejection(&e) => Err(write_err(state, &e)),
        Err(e) => {
            let cached = state
                .db
                .get_setting(&key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str::<EntryDetails>(&v).ok());
            match cached {
                Some(mut details) => {
                    details.warning = Some(e.to_string());
                    Ok(details)
                }
                None => Err(e.to_string()),
            }
        }
    }
}

fn validate_entry_date(date: Option<&FuzzyDate>) -> Result<(), String> {
    let Some(date) = date else { return Ok(()) };
    if date.year.is_some_and(|y| !(1..=9999).contains(&y))
        || date.month.is_some_and(|m| !(1..=12).contains(&m))
        || date.day.is_some_and(|d| !(1..=31).contains(&d))
    {
        return Err("Enter a valid viewing date".into());
    }
    if let (Some(month), Some(day)) = (date.month, date.day) {
        if chrono::NaiveDate::from_ymd_opt(
            date.year.unwrap_or(2000) as i32,
            month as u32,
            day as u32,
        )
        .is_none()
        {
            return Err("Enter a valid viewing date".into());
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn update_entry_details(
    media_id: i64,
    notes: Option<String>,
    started_at: Option<FuzzyDate>,
    completed_at: Option<FuzzyDate>,
    custom_lists: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> Result<EntryDetails, String> {
    update_entry_details_inner(
        media_id,
        notes,
        started_at,
        completed_at,
        custom_lists,
        &state,
    )
    .await
}

async fn update_entry_details_inner(
    media_id: i64,
    notes: Option<String>,
    started_at: Option<FuzzyDate>,
    completed_at: Option<FuzzyDate>,
    custom_lists: Option<Vec<String>>,
    state: &AppState,
) -> Result<EntryDetails, String> {
    if notes.as_ref().is_some_and(|v| v.chars().count() > 6000) {
        return Err("Notes must be 6000 characters or fewer".into());
    }
    validate_entry_date(started_at.as_ref())?;
    validate_entry_date(completed_at.as_ref())?;
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _guard = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    let user = cached_feature_user(state)?;
    let al = state.anilist.lock().clone();
    drop(_guard);
    let result = al.entry_details_with_id(media_id).await;
    let _guard = state.entry_lock.lock().await;
    let (entry_id, current) = result.map_err(|e| write_err(state, &e))?;
    if custom_lists.as_ref().is_some_and(|lists| {
        lists
            .iter()
            .any(|name| !current.available_custom_lists.contains(name))
    }) {
        return Err("Custom list names changed on AniList; reload the entry and try again".into());
    }
    drop(_guard);
    let result = al
        .save_entry_details(
            entry_id,
            notes.as_deref(),
            started_at.as_ref(),
            completed_at.as_ref(),
            custom_lists.as_deref(),
        )
        .await;
    let _guard = state.entry_lock.lock().await;
    let mut details = result.map_err(|e| write_err(state, &e))?;
    details.available_custom_lists = current.available_custom_lists;
    details.cached_at = Some(chrono::Utc::now().timestamp());
    if let Ok(value) = serde_json::to_string(&details) {
        let _ = state
            .db
            .set_setting(&format!("entry_details:{}:{media_id}", user.id), &value);
    }
    Ok(details)
}

static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    let rel = crate::updater::fetch_latest_release().await?;
    Ok(crate::updater::update_info(&rel))
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

pub(crate) fn parse_status(s: &str) -> Result<ListStatus, String> {
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

    #[test]
    fn app_settings_distinguish_missing_values_from_read_failures() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert_eq!(read_app_setting(&db, "hardware_acceleration"), Ok(None));
        db.set_setting("hardware_acceleration", "1").unwrap();
        assert_eq!(
            read_app_setting(&db, "hardware_acceleration"),
            Ok(Some("1".into()))
        );
        assert!(read_app_setting(&db, TOKEN_KEY).is_err());
        db.0.lock().execute_batch("DROP TABLE settings").unwrap();
        assert!(read_app_setting(&db, "hardware_acceleration").is_err());
    }

    fn test_state() -> AppState {
        AppState {
            anilist: Mutex::new(AniList::new()),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).expect("in-memory db")),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
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

#[cfg(test)]
mod regression_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn cached_edits_target_the_saved_entry_identity() {
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{
                "SaveMediaListEntry":{"id":22,"status":"CURRENT","progress":4,"repeat":0, "score": 0}
            }}),
        )])
        .await;
        let state = state_with_api(api);
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 3,
                ..Default::default()
            })
            .unwrap();
        save_entry_inner(&state, 1, None, Some(4), None, None)
            .await
            .unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":4})
        );
    }

    #[tokio::test]
    async fn missing_list_entries_retain_media_and_conflicting_edits() {
        let (api, mut requests) = anilist::mock_api(vec![(
            404,
            json!({
                "errors":[{"message":"Not Found","status":404}]
            }),
        )])
        .await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Saved show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 3,
                ..Default::default()
            })
            .unwrap();
        let result = save_entry_inner(&state, 1, None, Some(4), None, None).await;
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1, "result: {result:?}");
        assert_eq!(result.unwrap().progress, 4);
        assert!(pending[0].conflict);
        assert!(!pending[0].missing_media);
        assert!(state.db.get_media(1).unwrap().is_some());
        crate::sync_queue::flush_unlocked(&state).await.unwrap();
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":4})
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn unusable_success_responses_retain_saved_progress_for_retry() {
        for body in [
            json!({"message":"Gateway unavailable"}),
            json!({"data":null}),
            json!({"data":{"SaveMediaListEntry":null}}),
            json!({"data":{"SaveMediaListEntry":{"progress":4}}}),
        ] {
            let (api, _) = anilist::mock_api(vec![
                (200, body.clone()),
                (
                    200,
                    json!({"data":{"Media":{"mediaListEntry":{
                        "id":22,"status":"CURRENT","progress":4,"repeat":0,"score":0
                    }}}}),
                ),
            ])
            .await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        name: "Tester".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(22),
                    media_id: 1,
                    status: "CURRENT".into(),
                    progress: 3,
                    ..Default::default()
                })
                .unwrap();

            let result = save_entry_inner(&state, 1, None, Some(4), None, None).await;
            let pending = state.db.pending(7).unwrap();
            assert_eq!(pending.len(), 1, "response: {body}, result: {result:?}");
            assert_eq!(result.unwrap().progress, 4);
            assert_eq!(pending[0].patch.progress, Some(4));
            assert!(!pending[0].conflict);
            assert!(!pending[0].missing_media);
            assert!(state.anilist.lock().has_token());
            crate::sync_queue::flush_unlocked(&state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }

    fn state_with_api(api: AniList) -> AppState {
        AppState {
            anilist: Mutex::new(api),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
        }
    }

    fn viewer_response(id: i64, name: &str) -> serde_json::Value {
        json!({"data": {"Viewer": {"id": id, "name": name, "avatar": null,
            "mediaListOptions": {"scoreFormat": "POINT_5"}}}})
    }

    #[tokio::test]
    async fn the_latest_login_intent_owns_the_persisted_account() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, viewer_response(7, "Earlier")),
            (200, viewer_response(8, "Latest")),
        ])
        .await;
        let state = Arc::new(state_with_api(api));
        let guard = state.entry_lock.lock().await;
        let first_intent = state.begin_auth_intent();
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            login_with_token_inner("earlier-token".into(), &first_state, first_intent).await
        });
        requests.recv().await.unwrap();
        let latest_intent = state.begin_auth_intent();
        let latest_state = state.clone();
        let latest = tokio::spawn(async move {
            login_with_token_inner("latest-token".into(), &latest_state, latest_intent).await
        });
        requests.recv().await.unwrap();
        drop(guard);
        assert_eq!(first.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert_eq!(latest.await.unwrap().unwrap().id, 8);
        assert_eq!(state.db.cached_user().unwrap().unwrap().id, 8);
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("latest-token")
        );
    }

    #[tokio::test]
    async fn logout_cancels_a_login_waiting_to_commit() {
        let (api, mut requests) =
            anilist::mock_api(vec![(200, viewer_response(7, "Earlier"))]).await;
        let state = Arc::new(state_with_api(api));
        let guard = state.entry_lock.lock().await;
        let first_intent = state.begin_auth_intent();
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            login_with_token_inner("earlier-token".into(), &first_state, first_intent).await
        });
        requests.recv().await.unwrap();
        let logout_intent = state.begin_auth_intent();
        drop(guard);
        logout_inner(&state, logout_intent).await.unwrap();
        assert_eq!(first.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.cached_user().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_new_oauth_attempt_releases_a_superseded_preconnection() {
        use tokio::io::AsyncWriteExt;
        let state = Arc::new(state_with_api(AniList::new()));
        let first_intent = state.begin_auth_intent();
        let (_, port, receiver) = start_oauth_callback(&state, first_intent, 0).await.unwrap();
        let mut preconnect = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        preconnect.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        let first_state = state.clone();
        let first = tokio::spawn(async move {
            wait_for_oauth_token(&first_state, first_intent, receiver).await
        });
        let latest_intent = state.begin_auth_intent();
        let (_, replacement_port, receiver) = start_oauth_callback(&state, latest_intent, port)
            .await
            .unwrap();
        assert_eq!(replacement_port, port);
        assert_eq!(first.await.unwrap().unwrap_err(), AUTH_REPLACED);
        drop(receiver);
    }

    #[tokio::test]
    async fn a_stale_oauth_callback_cannot_start_token_verification() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state_with_api(api);
        let old_intent = state.begin_auth_intent();
        state.begin_auth_intent();
        assert_eq!(
            login_with_token_inner("old-token".into(), &state, old_intent)
                .await
                .unwrap_err(),
            AUTH_REPLACED
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn anilist_maintenance_keeps_the_offline_account_and_list() {
        let outage = json!({"errors": [{
            "message": "The AniList API has been temporarily disabled due to severe stability issues. Please check the official AniList Discord for more information.",
            "status": 403
        }], "data": null});
        for operation in ["profile", "sync", "save"] {
            let (api, _) = anilist::mock_api(vec![(403, outage.clone())]).await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        name: "Tester".into(),
                        score_format: Some("POINT_5".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(11),
                    media_id: 1,
                    progress: 4,
                    ..Default::default()
                })
                .unwrap();
            match operation {
                "profile" => {
                    let user = current_user_inner(&state).await.unwrap();
                    assert!(user.is_some_and(|user| user.offline && user.id == 7));
                }
                "sync" => assert!(sync_my_list_inner(&state).await.is_err()),
                _ => {
                    let entry =
                        save_entry_inner(&state, 1, Some("PAUSED".into()), None, None, None)
                            .await
                            .unwrap();
                    assert_eq!(entry.status, "PAUSED");
                    let pending = state.db.pending(7).unwrap();
                    assert_eq!(pending.len(), 1);
                    assert!(!pending[0].conflict);
                }
            }
            assert!(state.anilist.lock().has_token());
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        }
    }

    #[tokio::test]
    async fn an_offline_start_keeps_the_validated_score_format() {
        let (api, _) = anilist::mock_api(vec![
            (503, json!({"errors": [{"message": "Service unavailable"}]})),
            (200, json!({"data": {"Viewer": {"id": 7, "name": "Renamed", "avatar": null, "mediaListOptions": {"scoreFormat": "POINT_5"}}}})),
        ]).await;
        let state = state_with_api(api);
        let user = User {
            id: 7,
            name: "Tester".into(),
            score_format: Some("POINT_10_DECIMAL".into()),
            ..Default::default()
        };
        state.db.replace_account("test-session", &user).unwrap();
        let offline = current_user_inner(&state).await.unwrap().unwrap();
        assert_eq!(offline.id, user.id);
        assert_eq!(offline.score_format, user.score_format);
        assert_eq!(serde_json::to_value(offline).unwrap()["offline"], true);
        assert!(state.user.lock().is_none());
        let online = current_user_inner(&state).await.unwrap().unwrap();
        assert!(!online.offline);
        assert_eq!(online.score_format.as_deref(), Some("POINT_5"));
        assert_eq!(state.db.cached_user().unwrap().unwrap().name, "Renamed");
    }

    #[tokio::test]
    async fn a_legacy_offline_profile_is_marked_unavailable() {
        let (api, _) = anilist::mock_api(vec![(
            503,
            json!({"errors": [{"message": "Service unavailable"}]}),
        )])
        .await;
        let state = state_with_api(api);
        state.db.set_setting(USERNAME_KEY, "Tester").unwrap();
        let offline = current_user_inner(&state).await.unwrap().unwrap();
        assert!(offline.offline);
        assert!(offline.score_format.is_none());
        assert_eq!(offline.name, "Tester");
    }

    #[tokio::test]
    async fn removing_a_queued_add_checks_for_an_interrupted_remote_success() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"status":"CURRENT","progress":1,"repeat":0, "score": 0}}}})),
            (200, json!({"data":{"DeleteMediaListEntry":{"deleted":true}}})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let (_, pending, _) = crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(1),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        crate::sync_queue::local_entry(&state, &pending).unwrap();
        assert!(state.db.get_entry(1).unwrap().unwrap().id.is_none());
        delete_entry_inner(1, &state).await.unwrap();
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("mediaListEntry"));
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_remote_lookup_retains_the_queued_entry_during_removal() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let (_, pending, _) = crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(1),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        crate::sync_queue::local_entry(&state, &pending).unwrap();
        assert!(delete_entry_inner(1, &state).await.is_err());
        assert!(state.db.get_entry(1).unwrap().is_some());
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn explicit_edits_survive_a_local_cache_miss() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data": {
                "Media": {"mediaListEntry": {"id": 22, "status": "CURRENT", "progress": 3, "repeat": 0, "score": 0}},
                "MediaList": {"id": 22, "status": "CURRENT", "progress": 3, "repeat": 0, "score": 0}
            }})),
            (200, json!({"data": {"SaveMediaListEntry": {"id": 22, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}}})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        save_entry_inner(&state, 1, None, Some(4), Some(8.5), Some(2))
            .await
            .unwrap();
        requests.recv().await.unwrap();
        let write = requests.recv().await.unwrap();
        assert_eq!(write["variables"]["progress"], 4);
        assert_eq!(write["variables"]["score"], 8.5);
        assert_eq!(write["variables"]["repeat"], 2);
    }

    #[tokio::test]
    async fn adding_a_remote_entry_preserves_omitted_fields() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data": {"Media": {"mediaListEntry": {"id": 22, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"id": 22, "status": "PAUSED", "progress": 4, "score": 8.5, "repeat": 2}}})),
        ]).await;
        let state = state_with_api(api);
        let saved = save_entry_inner(&state, 1, Some("PAUSED".into()), None, None, None)
            .await
            .unwrap();
        requests.recv().await.unwrap();
        let write = requests.recv().await.unwrap();
        assert_eq!(write["variables"], json!({"id": 22, "status": "PAUSED"}));
        assert_eq!(
            (saved.progress, saved.score, saved.repeat),
            (4, Some(8.5), 2)
        );
    }

    #[tokio::test]
    async fn restoring_an_existing_remote_entry_immediately_enables_recognition() {
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data": {"Media": {"mediaListEntry": {"id": 22, "status": "CURRENT", "progress": 4, "repeat": 0, "score": 0}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"id": 22, "status": "CURRENT", "progress": 4, "repeat": 0, "score": 0}}})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Recognized show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        save_entry_inner(&state, 1, Some("CURRENT".into()), None, None, None)
            .await
            .unwrap();
        assert!(state.db.get_entry(1).unwrap().is_some());
        assert_eq!(
            state
                .matchers
                .lock()
                .iter()
                .map(|m| m.media_id)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn completing_always_sends_the_known_total() {
        for requested in [None, Some(0), Some(5), Some(99)] {
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data": {"Media": {"mediaListEntry": null}}})),
                (200, json!({"data": {"SaveMediaListEntry": {"id": 22, "status": "COMPLETED", "progress": 12, "repeat": 0, "score": 0}}})),
            ]).await;
            let state = state_with_api(api);
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();
            let saved =
                save_entry_inner(&state, 1, Some("COMPLETED".into()), requested, None, None)
                    .await
                    .unwrap();
            requests.recv().await.unwrap();
            assert_eq!(requests.recv().await.unwrap()["variables"]["progress"], 12);
            assert_eq!(saved.progress, 12);
        }
    }

    #[tokio::test]
    async fn a_remote_completed_entry_starts_rewatching_at_zero() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data": {"Media": {"mediaListEntry": {"id": 22, "status": "COMPLETED", "progress": 12, "repeat": 2, "score": 0}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"id": 22, "status": "REPEATING", "progress": 0, "repeat": 2, "score": 0}}})),
        ]).await;
        let state = state_with_api(api);
        save_entry_inner(&state, 1, Some("REPEATING".into()), None, None, None)
            .await
            .unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id": 22, "status": "REPEATING", "progress": 0})
        );
    }

    #[tokio::test]
    async fn incomplete_sync_keeps_the_cached_list() {
        for lists in [
            json!(null),
            json!([{"entries": null}]),
            json!([{"entries": [null]}]),
        ] {
            let (api, _) = anilist::mock_api(vec![(
                200,
                json!({"data": {"MediaListCollection": {"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        name: "Tester".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    media_id: 1,
                    progress: 4,
                    ..Default::default()
                })
                .unwrap();
            assert!(sync_my_list_inner(&state).await.is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        }
    }

    #[tokio::test]
    async fn deleting_a_recreated_entry_uses_the_authenticated_owner() {
        let (api, mut requests) = anilist::mock_api(vec![
            (404, json!({"errors": [{"message": "Not Found."}]})),
            (
                200,
                json!({"data": {
                    "Media": {"mediaListEntry": {"id": 22, "status": "CURRENT", "progress": 0, "repeat": 0, "score": 0}},
                    "MediaList": {"id": 999}
                }}),
            ),
            (
                200,
                json!({"data": {"DeleteMediaListEntry": {"deleted": true}}}),
            ),
        ])
        .await;
        let state = state_with_api(api);
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(11),
                media_id: 1,
                ..Default::default()
            })
            .unwrap();
        delete_entry_inner(1, &state).await.unwrap();
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 11);
        requests.recv().await.unwrap();
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.anilist.lock().has_token());
    }

    #[tokio::test]
    async fn deleting_a_gone_anime_removes_the_stale_list_row() {
        let missing = json!({"errors": [{"message": "Not Found."}]});
        let (api, _) = anilist::mock_api(vec![(404, missing.clone()), (404, missing)]).await;
        let state = state_with_api(api);
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(11),
                media_id: 1,
                ..Default::default()
            })
            .unwrap();
        delete_entry_inner(1, &state).await.unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.anilist.lock().has_token());
    }

    #[tokio::test]
    async fn an_old_playback_session_cannot_write_after_account_change() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = Arc::new(state_with_api(api));
        state
            .db
            .upsert_entry(&ListEntry {
                media_id: 1,
                progress: 4,
                ..Default::default()
            })
            .unwrap();
        let guard = state.entry_lock.lock().await;
        let worker_state = state.clone();
        let worker = tokio::spawn(async move {
            watcher_set_progress(&worker_state, 1, 5, Some("test-session")).await
        });
        tokio::task::yield_now().await;
        state
            .anilist
            .lock()
            .set_token(Some("another-session".into()));
        drop(guard);
        assert!(worker.await.unwrap().unwrap().is_none());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn queued_ui_writes_cannot_follow_an_account_change() {
        for operation in ["save", "progress", "delete"] {
            let (api, mut requests) = anilist::mock_api(vec![]).await;
            let state = Arc::new(state_with_api(api));
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(22),
                    media_id: 1,
                    progress: 4,
                    ..Default::default()
                })
                .unwrap();
            let guard = state.entry_lock.lock().await;
            let worker_state = state.clone();
            let worker = tokio::spawn(async move {
                match operation {
                    "save" => {
                        save_entry_inner(&worker_state, 1, Some("PAUSED".into()), None, None, None)
                            .await
                            .map(|_| ())
                    }
                    "progress" => set_progress_inner(&worker_state, 1, 5, Some(4))
                        .await
                        .map(|_| ()),
                    _ => delete_entry_inner(1, &worker_state).await,
                }
            });
            tokio::task::yield_now().await;
            state
                .db
                .replace_account(
                    "another-session",
                    &User {
                        id: 8,
                        name: "Another".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .anilist
                .lock()
                .set_token(Some("another-session".into()));
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(33),
                    media_id: 1,
                    progress: 4,
                    ..Default::default()
                })
                .unwrap();
            drop(guard);
            assert_eq!(
                worker.await.unwrap().unwrap_err(),
                "Your AniList account changed. Try again."
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(33));
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn cache_miss_auth_rejection_clears_the_cached_profile() {
        let (api, _) = anilist::mock_api(vec![(
            401,
            json!({"errors": [{"message": "Invalid Token"}]}),
        )])
        .await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let error = save_entry_inner(&state, 1, Some("PLANNING".into()), None, None, None)
            .await
            .unwrap_err();
        assert_eq!(error, SESSION_EXPIRED);
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.get_setting(PROFILE_KEY).unwrap().is_none());
    }
}

#[cfg(test)]
mod feature_cache_tests {
    use super::*;
    use serde_json::json;

    fn cache_media_details(state: &AppState) {
        let media = Media {
            id: 1,
            title_english: Some("Saved show".into()),
            episodes: Some(12),
            ..Default::default()
        };
        state.db.upsert_media(&media).unwrap();
        state
            .db
            .set_setting(
                "media_detail:1",
                &serde_json::to_string(&MediaDetail {
                    media,
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: Some(1),
                    warning: None,
                    unavailable_sections: vec![],
                })
                .unwrap(),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn adversarial_detail_rejections_expire_sessions_instead_of_returning_cached_success() {
        for status in [200, 401] {
            for cache in ["details", "media", "none"] {
                let (api, _) = anilist::mock_api(vec![(
                    status,
                    json!({"errors":[{"message":"Invalid Token","status":401}]}),
                )])
                .await;
                let state = state(api);
                pending_progress(&state);
                if cache != "none" {
                    cache_media_details(&state);
                    if cache == "media" {
                        state.db.delete_setting("media_detail:1").unwrap();
                    }
                }
                let result = get_media_detail_inner(1, &state).await;
                assert_eq!(
                    result.err().as_deref(),
                    Some(SESSION_EXPIRED),
                    "cache: {cache}"
                );
                assert_expired_with_pending_intact(&state);
            }
        }
    }

    #[tokio::test]
    async fn adversarial_detail_outages_keep_cached_content_and_the_account() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        pending_progress(&state);
        cache_media_details(&state);
        let detail = get_media_detail_inner(1, &state).await.unwrap();
        assert_eq!(detail.media.display_title(), "Saved show");
        assert_eq!(detail.cached_at, Some(1));
        assert!(detail.warning.is_some());
        assert!(state.anilist.lock().has_token());
        assert!(state.db.cached_user().unwrap().is_some());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
    }

    #[tokio::test]
    async fn adversarial_detail_late_rejections_cannot_expire_a_replacement_account() {
        let (api, mut requests) =
            anilist::mock_api(vec![(401, json!({"errors":[{"message":"Invalid Token"}]}))]).await;
        let state = Arc::new(state(api));
        cache_media_details(&state);
        let guard = state.entry_lock.lock().await;
        let reader = state.clone();
        let task = tokio::spawn(async move { get_media_detail_inner(1, &reader).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        state
            .anilist
            .lock()
            .set_token(Some("replacement-session".into()));
        drop(guard);
        assert!(task.await.unwrap().unwrap_err().contains("account changed"));
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("replacement-session")
        );
    }

    #[tokio::test]
    async fn adversarial_notification_rejection_expires_the_session_with_queue_intact() {
        for status in [200, 401] {
            let (api, _) = anilist::mock_api(vec![(
                status,
                json!({"errors":[{"message":"Invalid Token","status":401}]}),
            )])
            .await;
            let state = state(api);
            pending_progress(&state);
            assert!(get_notifications_page_inner(1, &state).await.is_err());
            assert_expired_with_pending_intact(&state);
        }
    }

    #[tokio::test]
    async fn adversarial_notification_outages_preserve_the_session_and_saved_changes() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        pending_progress(&state);
        assert!(get_notifications_page_inner(1, &state).await.is_err());
        assert!(state.anilist.lock().has_token());
        assert!(state.db.cached_user().unwrap().is_some());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
        assert!(state.db.get_entry(1).unwrap().is_some());
    }

    #[tokio::test]
    async fn adversarial_queued_notification_reads_cannot_expire_a_replacement_account() {
        let (api, mut requests) =
            anilist::mock_api(vec![(401, json!({"errors":[{"message":"Invalid Token"}]}))]).await;
        let state = Arc::new(state(api));
        let guard = state.entry_lock.lock().await;
        let reader = state.clone();
        let task = tokio::spawn(async move { get_notifications_page_inner(1, &reader).await });
        tokio::task::yield_now().await;
        state
            .anilist
            .lock()
            .set_token(Some("replacement-session".into()));
        drop(guard);
        assert!(task.await.unwrap().unwrap_err().contains("account changed"));
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("replacement-session")
        );
        assert!(requests.try_recv().is_err());
    }

    fn state(api: AniList) -> AppState {
        let db = Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap());
        db.replace_account(
            "test-session",
            &User {
                id: 7,
                name: "Tester".into(),
                ..Default::default()
            },
        )
        .unwrap();
        AppState {
            anilist: Mutex::new(api),
            db,
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
        }
    }

    fn pending_progress(state: &AppState) {
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 3,
                ..Default::default()
            })
            .unwrap();
        crate::sync_queue::stage(
            state,
            1,
            crate::sync_queue::Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn adversarial_fresh_details_update_playback_episode_recognition() {
        let (api, _) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"title":{"english":"Some Movie"},"episodes":1,
                "relations":{"edges":[]},"characters":{"edges":[]},"staff":{"edges":[]}}}}),
        )])
        .await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Some Movie".into()),
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "PLANNING".into(),
                ..Default::default()
            })
            .unwrap();
        state.refresh_matchers();
        let found = get_media_detail_inner(1, &state).await.unwrap();
        assert_eq!(found.media.episodes, Some(1));
        assert_eq!(state.db.get_media(1).unwrap().unwrap().episodes, Some(1));
        let matchers = state.matchers.lock().clone();
        assert_eq!(
            recognize::resolve_playback_episode(&matchers[0], &["Some Movie.mkv"], false),
            Some(1),
            "freshly fetched movie details did not reach playback recognition"
        );
    }

    fn assert_expired_with_pending_intact(state: &AppState) {
        assert!(
            !state.anilist.lock().has_token(),
            "rejected session should be expired"
        );
        assert!(state.db.cached_user().unwrap().is_none());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
    }

    #[tokio::test]
    async fn metadata_updates_target_the_preflight_entry_identity() {
        for removed in [false, true] {
            let response = if removed {
                (
                    404,
                    json!({"errors":[{"message":"Not Found","status":404}]}),
                )
            } else {
                (
                    200,
                    json!({"data":{"SaveMediaListEntry":{"notes":"New note","customLists":{}}}}),
                )
            };
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
                response,
            ]).await;
            let state = state(api);
            pending_progress(&state);
            let cached = serde_json::to_string(&EntryDetails {
                notes: "Old note".into(),
                ..Default::default()
            })
            .unwrap();
            state.db.set_setting("entry_details:7:1", &cached).unwrap();
            let result =
                update_entry_details_inner(1, Some("New note".into()), None, None, None, &state)
                    .await;
            assert_eq!(result.is_err(), removed);
            let lookup = requests.recv().await.unwrap();
            assert!(lookup["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry { id"));
            let mutation = requests.recv().await.unwrap();
            assert_eq!(
                mutation["variables"]["id"], 22,
                "metadata must target the fetched list entry"
            );
            let query = mutation["query"].as_str().unwrap();
            assert!(query.contains("SaveMediaListEntry(id: $id"));
            assert!(!query.contains("mediaId:"));
            assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
            assert!(state.anilist.lock().has_token());
            if removed {
                assert_eq!(
                    state.db.get_setting("entry_details:7:1").unwrap(),
                    Some(cached)
                );
            } else {
                assert_eq!(result.unwrap().notes, "New note");
            }
        }
    }

    #[tokio::test]
    async fn metadata_without_entry_identity_does_not_send_a_write() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{"mediaListEntry":{"notes":"Old note","customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
            (200, json!({"data":{"SaveMediaListEntry":{"notes":"New note","customLists":{}}}})),
        ]).await;
        let state = state(api);
        let result =
            update_entry_details_inner(1, Some("New note".into()), None, None, None, &state).await;
        assert!(
            result.is_err(),
            "a missing entry identity must stop metadata writes"
        );
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
        assert!(state.db.get_setting("entry_details:7:1").unwrap().is_none());
    }

    #[tokio::test]
    async fn rejected_metadata_reads_do_not_masquerade_as_offline_cache_hits() {
        let rejected = json!({"errors":[{"message":"Invalid Token","status":401}],"data":null});
        for read_stats in [false, true] {
            let (api, _) = anilist::mock_api(vec![(401, rejected.clone())]).await;
            let state = state(api);
            pending_progress(&state);
            state
                .db
                .set_setting(
                    "entry_details:7:1",
                    &serde_json::to_string(&EntryDetails {
                        notes: "Cached note".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .unwrap();
            state
                .db
                .set_setting(
                    "user_stats:7",
                    &serde_json::to_string(&UserStats::default()).unwrap(),
                )
                .unwrap();
            let error = if read_stats {
                get_user_stats_inner(&state).await.err()
            } else {
                get_entry_details_inner(1, &state).await.err()
            };
            assert_expired_with_pending_intact(&state);
            assert_eq!(error.as_deref(), Some(SESSION_EXPIRED));
        }
    }

    #[tokio::test]
    async fn rejected_metadata_writes_expire_the_session_without_losing_pending_changes() {
        let rejected = json!({"errors":[{"message":"Invalid Token","status":401}],"data":null});
        for reject_lookup in [true, false] {
            let mut responses = vec![];
            if !reject_lookup {
                responses.push((200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})));
            }
            responses.push((401, rejected.clone()));
            let (api, _) = anilist::mock_api(responses).await;
            let state = state(api);
            pending_progress(&state);
            let error =
                update_entry_details_inner(1, Some("New note".into()), None, None, None, &state)
                    .await
                    .err()
                    .unwrap();
            assert_expired_with_pending_intact(&state);
            assert_eq!(error, SESSION_EXPIRED);
        }
    }

    #[tokio::test]
    async fn rejected_mark_read_expires_the_session_without_losing_pending_changes() {
        let (api, _) = anilist::mock_api(vec![(
            401,
            json!({"errors":[{"message":"Invalid Token","status":401}]}),
        )])
        .await;
        let state = state(api);
        pending_progress(&state);
        let error = mark_notifications_read_inner(&state).await.err();
        assert_expired_with_pending_intact(&state);
        assert_eq!(error.as_deref(), Some(SESSION_EXPIRED));
    }

    #[tokio::test]
    async fn non_auth_feature_errors_preserve_the_session_queue_and_cached_metadata() {
        for status in [503, 403] {
            let error = json!({"errors":[{"status":status,"message":"Feature temporarily unavailable"}],"data":null});
            let (api, _) = anilist::mock_api(vec![
                (status, error.clone()),
                (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
                (status, error.clone()),
                (status, error.clone()),
                (status, error),
            ]).await;
            let state = state(api);
            pending_progress(&state);
            let cached = serde_json::to_string(&EntryDetails {
                notes: "Cached note".into(),
                ..Default::default()
            })
            .unwrap();
            state.db.set_setting("entry_details:7:1", &cached).unwrap();
            state
                .db
                .set_setting(
                    "user_stats:7",
                    &serde_json::to_string(&UserStats {
                        count: 12,
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .unwrap();
            let details = get_entry_details_inner(1, &state).await.unwrap();
            assert_eq!(details.notes, "Cached note");
            assert!(details.warning.is_some());
            assert!(update_entry_details_inner(
                1,
                Some("Unsaved note".into()),
                None,
                None,
                None,
                &state
            )
            .await
            .is_err());
            let stats = get_user_stats_inner(&state).await.unwrap();
            assert_eq!(stats.count, 12);
            assert!(stats.warning.is_some());
            assert!(mark_notifications_read_inner(&state).await.is_err());
            assert!(state.anilist.lock().has_token());
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
            assert!(state.db.get_entry(1).unwrap().is_some());
            assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
            assert_eq!(
                state
                    .db
                    .get_setting("entry_details:7:1")
                    .unwrap()
                    .as_deref(),
                Some(cached.as_str())
            );
        }
    }

    #[tokio::test]
    async fn offline_profile_can_recover_saved_statistics_and_entry_details() {
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"User":{"statistics":null}}})),
            (200, json!({"data":{"Media":{"mediaListEntry":null}}})),
        ])
        .await;
        let state = state(api);
        let stats = UserStats {
            count: 12,
            cached_at: Some(100),
            ..Default::default()
        };
        state
            .db
            .set_setting("user_stats:7", &serde_json::to_string(&stats).unwrap())
            .unwrap();
        let details = EntryDetails {
            notes: "Keep me".into(),
            cached_at: Some(100),
            ..Default::default()
        };
        state
            .db
            .set_setting(
                "entry_details:7:1",
                &serde_json::to_string(&details).unwrap(),
            )
            .unwrap();
        let recovered = get_user_stats_inner(&state).await.unwrap();
        assert_eq!(recovered.count, 12);
        assert!(recovered.warning.is_some());
        let recovered = get_entry_details_inner(1, &state).await.unwrap();
        assert_eq!(recovered.notes, "Keep me");
        assert!(recovered.warning.is_some());
    }

    #[tokio::test]
    async fn queued_metadata_and_mark_read_actions_cannot_follow_an_account_switch() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = Arc::new(state(api));
        let guard = state.entry_lock.lock().await;
        let mark_state = state.clone();
        let marking = tokio::spawn(async move { mark_notifications_read_inner(&mark_state).await });
        let details_state = state.clone();
        let saving = tokio::spawn(async move {
            update_entry_details_inner(
                1,
                Some("Old account note".into()),
                None,
                None,
                None,
                &details_state,
            )
            .await
        });
        tokio::task::yield_now().await;
        state.anilist.lock().set_token(Some("next-account".into()));
        drop(guard);
        assert!(marking
            .await
            .unwrap()
            .unwrap_err()
            .contains("account changed"));
        assert!(saving
            .await
            .unwrap()
            .unwrap_err()
            .contains("account changed"));
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn date_validation_accepts_partial_dates_and_rejects_impossible_days() {
        assert!(validate_entry_date(Some(&FuzzyDate {
            year: Some(2024),
            month: None,
            day: None
        }))
        .is_ok());
        assert!(validate_entry_date(Some(&FuzzyDate {
            year: Some(2023),
            month: Some(2),
            day: Some(29)
        }))
        .is_err());
        assert!(validate_entry_date(Some(&FuzzyDate {
            year: None,
            month: Some(2),
            day: Some(29)
        }))
        .is_ok());
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn state(api: AniList) -> AppState {
        let user = User {
            id: 7,
            name: "Tester".into(),
            ..Default::default()
        };
        let state = AppState {
            anilist: Mutex::new(api),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            user: Mutex::new(Some(user.clone())),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
        };
        state.db.replace_account("test-session", &user).unwrap();
        state
            .db
            .upsert_media(&Media {
                id: 1,
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "COMPLETED".into(),
                progress: 12,
                repeat: 2,
                score: Some(70.0),
                ..Default::default()
            })
            .unwrap();
        state
    }

    fn remote() -> serde_json::Value {
        json!({"id":22,"mediaId":1,"status":"COMPLETED","progress":12,"repeat":2,"score":70})
    }

    fn assert_progress_preserved(state: &AppState) {
        let entry = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (entry.status.as_str(), entry.progress, entry.repeat),
            ("COMPLETED", 12, 2)
        );
    }

    #[tokio::test]
    async fn remote_retries_leave_cached_reads_and_playback_state_available() {
        for operation in [
            "notifications",
            "statistics",
            "details",
            "sync",
            "queue",
            "save",
        ] {
            let (api, mut requests) = anilist::mock_api(vec![
                (429, json!({})),
                (503, json!({"errors":[{"message":"Unavailable"}]})),
            ])
            .await;
            let state = Arc::new(state(api));
            if operation == "queue" {
                crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        score: Some(80.0),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            let worker_state = state.clone();
            let worker = tokio::spawn(async move {
                match operation {
                    "notifications" => get_notifications_page_inner(1, &worker_state)
                        .await
                        .map(|_| ()),
                    "statistics" => get_user_stats_inner(&worker_state).await.map(|_| ()),
                    "details" => get_entry_details_inner(1, &worker_state).await.map(|_| ()),
                    "sync" => sync_my_list_inner(&worker_state).await.map(|_| ()),
                    "queue" => {
                        let _remote = worker_state.remote_lock.lock().await;
                        crate::sync_queue::flush_unlocked(&worker_state).await
                    }
                    _ => save_entry_inner(&worker_state, 1, None, None, Some(80.0), None)
                        .await
                        .map(|_| ()),
                }
            });
            requests.recv().await.unwrap();
            tokio::time::timeout(Duration::from_millis(200), async {
                assert_eq!(current_user_inner(&state).await.unwrap().unwrap().id, 7);
                assert_eq!(local_entries_inner(&state).await.unwrap()[0].progress, 12);
                let _local = state.entry_lock.lock().await;
                assert_eq!(
                    state.anilist.lock().token().as_deref(),
                    Some("test-session")
                );
                assert_progress_preserved(&state);
            })
            .await
            .unwrap_or_else(|_| panic!("local state blocked behind {operation}"));
            assert!(
                !worker.is_finished(),
                "fixture must still be retrying {operation}"
            );
            let _ = worker.await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejected_saves_restore_entries_overlaid_during_the_request() {
        for adding in [false, true] {
            let mut replies = vec![];
            if adding {
                replies.push((200, json!({"data":{"Media":{"mediaListEntry":null}}})));
            }
            replies.extend([
                (429, json!({})),
                (
                    400,
                    json!({"errors":[{"message":"Invalid score","status":400}]}),
                ),
            ]);
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = Arc::new(state(api));
            if adding {
                state.db.delete_entry(1).unwrap();
            }
            let editing = state.clone();
            let edit = tokio::spawn(async move {
                save_entry_inner(&editing, 1, None, None, Some(80.0), None).await
            });
            if adding {
                requests.recv().await.unwrap();
            }
            requests.recv().await.unwrap();
            assert_eq!(
                local_entries_inner(&state).await.unwrap()[0].score,
                Some(80.0)
            );
            assert!(edit.await.unwrap().is_err());
            let entries = local_entries_inner(&state).await.unwrap();
            if adding {
                assert!(entries.is_empty());
            } else {
                assert_eq!(entries[0].score, Some(70.0));
            }
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn missing_scores_do_not_erase_cached_scores_or_acknowledge_queued_edits() {
        let mut reply = remote();
        reply.as_object_mut().unwrap().remove("score");
        let (api, _) =
            anilist::mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":reply}}))]).await;
        let state = state(api);
        let saved = save_entry_inner(&state, 1, None, None, None, Some(3))
            .await
            .unwrap();
        assert_eq!(saved.score, Some(70.0));
        assert_eq!(state.db.pending(7).unwrap()[0].patch.repeat, Some(3));
    }

    #[tokio::test]
    async fn partial_save_replies_preserve_progress_and_pending_intent() {
        for field in ["status", "progress", "repeat"] {
            for missing in [false, true] {
                let mut reply = remote();
                reply["score"] = json!(80);
                if missing {
                    reply.as_object_mut().unwrap().remove(field);
                } else {
                    reply[field] = json!(null);
                }
                let (api, mut requests) =
                    anilist::mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":reply}}))])
                        .await;
                let state = state(api);
                let saved = save_entry_inner(&state, 1, None, None, Some(80.0), None)
                    .await
                    .unwrap();
                assert_eq!(saved.score, Some(80.0));
                assert_progress_preserved(&state);
                assert_eq!(state.db.pending(7).unwrap()[0].patch.score, Some(80.0));
                requests.recv().await.unwrap();
                set_progress_inner(&state, 1, 13, Some(12)).await.unwrap();
                assert_progress_preserved(&state);
                assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(12));
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn partial_list_replies_preserve_the_last_complete_snapshot() {
        for field in ["status", "progress", "repeat", "score"] {
            let mut reply = remote();
            if field == "score" {
                reply.as_object_mut().unwrap().remove(field);
            } else {
                reply[field] = json!(null);
            }
            let (api, _) = anilist::mock_api(vec![(
                200,
                json!({"data":{"MediaListCollection":{
                    "hasNextChunk":false,"lists":[{"entries":[reply]}]
                }}}),
            )])
            .await;
            let state = state(api);
            assert!(sync_my_list_inner(&state).await.is_err());
            assert_progress_preserved(&state);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
        }
    }

    #[tokio::test]
    async fn incomplete_queue_lookups_and_acknowledgements_keep_saved_changes() {
        for lookup in [true, false] {
            let mut partial = remote();
            partial["repeat"] = json!(null);
            let replies = if lookup {
                vec![(200, json!({"data":{"Media":{"mediaListEntry":partial}}}))]
            } else {
                vec![
                    (200, json!({"data":{"Media":{"mediaListEntry":remote()}}})),
                    (200, json!({"data":{"SaveMediaListEntry":partial}})),
                ]
            };
            let (api, _) = anilist::mock_api(replies).await;
            let state = state(api);
            let (_, pending, _) = crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    score: Some(80.0),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            crate::sync_queue::local_entry(&state, &pending).unwrap();
            assert!(crate::sync_queue::flush_unlocked(&state).await.is_err());
            assert_progress_preserved(&state);
            let pending = state.db.pending(7).unwrap();
            assert_eq!(pending[0].patch.score, Some(80.0));
            assert!(!pending[0].conflict);
        }
    }

    #[tokio::test]
    async fn a_slow_metadata_read_cannot_replace_a_newer_save() {
        let old = json!({"data":{
            "Media":{"mediaListEntry":{"id":22,"notes":"Old note","customLists":{}}},
            "Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}
        }});
        let (api, mut reads) = anilist::mock_api(vec![(429, json!({})), (200, old.clone())]).await;
        let state = Arc::new(state(api));
        let reading = state.clone();
        let read = tokio::spawn(async move { get_entry_details_inner(1, &reading).await });
        reads.recv().await.unwrap();

        let (api, _) = anilist::mock_api(vec![
            (200, old),
            (
                200,
                json!({"data":{"SaveMediaListEntry":{"notes":"New note","customLists":{}}}}),
            ),
        ])
        .await;
        *state.anilist.lock() = api;
        let editing = state.clone();
        let edit = tokio::spawn(async move {
            update_entry_details_inner(1, Some("New note".into()), None, None, None, &editing).await
        });
        read.await.unwrap().unwrap();
        edit.await.unwrap().unwrap();
        let cached: EntryDetails =
            serde_json::from_str(&state.db.get_setting("entry_details:7:1").unwrap().unwrap())
                .unwrap();
        assert_eq!(cached.notes, "New note");
    }

    #[tokio::test]
    async fn a_slow_snapshot_cannot_replace_a_newer_save() {
        let mut saved = remote();
        saved["score"] = json!(80);
        let (api, mut requests) = anilist::mock_api(vec![
            (429, json!({})),
            (
                200,
                json!({"data":{"MediaListCollection":{
                    "hasNextChunk":false,"lists":[{"entries":[remote()]}]
                }}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":saved}})),
        ])
        .await;
        let state = Arc::new(state(api));
        let syncing = state.clone();
        let sync = tokio::spawn(async move { sync_my_list_inner(&syncing).await });
        requests.recv().await.unwrap();
        let editing = state.clone();
        let edit = tokio::spawn(async move {
            save_entry_inner(&editing, 1, None, None, Some(80.0), None).await
        });
        sync.await.unwrap().unwrap();
        edit.await.unwrap().unwrap();
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(80.0));
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_late_notification_rejection_cannot_clear_a_new_login() {
        let (api, mut requests) = anilist::mock_api(vec![
            (429, json!({})),
            (
                200,
                json!({"data":{"Viewer":{"id":8,"name":"New account"}}}),
            ),
            (401, json!({"errors":[{"message":"Invalid Token"}]})),
        ])
        .await;
        let state = Arc::new(state(api));
        let reading = state.clone();
        let read = tokio::spawn(async move { get_notifications_page_inner(1, &reading).await });
        requests.recv().await.unwrap();
        let intent = state.begin_auth_intent();
        login_with_token_inner("replacement".into(), &state, intent)
            .await
            .unwrap();
        assert!(read.await.unwrap().unwrap_err().contains("account changed"));
        assert_eq!(current_user_inner(&state).await.unwrap().unwrap().id, 8);
        assert_eq!(state.anilist.lock().token().as_deref(), Some("replacement"));
    }
}
