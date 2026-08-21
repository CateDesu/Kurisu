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
const ALLOWED_REDIRECT_URIS: &[&str] = &[
    "http://127.0.0.1:39417/",
    "http://localhost:39417/",
];

pub struct AppState {
    pub anilist: Mutex<AniList>,
    pub db: Db,
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

/// Playback tracking config. Stored in the settings table, surfaced in Settings.
/// Mode defaults to `off` since tracking edits the live AniList list. `auto_ask`
/// is independent of mode. When on, the watcher jumps to Currently Watching and
/// asks to update after a few seconds of playback. This is the main detection
/// surface.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackingConfig {
    pub mode: String,            // "off" | "prompt" | "auto"
    pub prompt_seconds: u64,     // prompt mode: seconds of playback before asking
    pub auto_percent: u64,       // auto mode: watched percent that triggers a +1
    pub auto_ask: bool,          // jump to Currently Watching and ask after a delay
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self { mode: "off".into(), prompt_seconds: 120, auto_percent: 80, auto_ask: true }
    }
}

const TRACKING_MODE_KEY: &str = "tracking_mode";
const TRACKING_PROMPT_KEY: &str = "tracking_prompt_seconds";
const TRACKING_AUTO_KEY: &str = "tracking_auto_percent";
const TRACKING_AUTO_ASK_KEY: &str = "tracking_auto_ask";

impl TrackingConfig {
    pub fn load(db: &Db) -> Self {
        // Batch read so a concurrent set_settings can't interleave a new mode
        // with an old threshold.
        let kv = db.get_settings_batch(&[
            TRACKING_MODE_KEY,
            TRACKING_PROMPT_KEY,
            TRACKING_AUTO_KEY,
            TRACKING_AUTO_ASK_KEY,
        ]).unwrap_or_default();
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
        Self { mode, prompt_seconds, auto_percent, auto_ask }
    }

    pub fn save(&self, db: &Db) -> Result<(), String> {
        // One transaction so the playback tick never sees a half-saved config
        // like new mode plus old threshold.
        db.set_settings(&[
            (TRACKING_MODE_KEY, &self.mode),
            (TRACKING_PROMPT_KEY, &self.prompt_seconds.to_string()),
            (TRACKING_AUTO_KEY, &self.auto_percent.to_string()),
            (TRACKING_AUTO_ASK_KEY, if self.auto_ask { "1" } else { "0" }),
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
        auto_ask,
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
pub fn set_app_setting(key: String, value: String, state: State<'_, AppState>) -> Result<(), String> {
    check_app_setting_key(&key)?;
    state.db.set_setting(&key, &value).map_err(|e| e.to_string())
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
    // Persist before mutating in memory state so a failed DB write doesn't
    // leave us with a token that won't survive a restart.
    // One transaction. Written separately, a crash between the two left a token
    // with no username. sync_my_list and get_user_stats key off the username
    // and would fail while `is_logged_in` reports true.
    state
        .db
        .set_settings(&[(TOKEN_KEY, token.as_str()), (USERNAME_KEY, user.name.as_str())])
        .map_err(|e| e.to_string())?;
    state.anilist.lock().set_token(Some(token));
    *state.user.lock() = Some(user.clone());
    Ok(user)
}

/// Browser OAuth2 implicit flow. Starts the localhost callback server, opens the
/// AniList authorize page, waits up to 5 min for the redirect. The callback
/// yields the access token directly, no client_secret, no code exchange.
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
    // VACUUM can fail on full disk or busy db. Fall back to plain delete and
    // report the failure. Don't fake a clean logout.
    if state.db.scrub_setting(TOKEN_KEY).is_err() {
        let _ = state.db.set_setting(TOKEN_KEY, "");
        state.db.delete_setting(TOKEN_KEY).map_err(|e| e.to_string())?;
    }
    // Drop the previous account's cached list and identity. A different account
    // signing in next must not see these rows, and the stepper, edit modal or
    // auto tracker must not push writes keyed on them to the new account. The
    // recognizer cache is rebuilt from the emptied list.
    state.db.clear_entries().map_err(|e| e.to_string())?;
    state.db.delete_setting(USERNAME_KEY).map_err(|e| e.to_string())?;
    state.refresh_matchers();
    Ok(())
}

#[tauri::command]
pub async fn current_user(state: State<'_, AppState>) -> Result<Option<User>, String> {
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Ok(None);
    }
    // Cached user wins. A name or avatar change on AniList only shows after
    // logout and login, no re-fetch per call. Fine for a single-user app.
    // Checked after the token so a stale cache can't outlive the login.
    if let Some(u) = state.user.lock().clone() {
        return Ok(Some(u));
    }
    let u = match al.viewer().await {
        Ok(u) => {
            // A logout that raced the await has already cleared the shared
            // client. The clone still holds the old token, so re-read the
            // shared client before writing anything back. Caching the user
            // or the username now would resurrect the signed out account.
            if !state.anilist.lock().has_token() {
                return Ok(None);
            }
            // Refresh the stored username on every successful viewer() call. It
            // was only written at login, so renaming the AniList account left a
            // stale name behind and list queries keyed off it failed with
            // "User not found" until the user logged out and back in.
            if !u.name.is_empty()
                && state.db.get_setting(USERNAME_KEY).ok().flatten().as_deref() != Some(u.name.as_str())
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
            let name = state
                .db
                .get_setting(USERNAME_KEY)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| e.to_string())?;
            // Not cached. A later call after the network returns should retry
            // viewer() and upgrade to the real profile with avatar and id,
            // instead of serving the placeholder for the rest of the session.
            return Ok(Some(User { name, ..Default::default() }));
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
    for m in &media {
        let _ = state.db.upsert_media(m);
    }
    Ok(media)
}

/// One anime season for the `/seasons` browser. Results cached like search hits.
#[tauri::command]
pub async fn get_season(
    season: String,
    year: i64,
    page: i64,
    state: State<'_, AppState>,
) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
    let media = al.season(&season, year, page).await.map_err(|e| e.to_string())?;
    for m in &media {
        let _ = state.db.upsert_media(m);
    }
    Ok(media)
}

/// Community recommendations for a title. Shows in the edit modal's "also like" strip.
#[tauri::command]
pub async fn get_recommendations(media_id: i64, state: State<'_, AppState>) -> Result<Vec<Media>, String> {
    let al = state.anilist.lock().clone();
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
    let al = state.anilist.lock().clone();
    let v = al.media_by_id(id).await.map_err(|e| e.to_string())?;
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
            let _ = state.db.upsert_media(&media);
            for r in &relations {
                let _ = state.db.upsert_media(&r.media);
            }
            Ok(MediaDetail { media, relations, characters, staff })
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
    let items = al.airing_schedule(start, end).await.map_err(|e| e.to_string())?;
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
    // pre-save values over the fresh local write when the upserts land.
    let _write = state.entry_lock.lock().await;
    // user_list only returns Ok after a complete chunk walk when hasNextChunk
    // is false. So `entries` is the whole remote list, never a partial page.
    let entries = al.user_list(&user_name).await.map_err(|e| e.to_string())?;
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    let mut failed = 0usize;
    for e in &entries {
        seen.insert(e.media_id);
        let mut ok = true;
        if let Some(m) = &e.media {
            ok = state.db.upsert_media(m).is_ok() && ok;
        }
        ok = state.db.upsert_entry(e).is_ok() && ok;
        if !ok {
            failed += 1;
        }
    }
    if failed > 0 {
        return Err(format!(
            "sync incomplete: {failed} of {} entries could not be cached; remote deletions were not reconciled",
            entries.len()
        ));
    }
    // Reconcile. Rows the remote no longer has were deleted elsewhere or belong
    // to a previously signed-in account. Dropping them keeps the recognizer and
    // watcher from resurrecting entries the user removed. Only safe because
    // every fetched row persisted above.
    state.db.delete_entries_not_in(&seen).map_err(|e| e.to_string())?;
    state.refresh_matchers();
    state.db.entries_with_media().map_err(|e| e.to_string())
}

/// Offline/local view of the cached list. No network.
#[tauri::command]
pub fn local_entries(state: State<'_, AppState>) -> Result<Vec<ListEntry>, String> {
    state.db.entries_with_media().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_entry(media_id: i64, state: State<'_, AppState>) -> Result<Option<ListEntry>, String> {
    state.db.get_entry(media_id).map_err(|e| e.to_string())
}

/// Add or update an entry. Pushes to AniList and mirrors the returned entry to
/// the local cache. Progress is clamped into range. Marking COMPLETED with a
/// known episode count fills progress to that count.
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

/// Shared push-to-AniList and mirror-to-cache used by `update_entry`. Takes
/// `&AppState` so it works outside the command layer. Serialized under
/// `entry_lock` against the read-modify-write paths below.
pub async fn save_entry_inner(
    state: &AppState,
    media_id: i64,
    status: String,
    progress: i64,
    score: Option<f64>,
    repeat: i64,
) -> Result<ListEntry, String> {
    let _write = state.entry_lock.lock().await;
    let st = parse_status(&status)?;
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let total = media.as_ref().and_then(|m| m.episodes);
    // Clamp into range. The command accepts any i64 from any caller.
    let mut progress = progress.max(0);
    if let Some(t) = total {
        progress = progress.min(t);
    }
    // Marking COMPLETED fills progress to the episode count when known. This
    // mirrors the auto-complete rule where progress reaching the finale flips
    // status, so a quick-add "Completed" never lands as "finished, 0 of 24".
    let filled_to_total = st == ListStatus::Completed && total.is_some();
    if let Some(t) = total.filter(|_| st == ListStatus::Completed) {
        progress = progress.max(t);
    }
    let al = state.anilist.lock().clone();
    // Adding from a cold or partial cache. The show may already be on AniList
    // from anilist.co, another device, or another account's rows. Pushing
    // progress, score or repeat then would reset the real entry. So check
    // remotely and if it's there, send only the status plus the completion-fill
    // when it fired. Everything else keeps its remote values and comes back in
    // the response, which gets cached.
    if state.db.get_entry(media_id).map_err(|e| e.to_string())?.is_none()
        && al
            .entry_by_media_id(media_id)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
    {
        return save_entry_unlocked(
            state,
            media_id,
            Some(status),
            filled_to_total.then_some(progress),
            None,
            None,
        )
        .await;
    }
    save_entry_unlocked(state, media_id, Some(status), Some(progress), score, Some(repeat)).await
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
    let saved = al
        .save_entry(media_id, st, progress, score, repeat)
        .await
        .map_err(|e| e.to_string())?;
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
    state.refresh_matchers();
    Ok(entry)
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
    let mut progress = cur.progress + 1;
    let media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    // Unknown episode total. Still cap at a sane ceiling so the +1 button can't
    // push unbounded bogus progress to AniList.
    let total = media.as_ref().and_then(|m| m.episodes).unwrap_or(9999);
    if progress > total {
        progress = total;
    }
    // Advancing a PLANNING entry starts it. Moves to CURRENT.
    let status = if matches!(cur.status.as_str(), "" | "PLANNING") {
        ListStatus::Current.as_str().to_string()
    } else {
        cur.status.clone()
    };
    // Auto-complete at the last episode. Finishing while REPEATING also bumps
    // the rewatch count per AniList's convention, instead of silently losing it.
    let (final_status, repeat) = if at_last_episode(media.as_ref().and_then(|m| m.episodes), progress) {
        let repeat = if status == ListStatus::Repeating.as_str() { cur.repeat + 1 } else { cur.repeat };
        (ListStatus::Completed.as_str().to_string(), repeat)
    } else {
        (status, cur.repeat)
    };
    // Send only what actually changed. A plain +1 that republished the cached
    // status or repeat would clobber edits made outside Kurisu since the last
    // sync. Score never rides along on a progress write.
    save_entry_unlocked(
        state,
        media_id,
        (final_status != cur.status).then_some(final_status),
        Some(progress),
        None,
        (repeat != cur.repeat).then_some(repeat),
    )
    .await
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
        if state.db.get_entry(media_id).map_err(|e| e.to_string())?.is_none() {
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
pub async fn watcher_set_progress(state: &AppState, media_id: i64, episode: i64) -> Result<Option<ListEntry>, String> {
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
    // a COMPLETED one moves it to CURRENT.
    let (status, repeat) = if overshot {
        (prev_status, prev_repeat)
    } else if at_end && prev_status == "REPEATING" {
        (ListStatus::Completed.as_str(), prev_repeat + 1)
    } else if at_end {
        (ListStatus::Completed.as_str(), prev_repeat)
    } else if prev_status.is_empty() || (progress > 0 && matches!(prev_status, "COMPLETED" | "PLANNING")) {
        (ListStatus::Current.as_str(), prev_repeat)
    } else {
        (prev_status, prev_repeat)
    };
    // An unknown status still resolves to CURRENT since a brand-new entry
    // needs a status on AniList. Anything else that didn't transition stays
    // unsent.
    let status = if status.is_empty() { ListStatus::Current.as_str() } else { status };
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
            al.delete_entry(id).await.map_err(|e| e.to_string())?;
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
pub fn remove_library_folder(path: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
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
    if state.db.get_entry(media_id).map_err(|e| e.to_string())?.is_none() {
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
        .map(|f| FeedFailure { url: f.url, error: f.error })
        .collect();
    let raw = fetched.items;
    let _ = state.db.prune_rss_seen(60);
    let seen = state.db.rss_seen_set().map_err(|e| e.to_string())?;
    let matchers = state.matchers.lock().clone();
    let mut items: Vec<TorrentItem> = raw
        .into_iter()
        .map(|r| {
            let matched = recognize::match_title(&matchers, &r.title, "");
            let episode = matched.and_then(|m| recognize::resolve_episode(m, &[r.title.as_str()]));
            let (progress, total) = match matched {
                Some(m) => (
                    state.db.get_entry(m.media_id).ok().flatten().map(|e| e.progress),
                    state.db.get_media(m.media_id).ok().flatten().and_then(|md| md.episodes),
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
    Ok(TorrentFetch { items, failures })
}

#[tauri::command]
pub fn mark_torrents_seen(guids: Vec<String>, state: State<'_, AppState>) -> Result<(), String> {
    state.db.mark_rss_seen(&guids).map_err(|e| e.to_string())
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
    al.user_statistics(&user_name).await.map_err(|e| e.to_string())
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
            .ok_or_else(|| "no SHA-256 checksum available for this release; refusing to install unverified".to_string())?;
        crate::updater::download(&url, &tmp).await?;

        // Verify against the published `.sha256` sidecar and FAIL CLOSED. An
        // unverifiable download is refused, never installed. The digest is
        // taken from an OPEN handle, and that same handle is what gets
        // installed or executed below. Another process can't swap the file
        // between verify and use, no TOCTOU. Hashing a 150 MB file is blocking
        // I/O, so off the async runtime.
        let verify = async {
            let tmp2 = tmp.clone();
            match tokio::task::spawn_blocking(move || crate::updater::verify_and_open(&tmp2, &sidecar)).await {
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
            // loader reads exactly the bytes we hashed.
            std::process::Command::new(&tmp)
                .spawn()
                .map_err(|e| format!("could not launch the installer: {e}"))?;
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
            let result = tokio::task::spawn_blocking(move || crate::updater::apply_linux_update(&mut verified))
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
            db: Db::open(std::path::Path::new(":memory:")).expect("in-memory db"),
            user: Mutex::new(None),
            entry_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
        }
    }

    fn seed(state: &AppState, episodes: Option<i64>, status: &str, progress: i64, repeat: i64) {
        state
            .db
            .upsert_media(&Media { id: 1, episodes, ..Default::default() })
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
        assert_eq!((w.status.as_str(), w.progress, w.repeat), ("COMPLETED", 12, 3));
        assert!(w.send_status && w.send_repeat);
    }

    /// C1. A plain CURRENT entry completing at the last episode keeps repeat=0.
    #[test]
    fn finishing_first_watch_keeps_repeat() {
        let state = test_state();
        seed(&state, Some(12), "CURRENT", 11, 0);
        let w = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!((w.status.as_str(), w.progress, w.repeat), ("COMPLETED", 12, 0));
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
}
