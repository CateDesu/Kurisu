//! Tauri commands. The only surface the frontend can call. Thin wrappers over
//! the AniList client and DB cache. State sits behind non-poisoning parking_lot
//! Mutexes. Async commands clone the AniList client out of the lock before
//! awaiting so the futures stay Send, which Tauri requires.

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

/// The registered AniList client id, a public OAuth identifier. Override in
/// Settings if you register a different client.
const DEFAULT_CLIENT_ID: &str = "45266";
/// The redirect URI the callback server answers on. MUST byte-match the URI
/// registered with AniList. The callback server always binds 127.0.0.1:39417
/// and reads the token from the query of any path.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:39417/";
/// The only redirect URIs the callback server can answer. It binds
/// 127.0.0.1:39417 and nothing else. Anything else would deliver the token, in
/// the URL fragment, to a page we don't serve.
const ALLOWED_REDIRECT_URIS: &[&str] = &["http://127.0.0.1:39417/", "http://localhost:39417/"];

pub struct AppState {
    pub anilist: Mutex<AniList>,
    /// Arc so bulk and scrub work can run on blocking threads without
    /// borrowing the managed state.
    pub db: std::sync::Arc<Db>,
    pub user: Mutex<Option<User>>,
    /// Locks entry read-modify-write so user clicks and the auto tracker
    /// don't clobber each other. Async because we hold it across the AniList call.
    pub entry_lock: tokio::sync::Mutex<()>,
    /// Recognizer matchers cached so the playback watcher and library scanner
    /// don't rebuild them every few seconds. Rebuilt on every local list mutation
    /// via `refresh_matchers`.
    pub matchers: Mutex<Arc<Vec<recognize::Matcher>>>,
}

impl AppState {
    /// Rebuild matchers after a local list mutation.
    pub fn refresh_matchers(&self) {
        *self.matchers.lock() = Arc::new(recognize::build_matchers(&self.db));
    }
}

/// The app handle, captured at setup so the write paths can emit
/// `kurisu://auth-expired` when a push reveals a dead session mid use. The
/// frontend flips every page to the login card instead of failing each
/// action with a raw 401 until restart.
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

/// What a failed AniList session says to the user. Returned alongside the
/// session clear so every write path reports it the same way.
const SESSION_EXPIRED: &str = "Your AniList session expired or was revoked. Please sign in again.";

/// Clear an auth rejected session. Callers must already hold `entry_lock`, or
/// take it around the call, so a list write in flight cannot land rows after
/// the clear. Mirrors logout minus the VACUUM scrub: the token is dead, plain
/// deletes are enough. The cached list rows go too, a different account
/// signing in next must not see or push writes through them.
fn clear_rejected_session(state: &AppState) {
    state.anilist.lock().set_token(None);
    *state.user.lock() = None;
    if let Err(e) = state.db.delete_setting(TOKEN_KEY) {
        log::warn!("failed to drop rejected token from db: {e}");
    }
    if let Err(e) = state.db.delete_setting(USERNAME_KEY) {
        log::warn!("failed to drop rejected username from db: {e}");
    }
    if let Err(e) = state.db.clear_entries() {
        log::warn!("failed to clear the cached list of the rejected session: {e}");
    }
    state.refresh_matchers();
    emit_auth_expired();
}

/// Map a failed AniList write for the user. A rejected token clears the
/// session and says so, everything else passes the raw error through.
/// Callers must hold `entry_lock`, which every write path already does.
fn write_err(state: &AppState, e: &anyhow::Error) -> String {
    if anilist::is_auth_rejection(e) {
        clear_rejected_session(state);
        SESSION_EXPIRED.to_string()
    } else {
        e.to_string()
    }
}

/// Playback tracking config. Stored in the settings table, surfaced in Settings.
/// Mode defaults to `off` since tracking edits the live AniList list. `auto_ask`
/// is independent of mode. When on, the watcher jumps to Currently Watching and
/// asks to update after a few seconds of playback. This is the main detection
/// surface. `discord_enabled` is independent of mode too. It only announces the
/// detected episode as Discord Rich Presence and never touches the list.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackingConfig {
    pub mode: String,        // "off" | "prompt" | "auto"
    pub prompt_seconds: u64, // prompt mode: seconds of playback before asking
    pub auto_percent: u64,   // auto mode: watched percent that triggers a +1
    pub auto_ask: bool,      // jump to Currently Watching and ask after a delay
    /// Optional mpv --input-ipc-server socket path. Empty tries the well
    /// known defaults. Bare mpv never registers with the OS media session
    /// so the watcher reads the socket directly.
    pub mpv_ipc_socket: String,
    /// Show the detected episode as Discord Rich Presence.
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
        // Batch read so a concurrent set_settings can't interleave a new mode
        // with an old threshold.
        let kv = match db.get_settings_batch(&[
            TRACKING_MODE_KEY,
            TRACKING_PROMPT_KEY,
            TRACKING_AUTO_KEY,
            TRACKING_AUTO_ASK_KEY,
            TRACKING_MPV_SOCKET_KEY,
            TRACKING_DISCORD_KEY,
        ]) {
            Ok(kv) => kv,
            // Surface the failure instead of silently degrading to defaults.
            // A transient read error used to look exactly like unset keys,
            // which quietly turned tracking off until the next good read.
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
        // auto_ask defaults ON. Only "0" turns it off, so existing installs
        // pick it up without a migration.
        let auto_ask = kv
            .get(TRACKING_AUTO_ASK_KEY)
            .map(|s| s != "0")
            .unwrap_or(true);
        let mpv_ipc_socket = kv.get(TRACKING_MPV_SOCKET_KEY).cloned().unwrap_or_default();
        // Discord presence defaults ON like auto_ask. Only "0" turns it off,
        // so existing installs pick it up without a migration.
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
        // One transaction so the playback tick never sees a half-saved config
        // like new mode plus old threshold.
        db.set_settings(&[
            (TRACKING_MODE_KEY, &self.mode),
            (TRACKING_PROMPT_KEY, &self.prompt_seconds.to_string()),
            (TRACKING_AUTO_KEY, &self.auto_percent.to_string()),
            (TRACKING_AUTO_ASK_KEY, if self.auto_ask { "1" } else { "0" }),
            (TRACKING_MPV_SOCKET_KEY, self.mpv_ipc_socket.trim()),
            (TRACKING_DISCORD_KEY, if self.discord_enabled { "1" } else { "0" }),
        ])
        .map_err(|e| e.to_string())
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
    // The client id is a public OAuth identifier, digits only on AniList.
    // Anything else is a typo or an attempt to point the next sign in at a
    // different app's redirect URI. Trim, then require 1 to 16 ASCII digits
    // so a trailing newline can not quietly brick every later login.
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
    auto_ask: bool,
    mpv_ipc_socket: String,
    discord_enabled: bool,
    state: State<'_, AppState>,
) -> Result<TrackingConfig, String> {
    let normalized_mode = match mode.as_str() {
        "prompt" | "auto" => mode,
        _ => "off".to_string(),
    };
    // Trim and cap instead of rejecting. The field is optional, so
    // garbage just fails to resolve and the defaults take over.
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

/// Keys the generic accessors below may touch. Everything else has a dedicated
/// command with its own validation. Especially the AniList token, which must
/// not be one invoke away from any script in the webview. Add UI toggles here.
const APP_SETTING_KEYS: &[&str] = &["close_to_tray", "auto_update"];

fn check_app_setting_key(key: &str) -> Result<(), String> {
    if APP_SETTING_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(format!("not a UI setting key: {key}"))
    }
}

/// Generic key/value settings access for small UI toggles. Allowlisted to
/// `APP_SETTING_KEYS`.
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

/// Manual token entry, fallback when the browser flow can't be used. Verifies
/// the token via Viewer, then persists it.
#[tauri::command]
pub async fn login_with_token(token: String, state: State<'_, AppState>) -> Result<User, String> {
    // Verify on a clone first so the shared client keeps its token, possibly a
    // working one, until the new one proves valid. A rejected token must not
    // clobber it and log the user out.
    let mut probe = state.anilist.lock().clone();
    probe.set_token(Some(token.clone()));
    let user = probe.viewer().await.map_err(|e| e.to_string())?;
    // Serialize the session swap with any in-flight sync or save. Sync holds
    // this lock across its fetch, so the row clear below cannot be overtaken
    // by a snapshot the previous account's fetch is still writing.
    let _write = state.entry_lock.lock().await;
    // A different account signing in without a logout first. The cached rows
    // belong to the previous user, and the stepper, edit modal or auto
    // tracker could push writes keyed on them onto the new account. Same
    // invariant logout states explicitly. Rows without a stored username are
    // orphans, they go too.
    let prev_user = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?;
    let account_changed = prev_user
        .as_deref()
        .map(|u| u != user.name.as_str())
        .unwrap_or(true);
    // Persist before mutating in memory state so a failed DB write doesn't
    // leave us with a token that won't survive a restart.
    // One transaction. Written separately, a crash between the two left a token
    // with no username. sync_my_list and get_user_stats key off the username
    // and would fail while `is_logged_in` reports true.
    state
        .db
        .set_settings(&[
            (TOKEN_KEY, token.as_str()),
            (USERNAME_KEY, user.name.as_str()),
        ])
        .map_err(|e| e.to_string())?;
    state.anilist.lock().set_token(Some(token));
    *state.user.lock() = Some(user.clone());
    if account_changed {
        state.db.clear_entries().map_err(|e| e.to_string())?;
        state.refresh_matchers();
    }
    Ok(user)
}

/// Browser OAuth2 implicit flow. Starts the localhost callback server, opens the
/// AniList authorize page, waits up to 5 min for the redirect. The callback
/// yields the access token directly, no client_secret, no code exchange.
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
    // Implicit flow. The callback yields the access token itself, no exchange
    // step. The listener keeps running through AniList errors and only resolves
    // here on a verified token. So this wait ends on success, on the 5 minute
    // timeout, or when the listener task dies.
    let token = tokio::time::timeout(std::time::Duration::from_secs(300), rx)
        .await
        .map_err(|_| "Timed out waiting for AniList to redirect.".to_string())?
        .map_err(|_| "OAuth callback channel closed.".to_string())?;
    login_with_token(token, state).await
}

#[tauri::command]
pub async fn logout(state: State<'_, AppState>) -> Result<(), String> {
    // Serialize with any in-flight list write. If a save, increment or sync
    // holds the lock, logout waits for it to finish before clearing the table
    // so the write can't resurrect rows after clear_entries.
    let _write = state.entry_lock.lock().await;
    state.anilist.lock().set_token(None);
    *state.user.lock() = None;
    // Scrub, don't overwrite. An emptied row can survive on a freed SQLite page.
    // scrub_setting DELETEs the row, VACUUMs the freed page away, and truncates
    // the WAL so no copy of the token outlives the logout in the db files.
    // VACUUM of a large cache is slow blocking IO, so it runs off the async
    // worker instead of parking the runtime for the duration.
    let db = state.db.clone();
    let scrub = tokio::task::spawn_blocking(move || db.scrub_setting(TOKEN_KEY))
        .await
        .map_err(|e| e.to_string())?;
    let scrub_failure = scrub.err();
    if scrub_failure.is_some() {
        // VACUUM can fail on full disk or busy db. Fall back to plain delete
        // but do not fake a clean logout, the failure is reported below.
        let _ = state.db.set_setting(TOKEN_KEY, "");
        state
            .db
            .delete_setting(TOKEN_KEY)
            .map_err(|e| e.to_string())?;
    }
    // Drop the previous account's cached list and identity. A different account
    // signing in next must not see these rows, and the stepper, edit modal or
    // auto tracker must not push writes keyed on them to the new account. The
    // recognizer cache is rebuilt from the emptied list.
    state.db.clear_entries().map_err(|e| e.to_string())?;
    state
        .db
        .delete_setting(USERNAME_KEY)
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
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Ok(None);
    }
    // The token this query runs as. A logout or an account switch during the
    // await replaces the shared client's token, and caching the answer then
    // would resurrect the old identity over the new session.
    let token_used = al.token();
    // Cached user wins. A name or avatar change on AniList only shows after
    // logout and login, no re-fetch per call. Fine for a single-user app.
    // Checked after the token so a stale cache can't outlive the login.
    if let Some(u) = state.user.lock().clone() {
        return Ok(Some(u));
    }
    let u = match al.viewer().await {
        Ok(u) => {
            // A logout that raced the await has already cleared the shared
            // client, and a login as a DIFFERENT account has replaced it. The
            // clone still holds the old token, so compare identity, not just
            // presence, before writing anything back. Caching the user or the
            // username now would resurrect the signed out or switched away
            // account.
            if state.anilist.lock().token() != token_used {
                return Ok(None);
            }
            // Refresh the stored username on every successful viewer() call. It
            // was only written at login, so renaming the AniList account left a
            // stale name behind and list queries keyed off it failed with
            // "User not found" until the user logged out and back in.
            if !u.name.is_empty()
                && state.db.get_setting(USERNAME_KEY).ok().flatten().as_deref()
                    != Some(u.name.as_str())
            {
                let _ = state.db.set_setting(USERNAME_KEY, &u.name);
            }
            u
        }
        // A transport failure is not "logged out". Fall back to an offline
        // identity from the persisted username so offline-capable surfaces like
        // local list, library, cached detail pages stay reachable instead of
        // hiding behind the login card.
        Err(e) => {
            // A rejected token is a different thing entirely. Serving the
            // placeholder here kept a dead session alive forever: every
            // mutation failed while the app insisted the user was signed
            // in. Clear the session, rows included, so the login card comes
            // back and the next account never sees this one's list.
            if anilist::is_auth_rejection(&e) {
                let _write = state.entry_lock.lock().await;
                clear_rejected_session(state.inner());
                return Ok(None);
            }
            let name = state
                .db
                .get_setting(USERNAME_KEY)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| e.to_string())?;
            // Not cached. A later call after the network returns should retry
            // viewer() and upgrade to the real profile with avatar and id,
            // instead of serving the placeholder for the rest of the session.
            return Ok(Some(User {
                name,
                ..Default::default()
            }));
        }
    };
    *state.user.lock() = Some(u.clone());
    Ok(Some(u))
}

// ───────────────────────────── anime / list ─────────────────────────────

#[tauri::command]
pub async fn search_anime(query: String, state: State<'_, AppState>) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
    let media = al.search(&query, 25).await.map_err(|e| e.to_string())?;
    let _ = state.db.upsert_media_batch(&media);
    Ok(media)
}

/// One anime season for the `/seasons` browser. Walks every page so nothing
/// past the popular head is missing. Results cached like search hits.
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

/// Community recommendations for a title. Shows in the edit modal's "also like" strip.
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

/// The detail page. Fetch media, relations and credits fresh. Fall back to the
/// cached media with empty relations and credits when AniList is unreachable so
/// the page still renders offline.
#[tauri::command]
pub async fn get_media_detail(id: i64, state: State<'_, AppState>) -> Result<MediaDetail, String> {
    let al = state.anilist.lock().clone();
    match al.media_detail(id).await {
        Ok((media, relations, characters, staff)) => {
            // Detail upsert, so a studio or banner AniList no longer lists
            // actually clears instead of sticking in the cache forever.
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
        // A Not Found here is not an outage. AniList merged or deleted the
        // entry. The cached rows are dead and must not keep rendering a
        // normal looking page whose every add and save then fails with a raw
        // 404. The entry row goes too: it is keyed on the dead id, and if the
        // remote entry was moved to a merge target the next sync caches it
        // under the new id.
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

/// Everything airing in [start, end) for the calendar. Only media already on
/// the user's list is upserted into the cache. That refreshes airing info
/// without bloating the cache with hundreds of transient rows per week view.
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

/// Sync the remote list into the local cache, then return the local view.
/// Either the whole snapshot lands and the reconcile-delete runs, or the sync
/// reports a partial failure and nothing is deleted. Deleting on a half-written
/// snapshot would drop rows the remote still has, and the stale rows left
/// behind would be republished by later progress writes.
#[tauri::command]
pub async fn sync_my_list(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    let user_name = state
        .db
        .get_setting(USERNAME_KEY)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not logged in".to_string())?;
    let al = state.anilist.lock().clone();
    // The lock covers the fetch too, not just the upserts. A list snapshot
    // pulled before a concurrent save's push would resurrect that entry's
    // pre-save values over the fresh local write when the upserts land. It
    // also orders the sync against logins and the session clear, which take
    // the same lock.
    let _write = state.entry_lock.lock().await;
    // user_list only returns Ok after a complete chunk walk when hasNextChunk
    // is false. So `entries` is the whole remote list, never a partial page.
    let entries = match al.user_list(&user_name).await {
        Ok(v) => v,
        Err(e) => return Err(write_err(state.inner(), &e)),
    };
    // One transaction for the whole snapshot plus the reconcile delete, run
    // on a blocking thread. The per row path autocommitted two statements per
    // entry, so a 1300 entry sync was thousands of individually synced writes
    // while every progress write and auto track push waited on entry_lock,
    // and readers mid walk saw a torn half old half new list.
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || db.replace_list_snapshot(&entries))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| {
            format!(
                "sync incomplete: the remote list could not be cached, remote deletions were not reconciled: {e}"
            )
        })?;
    // Trim the media cache of rows nothing references anymore. List backed
    // rows are spared inside the prune. Best effort.
    let _ = state.db.prune_media_cache(30);
    state.refresh_matchers();
    state.db.entries_with_media().map_err(|e| e.to_string())
}

/// Offline/local view of the cached list. No network. The join is real work
/// on a 1300 row list, so it runs on a blocking thread instead of the
/// command's async worker.
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

/// AniList no longer resolves this media id. It was merged into another entry
/// or deleted upstream. Callers drop the dead cached rows and report this
/// instead of surfacing a raw 404 that can never be resolved from the app.
const MEDIA_GONE: &str =
    "AniList no longer has this anime. It was likely merged into another entry. Search for it to re-add the correct one.";

/// Add or update an entry. Pushes to AniList and mirrors the returned entry to
/// the local cache. Null fields are left untouched on AniList, so an edit only
/// sends what the user changed and can not republish stale cached values over
/// edits made elsewhere since the last sync. Progress is clamped into range.
/// Marking COMPLETED with a known episode count fills progress to that count.
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

/// Shared push-to-AniList and mirror-to-cache used by `update_entry`. Takes
/// `&AppState` so it works outside the command layer. Serialized under
/// `entry_lock` against the read-modify-write paths below.
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
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes);
    // Clamp into range. The command accepts any i64 from any caller.
    let mut progress = progress.map(|p| p.max(0));
    if let (Some(t), Some(p)) = (total, progress.as_mut()) {
        *p = (*p).min(t);
    }
    // Marking COMPLETED fills progress to the episode count when known. This
    // mirrors the auto-complete rule where progress reaching the finale flips
    // status, so a quick-add "Completed" never lands as "finished, 0 of 24".
    let mut filled_to_total = false;
    if st == Some(ListStatus::Completed) {
        if let Some(t) = total {
            progress = Some(progress.map_or(t, |p| p.max(t)));
            filled_to_total = true;
        }
    }
    let al = state.anilist.lock().clone();
    // Adding from a cold or partial cache. The show may already be on AniList
    // from anilist.co, another device, or another account's rows. Pushing
    // progress, score or repeat then would reset the real entry. So check
    // remotely and if it's there, send only the status plus the completion-fill
    // when it fired. Everything else keeps its remote values and comes back in
    // the response, which gets cached.
    if state
        .db
        .get_entry(media_id)
        .map_err(|e| e.to_string())?
        .is_none()
    {
        match al.entry_by_media_id(media_id).await {
            Ok(Some(_)) => {
                return save_entry_unlocked(
                    state,
                    media_id,
                    status,
                    filled_to_total.then_some(progress).flatten(),
                    None,
                    None,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) if anilist::media_not_found(&e) => {
                // The media id itself no longer resolves. Drop the dead cached
                // row so every surface refetches instead of serving an entry
                // whose every write fails forever.
                let _ = state.db.delete_media(media_id);
                state.refresh_matchers();
                return Err(MEDIA_GONE.to_string());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    save_entry_unlocked(state, media_id, status, progress, score, repeat).await
}

/// The body shared by every AniList entry write, for callers already holding
/// `entry_lock`. increment and set_progress computed their values from a read
/// taken under the same lock so no other writer can have invalidated them.
/// Sends only the `Some(..)` fields. AniList leaves omitted arguments
/// unchanged, so a progress write can't republish stale cached score or
/// repeat over edits made elsewhere. Then mirrors the returned values, which
/// are AniList's true post-write state, into the local cache.
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
        // The media id is dead upstream. Same recovery as the add path: drop
        // the cached rows so the failure is not permanent, and say what
        // happened instead of the raw status. The entry row is keyed on the
        // dead id, so keeping it would render a blank list card whose every
        // write fails this same way.
        Err(e) if anilist::media_not_found(&e) => {
            let _ = state.db.delete_entry(media_id);
            let _ = state.db.delete_media(media_id);
            state.refresh_matchers();
            return Err(MEDIA_GONE.to_string());
        }
        // A rejected token kills the session here too, not just in
        // current_user. Otherwise every save and auto track push fails with
        // a raw 401 until the app restarts.
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
    // The matcher set feeds on the entry's status rank and titles. A pure
    // progress, score or rewatch edit changes none of that, so the rebuild
    // is only worth it when the row is new or the status moved.
    if before.as_ref().map(|b| b.status.as_str()) != Some(entry.status.as_str()) {
        state.refresh_matchers();
    }
    Ok(entry)
}

/// What a +1 should do, lifted out of `increment_inner` so the awkward
/// corners are testable without a network. `Advance` carries the fields the
/// write sends. A +1 that cannot advance because the entry already sits at
/// the known total must not fire the finale rule: REPEATING would credit a
/// full rewatch from one click and the watcher would stay dead for the whole
/// run, since it skips episodes at or below progress. REPEATING wraps to
/// episode 1 and starts the rewatch for real, everything else is done.
#[derive(Debug)]
enum PlusOne {
    Advance {
        status: String,
        progress: i64,
        repeat: i64,
    },
    StartRewatch,
    NoOp,
}

fn plus_one_plan(
    cur_status: &str,
    cur_progress: i64,
    cur_repeat: i64,
    known_total: Option<i64>,
) -> PlusOne {
    // Unknown episode total. Still cap at a sane ceiling so the +1 button
    // can't push unbounded bogus progress to AniList.
    let total = known_total.unwrap_or(9999);
    let mut progress = cur_progress + 1;
    if progress > total {
        progress = total;
    }
    if progress == cur_progress {
        return match (cur_status, known_total) {
            ("REPEATING", Some(_)) => PlusOne::StartRewatch,
            _ => PlusOne::NoOp,
        };
    }
    // Advancing a PLANNING entry starts it. Moves to CURRENT. The pinned at
    // zero case is already handled above, so a stray +1 on an episodes 0
    // announced title cannot flip the plan to current either.
    let status = if matches!(cur_status, "" | "PLANNING") {
        ListStatus::Current.as_str().to_string()
    } else {
        cur_status.to_string()
    };
    // Auto-complete at the last episode. Finishing while REPEATING also bumps
    // the rewatch count per AniList's convention, instead of silently losing it.
    if at_last_episode(known_total, progress) {
        let repeat = if status == ListStatus::Repeating.as_str() {
            cur_repeat + 1
        } else {
            cur_repeat
        };
        return PlusOne::Advance {
            status: ListStatus::Completed.as_str().to_string(),
            progress,
            repeat,
        };
    }
    PlusOne::Advance {
        status,
        progress,
        repeat: cur_repeat,
    }
}

/// Increment progress by one, the "+1 episode" button. Clamps at the episode total when known.
#[tauri::command]
pub async fn increment_episode(
    media_id: i64,
    state: State<'_, AppState>,
) -> Result<ListEntry, String> {
    increment_inner(state.inner(), media_id).await
}

/// Shared +1 logic used by `increment_episode`. Clamps to the known episode
/// count, moves a PLANNING entry to CURRENT, and auto-completes at the last
/// episode, mirroring the stepper's `compute_set_progress`. The whole
/// read, compute and push runs under `entry_lock` so concurrent writes can't
/// be clobbered by values computed from a stale read.
pub async fn increment_inner(state: &AppState, media_id: i64) -> Result<ListEntry, String> {
    let _write = state.entry_lock.lock().await;
    // +1 must never CREATE an entry. A missing row means the user deleted it.
    // Writing now would resurrect it locally and on AniList. Same guard as the
    // watcher and the stepper's compare and swap.
    let Some(cur) = state.db.get_entry(media_id).map_err(|e| e.to_string())? else {
        return Err("not on your list".to_string());
    };
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let known_total = media.as_ref().and_then(|m| m.episodes);
    match plus_one_plan(&cur.status, cur.progress, cur.repeat, known_total) {
        PlusOne::StartRewatch => {
            save_entry_unlocked(state, media_id, None, Some(1), None, None).await
        }
        PlusOne::NoOp => {
            let mut done = cur;
            done.media = media;
            Ok(done)
        }
        PlusOne::Advance {
            status,
            progress,
            repeat,
        } => {
            // Send only what actually changed. A plain +1 that republished the
            // cached status or repeat would clobber edits made outside Kurisu
            // since the last sync. Score never rides along on a progress write.
            save_entry_unlocked(
                state,
                media_id,
                (status != cur.status).then_some(status),
                Some(progress),
                None,
                (repeat != cur.repeat).then_some(repeat),
            )
            .await
        }
    }
}

/// Set absolute episode progress, the list's minus/plus stepper. Clamps to the
/// known episode count, auto-completes at the last episode, starts a PLANNING
/// entry, and drops a Completed entry back to Current if you rewind past the
/// end. `expected` is the caller's compare and swap baseline. The stepper
/// buffers edits for 3s, so it passes the progress it sampled. If a concurrent
/// write from the auto tracker or another stepper moved progress since, the
/// write is skipped and the CURRENT entry is returned for the caller to adopt.
/// No rewind.
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
        // A missing row is NOT "progress 0". The entry was deleted after the
        // caller sampled its baseline. Treat it as a CAS failure instead of
        // resurrecting the entry on AniList. Same as the watcher's guard below.
        let Some(entry) = cur else {
            return Err("the entry is no longer on your list".to_string());
        };
        if entry.progress != exp {
            let mut entry = entry;
            entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
            return Ok(entry);
        }
    } else {
        // Without a CAS baseline we still must not CREATE an entry. A missing
        // row means the user deleted it, and writing now would resurrect it on
        // AniList. Same guard as the watcher and increment paths.
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

/// The auto tracker's variant of `set_progress_inner`. The watcher decided to
/// write from a seconds old sample. If the user rewound past the detected
/// episode in the meantime, writing now would resurrect stale progress. So
/// re-check under the write lock that the set still moves the entry forward.
/// Ok(None) means skipped, entry already at or past `episode`.
pub async fn watcher_set_progress(
    state: &AppState,
    media_id: i64,
    episode: i64,
) -> Result<Option<ListEntry>, String> {
    let _write = state.entry_lock.lock().await;
    // Auto-tracking must never CREATE an entry. A missing row means the user
    // deleted it, possibly seconds ago, winning the lock before us. Writing
    // now would resurrect it locally and on AniList.
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

/// What an absolute progress set writes, and which fields go on the wire.
/// `progress` is always sent. `status` and `repeat` ride along ONLY when the
/// write actually changes them. Auto-complete, rewind-reopen, PLANNING start,
/// rewatch bump. AniList leaves omitted arguments unchanged, so a plain plus
/// or minus can't republish stale cached status or repeat over edits the user
/// made elsewhere. Score is never part of a progress write.
struct ProgressWrite {
    status: String,
    progress: i64,
    repeat: i64,
    send_status: bool,
    send_repeat: bool,
}

/// True when progress has landed exactly on the finale. The progress > 0
/// half keeps a show cached with episodes 0, an announced title, from
/// completing on a bare +1. Shared by the +1 path and the stepper so the
/// two auto-complete rules can not drift apart.
fn at_last_episode(total: Option<i64>, progress: i64) -> bool {
    total == Some(progress) && progress > 0
}

/// Compute status, progress and repeat for an absolute progress set. Caller
/// must hold `entry_lock` so reads are consistent against other writers.
/// Clamps to the known episode count, auto-completes at the last episode, and
/// drops a Completed entry back to Current if you rewind past the end. A
/// request past the known total is parser garbage or stepper overshoot, not a
/// finale. The number is clamped but the status never flips on it. Episode
/// "265" of a 12 episode show must not mark it COMPLETED.
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
    // Auto-complete at the last episode. Bump the rewatch count when a
    // REPEATING entry finishes, per AniList's convention, instead of silently
    // dropping the rewatch. Starting progress on a PLANNING entry or rewinding
    // a COMPLETED one moves it to CURRENT. A completed entry reopens at any
    // rewind, including straight to zero. PLANNING only flips once progress
    // actually starts, so a stray zero write can't turn a plan into current.
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
    // An unknown status still resolves to CURRENT since a brand-new entry
    // needs a status on AniList. Anything else that didn't transition stays
    // unsent.
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
    // Serialized with entry writes. A delete racing a save must not end with
    // the save resurrecting the row afterwards.
    let _write = state.entry_lock.lock().await;
    if let Some(entry) = state.db.get_entry(media_id).map_err(|e| e.to_string())? {
        if let Some(id) = entry.id {
            let al = state.anilist.lock().clone();
            // Propagate a remote failure instead of deleting locally anyway.
            // A silent local-only delete would pop back to life on the next
            // sync. An "already gone remotely" answer is fine though, that IS
            // the desired end state, so the local row comes out below either
            // way.
            let deleted = al
                .delete_entry(id)
                .await
                .map_err(|e| write_err(state.inner(), &e))?;
            if !deleted {
                // "Already gone" under a stale entry id. The entry was
                // deleted and re-added on anilist.co or another device, so
                // the live copy answers under a NEW id while this one still
                // shows on the list. Find it by media id and remove it too,
                // or the next sync resurrects the row we are deleting.
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
pub fn remove_library_folder(
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<String>, String> {
    library::remove_folder(&state.db, &path).map_err(|e| e.to_string())
}

/// Scan the configured folders for video files and recognize them against the
/// cached list. Matchers come from the shared cache, rebuilt on every list
/// mutation. The filesystem walk itself runs on a blocking thread.
#[tauri::command]
pub async fn scan_library(state: State<'_, AppState>) -> Result<LibraryScan, String> {
    let folders = library::get_folders(&state.db);
    let bindings = library::get_bindings(&state.db);
    let matchers = state.matchers.lock().clone();
    tokio::task::spawn_blocking(move || library::scan_paths(&folders, &matchers, &bindings))
        .await
        .map_err(|e| e.to_string())
}

/// Manually link a file or folder to a show on the list, the Library's
/// "unmatched" fix-up. Only list members can be linked. The scan needs the
/// entry's titles and progress to do anything useful with the files.
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

/// Remove every manual link pointing at this show.
#[tauri::command]
pub fn unbind_library_media(media_id: i64, state: State<'_, AppState>) -> Result<(), String> {
    library::unbind_media(&state.db, media_id).map_err(|e| e.to_string())
}

// ───────────────────────────── torrents M6 ─────────────────────────────

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

/// Refresh the configured feeds and match every item against the list with the
/// shared recognizer. `is_new` is set when matched, episode is past the entry's
/// progress, and not marked seen. Items are newest first. Unmatched ones ride
/// along so the UI can report what the feed carried.
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
    // One read of the local list for every matched item, instead of two DB
    // round trips per row. Carries progress and the episode total, which is
    // all the is_new computation needs.
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
            // An episode past the entry's known total is another part of the
            // franchise, a new season or series matching an older completed
            // entry. Group it, but never flag it NEW.
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
    // Age prune, sparing the guids this very fetch carried. A blanket cutoff
    // resurrected dismissed items that a quiet feed keeps listing past the
    // window, and they came back flagged NEW.
    let carried: Vec<String> = items.iter().map(|i| i.guid.clone()).collect();
    let _ = state.db.prune_rss_seen_keeping(60, &carried);
    Ok(TorrentFetch { items, failures })
}

#[tauri::command]
pub fn mark_torrents_seen(guids: Vec<String>, state: State<'_, AppState>) -> Result<(), String> {
    state.db.mark_rss_seen(&guids).map_err(|e| e.to_string())
}

/// nyaa search for arbitrary torrents, list match or not. Results are not
/// run through the recognizer and carry no seen state. They exist to be
/// opened in the torrent client.
#[tauri::command]
pub async fn search_torrents(query: String) -> Result<Vec<TorrentItem>, String> {
    let query = query.trim().to_string();
    if query.is_empty() || query.chars().count() > 200 {
        return Err("empty or overlong search query".to_string());
    }
    let raw = rss::search(&query).await.map_err(|e| e.to_string())?;
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
            published: r.published,
            media_id: None,
            matched: None,
            episode: None,
            is_new: false,
            seen: false,
        })
        .collect())
}

// ───────────────────────────── stats M6 ─────────────────────────────

/// AniList's server-side profile statistics for the signed-in user.
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

// ───────────────────────────── notifications ─────────────────────────────

#[tauri::command]
pub async fn get_notifications(state: State<'_, AppState>) -> Result<Vec<Notification>, String> {
    let al = state.anilist.lock().clone();
    al.notifications().await.map_err(|e| e.to_string())
}

// ───────────────────────────── self-update ─────────────────────────────

/// Process wide serialization for `install_update`. See the command body.
static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Check GitHub for a newer release. Returns `{available, can_install, version,
/// tag, html_url, body, current}`. `available` means a newer release exists.
/// `can_install` means the release ships an asset this platform can install,
/// NSIS installer on Windows or bare binary on Linux. Other platforms get
/// `can_install: false` and update manually from the release page.
#[tauri::command]
pub async fn check_update() -> Result<serde_json::Value, String> {
    let rel = crate::updater::fetch_latest_release().await?;
    let available = crate::updater::is_newer(&rel.version, crate::updater::current_version());
    let can_install = crate::updater::platform_asset(&rel).is_some();
    Ok(serde_json::json!({
        "available": available,
        "can_install": can_install,
        // True once an update was applied this session. The UI should ask for
        // a restart rather than offering another install or a manual download.
        "restart_pending": crate::updater::update_applied(),
        "version": rel.version,
        "tag": rel.tag,
        "html_url": rel.html_url,
        "body": rel.body,
        "current": crate::updater::current_version(),
    }))
}

/// Download, verify, and install the latest release. On Windows the verified
/// NSIS installer is launched and the app quits as "restarting". On Linux the
/// running binary is swapped in place as "installed" and the UI prompts a
/// restart. Fails closed on a checksum problem.
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    // Serialized process wide. The scratch paths are per-process, so two
    // concurrent installs would truncate each other's files during download
    // or apply. The startup modal's backdrop can dismiss without cancelling,
    // then Settings offers Install again.
    let _install = INSTALL_LOCK
        .try_lock()
        .map_err(|_| "an update is already in progress".to_string())?;
    // A successful swap makes the running process older than the on-disk
    // binary. A second install in the same session would target the wrong path.
    if crate::updater::update_applied() {
        return Err("an update was already installed; restart Kurisu to finish".to_string());
    }
    let rel = crate::updater::fetch_latest_release().await?;
    // Re-check freshness here, not only in the check that opened the modal.
    // A re-published or reordered "latest" release must never downgrade us.
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
        // Download into the app-data dir, always user-writable, under a
        // pid-unique name. Swept on the next launch.
        let dir = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
        let dir2 = dir.clone();
        tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir2))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(".kurisu-update-{}-{asset}", std::process::id()));
        // Fetch the checksum sidecar before the 150 MB download. When no
        // checksum is available we refuse up front instead of after wasting
        // the transfer. The fetch itself is bounded to 15 s timeout and 4 KiB
        // cap.
        let sidecar = crate::updater::fetch_sidecar(&rel, &asset)
            .await
            .ok_or_else(|| {
                "no SHA-256 checksum available for this release; refusing to install unverified"
                    .to_string()
            })?;
        crate::updater::download(&url, &tmp).await?;

        // Verify against the published `.sha256` sidecar and FAIL CLOSED. An
        // unverifiable download is refused, never installed. The digest is
        // taken from an OPEN handle, and that same handle is what gets
        // installed or executed below. Another process can't swap the file
        // between verify and use, no TOCTOU. Hashing a 150 MB file is blocking
        // I/O, so off the async runtime.
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
            // Hand off. Launch the installer, then quit so it can overwrite us.
            // `verified` is held open with read-only sharing until AFTER the
            // spawn. The file can't be renamed or overwritten under us, so the
            // loader reads exactly the bytes we hashed. A failed launch also
            // drops the 150 MB download instead of leaving it for the next
            // launch's hour delayed sweep to collect.
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
            // The swap copies FROM the verified handle, not the path, and does
            // blocking renames off the async runtime.
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

    /// An AppState backed by an in-memory DB. No network needed since the
    /// progress compute helpers are pure local read-modify logic.
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

    /// C1. Finishing the last episode while REPEATING completes the entry AND
    /// bumps the rewatch count. Never silently drops the rewatch.
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

    /// C1. A plain CURRENT entry completing at the last episode keeps repeat=0.
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

    /// Rewinding a COMPLETED entry drops it back to CURRENT. Rewatch preserved
    /// for a REPEATING entry still mid run.
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

    /// Rewinding a completed entry to exactly zero still reopens it. The old
    /// progress > 0 guard left a finished show marked COMPLETED at 0 of 12.
    #[test]
    fn rewinding_completed_to_zero_reopens() {
        let state = test_state();
        seed(&state, Some(12), "COMPLETED", 12, 0);
        let w = compute_set_progress(&state, 1, 0).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("CURRENT", 0));
        assert!(w.send_status);
    }

    /// C7. Advancing progress on a PLANNING entry starts it, moves it to
    /// CURRENT. A PLANNING entry left at 0 stays PLANNING.
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

    /// T1. A request past the known total, parser garbage or stepper overshoot,
    /// is clamped but never flips the status. A misparse must not mark the show
    /// COMPLETED, and an already-COMPLETED entry stays COMPLETED.
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
        // The legitimate finale where request == total still completes, exactly as before.
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 11, 0);
        let w = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!((w.status.as_str(), w.progress), ("COMPLETED", 12));
    }

    /// C5. A plain minus or plus advance changes nothing but progress, so
    /// nothing but progress may go on the wire. The cached status and repeat
    /// stay unsent and can't clobber edits made outside Kurisu.
    #[test]
    fn a_plain_advance_sends_only_progress() {
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 5, 1);
        let w = compute_set_progress(&state, 1, 6).unwrap();
        assert_eq!((w.status.as_str(), w.progress, w.repeat), ("CURRENT", 6, 1));
        assert!(!w.send_status && !w.send_repeat);
    }

    /// A3. The +1 path shares the stepper's finale check. A show cached with
    /// episodes 0, an announced title, must not auto-complete when +1 clamps
    /// progress back to 0.
    #[test]
    fn zero_episode_show_does_not_auto_complete() {
        assert!(!at_last_episode(Some(0), 0));
        assert!(at_last_episode(Some(12), 12));
        assert!(!at_last_episode(Some(12), 11));
        assert!(!at_last_episode(None, 3));
    }

    /// A REPEATING entry sitting at the total, the state the edit modal
    /// leaves after moving a finished show to Rewatching. One +1 must not
    /// credit a full rewatch: it starts the rewatch at episode 1 instead.
    #[test]
    fn plus_one_at_the_total_starts_a_rewatch_not_finishes_it() {
        match plus_one_plan("REPEATING", 12, 2, Some(12)) {
            PlusOne::StartRewatch => {}
            other => panic!("expected a rewatch wrap, got {other:?}"),
        }
        // The honest finale still completes and bumps repeat.
        match plus_one_plan("REPEATING", 11, 2, Some(12)) {
            PlusOne::Advance {
                status,
                progress,
                repeat,
            } => {
                assert_eq!((status.as_str(), progress, repeat), ("COMPLETED", 12, 3));
            }
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    /// A COMPLETED entry at the total is done. The +1 is a no-op, it does
    /// not republish anything, and the status cannot flip on a pinned zero.
    #[test]
    fn plus_one_at_the_total_is_a_no_op_when_not_rewatching() {
        match plus_one_plan("COMPLETED", 12, 1, Some(12)) {
            PlusOne::NoOp => {}
            other => panic!("expected a no-op, got {other:?}"),
        }
        // Episodes 0 announced title, PLANNING at 0. The clamp pins progress
        // at 0, so the plan must not flip to CURRENT. Same rule the stepper
        // enforces in compute_set_progress.
        match plus_one_plan("PLANNING", 0, 0, Some(0)) {
            PlusOne::NoOp => {}
            other => panic!("expected a no-op, got {other:?}"),
        }
    }

    /// A plain advance moves PLANNING to CURRENT and CURRENT stays put.
    #[test]
    fn plus_one_advances_and_starts_planning() {
        match plus_one_plan("PLANNING", 0, 0, Some(12)) {
            PlusOne::Advance {
                status,
                progress,
                repeat,
            } => {
                assert_eq!((status.as_str(), progress, repeat), ("CURRENT", 1, 0));
            }
            other => panic!("expected an advance, got {other:?}"),
        }
        match plus_one_plan("CURRENT", 5, 1, Some(12)) {
            PlusOne::Advance {
                status,
                progress,
                repeat,
            } => {
                assert_eq!((status.as_str(), progress, repeat), ("CURRENT", 6, 1));
            }
            other => panic!("expected an advance, got {other:?}"),
        }
        // Unknown total keeps the sane ceiling instead of growing forever.
        match plus_one_plan("CURRENT", 9999, 0, None) {
            PlusOne::NoOp => {}
            other => panic!("expected the unknown total cap to hold, got {other:?}"),
        }
    }
}
