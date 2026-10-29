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
    /// Refresh after list changes that affect recognition.
    pub matchers: Mutex<Arc<Vec<recognize::Matcher>>>,
    pub library_cache: Arc<Mutex<library::ScanCache>>,
}

impl AppState {
    /// Caller serializes list writes with entry_lock.
    pub fn refresh_matchers(&self) {
        let previous = self.matchers.lock().clone();
        let matchers = recognize::refresh_matchers(&self.db, &previous, None);
        *self.matchers.lock() = matchers;
    }

    fn invalidate_matchers(&self) {
        let matchers = recognize::build_matchers(&self.db);
        *self.matchers.lock() = Arc::new(matchers);
    }

    fn cache_media_batch(&self, media: &[Media]) {
        let _ = self.db.upsert_media_batch(media);
        self.refresh_media_matchers(&media.iter().map(|item| item.id).collect::<Vec<_>>());
    }

    fn refresh_media_matchers(&self, ids: &[i64]) {
        let previous = self.matchers.lock().clone();
        let matchers = recognize::refresh_matchers(&self.db, &previous, Some(ids));
        *self.matchers.lock() = matchers;
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
    state.invalidate_matchers();
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
    let session_changed = state.anilist.lock().token().as_ref() != Some(&token);
    state.anilist.lock().set_token(Some(token));
    *state.user.lock() = Some(user.clone());
    if account_changed || session_changed {
        state.invalidate_matchers();
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
    if client_id != DEFAULT_CLIENT_ID || redirect_uri != DEFAULT_REDIRECT_URI {
        return Err("Browser sign-in requires Kurisu's registered AniList client. Use an access token for a custom client.".into());
    }
    let (oauth_state, _, rx) =
        start_oauth_callback(state.inner(), intent, anilist::OAUTH_PORT).await?;
    let url = anilist::authorize_url(&client_id, &redirect_uri, &oauth_state);
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())?;
    finish_oauth_login(state.inner(), intent, rx).await
}

async fn finish_oauth_login(
    state: &AppState,
    intent: u64,
    receiver: anilist::OAuthReceiver,
) -> Result<User, String> {
    let code = wait_for_oauth_code(state, intent, receiver).await?;
    let api = state.anilist.lock().clone();
    let token = tokio::select! {
        result = api.exchange_code(&code) => result.map_err(|e| e.to_string())?,
        _ = state.auth_intent_changed(intent) => return Err(AUTH_REPLACED.into()),
    };
    login_with_token_inner(token, state, intent).await
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

async fn wait_for_oauth_code(
    state: &AppState,
    intent: u64,
    receiver: anilist::OAuthReceiver,
) -> Result<String, String> {
    state.check_auth_intent(intent)?;
    let code = tokio::select! {
        result = tokio::time::timeout(std::time::Duration::from_secs(300), receiver) => result
            .map_err(|_| "Timed out waiting for AniList to redirect.".to_string())?
            .map_err(|_| "OAuth callback channel closed.".to_string())?,
        _ = state.auth_intent_changed(intent) => Err(AUTH_REPLACED.into()),
    }?;
    state.check_auth_intent(intent)?;
    Ok(code)
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
    state.invalidate_matchers();
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
                state.invalidate_matchers();
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
    state.cache_media_batch(&result.items);
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
    state.cache_media_batch(&media);
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
    state.cache_media_batch(&media);
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
    state.refresh_media_matchers(&[v.id]);
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
            let ids = std::iter::once(detail.media.id)
                .chain(detail.relations.iter().map(|relation| relation.media.id))
                .collect::<Vec<_>>();
            state.refresh_media_matchers(&ids);
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
    state.refresh_media_matchers(
        &items
            .iter()
            .filter(|item| on_list.contains(&item.media.id))
            .map(|item| item.media.id)
            .collect::<Vec<_>>(),
    );
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
pub async fn local_entry_updates(
    media_ids: Vec<i64>,
    account_id: i64,
    state: State<'_, AppState>,
) -> Result<Vec<Option<ListEntry>>, String> {
    local_entry_updates_inner(state.inner(), &media_ids, account_id).await
}

async fn local_entry_updates_inner(
    state: &AppState,
    media_ids: &[i64],
    account_id: i64,
) -> Result<Vec<Option<ListEntry>>, String> {
    let token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    if token.is_none()
        || state.anilist.lock().token() != token
        || crate::sync_queue::account_id(&state.db) != Some(account_id)
    {
        return Err("Account changed".into());
    }
    let mut matchers_changed = false;
    let entries = media_ids
        .iter()
        .map(|&media_id| {
            let stored = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
            let pending = state
                .db
                .pending_change(account_id, media_id)
                .map_err(|e| e.to_string())?;
            let Some(pending) = pending else {
                return Ok(stored);
            };
            if pending.missing_media {
                if stored.is_some() {
                    state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
                    matchers_changed = true;
                }
                return Ok(None);
            }
            let mut entry = stored
                .clone()
                .or_else(|| pending.base.clone())
                .unwrap_or(ListEntry {
                    media_id,
                    status: "CURRENT".into(),
                    ..Default::default()
                });
            pending.patch.apply(&mut entry);
            if stored.is_none() {
                entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
            }
            if stored.as_ref().is_none_or(|stored| {
                stored.status != entry.status
                    || stored.progress != entry.progress
                    || stored.score != entry.score
                    || stored.repeat != entry.repeat
            }) {
                state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
                matchers_changed |= stored
                    .as_ref()
                    .is_none_or(|stored| stored.status != entry.status);
            }
            Ok(Some(entry))
        })
        .collect::<Result<Vec<_>, String>>();
    if matchers_changed {
        state.refresh_matchers();
    }
    entries
}

#[tauri::command]
pub async fn get_entry(
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<Option<ListEntry>, String> {
    let Some(account) = crate::sync_queue::account_id(&state.db) else {
        return Ok(None);
    };
    local_entry_updates_inner(state.inner(), &[media_id], account)
        .await
        .map(|mut entries| entries.remove(0))
}

const MEDIA_GONE: &str =
    "AniList no longer has this anime. It was likely merged into another entry. Search for it to re-add the correct one.";

/// Reaching the total completes unless another status is chosen.
/// Completed fills progress to the known total.
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
    let status = st.map(|status| status.as_str().to_string());
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
    let requested_progress = progress;
    let (status, repeat) = complete_progress_edit(status, progress, repeat, total, before.as_ref());
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
                let entry = ListEntry {
                    id: Some(remote.id),
                    media_id,
                    status: remote.status,
                    progress: remote.progress,
                    score: remote.score,
                    repeat: remote.repeat,
                    updated_at: remote.updated_at,
                    ..Default::default()
                };
                state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
                let (status, repeat) =
                    complete_progress_edit(status, requested_progress, repeat, total, Some(&entry));
                state.refresh_matchers();
                drop(_write);
                return save_entry_unlocked(
                    state,
                    media_id,
                    status,
                    if remote_rewatch { Some(0) } else { progress },
                    score,
                    repeat,
                    false,
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
    save_entry_unlocked(state, media_id, status, progress, score, repeat, false).await
}

fn complete_progress_edit(
    status: Option<String>,
    progress: Option<i64>,
    repeat: Option<i64>,
    total: Option<i64>,
    before: Option<&ListEntry>,
) -> (Option<String>, Option<i64>) {
    if !progress.is_some_and(|p| at_last_episode(total, p))
        || status
            .as_deref()
            .is_some_and(|s| !s.eq_ignore_ascii_case("COMPLETED"))
    {
        return (status, repeat);
    }
    let repeat = repeat.or_else(|| {
        before
            .filter(|entry| {
                entry.status == "REPEATING" && progress.is_some_and(|p| entry.progress < p)
            })
            .map(|entry| entry.repeat + 1)
    });
    let status = if before.is_none_or(|entry| entry.status != "COMPLETED") {
        Some("COMPLETED".into())
    } else {
        status
    };
    (status, repeat)
}

/// Caller holds remote_lock and sends only changed fields.
async fn save_entry_unlocked(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    check_remote: bool,
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
    if check_remote && staged.is_none() {
        return Err("Reconnect to AniList before tracking progress for this account.".into());
    }
    let entry_id = before.as_ref().and_then(|entry| entry.id);
    drop(_write);
    // Automatic playback must respect the same baseline as queued retries.
    let preflight = if let Some((account, pending, _)) = staged.as_ref().filter(|_| check_remote) {
        match al.entry_by_media_id(media_id).await {
            Ok(remote) => {
                let snapshot = remote
                    .as_ref()
                    .map(|entry| crate::sync_queue::remote_entry(media_id, entry.clone()));
                let _write = state.entry_lock.lock().await;
                if snapshot
                    .as_ref()
                    .is_some_and(|entry| crate::sync_queue::already_applied(pending, entry))
                {
                    Ok(remote)
                } else if crate::sync_queue::conflict(pending, snapshot.as_ref()) {
                    let mut pending = pending.clone();
                    pending.conflict = true;
                    pending.error = Some("AniList changed before this playback update. Choose which version to keep.".into());
                    state
                        .db
                        .put_pending(*account, &pending)
                        .map_err(|e| e.to_string())?;
                    return crate::sync_queue::local_entry(state, &pending)
                        .map_err(|e| e.to_string());
                } else {
                    Ok(None)
                }
            }
            Err(error) if anilist::media_not_found(&error) => {
                let _write = state.entry_lock.lock().await;
                crate::sync_queue::mark_media_missing(state, media_id)
                    .map_err(|e| e.to_string())?;
                return Err(MEDIA_GONE.to_string());
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(None)
    };
    let saved = match preflight {
        Err(error) => Err(error),
        Ok(Some(saved)) => Ok(saved),
        Ok(None) => match entry_id {
            Some(id) => al.save_entry_by_id(id, st, progress, score, repeat).await,
            None => al.save_entry(media_id, st, progress, score, repeat).await,
        },
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
        updated_at: Some(
            saved
                .updated_at
                .unwrap_or_else(|| chrono::Utc::now().timestamp()),
        ),
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
        false,
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
        true,
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
pub async fn get_library_folders(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    get_library_folders_inner(state.inner()).await
}

async fn library_db_work<T: Send + 'static>(
    db: Arc<Db>,
    work: impl FnOnce(&Db) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || work(&db))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

async fn get_library_folders_inner(state: &AppState) -> Result<Vec<String>, String> {
    library_db_work(state.db.clone(), library::get_folders).await
}

#[tauri::command]
pub async fn add_library_folder(
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    add_library_folder_inner(path, state.inner()).await
}

async fn add_library_folder_inner(path: String, state: &AppState) -> Result<Vec<String>, String> {
    library_db_work(state.db.clone(), move |db| library::add_folder(db, &path)).await
}

#[tauri::command]
pub async fn remove_library_folder(
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    remove_library_folder_inner(path, state.inner()).await
}

async fn remove_library_folder_inner(
    path: String,
    state: &AppState,
) -> Result<Vec<String>, String> {
    library_db_work(state.db.clone(), move |db| {
        library::remove_folder(db, &path)
    })
    .await
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
pub async fn bind_library_path(
    path: String,
    media_id: i64,
    episode_offset: Option<i64>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    bind_library_path_inner(path, media_id, episode_offset, state.inner()).await
}

async fn bind_library_path_inner(
    path: String,
    media_id: i64,
    episode_offset: Option<i64>,
    state: &AppState,
) -> Result<(), String> {
    let expected_token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    library_db_work(state.db.clone(), move |db| {
        if db.get_entry(media_id)?.is_none() {
            return Err(anyhow::anyhow!("only shows on your list can be linked"));
        }
        library::bind_path(db, &path, media_id, episode_offset.unwrap_or(0))
    })
    .await
}

#[tauri::command]
pub async fn library_binding_for(
    path: String,
    state: State<'_, AppState>,
) -> Result<Option<i64>, String> {
    library_binding_for_inner(path, state.inner()).await
}

async fn library_binding_for_inner(path: String, state: &AppState) -> Result<Option<i64>, String> {
    library_db_work(state.db.clone(), move |db| {
        library::binding_for_exact(db, &path).map(|binding| binding.map(|b| b.media_id))
    })
    .await
}

#[tauri::command]
pub async fn library_binding_details(
    path: String,
    state: State<'_, AppState>,
) -> Result<Option<library::LibraryBinding>, String> {
    library_binding_details_inner(path, state.inner()).await
}

async fn library_binding_details_inner(
    path: String,
    state: &AppState,
) -> Result<Option<library::LibraryBinding>, String> {
    let expected_token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    library_db_work(state.db.clone(), move |db| {
        library::effective_binding(db, &path)
    })
    .await
}

#[tauri::command]
pub async fn get_watch_history(
    limit: Option<i64>,
    before: Option<i64>,
    state: State<'_, AppState>,
) -> Result<Vec<crate::sync_queue::WatchHistoryItem>, String> {
    get_watch_history_inner(limit, before, state.inner()).await
}

async fn get_watch_history_inner(
    limit: Option<i64>,
    before: Option<i64>,
    state: &AppState,
) -> Result<Vec<crate::sync_queue::WatchHistoryItem>, String> {
    let expected_token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    library_db_work(state.db.clone(), move |db| {
        let Some(account) = crate::sync_queue::account_id(db) else {
            return Ok(vec![]);
        };
        db.watch_history(account, limit.unwrap_or(50).clamp(1, 100), before)
    })
    .await
}

#[tauri::command]
pub async fn unbind_library_media(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    unbind_library_media_inner(media_id, state.inner()).await
}

async fn unbind_library_media_inner(media_id: i64, state: &AppState) -> Result<(), String> {
    library_db_work(state.db.clone(), move |db| {
        library::unbind_media(db, media_id)
    })
    .await
}

#[cfg(test)]
#[path = "library_command_tests.rs"]
mod library_command_tests;

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
            // Episodes beyond the total may belong to another season, so hide new flags.
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
    // Retain seen marks for current feed items to avoid flagging old releases.
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
    // Serialize installs to prevent scratch file collisions.
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

        // Install through the verified handle to prevent replacement races.
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
            // Hold the verified handle through spawn to prevent replacement.
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
        assert_eq!(read_app_setting(&db, "close_to_tray"), Ok(None));
        db.set_setting("close_to_tray", "1").unwrap();
        assert_eq!(read_app_setting(&db, "close_to_tray"), Ok(Some("1".into())));
        assert!(read_app_setting(&db, TOKEN_KEY).is_err());
        assert!(read_app_setting(&db, "hardware_acceleration").is_err());
        db.0.lock().execute_batch("DROP TABLE settings").unwrap();
        assert!(read_app_setting(&db, "close_to_tray").is_err());
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

    #[tokio::test]
    async fn local_entry_updates_read_requested_rows_and_repair_their_pending_overlays() {
        let state = test_state();
        state
            .anilist
            .lock()
            .set_token(Some("synthetic-session".into()));
        state
            .db
            .replace_account(
                "synthetic-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        seed(&state, Some(12), "CURRENT", 3, 0);
        let pending = crate::sync_queue::Pending {
            media_id: 1,
            base: state.db.get_entry(1).unwrap(),
            patch: crate::sync_queue::Patch {
                status: Some("REPEATING".into()),
                progress: Some(8),
                score: Some(90.0),
                repeat: Some(4),
            },
            error: Some("Synthetic conflict".into()),
            conflict: true,
            missing_media: false,
        };
        state.db.put_pending(7, &pending).unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                media_id: 2,
                status: "CURRENT".into(),
                progress: 3,
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .put_pending(
                7,
                &crate::sync_queue::Pending {
                    media_id: 2,
                    missing_media: true,
                    ..pending.clone()
                },
            )
            .unwrap();
        state
            .db
            .put_pending(
                8,
                &crate::sync_queue::Pending {
                    media_id: 3,
                    ..pending.clone()
                },
            )
            .unwrap();
        state
            .db
            .put_pending(
                7,
                &crate::sync_queue::Pending {
                    media_id: 4,
                    base: None,
                    ..pending
                },
            )
            .unwrap();
        state.db.0.lock().execute_batch(
            "INSERT INTO list_entry (media_id,status,progress,repeat) VALUES (99,'CURRENT','invalid',0)"
        ).unwrap();

        let result = local_entry_updates_inner(&state, &[1, 2, 3, 4, 5], 7)
            .await
            .unwrap();
        let first = result[0].as_ref().unwrap();
        assert_eq!(
            (
                first.media_id,
                first.status.as_str(),
                first.progress,
                first.score,
                first.repeat
            ),
            (1, "REPEATING", 8, Some(90.0), 4)
        );
        assert_eq!(first.media.as_ref().unwrap().episodes, Some(12));
        assert!(result[1].is_none());
        assert!(state.db.get_entry(2).unwrap().is_none());
        assert!(set_progress_inner(&state, 2, 4, Some(3)).await.is_err());
        assert!(result[2].is_none());
        assert_eq!(result[3].as_ref().unwrap().media_id, 4);
        assert_eq!(result[3].as_ref().unwrap().progress, 8);
        assert!(result[4].is_none());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 8);
        assert_eq!(state.db.get_entry(4).unwrap().unwrap().progress, 8);
        let changes = state.db.0.lock().total_changes();
        local_entry_updates_inner(&state, &[1, 2, 3, 4, 5], 7)
            .await
            .unwrap();
        assert_eq!(state.db.0.lock().total_changes(), changes);
        assert!(state.db.entries_with_media().is_err());
    }

    #[tokio::test]
    async fn local_entry_updates_restore_the_expected_baseline_for_the_next_progress_edit() {
        for stored in [false, true] {
            let state = test_state();
            state
                .anilist
                .lock()
                .set_token(Some("synthetic-session".into()));
            state
                .db
                .replace_account(
                    "synthetic-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            seed(&state, Some(12), "CURRENT", 3, 0);
            let base = state.db.get_entry(1).unwrap();
            state
                .db
                .put_pending(
                    7,
                    &crate::sync_queue::Pending {
                        media_id: 1,
                        base,
                        patch: crate::sync_queue::Patch {
                            progress: Some(8),
                            ..Default::default()
                        },
                        error: None,
                        conflict: false,
                        missing_media: false,
                    },
                )
                .unwrap();
            if !stored {
                state.db.delete_entry(1).unwrap();
            }
            let entry = local_entry_updates_inner(&state, &[1], 7)
                .await
                .unwrap()
                .remove(0)
                .unwrap();
            assert_eq!(entry.progress, 8);
            let next = set_progress_inner(&state, 1, 9, Some(entry.progress))
                .await
                .unwrap();
            assert_eq!(next.progress, 9);
            assert_eq!(
                state
                    .db
                    .pending_change(7, 1)
                    .unwrap()
                    .unwrap()
                    .patch
                    .progress,
                Some(9)
            );
            let stale = set_progress_inner(&state, 1, 10, Some(entry.progress))
                .await
                .unwrap();
            assert_eq!(stale.progress, 9);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 9);
        }
    }

    #[tokio::test]
    async fn local_entry_updates_refresh_repaired_matchers_before_a_later_row_error() {
        let state = test_state();
        state
            .anilist
            .lock()
            .set_token(Some("synthetic-session".into()));
        state
            .db
            .replace_account(
                "synthetic-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        seed(&state, Some(12), "CURRENT", 3, 0);
        let mut media = state.db.get_media(1).unwrap().unwrap();
        media.title_english = Some("Synthetic Show".into());
        state.db.upsert_media(&media).unwrap();
        state.refresh_matchers();
        let before = state.matchers.lock().clone();
        assert_eq!(before.len(), 1);
        state
            .db
            .put_pending(
                7,
                &crate::sync_queue::Pending {
                    media_id: 1,
                    base: state.db.get_entry(1).unwrap(),
                    patch: crate::sync_queue::Patch {
                        status: Some("REPEATING".into()),
                        ..Default::default()
                    },
                    error: None,
                    conflict: false,
                    missing_media: false,
                },
            )
            .unwrap();
        state.db.0.lock().execute_batch(
            "INSERT INTO list_entry (media_id,status,progress,repeat) VALUES (99,'CURRENT','invalid',0)"
        ).unwrap();

        assert!(local_entry_updates_inner(&state, &[1, 99], 7)
            .await
            .is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "REPEATING");
        let repaired = state.matchers.lock().clone();
        assert!(!Arc::ptr_eq(&before, &repaired));
        state.refresh_matchers();
        assert!(Arc::ptr_eq(&repaired, &state.matchers.lock()));
    }

    #[tokio::test]
    async fn local_entry_updates_reject_a_different_account_or_logout() {
        let state = test_state();
        state
            .anilist
            .lock()
            .set_token(Some("synthetic-session".into()));
        state
            .db
            .replace_account(
                "synthetic-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        seed(&state, Some(12), "CURRENT", 3, 0);
        assert!(local_entry_updates_inner(&state, &[1], 8).await.is_err());
        state.anilist.lock().set_token(None);
        assert!(local_entry_updates_inner(&state, &[1], 7).await.is_err());
    }

    #[tokio::test]
    async fn local_entry_updates_wait_for_writes_and_recheck_the_session() {
        let state = test_state();
        state
            .anilist
            .lock()
            .set_token(Some("synthetic-session".into()));
        state
            .db
            .replace_account(
                "synthetic-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        seed(&state, Some(12), "CURRENT", 3, 0);
        let guard = state.entry_lock.lock().await;
        let read = local_entry_updates_inner(&state, &[1], 7);
        tokio::pin!(read);
        tokio::select! {
            biased;
            result = &mut read => panic!("read bypassed the write lock: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        state
            .anilist
            .lock()
            .set_token(Some("replacement-session".into()));
        drop(guard);
        assert_eq!(read.await.unwrap_err(), "Account changed");

        let guard = state.entry_lock.lock().await;
        let read = local_entry_updates_inner(&state, &[1], 7);
        tokio::pin!(read);
        tokio::select! {
            biased;
            result = &mut read => panic!("read bypassed the write lock: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.progress = 6;
        state.db.upsert_entry(&entry).unwrap();
        drop(guard);
        assert_eq!(read.await.unwrap()[0].as_ref().unwrap().progress, 6);
    }

    #[test]
    fn unchanged_search_cache_writes_preserve_matchers_and_scanned_recognition() {
        let state = test_state();
        let mut media = Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
            format: Some("TV".into()),
            ..Default::default()
        };
        state.db.upsert_media(&media).unwrap();
        let mut entry = ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        };
        state.db.upsert_entry(&entry).unwrap();
        state.refresh_matchers();
        let original = state.matchers.lock().clone();
        let dir = std::env::temp_dir().join(format!(
            "kurisu-matcher-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Some Show - 03.mkv"), []).unwrap();
        let folders = vec![dir.to_str().unwrap().to_string()];
        let bindings = std::collections::HashMap::new();
        let first = state
            .library_cache
            .lock()
            .scan(&folders, &original, &bindings);
        assert_eq!(first.files[0].episode, Some(3));

        let before = recognize::matcher_work_counts();
        state.cache_media_batch(&[Media {
            id: 2,
            title_english: Some("Unlisted Show".into()),
            ..Default::default()
        }]);
        assert_eq!(recognize::matcher_work_counts(), before);
        for _ in 0..3 {
            state.cache_media_batch(&[media.clone()]);
        }
        media.source = Some("MANGA".into());
        media.cover_large = Some("synthetic-cover".into());
        media.next_airing_episode = Some(4);
        state.cache_media_batch(&[media.clone()]);
        assert_eq!(recognize::matcher_work_counts(), (before.0 + 4, before.1));
        entry.id = Some(10);
        entry.progress = 2;
        entry.score = Some(8.0);
        entry.repeat = 3;
        entry.updated_at = Some(123);
        state.db.upsert_entry(&entry).unwrap();
        state.refresh_matchers();
        let unchanged = state.matchers.lock().clone();
        assert!(Arc::ptr_eq(&original, &unchanged));
        assert_eq!(recognize::matcher_work_counts().1, before.1);
        let second = state
            .library_cache
            .lock()
            .scan(&folders, &unchanged, &bindings);
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        assert_eq!(state.library_cache.lock().recognized_count(), 1);

        media.episodes = Some(2);
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_cache BEFORE INSERT ON media BEGIN SELECT RAISE(ABORT, 'injected'); END",
            )
            .unwrap();
        state.cache_media_batch(&[media.clone()]);
        assert!(Arc::ptr_eq(&original, &state.matchers.lock()));
        assert_eq!(state.db.get_media(1).unwrap().unwrap().episodes, Some(12));
        assert_eq!(recognize::matcher_work_counts().1, before.1);
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_cache")
            .unwrap();
        state.cache_media_batch(&[media]);
        let changed = state.matchers.lock().clone();
        assert!(!Arc::ptr_eq(&original, &changed));
        let scan = state
            .library_cache
            .lock()
            .scan(&folders, &changed, &bindings);
        assert_eq!(scan.files[0].episode, None);
        assert_eq!(state.library_cache.lock().recognized_count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejected_sessions_invalidate_even_an_empty_matcher_cache() {
        let state = test_state();
        let before = state.matchers.lock().clone();
        clear_rejected_session(&state);
        assert!(!Arc::ptr_eq(&before, &state.matchers.lock()));
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
    async fn oauth_callback_exchanges_the_code_before_saving_the_verified_account() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"access_token":"exchanged-token"})),
            (200, viewer_response(7, "Tester")),
        ])
        .await;
        let state = state_with_api(api);
        let intent = state.begin_auth_intent();
        let (oauth_state, port, receiver) = start_oauth_callback(&state, intent, 0).await.unwrap();
        let callback = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{port}/?code=one-use-code&state={oauth_state}"
            ))
            .send()
            .await
            .unwrap();
        assert!(callback.status().is_success());
        let user = finish_oauth_login(&state, intent, receiver).await.unwrap();
        assert_eq!(user.id, 7);
        assert_eq!(
            requests.recv().await.unwrap(),
            json!({"code":"one-use-code"})
        );
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert_eq!(
            state.db.get_setting(TOKEN_KEY).unwrap().as_deref(),
            Some("exchanged-token")
        );
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("exchanged-token")
        );
    }

    #[tokio::test]
    async fn failed_oauth_exchange_preserves_the_previous_session() {
        let (api, mut requests) =
            anilist::mock_api(vec![(400, json!({"error":"invalid_grant"}))]).await;
        let state = state_with_api(api);
        let intent = state.begin_auth_intent();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(Ok("expired-code".into())).unwrap();
        let error = finish_oauth_login(&state, intent, receiver)
            .await
            .unwrap_err();
        assert!(error.contains("Start sign-in again"));
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("test-session")
        );
        assert_eq!(
            requests.recv().await.unwrap(),
            json!({"code":"expired-code"})
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_superseded_oauth_callback_cannot_exchange_its_code() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state_with_api(api);
        let intent = state.begin_auth_intent();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(Ok("old-code".into())).unwrap();
        state.begin_auth_intent();
        assert_eq!(
            finish_oauth_login(&state, intent, receiver)
                .await
                .unwrap_err(),
            AUTH_REPLACED
        );
        assert!(requests.try_recv().is_err());
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
        let first =
            tokio::spawn(
                async move { wait_for_oauth_code(&first_state, first_intent, receiver).await },
            );
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
    async fn incomplete_detail_replies_preserve_the_complete_cached_snapshot() {
        let complete = json!({
            "id": 1, "idMal": null,
            "title": {"romaji": null, "english": "Saved show", "native": null},
            "coverImage": {"medium": null, "large": null},
            "episodes": 12, "format": "TV", "status": "FINISHED",
            "averageScore": null, "season": null, "seasonYear": null,
            "description": "Saved description", "nextAiringEpisode": null,
            "bannerImage": "saved.jpg", "genres": ["Action"], "duration": 24,
            "source": "MANGA", "studios": {"nodes": [{"name": "Saved studio"}]},
            "relations": {"edges": []}, "characters": {"edges": []}, "staff": {"edges": []}
        });
        for field in [
            "id",
            "idMal",
            "title",
            "coverImage",
            "episodes",
            "format",
            "status",
            "averageScore",
            "season",
            "seasonYear",
            "description",
            "nextAiringEpisode",
            "bannerImage",
            "genres",
            "duration",
            "source",
            "studios",
        ] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            let (api, _) = anilist::mock_api(vec![
                (200, json!({"data": {"Media": complete}})),
                (200, json!({"data": {"Media": incomplete}})),
            ])
            .await;
            let state = state(api);
            get_media_detail_inner(1, &state).await.unwrap();

            let found = get_media_detail_inner(1, &state).await.unwrap();
            assert!(
                found.warning.is_some(),
                "missing {field} was accepted as fresh metadata"
            );
            assert_eq!(found.media.id, 1, "missing {field}");
            assert_eq!(found.media.episodes, Some(12), "missing {field}");
            assert_eq!(
                found.media.banner_image.as_deref(),
                Some("saved.jpg"),
                "missing {field}"
            );
            let stored = state.db.get_media(1).unwrap().unwrap();
            assert_eq!(stored.episodes, Some(12), "missing {field}");
            assert_eq!(
                stored.description.as_deref(),
                Some("Saved description"),
                "missing {field}"
            );
            assert_eq!(
                stored.genres,
                Some(vec!["Action".into()]),
                "missing {field}"
            );
            assert!(state.db.get_media(0).unwrap().is_none(), "missing {field}");
            assert!(state.anilist.lock().has_token(), "missing {field}");
        }
    }

    #[tokio::test]
    async fn unexpected_detail_identity_preserves_the_requested_cached_show() {
        let mut saved = anilist::mock_media(1);
        saved["title"]["english"] = json!("Saved show");
        saved["episodes"] = json!(12);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (200, json!({"data":{"Media":anilist::mock_media(2)}})),
        ])
        .await;
        let state = state(api);
        get_media_detail_inner(1, &state).await.unwrap();

        let found = get_media_detail_inner(1, &state).await.unwrap();
        assert!(found.warning.is_some());
        assert_eq!(found.media.id, 1);
        assert_eq!(found.media.episodes, Some(12));
        assert!(state.db.get_media(2).unwrap().is_none());
        let stored: MediaDetail =
            serde_json::from_str(&state.db.get_setting("media_detail:1").unwrap().unwrap())
                .unwrap();
        assert_eq!(stored.media.id, 1);
        assert!(state.anilist.lock().has_token());
    }

    #[tokio::test]
    async fn explicit_null_detail_fields_still_clear_cached_values() {
        let mut saved = anilist::mock_media(1);
        saved["episodes"] = json!(12);
        saved["bannerImage"] = json!("saved.jpg");
        saved["genres"] = json!(["Action"]);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (200, json!({"data":{"Media":anilist::mock_media(1)}})),
        ])
        .await;
        let state = state(api);
        get_media_detail_inner(1, &state).await.unwrap();

        let found = get_media_detail_inner(1, &state).await.unwrap();
        assert!(found.warning.is_none());
        assert_eq!(found.media.id, 1);
        assert_eq!(found.media.episodes, None);
        assert_eq!(found.media.banner_image, None);
        assert_eq!(found.media.genres, None);
        let stored = state.db.get_media(1).unwrap().unwrap();
        assert_eq!(stored.episodes, None);
        assert_eq!(stored.banner_image, None);
        assert_eq!(stored.genres, None);
    }

    #[tokio::test]
    async fn incomplete_statistics_replies_preserve_cached_statistics() {
        let complete = json!({
            "count": 3, "episodesWatched": 36, "minutesWatched": 864,
            "meanScore": 80.0, "standardDeviation": 4.0,
            "scores": [{"score": 80, "count": 3}],
            "statuses": [{"status": "COMPLETED", "count": 3}],
            "formats": [{"format": "TV", "count": 3}],
            "genres": [{"genre": "Action", "count": 3, "minutesWatched": 864}],
            "releaseYears": [{"releaseYear": 2024, "count": 3}]
        });
        for field in [
            "count",
            "episodesWatched",
            "minutesWatched",
            "meanScore",
            "standardDeviation",
            "scores",
            "statuses",
            "formats",
            "genres",
            "releaseYears",
            "statuses.status",
            "formats.format",
            "genres.genre",
            "genres.minutesWatched",
            "releaseYears.releaseYear",
        ] {
            let mut incomplete = complete.clone();
            if let Some((bucket, nested)) = field.split_once('.') {
                incomplete[bucket][0]
                    .as_object_mut()
                    .unwrap()
                    .remove(nested);
            } else {
                incomplete.as_object_mut().unwrap().remove(field);
            }
            let (api, _) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"User":{"statistics":{"anime":complete}}}}),
                ),
                (
                    200,
                    json!({"data":{"User":{"statistics":{"anime":incomplete}}}}),
                ),
            ])
            .await;
            let state = state(api);
            get_user_stats_inner(&state).await.unwrap();

            let found = get_user_stats_inner(&state).await.unwrap();
            assert!(
                found.warning.is_some(),
                "missing {field} was accepted as fresh statistics"
            );
            assert_eq!(found.count, 3, "missing {field}");
            assert_eq!(found.episodes_watched, 36, "missing {field}");
            assert_eq!(found.scores[0].score, 80, "missing {field}");
            let stored: UserStats =
                serde_json::from_str(&state.db.get_setting("user_stats:7").unwrap().unwrap())
                    .unwrap();
            assert_eq!(stored.count, 3, "missing {field}");
            assert_eq!(stored.episodes_watched, 36, "missing {field}");
            assert!(state.anilist.lock().has_token(), "missing {field}");
        }
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
        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Some Movie");
        media["episodes"] = json!(1);
        let (api, _) = anilist::mock_api(vec![(200, json!({"data":{"Media":media}}))]).await;
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
    async fn incomplete_metadata_preserves_cached_fields_and_does_not_report_saved() {
        let complete = json!({"id":22,"notes":"Remote note",
            "startedAt":{"year":2024,"month":1,"day":2},
            "completedAt":{"year":2024,"month":2,"day":3},"customLists":{}});
        for missing in [
            "notes",
            "startedAt",
            "completedAt",
            "customLists",
            "startedAt.year",
            "startedAt.month",
            "startedAt.day",
            "completedAt.year",
            "completedAt.month",
            "completedAt.day",
        ] {
            for operation in ["read", "preflight", "save"] {
                let mut incomplete = complete.clone();
                if let Some((parent, field)) = missing.split_once('.') {
                    incomplete[parent].as_object_mut().unwrap().remove(field);
                } else {
                    incomplete.as_object_mut().unwrap().remove(missing);
                }
                let lookup = if operation == "save" {
                    complete.clone()
                } else {
                    incomplete.clone()
                };
                let (api, mut requests) = anilist::mock_api(vec![
                    (200, json!({"data":{"Media":{"mediaListEntry":lookup}}})),
                    (200, json!({"data":{"SaveMediaListEntry":incomplete}})),
                ])
                .await;
                let state = state(api);
                pending_progress(&state);
                let cached = EntryDetails {
                    notes: "Saved note".into(),
                    started_at: Some(FuzzyDate {
                        year: Some(2024),
                        month: Some(1),
                        day: Some(2),
                    }),
                    completed_at: Some(FuzzyDate {
                        year: Some(2024),
                        month: Some(2),
                        day: Some(3),
                    }),
                    custom_lists: vec!["Favorites".into()],
                    cached_at: Some(100),
                    ..Default::default()
                };
                let saved = serde_json::to_string(&cached).unwrap();
                state.db.set_setting("entry_details:7:1", &saved).unwrap();
                if operation == "read" {
                    let recovered = get_entry_details_inner(1, &state).await.unwrap();
                    assert!(
                        recovered.warning.is_some(),
                        "missing {missing} was accepted as fresh metadata"
                    );
                    assert_eq!(recovered.notes, cached.notes);
                    assert_eq!(recovered.started_at, cached.started_at);
                    assert_eq!(recovered.completed_at, cached.completed_at);
                    assert_eq!(recovered.custom_lists, cached.custom_lists);
                    assert_eq!(recovered.cached_at, Some(100));
                } else {
                    assert!(
                        update_entry_details_inner(
                            1,
                            Some("New note".into()),
                            None,
                            None,
                            None,
                            &state
                        )
                        .await
                        .is_err(),
                        "incomplete {operation} with missing {missing} reported success"
                    );
                }
                requests.recv().await.unwrap();
                if operation == "save" {
                    assert_eq!(
                        requests.recv().await.unwrap()["variables"]["notes"],
                        "New note"
                    );
                } else {
                    assert!(
                        requests.try_recv().is_err(),
                        "incomplete preflight must not send a mutation"
                    );
                }
                assert_eq!(
                    state.db.get_setting("entry_details:7:1").unwrap(),
                    Some(saved)
                );
                assert!(state.anilist.lock().has_token());
                assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
            }
        }
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
                    json!({"data":{"SaveMediaListEntry":{"notes":"New note","startedAt":null,"completedAt":null,"customLists":{}}}}),
                )
            };
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
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
            (200, json!({"data":{"Media":{"mediaListEntry":{"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
            (200, json!({"data":{"SaveMediaListEntry":{"notes":"New note","startedAt":null,"completedAt":null,"customLists":{}}}})),
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
                responses.push((200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})));
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
                (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
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

    #[tokio::test]
    async fn accepted_status_spellings_match_online_and_offline() {
        for (requested, canonical) in [
            ("watching", "CURRENT"),
            ("plan_to_watch", "PLANNING"),
            ("completed", "COMPLETED"),
            ("repeating", "REPEATING"),
        ] {
            for offline in [false, true] {
                let progress = if canonical == "COMPLETED" { 12 } else { 3 };
                let mut before = remote();
                before["status"] = json!("PAUSED");
                before["progress"] = json!(3);
                let mut after = before.clone();
                after["status"] = json!(canonical);
                after["progress"] = json!(progress);
                let responses = if offline {
                    vec![
                        (503, json!({"errors":[{"message":"Unavailable"}]})),
                        (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
                        (200, json!({"data":{"SaveMediaListEntry":after}})),
                    ]
                } else {
                    vec![(200, json!({"data":{"SaveMediaListEntry":after}}))]
                };
                let (api, mut requests) = anilist::mock_api(responses).await;
                let state = state(api);
                let mut entry = state.db.get_entry(1).unwrap().unwrap();
                entry.status = "PAUSED".into();
                entry.progress = 3;
                state.db.upsert_entry(&entry).unwrap();

                let saved = save_entry_inner(&state, 1, Some(requested.into()), None, None, None)
                    .await
                    .unwrap();

                assert_eq!(saved.status, canonical, "{requested}, offline={offline}");
                assert_eq!(saved.progress, progress);
                let sent = requests.recv().await.unwrap();
                assert_eq!(sent["variables"]["status"], canonical);
                if offline {
                    assert_eq!(
                        state.db.pending(7).unwrap()[0].patch.status.as_deref(),
                        Some(canonical)
                    );
                    assert_eq!(
                        local_entries_inner(&state).await.unwrap()[0].status,
                        canonical
                    );
                    crate::sync_queue::flush_unlocked(&state).await.unwrap();
                    requests.recv().await.unwrap();
                    assert_eq!(
                        requests.recv().await.unwrap()["variables"],
                        sent["variables"]
                    );
                }
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, canonical);
                assert!(state.db.pending(7).unwrap().is_empty());
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn final_episode_edits_complete_online_and_after_offline_retry() {
        for offline in [false, true] {
            for stepper in [false, true] {
                for previous_status in ["CURRENT", "REPEATING"] {
                    let mut before = remote();
                    before["status"] = json!(previous_status);
                    before["progress"] = json!(11);
                    let mut after = remote();
                    let finishing_rewatch = previous_status == "REPEATING";
                    after["repeat"] = json!(if finishing_rewatch { 3 } else { 2 });
                    let responses = if offline {
                        vec![
                            (503, json!({"errors":[{"message":"Unavailable"}]})),
                            (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
                            (200, json!({"data":{"SaveMediaListEntry":after}})),
                        ]
                    } else {
                        vec![(200, json!({"data":{"SaveMediaListEntry":after}}))]
                    };
                    let (api, mut requests) = anilist::mock_api(responses).await;
                    let state = state(api);
                    let mut entry = state.db.get_entry(1).unwrap().unwrap();
                    entry.status = previous_status.into();
                    entry.progress = 11;
                    state.db.upsert_entry(&entry).unwrap();

                    let saved = if stepper {
                        set_progress_inner(&state, 1, 12, Some(11)).await
                    } else {
                        save_entry_inner(&state, 1, None, Some(12), None, None).await
                    }
                    .unwrap();
                    assert_eq!((saved.status.as_str(), saved.progress), ("COMPLETED", 12));
                    assert_eq!(saved.repeat, if finishing_rewatch { 3 } else { 2 });
                    let mut expected = json!({"id":22,"status":"COMPLETED","progress":12});
                    if finishing_rewatch {
                        expected["repeat"] = json!(3);
                    }
                    assert_eq!(requests.recv().await.unwrap()["variables"], expected);
                    if offline {
                        let pending = state.db.pending(7).unwrap();
                        assert_eq!(pending[0].patch.status.as_deref(), Some("COMPLETED"));
                        assert_eq!(pending[0].patch.progress, Some(12));
                        assert_eq!(pending[0].patch.repeat, finishing_rewatch.then_some(3));
                        crate::sync_queue::flush_unlocked(&state).await.unwrap();
                        requests.recv().await.unwrap();
                        assert_eq!(requests.recv().await.unwrap()["variables"], expected);
                    }
                    let cached = state.db.get_entry(1).unwrap().unwrap();
                    assert_eq!((cached.status.as_str(), cached.progress), ("COMPLETED", 12));
                    assert_eq!(cached.repeat, saved.repeat);
                    assert_eq!(cached.score, Some(70.0));
                    assert!(state.db.pending(7).unwrap().is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn final_episode_edits_preserve_explicit_status_and_repeat_choices() {
        for (total, progress, status, repeat, previous_progress, expected) in [
            (
                Some(12),
                Some(11),
                None,
                None,
                10,
                json!({"id":22,"progress":11}),
            ),
            (
                Some(12),
                Some(13),
                None,
                None,
                11,
                json!({"id":22,"progress":12}),
            ),
            (
                None,
                Some(12),
                None,
                None,
                11,
                json!({"id":22,"progress":12}),
            ),
            (
                Some(0),
                Some(0),
                None,
                None,
                0,
                json!({"id":22,"progress":0}),
            ),
            (
                Some(12),
                Some(12),
                Some("PAUSED"),
                None,
                11,
                json!({"id":22,"status":"PAUSED","progress":12}),
            ),
            (
                Some(12),
                Some(12),
                Some("CURRENT"),
                None,
                11,
                json!({"id":22,"status":"CURRENT","progress":12}),
            ),
            (
                Some(12),
                Some(12),
                Some("REPEATING"),
                None,
                11,
                json!({"id":22,"status":"REPEATING","progress":12}),
            ),
            (
                Some(12),
                Some(12),
                Some("completed"),
                None,
                11,
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":3}),
            ),
            (
                Some(12),
                Some(12),
                Some("COMPLETED"),
                Some(9),
                11,
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":9}),
            ),
            (
                Some(12),
                None,
                Some("COMPLETED"),
                None,
                11,
                json!({"id":22,"status":"COMPLETED","progress":12}),
            ),
            (
                Some(12),
                Some(12),
                None,
                None,
                12,
                json!({"id":22,"status":"COMPLETED","progress":12}),
            ),
        ] {
            let mut reply = remote();
            reply["status"] = expected
                .get("status")
                .cloned()
                .unwrap_or(json!("REPEATING"));
            reply["progress"] = expected["progress"].clone();
            reply["repeat"] = expected.get("repeat").cloned().unwrap_or(json!(2));
            let (api, mut requests) =
                anilist::mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":reply}}))]).await;
            let state = state(api);
            let mut entry = state.db.get_entry(1).unwrap().unwrap();
            entry.status = "REPEATING".into();
            entry.progress = previous_progress;
            state.db.upsert_entry(&entry).unwrap();
            let mut media = state.db.get_media(1).unwrap().unwrap();
            media.episodes = total;
            state.db.upsert_media(&media).unwrap();

            save_entry_inner(&state, 1, status.map(String::from), progress, None, repeat)
                .await
                .unwrap();

            assert_eq!(requests.recv().await.unwrap()["variables"], expected);
            let cached = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(cached.status, reply["status"]);
            assert_eq!(cached.progress, reply["progress"]);
            assert_eq!(cached.repeat, reply["repeat"]);
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_final_episode_cache_miss_uses_the_remote_rewatch_count_once() {
        for explicit_completion in [false, true] {
            let mut before = remote();
            before["status"] = json!("REPEATING");
            before["progress"] = json!(11);
            before["repeat"] = json!(5);
            let mut after = remote();
            after["repeat"] = json!(6);
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
                (200, json!({"data":{"SaveMediaListEntry":after}})),
                (200, json!({"data":{"SaveMediaListEntry":after}})),
            ])
            .await;
            let state = state(api);
            state.db.delete_entry(1).unwrap();

            let saved = save_entry_inner(
                &state,
                1,
                explicit_completion.then(|| "COMPLETED".into()),
                Some(12),
                None,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                ("COMPLETED", 12, 6)
            );
            requests.recv().await.unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":6})
            );

            save_entry_inner(&state, 1, None, Some(12), None, None)
                .await
                .unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"progress":12})
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().repeat, 6);
        }
    }

    #[tokio::test]
    async fn a_final_episode_edit_can_create_a_completed_entry() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{"mediaListEntry":null}}})),
            (200, json!({"data":{"SaveMediaListEntry":remote()}})),
        ])
        .await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();

        let saved = save_entry_inner(&state, 1, None, Some(12), None, None)
            .await
            .unwrap();

        assert_eq!((saved.status.as_str(), saved.progress), ("COMPLETED", 12));
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"mediaId":1,"status":"COMPLETED","progress":12})
        );
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn progress_below_the_latest_total_does_not_complete() {
        let mut reply = remote();
        reply["status"] = json!("CURRENT");
        let (api, mut requests) =
            anilist::mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":reply}}))]).await;
        let state = state(api);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "CURRENT".into();
        entry.progress = 11;
        state.db.upsert_entry(&entry).unwrap();
        let mut media = state.db.get_media(1).unwrap().unwrap();
        media.episodes = Some(13);
        state.db.upsert_media(&media).unwrap();

        let saved = save_entry_inner(&state, 1, None, Some(12), None, None)
            .await
            .unwrap();

        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":12})
        );
        assert_eq!((saved.status.as_str(), saved.progress), ("CURRENT", 12));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "CURRENT");
    }

    #[tokio::test]
    async fn final_episode_edits_coalesce_without_counting_a_rewatch_twice() {
        let mut before = remote();
        before["status"] = json!("REPEATING");
        before["progress"] = json!(10);
        let mut after = remote();
        after["repeat"] = json!(3);
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
            (200, json!({"data":{"SaveMediaListEntry":after}})),
        ])
        .await;
        let state = state(api);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "REPEATING".into();
        entry.progress = 10;
        state.db.upsert_entry(&entry).unwrap();

        save_entry_inner(&state, 1, None, Some(11), None, None)
            .await
            .unwrap();
        save_entry_inner(&state, 1, None, Some(12), None, None)
            .await
            .unwrap();
        save_entry_inner(&state, 1, None, Some(12), None, None)
            .await
            .unwrap();
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending[0].patch.status.as_deref(), Some("COMPLETED"));
        assert_eq!(pending[0].patch.progress, Some(12));
        assert_eq!(pending[0].patch.repeat, Some(3));
        assert_eq!(pending[0].base.as_ref().unwrap().progress, 10);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());

        crate::sync_queue::flush_unlocked(&state).await.unwrap();

        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"status":"COMPLETED","progress":12,"repeat":3})
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().repeat, 3);
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
            "playback",
        ] {
            let (api, mut requests) = anilist::mock_api(vec![
                (429, json!({})),
                (503, json!({"errors":[{"message":"Unavailable"}]})),
            ])
            .await;
            let state = Arc::new(state(api));
            if operation == "playback" {
                let mut entry = state.db.get_entry(1).unwrap().unwrap();
                entry.status = "CURRENT".into();
                entry.progress = 11;
                state.db.upsert_entry(&entry).unwrap();
            }
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
                    "playback" => watcher_set_progress(&worker_state, 1, 12, Some("test-session"))
                        .await
                        .map(|_| ()),
                    _ => save_entry_inner(&worker_state, 1, None, None, Some(80.0), None)
                        .await
                        .map(|_| ()),
                }
            });
            let first = requests.recv().await.unwrap();
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
            let retry = tokio::time::timeout(Duration::from_secs(3), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retry, first, "retry changed the {operation} request");
            let result = worker.await.unwrap();
            if matches!(operation, "save" | "playback") {
                result.unwrap();
            } else {
                assert!(result.unwrap_err().contains("Unavailable"), "{operation}");
            }
            assert!(
                requests.try_recv().is_err(),
                "extra request for {operation}"
            );
            let pending = state.db.pending(7).unwrap();
            if matches!(operation, "queue" | "save" | "playback") {
                assert_eq!(pending.len(), 1, "{operation}");
                assert!(!pending[0].conflict);
                assert!(pending[0].error.as_deref().unwrap().contains("Unavailable"));
                let base = pending[0].base.as_ref().unwrap();
                assert_eq!(base.progress, if operation == "playback" { 11 } else { 12 });
                if operation == "playback" {
                    assert!(!first["query"].as_str().unwrap().contains("mutation"));
                    assert_eq!(pending[0].patch.progress, Some(12));
                    assert_eq!(pending[0].patch.status.as_deref(), Some("COMPLETED"));
                } else {
                    assert_eq!(pending[0].patch.score, Some(80.0));
                }
            } else {
                assert!(pending.is_empty(), "{operation}");
            }
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
            "Media":{"mediaListEntry":{"id":22,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},
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
                json!({"data":{"SaveMediaListEntry":{"notes":"New note","startedAt":null,"completedAt":null,"customLists":{}}}}),
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
