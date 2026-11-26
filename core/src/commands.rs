use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::OwnedMutexGuard;

use crate::anilist::{self, AniList};
use crate::db::{CatalogRead, CatalogUpdate, Db, ListPreferences, TOKEN_KEY, TOKEN_REJECTED_KEY};
use crate::library;
use crate::models::{
    AiringItem, EntryDetails, FuzzyDate, LibraryScan, ListEntry, ListStatus, Media, MediaDetail,
    NotificationPage, SearchPage, ShowTorrents, TorrentFetch, TorrentItem, User, UserStats,
};
use crate::recognize;
use crate::rss;

const CLIENT_ID_KEY: &str = "anilist_client_id";
const REDIRECT_URI_KEY: &str = "anilist_redirect_uri";
const USERNAME_KEY: &str = "anilist_username";
const USER_ID_KEY: &str = "anilist_user_id";
const PROFILE_KEY: &str = "anilist_profile";

const DEFAULT_CLIENT_ID: &str = "45266";
static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);
/// Must exactly match the redirect URI registered with AniList.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:39417/";

pub struct AppState {
    pub anilist: Mutex<AniList>,
    pub db: std::sync::Arc<Db>,
    pub user: Mutex<Option<User>>,
    pub auth_intent: tokio::sync::watch::Sender<u64>,
    /// Protect local entry and account transitions. Never hold during network I/O.
    pub entry_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serialize remote mutations, snapshots and account replacement.
    pub remote_lock: Arc<tokio::sync::Mutex<()>>,
    /// Refresh after list changes that affect recognition.
    pub(crate) matchers: Mutex<Arc<Vec<recognize::Matcher>>>,
    pub library_cache: Arc<Mutex<library::ScanCache>>,
    pub events: tokio::sync::broadcast::Sender<crate::Event>,
    pub last_auth_expired: Mutex<Option<crate::Event>>,
    pub last_now_playing: Mutex<Option<crate::Event>>,
    pub(crate) last_tracking_prompt: Mutex<Option<RetainedTrackingPrompt>>,
}

pub(crate) struct RetainedTrackingPrompt {
    event: crate::Event,
    entry_identity: Option<crate::EntryIdentity>,
    watched: Option<crate::playback::PromptWatch>,
}

fn same_tracking_prompt(a: &crate::Event, b: &crate::Event) -> bool {
    if a.generation != b.generation
        || a.account_id != b.account_id
        || a.prompt_id.is_none()
        || a.prompt_id != b.prompt_id
    {
        return false;
    }
    match (&a.kind, &b.kind) {
        (crate::EventKind::TrackingPrompt(a), crate::EventKind::TrackingPrompt(b))
        | (crate::EventKind::TrackingAsk(a), crate::EventKind::TrackingAsk(b)) => a == b,
        _ => false,
    }
}

impl AppState {
    pub(crate) fn cached_score_format(&self) -> Result<Option<String>, String> {
        Ok(self
            .db
            .cached_user()
            .map_err(|e| e.to_string())?
            .and_then(|user| user.score_format))
    }

    pub(crate) fn origin(&self) -> crate::EventOrigin {
        crate::EventOrigin {
            generation: *self.auth_intent.borrow(),
            account_id: crate::sync_queue::account_id(&self.db),
        }
    }

    pub(crate) fn origin_is_current(&self, origin: crate::EventOrigin) -> bool {
        self.origin() == origin
    }

    fn send_event(
        &self,
        origin: crate::EventOrigin,
        kind: crate::EventKind,
        watched: Option<crate::playback::PromptWatch>,
    ) {
        let prompt_id = matches!(
            &kind,
            crate::EventKind::TrackingPrompt(_) | crate::EventKind::TrackingAsk(_)
        )
        .then(|| NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed));
        let event = crate::Event {
            generation: origin.generation,
            account_id: if kind.is_global() {
                None
            } else {
                origin.account_id
            },
            prompt_id,
            kind,
        };
        match &event.kind {
            crate::EventKind::AuthExpired => {
                *self.last_auth_expired.lock() = Some(event.clone());
            }
            crate::EventKind::NowPlaying(now) => {
                if !now.active {
                    self.clear_tracking_prompt_from(origin);
                }
                *self.last_now_playing.lock() = Some(event.clone());
            }
            crate::EventKind::TrackingPrompt(prompt) | crate::EventKind::TrackingAsk(prompt) => {
                let Ok(entry) = self.db.get_entry(prompt.media_id) else {
                    return;
                };
                let mut retained = self.last_tracking_prompt.lock();
                if !self.origin_is_current(origin) {
                    return;
                }
                *retained = Some(RetainedTrackingPrompt {
                    event: event.clone(),
                    entry_identity: entry.as_ref().map(ListEntry::identity),
                    watched,
                });
            }
            _ => {}
        }
        let _ = self.events.send(event);
    }

    pub(crate) fn emit_from(&self, origin: crate::EventOrigin, kind: crate::EventKind) {
        if kind.is_global() || self.origin_is_current(origin) {
            self.send_event(origin, kind, None);
        }
    }

    pub(crate) fn emit_tracking_offer(
        &self,
        origin: crate::EventOrigin,
        kind: crate::EventKind,
        watched: crate::playback::PromptWatch,
    ) {
        if self.origin_is_current(origin) {
            self.send_event(origin, kind, Some(watched));
        }
    }

    pub(crate) fn event_is_current(&self, event: &crate::Event) -> bool {
        if event.kind.is_global() {
            return true;
        }
        if matches!(&event.kind, crate::EventKind::AuthExpired) {
            return self.origin().generation == event.generation
                && !self.anilist.lock().has_token()
                && self.last_auth_expired.lock().as_ref().is_some_and(|last| {
                    last.generation == event.generation && last.account_id == event.account_id
                });
        }
        if matches!(
            &event.kind,
            crate::EventKind::TrackingPrompt(_) | crate::EventKind::TrackingAsk(_)
        ) {
            return self
                .retained_tracking_prompt()
                .as_ref()
                .is_some_and(|retained| same_tracking_prompt(retained, event));
        }
        let current = self.origin();
        current.generation == event.generation && current.account_id == event.account_id
    }

    pub(crate) fn clear_tracking_prompt_from(&self, origin: crate::EventOrigin) {
        let mut retained = self.last_tracking_prompt.lock();
        if retained.as_ref().is_some_and(|retained| {
            let event = &retained.event;
            event.generation == origin.generation && event.account_id == origin.account_id
        }) {
            retained.take();
        }
    }

    fn clear_tracking_prompt_if_advanced(&self, origin: crate::EventOrigin, entry: &ListEntry) {
        let mut retained = self.last_tracking_prompt.lock();
        if retained.as_ref().is_some_and(|retained| {
            let event = &retained.event;
            match &event.kind {
                crate::EventKind::TrackingPrompt(prompt)
                | crate::EventKind::TrackingAsk(prompt) => {
                    event.generation == origin.generation
                        && event.account_id == origin.account_id
                        && prompt.media_id == entry.media_id
                        && entry.progress >= prompt.episode
                }
                _ => false,
            }
        }) {
            retained.take();
        }
    }

    pub(crate) fn clear_tracking_prompt_for_media(
        &self,
        origin: crate::EventOrigin,
        media_id: i64,
    ) {
        let mut retained = self.last_tracking_prompt.lock();
        if retained.as_ref().is_some_and(|retained| {
            let event = &retained.event;
            match &event.kind {
                crate::EventKind::TrackingPrompt(prompt)
                | crate::EventKind::TrackingAsk(prompt) => {
                    event.generation == origin.generation
                        && event.account_id == origin.account_id
                        && prompt.media_id == media_id
                }
                _ => false,
            }
        }) {
            retained.take();
        }
    }

    pub(crate) fn retained_tracking_prompt(&self) -> Option<crate::Event> {
        let retained = self.last_tracking_prompt.lock().as_ref()?.event.clone();
        let origin = self.origin();
        let prompt = match &retained.kind {
            crate::EventKind::TrackingPrompt(prompt) | crate::EventKind::TrackingAsk(prompt) => {
                prompt
            }
            _ => return None,
        };
        let obsolete = retained.generation != origin.generation
            || retained.account_id != origin.account_id
            || self
                .db
                .unqueued_progress(prompt.media_id, origin.account_id)
                .ok()
                .flatten()
                .is_some_and(|progress| progress >= prompt.episode);
        if obsolete {
            let mut current = self.last_tracking_prompt.lock();
            if current
                .as_ref()
                .is_some_and(|current| same_tracking_prompt(&current.event, &retained))
            {
                current.take();
            }
            None
        } else {
            Some(retained)
        }
    }

    pub(crate) async fn acknowledge_tracking_prompt(&self, event: crate::Event) -> bool {
        let _guard = self.entry_lock.lock().await;
        if !self
            .retained_tracking_prompt()
            .as_ref()
            .is_some_and(|retained| same_tracking_prompt(retained, &event))
        {
            return false;
        }
        self.take_tracking_prompt(&event)
    }

    fn take_tracking_prompt(&self, event: &crate::Event) -> bool {
        let mut retained = self.last_tracking_prompt.lock();
        if retained
            .as_ref()
            .is_some_and(|current| same_tracking_prompt(&current.event, event))
        {
            retained.take();
            true
        } else {
            false
        }
    }

    pub fn emit(&self, kind: crate::EventKind) {
        self.emit_from(self.origin(), kind);
    }

    /// Caller serializes list writes with entry_lock.
    pub fn refresh_matchers(&self) {
        if !self.anilist.lock().has_token() {
            return;
        }
        let previous = self.matchers.lock().clone();
        let matchers = recognize::refresh_matchers(&self.db, &previous, None);
        *self.matchers.lock() = matchers;
    }

    fn invalidate_matchers(&self) {
        let matchers = recognize::build_matchers(&self.db);
        *self.matchers.lock() = Arc::new(matchers);
    }

    fn clear_session(&self) {
        // Playback samples recognition before it takes the entry lock.
        *self.matchers.lock() = Arc::new(Vec::new());
        self.anilist.lock().set_token(None);
        *self.user.lock() = None;
    }

    fn restore_session(&self, token: Option<String>, user: Option<User>) {
        if let Some(token) = token {
            self.anilist.lock().set_token(Some(token));
            *self.user.lock() = user;
            self.refresh_matchers();
        }
    }

    fn cache_media_batch(&self, read: &CatalogRead, media: &mut Vec<Media>) {
        let removed = read.removed_media();
        media.retain(|item| !removed.contains(&item.id));
        read.preserve_newer(&self.db, media.iter_mut());
        if self.db.upsert_media_batch(media).is_ok() {
            read.record_write(
                &self.db,
                CatalogUpdate::Metadata,
                media.iter().map(|item| item.id),
            );
        }
        self.refresh_media_matchers(&media.iter().map(|item| item.id).collect::<Vec<_>>());
    }

    fn refresh_media_matchers(&self, ids: &[i64]) {
        if !self.anilist.lock().has_token() {
            return;
        }
        let previous = self.matchers.lock().clone();
        let matchers = recognize::refresh_matchers(&self.db, &previous, Some(ids));
        *self.matchers.lock() = matchers;
    }

    pub(crate) fn begin_auth_intent(&self) -> u64 {
        let mut intent = 0;
        self.auth_intent.send_modify(|current| {
            *current = current.wrapping_add(1);
            intent = *current;
        });
        self.last_tracking_prompt.lock().take();
        intent
    }

    pub(crate) fn cancel_login(&self) {
        self.begin_auth_intent();
    }

    fn check_auth_intent(&self, intent: u64) -> Result<(), String> {
        self.auth_intent_guard(intent).map(drop)
    }

    pub(crate) fn auth_intent_guard(
        &self,
        generation: u64,
    ) -> Result<tokio::sync::watch::Ref<'_, u64>, String> {
        let intent = self.auth_intent.borrow();
        if *intent != generation {
            return Err(AUTH_REPLACED.into());
        }
        Ok(intent)
    }

    async fn auth_intent_changed(&self, intent: u64) {
        let mut changes = self.auth_intent.subscribe();
        let _ = changes.wait_for(|current| *current != intent).await;
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

fn entry_account(state: &AppState, origin: crate::EventOrigin) -> Result<i64, String> {
    origin
        .account_id
        .or_else(|| {
            let verified = state.anilist.lock().verified_account()?;
            let owners = state
                .db
                .get_settings_batch(&[USER_ID_KEY, crate::db::LIST_OWNER_KEY])
                .ok()?;
            owners
                .values()
                .all(|owner| owner.parse::<i64>() == Ok(verified))
                .then_some(verified)
        })
        .filter(|id| anilist::valid_account_id(*id))
        .ok_or_else(|| "Reconnect to AniList to verify your account.".into())
}

fn require_write_session(state: &AppState, expected_token: &Option<String>) -> Result<(), String> {
    if expected_token.is_none() {
        return Err("not authenticated".into());
    }
    check_write_session(state, expected_token)
}

pub(crate) fn check_write_origin(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
) -> Result<(), String> {
    require_write_session(state, token)?;
    if !state.origin_is_current(origin) {
        return Err(AUTH_REPLACED.into());
    }
    Ok(())
}

pub(crate) fn write_origin_guard<'a>(
    state: &'a AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
) -> Result<tokio::sync::watch::Ref<'a, u64>, String> {
    let intent = state.auth_intent_guard(origin.generation)?;
    require_write_session(state, token)?;
    if crate::sync_queue::account_id(&state.db) != origin.account_id {
        return Err(AUTH_REPLACED.into());
    }
    Ok(intent)
}

pub(crate) fn write_session_account_matches(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
) -> bool {
    require_write_session(state, token).is_ok()
        && crate::sync_queue::account_id(&state.db) == origin.account_id
}

fn signed_out_origin_guard(
    state: &AppState,
    origin: crate::EventOrigin,
) -> Result<tokio::sync::watch::Ref<'_, u64>, String> {
    let intent = state.auth_intent_guard(origin.generation)?;
    check_write_session(state, &None)?;
    if crate::sync_queue::account_id(&state.db) != origin.account_id {
        return Err(AUTH_REPLACED.into());
    }
    Ok(intent)
}

#[cfg(test)]
fn clear_rejected_session(state: &AppState) {
    let token = state.anilist.lock().token();
    reject_write_session(state, state.origin(), &token);
}

fn clear_rejected_session_after<T>(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    settle: impl FnOnce() -> T,
) -> Option<(T, WriteAuthFailure)> {
    let intent = state.auth_intent.borrow();
    if *intent != origin.generation
        || crate::sync_queue::account_id(&state.db) != origin.account_id
        || require_write_session(state, token).is_err()
    {
        return None;
    }
    let failure = match remove_saved_token(&state.db) {
        Ok(warning) => {
            if let Some(warning) = warning {
                log::warn!("failed to scrub rejected token: {warning}");
            }
            WriteAuthFailure::Expired
        }
        Err(error) => {
            log::warn!("failed to remove rejected token: {error}");
            match state.db.durable_write(|tx| {
                let written = tx.execute(
                    "INSERT OR REPLACE INTO settings (key,value) VALUES (?,'1')",
                    [TOKEN_REJECTED_KEY],
                )?;
                anyhow::ensure!(written == 1, "token rejection marker was not written");
                Ok(())
            }) {
                Ok(()) => WriteAuthFailure::Expired,
                Err(error) => {
                    log::error!("failed to record rejected token: {error}");
                    WriteAuthFailure::PersistenceFailed
                }
            }
        }
    };
    state.clear_session();
    let settled = settle();
    if let Err(e) = state.db.preserve_list_owner() {
        log::warn!("failed to preserve list owner of the rejected session: {e}");
        for key in crate::db::SCOPED_LIST_PREFERENCE_KEYS {
            if let Err(del_err) = state.db.delete_setting(key) {
                log::warn!("failed to clear unowned list preference {key}: {del_err}");
            }
        }
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
    state.send_event(origin, crate::EventKind::AuthExpired, None);
    drop(intent);
    state.clear_tracking_prompt_from(origin);
    Some((settled, failure))
}

#[derive(Debug, Clone, Copy)]
enum WriteAuthFailure {
    Expired,
    PersistenceFailed,
    Replaced,
}

impl WriteAuthFailure {
    fn message(self) -> &'static str {
        match self {
            Self::Expired => SESSION_EXPIRED,
            Self::PersistenceFailed => concat!(
                "Your AniList session expired. Kurisu could not disable the saved token. ",
                "It may be loaded again after a restart. Please sign in again."
            ),
            Self::Replaced => AUTH_REPLACED,
        }
    }
}

/// Caller holds remote_lock and entry_lock when handling an auth rejection.
fn reject_write_session(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
) -> WriteAuthFailure {
    clear_rejected_session_after(state, origin, token, || {})
        .map_or(WriteAuthFailure::Replaced, |(_, failure)| failure)
}

pub(crate) fn write_err(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    e: &anyhow::Error,
) -> String {
    if anilist::is_auth_rejection(e) {
        reject_write_session(state, origin, token).message().into()
    } else {
        e.to_string()
    }
}

/// Caller holds remote_lock and entry_lock.
pub(crate) fn settle_write_error<T>(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    error: &anyhow::Error,
    settle: impl FnOnce(&str) -> Result<T, String>,
) -> Result<(T, String), String> {
    if anilist::is_auth_rejection(error) {
        let (settled, failure) =
            clear_rejected_session_after(state, origin, token, || settle(SESSION_EXPIRED))
                .ok_or(AUTH_REPLACED)?;
        return Ok((settled?, failure.message().into()));
    }
    let _intent = write_origin_guard(state, origin, token)?;
    let message = error.to_string();
    Ok((settle(&message)?, message))
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
    /// Compatibility fallback. Decision-making callers should use try_load.
    pub fn load(db: &Db) -> Self {
        Self::try_load(db).unwrap_or_else(|error| {
            log::warn!("tracking config read failed, using defaults: {error}");
            Self::default()
        })
    }

    pub fn try_load(db: &Db) -> Result<Self, String> {
        let kv = db
            .get_settings_batch(&[
                TRACKING_MODE_KEY,
                TRACKING_PROMPT_KEY,
                TRACKING_AUTO_KEY,
                TRACKING_AUTO_ASK_KEY,
                TRACKING_MPV_SOCKET_KEY,
                TRACKING_DISCORD_KEY,
            ])
            .map_err(|error| error.to_string())?;
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
        Ok(Self {
            mode,
            prompt_seconds,
            auto_percent,
            auto_ask,
            mpv_ipc_socket,
            discord_enabled,
        })
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

pub fn get_tracking_config(state: &AppState) -> TrackingConfig {
    TrackingConfig::load(&state.db)
}

pub fn try_get_tracking_config(state: &AppState) -> Result<TrackingConfig, String> {
    TrackingConfig::try_load(&state.db)
}

pub fn set_tracking_config(
    mode: String,
    prompt_seconds: u64,
    auto_percent: u64,
    auto_ask: bool,
    mpv_ipc_socket: String,
    discord_enabled: bool,
    state: &AppState,
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

pub fn is_logged_in(state: &AppState) -> bool {
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

pub fn get_app_setting(key: String, state: &AppState) -> Result<Option<String>, String> {
    read_app_setting(&state.db, &key)
}

fn read_app_setting(db: &Db, key: &str) -> Result<Option<String>, String> {
    check_app_setting_key(key)?;
    db.get_setting(key).map_err(|e| e.to_string())
}

pub fn set_app_setting(key: String, value: String, state: &AppState) -> Result<(), String> {
    check_app_setting_key(&key)?;
    state
        .db
        .set_setting(&key, &value)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub async fn login_with_token(
    token: String,
    state: &AppState,
    preferences: ListPreferences,
) -> Result<User, String> {
    let intent = state.begin_auth_intent();
    login_with_token_inner(token, state, intent, preferences).await
}

pub(crate) async fn login_with_token_inner(
    token: String,
    state: &AppState,
    intent: u64,
    preferences: ListPreferences,
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
    let session_changed = state.anilist.lock().token().as_ref() != Some(&token);
    if session_changed {
        *state.matchers.lock() = Arc::new(Vec::new());
    }
    let switching = saved_api_account_id(state) != Some(user.id);
    if switching && !session_changed {
        *state.matchers.lock() = Arc::new(Vec::new());
    }
    let account_changed =
        match state
            .db
            .replace_account_with_preferences(&token, &user, preferences)
        {
            Ok(changed) => changed,
            Err(e) => {
                if switching || session_changed {
                    state.refresh_matchers();
                }
                return Err(e.to_string());
            }
        };
    if account_changed || session_changed {
        state.invalidate_matchers();
    }
    *state.user.lock() = Some(user.clone());
    let mut al = state.anilist.lock();
    al.set_token(Some(token));
    al.record_viewer(user.id);
    Ok(user)
}

pub async fn login_oauth(
    platform: &dyn crate::Platform,
    state: &AppState,
    intent: u64,
    preferences: ListPreferences,
) -> Result<User, String> {
    state.check_auth_intent(intent)?;
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
    let (oauth_state, _, rx) = start_oauth_callback(state, intent, anilist::OAUTH_PORT).await?;
    let url = anilist::authorize_url(&client_id, &redirect_uri, &oauth_state);
    platform.open_url(&url)?;
    finish_oauth_login(state, intent, rx, preferences).await
}

async fn finish_oauth_login(
    state: &AppState,
    intent: u64,
    receiver: anilist::OAuthReceiver,
    preferences: ListPreferences,
) -> Result<User, String> {
    let code = wait_for_oauth_code(state, intent, receiver).await?;
    let api = state.anilist.lock().clone();
    let token = tokio::select! {
        result = api.exchange_code(&code) => result.map_err(|e| e.to_string())?,
        _ = state.auth_intent_changed(intent) => return Err(AUTH_REPLACED.into()),
    };
    login_with_token_inner(token, state, intent, preferences).await
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

// Admitted writes own their guards even if the caller stops awaiting the result.
async fn persist<T: Send + 'static>(
    entry: OwnedMutexGuard<()>,
    remote: Option<OwnedMutexGuard<()>>,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || {
        let _remote = remote;
        let _entry = entry;
        work()
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn logout(state: &Arc<AppState>) -> Result<(), String> {
    let intent = state.begin_auth_intent();
    logout_inner(state, intent).await
}

async fn logout_inner(state: &Arc<AppState>, intent: u64) -> Result<(), String> {
    let remote = state.remote_lock.clone().lock_owned().await;
    let entry = state.entry_lock.clone().lock_owned().await;
    state.check_auth_intent(intent)?;
    let previous_token = state.anilist.lock().token();
    let previous_user = state.user.lock().clone();
    state.clear_session();
    let state = state.clone();
    persist(entry, Some(remote), move || {
        finish_logout(&state, previous_token, previous_user)
    })
    .await
}

fn remove_saved_token(db: &Db) -> Result<Option<String>, String> {
    let Err(e) = db.scrub_setting(TOKEN_KEY) else {
        return Ok(None);
    };
    if db
        .get_setting(TOKEN_KEY)
        .is_ok_and(|saved| saved.is_none_or(|token| token.is_empty()))
    {
        return Ok(Some(format!(
            "signed out, but the token could not be scrubbed from the local database: {e}"
        )));
    }
    db.durable_write(|tx| {
        let written = tx.execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?, '')",
            [TOKEN_KEY],
        )?;
        anyhow::ensure!(written == 1, "the saved token was not blanked");
        Ok(())
    })
    .map_err(|_| e.to_string())?;
    Ok(Some(e.to_string()))
}

fn finish_logout(
    state: &AppState,
    previous_token: Option<String>,
    previous_user: Option<User>,
) -> Result<(), String> {
    let token_warning = match remove_saved_token(&state.db) {
        Ok(warning) => warning,
        Err(e) if previous_token.is_some() => {
            state.restore_session(previous_token, previous_user);
            return Err(e);
        }
        Err(e) => Some(e),
    };
    let mut cleanup_error = None;
    let mut record = |result: anyhow::Result<()>| {
        if let Err(e) = result {
            if cleanup_error.is_none() {
                cleanup_error = Some(e.to_string());
            }
        }
    };
    if let Err(e) = state.db.preserve_list_owner() {
        record(Err(e));
        for key in crate::db::SCOPED_LIST_PREFERENCE_KEYS {
            record(state.db.delete_setting(key));
        }
    }
    record(state.db.clear_entries());
    record(state.db.delete_setting(USERNAME_KEY));
    record(state.db.delete_setting(USER_ID_KEY));
    record(state.db.delete_setting(PROFILE_KEY));
    if let Some(error) = token_warning.or(cleanup_error) {
        return Err(error);
    }
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn refresh_current_user(state: &AppState) -> Result<Option<User>, String> {
    current_user_mode(state, true, ListPreferences::Preserve).await
}

#[cfg(test)]
async fn current_user_inner(state: &AppState) -> Result<Option<User>, String> {
    current_user_mode(state, false, ListPreferences::Preserve).await
}

pub(crate) async fn current_user_mode(
    state: &AppState,
    refresh: bool,
    preferences: ListPreferences,
) -> Result<Option<User>, String> {
    let intent = *state.auth_intent.borrow();
    let (al, origin) = {
        let _write = state.entry_lock.lock().await;
        state.check_auth_intent(intent)?;
        let al = state.anilist.lock().clone();
        if !al.has_token() {
            return Ok(None);
        }
        if !refresh {
            if let Some(u) = state.user.lock().clone() {
                return Ok(Some(u));
            }
        }
        (al, state.origin())
    };
    let token_used = al.token();
    let (result, _remote, _write) = tokio::select! {
        _ = state.auth_intent_changed(intent) => return Err(AUTH_REPLACED.into()),
        ready = async {
            let result = al.viewer().await;
            let remote = state.remote_lock.lock().await;
            let write = state.entry_lock.lock().await;
            (result, remote, write)
        } => ready,
    };
    let current_intent = state.auth_intent.borrow();
    if *current_intent != intent {
        return Err(AUTH_REPLACED.into());
    }
    if state.anilist.lock().token() != token_used
        || crate::sync_queue::account_id(&state.db) != origin.account_id
    {
        return Ok(state.user.lock().clone());
    }
    if !refresh {
        if let Some(user) = state.user.lock().clone() {
            return Ok(Some(user));
        }
    }
    let u = match result {
        Ok(u) => {
            let switching = origin.account_id != Some(u.id);
            if switching {
                *state.matchers.lock() = Arc::new(Vec::new());
            }
            let changed = match state.db.replace_account_with_preferences(
                token_used.as_deref().unwrap(),
                &u,
                preferences,
            ) {
                Ok(changed) => changed,
                Err(e) => {
                    if switching {
                        state.anilist.lock().set_token(token_used);
                        state.refresh_matchers();
                    }
                    return Err(e.to_string());
                }
            };
            state.anilist.lock().record_viewer(u.id);
            if changed || switching {
                state.invalidate_matchers();
            }
            u
        }
        // Keep cached data accessible during an outage.
        Err(e) => {
            if anilist::is_auth_rejection(&e) {
                drop(current_intent);
                return match reject_write_session(state, origin, &token_used) {
                    WriteAuthFailure::Expired => Ok(None),
                    failure => Err(failure.message().into()),
                };
            }
            if refresh {
                *state.user.lock() = None;
            }
            let name = state
                .db
                .get_setting(USERNAME_KEY)
                .map_err(|e| e.to_string())?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| e.to_string())?;
            let id = match preferences {
                ListPreferences::Native { .. } => origin
                    .account_id
                    .filter(|id| anilist::valid_account_id(*id))
                    .unwrap_or_default(),
                ListPreferences::Preserve => 0,
            };
            let mut user = state
                .db
                .cached_user()
                .map_err(|e| e.to_string())?
                .unwrap_or(User {
                    id,
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

fn catalog_origin_guard<'a>(
    state: &'a AppState,
    origin: crate::EventOrigin,
    expected_token: &Option<String>,
) -> Result<tokio::sync::watch::Ref<'a, u64>, String> {
    if expected_token.is_some() {
        write_origin_guard(state, origin, expected_token)
    } else {
        signed_out_origin_guard(state, origin)
    }
}

async fn catalog_read_err(
    state: &AppState,
    origin: crate::EventOrigin,
    expected_token: &Option<String>,
    error: anyhow::Error,
) -> String {
    if error.is::<anilist::UnsentRequest>() {
        return AUTH_REPLACED.into();
    }
    if expected_token.is_some() && anilist::is_auth_rejection(&error) {
        let _remote = state.remote_lock.lock().await;
        let _guard = state.entry_lock.lock().await;
        if let Err(session_err) = check_write_session(state, expected_token) {
            return session_err;
        }
        return write_err(state, origin, expected_token, &error);
    }
    error.to_string()
}

pub async fn search_anime_page(
    query: String,
    page: i64,
    state: &AppState,
) -> Result<SearchPage, String> {
    if query.trim().is_empty() || !(1..=10000).contains(&page) {
        return Err("invalid search or page".into());
    }
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let read = state.db.catalog_read();
    let mut result = match al.search_page(query.trim(), page, 25).await {
        Ok(result) => result,
        Err(e) => return Err(catalog_read_err(state, origin, &expected_token, e).await),
    };
    let _write = state.entry_lock.lock().await;
    let _intent = catalog_origin_guard(state, origin, &expected_token)?;
    state.cache_media_batch(&read, &mut result.items);
    Ok(result)
}

pub async fn get_season(season: String, year: i64, state: &AppState) -> Result<Vec<Media>, String> {
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let read = state.db.catalog_read();
    let mut media = match al.season_all(&season, year).await {
        Ok(media) => media,
        Err(e) => return Err(catalog_read_err(state, origin, &expected_token, e).await),
    };
    let _write = state.entry_lock.lock().await;
    let _intent = catalog_origin_guard(state, origin, &expected_token)?;
    state.cache_media_batch(&read, &mut media);
    Ok(media)
}

pub async fn get_recommendations(media_id: i64, state: &AppState) -> Result<Vec<Media>, String> {
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let read = state.db.catalog_read();
    let mut media = match al.recommendations(media_id).await {
        Ok(media) => media,
        Err(e) => return Err(catalog_read_err(state, origin, &expected_token, e).await),
    };
    let _write = state.entry_lock.lock().await;
    let _intent = catalog_origin_guard(state, origin, &expected_token)?;
    state.cache_media_batch(&read, &mut media);
    Ok(media)
}

pub async fn get_media(id: i64, state: &AppState) -> Result<Media, String> {
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let read = state.db.catalog_read();
    if let Some(m) = state.db.get_media(id).map_err(|e| e.to_string())? {
        return Ok(m);
    }
    let result = al.media_by_id(id).await;
    let mut v = match result {
        Ok(v) => v,
        Err(e) if e.is::<anilist::UnsentRequest>() => return Err(AUTH_REPLACED.into()),
        Err(e) if expected_token.is_some() && anilist::is_auth_rejection(&e) => {
            let _remote = state.remote_lock.lock().await;
            let _guard = state.entry_lock.lock().await;
            check_write_session(state, &expected_token)?;
            return Err(write_err(state, origin, &expected_token, &e));
        }
        Err(e) => {
            let _write = state.entry_lock.lock().await;
            let _intent = catalog_origin_guard(state, origin, &expected_token)?;
            if read.removed_media().contains(&id) {
                return Err(MEDIA_GONE.into());
            }
            if anilist::media_not_found(&e) {
                if read.has_newer_media(id) {
                    if let Some(media) = state
                        .db
                        .cached_catalog_media(id)
                        .map_err(|e| e.to_string())?
                    {
                        return Ok(media);
                    }
                }
                return Err(MEDIA_GONE.into());
            }
            return Err(e.to_string());
        }
    };
    let _write = state.entry_lock.lock().await;
    let _intent = catalog_origin_guard(state, origin, &expected_token)?;
    if read.removed_media().contains(&id) {
        return Err(MEDIA_GONE.into());
    }
    read.preserve_newer(&state.db, [&mut v]);
    state.db.upsert_media(&v).map_err(|e| e.to_string())?;
    read.record_write(&state.db, CatalogUpdate::Metadata, std::iter::once(v.id));
    state.refresh_media_matchers(&[v.id]);
    Ok(v)
}

pub async fn get_media_detail(id: i64, state: &AppState) -> Result<MediaDetail, String> {
    get_media_detail_inner(id, state).await
}

async fn get_media_detail_inner(id: i64, state: &AppState) -> Result<MediaDetail, String> {
    const SECTIONS: [&str; 3] = ["relations", "characters", "staff"];
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let check_origin = || catalog_origin_guard(state, origin, &expected_token);
    let key = format!("media_detail:{id}");
    let cached = || {
        state
            .db
            .get_setting(&key)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<MediaDetail>(&v).ok())
            .filter(|detail| {
                detail.media.id == id && (detail.cached_at.is_some() || detail.media.has_metadata())
            })
            .map(|mut detail| {
                detail.media.episodes = detail.media.known_episodes();
                for relation in &mut detail.relations {
                    relation.media.episodes = relation.media.known_episodes();
                }
                detail
            })
    };
    let read = state.db.catalog_read();
    let fallback = |error: anyhow::Error, removed: &std::collections::HashSet<i64>| {
        if removed.contains(&id) {
            return Err(MEDIA_GONE.into());
        }
        if let Some(mut detail) = cached() {
            detail.relations.retain(|r| !removed.contains(&r.media.id));
            state.db.restore_cached_metadata(&mut detail.media);
            state.db.restore_cached_relations(&mut detail.relations);
            detail.warning = Some(error.to_string());
            return Ok(detail);
        }
        match state
            .db
            .cached_catalog_media(id)
            .map_err(|e| e.to_string())?
        {
            Some(media) => Ok(MediaDetail {
                media,
                relations: vec![],
                characters: vec![],
                staff: vec![],
                cached_at: None,
                warning: Some(error.to_string()),
                unavailable_sections: SECTIONS.map(str::to_owned).into(),
            }),
            None => Err(error.to_string()),
        }
    };
    match al.media_detail(id).await {
        Ok(mut detail) => {
            let _write = state.entry_lock.lock().await;
            let _intent = check_origin()?;
            let removed = read.removed_media();
            if removed.contains(&id) {
                return Err(MEDIA_GONE.into());
            }
            detail.relations.retain(|r| !removed.contains(&r.media.id));
            read.preserve_newer(
                &state.db,
                std::iter::once(&mut detail.media).chain(
                    detail
                        .relations
                        .iter_mut()
                        .map(|relation| &mut relation.media),
                ),
            );
            if state.db.upsert_media_detail(&detail.media).is_ok() {
                read.record_write(&state.db, CatalogUpdate::Detail, std::iter::once(id));
            }
            if state
                .db
                .upsert_media_batch(
                    &detail
                        .relations
                        .iter()
                        .map(|r| r.media.clone())
                        .collect::<Vec<_>>(),
                )
                .is_ok()
            {
                read.record_write(
                    &state.db,
                    CatalogUpdate::Metadata,
                    detail.relations.iter().map(|relation| relation.media.id),
                );
            }
            let ids = std::iter::once(detail.media.id)
                .chain(detail.relations.iter().map(|relation| relation.media.id))
                .collect::<Vec<_>>();
            state.refresh_media_matchers(&ids);
            let unavailable = detail.unavailable_sections.clone();
            let newer = SECTIONS.map(|section| read.has_newer_section(id, section));
            if let Some(mut old) = (newer.contains(&true) || !unavailable.is_empty())
                .then(cached)
                .flatten()
            {
                for (section, newer) in SECTIONS.into_iter().zip(newer) {
                    let incoming_unavailable = unavailable.iter().any(|name| name == section);
                    if !incoming_unavailable && !newer {
                        continue;
                    }
                    match section {
                        "relations" => {
                            detail.relations = std::mem::take(&mut old.relations);
                            detail.relations.retain(|r| !removed.contains(&r.media.id));
                            state.db.restore_cached_relations(&mut detail.relations);
                        }
                        "characters" => detail.characters = std::mem::take(&mut old.characters),
                        "staff" => detail.staff = std::mem::take(&mut old.staff),
                        _ => {}
                    }
                    if newer {
                        detail.unavailable_sections.retain(|name| name != section);
                        if old.unavailable_sections.iter().any(|name| name == section) {
                            detail.unavailable_sections.push(section.into());
                        }
                    }
                }
            }
            detail.cached_at = Some(chrono::Utc::now().timestamp());
            if state.db.cache_media_detail_sections(&key, &detail).is_ok() {
                for section in SECTIONS {
                    if !unavailable.iter().any(|name| name == section) {
                        read.record_write(
                            &state.db,
                            CatalogUpdate::Section(section),
                            std::iter::once(id),
                        );
                    }
                }
            }
            Ok(detail)
        }
        Err(e) if e.is::<anilist::UnsentRequest>() => Err(AUTH_REPLACED.into()),
        Err(e) if expected_token.is_some() && anilist::is_auth_rejection(&e) => {
            let _remote = state.remote_lock.lock().await;
            let _guard = state.entry_lock.lock().await;
            check_write_session(state, &expected_token)?;
            Err(write_err(state, origin, &expected_token, &e))
        }
        Err(e) if anilist::media_not_found(&e) => {
            let _remote = state.remote_lock.lock().await;
            let _guard = state.entry_lock.lock().await;
            if read.has_newer_media(id) {
                let _intent = check_origin()?;
                return fallback(e, &read.removed_media());
            }
            if expected_token.is_some() {
                let intent = write_origin_guard(state, origin, &expected_token)?;
                crate::sync_queue::mark_media_missing_from(state, origin, intent, id, true)
                    .map_err(|e| e.to_string())?;
            } else {
                let _intent = signed_out_origin_guard(state, origin)?;
                state
                    .db
                    .commit_media_removal(Some(id), |_| Ok(()))
                    .map_err(|e| e.to_string())?;
            }
            Err(MEDIA_GONE.to_string())
        }
        Err(e) => {
            let _write = state.entry_lock.lock().await;
            let _intent = check_origin()?;
            fallback(e, &read.removed_media())
        }
    }
}

pub async fn get_airing_schedule(
    start: i64,
    end: i64,
    state: &AppState,
) -> Result<Vec<AiringItem>, String> {
    if !end
        .checked_sub(start)
        .is_some_and(|span| (1..=15 * 86_400).contains(&span))
    {
        return Err("invalid schedule range".to_string());
    }
    let origin = state.origin();
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let expected_token = al.token();
    let read = state.db.catalog_read();
    let mut items = match al.airing_schedule(start, end).await {
        Ok(items) => items,
        Err(e) => return Err(catalog_read_err(state, origin, &expected_token, e).await),
    };
    let _write = state.entry_lock.lock().await;
    let _intent = catalog_origin_guard(state, origin, &expected_token)?;
    let removed = read.removed_media();
    items.retain(|item| !removed.contains(&item.media.id));
    read.preserve_newer(&state.db, items.iter_mut().map(|item| &mut item.media));
    let on_list: std::collections::HashSet<i64> = state
        .db
        .entry_media_ids()
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    for item in &items {
        if on_list.contains(&item.media.id) && state.db.upsert_media(&item.media).is_ok() {
            read.record_write(
                &state.db,
                CatalogUpdate::Metadata,
                std::iter::once(item.media.id),
            );
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

#[cfg_attr(not(test), allow(dead_code))]
pub async fn sync_my_list(state: &Arc<AppState>) -> Result<Vec<ListEntry>, String> {
    sync_my_list_mode(state, ListPreferences::Preserve).await
}

pub(crate) async fn sync_my_list_mode(
    state: &Arc<AppState>,
    preferences: ListPreferences,
) -> Result<Vec<ListEntry>, String> {
    sync_my_list_inner(state, preferences).await
}

fn saved_api_account_id(state: &AppState) -> Option<i64> {
    crate::sync_queue::account_id(&state.db).filter(|id| anilist::valid_account_id(*id))
}

fn check_read_account(
    state: &AppState,
    account: i64,
    token: &Option<String>,
    intent: u64,
) -> Result<(), String> {
    check_write_session(state, token)?;
    state.check_auth_intent(intent)?;
    if saved_api_account_id(state) != Some(account) {
        return Err("Your AniList account changed. Try again.".into());
    }
    Ok(())
}

async fn sync_my_list_inner(
    state: &Arc<AppState>,
    preferences: ListPreferences,
) -> Result<Vec<ListEntry>, String> {
    let pre_intent = *state.auth_intent.borrow();
    let pre_token = state.anilist.lock().token();
    if pre_token.is_none() {
        return Err("not logged in".to_string());
    }
    let pre_account = saved_api_account_id(state);
    if pre_account.is_none() {
        // Profile adoption acquires remote_lock itself.
        current_user_mode(state, true, preferences).await?;
    }
    let remote = state.remote_lock.clone().lock_owned().await;
    let _write = state.entry_lock.lock().await;
    state.check_auth_intent(pre_intent)?;
    require_write_session(state, &pre_token)?;
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, pre_intent);
    if !al.has_token() {
        return Err("not logged in".to_string());
    }
    let account = saved_api_account_id(state)
        .ok_or("Reconnect to AniList to verify your account before syncing")?;
    if pre_account.is_some_and(|expected| expected != account) {
        return Err("Your AniList account changed. Try again.".into());
    }
    let token = pre_token;
    let intent = pre_intent;
    let origin = crate::EventOrigin {
        generation: intent,
        account_id: Some(account),
    };
    drop(_write);
    match crate::sync_queue::flush_from(state, origin).await {
        Ok(()) => {}
        Err(crate::sync_queue::EntryFlushError::Entry(error)) => {
            log::debug!("pending sync: {error}")
        }
        Err(crate::sync_queue::EntryFlushError::Batch(error)) => return Err(error),
    }
    let _write = state.entry_lock.lock().await;
    check_read_account(state, account, &token, intent)?;
    let mut score_format = state
        .db
        .cached_user()
        .map_err(|e| e.to_string())?
        .and_then(|user| user.score_format);
    drop(_write);
    let mut snapshot = None;
    for _ in 0..3 {
        // Pin every chunk so a format change cannot mix score scales.
        let read = state.db.catalog_read();
        let result = al.user_list(account, score_format.as_deref()).await;
        let entries = {
            let _write = state.entry_lock.lock().await;
            check_read_account(state, account, &token, intent)?;
            result.map_err(|e| write_err(state, origin, &token, &e))?
        };
        let result = al.viewer().await;
        let _write = state.entry_lock.lock().await;
        check_read_account(state, account, &token, intent)?;
        let user = result.map_err(|e| write_err(state, origin, &token, &e))?;
        let _intent = write_origin_guard(state, origin, &token)?;
        state.anilist.lock().record_viewer(user.id);
        if user.id != account {
            return Err("Account changed".into());
        }
        if score_format == user.score_format {
            snapshot = Some((entries, user, read));
            break;
        }
        score_format = user.score_format;
    }
    let (mut entries, user, read) = snapshot.ok_or(
        "Score format kept changing during sync. Try again after changing your AniList settings.",
    )?;
    let entry = state.entry_lock.clone().lock_owned().await;
    check_read_account(state, account, &token, intent)?;
    let token = token.ok_or("Not signed in")?;
    let state = state.clone();
    persist(entry, Some(remote), move || {
        let intent = state.auth_intent_guard(intent)?;
        let removed = read.removed_media();
        entries.retain(|entry| !removed.contains(&entry.media_id));
        read.preserve_newer(
            &state.db,
            entries.iter_mut().filter_map(|entry| entry.media.as_mut()),
        );
        state.db.replace_account_and_list(&token, &user, preferences, Some(&entries)).map_err(|e| {
            format!(
                "sync incomplete: the remote list could not be cached, remote deletions were not reconciled: {e}"
            )
        })?;
        read.record_write(
            &state.db,
            CatalogUpdate::Metadata,
            entries
                .iter()
                .filter_map(|entry| entry.media.as_ref().map(|media| media.id)),
        );
        *state.user.lock() = Some(user);
        drop(intent);
        let overlay = crate::sync_queue::overlay(&state);
        state.refresh_matchers();
        let saved_media = overlay.map_err(|e| e.to_string())?;
        let _ = state.db.prune_media_cache(30);
        state.db.entries_with_saved_media(saved_media).map_err(|e| e.to_string())
    })
    .await
}

pub async fn local_entries(state: &AppState) -> Result<Vec<ListEntry>, String> {
    local_entries_inner(state).await
}

async fn local_entries_inner(state: &AppState) -> Result<Vec<ListEntry>, String> {
    let _write = state.entry_lock.lock().await;
    if !state.anilist.lock().has_token() {
        return Ok(Vec::new());
    }
    let saved_media = crate::sync_queue::overlay(state).map_err(|e| e.to_string())?;
    let db = state.db.clone();
    tokio::task::spawn_blocking(move || db.entries_with_saved_media(saved_media))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

pub async fn local_entry_updates(
    media_ids: Vec<i64>,
    account_id: i64,
    state: &AppState,
) -> Result<Vec<Option<ListEntry>>, String> {
    local_entry_updates_inner(state, &media_ids, account_id).await
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
    local_entry_updates_locked(state, media_ids, account_id)
}

fn local_entry_updates_locked(
    state: &AppState,
    media_ids: &[i64],
    account_id: i64,
) -> Result<Vec<Option<ListEntry>>, String> {
    let mut matchers_changed = false;
    let entries = media_ids
        .iter()
        .map(|&media_id| {
            let mut stored = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
            let pending = state
                .db
                .pending_change(account_id, media_id)
                .map_err(|e| e.to_string())?;
            let Some(pending) = pending.filter(|pending| !pending.needs_cleanup_only()) else {
                return Ok(stored);
            };
            if !crate::sync_queue::overlay_allowed(state, account_id, &pending, stored.as_ref())
                .map_err(|e| e.to_string())?
            {
                return Ok(stored);
            }
            if pending.deleting {
                if let Some(entry) = stored.as_mut().filter(|entry| entry.media.is_none()) {
                    let total = state
                        .db
                        .cached_episode_total(media_id)
                        .map_err(|e| e.to_string())?;
                    entry.media = pending.saved_media(total);
                }
                return Ok(stored);
            }
            if pending.missing_media {
                if stored.is_some() {
                    state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
                    matchers_changed = true;
                }
                return Ok(None);
            }
            let entry = crate::sync_queue::projected_entry(state, &pending, stored.clone())
                .map_err(|e| e.to_string())?;
            if stored.as_ref().is_none_or(|stored| {
                stored.status != entry.status
                    || stored.progress != entry.progress
                    || stored.score != entry.score
                    || stored.repeat != entry.repeat
                    || stored.queued_creation != entry.queued_creation
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

pub async fn get_entry(media_id: i64, state: &AppState) -> Result<Option<ListEntry>, String> {
    let intent = *state.auth_intent.borrow();
    let token = state.anilist.lock().token();
    if token.is_none() {
        return Ok(None);
    }
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &token)?;
    state.check_auth_intent(intent)?;
    let Some(account) = crate::sync_queue::account_id(&state.db) else {
        // Offline pre-adoption sessions still have a local list before Viewer verifies an account ID.
        return state.db.get_entry(media_id).map_err(|e| e.to_string());
    };
    local_entry_updates_locked(state, &[media_id], account).map(|mut entries| entries.remove(0))
}

const MEDIA_GONE: &str = "AniList no longer has this anime. It was likely merged into another entry. Search for it to re-add the correct one.";
const EDITOR_ENTRY_CHANGED: &str =
    "Your AniList entry changed or was removed. Refresh your list and reopen the editor.";
const EDITOR_ENTRY_UNVERIFIED: &str =
    "This entry has no verified AniList ID. Refresh your list and reopen the editor.";

fn check_editor_account(
    target: Option<&crate::EntryEditTarget>,
    account: i64,
) -> Result<(), String> {
    if target.is_some_and(|target| target.account_id != Some(account)) {
        return Err("Your AniList account changed. Reopen the editor.".into());
    }
    Ok(())
}

/// Reaching the total completes unless another status is chosen.
/// Completed fills progress to the known total.
pub async fn update_entry(
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    state: &AppState,
) -> Result<ListEntry, String> {
    save_entry_inner(state, media_id, status, progress, score, repeat).await
}

pub async fn save_entry_inner(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
) -> Result<ListEntry, String> {
    save_entry_inner_checked(state, media_id, status, progress, score, repeat, None).await
}

impl crate::Core {
    #[allow(clippy::too_many_arguments)]
    pub async fn update_entry_in_format(
        &self,
        media_id: i64,
        status: Option<String>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
        expected_progress: Option<i64>,
        displayed_format: Option<String>,
    ) -> Result<ListEntry, String> {
        save_entry_inner_checked_impl(
            &self.state,
            media_id,
            status,
            progress,
            score,
            repeat,
            expected_progress,
            Some(displayed_format),
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_editor_entry(
        &self,
        media_id: i64,
        target: crate::EntryEditTarget,
        status: Option<String>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
        expected_progress: i64,
        displayed_format: Option<String>,
    ) -> Result<ListEntry, String> {
        save_entry_inner_checked_impl(
            &self.state,
            media_id,
            status,
            progress,
            score,
            repeat,
            Some(expected_progress),
            Some(displayed_format),
            Some(EntryWriteTarget::Editor(target)),
        )
        .await
    }

    pub async fn update_discovery_status(
        &self,
        media_id: i64,
        account_id: Option<i64>,
        identity: Option<crate::EntryIdentity>,
        status: String,
    ) -> Result<ListEntry, String> {
        save_entry_inner_checked_impl(
            &self.state,
            media_id,
            Some(status),
            None,
            None,
            None,
            None,
            None,
            Some(EntryWriteTarget::Discovery {
                account_id,
                identity,
            }),
        )
        .await
    }
}

enum EntryWriteTarget {
    Editor(crate::EntryEditTarget),
    Discovery {
        account_id: Option<i64>,
        identity: Option<crate::EntryIdentity>,
    },
}

const DISCOVERY_ENTRY_CHANGED: &str =
    "Your AniList entry changed. Review its status before trying again.";

pub async fn save_entry_inner_checked(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    expected_progress: Option<i64>,
) -> Result<ListEntry, String> {
    save_entry_inner_checked_impl(
        state,
        media_id,
        status,
        progress,
        score,
        repeat,
        expected_progress,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn save_entry_inner_checked_impl(
    state: &AppState,
    media_id: i64,
    status: Option<String>,
    progress: Option<i64>,
    score: Option<f64>,
    repeat: Option<i64>,
    expected_progress: Option<i64>,
    displayed_format: Option<Option<String>>,
    target: Option<EntryWriteTarget>,
) -> Result<ListEntry, String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    if score.is_some_and(|score| score != 0.0) {
        if let Some(displayed) = displayed_format {
            let current = state
                .db
                .cached_user()
                .map_err(|e| e.to_string())?
                .and_then(|user| user.score_format);
            if displayed.is_none() || displayed != current {
                return Err("Score format changed. Re-enter the score before saving.".into());
            }
        }
    }
    let account = entry_account(state, origin)?;
    let before = crate::sync_queue::current_entry(state, media_id).map_err(|e| e.to_string())?;
    let expects_absent = matches!(
        &target,
        Some(EntryWriteTarget::Discovery { identity: None, .. })
    );
    if let Some(EntryWriteTarget::Discovery {
        account_id,
        identity,
    }) = &target
    {
        let matches = match (identity.as_ref(), before.as_ref()) {
            (Some(identity), Some(entry)) => identity.matches(entry),
            (None, None) => true,
            _ => false,
        };
        if *account_id != origin.account_id || !matches {
            return Err(DISCOVERY_ENTRY_CHANGED.into());
        }
    }
    if let Some(EntryWriteTarget::Editor(target)) = target {
        check_editor_account(Some(&target), account)?;
        if target.identity == crate::EntryIdentity::Unverified {
            return Err(EDITOR_ENTRY_UNVERIFIED.into());
        }
        if before
            .as_ref()
            .is_none_or(|entry| !target.identity.matches(entry))
        {
            return Err(EDITOR_ENTRY_CHANGED.into());
        }
        if before.as_ref().is_some_and(|entry| entry.id.is_none())
            && state
                .db
                .pending_change(account, media_id)
                .map_err(|e| e.to_string())?
                .is_none_or(|pending| pending.base.is_some())
        {
            return Err(EDITOR_ENTRY_UNVERIFIED.into());
        }
    }
    if let Some(expected) = expected_progress {
        match before.as_ref() {
            None => return Err("the entry is no longer on your list".into()),
            Some(current) if current.progress != expected => {
                return Err(format!(
                    "Progress changed from {expected} to {}. Review this entry before saving.",
                    current.progress
                ));
            }
            Some(_) => {}
        }
    }
    let st = status.as_deref().map(parse_status).transpose()?;
    let status = st.map(|status| status.as_str().to_string());
    if status.is_none() && progress.is_none() && score.is_none() && repeat.is_none() {
        return Err("nothing to update".to_string());
    }
    let total = state
        .db
        .episode_total(media_id)
        .map_err(|e| e.to_string())?;
    let edit = |total, before: Option<&ListEntry>| {
        let progress = if st == Some(ListStatus::Repeating)
            && progress.is_none()
            && before.is_some_and(|entry| entry.status == "COMPLETED")
        {
            Some(0)
        } else {
            progress
        };
        let (status, repeat) =
            complete_progress_edit(status.clone(), progress, repeat, total, before);
        let progress = if st == Some(ListStatus::Completed) && total.is_some() {
            total
        } else {
            progress.map(|p| total.map_or(p.max(0), |t| p.max(0).min(t)))
        };
        (status, progress, repeat)
    };
    let to_patch = |total: Option<i64>, before: Option<&ListEntry>| {
        let total = if matches!(st, Some(ListStatus::Current | ListStatus::Repeating))
            && progress
                .zip(total)
                .is_some_and(|(progress, total)| progress > total)
            && crate::sync_queue::can_recheck_forced_completion(state, media_id)
                .map_err(|e| e.to_string())?
        {
            None
        } else {
            total
        };
        if st != Some(ListStatus::Completed) || total.is_none() {
            crate::sync_queue::validate_progress_edit(state, media_id, progress, total)
                .map_err(|e| e.to_string())?;
        }
        let requested_repeat = repeat;
        let (status, progress, repeat) = edit(total, before);
        validate_entry_values(progress, score, repeat)?;
        let automatic_completion = (st.is_none() && status.as_deref() == Some("COMPLETED"))
            .then(|| crate::sync_queue::AutomaticCompletion::new(requested_repeat));
        Ok::<_, String>(crate::sync_queue::Patch {
            status,
            progress,
            score,
            repeat,
            automatic_completion,
            ..Default::default()
        })
    };
    let initial_patch = to_patch(total, before.as_ref())?;
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    // A cache miss may already exist remotely.
    let saved = if before.is_none() {
        let existing = origin
            .account_id
            .map(|account| state.db.pending_change(account, media_id))
            .transpose()
            .map_err(|e| e.to_string())?
            .flatten()
            .filter(|pending| !pending.needs_cleanup_only());
        if expects_absent && existing.is_some() {
            return Err(DISCOVERY_ENTRY_CHANGED.into());
        }
        if let Some(existing) = &existing {
            if existing.deleting {
                return Err(
                    "Removal is pending. Retry it or keep the AniList version before editing."
                        .into(),
                );
            }
            if existing.missing_media {
                return Err(
                    "This anime was removed from AniList. Resolve its saved change before continuing."
                        .into(),
                );
            }
        }
        drop(_write);
        let result = if existing.is_some() && al.verified_account() != origin.account_id {
            // Merge edits into the saved baseline until the token owner is known.
            Ok((None, None))
        } else {
            match crate::sync_queue::verify_write_account(state, origin, &al, account).await {
                Ok(()) => {
                    al.entry_by_media_id_with_episodes(
                        media_id,
                        state.cached_score_format()?.as_deref(),
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        };
        let _write = state.entry_lock.lock().await;
        check_write_origin(state, origin, &expected_token)?;
        match result {
            Ok((remote, episodes)) => {
                crate::sync_queue::cache_reported_episodes(
                    state,
                    origin,
                    &expected_token,
                    media_id,
                    episodes,
                )?;
                let mut entry =
                    remote.map(|remote| crate::sync_queue::remote_entry(media_id, remote));
                if let Some(entry) = entry.as_mut().filter(|_| expects_absent) {
                    let intent = write_origin_guard(state, origin, &expected_token)?;
                    entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
                    state.db.upsert_entry(entry).map_err(|e| e.to_string())?;
                    drop(intent);
                    state.refresh_matchers();
                    crate::sync_queue::publish_from(state, origin, entry);
                    return Err(DISCOVERY_ENTRY_CHANGED.into());
                }
                if let Some(existing) = &existing {
                    let queue_account = origin.account_id.ok_or(AUTH_REPLACED)?;
                    let replaced = crate::sync_queue::cache_entry_replacement(
                        state,
                        origin,
                        &expected_token,
                        queue_account,
                        existing,
                        entry.as_ref(),
                    )?;
                    if replaced
                        || !crate::sync_queue::overlay_allowed(
                            state,
                            queue_account,
                            existing,
                            entry.as_ref(),
                        )
                        .map_err(|e| e.to_string())?
                    {
                        return Err(EDITOR_ENTRY_CHANGED.into());
                    }
                    let projected =
                        crate::sync_queue::projected_entry(state, existing, entry.clone())
                            .map_err(|e| e.to_string())?;
                    let patch = to_patch(episodes.unwrap_or(total), Some(&projected))?;
                    let intent = write_origin_guard(state, origin, &expected_token)?;
                    let (account, pending, _) = crate::sync_queue::prepare_entry(
                        state,
                        media_id,
                        patch,
                        None,
                        entry.as_ref(),
                    )
                    .map_err(|e| e.to_string())?
                    .ok_or(AUTH_REPLACED)?;
                    let saved = crate::sync_queue::commit_pending_projection(
                        state, origin, intent, account, &pending, entry,
                    )?;
                    state.clear_tracking_prompt_if_advanced(origin, &saved);
                    return Ok(saved);
                }
                if let Some(entry) = &entry {
                    let intent = write_origin_guard(state, origin, &expected_token)?;
                    state.db.upsert_entry(entry).map_err(|e| e.to_string())?;
                    drop(intent);
                    state.refresh_matchers();
                }
                save_entry_unlocked(
                    state,
                    origin,
                    media_id,
                    to_patch(episodes.unwrap_or(total), entry.as_ref())?,
                    EntrySave::Manual,
                    _write,
                )
                .await?
            }
            Err(e) if anilist::media_not_found(&e) => {
                let intent = write_origin_guard(state, origin, &expected_token)?;
                crate::sync_queue::mark_media_missing_from(state, origin, intent, media_id, true)
                    .map_err(|e| e.to_string())?;
                return Err(MEDIA_GONE.to_string());
            }
            Err(e) if anilist::is_retryable(&e) => {
                let patch = if let Some(existing) = &existing {
                    let projected = crate::sync_queue::projected_entry(state, existing, None)
                        .map_err(|e| e.to_string())?;
                    to_patch(total, Some(&projected))?
                } else {
                    initial_patch
                };
                let intent = write_origin_guard(state, origin, &expected_token)?;
                if let Some((account, mut pending, _)) =
                    crate::sync_queue::prepare_entry(state, media_id, patch, None, None)
                        .map_err(|e| e.to_string())?
                {
                    if !pending.conflict {
                        pending.error = Some(e.to_string());
                    }
                    crate::sync_queue::commit_pending_projection(
                        state, origin, intent, account, &pending, None,
                    )?
                } else {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(write_err(state, origin, &expected_token, &e)),
        }
    } else {
        save_entry_unlocked(
            state,
            origin,
            media_id,
            initial_patch,
            EntrySave::Manual,
            _write,
        )
        .await?
    };
    state.clear_tracking_prompt_if_advanced(origin, &saved);
    Ok(saved)
}

fn complete_progress_edit(
    status: Option<String>,
    progress: Option<i64>,
    repeat: Option<i64>,
    total: Option<i64>,
    before: Option<&ListEntry>,
) -> (Option<String>, Option<i64>) {
    if status.is_none()
        && progress.is_some_and(|progress| reopens_completed(before, total, progress))
    {
        return (Some("CURRENT".into()), repeat);
    }
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
            .map(|entry| entry.repeat.saturating_add(1).min(1000))
    });
    let status = if before.is_none_or(|entry| entry.status != "COMPLETED") {
        Some("COMPLETED".into())
    } else {
        status
    };
    (status, repeat)
}

pub(crate) fn reopens_completed(
    before: Option<&ListEntry>,
    total: Option<i64>,
    progress: i64,
) -> bool {
    before.is_some_and(|entry| {
        entry.status == "COMPLETED"
            && progress.max(0) < total.filter(|total| *total > 0).unwrap_or(entry.progress)
    })
}

enum EntrySave<'a> {
    Manual,
    AutomaticPlayback(i64),
    Prompt {
        event: &'a crate::Event,
        episode: i64,
        total: Option<i64>,
    },
}

const PROMPT_CHANGED: &str = "Your list changed. Review the playback update again.";

/// Caller holds remote_lock. Explicit values are sent even when they match the cache.
async fn save_entry_unlocked(
    state: &AppState,
    origin: crate::EventOrigin,
    media_id: i64,
    patch: crate::sync_queue::Patch,
    source: EntrySave<'_>,
    mut write_guard: tokio::sync::MutexGuard<'_, ()>,
) -> Result<ListEntry, String> {
    let playback_episode = match source {
        EntrySave::AutomaticPlayback(episode) | EntrySave::Prompt { episode, .. } => Some(episode),
        EntrySave::Manual => None,
    };
    let allow_adjusted_completion =
        playback_episode.is_none() && patch.automatic_completion.is_none();
    let progress = patch.progress;
    let score = patch.score;
    let mut repeat = patch.repeat;
    validate_entry_values(progress, score, repeat)?;
    let mut st = patch.status.as_deref().map(parse_status).transpose()?;
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let token = al.token();
    check_write_origin(state, origin, &token)?;
    let account = entry_account(state, origin)?;
    let mut before =
        crate::sync_queue::current_entry(state, media_id).map_err(|e| e.to_string())?;
    let before_identity = before.as_ref().map(ListEntry::identity);
    let check_prompt = || -> Result<(), String> {
        if let EntrySave::Prompt { event, total, .. } = source {
            if !state.event_is_current(event)
                || state
                    .db
                    .episode_total(media_id)
                    .map_err(|e| e.to_string())?
                    != total
                || crate::sync_queue::current_entry(state, media_id)
                    .map_err(|e| e.to_string())?
                    .is_none_or(|entry| {
                        Some(entry.identity()) != before_identity
                            || progress.is_some_and(|target| entry.progress >= target)
                    })
            {
                return Err(PROMPT_CHANGED.into());
            }
        }
        Ok(())
    };
    let prompt_guard = || {
        let EntrySave::Prompt { event, .. } = source else {
            return Ok(None);
        };
        let retained = state.last_tracking_prompt.lock();
        if retained
            .as_ref()
            .is_some_and(|current| same_tracking_prompt(&current.event, event))
        {
            Ok(Some(retained))
        } else {
            Err(PROMPT_CHANGED.to_string())
        }
    };
    let mut staged = crate::sync_queue::prepare_entry_for_local(
        state,
        media_id,
        patch,
        playback_episode,
        before.as_mut(),
    )
    .map_err(|e| e.to_string())?;
    if let Some((account, pending, _)) = staged
        .as_ref()
        .filter(|(_, pending, already_pending)| *already_pending || pending.conflict)
    {
        let _prompt = prompt_guard()?;
        let intent = write_origin_guard(state, origin, &token)?;
        return crate::sync_queue::commit_pending_projection(
            state, origin, intent, *account, pending, before,
        );
    }
    if playback_episode.is_some() && staged.is_none() {
        return Err("Reconnect to AniList before tracking progress for this account.".into());
    }
    let entry_id = before.as_ref().and_then(|entry| entry.id);
    if let Some((account, pending, _)) = staged.as_ref().filter(|_| score.is_some_and(|s| s != 0.0))
    {
        drop(write_guard);
        let result = crate::sync_queue::check_score_format_detailed(
            state, origin, &al, *account, pending, false,
        )
        .await;
        write_guard = state.entry_lock.lock().await;
        if let Err(error) = result {
            if error.auth_rejected || error.account_changed {
                return Err(error.message);
            }
            if error.format_mismatch || error.retryable {
                let intent = write_origin_guard(state, origin, &token)?;
                let mut pending = pending.clone();
                pending.conflict = error.format_mismatch;
                pending.error = Some(error.message.clone());
                return crate::sync_queue::commit_pending_projection(
                    state, origin, intent, *account, &pending, before,
                );
            }
            return Err(error.message);
        }
        check_write_origin(state, origin, &token)?;
    }
    let verified = if state.anilist.lock().verified_account() != Some(account) {
        drop(write_guard);
        let result = crate::sync_queue::verify_write_account(state, origin, &al, account).await;
        write_guard = state.entry_lock.lock().await;
        check_write_origin(state, origin, &token)?;
        if result.is_ok() || result.as_ref().is_err_and(anilist::is_retryable) {
            check_prompt()?;
        }
        result
    } else {
        Ok(())
    };
    let score_format = state.cached_score_format()?;
    let preflight = if let Err(error) = verified {
        Err(error)
    } else if let Some((account, pending, _)) =
        staged.as_mut().filter(|_| playback_episode.is_some())
    {
        drop(write_guard);
        let result = al
            .entry_by_media_id_with_episodes(media_id, score_format.as_deref())
            .await;
        write_guard = state.entry_lock.lock().await;
        check_write_origin(state, origin, &token)?;
        if !result.as_ref().is_err_and(anilist::is_auth_rejection) {
            check_prompt()?;
        }
        match result {
            Ok((remote, total)) => {
                crate::sync_queue::cache_reported_episodes(state, origin, &token, media_id, total)?;
                if let (EntrySave::Prompt { episode, .. }, Some(fresh_total)) = (&source, total) {
                    let target =
                        progress_for_total(before.as_ref(), fresh_total, *episode).progress;
                    if progress != Some(target) {
                        return Err(PROMPT_CHANGED.into());
                    }
                }
                if !pending.refresh_playback_total(state, total.flatten())? {
                    return Err(EPISODE_EXCEEDS_TOTAL.into());
                }
                st = pending
                    .patch
                    .status
                    .as_deref()
                    .map(parse_status)
                    .transpose()?;
                repeat = pending.patch.repeat;
                let snapshot = remote
                    .as_ref()
                    .map(|entry| crate::sync_queue::remote_entry(media_id, entry.clone()));
                if snapshot
                    .as_ref()
                    .is_some_and(|entry| crate::sync_queue::already_applied(pending, entry))
                {
                    Ok(remote)
                } else if crate::sync_queue::conflict_in_format(
                    pending,
                    snapshot.as_ref(),
                    score_format.as_deref(),
                ) {
                    let _prompt = prompt_guard()?;
                    let intent = write_origin_guard(state, origin, &token)?;
                    let mut pending = pending.clone();
                    pending.conflict = true;
                    pending.error = Some("AniList changed before this playback update. Choose which version to keep.".into());
                    return crate::sync_queue::commit_pending_projection(
                        state, origin, intent, *account, &pending, before,
                    );
                } else {
                    Ok(None)
                }
            }
            Err(error) if anilist::media_not_found(&error) => {
                let intent = write_origin_guard(state, origin, &token)?;
                crate::sync_queue::commit_missing_media(
                    state,
                    origin,
                    intent,
                    media_id,
                    Some((*account, pending.clone())),
                    false,
                )
                .map_err(|e| e.to_string())?;
                return Err(MEDIA_GONE.to_string());
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(None)
    };
    let dispatched = matches!(preflight, Ok(None));
    let prompt = if dispatched { prompt_guard()? } else { None };
    let intent = write_origin_guard(state, origin, &token)?;
    if let Some((account, pending, _)) = staged.as_ref().filter(|_| dispatched) {
        state
            .db
            .put_pending(*account, pending)
            .map_err(|e| e.to_string())?;
    }
    drop(intent);
    drop(prompt);
    drop(write_guard);
    let saved = match preflight {
        Err(error) => Err(error),
        Ok(Some(saved)) => Ok((saved, None)),
        Ok(None) => {
            al.save_entry(
                media_id,
                entry_id,
                st,
                progress,
                score,
                repeat,
                score_format.as_deref(),
                allow_adjusted_completion,
            )
            .await
        }
    };
    if crate::sync_queue::prepare_score_confirmation(state, origin, &token, media_id, score, &saved)
        .await?
    {
        if let Some((account, pending, _)) = staged.as_ref() {
            let refused = saved
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<anilist::IncompatibleEntryWrite>())
                .map(|rejected| {
                    let mut refused = pending.clone();
                    refused.record_incompatible_write(rejected.clone(), None);
                    refused
                });
            let checked = refused.as_ref().unwrap_or(pending);
            if let Err(error) = crate::sync_queue::check_score_format_detailed(
                state, origin, &al, *account, checked, dispatched,
            )
            .await
            {
                let _write = state.entry_lock.lock().await;
                if error.auth_rejected {
                    return Err(error.message);
                }
                if error.account_changed {
                    let intent = write_origin_guard(state, origin, &token).ok();
                    let notify = refused.is_some() || dispatched;
                    if let Some(refused) = refused {
                        state
                            .db
                            .put_pending(*account, &refused)
                            .map_err(|e| e.to_string())?;
                    }
                    if let Some(intent) = intent.filter(|_| notify) {
                        drop(intent);
                        state.emit_from(
                            origin,
                            crate::EventKind::PendingChanged(Some(vec![media_id])),
                        );
                    }
                    return Err(error.message);
                }
                let intent = match write_origin_guard(state, origin, &token) {
                    Ok(intent) => intent,
                    Err(origin_error) => {
                        if let Some(mut refused) = refused {
                            if error.format_mismatch {
                                refused.error = Some(error.message);
                            }
                            state
                                .db
                                .put_pending(*account, &refused)
                                .map_err(|e| e.to_string())?;
                        }
                        return Err(origin_error);
                    }
                };
                let pending = if let Some(mut refused) = refused {
                    if error.format_mismatch {
                        refused.error = Some(error.message.clone());
                    } else {
                        refused.record_retry_error(&error.message);
                    }
                    refused
                } else {
                    let mut pending = pending.clone();
                    pending.conflict = error.format_mismatch || !error.retryable;
                    pending.error = Some(error.message.clone());
                    pending
                };
                if error.format_mismatch || error.retryable {
                    return match crate::sync_queue::commit_pending_projection(
                        state, origin, intent, *account, &pending, before,
                    ) {
                        Ok(entry) => Ok(entry),
                        Err(commit_error) if pending.conflict || dispatched => {
                            if let Err(pending_err) = state.db.put_pending(*account, &pending) {
                                let label = if pending.conflict {
                                    "failed to mark pending change conflicted"
                                } else {
                                    "failed to record pending change error"
                                };
                                return Err(format!("{commit_error}; {label}: {pending_err}"));
                            }
                            state.emit_from(
                                origin,
                                crate::EventKind::PendingChanged(Some(vec![media_id])),
                            );
                            Err(commit_error)
                        }
                        Err(commit_error) => Err(commit_error),
                    };
                }
                state
                    .db
                    .put_pending(*account, &pending)
                    .map_err(|e| e.to_string())?;
                drop(intent);
                state.emit_from(
                    origin,
                    crate::EventKind::PendingChanged(Some(vec![media_id])),
                );
                return Err(error.message);
            }
        }
    }
    let _write = state.entry_lock.lock().await;
    if let Err(e) = &saved {
        let staged_write = staged.as_mut().filter(|_| dispatched);
        if anilist::is_auth_rejection(e) {
            let unconfirmed = e.is::<anilist::UnconfirmedEntryWrite>();
            let mut staged_write = staged_write;
            let rejection = clear_rejected_session_after(state, origin, &token, || {
                let Some((account, pending, _)) = staged_write.as_deref_mut() else {
                    return Ok(());
                };
                if unconfirmed {
                    pending.error = Some(SESSION_EXPIRED.into());
                    state
                        .db
                        .put_pending(*account, pending)
                        .map_err(|e| e.to_string())
                } else {
                    remove_rejected_pending(state, *account, pending, SESSION_EXPIRED)
                }
            });
            if let Some((settlement, failure)) = rejection {
                settlement?;
                return Err(failure.message().into());
            }
            if let Some((account, pending, _)) = staged_write.filter(|_| !unconfirmed) {
                restore_rejected_entry(state, *account, pending, &e.to_string(), origin)
                    .map_err(|e| e.to_string())?;
            }
            return Err(AUTH_REPLACED.into());
        }
        // Auth intents can advance here, but replacing the session still needs both locks.
        if let Some((account, pending, _)) = staged_write {
            let rejected = e.downcast_ref::<anilist::IncompatibleEntryWrite>();
            let missing_entry = entry_id.is_some() && anilist::media_not_found(e);
            let missing_media = entry_id.is_none() && anilist::media_not_found(e);
            if let Some(rejected) = rejected {
                pending.record_incompatible_write(rejected.clone(), None);
            } else if missing_entry {
                pending.conflict = true;
                pending.error = Some(
                    "Your AniList entry changed or was removed. Choose which version to keep."
                        .into(),
                );
            } else if missing_media {
                match write_origin_guard(state, origin, &token) {
                    Ok(intent) => {
                        match state.db.commit_media_removal(Some(media_id), |tx| {
                            tx.execute(
                                "DELETE FROM pending_change WHERE account_id = ? AND media_id = ?",
                                [*account, media_id],
                            )?;
                            tx.execute("DELETE FROM list_entry WHERE media_id = ?", [media_id])?;
                            Ok(())
                        }) {
                            Ok(()) => {
                                drop(intent);
                                state.clear_tracking_prompt_for_media(origin, media_id);
                                state.refresh_matchers();
                                state.emit_from(
                                    origin,
                                    crate::EventKind::PendingChanged(Some(vec![media_id])),
                                );
                                return Err(MEDIA_GONE.to_string());
                            }
                            Err(removal_err) => {
                                pending.mark_media_missing();
                                let fallback_err = match state.db.durable_write(|tx| {
                                    tx.execute(
                                        "INSERT OR REPLACE INTO pending_change (account_id,media_id,payload) VALUES (?,?,?)",
                                        rusqlite::params![
                                            *account,
                                            pending.media_id,
                                            serde_json::to_string(pending)?
                                        ],
                                    )?;
                                    tx.execute(
                                        "DELETE FROM list_entry WHERE media_id = ?",
                                        [media_id],
                                    )?;
                                    Ok(())
                                }) {
                                    Ok(()) => None,
                                    Err(_) => state.db.put_pending(*account, pending).err(),
                                };
                                drop(intent);
                                state.clear_tracking_prompt_for_media(origin, media_id);
                                state.refresh_matchers();
                                state.emit_from(
                                    origin,
                                    crate::EventKind::PendingChanged(Some(vec![media_id])),
                                );
                                if let Some(conflict_err) = fallback_err {
                                    return Err(format!(
                                        "{removal_err}; failed to mark pending change missing: {conflict_err}"
                                    ));
                                }
                                return Err(removal_err.to_string());
                            }
                        }
                    }
                    Err(error) => {
                        restore_rejected_entry(state, *account, pending, &e.to_string(), origin)
                            .map_err(|e| e.to_string())?;
                        return Err(error);
                    }
                }
            } else if !anilist::is_retryable(e) && !e.is::<anilist::UnconfirmedEntryWrite>() {
                restore_rejected_entry(state, *account, pending, &e.to_string(), origin)
                    .map_err(|e| e.to_string())?;
            }
            if rejected.is_some() || missing_entry {
                match write_origin_guard(state, origin, &token) {
                    Ok(intent) => {
                        let mut episodes_changed = false;
                        let cache_err = match rejected.and_then(|r| r.total) {
                            Some(total) => match state.db.cache_episode_total(media_id, total) {
                                Ok(changed) => {
                                    episodes_changed = changed;
                                    None
                                }
                                Err(error) => Some(error),
                            },
                            None => None,
                        };
                        let error = if let Some(error) = cache_err {
                            if let Err(conflict_err) = state.db.put_pending(*account, pending) {
                                return Err(format!(
                                    "{error}; failed to mark pending change conflicted: {conflict_err}"
                                ));
                            }
                            drop(intent);
                            error.to_string()
                        } else {
                            match crate::sync_queue::commit_pending_projection(
                                state, origin, intent, *account, pending, before,
                            ) {
                                Ok(entry) => return Ok(entry),
                                Err(error) => {
                                    let fallback = state.db.put_pending(*account, pending);
                                    if episodes_changed {
                                        state.refresh_matchers();
                                    }
                                    if let Err(conflict_err) = fallback {
                                        return Err(format!(
                                            "{error}; failed to mark pending change conflicted: {conflict_err}"
                                        ));
                                    }
                                    error
                                }
                            }
                        };
                        state.emit_from(
                            origin,
                            crate::EventKind::PendingChanged(Some(vec![media_id])),
                        );
                        return Err(error);
                    }
                    Err(error) => {
                        state
                            .db
                            .put_pending(*account, pending)
                            .map_err(|e| e.to_string())?;
                        return Err(error);
                    }
                }
            }
        }
    }
    let (saved, confirmed_total) = match saved {
        Ok(s) => s,
        Err(e) => {
            check_write_origin(state, origin, &token)?;
            if e.is::<anilist::IncompatibleEntryWrite>() {
                let rejected = e.downcast::<anilist::IncompatibleEntryWrite>().unwrap();
                crate::sync_queue::cache_reported_episodes(
                    state,
                    origin,
                    &token,
                    media_id,
                    rejected.total,
                )?;
                return Err(rejected.to_string());
            }
            if entry_id.is_some() && anilist::media_not_found(&e) {
                let error =
                    "Your AniList entry changed or was removed. Choose which version to keep.";
                return Err(error.into());
            }
            if anilist::media_not_found(&e) {
                let intent = write_origin_guard(state, origin, &token)?;
                crate::sync_queue::commit_missing_media(
                    state, origin, intent, media_id, None, true,
                )
                .map_err(|e| e.to_string())?;
                return Err(MEDIA_GONE.to_string());
            }
            let unconfirmed_episodes = e
                .downcast_ref::<anilist::UnconfirmedEntryWrite>()
                .and_then(|unconfirmed| unconfirmed.0);
            if let Some((account, mut pending, _)) = staged {
                if anilist::is_retryable(&e) {
                    let _prompt = if dispatched {
                        None
                    } else {
                        check_prompt()?;
                        prompt_guard()?
                    };
                    let intent = write_origin_guard(state, origin, &token)?;
                    pending.error = Some(e.to_string());
                    let mut episodes_changed = false;
                    let cache_err = match unconfirmed_episodes {
                        Some(total) => match state.db.cache_episode_total(media_id, total) {
                            Ok(changed) => {
                                episodes_changed = changed;
                                None
                            }
                            Err(error) => Some(error.to_string()),
                        },
                        None => None,
                    };
                    if let Some(error) = cache_err {
                        if dispatched {
                            if let Err(pending_err) = state.db.put_pending(account, &pending) {
                                return Err(format!(
                                    "{error}; failed to record pending change error: {pending_err}"
                                ));
                            }
                            drop(intent);
                            state.emit_from(
                                origin,
                                crate::EventKind::PendingChanged(Some(vec![media_id])),
                            );
                        }
                        return Err(error);
                    }
                    return match crate::sync_queue::commit_pending_projection(
                        state, origin, intent, account, &pending, before,
                    ) {
                        Ok(entry) => Ok(entry),
                        Err(commit_error) if dispatched => {
                            let fallback = state.db.put_pending(account, &pending);
                            if episodes_changed {
                                state.refresh_matchers();
                            }
                            if let Err(pending_err) = fallback {
                                return Err(format!(
                                    "{commit_error}; failed to record pending change error: {pending_err}"
                                ));
                            }
                            state.emit_from(
                                origin,
                                crate::EventKind::PendingChanged(Some(vec![media_id])),
                            );
                            Err(commit_error)
                        }
                        Err(commit_error) => {
                            if episodes_changed {
                                state.refresh_matchers();
                            }
                            Err(commit_error)
                        }
                    };
                }
            }
            if let Some(unconfirmed) = e.downcast_ref::<anilist::UnconfirmedEntryWrite>() {
                crate::sync_queue::cache_reported_episodes(
                    state,
                    origin,
                    &token,
                    media_id,
                    unconfirmed.0,
                )?;
            }
            return Err(e.to_string());
        }
    };
    let mut entry = ListEntry {
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
        media: None,
        queued_creation: if dispatched && entry_id.is_none() {
            staged
                .as_ref()
                .and_then(|(_, pending, _)| pending.patch.creation_key.clone())
        } else {
            None
        },
    };
    // Settle a confirmed write before a waiting auth change takes remote_lock.
    let origin_result = write_origin_guard(state, origin, &token);
    if let Err(origin_error) = &origin_result {
        if !dispatched {
            return Err(origin_error.clone());
        }
        if !write_session_account_matches(state, origin, &token) {
            if let Some((account, _, _)) = staged {
                if let Err(error) = state.db.remove_pending(account, media_id) {
                    state
                        .db
                        .mark_acknowledged_pending(account, &entry)
                        .map_err(|e| e.to_string())?;
                    return Err(error.to_string());
                }
            }
            return Err(origin_error.clone());
        }
    }
    // A cached read can apply the staged status during the request.
    let replaced = state.db.get_entry(media_id).ok().flatten();
    state
        .db
        .cache_acknowledged_entry(
            staged.as_ref().map(|(account, _, _)| *account),
            &mut entry,
            confirmed_total,
        )
        .map_err(|e| e.to_string())?;
    entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
    let acknowledgement = staged.map_or(Ok(()), |(account, _, _)| {
        state.db.remove_pending(account, media_id)
    });
    match origin_result {
        Ok(intent) => {
            drop(intent);
            if acknowledgement.is_err()
                || confirmed_total.is_some()
                || replaced.as_ref().map(|row| row.status.as_str()) != Some(entry.status.as_str())
            {
                state.refresh_matchers();
            }
            if acknowledgement.is_err() {
                crate::sync_queue::publish_from(state, origin, &entry);
                state.emit_from(
                    origin,
                    crate::EventKind::PendingChanged(Some(vec![media_id])),
                );
            }
            acknowledgement.map_err(|e| e.to_string())?;
            Ok(entry)
        }
        Err(origin_error) => {
            state.refresh_matchers();
            acknowledgement.map_err(|e| e.to_string())?;
            Err(origin_error)
        }
    }
}

fn remove_rejected_pending(
    state: &AppState,
    account: i64,
    pending: &mut crate::sync_queue::Pending,
    rejection: &str,
) -> Result<(), String> {
    if let Err(ack_err) = state.db.remove_pending(account, pending.media_id) {
        pending.conflict = true;
        pending.error = Some(rejection.to_owned());
        if let Err(put_err) = state.db.put_pending(account, pending) {
            return Err(format!(
                "{ack_err}; failed to mark pending change conflicted: {put_err}"
            ));
        }
        return Err(ack_err.to_string());
    }
    Ok(())
}

fn restore_rejected_entry(
    state: &AppState,
    account: i64,
    pending: &crate::sync_queue::Pending,
    rejection: &str,
    origin: crate::EventOrigin,
) -> anyhow::Result<()> {
    if crate::sync_queue::account_id(&state.db) != Some(account) {
        let mut retained = pending.clone();
        return remove_rejected_pending(state, account, &mut retained, rejection)
            .map_err(anyhow::Error::msg);
    }
    match state.db.restore_and_remove_pending(
        account,
        pending.media_id,
        pending.base.as_ref(),
        None,
    ) {
        Ok(()) => {
            state.refresh_matchers();
            state.emit_from(origin, crate::EventKind::PendingChanged(None));
            Ok(())
        }
        Err(rollback_err) => {
            // Mark retained rejections conflicted so background sync cannot replay them.
            let mut retained = pending.clone();
            retained.conflict = true;
            retained.error = Some(rejection.to_string());
            let conflict_err = state.db.put_pending(account, &retained).err();
            state.refresh_matchers();
            state.emit_from(
                origin,
                crate::EventKind::PendingChanged(Some(vec![pending.media_id])),
            );
            if let Some(conflict_err) = conflict_err {
                return Err(anyhow::anyhow!(
                    "{rollback_err}; failed to mark pending change conflicted: {conflict_err}"
                ));
            }
            Err(rollback_err)
        }
    }
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
pub async fn set_progress(
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
    state: &AppState,
) -> Result<ListEntry, String> {
    let (outcome, origin) =
        set_progress_inner_with_origin(state, media_id, progress, expected, None).await?;
    let (ControlFlow::Continue(entry) | ControlFlow::Break(entry)) = outcome;
    crate::sync_queue::publish_from(state, origin, &entry);
    Ok(entry)
}

impl crate::Core {
    /// A baseline conflict breaks with the current entry and skips the write.
    pub async fn set_list_progress(
        &self,
        media_id: i64,
        progress: i64,
        expected: i64,
        account_id: Option<i64>,
        identity: crate::EntryIdentity,
    ) -> Result<ControlFlow<ListEntry, ListEntry>, String> {
        let (outcome, origin) = set_progress_inner_with_origin(
            &self.state,
            media_id,
            progress,
            Some(expected),
            Some((account_id, identity)),
        )
        .await?;
        if let ControlFlow::Continue(entry) = &outcome {
            crate::sync_queue::publish_from(&self.state, origin, entry);
        }
        Ok(outcome)
    }
}

#[cfg(test)]
pub async fn set_progress_inner(
    state: &AppState,
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
) -> Result<ListEntry, String> {
    set_progress_inner_with_origin(state, media_id, progress, expected, None)
        .await
        .map(|(ControlFlow::Continue(entry) | ControlFlow::Break(entry), _)| entry)
}

async fn set_progress_inner_with_origin(
    state: &AppState,
    media_id: i64,
    progress: i64,
    expected: Option<i64>,
    target: Option<(Option<i64>, crate::EntryIdentity)>,
) -> Result<(ControlFlow<ListEntry, ListEntry>, crate::EventOrigin), String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let mut write = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    let cur = crate::sync_queue::current_entry(state, media_id).map_err(|e| e.to_string())?;
    // A missing row was deleted. Never recreate it from a buffered edit.
    let Some(entry) = cur else {
        return Err("the entry is no longer on your list".to_string());
    };
    if let Some((account_id, identity)) = &target {
        if *account_id != origin.account_id || !identity.matches(&entry) {
            return Err("Your AniList entry changed. Review before saving.".into());
        }
    }
    if let Some(exp) = expected {
        if entry.progress != exp {
            let mut entry = entry;
            entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
            return Ok((ControlFlow::Break(entry), origin));
        }
    }
    // Existing queued edits check their baseline on replay.
    let lookup_error = match target.as_ref().zip(origin.account_id) {
        Some(((_, identity), account))
            if state
                .db
                .pending_change(account, media_id)
                .map_err(|e| e.to_string())?
                .is_none_or(|pending| pending.needs_cleanup_only()) =>
        {
            let al = state
                .anilist
                .lock()
                .for_intent(&state.auth_intent, origin.generation);
            let format = state.cached_score_format()?;
            drop(write);
            let result =
                match crate::sync_queue::verify_write_account(state, origin, &al, account).await {
                    Ok(()) => {
                        al.entry_by_media_id_with_episodes(media_id, format.as_deref())
                            .await
                    }
                    Err(error) => Err(error),
                };
            write = state.entry_lock.lock().await;
            check_write_origin(state, origin, &expected_token)?;
            match result {
                Ok((remote, total)) => {
                    let remote = remote.map(|saved| {
                        let mut remote = crate::sync_queue::remote_entry(media_id, saved);
                        if remote.id == entry.id {
                            remote.queued_creation.clone_from(&entry.queued_creation);
                        }
                        remote
                    });
                    let intent = write_origin_guard(state, origin, &expected_token)?;
                    state
                        .db
                        .durable_write_with_episode_total(media_id, total, |tx| match &remote {
                            Some(entry) => Db::write_entry(tx, entry),
                            None => Db::delete_entry_from(tx, media_id),
                        })
                        .map_err(|e| e.to_string())?;
                    drop(intent);
                    state.refresh_matchers();
                    let Some(mut remote) = remote else {
                        state.clear_tracking_prompt_for_media(origin, media_id);
                        return Err("the entry is no longer on your list".into());
                    };
                    if !identity.matches(&remote) || expected != Some(remote.progress) {
                        remote.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
                        state.clear_tracking_prompt_if_advanced(origin, &remote);
                        return Ok((ControlFlow::Break(remote), origin));
                    }
                    None
                }
                Err(error) if anilist::media_not_found(&error) => {
                    let intent = write_origin_guard(state, origin, &expected_token)?;
                    crate::sync_queue::mark_media_missing_from(
                        state, origin, intent, media_id, true,
                    )
                    .map_err(|e| e.to_string())?;
                    return Err(MEDIA_GONE.into());
                }
                Err(error) if anilist::is_retryable(&error) => Some(error.to_string()),
                Err(error) => return Err(write_err(state, origin, &expected_token, &error)),
            }
        }
        _ => None,
    };
    let w = compute_set_progress(state, media_id, progress)?;
    crate::sync_queue::validate_progress_edit(state, media_id, Some(progress), Some(w.progress))
        .map_err(|e| e.to_string())?;
    let automatic_completion = (w.send_status && w.status == "COMPLETED")
        .then(|| crate::sync_queue::AutomaticCompletion::new(None));
    let patch = crate::sync_queue::Patch {
        status: w.send_status.then_some(w.status),
        progress: Some(w.progress),
        repeat: w.send_repeat.then_some(w.repeat),
        automatic_completion,
        ..Default::default()
    };
    validate_entry_values(patch.progress, patch.score, patch.repeat)?;
    let entry = if let Some(error) = lookup_error {
        let intent = write_origin_guard(state, origin, &expected_token)?;
        let (account, mut pending, _) =
            crate::sync_queue::prepare_entry(state, media_id, patch, None, Some(&entry))
                .map_err(|e| e.to_string())?
                .ok_or(AUTH_REPLACED)?;
        pending.error = Some(error);
        crate::sync_queue::commit_pending_projection(
            state,
            origin,
            intent,
            account,
            &pending,
            Some(entry),
        )?
    } else {
        save_entry_unlocked(state, origin, media_id, patch, EntrySave::Manual, write).await?
    };
    state.clear_tracking_prompt_if_advanced(origin, &entry);
    Ok((ControlFlow::Continue(entry), origin))
}

pub(crate) async fn confirm_tracking_prompt(
    event: crate::Event,
    displayed_target: i64,
    state: &AppState,
) -> Result<(), String> {
    let prompt = match &event.kind {
        crate::EventKind::TrackingPrompt(prompt) | crate::EventKind::TrackingAsk(prompt) => {
            prompt.clone()
        }
        _ => return Err("Playback prompt is no longer current.".into()),
    };
    if !state.event_is_current(&event) {
        return Err("Playback prompt is no longer current.".into());
    }
    let origin = crate::EventOrigin {
        generation: event.generation,
        account_id: event.account_id,
    };
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    if !state.event_is_current(&event) {
        return Err("Playback prompt is no longer current.".into());
    }
    let (offered_identity, watched) = {
        let retained = state.last_tracking_prompt.lock();
        let Some(retained) = retained
            .as_ref()
            .filter(|retained| same_tracking_prompt(&retained.event, &event))
        else {
            return Err("Playback prompt is no longer current.".into());
        };
        (retained.entry_identity.clone(), retained.watched.clone())
    };
    let entry = crate::sync_queue::current_entry(state, prompt.media_id)
        .map_err(|e| e.to_string())?
        .filter(|entry| entry.progress < prompt.episode && prompt.progress < prompt.episode);
    let Some(entry) = entry else {
        state.take_tracking_prompt(&event);
        return Ok(());
    };
    if offered_identity != Some(entry.identity()) {
        state.take_tracking_prompt(&event);
        return Err(PROMPT_CHANGED.into());
    }
    let total = state
        .db
        .episode_total(prompt.media_id)
        .map_err(|e| e.to_string())?;
    let w = progress_for_total(Some(&entry), total, prompt.episode);
    if w.progress != displayed_target {
        return Err(PROMPT_CHANGED.into());
    }
    if w.progress <= entry.progress {
        state.take_tracking_prompt(&event);
        return Ok(());
    }
    crate::sync_queue::validate_progress_edit(
        state,
        prompt.media_id,
        Some(prompt.episode),
        Some(w.progress),
    )
    .map_err(|e| e.to_string())?;
    let saved = save_entry_unlocked(
        state,
        origin,
        prompt.media_id,
        crate::sync_queue::Patch {
            status: w.send_status.then_some(w.status),
            progress: Some(w.progress),
            repeat: w.send_repeat.then_some(w.repeat),
            ..Default::default()
        },
        EntrySave::Prompt {
            event: &event,
            episode: prompt.episode,
            total,
        },
        _write,
    )
    .await?;
    if let Some(watched) = watched.filter(|_| saved.progress >= prompt.episode) {
        let _write = state.entry_lock.lock().await;
        if let Some(watched) = watched.lock().as_ref() {
            watched.record(state, origin);
        }
    }
    state.take_tracking_prompt(&event);
    crate::sync_queue::publish_from(state, origin, &saved);
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub async fn watcher_set_progress(
    state: &AppState,
    media_id: i64,
    episode: i64,
    expected_token: Option<&str>,
) -> Result<Option<ListEntry>, String> {
    watcher_set_progress_from(state, state.origin(), media_id, episode, expected_token).await
}

pub(crate) async fn watcher_set_progress_from(
    state: &AppState,
    origin: crate::EventOrigin,
    media_id: i64,
    episode: i64,
    expected_token: Option<&str>,
) -> Result<Option<ListEntry>, String> {
    let _remote = state.remote_lock.lock().await;
    if !state.origin_is_current(origin) {
        return Ok(None);
    }
    let _write = state.entry_lock.lock().await;
    if expected_token.is_none()
        || state.anilist.lock().token().as_deref() != expected_token
        || !state.origin_is_current(origin)
    {
        return Ok(None);
    }
    let Some(cur) = crate::sync_queue::current_entry(state, media_id).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if episode <= cur.progress {
        return Ok(None);
    }
    if state
        .db
        .episode_total(media_id)
        .map_err(|e| e.to_string())?
        .is_some_and(|total| episode > total)
    {
        return Err(EPISODE_EXCEEDS_TOTAL.into());
    }
    let w = compute_set_progress(state, media_id, episode)?;
    let entry = save_entry_unlocked(
        state,
        origin,
        media_id,
        crate::sync_queue::Patch {
            status: w.send_status.then_some(w.status),
            progress: Some(w.progress),
            repeat: w.send_repeat.then_some(w.repeat),
            ..Default::default()
        },
        EntrySave::AutomaticPlayback(episode),
        _write,
    )
    .await?;
    state.clear_tracking_prompt_if_advanced(origin, &entry);
    Ok(Some(entry))
}

pub(crate) fn playback_fields(
    base: &ListEntry,
    total: Option<i64>,
    episode: i64,
) -> (Option<String>, Option<i64>) {
    let revised = progress_for_total(Some(base), total, episode);
    (
        revised.send_status.then_some(revised.status),
        revised.send_repeat.then_some(revised.repeat),
    )
}

pub(crate) const EPISODE_EXCEEDS_TOTAL: &str =
    "Detected episode exceeds this show’s total. Adjust its episode offset in Library.";

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
    let cur = crate::sync_queue::current_entry(state, media_id).map_err(|e| e.to_string())?;
    let total = state
        .db
        .episode_total(media_id)
        .map_err(|e| e.to_string())?;
    Ok(progress_for_total(cur.as_ref(), total, progress))
}

fn progress_for_total(cur: Option<&ListEntry>, total: Option<i64>, progress: i64) -> ProgressWrite {
    let requested = progress.max(0);
    let overshot = total.is_some_and(|t| requested > t);
    let mut progress = requested;
    if let Some(t) = total {
        progress = progress.min(t);
    }
    let prev_status = cur.map(|e| e.status.as_str()).unwrap_or("");
    let prev_progress = cur.map(|e| e.progress).unwrap_or(0);
    let prev_repeat = cur.map(|e| e.repeat).unwrap_or(0);
    let at_end = at_last_episode(total, progress);
    let (status, repeat) = if overshot {
        (prev_status, prev_repeat)
    } else if at_end && prev_status == "REPEATING" && prev_progress < progress {
        (
            ListStatus::Completed.as_str(),
            prev_repeat.saturating_add(1).min(1000),
        )
    } else if at_end {
        (ListStatus::Completed.as_str(), prev_repeat)
    } else if prev_status.is_empty()
        || reopens_completed(cur, total, progress)
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
    ProgressWrite {
        status: status.to_string(),
        progress,
        repeat,
        send_status: status != prev_status,
        send_repeat: repeat != prev_repeat,
    }
}

pub async fn delete_entry_cmd(media_id: i64, state: &AppState) -> Result<(), String> {
    delete_entry_inner(media_id, state).await
}

impl crate::Core {
    pub async fn delete_editor_entry(
        &self,
        media_id: i64,
        target: crate::EntryEditTarget,
    ) -> Result<(), String> {
        delete_entry_target(media_id, Some(target), &self.state).await
    }
}

async fn delete_entry_inner(media_id: i64, state: &AppState) -> Result<(), String> {
    delete_entry_target(media_id, None, state).await
}

async fn delete_entry_target(
    media_id: i64,
    target: Option<crate::EntryEditTarget>,
    state: &AppState,
) -> Result<(), String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _write = state.entry_lock.lock().await;
    require_write_session(state, &expected_token)?;
    if !state.origin_is_current(origin) {
        return Err("Account changed".into());
    }
    drop(_write);
    delete_entry_with_target(media_id, true, state, origin, target)
        .await
        .map_err(String::from)
}

/// Caller holds remote_lock through deletion and durable acknowledgement.
pub(crate) async fn delete_entry_unlocked(
    media_id: i64,
    explicit: bool,
    state: &AppState,
    origin: crate::EventOrigin,
) -> Result<(), crate::sync_queue::EntryFlushError> {
    delete_entry_with_target(media_id, explicit, state, origin, None).await
}

async fn delete_entry_with_target(
    media_id: i64,
    explicit: bool,
    state: &AppState,
    origin: crate::EventOrigin,
    target: Option<crate::EntryEditTarget>,
) -> Result<(), crate::sync_queue::EntryFlushError> {
    use crate::sync_queue::EntryFlushError;

    let mut write_guard = state.entry_lock.lock().await;
    if !state.origin_is_current(origin) {
        return Err(EntryFlushError::Batch("Account changed".into()));
    }
    if !state.anilist.lock().has_token() {
        return Err(EntryFlushError::Batch("not authenticated".into()));
    }
    let owner = entry_account(state, origin)?;
    check_editor_account(target.as_ref(), owner)?;
    let entry = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
    let account = origin.account_id;
    let queued = account
        .map(|account| state.db.pending_change(account, media_id))
        .transpose()
        .map_err(|e| e.to_string())?
        .flatten();
    let editor_id = match target.as_ref().map(|target| &target.identity) {
        Some(crate::EntryIdentity::Unverified) => {
            return Err(EDITOR_ENTRY_UNVERIFIED.to_string().into())
        }
        Some(identity @ crate::EntryIdentity::Remote(id))
            if entry.as_ref().is_none_or(|entry| identity.matches(entry)) =>
        {
            Some(*id)
        }
        Some(identity @ crate::EntryIdentity::Queued(key)) => match entry.as_ref() {
            Some(entry) if identity.matches(entry) && entry.id.is_some() => entry.id,
            Some(entry)
                if identity.matches(entry)
                    && queued.as_ref().is_some_and(|pending| {
                        pending.base.is_none()
                            && pending.deletion_id.is_none()
                            && pending.patch.creation_key.as_ref() == Some(key)
                    }) =>
            {
                None
            }
            _ => return Err(EDITOR_ENTRY_CHANGED.to_string().into()),
        },
        Some(_) => return Err(EDITOR_ENTRY_CHANGED.to_string().into()),
        None => None,
    };
    if let (Some(account), Some(queued)) = (account, &queued) {
        if (!explicit || target.is_some())
            && !crate::sync_queue::overlay_allowed(state, account, queued, entry.as_ref())
                .map_err(|e| e.to_string())?
        {
            return if explicit {
                Err(EDITOR_ENTRY_CHANGED.to_string().into())
            } else {
                Ok(())
            };
        }
    }
    let mut deletion = queued.clone().unwrap_or(crate::sync_queue::Pending {
        media_id,
        deleting: false,
        deletion_id: None,
        base: entry.clone(),
        patch: Default::default(),
        error: None,
        conflict: false,
        missing_media: false,
        score_format: None,
        playback: false,
        base_score_format: None,
        rejected_progress: None,
        retry_error: None,
        rejected_total: None,
    });
    deletion.deleting = true;
    deletion.conflict = false;
    deletion.error = None;
    deletion.retry_error = None;
    if target.is_some() {
        deletion.deletion_id = editor_id;
    } else if explicit {
        deletion.deletion_id = entry
            .as_ref()
            .and_then(|entry| entry.id)
            .or(deletion.deletion_id);
    }
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    let token = al.token();
    if explicit {
        drop(write_guard);
        crate::sync_queue::confirm_write_account(state, origin, &al, owner).await?;
        write_guard = state.entry_lock.lock().await;
        check_write_origin(state, origin, &token)?;
    }
    let restore_previous = |account| match &queued {
        Some(previous) => state.db.put_pending(account, previous),
        None => state.db.remove_pending(account, media_id),
    };
    let mut staged = false;
    let mut staged_absence = false;
    let mut confirmed_removed = false;
    let mut retain_failure = false;
    let mut unsent = false;
    let mut could_have_deleted = false;
    let mut record_delete = |result: anyhow::Result<bool>| {
        match &result {
            Ok(deleted) => could_have_deleted |= deleted,
            Err(error) => {
                unsent = error.is::<anilist::UnsentRequest>();
                could_have_deleted |= !unsent && anilist::is_retryable(error);
            }
        }
        result
    };
    let result = async {
        let mut write = write_guard;
        let original_target = deletion.deletion_id;
        let verify_absent_cache = target.is_some() && entry.is_none();
        let lookup = if let Some(id) = deletion.deletion_id.filter(|_| !verify_absent_cache) {
            Ok(Some(id))
        } else if explicit || entry.is_some() || queued.is_some() {
            drop(write);
            let result = al.entry_by_media_id(media_id, None).await;
            write = state.entry_lock.lock().await;
            check_write_origin(state, origin, &token).map_err(anyhow::Error::msg)?;
            match result {
                Ok(Some(remote)) if target.is_some() && editor_id != Some(remote.id) => {
                    anyhow::bail!(EDITOR_ENTRY_CHANGED);
                }
                Ok(Some(remote)) if explicit => {
                    deletion.deletion_id = Some(remote.id);
                    Ok(Some(remote.id))
                }
                Ok(Some(_)) => Err(anyhow::anyhow!("AniList entry identity changed or was not saved before removal. Choose which version to keep.")),
                Ok(None) => {
                    confirmed_removed = true;
                    Ok(None)
                }
                Err(e) if anilist::media_not_found(&e) => {
                    confirmed_removed = true;
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        } else {
            Ok(None)
        };
        let id = lookup.inspect_err(|_| {
            retain_failure = true;
        })?;
        let Some(id) = id else {
            if let Some(account) = account.filter(|_| {
                queued.as_ref().is_some_and(|pending| {
                    !pending.deleting && !pending.needs_cleanup_only()
                })
            }) {
                let _intent =
                    write_origin_guard(state, origin, &token).map_err(anyhow::Error::msg)?;
                state.db.put_pending(account, &deletion)?;
                staged_absence = true;
            }
            return Ok(());
        };
        let intent = write_origin_guard(state, origin, &token).map_err(anyhow::Error::msg)?;
        if let Some(account) = account {
            // Supersede old add intent before a request can have a remote effect.
            if let Err(error) = state.db.put_pending(account, &deletion) {
                if deletion.deletion_id != original_target {
                    deletion.deletion_id = original_target;
                    retain_failure = true;
                }
                return Err(error);
            }
            staged = true;
        }
        drop(intent);
        if staged {
            state.emit_from(origin, crate::EventKind::PendingChanged(Some(vec![media_id])));
        }
        drop(write);
        if record_delete(al.delete_entry(id).await)? {
            confirmed_removed = true;
        } else {
            let result = al.entry_by_media_id(media_id, None).await;
            match result {
                Ok(Some(live)) => {
                    let _write = state.entry_lock.lock().await;
                    let intent =
                        write_origin_guard(state, origin, &token).map_err(anyhow::Error::msg)?;
                    if (!explicit || target.is_some()) && deletion.deletion_id != Some(live.id) {
                        anyhow::bail!("AniList entry identity changed during removal. Choose which version to keep.");
                    }
                    let previous_target = deletion.deletion_id.replace(live.id);
                    if let Some(account) = account {
                        if let Err(error) = state.db.put_pending(account, &deletion) {
                            deletion.deletion_id = previous_target;
                            return Err(error);
                        }
                    }
                    drop(intent);
                    drop(_write);
                    if !record_delete(al.delete_entry(live.id).await)? {
                        anyhow::bail!("AniList did not confirm removal. Try again.");
                    }
                    confirmed_removed = true;
                }
                Ok(None) => {
                    confirmed_removed = true;
                }
                Err(e) if anilist::media_not_found(&e) => {
                    confirmed_removed = true;
                }
                Err(e) => return Err(e),
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _write = state.entry_lock.lock().await;
    if let Err(error) = result {
        let auth_rejected = anilist::is_auth_rejection(&error);
        let mut settle = |replaced: bool| {
            let Some(account) = account.filter(|_| staged || retain_failure && !replaced) else {
                return Ok(false);
            };
            if auth_rejected && !replaced && !could_have_deleted {
                if let Some(previous) = queued.as_ref().filter(|pending| pending.deleting) {
                    let mut retained = previous.clone();
                    retained.record_retry_error(SESSION_EXPIRED);
                    state.db.put_pending(account, &retained)?;
                    return Ok(true);
                }
                if staged {
                    restore_previous(account).or_else(|restore_error| {
                        deletion.conflict = true;
                        deletion.error = Some(SESSION_EXPIRED.into());
                        state.db.put_pending(account, &deletion)?;
                        Err(restore_error)
                    })?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if staged && (unsent || replaced && !could_have_deleted) {
                restore_previous(account).or_else(|restore_error| {
                    deletion.conflict = true;
                    deletion.error = Some(error.to_string());
                    state.db.put_pending(account, &deletion)?;
                    Err(restore_error)
                })?;
            } else {
                deletion.conflict = true;
                deletion.error = Some(format!(
                    "Removal is pending. Send saved change retries removal. Keep AniList version cancels it. {error}"
                ));
                state.db.put_pending_with_retry_position(
                    account,
                    &deletion,
                    !explicit && !replaced && anilist::is_entry_retry_error(&error),
                )?;
            }
            Ok(true)
        };
        let (settlement, failure) = if auth_rejected {
            match clear_rejected_session_after(state, origin, &token, || settle(false)) {
                Some((result, failure)) => (result, failure.message().to_string().into()),
                None => (settle(true), EntryFlushError::Batch(AUTH_REPLACED.into())),
            }
        } else {
            let intent = write_origin_guard(state, origin, &token);
            let result = settle(intent.is_err());
            let failure = match intent {
                Ok(_) => EntryFlushError::from_remote(&error, error.to_string()),
                Err(error) => error.into(),
            };
            (result, failure)
        };
        if settlement.map_err(|e: anyhow::Error| e.to_string())? {
            state.emit_from(
                origin,
                crate::EventKind::PendingChanged(Some(vec![media_id])),
            );
        }
        return Err(failure);
    }
    let origin_result = write_origin_guard(state, origin, &token);
    if let Err(origin_error) = &origin_result {
        if !confirmed_removed {
            return Err(origin_error.clone().into());
        }
        // Settle a confirmed removal before a waiting auth change takes remote_lock.
        if !write_session_account_matches(state, origin, &token) {
            if let Some(account) = account {
                state
                    .db
                    .remove_pending(account, media_id)
                    .map_err(|e| e.to_string())?;
            }
            return Err(origin_error.clone().into());
        }
    }
    if let Err(error) = state.db.durable_write(|tx| {
        Db::delete_entry_from(tx, media_id)?;
        if let Some(account) = account {
            Db::delete_pending_from(tx, account, media_id)?;
        }
        Ok(())
    }) {
        if staged_absence {
            if let Ok(intent) = origin_result {
                drop(intent);
                state.emit_from(
                    origin,
                    crate::EventKind::PendingChanged(Some(vec![media_id])),
                );
            }
        }
        return Err(error.to_string().into());
    }
    match origin_result {
        Ok(intent) => {
            drop(intent);
            state.clear_tracking_prompt_for_media(origin, media_id);
            state.refresh_matchers();
            state.emit_from(
                origin,
                crate::EventKind::PendingChanged(Some(vec![media_id])),
            );
            Ok(())
        }
        Err(origin_error) => {
            state.refresh_matchers();
            Err(origin_error.into())
        }
    }
}

pub async fn get_library_folders(state: &AppState) -> Result<Vec<String>, String> {
    get_library_folders_inner(state).await
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

async fn library_write<T: Send + 'static>(
    state: &AppState,
    work: impl FnOnce(&Db, Option<Arc<Vec<recognize::Matcher>>>) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    let intent = *state.auth_intent.borrow();
    let expected_token = state.anilist.lock().token();
    let entry = state.entry_lock.clone().lock_owned().await;
    state.check_auth_intent(intent)?;
    check_write_session(state, &expected_token)?;
    let matchers = expected_token.map(|_| state.matchers.lock().clone());
    let db = state.db.clone();
    persist(entry, None, move || {
        work(&db, matchers).map_err(|e| e.to_string())
    })
    .await
}

async fn get_library_folders_inner(state: &AppState) -> Result<Vec<String>, String> {
    library_db_work(state.db.clone(), library::get_folders).await
}

pub async fn add_library_folder(path: String, state: &AppState) -> Result<Vec<String>, String> {
    add_library_folder_inner(path, state).await
}

async fn add_library_folder_inner(path: String, state: &AppState) -> Result<Vec<String>, String> {
    library_write(state, move |db, _| library::add_folder(db, &path)).await
}

pub async fn remove_library_folder(path: String, state: &AppState) -> Result<Vec<String>, String> {
    remove_library_folder_inner(path, state).await
}

async fn remove_library_folder_inner(
    path: String,
    state: &AppState,
) -> Result<Vec<String>, String> {
    library_write(state, move |db, _| library::remove_folder(db, &path)).await
}

pub async fn scan_library(state: &AppState) -> Result<LibraryScan, String> {
    let intent = *state.auth_intent.borrow();
    let expected_token = state.anilist.lock().token();
    let (folders, bindings, account, matchers) = {
        let _entry = state.entry_lock.lock().await;
        state.check_auth_intent(intent)?;
        check_write_session(state, &expected_token)?;
        let (folders, bindings, account) = library_db_work(state.db.clone(), |db| {
            Ok((
                library::get_folders(db)?,
                library::get_bindings(db)?,
                crate::sync_queue::account_id(db),
            ))
        })
        .await?;
        (folders, bindings, account, state.matchers.lock().clone())
    };
    let cache = state.library_cache.clone();
    let db = state.db.clone();
    let (scan, current_account) = tokio::task::spawn_blocking(move || {
        let scan = cache.lock().scan(&folders, &matchers, &bindings);
        (scan, crate::sync_queue::account_id(&db))
    })
    .await
    .map_err(|e| e.to_string())?;
    state.check_auth_intent(intent)?;
    check_write_session(state, &expected_token)?;
    if current_account != account {
        return Err("Your AniList account changed. Try again.".into());
    }
    Ok(scan)
}

pub async fn bind_library_path(
    path: String,
    media_id: i64,
    episode_offset: Option<i64>,
    scope: library::LibraryBindingScope,
    state: &AppState,
) -> Result<(), String> {
    library_write(state, move |db, matchers| {
        let Some(matchers) = matchers else {
            return Err(anyhow::anyhow!("only shows on your list can be linked"));
        };
        if db.get_entry(media_id)?.is_none() {
            return Err(anyhow::anyhow!("only shows on your list can be linked"));
        }
        if !matchers.iter().any(|matcher| matcher.media_id == media_id) {
            return Err(anyhow::anyhow!(
                "Show details are unavailable. Refresh your list and try again."
            ));
        }
        library::bind_scoped_path(db, &path, scope, media_id, episode_offset.unwrap_or(0))
    })
    .await
}

pub async fn library_binding_for(path: String, state: &AppState) -> Result<Option<i64>, String> {
    library_binding_for_inner(path, state).await
}

async fn library_binding_for_inner(path: String, state: &AppState) -> Result<Option<i64>, String> {
    library_db_work(state.db.clone(), move |db| {
        library::binding_for_exact(db, &path).map(|binding| binding.map(|b| b.media_id))
    })
    .await
}

pub async fn library_binding_details(
    path: String,
    state: &AppState,
) -> Result<Option<library::LibraryBinding>, String> {
    library_binding_details_inner(path, state).await
}

async fn library_binding_details_inner(
    path: String,
    state: &AppState,
) -> Result<Option<library::LibraryBinding>, String> {
    let expected_token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    if expected_token.is_none() {
        return Ok(None);
    }
    let matchers = state.matchers.lock().clone();
    library_db_work(state.db.clone(), move |db| {
        library::effective_binding(db, &path, &matchers)
    })
    .await
}

pub async fn get_watch_history(
    limit: Option<i64>,
    before: Option<i64>,
    state: &AppState,
) -> Result<Vec<crate::sync_queue::WatchHistoryItem>, String> {
    get_watch_history_inner(limit, before, state).await
}

async fn get_watch_history_inner(
    limit: Option<i64>,
    before: Option<i64>,
    state: &AppState,
) -> Result<Vec<crate::sync_queue::WatchHistoryItem>, String> {
    let expected_token = state.anilist.lock().token();
    let _write = state.entry_lock.lock().await;
    check_write_session(state, &expected_token)?;
    if expected_token.is_none() {
        return Ok(vec![]);
    }
    library_db_work(state.db.clone(), move |db| {
        let Some(account) = crate::sync_queue::account_id(db) else {
            return Ok(vec![]);
        };
        db.watch_history(account, limit.unwrap_or(50).clamp(1, 100), before)
    })
    .await
}

pub async fn unbind_library_media(media_id: i64, state: &AppState) -> Result<(), String> {
    unbind_library_media_inner(media_id, state).await
}

async fn unbind_library_media_inner(media_id: i64, state: &AppState) -> Result<(), String> {
    library_write(state, move |db, _| library::unbind_media(db, media_id)).await
}

#[cfg(test)]
#[path = "library_command_tests.rs"]
mod library_command_tests;

pub fn get_rss_feeds(state: &AppState) -> Result<Vec<String>, String> {
    rss::get_feeds(&state.db).map_err(|e| e.to_string())
}

pub fn add_rss_feed(url: String, state: &AppState) -> Result<Vec<String>, String> {
    rss::add_feed(&state.db, &url).map_err(|e| e.to_string())
}

pub fn remove_rss_feed(url: String, state: &AppState) -> Result<Vec<String>, String> {
    rss::remove_feed(&state.db, &url).map_err(|e| e.to_string())
}

pub async fn fetch_torrents(state: &AppState) -> Result<TorrentFetch, String> {
    let read = rss::read_feeds(&state.db).map_err(|e| e.to_string())?;
    if read.feeds.is_empty() {
        return Ok(TorrentFetch::default());
    }
    let fetched = rss::fetch_all(&read.feeds)
        .await
        .map_err(|e| e.to_string())?;
    let failures = fetched.failures;
    let raw = fetched.items;
    let (seen, matchers, progress_by_id, total_by_id) = {
        let _guard = state.entry_lock.lock().await;
        let seen = state.db.rss_seen_set().map_err(|e| e.to_string())?;
        let matchers = if state.anilist.lock().has_token() {
            state.matchers.lock().clone()
        } else {
            Arc::new(Vec::new())
        };
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
        (seen, matchers, progress_by_id, total_by_id)
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
    // A partial feed snapshot cannot establish that an old release is gone.
    if failures.is_empty() {
        let carried: Vec<_> = items
            .iter()
            .flat_map(|item| std::iter::once(item.guid.clone()).chain(item.seen_guid.clone()))
            .collect();
        let _ = read.prune_seen(&carried);
    }
    Ok(TorrentFetch { items, failures })
}

#[cfg(test)]
#[tokio::test]
async fn partial_rss_refresh_preserves_seen_marks_until_feeds_recover() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let state = tests::test_state();
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let good_feed = format!("http://{address}/good");
    let recovering_feed = format!("http://{address}/recovering");
    state
        .db
        .set_setting(
            "rss_feeds",
            &serde_json::json!([good_feed, recovering_feed]).to_string(),
        )
        .unwrap();
    let carried = format!("{recovering_feed}\u{1}episode");
    state
        .db
        .0
        .lock()
        .execute(
            "INSERT INTO rss_seen VALUES (?,0), ('expired-absent',0)",
            [&carried],
        )
        .unwrap();
    let responses = tokio::spawn(async move {
        let mut recovered = false;
        for _ in 0..4 {
            let (stream, _) = server.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let recovering = line.starts_with("GET /recovering ");
            loop {
                line.clear();
                if stream.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let (status, body) = if !recovering {
                ("200 OK", "<rss><channel></channel></rss>")
            } else if recovered {
                (
                    "200 OK",
                    "<rss><channel><item><title>Show - 01</title>\
                     <link>https://example.invalid/episode</link>\
                     <guid>episode</guid></item></channel></rss>",
                )
            } else {
                recovered = true;
                ("503 Service Unavailable", "")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
    });

    let partial = fetch_torrents(&state).await.unwrap();
    assert_eq!(partial.failures.len(), 1);
    assert_eq!(partial.failures[0].url, recovering_feed);
    assert!(state.db.rss_seen_set().unwrap().contains(&carried));
    let complete = fetch_torrents(&state).await.unwrap();
    assert!(complete.failures.is_empty());
    assert_eq!(complete.items.len(), 1);
    assert!(complete.items[0].seen);
    let seen = state.db.rss_seen_set().unwrap();
    assert!(seen.contains(&carried));
    assert!(!seen.contains("expired-absent"));
    responses.await.unwrap();
}

#[cfg(test)]
#[tokio::test]
async fn rss_refresh_does_not_prune_seen_marks_after_a_feed_is_added() {
    assert_rss_refresh_keeps_seen_marks_after_feed_edits(false).await;
}

#[cfg(test)]
#[tokio::test]
async fn rss_refresh_does_not_prune_seen_marks_after_feed_configuration_is_restored() {
    assert_rss_refresh_keeps_seen_marks_after_feed_edits(true).await;
}

#[cfg(test)]
async fn assert_rss_refresh_keeps_seen_marks_after_feed_edits(restore: bool) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let state = Arc::new(tests::test_state());
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = server.local_addr().unwrap();
    let original_feed = format!("http://{address}/original");
    let added_feed = format!("http://{address}/added");
    state
        .db
        .set_setting("rss_feeds", &serde_json::json!([original_feed]).to_string())
        .unwrap();
    let seen_feed = if restore { &original_feed } else { &added_feed };
    let seen_guid = format!("{seen_feed}\u{1}episode");
    state
        .db
        .0
        .lock()
        .execute("INSERT INTO rss_seen VALUES (?,0)", [&seen_guid])
        .unwrap();

    let worker = state.clone();
    let refresh = tokio::spawn(async move { fetch_torrents(&worker).await.unwrap() });
    let (socket, _) = server.accept().await.unwrap();
    let mut socket = BufReader::new(socket);
    let mut line = String::new();
    while socket.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
        line.clear();
    }
    if restore {
        assert!(remove_rss_feed(original_feed.clone(), &state)
            .unwrap()
            .is_empty());
        assert_eq!(
            add_rss_feed(original_feed.clone(), &state).unwrap(),
            vec![original_feed]
        );
    } else {
        assert_eq!(
            add_rss_feed(added_feed.clone(), &state).unwrap(),
            vec![original_feed, added_feed]
        );
    }
    let body = "<rss><channel/></rss>";
    socket
        .get_mut()
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    assert!(refresh.await.unwrap().failures.is_empty());
    assert!(state.db.rss_seen_set().unwrap().contains(&seen_guid));
}

pub fn mark_torrents_seen(guids: Vec<String>, state: &AppState) -> Result<(), String> {
    state.db.mark_rss_seen(&guids).map_err(|e| e.to_string())
}

pub async fn search_torrents(
    query: String,
    category: Option<String>,
    filter: Option<String>,
    state: &AppState,
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

pub async fn find_show_torrents(
    media_id: i64,
    category: Option<String>,
    filter: Option<String>,
    state: &AppState,
) -> Result<ShowTorrents, String> {
    let (entry, matchers) = {
        let _guard = state.entry_lock.lock().await;
        if !state.anilist.lock().has_token() {
            return Err("This show is no longer on your list".to_string());
        }
        let entry = state
            .db
            .get_entry(media_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "This show is no longer on your list".to_string())?;
        (entry, state.matchers.lock().clone())
    };
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
        return Err("not authenticated".into());
    }
    state
        .user
        .lock()
        .clone()
        .filter(|user| anilist::valid_account_id(user.id))
        .or_else(|| state.db.cached_user().ok().flatten())
        .ok_or_else(|| "Reconnect to AniList to load your account profile".into())
}

pub async fn get_user_stats(state: &AppState) -> Result<UserStats, String> {
    get_user_stats_inner(state).await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedUserStats {
    #[serde(default)]
    owner_id: Option<i64>,
    #[serde(flatten)]
    stats: UserStats,
}

async fn get_user_stats_inner(state: &AppState) -> Result<UserStats, String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    let user = cached_feature_user(state)?;
    check_read_account(state, user.id, &expected_token, origin.generation)?;
    let key = format!("user_stats:{}", user.id);
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    drop(_guard);
    let result = al.user_statistics(user.id).await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_read_account(state, user.id, &expected_token, origin.generation)?;
    match result {
        Ok(mut stats) => {
            stats.cached_at = Some(chrono::Utc::now().timestamp());
            if let Ok(value) = serde_json::to_string(&CachedUserStats {
                owner_id: Some(user.id),
                stats: stats.clone(),
            }) {
                let _ = state.db.set_setting(&key, &value);
            }
            Ok(stats)
        }
        Err(e) if anilist::is_auth_rejection(&e) => {
            Err(write_err(state, origin, &expected_token, &e))
        }
        Err(e) => {
            let cached = state
                .db
                .get_setting(&key)
                .ok()
                .flatten()
                .and_then(|v| serde_json::from_str::<CachedUserStats>(&v).ok())
                .filter(|cached| cached.owner_id.is_none_or(|id| id == user.id));
            match cached {
                Some(mut cached) => {
                    cached.stats.warning = Some(if cached.owner_id.is_none() {
                        format!(
                            "{e}. Cached statistics ownership has not been verified. Refresh when AniList is available."
                        )
                    } else {
                        e.to_string()
                    });
                    Ok(cached.stats)
                }
                None => Err(e.to_string()),
            }
        }
    }
}

pub async fn get_notifications_page(
    page: i64,
    state: &AppState,
) -> Result<NotificationPage, String> {
    get_notifications_page_inner(page, state).await
}

async fn notification_client(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
) -> Result<AniList, String> {
    let entry = state.entry_lock.lock().await;
    check_write_origin(state, origin, token)?;
    let account = entry_account(state, origin)?;
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    drop(entry);
    if al.verified_account() != Some(account) {
        let _remote = state.remote_lock.lock().await;
        crate::sync_queue::confirm_write_account(state, origin, &al, account).await?;
    }
    Ok(al)
}

async fn get_notifications_page_inner(
    page: i64,
    state: &AppState,
) -> Result<NotificationPage, String> {
    if !(1..=10000).contains(&page) {
        return Err("invalid page".into());
    }
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let al = notification_client(state, origin, &expected_token).await?;
    let result = al.notifications_page(page).await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    result.map_err(|e| write_err(state, origin, &expected_token, &e))
}

pub async fn mark_notifications_read(state: &AppState) -> Result<(), String> {
    mark_notifications_read_inner(state).await
}

async fn mark_notifications_read_inner(state: &AppState) -> Result<(), String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let al = notification_client(state, origin, &expected_token).await?;
    let result = al.mark_notifications_read().await;
    let _remote = if result.as_ref().is_err_and(anilist::is_auth_rejection) {
        Some(state.remote_lock.lock().await)
    } else {
        None
    };
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    result.map_err(|e| write_err(state, origin, &expected_token, &e))
}

pub async fn get_entry_details(media_id: i64, state: &AppState) -> Result<EntryDetails, String> {
    get_entry_details_inner(media_id, state).await
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedEntryDetails {
    entry_id: i64,
    #[serde(flatten)]
    details: EntryDetails,
}

const ENTRY_DETAILS_MISSING: &str = "This show has no saved AniList entry yet";

fn read_cached_entry_details(db: &Db, key: &str) -> Option<CachedEntryDetails> {
    serde_json::from_str(&db.get_setting(key).ok().flatten()?).ok()
}

fn write_entry_details_cache(db: &Db, key: &str, value: &str) -> anyhow::Result<()> {
    db.durable_write(|tx| {
        let written = tx.execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
            [key, value],
        )?;
        anyhow::ensure!(written == 1, "entry details cache was not written");
        Ok(())
    })
}

fn invalidate_entry_details(db: &Db, key: &str) -> Result<(), String> {
    db.durable_write(|tx| Db::delete_setting_from(tx, key))
        .or_else(|_| write_entry_details_cache(db, key, "null"))
        .map_err(|e| format!("Could not invalidate saved entry details: {e}"))
}

fn cache_entry_details(
    db: &Db,
    account: i64,
    media_id: i64,
    entry_id: i64,
    details: &EntryDetails,
) {
    if let Ok(value) = serde_json::to_string(&CachedEntryDetails {
        entry_id,
        details: details.clone(),
    }) {
        let key = format!("entry_details:{account}:{media_id}");
        if let Err(error) = write_entry_details_cache(db, &key, &value) {
            log::warn!("Could not cache entry details: {error}");
            if read_cached_entry_details(db, &key).is_some_and(|cached| cached.entry_id != entry_id)
            {
                if let Err(error) = invalidate_entry_details(db, &key) {
                    log::warn!("{error}");
                }
            }
        }
    }
}

async fn get_entry_details_inner(media_id: i64, state: &AppState) -> Result<EntryDetails, String> {
    get_entry_details_target(media_id, None, state).await
}

impl crate::Core {
    pub async fn get_editor_entry_details(
        &self,
        media_id: i64,
        target: crate::EntryEditTarget,
    ) -> Result<EntryDetails, String> {
        get_entry_details_target(media_id, Some(target), &self.state).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_editor_entry_details(
        &self,
        media_id: i64,
        target: crate::EntryEditTarget,
        notes: Option<String>,
        started_at: Option<FuzzyDate>,
        completed_at: Option<FuzzyDate>,
        custom_lists: Option<Vec<String>>,
    ) -> Result<EntryDetails, String> {
        update_entry_details_target(
            media_id,
            notes,
            started_at,
            completed_at,
            custom_lists,
            Some(target),
            &self.state,
        )
        .await
    }
}

fn check_editor_details_target(
    state: &AppState,
    media_id: i64,
    account: i64,
    target: Option<&crate::EntryEditTarget>,
) -> Result<Option<i64>, String> {
    check_editor_account(target, account)?;
    if let Some(target) = target {
        let entry = state.db.get_entry(media_id).map_err(|e| e.to_string())?;
        match (&target.identity, entry.as_ref()) {
            (crate::EntryIdentity::Remote(id), entry)
                if entry.is_none_or(|entry| target.identity.matches(entry)) =>
            {
                return Ok(Some(*id))
            }
            (crate::EntryIdentity::Queued(_), Some(entry)) if target.identity.matches(entry) => {
                return entry
                    .id
                    .map(Some)
                    .ok_or_else(|| EDITOR_ENTRY_UNVERIFIED.into());
            }
            (crate::EntryIdentity::Unverified, _) => return Err(EDITOR_ENTRY_UNVERIFIED.into()),
            _ => return Err(EDITOR_ENTRY_CHANGED.into()),
        }
    }
    Ok(None)
}

async fn get_entry_details_target(
    media_id: i64,
    target: Option<crate::EntryEditTarget>,
    state: &AppState,
) -> Result<EntryDetails, String> {
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    let user = cached_feature_user(state)?;
    let editor_id = check_editor_details_target(state, media_id, user.id, target.as_ref())?;
    let key = format!("entry_details:{}:{media_id}", user.id);
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    drop(_guard);
    let result = match crate::sync_queue::verify_write_account(state, origin, &al, user.id).await {
        Ok(()) => al.entry_details_with_id(media_id).await,
        Err(e) if anilist::is_retryable(&e) || anilist::is_auth_rejection(&e) => Err(e),
        Err(e) => return Err(e.to_string()),
    };
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    match result {
        Ok(Some((entry_id, mut details))) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            details.cached_at = Some(chrono::Utc::now().timestamp());
            cache_entry_details(&state.db, user.id, media_id, entry_id, &details);
            if editor_id.is_some_and(|id| id != entry_id) {
                return Err(EDITOR_ENTRY_CHANGED.into());
            }
            Ok(details)
        }
        Ok(None) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            invalidate_entry_details(&state.db, &key)?;
            Err(ENTRY_DETAILS_MISSING.into())
        }
        Err(e) if anilist::media_not_found(&e) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            invalidate_entry_details(&state.db, &key)?;
            Err(e.to_string())
        }
        Err(e) if anilist::is_auth_rejection(&e) => {
            Err(write_err(state, origin, &expected_token, &e))
        }
        Err(e) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            if origin.account_id != Some(user.id) {
                return Err(e.to_string());
            }
            let entry_id = state
                .db
                .get_entry(media_id)
                .ok()
                .flatten()
                .and_then(|entry| entry.id)
                .ok_or_else(|| e.to_string())?;
            if editor_id.is_some_and(|id| id != entry_id) {
                return Err(EDITOR_ENTRY_CHANGED.into());
            }
            let cached = read_cached_entry_details(&state.db, &key)
                .filter(|cached| cached.entry_id == entry_id);
            match cached {
                Some(CachedEntryDetails { mut details, .. }) => {
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

pub async fn update_entry_details(
    media_id: i64,
    notes: Option<String>,
    started_at: Option<FuzzyDate>,
    completed_at: Option<FuzzyDate>,
    custom_lists: Option<Vec<String>>,
    state: &AppState,
) -> Result<EntryDetails, String> {
    update_entry_details_inner(
        media_id,
        notes,
        started_at,
        completed_at,
        custom_lists,
        state,
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
    update_entry_details_target(
        media_id,
        notes,
        started_at,
        completed_at,
        custom_lists,
        None,
        state,
    )
    .await
}

async fn update_entry_details_target(
    media_id: i64,
    notes: Option<String>,
    started_at: Option<FuzzyDate>,
    completed_at: Option<FuzzyDate>,
    custom_lists: Option<Vec<String>>,
    target: Option<crate::EntryEditTarget>,
    state: &AppState,
) -> Result<EntryDetails, String> {
    if notes.as_ref().is_some_and(|v| v.chars().count() > 6000) {
        return Err("Notes must be 6000 characters or fewer".into());
    }
    validate_entry_date(started_at.as_ref())?;
    validate_entry_date(completed_at.as_ref())?;
    let origin = state.origin();
    let expected_token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    let user = cached_feature_user(state)?;
    let editor_id = check_editor_details_target(state, media_id, user.id, target.as_ref())?;
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    drop(_guard);
    crate::sync_queue::confirm_write_account(state, origin, &al, user.id).await?;
    let result = al.entry_details_with_id(media_id).await;
    let _guard = state.entry_lock.lock().await;
    check_write_origin(state, origin, &expected_token)?;
    let key = format!("entry_details:{}:{media_id}", user.id);
    let (entry_id, current) = match result {
        Ok(Some(details)) => details,
        Ok(None) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            invalidate_entry_details(&state.db, &key)?;
            return Err(ENTRY_DETAILS_MISSING.into());
        }
        Err(e) if anilist::media_not_found(&e) => {
            let _intent = write_origin_guard(state, origin, &expected_token)?;
            invalidate_entry_details(&state.db, &key)?;
            return Err(e.to_string());
        }
        Err(e) => return Err(write_err(state, origin, &expected_token, &e)),
    };
    if editor_id.is_some_and(|id| id != entry_id) {
        let _intent = write_origin_guard(state, origin, &expected_token)?;
        invalidate_entry_details(&state.db, &key)?;
        return Err(EDITOR_ENTRY_CHANGED.into());
    }
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
            media_id,
            entry_id,
            notes.as_deref(),
            started_at.as_ref(),
            completed_at.as_ref(),
            custom_lists.as_deref(),
        )
        .await;
    let _guard = state.entry_lock.lock().await;
    let origin_result = write_origin_guard(state, origin, &expected_token);
    let mut details = match result {
        Ok(details) => details,
        Err(e) => {
            if e.is::<anilist::UnconfirmedEntryDetailsWrite>() {
                let _intent = origin_result?;
                if state.db.get_setting(&key).is_ok_and(|value| {
                    value.is_none_or(|value| {
                        serde_json::from_str::<CachedEntryDetails>(&value)
                            .is_ok_and(|cached| cached.entry_id != entry_id)
                    })
                }) {
                    let mut current = current;
                    current.cached_at = Some(chrono::Utc::now().timestamp());
                    cache_entry_details(&state.db, user.id, media_id, entry_id, &current);
                }
                return Err(e.to_string());
            }
            if anilist::media_not_found(&e) {
                let _intent = origin_result?;
                invalidate_entry_details(&state.db, &key)?;
                return Err(e.to_string());
            }
            drop(origin_result?);
            return Err(write_err(state, origin, &expected_token, &e));
        }
    };
    details.available_custom_lists = current.available_custom_lists;
    details.cached_at = Some(chrono::Utc::now().timestamp());
    // Persist confirmed details for this account before a waiting auth change takes remote_lock.
    cache_entry_details(&state.db, user.id, media_id, entry_id, &details);
    origin_result?;
    Ok(details)
}

static INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn check_update(
    channel: crate::UpdateChannel,
) -> Result<crate::models::UpdateInfo, String> {
    let rel = crate::updater::fetch_latest_release_channel(channel).await?;
    Ok(crate::updater::update_info_channel(&rel, channel))
}

pub async fn check_bridge_update() -> Result<crate::models::UpdateInfo, String> {
    let rel = crate::updater::fetch_bridge_release().await?;
    Ok(crate::updater::bridge_update_info(&rel))
}

pub async fn install_bridge_update(data_dir: std::path::PathBuf) -> Result<String, String> {
    install_update_with(data_dir, crate::UpdateChannel::Native, true).await
}

pub async fn install_update(
    data_dir: std::path::PathBuf,
    channel: crate::UpdateChannel,
) -> Result<String, String> {
    install_update_with(data_dir, channel, false).await
}

async fn install_update_with(
    data_dir: std::path::PathBuf,
    channel: crate::UpdateChannel,
    bridge: bool,
) -> Result<String, String> {
    // Serialize installs to prevent scratch file collisions.
    let _install = INSTALL_LOCK
        .try_lock()
        .map_err(|_| "an update is already in progress".to_string())?;
    // Restart after replacing the running build, even for a same-version bridge handoff.
    if crate::updater::update_applied() {
        return Err("an update was already installed; restart Kurisu to finish".to_string());
    }
    let rel = crate::updater::fetch_latest_release_channel(channel).await?;
    let current = crate::updater::current_version();
    if crate::updater::is_newer(current, &rel.version)
        || (!bridge && !crate::updater::is_newer(&rel.version, current))
    {
        return Err("the latest release is not newer than this build".to_string());
    }
    let asset = crate::updater::platform_asset_channel(&rel, channel)
        .ok_or_else(|| "the latest release has no build for this platform".to_string())?
        .to_string();
    let url = rel
        .assets
        .get(&asset)
        .cloned()
        .ok_or_else(|| "the latest release has no build for this platform".to_string())?;

    #[cfg(any(windows, target_os = "linux"))]
    {
        let dir = data_dir;
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

        // Linux copies from the verified handle. Windows keeps it open while launching by path.
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
            // Hold the verified handle through spawn, then close it before cleanup.
            let spawned = std::process::Command::new(&tmp).spawn();
            drop(verified);
            if let Err(e) = spawned {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("could not launch the installer: {e}"));
            }
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
        let _ = (data_dir, url);
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

    pub(super) fn test_state() -> AppState {
        AppState {
            anilist: Mutex::new(AniList::new()),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).expect("in-memory db")),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn admitted_persistence_owns_guards_through_finalization() {
        use std::future::Future;
        use std::sync::mpsc;
        use std::time::Duration;
        use tokio::sync::oneshot;

        let state = test_state();
        let remote = state.remote_lock.clone().lock_owned().await;
        let entry = state.entry_lock.clone().lock_owned().await;
        let (entered, started) = oneshot::channel();
        let (write, await_write) = mpsc::channel();
        let (committed, await_commit) = oneshot::channel();
        let (finalize, await_finalize) = mpsc::channel();
        let db = state.db.clone();
        let wait = Duration::from_secs(5);
        let mut observer = Box::pin(persist(entry, Some(remote), move || {
            entered.send(()).unwrap();
            await_write.recv_timeout(wait).unwrap();
            db.set_setting("fixture_write", "committed").unwrap();
            committed.send(()).unwrap();
            await_finalize.recv_timeout(wait).unwrap();
            db.set_setting("fixture_write", "finalized").unwrap();
            Ok(())
        }));
        std::future::poll_fn(|cx| {
            assert!(observer.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::timeout(wait, started).await.unwrap().unwrap();
        drop(observer);
        assert!(state.remote_lock.try_lock().is_err());
        assert!(state.entry_lock.try_lock().is_err());
        write.send(()).unwrap();
        tokio::time::timeout(wait, await_commit)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.db.get_setting("fixture_write").unwrap().as_deref(),
            Some("committed")
        );
        assert!(state.remote_lock.try_lock().is_err());
        assert!(state.entry_lock.try_lock().is_err());
        finalize.send(()).unwrap();
        let _remote = tokio::time::timeout(wait, state.remote_lock.lock())
            .await
            .unwrap();
        let _entry = tokio::time::timeout(wait, state.entry_lock.lock())
            .await
            .unwrap();
        assert_eq!(
            state.db.get_setting("fixture_write").unwrap().as_deref(),
            Some("finalized")
        );
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
                    score_format: Some("POINT_100".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        seed(&state, Some(12), "CURRENT", 3, 0);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                episodes: Some(12),
                title_english: Some("Fixture Title".into()),
                ..Default::default()
            })
            .unwrap();
        let pending = crate::sync_queue::Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: state.db.get_entry(1).unwrap(),
            patch: crate::sync_queue::Patch {
                status: Some("REPEATING".into()),
                progress: Some(8),
                score: Some(90.0),
                repeat: Some(4),
                ..Default::default()
            },
            error: Some("Synthetic conflict".into()),
            conflict: true,
            missing_media: false,
            score_format: Some("POINT_100".into()),
            playback: false,
            base_score_format: Some("POINT_100".into()),
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
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
                    score_format: Some("POINT_100".into()),
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
                    deleting: false,
                    deletion_id: None,
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
                        deleting: false,
                        deletion_id: None,
                        base,
                        patch: crate::sync_queue::Patch {
                            progress: Some(8),
                            ..Default::default()
                        },
                        error: None,
                        conflict: false,
                        missing_media: false,
                        score_format: None,
                        playback: false,
                        base_score_format: None,
                        rejected_progress: None,
                        retry_error: None,
                        rejected_total: None,
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
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Synthetic Show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state.refresh_matchers();
        let before = state.matchers.lock().clone();
        assert_eq!(before.len(), 1);
        state
            .db
            .put_pending(
                7,
                &crate::sync_queue::Pending {
                    media_id: 1,
                    deleting: false,
                    deletion_id: None,
                    base: state.db.get_entry(1).unwrap(),
                    patch: crate::sync_queue::Patch {
                        status: Some("REPEATING".into()),
                        ..Default::default()
                    },
                    error: None,
                    conflict: false,
                    missing_media: false,
                    score_format: None,
                    playback: false,
                    base_score_format: None,
                    rejected_progress: None,
                    retry_error: None,
                    rejected_total: None,
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
        state
            .anilist
            .lock()
            .set_token(Some("fixture-session".into()));
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
        let cache = |media: &[Media]| {
            state.cache_media_batch(&state.db.catalog_read(), &mut media.to_vec())
        };
        cache(&[Media {
            id: 2,
            title_english: Some("Unlisted Show".into()),
            ..Default::default()
        }]);
        assert_eq!(recognize::matcher_work_counts(), before);
        for _ in 0..3 {
            cache(&[media.clone()]);
        }
        media.source = Some("MANGA".into());
        media.cover_large = Some("synthetic-cover".into());
        media.next_airing_episode = Some(4);
        cache(&[media.clone()]);
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
        cache(&[media.clone()]);
        assert!(Arc::ptr_eq(&original, &state.matchers.lock()));
        assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
        assert_eq!(recognize::matcher_work_counts().1, before.1);
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_cache")
            .unwrap();
        cache(&[media]);
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
        state.anilist.lock().set_token(Some("test-session".into()));
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
                queued_creation: None,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn my_list_progress_reads_remote_baseline_before_saving() {
        use serde_json::json;

        for (id, progress) in [(10, 6), (10, 7), (11, 5), (10, 5)] {
            let mut replies = vec![(
                200,
                json!({"data":{"Media":{
                    "id":1,"episodes":12,"mediaListEntry":{
                        "id":id,"mediaId":1,"status":"PAUSED","progress":progress,"repeat":1,"score":0
                    }
                }}}),
            )];
            let can_save = (id, progress) == (10, 5);
            if can_save {
                replies.push((
                    200,
                    json!({"data":{"SaveMediaListEntry":{
                        "id":id,"mediaId":1,"status":"PAUSED","progress":7,"repeat":1,"score":0
                    }}}),
                ));
            }
            let (mut api, mut requests) = anilist::mock_api(replies).await;
            api.record_viewer(7);
            let state = test_state();
            *state.anilist.lock() = api;
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            seed(&state, Some(12), "CURRENT", 5, 0);

            let (result, _) = set_progress_inner_with_origin(
                &state,
                1,
                7,
                Some(5),
                Some((Some(7), crate::EntryIdentity::Remote(10))),
            )
            .await
            .unwrap();
            let lookup = requests.try_recv().unwrap();
            assert!(lookup["query"].as_str().unwrap().contains("mediaListEntry"));
            let saved = if can_save {
                let ControlFlow::Continue(saved) = result else {
                    panic!("save was skipped");
                };
                let write = requests.try_recv().unwrap();
                assert_eq!(write["variables"]["id"], 10);
                assert_eq!(write["variables"]["progress"], 7);
                assert!(write["variables"]["status"].is_null());
                saved
            } else {
                let ControlFlow::Break(saved) = result else {
                    panic!("expected a conflict, got {result:?}");
                };
                assert_eq!(saved.progress, progress);
                saved
            };
            assert_eq!(saved.id, Some(id));
            assert_eq!(saved.status, "PAUSED");
            assert_eq!(saved.repeat, 1);
            assert_eq!(
                state.db.get_entry(1).unwrap().unwrap().progress,
                saved.progress
            );
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn my_list_progress_queues_when_lookup_is_unavailable_but_skips_removed_entries() {
        use serde_json::json;

        for (status, body) in [
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":null}}}),
            ),
        ] {
            let (mut api, mut requests) = anilist::mock_api(vec![(status, body)]).await;
            api.record_viewer(7);
            let state = test_state();
            *state.anilist.lock() = api;
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            seed(&state, Some(12), "CURRENT", 5, 0);
            let result = set_progress_inner_with_origin(
                &state,
                1,
                7,
                Some(5),
                Some((Some(7), crate::EntryIdentity::Remote(10))),
            )
            .await;
            if status == 503 {
                assert!(
                    matches!(result, Ok((ControlFlow::Continue(entry), _)) if entry.progress == 7)
                );
                let pending = state.db.pending_change(7, 1).unwrap().unwrap();
                assert_eq!(pending.base.unwrap().progress, 5);
                assert_eq!(pending.patch.progress, Some(7));
            } else {
                assert_eq!(result.unwrap_err(), "the entry is no longer on your list");
                assert!(state.db.get_entry(1).unwrap().is_none());
                assert!(state.db.pending(7).unwrap().is_empty());
            }
            let lookup = requests.try_recv().unwrap();
            assert!(lookup["query"].as_str().unwrap().contains("mediaListEntry"));
            assert!(requests.try_recv().is_err());
        }
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
                "SaveMediaListEntry":{"mediaId":1,"id":22,"status":"CURRENT","progress":4,"repeat":0, "score": 0}
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
            json!({"id":22,"progress":4,"scoreFormat":null})
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
            json!({"id":22,"progress":4,"scoreFormat":null})
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
                    json!({"data":{"Media":{"id":1,"mediaListEntry":{
                        "id":22,"mediaId":1,"status":"CURRENT","progress":4,"repeat":0,"score":0
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

    fn state_with_api(mut api: AniList) -> AppState {
        api.record_viewer(7);
        AppState {
            anilist: Mutex::new(api),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
        }
    }

    fn viewer_response(id: i64, name: &str) -> serde_json::Value {
        json!({"data": {"Viewer": {"id": id, "name": name, "avatar": null,
            "mediaListOptions": {"scoreFormat": "POINT_5"}}}})
    }

    fn seed_existing_login(state: &AppState) {
        let user = User {
            id: 7,
            name: "Existing".into(),
            ..Default::default()
        };
        state.db.replace_account("test-session", &user).unwrap();
        *state.user.lock() = Some(user);
    }

    fn assert_existing_login(state: &AppState) {
        assert_eq!(
            state.anilist.lock().token().as_deref(),
            Some("test-session")
        );
        assert_eq!(
            state.db.get_setting(TOKEN_KEY).unwrap().as_deref(),
            Some("test-session")
        );
        assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
        assert_eq!(state.user.lock().as_ref().unwrap().id, 7);
    }

    #[tokio::test]
    async fn oauth_callback_exchanges_the_code_before_saving_the_verified_account() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"access_token":"exchanged-token"})),
            (200, viewer_response(7, "Tester")),
        ])
        .await;
        let state = state_with_api(api);
        state
            .db
            .set_setting("native_preference:kurisu.list.q", "old search")
            .unwrap();
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
        let user = finish_oauth_login(
            &state,
            intent,
            receiver,
            ListPreferences::Native {
                last_account: Some(8),
            },
        )
        .await
        .unwrap();
        assert!(state
            .db
            .get_setting("native_preference:kurisu.list.q")
            .unwrap()
            .is_none());
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
        let error = finish_oauth_login(&state, intent, receiver, ListPreferences::Preserve)
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
            finish_oauth_login(&state, intent, receiver, ListPreferences::Preserve)
                .await
                .unwrap_err(),
            AUTH_REPLACED
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancelled_oauth_wait_releases_its_port_and_rejects_old_state() {
        use tokio::io::AsyncWriteExt;
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"access_token":"new-token"})),
            (200, viewer_response(8, "Replacement")),
        ])
        .await;
        let state = Arc::new(state_with_api(api));
        seed_existing_login(&state);
        let first_intent = state.begin_auth_intent();
        let (old_state, port, receiver) =
            start_oauth_callback(&state, first_intent, 0).await.unwrap();
        let mut preconnect = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        preconnect.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        let waiting = state.clone();
        let old_wait =
            tokio::spawn(
                async move { wait_for_oauth_code(&waiting, first_intent, receiver).await },
            );
        state.cancel_login();
        let new_intent = *state.auth_intent.borrow();
        let (new_state, reused_port, mut next_receiver) =
            start_oauth_callback(&state, new_intent, port)
                .await
                .unwrap();
        assert_eq!(reused_port, port);
        assert_eq!(old_wait.await.unwrap().unwrap_err(), AUTH_REPLACED);
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{port}/?code=old-code&state={old_state}"
            ))
            .send()
            .await
            .unwrap();
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("Authorization failed"));
        assert!(next_receiver.try_recv().is_err());
        assert!(requests.try_recv().is_err());
        assert_existing_login(&state);
        let accepted = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{port}/?code=new-code&state={new_state}"
            ))
            .send()
            .await
            .unwrap();
        assert!(accepted
            .text()
            .await
            .unwrap()
            .contains("Authorization received"));
        let user = finish_oauth_login(&state, new_intent, next_receiver, ListPreferences::Preserve)
            .await
            .unwrap();
        assert_eq!(user.id, 8);
        assert_eq!(requests.recv().await.unwrap(), json!({"code":"new-code"}));
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert_eq!(
            state.db.get_setting(TOKEN_KEY).unwrap().as_deref(),
            Some("new-token")
        );
    }

    #[tokio::test]
    async fn cancelled_oauth_code_cannot_start_exchange() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state_with_api(api);
        seed_existing_login(&state);
        let intent = state.begin_auth_intent();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(Ok("old-code".into())).unwrap();
        state.cancel_login();
        assert_eq!(
            finish_oauth_login(&state, intent, receiver, ListPreferences::Preserve)
                .await
                .unwrap_err(),
            AUTH_REPLACED
        );
        assert!(requests.try_recv().is_err());
        assert_existing_login(&state);
    }

    #[tokio::test]
    async fn cancelled_login_during_verification_keeps_the_existing_account() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, viewer_response(8, "Replacement"))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state_with_api(api));
        seed_existing_login(&state);
        let intent = state.begin_auth_intent();
        let waiting = state.clone();
        let job = tokio::spawn(async move {
            login_with_token_inner(
                "replacement-token".into(),
                &waiting,
                intent,
                ListPreferences::Preserve,
            )
            .await
        });
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        state.cancel_login();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), job)
            .await
            .unwrap()
            .unwrap();
        gate.add_permits(1);
        assert_eq!(result.unwrap_err(), AUTH_REPLACED);
        assert_existing_login(&state);
    }

    #[tokio::test]
    async fn cancelled_login_waiting_to_commit_keeps_the_existing_account() {
        let (api, mut requests) =
            anilist::mock_api(vec![(200, viewer_response(8, "Replacement"))]).await;
        let state = Arc::new(state_with_api(api));
        seed_existing_login(&state);
        let entry_guard = state.entry_lock.lock().await;
        let intent = state.begin_auth_intent();
        let waiting = state.clone();
        let job = tokio::spawn(async move {
            login_with_token_inner(
                "replacement-token".into(),
                &waiting,
                intent,
                ListPreferences::Preserve,
            )
            .await
        });
        requests.recv().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if state.remote_lock.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.cancel_login();
        drop(entry_guard);
        assert_eq!(job.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert_existing_login(&state);
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
            login_with_token_inner(
                "earlier-token".into(),
                &first_state,
                first_intent,
                ListPreferences::Preserve,
            )
            .await
        });
        requests.recv().await.unwrap();
        let latest_intent = state.begin_auth_intent();
        let latest_state = state.clone();
        let latest = tokio::spawn(async move {
            login_with_token_inner(
                "latest-token".into(),
                &latest_state,
                latest_intent,
                ListPreferences::Preserve,
            )
            .await
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
            login_with_token_inner(
                "earlier-token".into(),
                &first_state,
                first_intent,
                ListPreferences::Preserve,
            )
            .await
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
            login_with_token_inner(
                "old-token".into(),
                &state,
                old_intent,
                ListPreferences::Preserve
            )
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
            let state = Arc::new(state_with_api(api));
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
                "sync" => assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                    .await
                    .is_err()),
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
    async fn forced_profile_refresh_checks_viewer_even_with_a_cached_user() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data": {"Viewer": {"id": 7, "name": "Fresh", "avatar": {"large": "https://s4.anilist.co/avatar.png"}, "mediaListOptions": {"scoreFormat": "POINT_10"}}}})),
            (503, json!({"errors": [{"message": "Service unavailable"}]})),
        ])
        .await;
        let state = state_with_api(api);
        let original = User {
            id: 7,
            name: "Cached".into(),
            ..Default::default()
        };
        state.db.replace_account("test-session", &original).unwrap();
        *state.user.lock() = Some(original);

        let online = refresh_current_user(&state).await.unwrap().unwrap();
        assert_eq!(online.name, "Fresh");
        assert_eq!(state.db.cached_user().unwrap().unwrap().name, "Fresh");
        assert_eq!(requests.recv().await.unwrap()["variables"], json!({}));

        let offline = refresh_current_user(&state).await.unwrap().unwrap();
        assert!(offline.offline);
        assert_eq!(offline.name, "Fresh");
        assert!(requests.recv().await.is_some());
        assert!(state.user.lock().is_none());
        assert!(state.anilist.lock().has_token());
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
        state.db.set_setting(USER_ID_KEY, "71").unwrap();
        let offline = current_user_inner(&state).await.unwrap().unwrap();
        assert!(offline.offline);
        assert!(offline.score_format.is_none());
        assert_eq!(offline.name, "Tester");
        assert_eq!(offline.id, 0);
    }

    #[tokio::test]
    async fn native_offline_profile_uses_saved_identity_without_cached_json() {
        for with_cached_profile in [false, true] {
            for (saved, expected) in [("71", 71), ("0", 0), ("-1", 0), ("2147483648", 0)] {
                let (api, _) = anilist::mock_api(vec![(
                    503,
                    json!({"errors": [{"message": "Service unavailable"}]}),
                )])
                .await;
                let state = state_with_api(api);
                state.db.set_setting(USERNAME_KEY, "Tester").unwrap();
                state.db.set_setting(USER_ID_KEY, saved).unwrap();
                if with_cached_profile {
                    let parsed: i64 = saved.parse().unwrap();
                    let profile = User {
                        id: parsed,
                        name: "Tester".into(),
                        score_format: Some("POINT_10".into()),
                        ..Default::default()
                    };
                    state
                        .db
                        .set_setting(PROFILE_KEY, &serde_json::to_string(&profile).unwrap())
                        .unwrap();
                }
                assert_eq!(
                    state.db.cached_user().unwrap().is_some(),
                    with_cached_profile && expected != 0
                );

                let offline = current_user_mode(
                    &state,
                    false,
                    ListPreferences::Native { last_account: None },
                )
                .await
                .unwrap()
                .unwrap();
                assert!(offline.offline);
                assert_eq!(offline.id, expected);
                assert_eq!(offline.name, "Tester");
                assert_eq!(
                    offline.score_format.as_deref(),
                    (with_cached_profile && expected != 0).then_some("POINT_10")
                );
                assert!(state.user.lock().is_none());
                if expected == 0 {
                    assert!(cached_feature_user(&state).is_err());
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn account_replacement_clears_matchers_before_publishing_new_session() {
        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
        for via_refresh in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "kurisu-switch-order-{}-{}-{via_refresh}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db_path = dir.join("kurisu.db");
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let viewer = (200, json!({"data": {"Viewer": {"id": 8, "name": "Next"}}}));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![viewer.clone(), viewer], Some(gate.clone())).await;
            let mut base_state = state_with_api(api);
            base_state.db = Arc::new(Db::open(&db_path).unwrap());
            let state = Arc::new(base_state);
            state
                .db
                .replace_account(
                    "token-a",
                    &User {
                        id: 7,
                        name: "Prev".into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            state.anilist.lock().set_token(Some(if via_refresh {
                "token-b".into()
            } else {
                "token-a".into()
            }));
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    title_english: Some("Account seven title".into()),
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
                    progress: 1,
                    ..Default::default()
                })
                .unwrap();
            state.refresh_matchers();
            assert_eq!(state.matchers.lock()[0].media_id, 1);

            state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_switch BEFORE INSERT ON settings
                     WHEN NEW.key = 'anilist_profile'
                     BEGIN SELECT RAISE(ABORT, 'switch failed'); END;",
                )
                .unwrap();
            gate.add_permits(1);
            let failed = if via_refresh {
                current_user_mode(
                    &state,
                    true,
                    ListPreferences::Native {
                        last_account: Some(7),
                    },
                )
                .await
                .unwrap_err()
            } else {
                login_with_token(
                    "token-b".into(),
                    &state,
                    ListPreferences::Native {
                        last_account: Some(7),
                    },
                )
                .await
                .unwrap_err()
            };
            assert!(failed.contains("switch failed"), "{failed}");
            assert_eq!(state.matchers.lock()[0].media_id, 1);
            requests.recv().await.unwrap();
            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_switch")
                .unwrap();

            static BUSY_GATE: Mutex<
                Option<(
                    tokio::sync::oneshot::Sender<()>,
                    std::sync::mpsc::Receiver<()>,
                )>,
            > = Mutex::new(None);
            fn on_busy(_: i32) -> bool {
                let gate = BUSY_GATE.lock().take();
                if let Some((entered, release)) = gate {
                    entered.send(()).unwrap();
                    release
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                true
            }

            let gate_path = dir.join("gate.db");
            let blocker = rusqlite::Connection::open(&gate_path).unwrap();
            blocker
                .execute_batch("PRAGMA journal_mode = DELETE; CREATE TABLE barrier (x INTEGER);")
                .unwrap();
            {
                let conn = state.db.0.lock();
                conn.execute(
                    "ATTACH DATABASE ?1 AS gate_db",
                    [gate_path.to_str().unwrap()],
                )
                .unwrap();
                conn.execute_batch(
                    "CREATE TEMP TRIGGER pause_before_account_publish BEFORE INSERT ON settings
                     WHEN NEW.key = 'anilist_user_id'
                     BEGIN INSERT INTO gate_db.barrier VALUES (1); END;",
                )
                .unwrap();
                conn.busy_handler(Some(on_busy)).unwrap();
            }
            blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
            let (tx_busy_entered, rx_busy_entered) = tokio::sync::oneshot::channel();
            let (tx_busy_release, rx_busy_release) = std::sync::mpsc::channel();
            *BUSY_GATE.lock() = Some((tx_busy_entered, rx_busy_release));

            let worker_state = state.clone();
            let job = tokio::spawn(async move {
                if via_refresh {
                    current_user_mode(
                        &worker_state,
                        true,
                        ListPreferences::Native {
                            last_account: Some(7),
                        },
                    )
                    .await
                    .map(|u| u.unwrap())
                } else {
                    login_with_token(
                        "token-b".into(),
                        &worker_state,
                        ListPreferences::Native {
                            last_account: Some(7),
                        },
                    )
                    .await
                }
            });
            requests.recv().await.unwrap();
            let holder_state = state.clone();
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _user_guard = holder_state.user.lock();
                entered.send(()).unwrap();
                blocked
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            });
            started.await.unwrap();
            gate.add_permits(1);
            // Pause inside replace_account_with_preferences before the new account ID commits.
            tokio::time::timeout(std::time::Duration::from_secs(5), rx_busy_entered)
                .await
                .unwrap()
                .unwrap();
            assert!(
                state.matchers.lock().is_empty(),
                "matchers must be cleared before committing replacement account, via_refresh={via_refresh}"
            );
            if !via_refresh {
                assert_eq!(state.anilist.lock().token().as_deref(), Some("token-a"));
            }
            blocker.execute_batch("ROLLBACK").unwrap();
            drop(blocker);
            tx_busy_release.send(()).unwrap();

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.origin().account_id != Some(8) {
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            assert!(state.matchers.lock().is_empty());
            if !via_refresh {
                assert_eq!(state.anilist.lock().token().as_deref(), Some("token-a"));
            }
            release.send(()).unwrap();
            holder.join().unwrap();
            let user = job.await.unwrap().unwrap();
            assert_eq!(user.id, 8);
            assert!(state.matchers.lock().is_empty());
            assert_eq!(state.anilist.lock().token().as_deref(), Some("token-b"));
            drop(state);
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[tokio::test]
    async fn uncached_removal_checks_remote_presence() {
        for present in [false, true] {
            let remote = if present {
                json!({"id":22,"mediaId":1,"status":"CURRENT","progress":3,"repeat":0,"score":0})
            } else {
                json!(null)
            };
            let mut responses = vec![(
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote}}}),
            )];
            if present {
                responses.push((
                    200,
                    json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
                ));
            }
            let (api, mut requests) = anilist::mock_api(responses).await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(state.db.get_entry(1).unwrap().is_none());
            assert!(state.db.pending(7).unwrap().is_empty());
            delete_entry_cmd(1, &state).await.unwrap();
            let lookup = requests
                .try_recv()
                .expect("a cache miss cannot prove remote absence");
            assert!(lookup["query"].as_str().unwrap().contains("mediaListEntry"));
            assert_eq!(lookup["variables"]["mediaId"], 1);
            if present {
                let deletion = requests.try_recv().unwrap();
                assert!(deletion["query"]
                    .as_str()
                    .unwrap()
                    .contains("DeleteMediaListEntry"));
                assert_eq!(deletion["variables"]["id"], 22);
            }
            assert!(requests.try_recv().is_err());
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_entry(1).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn uncached_removal_failure_survives_reopen_and_account_switch() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for lookup_fails in [true, false] {
            let directory = Directory(std::env::temp_dir().join(format!(
                    "kurisu-uncached-removal-{}-{}-{lookup_fails}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )));
            let path = directory.0.join("kurisu.db");
            let lookup = (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":3,"repeat":0,"score":0}}}}),
            );
            let mut responses = if lookup_fails {
                vec![(400, json!({"errors":[{"message":"Lookup refused"}]}))]
            } else {
                vec![
                    lookup.clone(),
                    (
                        200,
                        json!({"data":{"DeleteMediaListEntry":{"deleted":false}}}),
                    ),
                ]
            };
            responses.extend([
                (200, viewer_response(8, "Other account")),
                (200, viewer_response(7, "Original account")),
            ]);
            if lookup_fails {
                responses.push(lookup);
            }
            responses.push((
                200,
                json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
            ));
            let (api, mut requests) = anilist::mock_api(responses).await;
            let mut state = state_with_api(api);
            state.db = Arc::new(Db::open(&path).unwrap());
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            let error = delete_entry_cmd(1, &state).await.unwrap_err();
            assert!(error.contains(if lookup_fails {
                "Lookup refused"
            } else {
                "declined"
            }));
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            if !lookup_fails {
                assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
            }
            state.db = Arc::new(Db::open(&path).unwrap());
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.deleting && pending.conflict);
            assert_eq!(pending.deletion_id, (!lookup_fails).then_some(22));
            assert!(pending.base.is_none());
            assert!(pending
                .error
                .as_deref()
                .unwrap()
                .contains("Removal is pending"));
            crate::sync_queue::flush_background(&state).await.unwrap();
            assert!(
                requests.try_recv().is_err(),
                "failed removal requires review"
            );

            assert_eq!(
                login_with_token(
                    "synthetic-other-token".into(),
                    &state,
                    ListPreferences::Preserve
                )
                .await
                .unwrap()
                .id,
                8
            );
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            crate::sync_queue::flush_background(&state).await.unwrap();
            crate::sync_queue::resolve_pending_change(1, true, &state)
                .await
                .unwrap();
            assert!(
                requests.try_recv().is_err(),
                "another account must not inherit removal"
            );
            assert_eq!(
                serde_json::to_value(state.db.pending_change(7, 1).unwrap().unwrap()).unwrap(),
                serde_json::to_value(&pending).unwrap()
            );
            assert!(state.db.pending(8).unwrap().is_empty());

            assert_eq!(
                login_with_token(
                    "synthetic-original-token".into(),
                    &state,
                    ListPreferences::Preserve
                )
                .await
                .unwrap()
                .id,
                7
            );
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            crate::sync_queue::resolve_pending_change(1, true, &state)
                .await
                .unwrap();
            if lookup_fails {
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("mediaListEntry"));
            }
            let deletion = requests.try_recv().unwrap();
            assert!(deletion["query"]
                .as_str()
                .unwrap()
                .contains("DeleteMediaListEntry"));
            assert_eq!(deletion["variables"]["id"], 22);
            assert!(requests.try_recv().is_err());
            state.db = Arc::new(Db::open(&path).unwrap());
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_entry(1).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn removal_retains_pending_when_second_delete_is_not_confirmed() {
        let (api, mut requests) = anilist::mock_api(vec![
            (404, json!({"errors":[{"message":"Not Found."}]})),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":33,"mediaId":1,"status":"CURRENT","progress":9,"repeat":0,"score":80}}}})),
            (404, json!({"errors":[{"message":"Not Found."}]})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
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
                ..Default::default()
            })
            .unwrap();

        assert!(delete_entry_inner(1, &state).await.is_err());
        assert!(state.db.get_entry(1).unwrap().is_some());
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.deleting && pending.conflict);
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("mediaListEntry"));
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 33);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn background_removal_keeps_replacement_for_explicit_review() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"DeleteMediaListEntry":{"deleted":true}}})),
            (404, json!({"errors":[{"message":"Not Found."}]})),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":33,"mediaId":1,"status":"CURRENT","progress":9,"repeat":0,"score":80}}}})),
            (404, json!({"errors":[{"message":"Not Found."}]})),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":33,"mediaId":1,"status":"CURRENT","progress":9,"repeat":0,"score":80}}}})),
            (200, json!({"data":{"DeleteMediaListEntry":{"deleted":true}}})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                progress: 3,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        state.db.0.lock().execute_batch("CREATE TRIGGER fail_ack BEFORE DELETE ON pending_change BEGIN SELECT RAISE(FAIL, 'injected acknowledgement failure'); END;").unwrap();
        assert!(delete_entry_inner(1, &state)
            .await
            .unwrap_err()
            .contains("injected acknowledgement failure"));
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.deleting && !pending.conflict);
        assert_eq!(pending.base.unwrap().id, Some(22));
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_ack")
            .unwrap();
        let _ = crate::sync_queue::flush_background(&state).await;
        let retained = state
            .db
            .pending_change(7, 1)
            .unwrap()
            .expect("replacement requires review");
        assert!(retained.deleting && retained.conflict);
        assert!(state.db.get_entry(1).unwrap().is_some());
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("mediaListEntry"));
        assert!(
            requests.try_recv().is_err(),
            "replacement must not be deleted automatically"
        );
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(requests.try_recv().is_err());
        crate::sync_queue::resolve_pending_change(1, true, &state)
            .await
            .unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("mediaListEntry"));
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 33);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn removal_pins_resolved_target_across_reopen_and_cache_change() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        for background in [false, true] {
            let directory = Directory(std::env::temp_dir().join(format!(
                    "kurisu-removal-target-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )));
            let path = directory.0.join("removal.db");
            let live = json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":33,"mediaId":1,"status":"CURRENT","progress":9,"repeat":0,"score":80}}}});
            let (api, mut requests) = anilist::mock_api(vec![
                (404, json!({"errors":[{"message":"Not Found."}]})),
                (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":4,"repeat":0,"score":0}}}})),
                (200, json!({"data":{"DeleteMediaListEntry":{"deleted":true}}})),
                (200, live),
            ]).await;
            let mut state = state_with_api(api);
            state.db = Arc::new(Db::open(&path).unwrap());
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        score_format: Some("POINT_100".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(11),
                    media_id: 1,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                })
                .unwrap();
            crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap();
            state.db.0.lock().execute_batch("CREATE TRIGGER fail_ack BEFORE DELETE ON pending_change BEGIN SELECT RAISE(FAIL, 'ack failed'); END;").unwrap();
            assert!(delete_entry_inner(1, &state)
                .await
                .unwrap_err()
                .contains("ack failed"));
            state.db = Arc::new(Db::open(&path).unwrap());
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(pending.deletion_id, Some(22));
            assert_eq!(pending.base.unwrap().id, Some(11));
            assert_eq!(pending.patch.progress, Some(4));
            assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 11);
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_ack")
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(33),
                    media_id: 1,
                    progress: 9,
                    status: "CURRENT".into(),
                    ..Default::default()
                })
                .unwrap();
            let result = if background {
                crate::sync_queue::flush_background(&state).await
            } else {
                crate::sync_queue::sync_pending_changes(&state)
                    .await
                    .map(|_| ())
            };
            result.unwrap();
            assert!(requests.try_recv().is_err());
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict && pending.deleting);
            assert_eq!(pending.deletion_id, Some(22));
            crate::sync_queue::resolve_pending_change(1, false, &state)
                .await
                .unwrap();
            let saved = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(saved.id, Some(33));
            assert_eq!(saved.progress, 9);
            assert_eq!(saved.score, Some(80.0));
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            crate::sync_queue::flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn removal_requires_durable_resolved_target_before_delete() {
        for (cached, cached_id) in [(false, None), (true, None), (true, Some(11))] {
            let mut responses = vec![];
            if cached_id.is_some() {
                responses.push((404, json!({"errors":[{"message":"Not Found."}]})));
            }
            responses.push((200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":4,"repeat":0,"score":0}}}})));
            let (api, mut requests) = anilist::mock_api(responses).await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            if cached {
                state
                    .db
                    .upsert_entry(&ListEntry {
                        id: cached_id,
                        media_id: 1,
                        progress: 3,
                        status: "CURRENT".into(),
                        ..Default::default()
                    })
                    .unwrap();
            }
            state.db.0.lock().execute_batch("CREATE TRIGGER fail_target BEFORE INSERT ON pending_change WHEN json_extract(NEW.payload, '$.deletion_id') = 22 BEGIN SELECT RAISE(FAIL, 'target unavailable'); END;").unwrap();
            assert!(delete_entry_inner(1, &state)
                .await
                .unwrap_err()
                .contains("target unavailable"));
            if cached_id.is_some() {
                assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 11);
            }
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            assert!(
                requests.try_recv().is_err(),
                "resolved target must be durable before deletion"
            );
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict && pending.deleting);
            assert_eq!(pending.deletion_id, cached_id);
            assert_eq!(state.db.get_entry(1).unwrap().is_some(), cached);
        }
    }

    #[tokio::test]
    async fn removal_without_saved_target_never_infers_it_from_cache_or_old_baseline() {
        for present in [false, true] {
            let remote = if present {
                json!({"id":22,"mediaId":1,"status":"CURRENT","progress":4,"repeat":0,"score":0})
            } else {
                json!(null)
            };
            let (api, mut requests) = anilist::mock_api(vec![(
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote}}}),
            )])
            .await;
            let state = state_with_api(api);
            state
                .db
                .replace_account(
                    "test-session",
                    &User {
                        id: 7,
                        ..Default::default()
                    },
                )
                .unwrap();
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(22),
                    media_id: 1,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                })
                .unwrap();
            let (_, mut pending, _) = crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            pending.deleting = true;
            let mut payload = serde_json::to_value(pending).unwrap();
            payload.as_object_mut().unwrap().remove("deletion_id");
            state
                .db
                .0
                .lock()
                .execute(
                    "UPDATE pending_change SET payload = ? WHERE account_id = 7 AND media_id = 1",
                    [payload.to_string()],
                )
                .unwrap();
            let result = crate::sync_queue::flush_background(&state).await;
            assert_eq!(result.is_err(), present);
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            assert!(requests.try_recv().is_err());
            let pending = state.db.pending_change(7, 1).unwrap();
            if present {
                let pending = pending.unwrap();
                assert!(pending.deleting && pending.conflict);
                assert_eq!(pending.deletion_id, None);
                assert_eq!(pending.patch.progress, Some(4));
            } else {
                assert!(pending.is_none());
                assert!(state.db.get_entry(1).unwrap().is_none());
            }
        }
    }

    #[tokio::test]
    async fn removal_detects_a_silent_list_cache_delete_failure() {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
            ),
            (404, json!({"errors":[{"message":"Not Found"}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
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
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER ignore_list_removal BEFORE DELETE ON list_entry
             BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();

        let error = delete_entry_inner(1, &state).await.unwrap_err();
        assert!(error.contains("the list entry was not removed"), "{error}");
        assert!(state.db.get_entry(1).unwrap().is_some());
        assert!(state.db.pending_change(7, 1).unwrap().unwrap().deleting);
        assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);

        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER ignore_list_removal")
            .unwrap();
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn removing_an_absent_queued_add_does_not_replay_it_after_local_cleanup_fails() {
        let absent = (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
        );
        let (api, mut requests) = anilist::mock_api(vec![absent.clone(), absent]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
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
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_absent_removal BEFORE DELETE ON pending_change
             BEGIN SELECT RAISE(FAIL, 'fixture absence cleanup failure'); END;",
            )
            .unwrap();

        let error = delete_entry_inner(1, &state).await.unwrap_err();
        assert!(error.contains("fixture absence cleanup failure"), "{error}");
        let retained = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(retained.deleting);
        assert!(retained.deletion_id.is_none());
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_absent_removal")
            .unwrap();
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(state.db.get_entry(1).unwrap().is_none());
        for _ in 0..2 {
            let request = requests.try_recv().unwrap();
            assert!(request["query"].as_str().unwrap().starts_with("query"));
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn removal_survives_failed_acknowledgement_and_reopen_without_replaying_add() {
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Directory(std::env::temp_dir().join(format!(
                "kurisu-removal-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        let path = directory.0.join("removal.db");
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
            ),
            (404, json!({"errors":[{"message":"Not Found."}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
            ),
        ])
        .await;
        let mut state = state_with_api(api);
        state.db = Arc::new(Db::open(&path).unwrap());
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
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
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.id = Some(22);
        state.db.upsert_entry(&entry).unwrap();
        state.db.0.lock().execute_batch("CREATE TRIGGER fail_removal BEFORE DELETE ON pending_change BEGIN SELECT RAISE(FAIL, 'injected acknowledgement failure'); END;").unwrap();
        assert!(delete_entry_inner(1, &state)
            .await
            .unwrap_err()
            .contains("injected acknowledgement failure"));
        assert!(
            state.db.get_entry(1).unwrap().is_some(),
            "local acknowledgement is atomic"
        );
        state.db = Arc::new(Db::open(&path).unwrap());
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.deleting);
        assert_eq!(pending.deletion_id, Some(22));
        assert_eq!(pending.patch.progress, Some(1));
        assert!(pending.base.is_none());
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("DeleteMediaListEntry"));
        state
            .db
            .replace_account(
                "other-token",
                &User {
                    id: 8,
                    ..Default::default()
                },
            )
            .unwrap();
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending_change(7, 1).unwrap().unwrap().deleting);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        state.db.delete_entry(1).unwrap();
        assert!(local_entries_inner(&state).await.unwrap().is_empty());
        assert!(get_entry(1, &state).await.unwrap().is_none());
        assert!(crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(2),
                ..Default::default()
            }
        )
        .is_err());
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_removal")
            .unwrap();
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(requests.recv().await.unwrap()["variables"]["id"], 22);
        let request = requests.recv().await.unwrap();
        assert!(!request["query"]
            .as_str()
            .unwrap()
            .contains("SaveMediaListEntry"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn removal_requires_durable_intent_and_keeps_failed_remote_changes() {
        let (api, mut requests) = anilist::mock_api(vec![
            (400, json!({"errors":[{"message":"Removal refused"}]})),
            (
                200,
                json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
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
        let (_, pending, _) = crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        state.db.0.lock().execute_batch("CREATE TRIGGER fail_intent BEFORE INSERT ON pending_change BEGIN SELECT RAISE(FAIL, 'intent unavailable'); END;").unwrap();
        assert!(delete_entry_inner(1, &state)
            .await
            .unwrap_err()
            .contains("intent unavailable"));
        assert!(requests.try_recv().is_err());
        assert!(!state.db.pending_change(7, 1).unwrap().unwrap().deleting);
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_intent")
            .unwrap();
        assert!(delete_entry_inner(1, &state).await.is_err());
        let retained = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(retained.deleting && retained.conflict);
        assert_eq!(retained.patch.progress, pending.patch.progress);
        assert_eq!(
            retained.base.unwrap().progress,
            pending.base.unwrap().progress
        );
        assert!(state.db.get_entry(1).unwrap().is_some());
        crate::sync_queue::flush_background(&state).await.unwrap();
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("DeleteMediaListEntry"));
        assert!(requests.try_recv().is_err());
        crate::sync_queue::resolve_pending_change(1, true, &state)
            .await
            .unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(state.db.get_entry(1).unwrap().is_none());
    }

    #[tokio::test]
    async fn removing_a_queued_add_checks_for_an_interrupted_remote_success() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":1,"repeat":0, "score": 0}}}})),
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
                "Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "CURRENT", "progress": 3, "repeat": 0, "score": 0}},
                "MediaList": {"id": 22, "status": "CURRENT", "progress": 3, "repeat": 0, "score": 0}
            }})),
            (200, json!({"data": {"SaveMediaListEntry": {"mediaId":1,"id": 22, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}}})),
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
            (200, json!({"data": {"Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"mediaId":1,"id": 22, "status": "PAUSED", "progress": 4, "score": 8.5, "repeat": 2}}})),
        ]).await;
        let state = state_with_api(api);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    score_format: Some("POINT_10_DECIMAL".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let saved = save_entry_inner(&state, 1, Some("PAUSED".into()), None, None, None)
            .await
            .unwrap();
        requests.recv().await.unwrap();
        let write = requests.recv().await.unwrap();
        assert_eq!(
            write["variables"],
            json!({"id": 22, "status": "PAUSED", "scoreFormat": "POINT_10_DECIMAL"})
        );
        assert_eq!(
            (saved.progress, saved.score, saved.repeat),
            (4, Some(8.5), 2)
        );
    }

    #[tokio::test]
    async fn restoring_an_existing_remote_entry_immediately_enables_recognition() {
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data": {"Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "CURRENT", "progress": 4, "repeat": 0, "score": 0}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"mediaId":1,"id": 22, "status": "CURRENT", "progress": 4, "repeat": 0, "score": 0}}})),
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
                (200, json!({"data": {"Media": {"id":1,"mediaListEntry": null}}})),
                (200, json!({"data": {"SaveMediaListEntry": {"mediaId":1,"id": 22, "status": "COMPLETED", "progress": 12, "repeat": 0, "score": 0}}})),
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
    async fn lowering_progress_on_completed_entry_reopens_current_across_save_paths() {
        for mode in [
            "cached_online",
            "cached_offline",
            "remote_online",
            "remote_offline",
        ] {
            let remote_completed = json!({
                "data": {
                    "Media": {
                        "id": 1,
                        "mediaListEntry": {
                            "id": 22,"mediaId":1,
                            "status": "COMPLETED",
                            "progress": 12,
                            "repeat": 1,
                            "score": 80
                        }
                    }
                }
            });
            let save_ok = (
                200,
                json!({
                    "data": {
                        "SaveMediaListEntry": {
                            "mediaId": 1,
                            "id": 22,
                            "status": "CURRENT",
                            "progress": 5,
                            "repeat": 1,
                            "score": 80
                        }
                    }
                }),
            );
            let save_unavailable = (503, json!({"errors": [{"message": "Unavailable"}]}));
            let replies = match mode {
                "cached_online" => vec![save_ok],
                "cached_offline" => vec![save_unavailable.clone(), save_unavailable],
                "remote_online" => vec![(200, remote_completed), save_ok],
                _ => vec![
                    (200, remote_completed),
                    save_unavailable.clone(),
                    save_unavailable,
                ],
            };
            let (api, mut requests) = anilist::mock_api(replies).await;
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
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();
            if mode.starts_with("cached_") {
                state
                    .db
                    .upsert_entry(&ListEntry {
                        id: Some(22),
                        media_id: 1,
                        status: "COMPLETED".into(),
                        progress: 12,
                        score: Some(80.0),
                        repeat: 1,
                        ..Default::default()
                    })
                    .unwrap();
            }

            let saved = save_entry_inner(&state, 1, None, Some(5), None, None)
                .await
                .unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                ("CURRENT", 5, 1),
                "mode={mode}"
            );
            if mode.starts_with("remote_") {
                let lookup = requests.recv().await.unwrap();
                assert!(
                    lookup["query"].as_str().unwrap().contains("mediaListEntry"),
                    "mode={mode}"
                );
            }
            let write = requests.recv().await.unwrap();
            assert_eq!(write["variables"]["status"], "CURRENT", "mode={mode}");
            assert_eq!(write["variables"]["progress"], 5, "mode={mode}");
            if mode.ends_with("_offline") {
                let pending = state.db.pending_change(7, 1).unwrap().unwrap();
                assert_eq!(
                    pending.patch.status.as_deref(),
                    Some("CURRENT"),
                    "mode={mode}"
                );
                assert_eq!(pending.patch.progress, Some(5), "mode={mode}");
            }
        }
    }

    #[tokio::test]
    async fn a_remote_completed_entry_starts_rewatching_at_zero() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data": {"Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "COMPLETED", "progress": 12, "repeat": 2, "score": 0}}}})),
            (200, json!({"data": {"SaveMediaListEntry": {"mediaId":1,"id": 22, "status": "REPEATING", "progress": 0, "repeat": 2, "score": 0}}})),
        ]).await;
        let state = state_with_api(api);
        save_entry_inner(&state, 1, Some("REPEATING".into()), None, None, None)
            .await
            .unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id": 22, "status": "REPEATING", "progress": 0, "scoreFormat": null})
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
                json!({"data": {"MediaListCollection": {"user":{"id":7},"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            let state = Arc::new(state_with_api(api));
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
            assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .is_err());
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
                    "Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "CURRENT", "progress": 0, "repeat": 0, "score": 0}},
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
    async fn queued_playback_rejects_a_logout_intent_with_the_same_token() {
        use std::future::Future;

        for lock in ["remote", "entry"] {
            for already_pending in [false, true] {
                for sign_out in [false, true] {
                    let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
                    let (api, mut requests) = anilist::mock_api(vec![
                        (
                            200,
                            json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                        ),
                        (200, json!({"data":{"SaveMediaListEntry":remote(5)}})),
                    ])
                    .await;
                    let state = Arc::new(state_with_api(api));
                    seed_existing_login(&state);
                    state
                        .db
                        .upsert_entry(&ListEntry {
                            id: Some(22),
                            media_id: 1,
                            status: "CURRENT".into(),
                            progress: 3,
                            score: Some(0.0),
                            ..Default::default()
                        })
                        .unwrap();
                    if already_pending {
                        let (_, pending, _) = crate::sync_queue::stage(
                            &state,
                            1,
                            crate::sync_queue::Patch {
                                progress: Some(4),
                                ..Default::default()
                            },
                        )
                        .unwrap()
                        .unwrap();
                        crate::sync_queue::local_entry(&state, &pending).unwrap();
                    }
                    let saved_pending = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
                    let saved_entry = serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap();
                    let origin = state.origin();
                    let guard = if lock == "remote" {
                        state.remote_lock.lock().await
                    } else {
                        state.entry_lock.lock().await
                    };
                    let mut playback =
                        Box::pin(watcher_set_progress(&state, 1, 5, Some("test-session")));
                    std::future::poll_fn(|cx| {
                        assert!(playback.as_mut().poll(cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    if lock == "entry" {
                        assert!(state.remote_lock.try_lock().is_err());
                    }
                    let signing_out = if sign_out {
                        let mut action = Box::pin(logout(&state));
                        std::future::poll_fn(|cx| {
                            assert!(action.as_mut().poll(cx).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                        assert!(!state.origin_is_current(origin));
                        Some(action)
                    } else {
                        None
                    };
                    drop(guard);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(5), playback)
                        .await
                        .unwrap();
                    assert_existing_login(&state);
                    assert!(!requests.is_closed());
                    if sign_out {
                        let sent =
                            std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
                        assert!(
                            sent.is_empty(),
                            "playback after {lock}: {result:?}, sent {sent:?}, pending {:?}",
                            state.db.pending(7).unwrap()
                        );
                        assert!(result.unwrap().is_none());
                        assert_eq!(
                            serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                            saved_pending
                        );
                        assert_eq!(
                            serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap(),
                            saved_entry
                        );
                        signing_out.unwrap().await.unwrap();
                        assert!(!state.anilist.lock().has_token());
                        assert_eq!(
                            serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                            saved_pending
                        );
                    } else {
                        assert_eq!(result.unwrap().unwrap().progress, 5);
                        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 5);
                        if already_pending {
                            assert_eq!(
                                state
                                    .db
                                    .pending_change(7, 1)
                                    .unwrap()
                                    .unwrap()
                                    .patch
                                    .progress,
                                Some(5)
                            );
                        } else {
                            assert!(state.db.pending(7).unwrap().is_empty());
                            assert_eq!(
                                requests.try_recv().unwrap()["variables"],
                                json!({"mediaId":1,"scoreFormat":null})
                            );
                            let sent = requests.try_recv().unwrap();
                            assert!(sent["query"]
                                .as_str()
                                .unwrap()
                                .contains("SaveMediaListEntry"));
                            assert_eq!(
                                sent["variables"],
                                json!({"id":22,"progress":5,"scoreFormat":null})
                            );
                        }
                        assert!(matches!(
                            requests.try_recv(),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                        ));
                    }
                }
            }
        }
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
        let mut events = state.events.subscribe();
        let error = save_entry_inner(&state, 1, Some("PLANNING".into()), None, None, None)
            .await
            .unwrap_err();
        assert_eq!(error, SESSION_EXPIRED);
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.get_setting(PROFILE_KEY).unwrap().is_none());
        let expired = events.try_recv().unwrap();
        assert!(matches!(expired.kind, crate::EventKind::AuthExpired));
        assert_eq!(expired.account_id, Some(7));
        assert_eq!(expired.generation, 0);
        assert!(state.event_is_current(&expired));
    }
}

#[cfg(test)]
mod feature_cache_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn detail_removal_respects_the_session_that_started_the_read() {
        for (signed_out, superseded) in [(true, false), (true, true), (false, true)] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(
                vec![(404, json!({"errors":[{"message":"Not Found"}]}))],
                Some(gate.clone()),
            )
            .await;
            let state = Arc::new(state(api));
            cache_media_details(&state);
            pending_progress(&state);
            if signed_out {
                state.anilist.lock().set_token(None);
            }
            let detail = state.db.get_setting("media_detail:1").unwrap();
            let worker = state.clone();
            let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
            let request = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request["variables"]["id"], 1);
            if superseded {
                state.cancel_login();
            }
            gate.add_permits(1);

            assert_eq!(
                read.await.unwrap().unwrap_err(),
                if superseded {
                    AUTH_REPLACED
                } else {
                    MEDIA_GONE
                }
            );
            assert_eq!(state.db.get_media(1).unwrap().is_some(), superseded);
            assert_eq!(
                state.db.get_setting("media_detail:1").unwrap(),
                if superseded {
                    detail
                } else {
                    Some("null".into())
                }
            );
            assert!(state.db.get_entry(1).unwrap().is_some());
            let pending = &state.db.pending(7).unwrap()[0];
            assert_eq!(pending.patch.progress, Some(4));
            assert!(!pending.missing_media && !pending.conflict);
        }
    }

    #[tokio::test]
    async fn delayed_public_reads_respect_committed_removals() {
        for (search, commit) in [(true, true), (true, false), (false, true)] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let response = if search {
                total_search(Some(11))
            } else {
                (503, json!({"errors":[{"message":"Unavailable"}]}))
            };
            let (api, mut requests) =
                anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
            let state = Arc::new(state(api));
            cache_media_details(&state);
            if !search {
                state.anilist.lock().set_token(None);
            }
            let worker = state.clone();
            let read = tokio::spawn(async move {
                if search {
                    search_anime_page("test".into(), 1, &worker)
                        .await
                        .map(|page| !page.items.is_empty())
                } else {
                    get_media_detail(1, &worker).await.map(|_| true)
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            let (mut api, _) = anilist::mock_api(vec![
                (404, json!({"errors":[{"message":"Not Found"}]})),
                total_search(Some(13)),
            ])
            .await;
            if !search {
                api.set_token(None);
            }
            *state.anilist.lock() = api;
            if !commit {
                state
                    .db
                    .0
                    .lock()
                    .execute_batch(
                        "CREATE TEMP TRIGGER fail_detail BEFORE INSERT ON settings
                     BEGIN SELECT RAISE(ABORT, 'detail failed'); END;",
                    )
                    .unwrap();
            }
            let error = get_media_detail(1, &state).await.unwrap_err();
            assert!(if commit {
                error == MEDIA_GONE
            } else {
                error.contains("detail failed")
            });
            gate.add_permits(1);
            let delayed = read.await.unwrap();
            if search {
                assert_eq!(delayed.unwrap(), !commit);
            } else {
                assert_eq!(delayed.unwrap_err(), MEDIA_GONE);
            }
            assert_eq!(state.db.get_media(1).unwrap().is_some(), !commit);
            state.anilist.lock().set_token(Some("test-session".into()));
            let fresh = search_anime_page("test".into(), 1, &state).await.unwrap();
            assert_eq!(fresh.items[0].episodes, Some(13));
        }
    }

    #[tokio::test]
    async fn detail_missing_cleanup_rolls_back_together() {
        let (api, _) =
            anilist::mock_api(vec![(404, json!({"errors":[{"message":"Not Found"}]}))]).await;
        let state = state(api);
        cache_media_details(&state);
        pending_progress(&state);
        let detail = state.db.get_setting("media_detail:1").unwrap();
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER reject_media_cleanup BEFORE DELETE ON media
             BEGIN SELECT RAISE(ABORT, 'cleanup failed'); END;",
            )
            .unwrap();
        assert!(get_media_detail(1, &state)
            .await
            .unwrap_err()
            .contains("cleanup failed"));
        assert!(state.db.get_media(1).unwrap().is_some());
        assert!(state.db.get_entry(1).unwrap().is_some());
        assert_eq!(state.db.get_setting("media_detail:1").unwrap(), detail);
        let pending = &state.db.pending(7).unwrap()[0];
        assert!(!pending.missing_media && !pending.conflict);
    }

    #[tokio::test]
    async fn retryable_not_found_response_preserves_the_cached_detail() {
        for (status, body) in [
            (
                503,
                json!({"errors":[{"status":404,"message":"Not Found"}]}),
            ),
            (404, json!({"errors":[]})),
            (404, json!({"errors":[{}]})),
            (
                200,
                json!({"errors":[{"status":404,"message":"Not Found","path":["Media","staff"]}]}),
            ),
            (
                200,
                json!({"errors":[{"status":404,"message":"Not Found"},{"status":503,"message":"Unavailable"}]}),
            ),
        ] {
            let (api, _) = anilist::mock_api(vec![(status, body)]).await;
            let state = state(api);
            cache_media_details(&state);
            pending_progress(&state);
            let detail = get_media_detail(1, &state).await.unwrap();
            assert!(detail.warning.is_some());
            assert_eq!(detail.media.episodes, Some(12));
            assert!(state.db.get_entry(1).unwrap().is_some());
            assert!(!state.db.pending(7).unwrap()[0].missing_media);
        }
    }

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
    async fn delayed_detail_preserves_the_episode_total_confirmed_during_the_read() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut media = anilist::mock_media(1);
        media["episodes"] = json!(12);
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, json!({"data":{"Media":media}}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        cache_media_details(&state);
        let reading = state.clone();
        let read = tokio::spawn(async move { get_media_detail_inner(1, &reading).await });
        requests.recv().await.unwrap();
        state.db.cache_episode_total(1, Some(13)).unwrap();
        gate.add_permits(1);
        let detail = read.await.unwrap().unwrap();
        assert!(detail.warning.is_none());
        assert_eq!(detail.media.episodes, Some(13));
        assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
    }

    #[tokio::test]
    async fn delayed_detail_enriches_newer_search_metadata_and_keeps_it_offline() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Old title");
        media["seasonYear"] = json!(2025);
        media["bannerImage"] = json!("banner.jpg");
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, json!({"data":{"Media":media}}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
        requests.recv().await.unwrap();

        let search = |year| {
            let (status, mut body) = total_search(None);
            body["data"]["Page"]["media"][0]["seasonYear"] = json!(year);
            (status, body)
        };
        let (api, _) = anilist::mock_api(vec![
            search(Some(2026)),
            search(None),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        *state.anilist.lock() = api;
        search_anime_page("test".into(), 1, &state).await.unwrap();
        gate.add_permits(1);
        let detail = read.await.unwrap().unwrap();
        assert_eq!(detail.media.display_title(), "Search title");
        assert_eq!(detail.media.season_year, Some(2026));
        assert_eq!(detail.media.banner_image.as_deref(), Some("banner.jpg"));
        assert_eq!(get_media(1, &state).await.unwrap(), detail.media);

        search_anime_page("test".into(), 1, &state).await.unwrap();
        let cached = get_media_detail(1, &state).await.unwrap();
        assert!(cached.warning.is_some());
        assert_eq!(cached.media.season_year, None);
        assert_eq!(cached.media.banner_image.as_deref(), Some("banner.jpg"));
        assert_eq!(get_media(1, &state).await.unwrap(), cached.media);
    }

    #[tokio::test]
    async fn delayed_detail_ignores_a_search_cache_write_that_was_skipped() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Detail title");
        media["bannerImage"] = json!("banner.jpg");
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, json!({"data":{"Media":media}}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        cache_media_details(&state);
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
        requests.recv().await.unwrap();

        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER skip_search BEFORE INSERT ON media
             WHEN NEW.title_english = 'Search title' BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        let (api, _) = anilist::mock_api(vec![total_search(Some(24))]).await;
        *state.anilist.lock() = api;
        assert_eq!(
            search_anime_page("test".into(), 1, &state)
                .await
                .unwrap()
                .items[0]
                .display_title(),
            "Search title"
        );
        assert_eq!(
            state.db.get_media(1).unwrap().unwrap().display_title(),
            "Saved show"
        );
        gate.add_permits(1);
        let detail = read.await.unwrap().unwrap();
        assert_eq!(detail.media.display_title(), "Detail title");
        assert_eq!(detail.media.banner_image.as_deref(), Some("banner.jpg"));
        assert_eq!(get_media(1, &state).await.unwrap(), detail.media);
    }

    #[tokio::test]
    async fn offline_detail_keeps_newer_explicitly_empty_metadata() {
        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Saved show");
        media["staff"]["edges"] = json!([{
            "role":"Director","node":{"name":{"full":"Staff"},"image":null}
        }]);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":media}})),
            (200, json!({"data":{"Page":{"media":[anilist::mock_media(1)],"pageInfo":{"hasNextPage":false}}}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        let state = state(api);
        let first = get_media_detail(1, &state).await.unwrap();
        assert_eq!(first.media.display_title(), "Saved show");
        let newer = search_anime_page("test".into(), 1, &state).await.unwrap();
        assert_eq!(newer.items[0].display_title(), "#1");
        let cached = get_media_detail(1, &state).await.unwrap();
        assert!(cached.warning.is_some());
        assert_eq!(cached.media, newer.items[0]);
        assert_eq!(cached.staff[0].name, "Staff");
    }

    #[tokio::test]
    async fn delayed_detail_not_found_keeps_newer_search_metadata() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let missing = (404, json!({"errors":[{"message":"Not Found"}]}));
        let (api, mut requests) =
            anilist::mock_api_gated(vec![missing.clone()], Some(gate.clone())).await;
        let state = Arc::new(state(api));
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
        requests.recv().await.unwrap();
        let (mut api, _) = anilist::mock_api(vec![total_search(Some(12)), missing]).await;
        api.record_viewer(7);
        *state.anilist.lock() = api;
        let search = search_anime_page("test".into(), 1, &state).await.unwrap();
        gate.add_permits(1);

        let delayed = read.await.unwrap().unwrap();
        assert!(delayed.warning.is_some());
        assert_eq!(delayed.media, search.items[0]);
        assert_eq!(get_media(1, &state).await.unwrap(), delayed.media);
        assert_eq!(get_media_detail(1, &state).await.unwrap_err(), MEDIA_GONE);
        assert!(state.db.get_media(1).unwrap().is_none());
    }

    #[tokio::test]
    async fn delayed_lookup_not_found_keeps_a_newer_catalog_stub() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(404, json!({"errors":[{"message":"Not Found"}]}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media(1, &worker).await });
        requests.recv().await.unwrap();

        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Page":{"media":[anilist::mock_media(1)],"pageInfo":{"hasNextPage":false}}}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        *state.anilist.lock() = api;
        let search = search_anime_page("test".into(), 1, &state).await.unwrap();
        gate.add_permits(1);
        assert_eq!(read.await.unwrap().unwrap(), search.items[0]);
        let detail = get_media_detail(1, &state).await.unwrap();
        assert_eq!(detail.media, search.items[0]);
        assert!(detail.warning.is_some());
    }

    #[tokio::test]
    async fn delayed_detail_keeps_newer_sections_and_fills_unavailable_ones() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut older = anilist::mock_media(1);
        older["bannerImage"] = json!("stale.jpg");
        older["characters"]["edges"] = json!([{
            "role":"MAIN","node":{"name":{"full":"Character"},"image":null},"voiceActors":[]
        }]);
        older["staff"]["edges"] = json!([{
            "role":"Director","node":{"name":{"full":"Staff"},"image":null}
        }]);
        let mut newer = anilist::mock_media(1);
        newer["staff"] = json!(null);
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, json!({"data":{"Media":older}}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
        requests.recv().await.unwrap();
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":newer}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        *state.anilist.lock() = api;
        let fresh = get_media_detail(1, &state).await.unwrap();
        assert_eq!(fresh.unavailable_sections, ["staff"]);

        gate.add_permits(1);
        let delayed = read.await.unwrap().unwrap();
        let offline = get_media_detail(1, &state).await.unwrap();
        for detail in [delayed, offline] {
            assert_eq!(detail.media.banner_image, None);
            assert!(detail.characters.is_empty());
            assert!(detail.unavailable_sections.is_empty());
            assert_eq!(detail.staff[0].name, "Staff");
        }
    }

    #[tokio::test]
    async fn delayed_detail_enriches_when_newer_sections_were_not_cached() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut older = anilist::mock_media(1);
        older["staff"]["edges"] = json!([{
            "role":"Director","node":{"name":{"full":"Staff"},"image":null}
        }]);
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, json!({"data":{"Media":older}}))],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        cache_media_details(&state);
        let worker = state.clone();
        let read = tokio::spawn(async move { get_media_detail(1, &worker).await });
        requests.recv().await.unwrap();

        let mut newer = anilist::mock_media(1);
        newer["title"]["english"] = json!("New title");
        newer["bannerImage"] = json!("new-banner.jpg");
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":newer}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        *state.anilist.lock() = api;
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER skip_sections BEFORE INSERT ON settings
             WHEN NEW.key = 'media_detail:1' BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        let fresh = get_media_detail(1, &state).await.unwrap();
        assert!(fresh.staff.is_empty());
        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER skip_sections")
            .unwrap();
        let offline = get_media_detail(1, &state).await.unwrap();
        assert!(offline.warning.is_some());
        assert_eq!(
            offline.media.banner_image.as_deref(),
            Some("new-banner.jpg")
        );

        gate.add_permits(1);
        let delayed = read.await.unwrap().unwrap();
        let cached = get_media_detail(1, &state).await.unwrap();
        for detail in [delayed, cached] {
            assert_eq!(detail.media.display_title(), "New title");
            assert_eq!(detail.staff[0].name, "Staff");
        }
    }

    #[tokio::test]
    async fn delayed_search_preserves_a_lookup_total_and_fresh_search_can_replace_it() {
        for confirmed in [Some(13), Some(12), None] {
            let page = |total, title| {
                let mut media = anilist::mock_media(1);
                media["episodes"] = json!(total);
                media["title"]["english"] = json!(title);
                (
                    200,
                    json!({"data":{"Page":{"media":[media],"pageInfo":{"hasNextPage":false}}}}),
                )
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let stale = if confirmed == Some(12) { 11 } else { 12 };
            let (api, mut requests) = anilist::mock_api_gated(
                vec![page(Some(stale), "Search title")],
                Some(gate.clone()),
            )
            .await;
            let state = Arc::new(state(api));
            cache_media_details(&state);
            pending_progress(&state);
            let reading = state.clone();
            let read =
                tokio::spawn(async move { search_anime_page("test".into(), 1, &reading).await });
            requests.recv().await.unwrap();

            let (mut api, mut lookup) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":confirmed,"mediaListEntry":null}}}),
                ),
                page(Some(11), "Updated title"),
                page(None, "Unknown total"),
            ])
            .await;
            api.record_viewer(7);
            *state.anilist.lock() = api;
            crate::sync_queue::resolve_pending_change(1, false, &state)
                .await
                .unwrap();
            assert_eq!(lookup.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert_eq!(state.db.episode_total(1).unwrap(), confirmed);

            gate.add_permits(1);
            let delayed = read.await.unwrap().unwrap();
            assert_eq!(delayed.items[0].episodes, confirmed);
            let stored = state.db.get_media(1).unwrap().unwrap();
            assert_eq!(stored.episodes, confirmed);
            assert_eq!(stored.display_title(), "Search title");

            for expected in [Some(11), None] {
                let fresh = search_anime_page("test".into(), 1, &state).await.unwrap();
                assert_eq!(fresh.items[0].episodes, expected);
                assert_eq!(state.db.episode_total(1).unwrap(), expected);
            }
        }
    }

    fn total_search(total: Option<i64>) -> (u16, serde_json::Value) {
        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Search title");
        media["episodes"] = json!(total);
        (
            200,
            json!({"data":{"Page":{"media":[media],"pageInfo":{"hasNextPage":false}}}}),
        )
    }

    fn total_snapshot(total: Option<i64>) -> Vec<(u16, serde_json::Value)> {
        let mut media = anilist::mock_media(1);
        media["episodes"] = json!(total);
        media["title"]["english"] = json!("Snapshot title");
        let entry = json!({"id":22,"mediaId":1,"status":"CURRENT","progress":5,
                           "score":0,"repeat":0,"media":media});
        vec![
            (
                200,
                json!({"data":{"MediaListCollection":{"user":{"id":7},
                        "hasNextChunk":false,"lists":[{"entries":[entry]}]}}}),
            ),
            (
                200,
                json!({"data":{"Viewer":{"id":7,"name":"Tester",
                        "mediaListOptions":{"scoreFormat":null}}}}),
            ),
        ]
    }

    #[tokio::test]
    async fn delayed_snapshot_preserves_only_committed_search_totals() {
        for commit in [true, false] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(total_snapshot(Some(12)), Some(gate.clone())).await;
            let state = Arc::new(state(api));
            let worker = state.clone();
            let sync = tokio::spawn(async move { sync_my_list(&worker).await });
            assert_eq!(requests.recv().await.unwrap()["variables"]["accountId"], 7);

            let (mut api, _) = anilist::mock_api(vec![total_search(None)]).await;
            api.record_viewer(7);
            *state.anilist.lock() = api;
            if !commit {
                state.db.0.lock().execute_batch(
                "CREATE TEMP TRIGGER reject_catalog BEFORE INSERT ON media WHEN NEW.title_english = 'Search title'
                 BEGIN SELECT RAISE(ABORT, 'catalog failed'); END;"
            ).unwrap();
            }
            let search = search_anime_page("test".into(), 1, &state).await.unwrap();
            assert_eq!(search.items[0].episodes, None);
            gate.add_permits(2);

            let entries = sync.await.unwrap().unwrap();
            assert_eq!(entries[0].progress, 5);
            let expected = (!commit).then_some(12);
            assert_eq!(entries[0].media.as_ref().unwrap().episodes, expected);
            assert_eq!(
                entries[0].media.as_ref().unwrap().display_title(),
                if commit {
                    "Search title"
                } else {
                    "Snapshot title"
                }
            );
            assert_eq!(state.db.cached_episode_total(1).unwrap(), Some(expected));
        }
    }

    #[tokio::test]
    async fn delayed_search_preserves_only_committed_snapshot_totals() {
        for commit in [true, false] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![total_search(Some(11))], Some(gate.clone())).await;
            let state = Arc::new(state(api));
            cache_media_details(&state);
            let worker = state.clone();
            let read =
                tokio::spawn(async move { search_anime_page("test".into(), 1, &worker).await });
            requests.recv().await.unwrap();

            let (mut api, _) = anilist::mock_api(total_snapshot(Some(13))).await;
            api.record_viewer(7);
            *state.anilist.lock() = api;
            if !commit {
                state
                    .db
                    .0
                    .lock()
                    .execute_batch(
                        "CREATE TEMP TRIGGER reject_snapshot BEFORE INSERT ON list_entry
                         BEGIN SELECT RAISE(ABORT, 'snapshot failed'); END;",
                    )
                    .unwrap();
            }
            let expected = if commit {
                let entries = sync_my_list(&state).await.unwrap();
                assert_eq!(entries[0].media.as_ref().unwrap().episodes, Some(13));
                Some(13)
            } else {
                assert!(sync_my_list(&state)
                    .await
                    .unwrap_err()
                    .contains("snapshot failed"));
                assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
                Some(11)
            };
            gate.add_permits(1);
            let search = read.await.unwrap().unwrap();
            assert_eq!(search.items[0].episodes, expected);
            assert_eq!(
                search.items[0].display_title(),
                if commit {
                    "Snapshot title"
                } else {
                    "Search title"
                }
            );
            assert_eq!(state.db.episode_total(1).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn cached_detail_and_relations_use_current_totals_until_a_fresh_response() {
        let mut saved = anilist::mock_media(1);
        saved["title"]["english"] = json!("Saved show");
        saved["episodes"] = json!(12);
        let mut related = anilist::mock_media(2);
        related["type"] = json!("ANIME");
        related["episodes"] = json!(12);
        saved["relations"] = json!({"edges":[{"relationType":"SEQUEL","node":related}]});
        let mut partial = saved.clone();
        partial["relations"] = json!(null);
        let mut fresh = saved.clone();
        fresh["relations"]["edges"][0]["node"]["episodes"] = json!(24);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, json!({"data":{"Media":partial}})),
            (404, json!({"errors":[{"message":"Not Found"}]})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, json!({"data":{"Media":fresh}})),
        ])
        .await;
        let state = state(api);
        let original = get_media_detail_inner(1, &state).await.unwrap();
        assert_eq!(original.relations[0].media.episodes, Some(12));
        state.db.delete_media(1).unwrap();
        state.db.cache_episode_total(1, Some(13)).unwrap();
        state.db.cache_episode_total(2, None).unwrap();

        let detail = get_media_detail_inner(1, &state).await.unwrap();
        assert!(detail.warning.is_some());
        assert_eq!(detail.media.display_title(), "Saved show");
        assert_eq!(detail.media.episodes, Some(13));
        assert_eq!(detail.relations[0].media.episodes, None);

        let partial = get_media_detail_inner(1, &state).await.unwrap();
        assert!(partial.unavailable_sections.contains(&"relations".into()));
        assert_eq!(partial.relations[0].media.episodes, None);
        state.anilist.lock().set_token(None);
        assert_eq!(get_media_detail(2, &state).await.unwrap_err(), MEDIA_GONE);
        assert!(get_media_detail(1, &state)
            .await
            .unwrap()
            .relations
            .is_empty());
        let fresh = get_media_detail_inner(1, &state).await.unwrap();
        assert_eq!(fresh.relations[0].media.episodes, Some(24));
        assert_eq!(state.db.episode_total(2).unwrap(), Some(24));
    }

    #[tokio::test]
    async fn omitted_detail_fields_preserve_only_the_affected_cached_sections() {
        let mut saved = anilist::mock_media(1);
        saved["title"]["english"] = json!("Saved show");
        saved["characters"]["edges"] = json!([{
            "role":"MAIN",
            "node":{"name":{"full":"Character"},"image":null},
            "voiceActors":[{"name":{"full":"Actor"},"image":{"medium":"actor.jpg"}}]
        }]);
        saved["staff"]["edges"] = json!([{
            "role":"Director","node":{"name":{"full":"Staff"},"image":null}
        }]);
        let mut partial = saved.clone();
        partial["title"]["english"] = json!("Fresh show");
        partial["characters"]["edges"][0]
            .as_object_mut()
            .unwrap()
            .remove("voiceActors");
        partial["staff"]["edges"][0]["role"] = json!("Producer");
        let mut recovered = partial.clone();
        recovered["characters"]["edges"][0]["voiceActors"] = json!(null);
        recovered["staff"]["edges"] = json!([]);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (200, json!({"data":{"Media":partial}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, json!({"data":{"Media":recovered}})),
        ])
        .await;
        let state = state(api);
        get_media_detail(1, &state).await.unwrap();
        for warning in [false, true] {
            let detail = get_media_detail(1, &state).await.unwrap();
            assert_eq!(detail.warning.is_some(), warning);
            assert_eq!(detail.media.display_title(), "Fresh show");
            assert_eq!(detail.unavailable_sections, ["characters"]);
            assert_eq!(detail.characters[0].va_name.as_deref(), Some("Actor"));
            assert_eq!(detail.staff[0].role.as_deref(), Some("Producer"));
        }
        let recovered = get_media_detail(1, &state).await.unwrap();
        assert!(recovered.unavailable_sections.is_empty());
        assert_eq!(recovered.characters[0].va_name, None);
        assert!(recovered.staff.is_empty());
    }

    #[tokio::test]
    async fn detail_cache_keeps_credits_when_base_metadata_is_null() {
        let mut saved = anilist::mock_media(1);
        saved["staff"]["edges"] = json!([{
            "role":null,"node":{"name":{"full":"Staff"},"image":{"medium":null}}
        }]);
        let mut partial = saved.clone();
        partial["staff"] = json!(null);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (200, json!({"data":{"Media":partial}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        let state = state(api);
        for _ in 0..3 {
            let detail = get_media_detail(1, &state).await.unwrap();
            assert_eq!(detail.staff.len(), 1);
            assert_eq!(detail.staff[0].name, "Staff");
        }
    }

    #[tokio::test]
    async fn detail_settlement_does_not_outlive_its_session() {
        use std::time::Duration;

        let mut media = anilist::mock_media(1);
        media["title"]["english"] = json!("Late show");
        for response in [
            (200, json!({"data":{"Media":media}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
            let state = Arc::new(state(api));
            cache_media_details(&state);
            let stored = state.db.get_setting("media_detail:1").unwrap();
            let lock = state.entry_lock.lock().await;
            let baseline = state.auth_intent.receiver_count();
            let worker = state.clone();
            let task = tokio::spawn(async move { get_media_detail(1, &worker).await });
            tokio::time::timeout(Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(state.auth_intent.receiver_count() > baseline + 1);
            gate.add_permits(1);
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.auth_intent.receiver_count() > baseline + 1 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            state.begin_auth_intent();
            drop(lock);
            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
            assert_eq!(state.db.get_setting("media_detail:1").unwrap(), stored);
        }
    }

    #[tokio::test]
    async fn calendar_read_cancels_when_its_session_is_replaced() {
        use std::time::Duration;

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(
                200,
                json!({"data":{"Page":{
                    "pageInfo":{"hasNextPage":false},"airingSchedules":[]
                }}}),
            )],
            Some(gate.clone()),
        )
        .await;
        let state = Arc::new(state(api));
        let worker = state.clone();
        let mut task = tokio::spawn(async move { get_airing_schedule(100, 200, &worker).await });
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        state.begin_auth_intent();
        let result = tokio::time::timeout(Duration::from_millis(750), &mut task).await;
        gate.add_permits(1);
        assert_eq!(
            result
                .expect("Calendar read waited for the old session")
                .unwrap()
                .unwrap_err(),
            AUTH_REPLACED
        );
    }

    #[tokio::test]
    async fn invalid_calendar_page_keeps_cached_media_until_a_complete_retry() {
        for invalid in ["outside", "episode", "media", "adult"] {
            let mut media = anilist::mock_media(1);
            media["isAdult"] = json!(null);
            media["episodes"] = json!(99);
            let first = json!({"airingAt":100,"episode":1,"media":media});
            let mut second = json!({"airingAt":199,"episode":2,"media":media});
            let mut invalid_row = second.clone();
            match invalid {
                "outside" => invalid_row["airingAt"] = json!(200),
                "episode" => invalid_row["episode"] = json!(-1),
                "media" => invalid_row["media"]["id"] = json!(0),
                "adult" => {
                    invalid_row["media"]
                        .as_object_mut()
                        .unwrap()
                        .remove("isAdult");
                }
                _ => unreachable!(),
            }
            second["media"]["episodes"] = json!(13);
            second["episode"] = json!(0);
            let page = |number, more, rows| {
                (
                    200,
                    json!({"data":{"Page":{
                        "pageInfo":{"currentPage":number,"hasNextPage":more},
                        "airingSchedules":rows
                    }}}),
                )
            };
            let (api, _) = anilist::mock_api(vec![
                page(1, true, vec![first]),
                page(2, false, vec![invalid_row]),
                page(1, false, vec![second]),
            ])
            .await;
            let state = state(api);
            cache_media_details(&state);
            pending_progress(&state);

            assert!(
                get_airing_schedule(100, 200, &state).await.is_err(),
                "{invalid}"
            );
            assert_eq!(state.db.episode_total(1).unwrap(), Some(12), "{invalid}");
            let items = get_airing_schedule(100, 200, &state).await.unwrap();
            assert_eq!(items[0].episode, 0);
            assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
        }
    }

    #[tokio::test]
    async fn calendar_rejects_ranges_that_overflow() {
        let state = super::tests::test_state();
        assert_eq!(
            get_airing_schedule(i64::MIN, i64::MAX, &state)
                .await
                .unwrap_err(),
            "invalid schedule range"
        );
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
        let mut current = anilist::mock_media(1);
        current["title"] = json!({"romaji":null,"english":"Fixture Title","native":null});
        let mut saved = current.clone();
        saved["episodes"] = json!(12);
        saved["bannerImage"] = json!("saved.jpg");
        saved["genres"] = json!(["Action"]);
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":saved}})),
            (200, json!({"data":{"Media":current}})),
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
                    json!({"data":{"User":{"id":7,"statistics":{"anime":complete}}}}),
                ),
                (
                    200,
                    json!({"data":{"User":{"id":7,"statistics":{"anime":incomplete}}}}),
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

    fn state(mut api: AniList) -> AppState {
        api.record_viewer(7);
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
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
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
        assert_eq!(state.db.episode_total(1).unwrap(), Some(1));
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
        let complete = json!({"id":22,"mediaId":1,"notes":"Remote note",
            "startedAt":{"year":2024,"month":1,"day":2},
            "completedAt":{"year":2024,"month":2,"day":3},"customLists":{}});
        for missing in [
            "id",
            "mediaId",
            "wrong media",
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
                if missing == "wrong media" {
                    incomplete["mediaId"] = json!(2);
                } else if let Some((parent, field)) = missing.split_once('.') {
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
                let saved = serde_json::to_string(&CachedEntryDetails {
                    entry_id: 22,
                    details: cached.clone(),
                })
                .unwrap();
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
    async fn unconfirmed_metadata_save_drops_cache_for_a_replaced_entry() {
        let replacement = json!({"id":33,"mediaId":1,"notes":"Replacement note",
            "startedAt":null,"completedAt":null,"customLists":{}});
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":replacement}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":replacement}})),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ])
        .await;
        let state = state(api);
        pending_progress(&state);
        cache_entry_details(
            &state.db,
            7,
            1,
            22,
            &EntryDetails {
                notes: "Removed entry note".into(),
                ..Default::default()
            },
        );

        let error =
            update_entry_details_inner(1, Some("Requested note".into()), None, None, None, &state)
                .await
                .unwrap_err();
        assert!(error.contains("did not confirm"), "{error}");
        let cached = read_cached_entry_details(&state.db, "entry_details:7:1").unwrap();
        assert_eq!(
            (cached.entry_id, cached.details.notes.as_str()),
            (33, "Replacement note")
        );
        assert!(get_entry_details_inner(1, &state).await.is_err());
        requests.try_recv().unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 33);
        requests.try_recv().unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn metadata_updates_target_the_preflight_entry_identity() {
        for outcome in ["saved", "missing lookup ID", "removed", "wrong saved ID"] {
            let mut lookup = json!({"id":22,"mediaId":1,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}});
            if outcome == "missing lookup ID" {
                lookup.as_object_mut().unwrap().remove("id");
            }
            let mut saved = lookup.clone();
            saved["notes"] = json!("New note");
            if outcome == "wrong saved ID" {
                saved["id"] = json!(33);
            }
            let response = if outcome == "removed" {
                (
                    404,
                    json!({"errors":[{"message":"Not Found","status":404}]}),
                )
            } else {
                (200, json!({"data":{"SaveMediaListEntry":saved}}))
            };
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"Media":{"mediaListEntry":lookup},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
                response,
            ]).await;
            let state = state(api);
            pending_progress(&state);
            let cached = (outcome != "missing lookup ID").then(|| {
                let details = serde_json::to_string(&EntryDetails {
                    notes: "Old note".into(),
                    ..Default::default()
                })
                .unwrap();
                state.db.set_setting("entry_details:7:1", &details).unwrap();
                details
            });
            let result =
                update_entry_details_inner(1, Some("New note".into()), None, None, None, &state)
                    .await;
            let lookup = requests.try_recv().unwrap();
            assert!(lookup["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry { id mediaId"));
            if outcome != "missing lookup ID" {
                let mutation = requests.try_recv().unwrap();
                assert_eq!(mutation["variables"]["id"], 22);
                let query = mutation["query"].as_str().unwrap();
                assert!(query.contains("SaveMediaListEntry(id: $id"));
                assert!(query.contains("id mediaId notes"));
                assert!(!query.contains("mediaId:"));
            }
            assert!(requests.try_recv().is_err());
            assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
            assert!(state.anilist.lock().has_token());
            if outcome == "saved" {
                assert_eq!(result.unwrap().notes, "New note");
            } else {
                assert!(result.is_err(), "{outcome}");
                let expected = if outcome == "removed" { None } else { cached };
                assert_eq!(state.db.get_setting("entry_details:7:1").unwrap(), expected);
            }
        }
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
                responses.push((200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"mediaId":1,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})));
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
                (200, json!({"data":{"Media":{"mediaListEntry":{"id":22,"mediaId":1,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})),
                (status, error.clone()),
                (status, error.clone()),
                (status, error),
            ]).await;
            let state = state(api);
            pending_progress(&state);
            let cached = serde_json::to_string(&CachedEntryDetails {
                entry_id: 22,
                details: EntryDetails {
                    notes: "Cached note".into(),
                    ..Default::default()
                },
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
    async fn offline_profile_recovers_statistics_and_entry_details_with_a_known_entry_id() {
        let (api, _) = anilist::mock_api(vec![
            (200, json!({"data":{"User":{"id":7,"statistics":null}}})),
            (
                503,
                json!({"errors":[{"message":"Temporarily unavailable"}]}),
            ),
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
        pending_progress(&state);
        cache_entry_details(&state.db, 7, 1, 22, &details);
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

    #[tokio::test]
    async fn offline_entry_details_require_the_same_remote_entry_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "kurisu-entry-details-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let offline = (
            503,
            json!({"errors":[{"message":"Temporarily unavailable"}]}),
        );
        let details = json!({"data":{"Media":{"mediaListEntry":{
            "id":22,"mediaId":1,"notes":"Old entry notes",
            "startedAt":null,"completedAt":null,"customLists":null
        }}}});
        let mut snapshot = score_snapshot(70.0, false, 1);
        snapshot["data"]["MediaListCollection"]["lists"][0]["entries"][0]["id"] = json!(99);
        let (api, _) = anilist::mock_api(vec![offline.clone(), (200, details)]).await;
        let mut first = state(api);
        let db = Db::open(&path).unwrap();
        db.replace_account("test-session", &first.user.lock().clone().unwrap())
            .unwrap();
        db.upsert_entry(&first.db.get_entry(1).unwrap().unwrap())
            .unwrap();
        db.set_setting("entry_details:7:1", r#"{"notes":"Legacy notes"}"#)
            .unwrap();
        Arc::get_mut(&mut first).unwrap().db = Arc::new(db);
        assert!(get_entry_details(1, &first).await.is_err());
        assert_eq!(
            get_entry_details(1, &first).await.unwrap().notes,
            "Old entry notes"
        );
        drop(first);

        let (api, _) = anilist::mock_api(vec![
            offline.clone(),
            (200, snapshot),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let mut restarted = state(api);
        Arc::get_mut(&mut restarted).unwrap().db = Arc::new(Db::open(&path).unwrap());
        restarted
            .anilist
            .lock()
            .set_token(Some("test-session".into()));
        *restarted.user.lock() = None;
        let cached = get_entry_details(1, &restarted).await.unwrap();
        assert_eq!(cached.notes, "Old entry notes");
        assert!(cached.warning.is_some());
        assert_eq!(sync_my_list(&restarted).await.unwrap()[0].id, Some(99));
        drop(restarted);

        let (api, _) = anilist::mock_api(vec![offline]).await;
        let mut replaced = state(api);
        Arc::get_mut(&mut replaced).unwrap().db = Arc::new(Db::open(&path).unwrap());
        assert_eq!(replaced.db.get_entry(1).unwrap().unwrap().id, Some(99));
        assert!(get_entry_details(1, &replaced)
            .await
            .unwrap_err()
            .contains("503"));
        drop(replaced);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_offline_read_cannot_recover_details_after_confirmed_entry_removal() {
        for (removed, block_delete) in [
            (
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
                ),
                false,
            ),
            (
                (
                    404,
                    json!({"errors":[{"message":"Not Found","status":404}]}),
                ),
                true,
            ),
        ] {
            let (api, _) = anilist::mock_api(vec![
                removed,
                (
                    503,
                    json!({"errors":[{"message":"Temporarily unavailable"}]}),
                ),
            ])
            .await;
            let state = state(api);
            cache_entry_details(
                &state.db,
                7,
                1,
                22,
                &EntryDetails {
                    notes: "Old entry notes".into(),
                    ..Default::default()
                },
            );
            cache_entry_details(
                &state.db,
                8,
                1,
                23,
                &EntryDetails {
                    notes: "Other account notes".into(),
                    ..Default::default()
                },
            );
            if block_delete {
                state
                    .db
                    .0
                    .lock()
                    .execute_batch(
                        "CREATE TRIGGER refuse_cache_delete BEFORE DELETE ON settings
                     WHEN OLD.key = 'entry_details:7:1' BEGIN SELECT RAISE(IGNORE); END;",
                    )
                    .unwrap();
            }
            assert!(get_entry_details(1, &state).await.is_err());
            assert!(get_entry_details(1, &state)
                .await
                .unwrap_err()
                .contains("503"));
            assert_eq!(
                read_cached_entry_details(&state.db, "entry_details:8:1")
                    .unwrap()
                    .details
                    .notes,
                "Other account notes"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_cache_write_cannot_restore_notes_for_a_replaced_remote_entry() {
        let (api, _) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"mediaListEntry":{
                    "id":99,"mediaId":1,"notes":"New entry notes",
                    "startedAt":null,"completedAt":null,"customLists":null
                }}}}),
            ),
            (
                503,
                json!({"errors":[{"message":"Temporarily unavailable"}]}),
            ),
        ])
        .await;
        let state = state(api);
        cache_entry_details(
            &state.db,
            7,
            1,
            22,
            &EntryDetails {
                notes: "Old entry notes".into(),
                ..Default::default()
            },
        );
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER refuse_cache_write BEFORE INSERT ON settings
             WHEN NEW.key = 'entry_details:7:1' BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();
        assert_eq!(
            get_entry_details(1, &state).await.unwrap().notes,
            "New entry notes"
        );
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(22));
        assert!(get_entry_details(1, &state)
            .await
            .unwrap_err()
            .contains("503"));
    }

    fn state(mut api: AniList) -> Arc<AppState> {
        api.record_viewer(7);
        let user = User {
            id: 7,
            name: "Tester".into(),
            score_format: Some("POINT_100".into()),
            ..Default::default()
        };
        let state = Arc::new(AppState {
            anilist: Mutex::new(api),
            db: Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap()),
            user: Mutex::new(Some(user.clone())),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
        });
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
    async fn incompatible_progress_and_status_keep_the_edit_and_stop_repeated_overrides() {
        for (cached, before_status, status, progress, reply_status) in [
            (None, "COMPLETED", None, Some(13), "COMPLETED"),
            (Some(13), "COMPLETED", None, Some(13), "COMPLETED"),
            (Some(24), "COMPLETED", None, Some(13), "COMPLETED"),
            (
                Some(12),
                "COMPLETED",
                Some("CURRENT"),
                Some(12),
                "COMPLETED",
            ),
            (None, "COMPLETED", Some("CURRENT"), None, "COMPLETED"),
            (None, "CURRENT", None, Some(13), "COMPLETED"),
            (None, "CURRENT", None, Some(13), "CURRENT"),
        ] {
            let mut reply = remote();
            reply["status"] = json!(reply_status);
            let lookup = (
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":reply}}}),
            );
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"SaveMediaListEntry":reply}})),
                lookup.clone(),
                lookup,
            ])
            .await;
            let state = state(api);
            state.db.delete_media(1).unwrap();
            if let Some(cached) = cached {
                state.db.cache_episode_total(1, Some(cached)).unwrap();
            }
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = before_status.into();
            base.progress = if before_status == "CURRENT" { 11 } else { 12 };
            state.db.upsert_entry(&base).unwrap();

            let entry =
                save_entry_inner(&state, 1, status.map(str::to_owned), progress, None, None)
                    .await
                    .unwrap();
            assert_eq!(entry.progress, progress.unwrap_or(base.progress));
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict);
            assert_eq!(pending.patch.progress, progress);
            assert_eq!(
                pending.patch.status.as_deref(),
                if cached == Some(24) {
                    Some("CURRENT")
                } else {
                    status
                }
            );
            assert_eq!(
                entry.status,
                pending.patch.status.as_deref().unwrap_or(before_status)
            );
            assert_eq!(pending.base.unwrap().progress, base.progress);
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .starts_with("mutation"));

            for _ in 0..2 {
                crate::sync_queue::resolve_pending_change(1, true, &state)
                    .await
                    .unwrap_err();
                assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
                assert!(requests.try_recv().is_err());
                let retained = state.db.pending_change(7, 1).unwrap().unwrap();
                assert!(retained.conflict);
                assert_eq!(retained.patch.progress, progress);
            }
        }
    }

    #[tokio::test]
    async fn list_sync_continues_after_a_conflicting_reply_and_a_new_total_allows_an_override() {
        use crate::sync_queue::{resolve_pending_change, stage, Patch};

        let mut second = remote();
        second["id"] = json!(23);
        second["status"] = json!("CURRENT");
        second["progress"] = json!(1);
        second["mediaId"] = json!(2);
        let mut snapshot = score_snapshot(70.0, false, 1);
        snapshot["data"]["MediaListCollection"]["lists"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .push(second.clone());
        let mut expanded = remote();
        expanded["progress"] = json!(13);
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":null,"mediaListEntry":remote()}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":remote()}})),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote()}}}),
            ),
            (
                200,
                json!({"data":{"Media":{"id":2,"episodes":20,"mediaListEntry":null}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":second}})),
            (200, snapshot),
            (200, score_profile("POINT_100")),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":remote()}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":expanded}})),
        ])
        .await;
        let state = state(api);
        state.db.delete_media(1).unwrap();
        for (media_id, progress) in [(1, 13), (2, 1)] {
            stage(
                &state,
                media_id,
                Patch {
                    status: (media_id == 1).then(|| "COMPLETED".into()),
                    progress: Some(progress),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let entries = sync_my_list(&state).await.unwrap();
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].conflict);
        assert_eq!(pending[0].patch.progress, Some(13));
        assert_eq!(pending[0].rejected_progress.as_ref().unwrap().total, None);
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.media_id == 1)
                .unwrap()
                .progress,
            13
        );
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.media_id == 2)
                .unwrap()
                .progress,
            1
        );
        assert_eq!(state.db.episode_total(2).unwrap(), Some(20));
        for field in [
            "mediaListEntry",
            "SaveMediaListEntry",
            "mediaListEntry",
            "mediaListEntry",
            "SaveMediaListEntry",
            "MediaListCollection",
            "Viewer",
        ] {
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains(field));
        }

        resolve_pending_change(1, true, &state).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 13);
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 13);
    }

    #[tokio::test]
    async fn a_cold_merge_keeps_the_reopen_inferred_from_legacy_progress() {
        use crate::sync_queue::{stage, Patch};

        for reply in [
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":null,"mediaListEntry":remote()}}}),
            ),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
        ] {
            let (api, mut requests) = anilist::mock_api(vec![reply]).await;
            let state = state(api);
            state.db.delete_media(1).unwrap();
            stage(
                &state,
                1,
                Patch {
                    progress: Some(11),
                    ..Default::default()
                },
            )
            .unwrap();
            state.db.clear_entries().unwrap();

            let saved = save_entry_inner(&state, 1, None, Some(12), None, None)
                .await
                .unwrap();
            assert_eq!((saved.status.as_str(), saved.progress), ("CURRENT", 12));
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(pending.patch.status.as_deref(), Some("CURRENT"));
            assert_eq!(pending.patch.progress, Some(12));
            let base = pending.base.unwrap();
            assert_eq!(
                (base.id, base.status.as_str(), base.progress),
                (Some(22), "COMPLETED", 12)
            );
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn changed_completion_progress_requires_a_matching_remote_total_and_entry() {
        let completed = Some("COMPLETED");
        let matching = Some((22, "COMPLETED", 13));
        // A pending change can either retry or require conflict resolution.
        for (status, reported, readback, pending_conflict) in [
            (completed, Some(Some(13)), matching, None),
            (completed, Some(Some(12)), matching, Some(true)),
            (completed, Some(None), matching, Some(true)),
            (completed, None, matching, Some(true)),
            (completed, Some(Some(0)), matching, Some(true)),
            (
                completed,
                Some(Some(13)),
                Some((22, "COMPLETED", 12)),
                Some(false),
            ),
            (
                completed,
                Some(Some(13)),
                Some((23, "COMPLETED", 13)),
                Some(false),
            ),
            (completed, None, Some((22, "CURRENT", 13)), Some(false)),
            (completed, None, None, Some(false)),
            (None, Some(Some(13)), matching, Some(true)),
        ] {
            let mut saved = remote();
            saved["progress"] = json!(13);
            let readback = readback.map(|(id, status, progress)| {
                let mut entry = saved.clone();
                entry["id"] = json!(id);
                entry["status"] = json!(status);
                entry["progress"] = json!(progress);
                entry
            });
            let mut media = json!({"id":1,"mediaListEntry":readback});
            if let Some(total) = reported {
                media["episodes"] = json!(total);
            }
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (200, json!({"data":{"Media":media}})),
            ])
            .await;
            let state = state(api);
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    title_english: Some("Fixture Title".into()),
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();

            let entry = save_entry_inner(&state, 1, status.map(String::from), Some(12), None, None)
                .await
                .unwrap();

            let mutation = requests.try_recv().unwrap();
            assert_eq!(mutation["variables"]["progress"], 12);
            assert_eq!(mutation["variables"]["status"].as_str(), status);
            if status.is_some() {
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            }
            assert!(requests.try_recv().is_err());
            let expected_total = if status.is_some() {
                reported.unwrap_or(Some(12)).filter(|total| *total > 0)
            } else {
                Some(12)
            };
            let expected = expected_total.unwrap_or(12);
            assert_eq!(
                (entry.status.as_str(), entry.progress),
                ("COMPLETED", expected)
            );
            assert_eq!(entry.media.unwrap().episodes, expected_total);
            let cached = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(cached.progress, expected);
            assert_eq!(cached.media.unwrap().episodes, expected_total);
            let pending = state.db.pending(7).unwrap();
            if let Some(conflict) = pending_conflict {
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].conflict, conflict);
                assert!(pending[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("did not confirm"));
            } else {
                assert!(pending.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn decrements_reopen_completed_entries_without_an_episode_total() {
        for replay in [false, true] {
            let mut replies = Vec::new();
            if replay {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":null,"mediaListEntry":remote()}}}),
                ));
            }
            replies.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api);
            state.db.delete_media(1).unwrap();
            if replay {
                crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        progress: Some(11),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                let changes = crate::sync_queue::sync_pending_changes(&state)
                    .await
                    .unwrap();
                assert!(changes[0].conflict);
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .starts_with("query"));
            } else {
                let local =
                    save_entry_inner_checked(&state, 1, None, Some(11), None, None, Some(12))
                        .await
                        .unwrap();
                assert_eq!((local.status.as_str(), local.progress), ("CURRENT", 11));
            }
            let mutation = requests.try_recv().unwrap();
            assert_eq!(mutation["variables"]["status"], "CURRENT");
            assert_eq!(mutation["variables"]["progress"], 11);
            assert!(requests.try_recv().is_err());
            let pending = state.db.pending(7).unwrap().remove(0);
            assert_eq!(pending.patch.progress, Some(11));
            assert!(pending.conflict);
            assert!(pending.error.unwrap().contains("did not confirm"));
        }
    }

    #[tokio::test]
    async fn a_total_only_cache_preserves_pending_metadata() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Fixture Title".into()),
                ..Default::default()
            })
            .unwrap();
        crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(11),
                ..Default::default()
            },
        )
        .unwrap();
        state.db.delete_media(1).unwrap();
        state.db.cache_episode_total(1, Some(12)).unwrap();

        assert!(state.db.get_media(1).unwrap().is_none());
        assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
        assert!(state.db.entries_with_media().unwrap()[0].media.is_none());
        let mut pending = state.db.pending_change(7, 1).unwrap().unwrap();
        for deleting in [false, true] {
            pending.deleting = deleting;
            state.db.put_pending(7, &pending).unwrap();
            for entry in [
                get_entry(1, &state).await.unwrap().unwrap(),
                local_entries(&state).await.unwrap().remove(0),
            ] {
                let media = entry.media.unwrap();
                assert_eq!(media.display_title(), "Fixture Title");
                assert_eq!(media.episodes, Some(12));
            }
        }
        let summary = crate::sync_queue::get_pending_changes(&state)
            .await
            .unwrap();
        assert_eq!(summary[0].title, "Fixture Title");
        assert!(get_media_detail(1, &state).await.is_err());
    }

    #[tokio::test]
    async fn cold_edit_reopens_the_projected_pending_completion() {
        for offline in [false, true] {
            let mut remote = remote();
            remote["status"] = json!("CURRENT");
            remote["progress"] = json!(4);
            let reply = if offline {
                (503, json!({"errors":[{"message":"Unavailable"}]}))
            } else {
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":remote}}}),
                )
            };
            let (api, mut requests) = anilist::mock_api(vec![reply]).await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "CURRENT".into();
            base.progress = 4;
            state.db.upsert_entry(&base).unwrap();
            crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    status: Some("COMPLETED".into()),
                    progress: Some(12),
                    repeat: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
            state.db.clear_entries().unwrap();

            let saved = save_entry_inner(&state, 1, None, Some(5), None, None)
                .await
                .unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                ("CURRENT", 5, 3)
            );
            let pending = state.db.pending(7).unwrap().remove(0);
            assert_eq!(pending.base.unwrap().progress, 4);
            assert_eq!(pending.patch.status.as_deref(), Some("CURRENT"));
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 5);
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .starts_with("query"));
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn completion_readback_cannot_settle_after_a_new_auth_intent() {
        let mut saved = remote();
        saved["progress"] = json!(13);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":saved}}}),
                ),
            ],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let editing = state.clone();
        let mut task = tokio::spawn(async move {
            save_entry_inner(&editing, 1, Some("COMPLETED".into()), None, None, None).await
        });
        identity_request_while_running(&mut requests, &mut task).await;
        gate.add_permits(1);
        identity_request_while_running(&mut requests, &mut task).await;
        state.cancel_login();
        gate.add_permits(1);

        assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].conflict);
    }

    #[tokio::test]
    async fn completion_readback_errors_keep_the_mutation_pending() {
        for (status, message) in [(401, "Invalid token"), (404, "Not Found")] {
            let mut saved = remote();
            saved["progress"] = json!(13);
            let (api, _) = anilist::mock_api(vec![
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (status, json!({"errors":[{"message":message}]})),
            ])
            .await;
            let state = state(api);

            let result =
                save_entry_inner(&state, 1, Some("COMPLETED".into()), None, None, None).await;

            if status == 401 {
                assert_eq!(result.unwrap_err(), SESSION_EXPIRED);
                assert!(!state.anilist.lock().has_token());
            } else {
                assert_eq!(result.unwrap().progress, 12);
                assert!(state.anilist.lock().has_token());
            }
            assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
            let pending = state.db.pending(7).unwrap().remove(0);
            assert!(!pending.conflict);
            assert!(!pending.missing_media);
            assert_eq!(pending.patch.progress, Some(12));
        }
    }

    #[tokio::test]
    async fn completion_uses_the_episode_total_from_a_cold_lookup() {
        for (existing, confirmed) in [(true, true), (false, true), (true, false)] {
            let mut before = remote();
            before["status"] = json!("CURRENT");
            before["progress"] = json!(3);
            let mut after = remote();
            if !confirmed {
                after["progress"] = json!(3);
            }
            let mut replies = vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":existing.then_some(before)}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":after}})),
            ];
            if !confirmed {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":after}}}),
                ));
            }
            replies.push((200, json!({"data":{"Media":{
                    "id":1,"idMal":null,"title":{"romaji":null,"english":"Some Show","native":null},
                    "coverImage":null,"episodes":12,"format":"TV","status":"FINISHED",
                    "averageScore":null,"season":null,"seasonYear":null,"description":null,"nextAiringEpisode":null
            }}})));
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api);
            state.db.delete_media(1).unwrap();
            state.db.delete_entry(1).unwrap();
            let saved = save_entry_inner(&state, 1, Some("COMPLETED".into()), None, None, None)
                .await
                .unwrap();
            requests.try_recv().unwrap();
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
            assert_eq!((saved.status.as_str(), saved.progress), ("COMPLETED", 12));
            assert_eq!(state.db.pending(7).unwrap().is_empty(), confirmed);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 12);
            assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
            if !confirmed {
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            }
            assert_eq!(
                get_media(1, &state).await.unwrap().display_title(),
                "Some Show"
            );
            assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 1);
            assert_eq!(
                get_media(1, &state).await.unwrap().display_title(),
                "Some Show"
            );
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn cold_save_stops_when_the_lookup_outlives_its_auth_intent() {
        for status in [200, 503, 400] {
            let response = if status == 200 {
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":remote()}}})
            } else {
                json!({"errors":[{"message":"Cold lookup failed"}]})
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![(status, response)], Some(gate.clone())).await;
            let state = state(api);
            state.db.delete_media(1).unwrap();
            state.db.delete_entry(1).unwrap();
            let mut events = state.events.subscribe();
            let old = state.origin();
            let editing = state.clone();
            let mut task = tokio::spawn(async move {
                save_entry_inner(&editing, 1, Some("CURRENT".into()), Some(4), None, None).await
            });
            let lookup = identity_request_while_running(&mut requests, &mut task).await;
            assert!(lookup["query"].as_str().unwrap().contains("mediaListEntry"));
            state.cancel_login();
            assert_eq!(state.origin().account_id, old.account_id);
            assert!(!state.origin_is_current(old));
            gate.add_permits(2);

            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
            assert!(requests.try_recv().is_err());
            assert!(events.try_recv().is_err());
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_media(1).unwrap().is_none());
            assert!(state.db.get_entry(1).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn retryable_cold_lookup_keeps_the_edit_for_sync() {
        let (api, mut requests) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();
        let saved = save_entry_inner(&state, 1, Some("CURRENT".into()), Some(4), None, None)
            .await
            .unwrap();
        assert_eq!((saved.status.as_str(), saved.progress), ("CURRENT", 4));
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn entry_read_after_adoption_applies_the_pending_edit() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api);
        crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                score: Some(80.0),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        state.db.delete_setting(USER_ID_KEY).unwrap();
        let write = state.entry_lock.lock().await;
        let read = get_entry(1, &state);
        tokio::pin!(read);
        tokio::select! {
            biased;
            result = &mut read => panic!("Read finished before adoption: {result:?}"),
            () = std::future::ready(()) => {},
        }
        state.db.set_setting(USER_ID_KEY, "7").unwrap();
        drop(write);
        assert_eq!(read.await.unwrap().unwrap().score, Some(80.0));
    }

    #[tokio::test]
    async fn list_sync_stops_before_sending_a_queued_edit_after_auth_intent_changes() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":remote()}}}),
            )],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(11),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        let syncing = state.clone();
        let mut task = tokio::spawn(async move { sync_my_list(&syncing).await });
        identity_request_while_running(&mut requests, &mut task).await;
        state.cancel_login();
        gate.add_permits(1);
        assert!(task.await.unwrap().is_err());
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(11));
    }

    #[tokio::test]
    async fn successful_prompt_noops_retire_the_confirmed_prompt() {
        for queued in [false, true] {
            let (api, _) = anilist::mock_api(vec![]).await;
            let state = state(api);
            state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
                media_id: 1,
                episode: 13,
                title: "Some Show".into(),
                raw_title: "Some Show - 13".into(),
                progress: 12,
            }));
            let event = state.retained_tracking_prompt().unwrap();
            if queued {
                state.db.cache_episode_total(1, Some(13)).unwrap();
                crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        progress: Some(13),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(get_entry(1, &state).await.unwrap().unwrap().progress, 13);
            } else {
                state.db.delete_entry(1).unwrap();
            }
            assert!(state.event_is_current(&event));
            confirm_tracking_prompt(event, 13, &state).await.unwrap();
            assert!(state.last_tracking_prompt.lock().is_none());
        }
    }

    #[tokio::test]
    async fn prompt_checks_remote_progress_before_sending() {
        let mut advanced = remote();
        advanced["progress"] = json!(8);
        advanced["status"] = json!("CURRENT");
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":advanced}}}),
        )])
        .await;
        let state = state(api);
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.progress = 2;
        base.status = "CURRENT".into();
        state.db.upsert_entry(&base).unwrap();
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 3,
            progress: 2,
            title: "Fixture".into(),
            raw_title: "Fixture 3".into(),
        }));

        let result =
            confirm_tracking_prompt(state.retained_tracking_prompt().unwrap(), 3, &state).await;
        let request = requests.try_recv().unwrap();
        assert!(
            request["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"),
            "{request}"
        );
        assert!(requests.try_recv().is_err());
        result.unwrap();
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.conflict);
        assert_eq!(pending.base.unwrap().progress, 2);
        assert!(state.retained_tracking_prompt().is_none());
    }

    #[tokio::test]
    async fn prompt_preflight_rejects_a_changed_displayed_target() {
        let mut before = remote();
        before["progress"] = json!(11);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":before}}}),
        )])
        .await;
        let state = state(api);
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.progress = 11;
        state.db.upsert_entry(&base).unwrap();
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 13,
            progress: 11,
            title: "Fixture".into(),
            raw_title: "Fixture 13".into(),
        }));
        let event = state.retained_tracking_prompt().unwrap();

        assert_eq!(
            confirm_tracking_prompt(event.clone(), 12, &state)
                .await
                .unwrap_err(),
            PROMPT_CHANGED
        );
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
        assert!(state.event_is_current(&event));
        assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn prompt_preflight_rejects_a_replaced_local_entry() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let mut before = remote();
        before["status"] = json!("CURRENT");
        before["progress"] = json!(2);
        let mut saved = before.clone();
        saved["progress"] = json!(3);
        let (api, mut requests) = anilist::mock_api_gated(
            vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":before}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
            ],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.progress = 2;
        entry.status = "CURRENT".into();
        state.db.upsert_entry(&entry).unwrap();
        state.emit(crate::EventKind::TrackingAsk(crate::TrackingPrompt {
            media_id: 1,
            episode: 3,
            progress: 2,
            title: "Fixture".into(),
            raw_title: "Fixture 3".into(),
        }));
        let event = state.retained_tracking_prompt().unwrap();
        let task = {
            let state = state.clone();
            let event = event.clone();
            tokio::spawn(async move { confirm_tracking_prompt(event, 3, &state).await })
        };
        assert_eq!(requests.recv().await.unwrap()["variables"]["mediaId"], 1);
        {
            let _write = state.entry_lock.lock().await;
            entry.id = Some(23);
            state.db.upsert_entry(&entry).unwrap();
        }
        gate.add_permits(2);
        assert_eq!(task.await.unwrap().unwrap_err(), PROMPT_CHANGED);
        assert!(requests.try_recv().is_err());
        assert!(state.event_is_current(&event));
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(23));
    }

    #[tokio::test]
    async fn a_retained_prompt_cannot_update_a_replacement_entry() {
        let mut before = remote();
        before["id"] = json!(23);
        before["status"] = json!("CURRENT");
        before["progress"] = json!(2);
        let mut saved = before.clone();
        saved["progress"] = json!(3);
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":before}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":saved}})),
        ])
        .await;
        let state = state(api);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "CURRENT".into();
        entry.progress = 2;
        state.db.upsert_entry(&entry).unwrap();
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 3,
            progress: 2,
            title: "Fixture".into(),
            raw_title: "Fixture 3".into(),
        }));
        let event = state.retained_tracking_prompt().unwrap();

        entry.id = Some(23);
        state.db.upsert_entry(&entry).unwrap();
        assert!(confirm_tracking_prompt(event.clone(), 3, &state)
            .await
            .is_err());
        assert!(!state.event_is_current(&event));
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn prompt_rechecks_the_displayed_target_after_waiting() {
        for verify_owner in [false, true] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(
                vec![(200, score_profile("POINT_100"))],
                Some(gate.clone()),
            )
            .await;
            let state = state(api);
            if verify_owner {
                state.anilist.lock().set_token(Some("test-session".into()));
            }
            state.db.cache_episode_total(1, Some(13)).unwrap();
            state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
                media_id: 1,
                episode: 13,
                progress: 12,
                title: "Fixture".into(),
                raw_title: "Fixture 13".into(),
            }));
            let event = state.retained_tracking_prompt().unwrap();
            let original = event.clone();
            let held = state.entry_lock.lock().await;
            let worker = state.clone();
            let mut confirmation =
                Box::pin(async move { confirm_tracking_prompt(event, 13, &worker).await });
            // Poll once so the confirmation is waiting on the held entry lock.
            tokio::select! {
                biased;
                result = &mut confirmation => panic!("unexpected early confirmation: {result:?}"),
                () = std::future::ready(()) => {}
            }
            assert!(state.remote_lock.try_lock().is_err());
            let mut task = tokio::spawn(confirmation);
            if verify_owner {
                drop(held);
                let request = identity_request_while_running(&mut requests, &mut task).await;
                assert!(request["query"].as_str().unwrap().contains("Viewer"));
                let _entry = state.entry_lock.lock().await;
                state.db.cache_episode_total(1, Some(11)).unwrap();
            } else {
                state.db.cache_episode_total(1, Some(11)).unwrap();
                drop(held);
            }
            gate.add_permits(2);
            assert_eq!(task.await.unwrap().unwrap_err(), PROMPT_CHANGED);
            assert!(requests.try_recv().is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 12);
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.event_is_current(&original));
        }
    }

    #[tokio::test]
    async fn clamped_confirmation_retires_only_the_confirmed_prompt() {
        for replacement in [None, Some("preflight"), Some("mutation")] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let mut before = remote();
            before["progress"] = json!(11);
            let (api, mut requests) = anilist::mock_api_gated(
                vec![
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":before}}}),
                    ),
                    (200, json!({"data":{"SaveMediaListEntry":remote()}})),
                ],
                Some(gate.clone()),
            )
            .await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.progress = 11;
            state.db.upsert_entry(&base).unwrap();
            let prompt = crate::TrackingPrompt {
                media_id: 1,
                episode: 13,
                title: "Fixture Title".into(),
                raw_title: "Fixture Title - 13".into(),
                progress: 11,
            };
            state.emit(crate::EventKind::TrackingPrompt(prompt.clone()));
            let event = state.retained_tracking_prompt().unwrap();
            let confirms = state.clone();
            let requested = event.clone();
            let mut confirm =
                tokio::spawn(
                    async move { confirm_tracking_prompt(requested, 12, &confirms).await },
                );
            let lookup = identity_request_while_running(&mut requests, &mut confirm).await;
            assert_eq!(lookup["variables"]["mediaId"], 1);
            if replacement == Some("preflight") {
                state.emit(crate::EventKind::TrackingPrompt(prompt));
                let newer = state.retained_tracking_prompt().unwrap();
                gate.add_permits(1);
                assert_eq!(confirm.await.unwrap().unwrap_err(), PROMPT_CHANGED);
                assert!(state.event_is_current(&newer));
                assert!(state.db.pending(7).unwrap().is_empty());
                assert!(requests.try_recv().is_err());
                continue;
            }
            gate.add_permits(1);
            let mutation = identity_request_while_running(&mut requests, &mut confirm).await;
            assert_eq!(mutation["variables"]["progress"], 12);
            let newer = if replacement == Some("mutation") {
                state.emit(crate::EventKind::TrackingPrompt(prompt));
                state.retained_tracking_prompt()
            } else {
                None
            };
            gate.add_permits(1);

            confirm.await.unwrap().unwrap();
            assert!(!state.event_is_current(&event));
            assert_eq!(
                state
                    .retained_tracking_prompt()
                    .map(|prompt| prompt.prompt_id),
                newer.map(|prompt| prompt.prompt_id)
            );
        }
    }

    #[tokio::test]
    async fn signed_out_prompt_confirmation_keeps_the_prompt_and_progress() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api);
        state.anilist.lock().set_token(None);
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 13,
            title: "Some Show".into(),
            raw_title: "Some Show - 13".into(),
            progress: 12,
        }));
        let event = state.retained_tracking_prompt().unwrap();

        assert_eq!(
            confirm_tracking_prompt(event.clone(), 13, &state)
                .await
                .unwrap_err(),
            "not authenticated"
        );
        assert!(state.event_is_current(&event));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 12);
        assert!(requests.try_recv().is_err());
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
                        (
                            200,
                            json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                        ),
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
                            (
                                200,
                                json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                            ),
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
                    let mut expected = json!({"id":22,"status":"COMPLETED","progress":12,"scoreFormat":"POINT_100"});
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
                json!({"id":22,"progress":11,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(13),
                None,
                None,
                11,
                json!({"id":22,"progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                None,
                Some(12),
                None,
                None,
                11,
                json!({"id":22,"progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(0),
                Some(0),
                None,
                None,
                0,
                json!({"id":22,"progress":0,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                Some("PAUSED"),
                None,
                11,
                json!({"id":22,"status":"PAUSED","progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                Some("CURRENT"),
                None,
                11,
                json!({"id":22,"status":"CURRENT","progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                Some("REPEATING"),
                None,
                11,
                json!({"id":22,"status":"REPEATING","progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                Some("completed"),
                None,
                11,
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":3,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                Some("COMPLETED"),
                Some(9),
                11,
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":9,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                None,
                Some("COMPLETED"),
                None,
                11,
                json!({"id":22,"status":"COMPLETED","progress":12,"scoreFormat":"POINT_100"}),
            ),
            (
                Some(12),
                Some(12),
                None,
                None,
                12,
                json!({"id":22,"status":"COMPLETED","progress":12,"scoreFormat":"POINT_100"}),
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
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    episodes: total,
                    ..Default::default()
                })
                .unwrap();

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
    async fn checked_entry_patch_preserves_explicit_rewatch_at_total() {
        let mut reply = remote();
        reply["status"] = json!("REPEATING");
        reply["progress"] = json!(12);
        reply["score"] = json!(80);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_profile("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":reply}})),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        let mut before = state.db.get_entry(1).unwrap().unwrap();
        before.status = "REPEATING".into();
        before.progress = 11;
        state.db.upsert_entry(&before).unwrap();

        let saved = save_entry_inner_checked(
            &state,
            1,
            Some("REPEATING".into()),
            Some(12),
            Some(80.0),
            None,
            Some(11),
        )
        .await
        .unwrap();

        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"status":"REPEATING","progress":12,"score":80.0,"scoreFormat":"POINT_100"})
        );
        assert_eq!(
            (saved.status.as_str(), saved.progress, saved.repeat),
            ("REPEATING", 12, 2)
        );
        assert_eq!(saved.score, Some(80.0));
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn checked_entry_patch_rejects_stale_or_missing_baseline_without_writes() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api);
        let before = state.db.get_entry(1).unwrap().unwrap();
        let changed = state.db.0.lock().total_changes();

        let error = save_entry_inner_checked(
            &state,
            1,
            Some("REPEATING".into()),
            Some(3),
            Some(80.0),
            Some(4),
            Some(2),
        )
        .await
        .unwrap_err();
        assert!(error.contains("Progress changed from 2 to 12"));
        assert_eq!(state.db.0.lock().total_changes(), changed);
        let after = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (after.status, after.progress, after.score, after.repeat),
            (before.status, before.progress, before.score, before.repeat)
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(requests.try_recv().is_err());

        state.db.delete_entry(1).unwrap();
        let changed = state.db.0.lock().total_changes();
        let error = save_entry_inner_checked(
            &state,
            1,
            Some("CURRENT".into()),
            Some(3),
            None,
            None,
            Some(12),
        )
        .await
        .unwrap_err();
        assert!(error.contains("no longer on your list"));
        assert_eq!(state.db.0.lock().total_changes(), changed);
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(requests.try_recv().is_err());
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
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                ),
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
                json!({"id":22,"status":"COMPLETED","progress":12,"repeat":6,"scoreFormat":"POINT_100"})
            );

            save_entry_inner(&state, 1, None, Some(12), None, None)
                .await
                .unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"progress":12,"scoreFormat":"POINT_100"})
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().repeat, 6);
        }
    }

    #[tokio::test]
    async fn a_final_episode_edit_can_create_a_completed_entry() {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
            ),
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
            json!({"mediaId":1,"status":"COMPLETED","progress":12,"scoreFormat":"POINT_100"})
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
        state.db.cache_episode_total(1, Some(13)).unwrap();

        let saved = save_entry_inner(&state, 1, None, Some(12), None, None)
            .await
            .unwrap();

        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":12,"scoreFormat":"POINT_100"})
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
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
            ),
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
            json!({"id":22,"status":"COMPLETED","progress":12,"repeat":3,"scoreFormat":"POINT_100"})
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
            let state = state(api);
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
                    "sync" => sync_my_list_inner(&worker_state, ListPreferences::Preserve)
                        .await
                        .map(|_| ()),
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
                let expected_progress = if operation == "playback" { 11 } else { 12 };
                assert_eq!(
                    local_entries_inner(&state).await.unwrap()[0].progress,
                    expected_progress
                );
                let _local = state.entry_lock.lock().await;
                assert_eq!(
                    state.anilist.lock().token().as_deref(),
                    Some("test-session")
                );
                if operation == "playback" {
                    let entry = state.db.get_entry(1).unwrap().unwrap();
                    assert_eq!(
                        (entry.status.as_str(), entry.progress, entry.repeat),
                        ("CURRENT", 11, 2)
                    );
                } else {
                    assert_progress_preserved(&state);
                }
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
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
                ));
            }
            replies.push((200, json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":"POINT_100"}}}})));
            replies.extend([
                (429, json!({})),
                (
                    400,
                    json!({"errors":[{"message":"Invalid score","status":400}]}),
                ),
            ]);
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api);
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
                let (api, mut requests) = anilist::mock_api(vec![
                    (200, score_profile("POINT_100")),
                    (200, json!({"data":{"SaveMediaListEntry":reply}})),
                ])
                .await;
                let state = state(api);
                let saved = save_entry_inner(&state, 1, None, None, Some(80.0), None)
                    .await
                    .unwrap();
                assert_eq!(saved.score, Some(80.0));
                assert_progress_preserved(&state);
                assert_eq!(state.db.pending(7).unwrap()[0].patch.score, Some(80.0));
                assert!(requests.recv().await.unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("Viewer"));
                assert!(requests.recv().await.unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("mutation"));
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
                json!({"data":{"MediaListCollection":{"user":{"id":7},
                    "hasNextChunk":false,"lists":[{"entries":[reply]}]
                }}}),
            )])
            .await;
            let state = state(api);
            assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .is_err());
            assert_progress_preserved(&state);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
        }
    }

    #[tokio::test]
    async fn repeated_final_list_chunk_preserves_cached_entries() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_snapshot(90.0, true, 1)),
            (200, score_snapshot(90.0, false, 1)),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(23),
                media_id: 2,
                progress: 5,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        let result = sync_my_list(&state).await;
        assert_eq!(requests.try_recv().unwrap()["variables"]["chunk"], 1);
        assert_eq!(requests.try_recv().unwrap()["variables"]["chunk"], 2);
        assert!(state.db.get_entry(2).unwrap().is_some(), "{result:?}");
        assert!(result.is_err());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_entry_server_error_does_not_block_a_complete_list_refresh() {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                500,
                json!({"errors":[{"message":"Entry temporarily unavailable","status":500}]}),
            ),
            (200, score_snapshot(90.0, false, 2)),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                repeat: Some(3),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();

        let entries = sync_my_list(&state).await.unwrap();
        assert!(entries.iter().any(|entry| entry.media_id == 2));
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending
            .error
            .unwrap()
            .contains("Entry temporarily unavailable"));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().repeat, 3);
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert_eq!(requests.try_recv().unwrap()["variables"]["chunk"], 1);
        assert!(requests.try_recv().unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn incomplete_queue_lookups_and_acknowledgements_keep_saved_changes() {
        for lookup in [true, false] {
            let mut partial = remote();
            partial["repeat"] = json!(null);
            let replies = if lookup {
                vec![(
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":partial}}}),
                )]
            } else {
                vec![
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":remote()}}}),
                    ),
                    (200, score_profile("POINT_100")),
                    (200, json!({"data":{"SaveMediaListEntry":partial}})),
                ]
            };
            let mut replies = replies;
            replies.insert(0, (200, json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":"POINT_100"}}}})));
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
            "Media":{"mediaListEntry":{"id":22,"mediaId":1,"notes":"Old note","startedAt":null,"completedAt":null,"customLists":{}}},
            "Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}
        }});
        let (api, mut reads) = anilist::mock_api(vec![(429, json!({})), (200, old.clone())]).await;
        let state = state(api);
        let reading = state.clone();
        let read = tokio::spawn(async move { get_entry_details_inner(1, &reading).await });
        reads.recv().await.unwrap();

        let (mut api, _) = anilist::mock_api(vec![
            (200, old),
            (
                200,
                json!({"data":{"SaveMediaListEntry":{"id":22,"mediaId":1,"notes":"New note","startedAt":null,"completedAt":null,"customLists":{}}}}),
            ),
        ])
        .await;
        api.record_viewer(7);
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
                json!({"data":{"MediaListCollection":{"user":{"id":7},
                    "hasNextChunk":false,"lists":[{"entries":[remote()]}]
                }}}),
            ),
            (200, json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":"POINT_100"}}}})),
            (200, json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":"POINT_100"}}}})),
            (200, json!({"data":{"SaveMediaListEntry":saved}})),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        let syncing = state.clone();
        let sync =
            tokio::spawn(
                async move { sync_my_list_inner(&syncing, ListPreferences::Preserve).await },
            );
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
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![
                (401, json!({"errors":[{"message":"Invalid Token"}]})),
                (
                    200,
                    json!({"data":{"Viewer":{"id":8,"name":"New account"}}}),
                ),
            ],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let mut events = state.events.subscribe();
        let old = state.origin();
        let reading = state.clone();
        let read = tokio::spawn(async move { get_notifications_page_inner(1, &reading).await });
        requests.recv().await.unwrap();
        let intent = state.begin_auth_intent();
        assert_eq!(read.await.unwrap().unwrap_err(), AUTH_REPLACED);
        gate.add_permits(2);
        login_with_token_inner(
            "replacement".into(),
            &state,
            intent,
            ListPreferences::Preserve,
        )
        .await
        .unwrap();
        assert_eq!(current_user_inner(&state).await.unwrap().unwrap().id, 8);
        assert_eq!(state.anilist.lock().token().as_deref(), Some("replacement"));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let stale = crate::Event {
            generation: old.generation,
            account_id: old.account_id,
            prompt_id: None,
            kind: crate::EventKind::PendingChanged(Some(vec![1])),
        };
        assert!(!state.event_is_current(&stale));
    }
    #[tokio::test]
    async fn displayed_score_format_is_checked_before_staging() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api);
        crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                progress: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        let result = save_entry_inner_checked_impl(
            &state,
            1,
            None,
            None,
            Some(4.0),
            None,
            None,
            Some(Some("POINT_5".into())),
            None,
        )
        .await;
        assert!(result.unwrap_err().contains("Score format changed"));
        let pending = state.db.pending(7).unwrap().remove(0);
        assert_eq!(pending.patch.progress, Some(5));
        assert_eq!(pending.patch.score, None);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn foreground_format_change_preserves_intent_without_mutation() {
        let (api, mut requests) = anilist::mock_api(vec![(200,
            json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":"POINT_5"}}}})
        )]).await;
        let state = state(api);
        let saved = save_entry_inner_checked_impl(
            &state,
            1,
            None,
            Some(5),
            Some(80.0),
            None,
            None,
            Some(Some("POINT_100".into())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(saved.score, None);
        let pending = state.db.pending(7).unwrap().remove(0);
        assert_eq!(pending.patch.progress, Some(5));
        assert_eq!(pending.patch.score, Some(80.0));
        assert_eq!(
            state.user.lock().as_ref().unwrap().score_format.as_deref(),
            Some("POINT_5")
        );
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn editor_durable_idless_entry_only_edits_a_queued_add() {
        let unavailable = (503, json!({"errors":[{"message":"Unavailable"}]}));
        let (api, mut requests) = anilist::mock_api(vec![unavailable]).await;
        let state = state(api);
        let target = crate::EntryEditTarget {
            account_id: Some(7),
            identity: crate::EntryIdentity::Unverified,
        };
        let mut legacy = state.db.get_entry(1).unwrap().unwrap();
        legacy.id = None;
        state.db.upsert_entry(&legacy).unwrap();
        let save = |media_id, expected_progress, target| {
            save_entry_inner_checked_impl(
                &state,
                media_id,
                None,
                None,
                None,
                Some(5),
                Some(expected_progress),
                Some(Some("POINT_100".into())),
                Some(EntryWriteTarget::Editor(target)),
            )
        };
        assert_eq!(
            save(1, 12, target).await.unwrap_err(),
            EDITOR_ENTRY_UNVERIFIED
        );
        assert!(requests.try_recv().is_err());
        let new = save_entry_inner(&state, 2, Some("PLANNING".into()), None, None, None)
            .await
            .unwrap();
        assert_eq!(new.id, None);
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 2);
        let target = crate::EntryEditTarget {
            account_id: Some(7),
            identity: new.identity(),
        };
        let other_account = crate::EntryEditTarget {
            account_id: Some(8),
            ..target.clone()
        };
        assert!(save(2, 0, other_account)
            .await
            .unwrap_err()
            .contains("account changed"));
        assert_eq!(save(2, 0, target.clone()).await.unwrap().repeat, 5);
        let pending = state.db.pending_change(7, 2).unwrap().unwrap();
        assert!(pending.base.is_none());
        assert_eq!(pending.patch.repeat, Some(5));
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(44),
                repeat: 1,
                ..new
            })
            .unwrap();
        assert_eq!(save(2, 0, target).await.unwrap_err(), EDITOR_ENTRY_CHANGED);
        assert_eq!(state.db.get_entry(2).unwrap().unwrap().repeat, 1);
        assert!(state.db.pending_change(7, 2).unwrap().unwrap().conflict);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn guarded_progress_and_discovery_do_not_write_a_replacement() {
        let mut replacement = remote();
        replacement["id"] = json!(33);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":replacement}}}),
        )])
        .await;
        let state = state(api);
        let original = state.db.get_entry(1).unwrap().unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(33),
                ..original.clone()
            })
            .unwrap();

        assert!(set_progress_inner_with_origin(
            &state,
            1,
            11,
            Some(12),
            Some((Some(7), original.identity())),
        )
        .await
        .is_err());
        let change_status = |identity| {
            save_entry_inner_checked_impl(
                &state,
                1,
                Some("PLANNING".into()),
                None,
                None,
                None,
                None,
                None,
                Some(EntryWriteTarget::Discovery {
                    account_id: Some(7),
                    identity,
                }),
            )
        };
        assert_eq!(
            change_status(Some(original.identity())).await.unwrap_err(),
            DISCOVERY_ENTRY_CHANGED
        );
        assert_eq!(
            change_status(None).await.unwrap_err(),
            DISCOVERY_ENTRY_CHANGED
        );
        assert!(requests.try_recv().is_err());

        state.db.delete_entry(1).unwrap();
        assert_eq!(
            change_status(None).await.unwrap_err(),
            DISCOVERY_ENTRY_CHANGED
        );
        let lookup = requests.try_recv().unwrap();
        assert!(lookup["query"].as_str().unwrap().starts_with("query"));
        assert!(requests.try_recv().is_err());
        let cached = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (cached.id, cached.status, cached.progress),
            (Some(33), original.status, 12)
        );
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_editor_cannot_cancel_a_recreated_queued_add() {
        let absent = (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
        );
        let unavailable = (503, json!({"errors":[{"message":"Unavailable"}]}));
        let (api, mut requests) = anilist::mock_api(vec![
            unavailable.clone(),
            absent.clone(),
            unavailable,
            absent,
        ])
        .await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();
        let add = || save_entry_inner(&state, 1, Some("PLANNING".into()), None, None, None);
        let first = add().await.unwrap();
        let target = crate::EntryEditTarget {
            account_id: Some(7),
            identity: first.identity(),
        };
        crate::sync_queue::resolve_pending_change(1, false, &state)
            .await
            .unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
        let second = add().await.unwrap();
        assert_ne!(first.identity(), second.identity());
        for _ in 0..3 {
            assert_eq!(requests.recv().await.unwrap()["variables"]["mediaId"], 1);
        }
        let stale = delete_entry_target(1, Some(target), &state).await;
        assert_eq!(stale.unwrap_err(), EDITOR_ENTRY_CHANGED);
        assert!(requests.try_recv().is_err());
        assert_eq!(
            state.db.get_entry(1).unwrap().unwrap().identity(),
            second.identity()
        );
        assert!(state.db.pending_change(7, 1).unwrap().is_some());
        let target = crate::EntryEditTarget {
            account_id: Some(7),
            identity: second.identity(),
        };
        let other_account = crate::EntryEditTarget {
            account_id: Some(8),
            ..target.clone()
        };
        assert!(delete_entry_target(1, Some(other_account), &state)
            .await
            .unwrap_err()
            .contains("account changed"));
        assert!(requests.try_recv().is_err());
        delete_entry_target(1, Some(target), &state).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.db.pending_change(7, 1).unwrap().is_none());
    }

    #[tokio::test]
    async fn foreground_creation_acknowledges_its_projected_identity() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":remote()}})),
            ],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();
        let writer = state.clone();
        let save = tokio::spawn(async move {
            save_entry_inner(&writer, 1, Some("COMPLETED".into()), None, None, None).await
        });
        assert_eq!(requests.recv().await.unwrap()["variables"]["mediaId"], 1);
        gate.add_permits(1);
        let mutation = requests.recv().await.unwrap();
        assert!(mutation["query"]
            .as_str()
            .unwrap()
            .contains("SaveMediaListEntry"));
        let projected = get_entry(1, &state).await.unwrap().unwrap();
        let identity = projected.identity();
        assert!(matches!(identity, crate::EntryIdentity::Queued(_)));
        gate.add_permits(1);
        let confirmed = save.await.unwrap().unwrap();
        assert_eq!(confirmed.id, Some(22));
        assert!(identity.matches(&confirmed));
        assert!(identity.matches(&state.db.get_entry(1).unwrap().unwrap()));
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn guarded_progress_follows_only_its_acknowledged_creation() {
        for (remote_id, remote_progress) in [(22, 7), (22, 6), (33, 7)] {
            let mut current = remote();
            current["id"] = json!(remote_id);
            current["status"] = json!("CURRENT");
            current["progress"] = json!(remote_progress);
            let mut saved = current.clone();
            saved["progress"] = json!(8);
            let (api, mut requests) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":current}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
            ])
            .await;
            let state = state(api);
            let mut entry = state.db.get_entry(1).unwrap().unwrap();
            entry.status = "CURRENT".into();
            entry.progress = 7;
            let identity = crate::EntryIdentity::Queued("displayed-creation".into());
            entry.queued_creation = Some("displayed-creation".into());
            state
                .db
                .cache_acknowledged_entry(None, &mut entry, None)
                .unwrap();
            assert!(identity.matches(&entry));

            let (outcome, _) = set_progress_inner_with_origin(
                &state,
                1,
                8,
                Some(7),
                Some((Some(7), identity.clone())),
            )
            .await
            .unwrap();
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            if (remote_id, remote_progress) == (22, 7) {
                let ControlFlow::Continue(saved) = outcome else {
                    panic!("the acknowledged creation should still be editable");
                };
                assert_eq!(saved.progress, 8);
                assert!(identity.matches(&saved));
                assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
            } else {
                let ControlFlow::Break(current) = outcome else {
                    panic!("the conflicting entry must remain untouched");
                };
                assert_eq!(
                    (current.id, current.progress),
                    (Some(remote_id), remote_progress)
                );
                assert_eq!(identity.matches(&current), remote_id == 22);
                let cached = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!(cached.progress, remote_progress);
                assert_eq!(identity.matches(&cached), remote_id == 22);
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn editor_removal_cannot_retry_against_a_replacement_id() {
        let replacement = json!({"data":{"Media":{"id":1,"mediaListEntry":{
            "id":33,"mediaId":1,"status":"COMPLETED","progress":12,"score":70,"repeat":1
        }}}});
        let (api, mut requests) = anilist::mock_api(vec![
            (404, json!({"errors":[{"message":"Not Found."}]})),
            (200, replacement.clone()),
            (200, replacement),
        ])
        .await;
        let state = state(api);
        let target = crate::EntryEditTarget {
            account_id: Some(7),
            identity: crate::EntryIdentity::Remote(22),
        };
        assert!(delete_entry_target(1, Some(target), &state)
            .await
            .unwrap_err()
            .contains("identity changed"));
        assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.conflict && pending.deleting);
        assert_eq!(pending.deletion_id, Some(22));
        crate::sync_queue::resolve_pending_change(1, false, &state)
            .await
            .unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(33));
    }

    #[tokio::test]
    async fn foreground_score_preflight_expiration_is_not_a_successful_save() {
        let (api, _) = anilist::mock_api(vec![(
            401,
            json!({"errors":[{"message":"Invalid token","status":401}]}),
        )])
        .await;
        let state = state(api);
        assert!(save_entry_inner(&state, 1, None, None, Some(80.0), None)
            .await
            .is_err());
        assert!(state.anilist.lock().token().is_none());
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn foreground_score_postflight_expiration_allows_same_account_readback() {
        let mut committed = remote();
        committed["score"] = json!(80);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_profile("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":committed}})),
            (
                401,
                json!({"errors":[{"message":"Invalid token","status":401}]}),
            ),
            (200, score_profile("POINT_100")),
            (200, score_profile("POINT_100")),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":committed}}}),
            ),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);

        let error = save_entry_inner(&state, 1, None, None, Some(80.0), None)
            .await
            .unwrap_err();
        assert_eq!(error, SESSION_EXPIRED);
        assert!(!state.anilist.lock().has_token());
        let pending = state.db.pending(7).unwrap().remove(0);
        assert_eq!(pending.patch.score, Some(80.0));
        assert!(!pending.conflict);

        login_with_token("renewed-session".into(), &state, ListPreferences::Preserve)
            .await
            .unwrap();
        assert!(crate::sync_queue::sync_pending_changes(&state)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(80.0));
        for expected in [
            "Viewer",
            "SaveMediaListEntry",
            "Viewer",
            "Viewer",
            "Viewer",
            "mediaListEntry",
            "Viewer",
        ] {
            let request = requests.try_recv().unwrap();
            assert!(request["query"].as_str().unwrap().contains(expected));
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn superseded_score_preflights_leave_only_dispatched_writes_pending() {
        for (postflight, mismatch) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut responses = Vec::new();
            if postflight {
                let mut committed = remote();
                committed["score"] = json!(80);
                responses.push((200, score_profile("POINT_100")));
                responses.push((200, json!({"data":{"SaveMediaListEntry":committed}})));
            }
            responses.push(if mismatch {
                (200, score_profile("POINT_5"))
            } else {
                (
                    400,
                    json!({"errors":[{"message":"Invalid Viewer request","status":400}]}),
                )
            });
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            let editing = state.clone();
            let mut task = tokio::spawn(async move {
                save_entry_inner(&editing, 1, None, None, Some(80.0), None).await
            });
            if postflight {
                identity_request_while_running(&mut requests, &mut task).await;
                gate.add_permits(1);
                identity_request_while_running(&mut requests, &mut task).await;
                gate.add_permits(1);
            }
            identity_request_while_running(&mut requests, &mut task).await;
            state.cancel_login();
            gate.add_permits(1);
            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
            assert!(requests.try_recv().is_err());
            let pending = state.db.pending(7).unwrap();
            assert_eq!(
                state
                    .db
                    .cached_user()
                    .unwrap()
                    .unwrap()
                    .score_format
                    .as_deref(),
                Some("POINT_100")
            );
            assert_eq!(
                state.user.lock().as_ref().unwrap().name,
                if postflight { "Renamed" } else { "Tester" }
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
            if postflight {
                assert_eq!(pending.len(), 1);
                assert!(!pending[0].conflict);
                assert_eq!(pending[0].patch.score, Some(80.0));
            } else {
                assert!(pending.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn supersession_wins_after_a_viewer_error_and_before_rejection_classification() {
        use crate::sync_queue::{check_score_format_detailed, stage, Patch};

        for status in [400, 503] {
            let (api, _) = anilist::mock_api(vec![(
                status,
                json!({"errors":[{"message":"Viewer failed","status":status}]}),
            )])
            .await;
            let state = state(api);
            let origin = state.origin();
            let al = state
                .anilist
                .lock()
                .for_intent(&state.auth_intent, origin.generation);
            let token = al.token();
            let (_, mut pending, _) = stage(
                &state,
                1,
                Patch {
                    score: Some(80.0),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            pending.error = Some("Earlier issue".into());
            state.db.put_pending(7, &pending).unwrap();

            let failure = check_score_format_detailed(&state, origin, &al, 7, &pending, true)
                .await
                .unwrap_err();
            assert_eq!(failure.retryable, status == 503);
            assert!(!failure.account_changed && !failure.auth_rejected);
            check_write_origin(&state, origin, &token).unwrap();
            state.cancel_login();
            let _entry = state.entry_lock.lock().await;
            assert_eq!(
                write_origin_guard(&state, origin, &token).unwrap_err(),
                AUTH_REPLACED
            );
            assert!(matches!(
                reject_write_session(&state, origin, &token),
                WriteAuthFailure::Replaced
            ));
            assert!(state.anilist.lock().has_token());
            assert!(state.last_auth_expired.lock().is_none());
            let retained = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(retained.error.as_deref(), Some("Earlier issue"));
            assert!(!retained.conflict);
            assert_eq!(retained.patch.score, Some(80.0));
        }
    }

    #[tokio::test]
    async fn auth_supersession_releases_remote_lock_during_reads_and_cooldowns() {
        use crate::sync_queue::{
            flush_background, resolve_pending_change, stage, sync_pending_changes, Patch,
        };

        for path in [
            "list",
            "cold save",
            "manual replay",
            "background replay",
            "keep local",
            "keep AniList",
            "details",
            "edit details",
        ] {
            for rate_limited in [false, true] {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let reply = if rate_limited {
                    (429, json!({}))
                } else {
                    (200, score_snapshot(70.0, true, 1))
                };
                let (api, mut requests) =
                    anilist::mock_api_gated(vec![reply], Some(gate.clone())).await;
                let state = state(api);
                if path == "cold save" {
                    state.db.clear_entries().unwrap();
                } else if matches!(
                    path,
                    "manual replay" | "background replay" | "keep local" | "keep AniList"
                ) {
                    stage(
                        &state,
                        1,
                        Patch {
                            progress: Some(11),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                let pending = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
                let acting = state.clone();
                let mut task = tokio::spawn(async move {
                    match path {
                        "list" => sync_my_list(&acting).await.map(drop),
                        "cold save" => save_entry_inner(&acting, 1, None, Some(11), None, None)
                            .await
                            .map(drop),
                        "manual replay" => sync_pending_changes(&acting).await.map(drop),
                        "background replay" => flush_background(&acting).await,
                        "keep local" => resolve_pending_change(1, true, &acting).await,
                        "keep AniList" => resolve_pending_change(1, false, &acting).await,
                        "details" => get_entry_details_inner(1, &acting).await.map(drop),
                        "edit details" => update_entry_details_inner(
                            1,
                            Some("Note".into()),
                            None,
                            None,
                            None,
                            &acting,
                        )
                        .await
                        .map(drop),
                        _ => unreachable!(),
                    }
                });
                let request = identity_request_while_running(&mut requests, &mut task).await;
                assert!(
                    request["query"].as_str().unwrap().starts_with("query"),
                    "{path}"
                );
                if rate_limited {
                    gate.add_permits(1);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                state.cancel_login();
                let result = tokio::time::timeout(Duration::from_millis(750), &mut task)
                    .await
                    .unwrap_or_else(|_| panic!("{path} kept the remote lock"));
                assert_eq!(result.unwrap().unwrap_err(), AUTH_REPLACED, "{path}");
                assert!(state.remote_lock.try_lock().is_ok(), "{path}");
                assert!(requests.try_recv().is_err(), "{path}");
                assert_eq!(
                    serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                    pending,
                    "{path}"
                );
                assert!(state.anilist.lock().has_token());
            }
        }
    }

    #[tokio::test]
    async fn superseded_playback_lookup_leaves_the_baseline_and_existing_edits() {
        let mut current = remote();
        current["status"] = json!("CURRENT");
        current["progress"] = json!(7);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":current}}}),
            )],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.status = "CURRENT".into();
        base.progress = 7;
        state.db.upsert_entry(&base).unwrap();
        crate::sync_queue::stage(
            &state,
            2,
            crate::sync_queue::Patch {
                progress: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        let editing = state.clone();
        let mut task = tokio::spawn(async move {
            watcher_set_progress(&editing, 1, 8, Some("test-session")).await
        });
        identity_request_while_running(&mut requests, &mut task).await;
        assert_eq!(get_entry(1, &state).await.unwrap().unwrap().progress, 7);
        state.cancel_login();
        gate.add_permits(1);

        assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 7);
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].media_id, pending[0].patch.progress),
            (2, Some(3))
        );
    }

    #[tokio::test]
    async fn superseded_401_does_not_expire_a_logout_waiting_on_the_remote_lock() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(
                401,
                json!({"errors":[{"message":"Invalid token","status":401}]}),
            )],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let editing = state.clone();
        let mut edit =
            tokio::spawn(async move { set_progress_inner(&editing, 1, 11, Some(12)).await });
        identity_request_while_running(&mut requests, &mut edit).await;
        let signing_out = logout(&state);
        tokio::pin!(signing_out);
        tokio::select! {
            biased;
            result = &mut signing_out => panic!("logout bypassed the remote lock: {result:?}"),
            _ = std::future::ready(()) => {}
        }
        gate.add_permits(1);

        assert_eq!(edit.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert!(state.anilist.lock().has_token());
        signing_out.await.unwrap();
        assert!(state.last_auth_expired.lock().is_none());
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn superseded_refusals_cannot_replay_staged_mutations() {
        for (removal, status, body, ambiguous) in [
            (false, 400, json!({"errors":[{"message":"Refused"}]}), false),
            (
                false,
                401,
                json!({"errors":[{"message":"Invalid token"}]}),
                false,
            ),
            (true, 400, json!({"errors":[{"message":"Refused"}]}), false),
            (
                true,
                401,
                json!({"errors":[{"message":"Invalid token"}]}),
                false,
            ),
            (
                true,
                200,
                json!({"data":{"DeleteMediaListEntry":{"deleted":false}}}),
                false,
            ),
            (
                true,
                200,
                json!({"data":{"DeleteMediaListEntry":null}}),
                true,
            ),
            (
                true,
                503,
                json!({"errors":[{"message":"Unavailable"}]}),
                true,
            ),
        ] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![(status, body)], Some(gate.clone())).await;
            let state = state(api);
            let editing = state.clone();
            let mut edit = tokio::spawn(async move {
                if removal {
                    delete_entry_inner(1, &editing).await
                } else {
                    set_progress_inner(&editing, 1, 11, Some(12))
                        .await
                        .map(drop)
                }
            });
            identity_request_while_running(&mut requests, &mut edit).await;
            if !removal {
                assert_eq!(get_entry(1, &state).await.unwrap().unwrap().progress, 11);
            }
            state.cancel_login();
            gate.add_permits(1);

            assert_eq!(edit.await.unwrap().unwrap_err(), AUTH_REPLACED);
            let pending = state.db.pending(7).unwrap();
            if ambiguous {
                assert!(pending[0].deleting && pending[0].conflict);
            } else {
                assert!(pending.is_empty(), "{removal} {status}");
            }
            assert_progress_preserved(&state);
            crate::sync_queue::flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn superseded_delete_refusal_restores_the_previous_edit_for_its_account() {
        for code in [400, 401] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(
                vec![(
                    code,
                    json!({"errors":[{"message":"Refused","status":code}]}),
                )],
                Some(gate.clone()),
            )
            .await;
            let state = state(api);
            let (_, prior, _) = crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    score: Some(80.0),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            let mut other = prior.clone();
            other.patch.score = Some(35.0);
            state.db.put_pending(8, &other).unwrap();
            let editing = state.clone();
            let mut edit = tokio::spawn(async move { delete_entry_inner(1, &editing).await });
            identity_request_while_running(&mut requests, &mut edit).await;
            state.cancel_login();
            gate.add_permits(1);
            assert_eq!(edit.await.unwrap().unwrap_err(), AUTH_REPLACED);
            for (account, expected) in [(7, prior), (8, other)] {
                assert_eq!(
                    serde_json::to_value(state.db.pending_change(account, 1).unwrap().unwrap())
                        .unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
            assert!(state.anilist.lock().has_token());
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn superseded_removal_lookups_preserve_the_previous_queue() {
        for (after_delete, queued_removal) in [(false, false), (true, false), (true, true)] {
            for status in [400, 401, 503] {
                let mut replies = vec![];
                if after_delete {
                    replies.push((404, json!({"errors":[{"message":"Not Found"}]})));
                }
                replies.push((status, json!({"errors":[{"message":"Lookup failed"}]})));
                let gate = Arc::new(tokio::sync::Semaphore::new(usize::from(after_delete)));
                let (api, mut requests) =
                    anilist::mock_api_gated(replies, Some(gate.clone())).await;
                let state = state(api);
                let (_, mut prior, _) = crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        progress: Some(11),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                if queued_removal {
                    prior.deleting = true;
                    prior.deletion_id = Some(22);
                    state.db.put_pending(7, &prior).unwrap();
                }
                if !after_delete {
                    state.db.delete_entry(1).unwrap();
                }
                let editing = state.clone();
                let mut edit = tokio::spawn(async move {
                    if queued_removal {
                        crate::sync_queue::flush_background(&editing).await
                    } else {
                        delete_entry_inner(1, &editing).await
                    }
                });
                if after_delete {
                    assert_eq!(
                        identity_request_while_running(&mut requests, &mut edit).await["variables"]
                            ["id"],
                        22
                    );
                }
                assert_eq!(
                    identity_request_while_running(&mut requests, &mut edit).await["variables"]
                        ["mediaId"],
                    1
                );
                state.cancel_login();
                gate.add_permits(1);
                assert_eq!(edit.await.unwrap().unwrap_err(), AUTH_REPLACED);
                assert_eq!(
                    serde_json::to_value(state.db.pending_change(7, 1).unwrap().unwrap()).unwrap(),
                    serde_json::to_value(&prior).unwrap(),
                );
                assert!(state.anilist.lock().has_token());
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_errors_are_settled_before_auth_expired() {
        for operation in [
            "lookup",
            "mutation",
            "new score",
            "saved score",
            "confirm score",
        ] {
            let mut replies = match operation {
                "mutation" => vec![(
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote()}}}),
                )],
                "confirm score" => {
                    let mut saved = remote();
                    saved["score"] = json!(80);
                    vec![
                        (200, score_profile("POINT_100")),
                        (200, json!({"data":{"SaveMediaListEntry":saved}})),
                    ]
                }
                _ => vec![],
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(replies.len()));
            replies.push((401, json!({"errors":[{"message":"Invalid token"}]})));
            let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
            let state = state(api);
            let queued = matches!(operation, "lookup" | "mutation" | "saved score");
            if queued {
                crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        progress: (operation != "saved score").then_some(11),
                        score: (operation == "saved score").then_some(80.0),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            let mut events = state.events.subscribe();
            let editing = state.clone();
            let mut edit = tokio::spawn(async move {
                if queued {
                    crate::sync_queue::sync_pending_changes(&editing)
                        .await
                        .map(drop)
                } else {
                    save_entry_inner(&editing, 1, None, None, Some(80.0), None)
                        .await
                        .map(drop)
                }
            });
            let request_count = match operation {
                "mutation" => 2,
                "confirm score" => 3,
                _ => 1,
            };
            for _ in 0..request_count {
                identity_request_while_running(&mut requests, &mut edit).await;
            }
            tokio::task::block_in_place(|| {
                // Pause prompt cleanup while a newer sign-in claims the generation.
                let _prompt = state.last_tracking_prompt.lock();
                gate.add_permits(1);
                let event = tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(Duration::from_secs(3), events.recv())
                        .await
                        .unwrap()
                        .unwrap()
                });
                assert!(
                    matches!(event.kind, crate::EventKind::AuthExpired),
                    "{operation}"
                );
                let retained = state.db.pending_change(7, 1).unwrap();
                if operation == "new score" {
                    assert!(retained.is_none());
                } else {
                    let retained = retained.unwrap();
                    assert_eq!(
                        retained.error.as_deref(),
                        Some(SESSION_EXPIRED),
                        "{operation}"
                    );
                    assert!(!retained.conflict);
                }
                state.auth_intent.send_modify(|generation| *generation += 1);
            });
            assert_eq!(
                edit.await.unwrap().unwrap_err(),
                SESSION_EXPIRED,
                "{operation}"
            );
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn foreground_entry_requests_verify_the_saved_account() {
        for operation in [
            "cached",
            "uncached",
            "playback",
            "notes",
            "save notes",
            "notifications",
            "mark read",
        ] {
            let mut viewer = score_profile("POINT_100");
            viewer["data"]["Viewer"]["id"] = json!(8);
            let (api, mut requests) = anilist::mock_api(vec![(200, viewer)]).await;
            let state = state(api);
            state.anilist.lock().set_token(Some("test-session".into()));
            if operation == "uncached" {
                state.db.delete_entry(1).unwrap();
            }
            if operation == "playback" {
                let mut row = state.db.get_entry(1).unwrap().unwrap();
                row.status = "CURRENT".into();
                row.progress = 10;
                state.db.upsert_entry(&row).unwrap();
            }
            let result = match operation {
                "playback" => watcher_set_progress(&state, 1, 11, Some("test-session"))
                    .await
                    .map(drop),
                "notifications" => get_notifications_page_inner(1, &state).await.map(drop),
                "mark read" => mark_notifications_read_inner(&state).await,
                "notes" => get_entry_details_inner(1, &state).await.map(drop),
                "save notes" => {
                    update_entry_details_inner(1, Some("Edited".into()), None, None, None, &state)
                        .await
                        .map(drop)
                }
                _ => save_entry_inner(&state, 1, Some("CURRENT".into()), Some(11), None, None)
                    .await
                    .map(drop),
            };
            assert_eq!(result.unwrap_err(), "Account changed");
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            assert!(requests.try_recv().is_err(), "{operation}");
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_setting("entry_details:7:1").unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn unverified_legacy_sessions_keep_edits_with_the_original_account() {
        for owner in [None, Some("8"), Some("invalid")] {
            let (api, mut requests) = anilist::mock_api(vec![]).await;
            let state = state(api);
            let pending = conflicted_intent(&state);
            state.db.delete_setting(USER_ID_KEY).unwrap();
            state.anilist.lock().set_token(Some("restored".into()));
            if let Some(owner) = owner {
                state
                    .db
                    .set_setting(crate::db::LIST_OWNER_KEY, owner)
                    .unwrap();
                state.anilist.lock().record_viewer(7);
            }
            for result in [
                set_progress_inner(&state, 1, 11, Some(12)).await.map(drop),
                save_entry_inner(&state, 2, Some("PLANNING".into()), None, None, None)
                    .await
                    .map(drop),
                delete_entry_inner(1, &state).await,
                mark_notifications_read_inner(&state).await,
            ] {
                assert!(result.unwrap_err().contains("verify your account"));
            }
            assert!(requests.try_recv().is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(22));
            assert_eq!(
                serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                pending
            );
        }
    }

    #[tokio::test]
    async fn a_missing_viewer_does_not_remove_an_anime() {
        for cached in [true, false] {
            let (api, mut requests) =
                anilist::mock_api(vec![(404, json!({"errors":[{"message":"Not Found"}]}))]).await;
            let state = state(api);
            state.anilist.lock().set_token(Some("test-session".into()));
            if !cached {
                state.db.delete_entry(1).unwrap();
            }
            let error = save_entry_inner(&state, 1, Some("CURRENT".into()), Some(11), None, None)
                .await
                .unwrap_err();
            assert!(error.contains("account"), "{error}");
            assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
            assert_eq!(state.db.get_entry(1).unwrap().is_some(), cached);
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn retryable_account_preflight_preserves_the_saved_baseline() {
        let (api, mut requests) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Not Found"}]}))]).await;
        let state = state(api);
        state.anilist.lock().set_token(Some("test-session".into()));
        let original = state.db.get_entry(1).unwrap().unwrap();
        let saved = set_progress_inner(&state, 1, 11, Some(12)).await.unwrap();
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert_eq!(pending.base.as_ref().unwrap().progress, original.progress);
        assert_eq!(pending.base.as_ref().unwrap().score, original.score);
        assert_eq!(pending.base.as_ref().unwrap().id, original.id);
        assert_eq!(saved.progress, 11);
        assert_eq!(saved.score, original.score);
        assert!(!pending.conflict && !pending.missing_media);
        assert!(requests.try_recv().unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn save_and_playback_preflight_refusals_leave_no_pending_intent_at_auth_expiry() {
        for playback in [false, true] {
            let (api, mut requests) = anilist::mock_api(vec![(
                401,
                json!({
                    "errors":[{"message":"Invalid token","status":401}]
                }),
            )])
            .await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.progress = 11;
            state.db.upsert_entry(&base).unwrap();
            let mut events = state.events.subscribe();
            let editing = state.clone();
            let edit = tokio::spawn(async move {
                if playback {
                    watcher_set_progress(&editing, 1, 12, Some("test-session"))
                        .await
                        .map(drop)
                } else {
                    set_progress_inner(&editing, 1, 10, Some(11))
                        .await
                        .map(drop)
                }
            });
            loop {
                let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(event.kind, crate::EventKind::AuthExpired) {
                    break;
                }
            }
            state.cancel_login();
            assert!(state.db.pending(7).unwrap().is_empty());
            assert_eq!(edit.await.unwrap().unwrap_err(), SESSION_EXPIRED);
            let first = requests.try_recv().unwrap();
            assert_eq!(
                first["query"].as_str().unwrap().starts_with("query"),
                playback
            );
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn progress_after_a_queued_rewatch_finale_uses_the_revised_total() {
        for (manual, total, next, status, repeat) in [
            (true, 13, 13, "COMPLETED", 3),
            (true, 13, 10, "REPEATING", 2),
            (false, 14, 13, "REPEATING", 2),
        ] {
            let mut remote_base = remote();
            remote_base["status"] = json!("REPEATING");
            remote_base["progress"] = json!(11);
            let mut accepted = remote_base.clone();
            accepted["status"] = json!(status);
            accepted["progress"] = json!(next);
            accepted["repeat"] = json!(repeat);
            let (api, mut requests) = anilist::mock_api(vec![
                (503, json!({"errors":[{"message":"Offline"}]})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":total,"mediaListEntry":remote_base}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":accepted}})),
            ])
            .await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "REPEATING".into();
            base.progress = 11;
            state.db.upsert_entry(&base).unwrap();
            let first = watcher_set_progress(&state, 1, 12, Some("test-session"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!((first.status.as_str(), first.repeat), ("COMPLETED", 3));
            state.db.cache_episode_total(1, Some(total)).unwrap();
            let saved = if manual {
                set_progress_inner(&state, 1, next, Some(12)).await.unwrap()
            } else {
                watcher_set_progress(&state, 1, next, Some("test-session"))
                    .await
                    .unwrap()
                    .unwrap()
            };
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                (status, next, repeat)
            );
            assert!(crate::sync_queue::sync_pending_changes(&state)
                .await
                .unwrap()
                .is_empty());
            for _ in 0..2 {
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .starts_with("query"));
            }
            let mutation = requests.try_recv().unwrap();
            assert_eq!(mutation["variables"]["progress"], next);
            assert_eq!(
                mutation["variables"]["repeat"].as_i64(),
                (repeat == 3).then_some(3)
            );
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn playback_treats_nonpositive_legacy_episode_totals_as_unknown() {
        for total in [0, -4] {
            let mut saved = remote();
            saved["progress"] = json!(1);
            saved["status"] = json!("CURRENT");
            let mut base = saved.clone();
            base["progress"] = json!(0);
            let (api, mut requests) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":total,"mediaListEntry":base}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
            ])
            .await;
            let state = state(api);
            state
                .db
                .0
                .lock()
                .execute("UPDATE media SET episodes=? WHERE id=1", [total])
                .unwrap();
            state
                .db
                .0
                .lock()
                .execute(
                    "UPDATE list_entry SET progress=0, status='CURRENT' WHERE media_id=1",
                    [],
                )
                .unwrap();
            assert_eq!(state.db.episode_total(1).unwrap(), None);
            let entry = watcher_set_progress(&state, 1, 1, Some("test-session"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(entry.progress, 1);
            assert_eq!(entry.status, "CURRENT");
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 1);
        }
    }

    #[tokio::test]
    async fn superseded_rate_limited_mutations_leave_the_previous_local_intent() {
        for removal in [false, true] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(
                vec![(429, json!({})), (200, json!({}))],
                Some(gate.clone()),
            )
            .await;
            let state = state(api);
            if removal {
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
            let previous = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
            let editing = state.clone();
            let mut edit = tokio::spawn(async move {
                if removal {
                    delete_entry_inner(1, &editing).await
                } else {
                    set_progress_inner(&editing, 1, 11, Some(12))
                        .await
                        .map(drop)
                }
            });
            identity_request_while_running(&mut requests, &mut edit).await;
            if !removal {
                assert_eq!(get_entry(1, &state).await.unwrap().unwrap().progress, 11);
            }
            state.cancel_login();
            gate.add_permits(2);
            assert_eq!(edit.await.unwrap().unwrap_err(), AUTH_REPLACED);
            assert!(
                requests.try_recv().is_err(),
                "sent another request after supersession"
            );
            assert_eq!(
                serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                previous
            );
            assert_progress_preserved(&state);
        }
    }

    #[tokio::test]
    async fn superseded_rate_limited_confirmation_retains_the_dispatched_completion() {
        let mut committed = remote();
        committed["progress"] = json!(13);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![
                (200, json!({"data":{"SaveMediaListEntry":committed}})),
                (429, json!({})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":committed}}}),
                ),
            ],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let editing = state.clone();
        let mut task = tokio::spawn(async move {
            save_entry_inner(&editing, 1, Some("COMPLETED".into()), None, None, None).await
        });
        let mutation = identity_request_while_running(&mut requests, &mut task).await;
        assert_eq!(mutation["variables"]["progress"], 12);
        gate.add_permits(1);
        identity_request_while_running(&mut requests, &mut task).await;
        state.cancel_login();
        gate.add_permits(1);
        assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED);
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
        gate.add_permits(1);
        assert!(crate::sync_queue::sync_pending_changes(&state)
            .await
            .unwrap()
            .is_empty());
        let lookup = requests.try_recv().unwrap();
        assert!(lookup["query"].as_str().unwrap().starts_with("query"));
        assert!(requests.try_recv().is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 13);
    }

    #[tokio::test]
    async fn buffered_account_requests_do_not_adopt_a_new_auth_intent() {
        for operation in [
            "save",
            "progress",
            "remove",
            "details",
            "read details",
            "stats",
            "notifications",
        ] {
            let (api, mut requests) = anilist::mock_api(vec![]).await;
            let state = state(api);
            let guard = if matches!(operation, "stats" | "notifications") {
                state.entry_lock.lock().await
            } else {
                state.remote_lock.lock().await
            };
            let edit = async {
                match operation {
                    "save" => save_entry_inner(&state, 1, None, Some(11), None, None)
                        .await
                        .map(drop),
                    "progress" => set_progress_inner(&state, 1, 11, Some(12)).await.map(drop),
                    "remove" => delete_entry_inner(1, &state).await,
                    "read details" => get_entry_details_inner(1, &state).await.map(drop),
                    "stats" => get_user_stats_inner(&state).await.map(drop),
                    "notifications" => get_notifications_page_inner(1, &state).await.map(drop),
                    _ => update_entry_details_inner(
                        1,
                        Some("After".into()),
                        None,
                        None,
                        None,
                        &state,
                    )
                    .await
                    .map(drop),
                }
            };
            tokio::pin!(edit);
            tokio::select! {
                biased;
                result = &mut edit => panic!("{operation} bypassed the held lock: {result:?}"),
                _ = std::future::ready(()) => {}
            }
            state.cancel_login();
            drop(guard);

            assert!(edit.await.is_err(), "{operation}");
            assert!(requests.try_recv().is_err(), "{operation}");
            assert!(state.db.pending(7).unwrap().is_empty(), "{operation}");
            assert_progress_preserved(&state);
        }
    }

    #[tokio::test]
    async fn superseded_entry_preflights_do_not_send_mutations_or_replace_pending_state() {
        for operation in ["remove", "details", "queued score"] {
            let reply = match operation {
                "remove" => {
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":remote()}}})
                }
                "details" => {
                    json!({"data":{"Media":{"mediaListEntry":{"id":22,"mediaId":1,"notes":"Before","startedAt":null,"completedAt":null,"customLists":{}}},"Viewer":{"mediaListOptions":{"animeList":{"customLists":[]}}}}})
                }
                _ => score_profile("POINT_5"),
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![(200, reply)], Some(gate.clone())).await;
            let state = state(api);
            crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    score: Some(80.0),
                    ..Default::default()
                },
            )
            .unwrap();
            if operation == "remove" {
                state.db.clear_entries().unwrap();
            }
            let saved_pending = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
            let editing = state.clone();
            let mut edit = tokio::spawn(async move {
                match operation {
                    "remove" => delete_entry_inner(1, &editing).await,
                    "details" => update_entry_details_inner(
                        1,
                        Some("After".into()),
                        None,
                        None,
                        None,
                        &editing,
                    )
                    .await
                    .map(drop),
                    _ => crate::sync_queue::sync_pending_changes(&editing)
                        .await
                        .map(drop),
                }
            });
            let lookup = identity_request_while_running(&mut requests, &mut edit).await;
            assert!(lookup["query"].as_str().unwrap().starts_with("query"));
            state.cancel_login();
            gate.add_permits(1);

            assert!(edit.await.unwrap().is_err());
            assert!(requests.try_recv().is_err());
            assert_eq!(
                serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                saved_pending
            );
            assert_eq!(state.user.lock().as_ref().unwrap().name, "Tester");
            assert!(state.db.get_setting("entry_details:7:1").unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn score_rejection_waits_for_local_entry_writes() {
        let (api, mut requests) = anilist::mock_api(vec![(
            401,
            json!({"errors":[{"message":"Invalid token","status":401}]}),
        )])
        .await;
        let state = state(api);
        let pending = crate::sync_queue::Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: None,
            patch: crate::sync_queue::Patch {
                score: Some(80.0),
                ..Default::default()
            },
            error: None,
            conflict: false,
            missing_media: false,
            score_format: Some("POINT_100".into()),
            playback: false,
            base_score_format: Some("POINT_100".into()),
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        };
        let guard = state.entry_lock.lock().await;
        let worker = state.clone();
        let job = tokio::spawn(async move {
            let _remote = worker.remote_lock.lock().await;
            let al = worker.anilist.lock().clone();
            crate::sync_queue::check_score_format(&worker, &al, 7, &pending).await
        });
        requests.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            state.anilist.lock().token().is_some(),
            "rejection bypassed entry_lock"
        );
        drop(guard);
        assert!(job.await.unwrap().is_err());
        assert!(state.db.entries_with_media().unwrap().is_empty());
    }

    fn score_profile(format: &str) -> serde_json::Value {
        json!({"data":{"Viewer":{"id":7,"name":"Renamed","mediaListOptions":{"scoreFormat":format}}}})
    }

    fn score_snapshot(score: f64, more: bool, media_id: i64) -> serde_json::Value {
        let mut entry = remote();
        entry["id"] = json!(21 + media_id);
        entry["mediaId"] = json!(media_id);
        entry["score"] = json!(score);
        json!({"data":{"MediaListCollection":{"user":{"id":7},"hasNextChunk":more,"lists":[{"entries":[entry]}]}}})
    }

    fn statistics_reply(owner: serde_json::Value, count: i64) -> serde_json::Value {
        json!({"data":{"User":{"id":owner,"statistics":{"anime":{
            "count":count,"episodesWatched":36,"minutesWatched":864,
            "meanScore":80.0,"standardDeviation":4.0,
            "scores":[],"statuses":[],"formats":[],"genres":[],"releaseYears":[]
        }}}}})
    }

    fn conflicted_intent(state: &AppState) -> String {
        let (_, mut pending, _) = crate::sync_queue::stage(
            state,
            1,
            crate::sync_queue::Patch {
                progress: Some(11),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        pending.conflict = true;
        state.db.put_pending(7, &pending).unwrap();
        serde_json::to_string(&pending).unwrap()
    }

    async fn identity_request_while_running<T: std::fmt::Debug>(
        requests: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        task: &mut tokio::task::JoinHandle<T>,
    ) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                request = requests.recv() => request.expect("identity request channel closed"),
                result = task => panic!("identity task ended before the expected request: {result:?}"),
            }
        })
        .await
        .expect("identity request timed out")
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "identity task ended before the expected request")]
    async fn identity_request_wait_fails_when_the_task_ends_without_a_request() {
        let (_sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let mut task = tokio::spawn(async { Err::<(), _>("stopped before sending") });
        identity_request_while_running(&mut requests, &mut task).await;
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "identity request timed out")]
    async fn identity_request_wait_fails_when_the_task_stalls() {
        let (_sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let mut task = tokio::spawn(std::future::pending::<()>());
        identity_request_while_running(&mut requests, &mut task).await;
    }

    #[tokio::test]
    async fn identity_sync_uses_id_when_the_old_name_fails_or_points_elsewhere() {
        for old_name in ["missing", "reassigned", "absent"] {
            let (api, mut requests) = anilist::mock_api_mapped(
                vec![
                    (200, score_snapshot(90.0, false, 1)),
                    (200, score_profile("POINT_100")),
                ],
                None,
                move |request, reply| {
                    if request["variables"].get("userName").is_some() {
                        *reply = if old_name == "missing" {
                            json!({"errors":[{"message":"User not found","status":404}]})
                        } else {
                            let mut other = score_snapshot(90.0, false, 2);
                            other["data"]["MediaListCollection"]["user"]["id"] = json!(9);
                            other
                        };
                    }
                },
            )
            .await;
            let state = state(api);
            if old_name == "absent" {
                state.db.delete_setting(USERNAME_KEY).unwrap();
            }
            let entries = sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].media_id, 1);
            assert_eq!(state.db.cached_user().unwrap().unwrap().name, "Renamed");
            let request = requests.try_recv().expect("expected identity request");
            assert_eq!(request["variables"]["accountId"], 7);
            assert!(request["query"]
                .as_str()
                .unwrap()
                .contains("userId: $accountId"));
            requests.try_recv().expect("expected identity request");
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn identity_sync_rejects_unproven_owners_on_every_chunk() {
        for owner in [
            None,
            Some(json!(null)),
            Some(json!({})),
            Some(json!({"id":null})),
            Some(json!({"id":9})),
        ] {
            for later in [false, true] {
                let mut invalid = if later {
                    score_snapshot(90.0, false, 3)
                } else {
                    json!({"data":{"MediaListCollection":{"lists":[],"hasNextChunk":false}}})
                };
                invalid["data"]["MediaListCollection"]
                    .as_object_mut()
                    .unwrap()
                    .remove("user");
                if let Some(owner) = &owner {
                    invalid["data"]["MediaListCollection"]["user"] = owner.clone();
                }
                let mut replies = vec![];
                if later {
                    replies.push((200, score_snapshot(90.0, true, 2)));
                }
                let mut profile = score_profile("POINT_100");
                profile["data"]["Viewer"]["name"] = json!("Tester");
                replies.extend([(200, invalid), (200, profile)]);
                let (api, _) = anilist::mock_api(replies).await;
                let state = state(api);
                let pending = conflicted_intent(&state);
                assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                    .await
                    .is_err());
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
                assert!(state.db.get_entry(2).unwrap().is_none());
                assert!(state.anilist.lock().has_token());
                assert_eq!(
                    serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                    pending
                );
            }
        }
    }

    #[tokio::test]
    async fn sync_refreshes_snapshot_matchers_when_pending_overlay_fails() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"MediaListCollection":{"user":{"id":7},"lists":[],"hasNextChunk":false}}})),
            (200, score_profile("POINT_100")),
        ]).await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Synthetic Show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state.refresh_matchers();
        assert_eq!(state.matchers.lock()[0].media_id, 1);
        let intent = conflicted_intent(&state);
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_overlay BEFORE INSERT ON list_entry
             BEGIN SELECT RAISE(ABORT, 'synthetic overlay failure'); END;",
            )
            .unwrap();

        let error = sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .unwrap_err();
        assert!(error.contains("synthetic overlay failure"));
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(
            serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
            intent
        );
        assert!(
            state.matchers.lock().is_empty(),
            "removed anime remains matchable"
        );

        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER refuse_overlay")
            .unwrap();
        let entries = local_entries(&state).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].media_id, entries[0].progress), (1, 11));
        assert_eq!(state.matchers.lock()[0].media_id, 1);
    }

    #[tokio::test]
    async fn identity_sync_accepts_an_empty_owned_list_and_preserves_pending_overlay() {
        for pending in [false, true] {
            let (api, _) = anilist::mock_api(vec![
                (200, json!({"data":{"MediaListCollection":{"user":{"id":7},"lists":[],"hasNextChunk":false}}})),
                (200, score_profile("POINT_100")),
            ]).await;
            let state = state(api);
            let intent = pending.then(|| conflicted_intent(&state));
            let entries = sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .unwrap();
            if let Some(intent) = intent {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].progress, 11);
                assert_eq!(
                    serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                    intent
                );
            } else {
                assert!(entries.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn identity_sync_failures_preserve_snapshot_and_pending_intent() {
        for failure in ["id404", "later503", "viewer503", "viewer_mismatch"] {
            let replies = match failure {
                "id404" => vec![(
                    404,
                    json!({"errors":[{"message":"User not found","status":404}]}),
                )],
                "later503" => vec![
                    (200, score_snapshot(90.0, true, 2)),
                    (503, json!({"errors":[{"message":"Unavailable"}]})),
                ],
                "viewer503" => vec![
                    (200, score_snapshot(90.0, false, 2)),
                    (503, json!({"errors":[{"message":"Unavailable"}]})),
                ],
                _ => vec![
                    (200, score_snapshot(90.0, false, 2)),
                    (200, json!({"data":{"Viewer":{"id":9,"name":"Other"}}})),
                ],
            };
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api);
            let pending = conflicted_intent(&state);
            assert!(
                sync_my_list_inner(&state, ListPreferences::Preserve)
                    .await
                    .is_err(),
                "{failure}"
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
            assert!(state.db.get_entry(2).unwrap().is_none());
            assert!(state.anilist.lock().has_token());
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
            assert_eq!(
                serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                pending
            );
            while let Ok(request) = requests.try_recv() {
                assert!(request["variables"].get("userName").is_none());
            }
        }
    }

    #[tokio::test]
    async fn identity_sync_adoption_failure_keeps_local_data() {
        let (api, mut requests) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        let pending = conflicted_intent(&state);
        state.db.delete_setting(USER_ID_KEY).unwrap();
        assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .is_err());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
        assert_eq!(
            serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
            pending
        );
        assert!(state.anilist.lock().has_token());
        assert!(
            requests.try_recv().expect("expected identity request")["query"]
                .as_str()
                .unwrap()
                .contains("Viewer")
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn identity_sync_does_not_adopt_an_invalid_viewer_id() {
        for id in [json!(0), json!(-1), json!(2147483648_i64), json!(null)] {
            let mut profile = score_profile("POINT_100");
            profile["data"]["Viewer"]["id"] = id;
            let (api, mut requests) = anilist::mock_api(vec![(200, profile)]).await;
            let state = state(api);
            state.db.delete_setting(USER_ID_KEY).unwrap();
            assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
            assert!(state.db.get_setting(USER_ID_KEY).unwrap().is_none());
            assert!(state.anilist.lock().has_token());
            requests.try_recv().expect("expected identity request");
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn identity_sync_adopts_viewer_before_querying_without_a_saved_id() {
        for saved_id in [None, Some("0"), Some("invalid"), Some("2147483648")] {
            let (api, mut requests) = anilist::mock_api(vec![
                (200, score_profile("POINT_100")),
                (200, score_snapshot(90.0, false, 1)),
                (200, score_profile("POINT_100")),
            ])
            .await;
            let state = state(api);
            state.db.delete_setting(USER_ID_KEY).unwrap();
            if let Some(id) = saved_id {
                state.db.set_setting(USER_ID_KEY, id).unwrap();
            }
            assert_eq!(
                sync_my_list_inner(&state, ListPreferences::Preserve)
                    .await
                    .unwrap()[0]
                    .score,
                Some(90.0)
            );
            assert!(
                requests.try_recv().expect("expected identity request")["query"]
                    .as_str()
                    .unwrap()
                    .contains("Viewer")
            );
            assert_eq!(
                requests.try_recv().expect("expected identity request")["variables"]["accountId"],
                7
            );
            requests.try_recv().expect("expected identity request");
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn identity_statistics_use_id_and_reject_unproven_owners() {
        for owner in [Some(json!(9)), Some(json!(null)), None, Some(json!(7))] {
            let mut reply = statistics_reply(owner.clone().unwrap_or_default(), 99);
            if owner.is_none() {
                reply["data"]["User"].as_object_mut().unwrap().remove("id");
            }
            let (api, mut requests) = anilist::mock_api(vec![(200, reply)]).await;
            let state = state(api);
            let cached = serde_json::to_string(&UserStats {
                count: 4,
                cached_at: Some(100),
                ..Default::default()
            })
            .unwrap();
            state.db.set_setting("user_stats:7", &cached).unwrap();
            let stats = get_user_stats_inner(&state).await.unwrap();
            if owner == Some(json!(7)) {
                assert_eq!(stats.count, 99);
                assert!(stats.warning.is_none());
                let stored: serde_json::Value =
                    serde_json::from_str(&state.db.get_setting("user_stats:7").unwrap().unwrap())
                        .unwrap();
                assert_eq!(stored["owner_id"], 7);
            } else {
                assert_eq!(stats.count, 4);
                assert_eq!(stats.cached_at, Some(100));
                assert!(stats.warning.as_deref().unwrap().contains("ownership"));
                assert_eq!(
                    state.db.get_setting("user_stats:7").unwrap().as_deref(),
                    Some(cached.as_str())
                );
            }
            let request = requests.try_recv().expect("expected identity request");
            assert_eq!(request["variables"]["accountId"], 7);
            assert!(request["query"]
                .as_str()
                .unwrap()
                .contains("User(id: $accountId)"));
            assert!(state.anilist.lock().has_token());
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn identity_statistics_recheck_saved_account_after_the_request() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, statistics_reply(json!(7), 99))],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let reading = state.clone();
        let mut job = tokio::spawn(async move { get_user_stats_inner(&reading).await });
        identity_request_while_running(&mut requests, &mut job).await;
        state.db.set_setting(USER_ID_KEY, "9").unwrap();
        gate.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .expect("identity task timed out")
            .unwrap()
            .is_err());
        assert!(state.db.get_setting("user_stats:7").unwrap().is_none());
        assert!(state.db.get_setting("user_stats:9").unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_login_aborts_in_flight_current_user_before_it_changes_the_account() {
        for response in [
            (200, json!({"data":{"Viewer":{"id":8,"name":"Other"}}})),
            (
                401,
                json!({"errors":[{"message":"Invalid token","status":401}]}),
            ),
        ] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
            let state = state(api);
            state.user.lock().take();
            let reader = state.clone();
            let mut job = tokio::spawn(async move { current_user_inner(&reader).await });
            identity_request_while_running(&mut requests, &mut job).await;

            state.cancel_login();

            let result = tokio::time::timeout(Duration::from_secs(1), &mut job).await;
            gate.add_permits(1);
            let result = match result {
                Ok(result) => result.unwrap(),
                Err(_) => {
                    let late = tokio::time::timeout(Duration::from_secs(5), job)
                        .await
                        .expect("current_user stalled after the Viewer response")
                        .unwrap();
                    panic!(
                        "current_user waited for the Viewer response after cancellation: {late:?}"
                    );
                }
            };
            assert_eq!(result.unwrap_err(), AUTH_REPLACED);
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
            assert_eq!(
                state.db.get_setting(TOKEN_KEY).unwrap().as_deref(),
                Some("test-session")
            );
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
            assert!(state.user.lock().is_none());
        }
    }

    #[tokio::test]
    async fn identity_reads_reject_replaced_sessions_and_cancelled_auth_intent() {
        for list in [false, true] {
            for replacement in ["account", "token", "intent"] {
                for rejected in [false, true] {
                    let response = if rejected {
                        (
                            401,
                            json!({"errors":[{"message":"Invalid token","status":401}]}),
                        )
                    } else if list {
                        (200, score_snapshot(90.0, false, 2))
                    } else {
                        (200, statistics_reply(json!(7), 99))
                    };
                    let gate = Arc::new(tokio::sync::Semaphore::new(0));
                    let (api, mut requests) =
                        anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
                    let state = state(api);
                    let reading = state.clone();
                    let mut job = tokio::spawn(async move {
                        if list {
                            sync_my_list_inner(&reading, ListPreferences::Preserve)
                                .await
                                .map(|_| ())
                        } else {
                            get_user_stats_inner(&reading).await.map(|_| ())
                        }
                    });
                    identity_request_while_running(&mut requests, &mut job).await;
                    match replacement {
                        "account" => state.db.set_setting(USER_ID_KEY, "9").unwrap(),
                        "token" => state
                            .anilist
                            .lock()
                            .set_token(Some("replacement-session".into())),
                        _ => {
                            state.begin_auth_intent();
                        }
                    }
                    gate.add_permits(1);
                    assert!(tokio::time::timeout(Duration::from_secs(5), job)
                        .await
                        .expect("identity task timed out")
                        .unwrap()
                        .is_err());
                    assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
                    assert!(state.db.get_entry(2).unwrap().is_none());
                    assert!(state.db.get_setting("user_stats:7").unwrap().is_none());
                    assert!(state.anilist.lock().has_token());
                    assert!(requests.try_recv().is_err());
                }
            }
        }
    }

    #[tokio::test]
    async fn identity_statistics_keep_verified_cache_during_outages() {
        for failure in ["503", "html403", "timeout", "auth"] {
            let response = match failure {
                "html403" => (403, json!("<html>Cloudflare unavailable</html>")),
                "auth" => (
                    401,
                    json!({"errors":[{"message":"Invalid token","status":401}]}),
                ),
                _ => (503, json!({"errors":[{"message":"Unavailable"}]})),
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(1));
            let (api, requests) = anilist::mock_api_gated(
                vec![(200, statistics_reply(json!(7), 4)), response],
                (failure == "timeout").then_some(gate),
            )
            .await;
            let state = state(api);
            let pending = conflicted_intent(&state);
            let fresh = get_user_stats_inner(&state).await.unwrap();
            let cached = state.db.get_setting("user_stats:7").unwrap().unwrap();
            let result = get_user_stats_inner(&state).await;
            if failure == "auth" {
                assert_eq!(result.unwrap_err(), SESSION_EXPIRED);
                assert!(!state.anilist.lock().has_token());
                assert!(state.db.entries_with_media().unwrap().is_empty());
            } else {
                let stats = result.unwrap();
                assert_eq!(stats.count, 4);
                assert_eq!(stats.cached_at, fresh.cached_at);
                assert!(!stats.warning.as_deref().unwrap().contains("ownership"));
                assert!(state.anilist.lock().has_token());
                assert!(state.db.get_entry(1).unwrap().is_some());
            }
            assert_eq!(
                state.db.get_setting("user_stats:7").unwrap().as_deref(),
                Some(cached.as_str())
            );
            assert_eq!(
                serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                pending
            );
            drop(requests);
        }
    }

    #[tokio::test]
    async fn identity_statistics_cache_provenance_is_not_inferred_from_its_key() {
        for owner in [None, Some(7), Some(9)] {
            let (api, _) =
                anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
            let state = state(api);
            let mut cached = serde_json::to_value(UserStats {
                count: 4,
                cached_at: Some(100),
                ..Default::default()
            })
            .unwrap();
            if let Some(owner) = owner {
                cached["owner_id"] = json!(owner);
            }
            let cached = cached.to_string();
            state.db.set_setting("user_stats:7", &cached).unwrap();
            let result = get_user_stats_inner(&state).await;
            if owner == Some(9) {
                assert!(result.is_err());
            } else {
                let stats = result.unwrap();
                assert_eq!(stats.count, 4);
                assert_eq!(stats.cached_at, Some(100));
                assert_eq!(
                    stats.warning.unwrap().contains("ownership"),
                    owner.is_none()
                );
            }
            assert_eq!(
                state.db.get_setting("user_stats:7").unwrap().as_deref(),
                Some(cached.as_str())
            );
        }
    }

    #[tokio::test]
    async fn identity_statistics_late_results_cannot_change_a_new_login() {
        for rejected in [false, true] {
            let response = if rejected {
                (
                    401,
                    json!({"errors":[{"message":"Invalid token","status":401}]}),
                )
            } else {
                (200, statistics_reply(json!(7), 99))
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut old_requests) =
                anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
            let state = state(api);
            let pending = conflicted_intent(&state);
            let old_cache = serde_json::to_string(&UserStats {
                count: 4,
                cached_at: Some(100),
                ..Default::default()
            })
            .unwrap();
            state.db.set_setting("user_stats:7", &old_cache).unwrap();
            let reading = state.clone();
            let mut job = tokio::spawn(async move { get_user_stats(&reading).await });
            identity_request_while_running(&mut old_requests, &mut job).await;

            let (api, mut new_requests) = anilist::mock_api(vec![
                (200, json!({"data":{"Viewer":{"id":9,"name":"Other"}}})),
                (200, statistics_reply(json!(9), 6)),
            ])
            .await;
            *state.anilist.lock() = api;
            login_with_token(
                "replacement-session".into(),
                &state,
                ListPreferences::Preserve,
            )
            .await
            .unwrap();
            assert_eq!(get_user_stats(&state).await.unwrap().count, 6);
            let new_cache = state.db.get_setting("user_stats:9").unwrap().unwrap();
            gate.add_permits(1);
            assert!(tokio::time::timeout(Duration::from_secs(5), job)
                .await
                .expect("identity task timed out")
                .unwrap()
                .is_err());
            assert_eq!(
                state.db.get_setting("user_stats:7").unwrap(),
                Some(old_cache)
            );
            assert_eq!(
                state.db.get_setting("user_stats:9").unwrap(),
                Some(new_cache)
            );
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 9);
            assert_eq!(
                state.anilist.lock().token().as_deref(),
                Some("replacement-session")
            );
            assert!(state.last_auth_expired.lock().is_none());
            assert_eq!(
                serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                pending
            );
            assert!(state.db.pending(9).unwrap().is_empty());
            assert!(
                new_requests.try_recv().expect("expected identity request")["query"]
                    .as_str()
                    .unwrap()
                    .contains("Viewer")
            );
            assert_eq!(
                new_requests.try_recv().expect("expected identity request")["variables"]
                    ["accountId"],
                9
            );
            assert!(new_requests.try_recv().is_err());
            assert!(old_requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn identity_rename_does_not_certify_legacy_statistics() {
        let unavailable = (503, json!({"errors":[{"message":"Unavailable"}]}));
        let (api, _) = anilist::mock_api(vec![
            (200, score_snapshot(70.0, false, 1)),
            (200, score_profile("POINT_100")),
            unavailable.clone(),
            (200, statistics_reply(json!(7), 6)),
            unavailable,
        ])
        .await;
        let state = state(api);
        *state.user.lock() = None;
        let legacy = serde_json::to_string(&UserStats {
            count: 4,
            cached_at: Some(100),
            ..Default::default()
        })
        .unwrap();
        state.db.set_setting("user_stats:7", &legacy).unwrap();
        sync_my_list(&state).await.unwrap();
        assert_eq!(state.db.cached_user().unwrap().unwrap().name, "Renamed");
        let cached = get_user_stats(&state).await.unwrap();
        assert_eq!((cached.count, cached.cached_at), (4, Some(100)));
        assert!(cached.warning.unwrap().contains("ownership"));
        assert_eq!(state.db.get_setting("user_stats:7").unwrap(), Some(legacy));

        let fresh = get_user_stats(&state).await.unwrap();
        assert_eq!(fresh.count, 6);
        assert!(fresh.warning.is_none());
        let verified = state.db.get_setting("user_stats:7").unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<UserStats>(&verified).unwrap().count,
            6
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&verified).unwrap()["owner_id"],
            7
        );
        let cached = get_user_stats(&state).await.unwrap();
        assert_eq!(cached.cached_at, fresh.cached_at);
        assert!(!cached.warning.unwrap().contains("ownership"));
        assert_eq!(
            state.db.get_setting("user_stats:7").unwrap(),
            Some(verified)
        );
    }

    #[tokio::test]
    async fn identity_missing_id_recovery_keeps_pending_changes_with_their_owner() {
        let mut profile = score_profile("POINT_100");
        profile["data"]["Viewer"]["id"] = json!(9);
        let mut snapshot = score_snapshot(90.0, false, 1);
        snapshot["data"]["MediaListCollection"]["user"]["id"] = json!(9);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, profile.clone()),
            (200, snapshot),
            (200, profile),
        ])
        .await;
        let state = state(api);
        let pending = conflicted_intent(&state);
        state.db.delete_setting(USER_ID_KEY).unwrap();
        let entries = sync_my_list(&state).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].score, Some(90.0));
        assert_eq!(entries[0].progress, remote()["progress"].as_i64().unwrap());
        assert_eq!(state.db.cached_user().unwrap().unwrap().id, 9);
        assert!(state.db.pending(9).unwrap().is_empty());
        assert_eq!(
            serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
            pending
        );
        assert!(
            requests.try_recv().expect("expected identity request")["query"]
                .as_str()
                .unwrap()
                .contains("Viewer")
        );
        assert_eq!(
            requests.try_recv().expect("expected identity request")["variables"]["accountId"],
            9
        );
        requests.try_recv().expect("expected identity request");
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn identity_sync_late_chunks_and_viewer_cannot_disrupt_a_waiting_login() {
        for later_chunk in [false, true] {
            for rejected in [false, true] {
                let response = if rejected {
                    (
                        401,
                        json!({"errors":[{"message":"Invalid token","status":401}]}),
                    )
                } else if later_chunk {
                    (200, score_snapshot(90.0, false, 3))
                } else {
                    (200, score_profile("POINT_100"))
                };
                let gate = Arc::new(tokio::sync::Semaphore::new(1));
                let (api, mut old_requests) = anilist::mock_api_gated(
                    vec![(200, score_snapshot(90.0, later_chunk, 2)), response],
                    Some(gate.clone()),
                )
                .await;
                let state = state(api);
                let pending = conflicted_intent(&state);
                let syncing = state.clone();
                let mut sync = tokio::spawn(async move { sync_my_list(&syncing).await });
                identity_request_while_running(&mut old_requests, &mut sync).await;
                let blocked = identity_request_while_running(&mut old_requests, &mut sync).await;
                assert_eq!(
                    blocked["query"]
                        .as_str()
                        .unwrap()
                        .contains("MediaListCollection"),
                    later_chunk
                );

                let (api, mut new_requests) = anilist::mock_api(vec![(
                    200,
                    json!({"data":{"Viewer":{"id":9,"name":"Other"}}}),
                )])
                .await;
                *state.anilist.lock() = api;
                let signing_in = state.clone();
                let mut login = tokio::spawn(async move {
                    login_with_token(
                        "replacement-session".into(),
                        &signing_in,
                        ListPreferences::Preserve,
                    )
                    .await
                });
                assert!(
                    identity_request_while_running(&mut new_requests, &mut login).await["query"]
                        .as_str()
                        .unwrap()
                        .contains("Viewer")
                );
                assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
                assert!(!login.is_finished());
                gate.add_permits(1);
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), sync)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap_err(),
                    AUTH_REPLACED
                );
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), login)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                        .id,
                    9
                );
                assert_eq!(state.db.cached_user().unwrap().unwrap().id, 9);
                assert_eq!(
                    state.anilist.lock().token().as_deref(),
                    Some("replacement-session")
                );
                assert!(state.last_auth_expired.lock().is_none());
                assert!(state.db.entries_with_media().unwrap().is_empty());
                assert_eq!(
                    serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap(),
                    pending
                );
                assert!(state.db.pending(9).unwrap().is_empty());
                assert!(old_requests.try_recv().is_err());
                assert!(new_requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn identity_statistics_cache_ownership_survives_reopening_the_database() {
        let dir = std::env::temp_dir().join(format!(
            "kurisu-identity-statistics-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        for owner in [None, Some(7), Some(9)] {
            let path = dir.join(format!("stats-{}.db", owner.unwrap_or(0)));
            let stored = {
                let (api, _) = anilist::mock_api(vec![(200, statistics_reply(json!(7), 4))]).await;
                let mut state = state(api);
                Arc::get_mut(&mut state).unwrap().db = Arc::new(Db::open(&path).unwrap());
                state
                    .db
                    .replace_account("test-session", state.user.lock().as_ref().unwrap())
                    .unwrap();
                if owner == Some(7) {
                    get_user_stats(&state).await.unwrap();
                } else {
                    let mut cache = serde_json::to_value(UserStats {
                        count: 4,
                        cached_at: Some(100),
                        ..Default::default()
                    })
                    .unwrap();
                    if let Some(owner) = owner {
                        cache["owner_id"] = json!(owner);
                    }
                    state
                        .db
                        .set_setting("user_stats:7", &cache.to_string())
                        .unwrap();
                }
                state.db.get_setting("user_stats:7").unwrap().unwrap()
            };
            {
                let (api, _) =
                    anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))])
                        .await;
                let mut state = state(api);
                Arc::get_mut(&mut state).unwrap().db = Arc::new(Db::open(&path).unwrap());
                *state.user.lock() = None;
                let result = get_user_stats(&state).await;
                if owner == Some(9) {
                    assert!(result.is_err());
                } else {
                    let cached = result.unwrap();
                    let previous: UserStats = serde_json::from_str(&stored).unwrap();
                    assert_eq!(cached.count, previous.count);
                    assert_eq!(cached.cached_at, previous.cached_at);
                    assert_eq!(
                        cached.warning.unwrap().contains("ownership"),
                        owner.is_none()
                    );
                }
                assert_eq!(state.db.get_setting("user_stats:7").unwrap(), Some(stored));
                assert!(state.anilist.lock().has_token());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sync_refreshes_a_renamed_profile_without_refetching_by_name() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_snapshot(70.0, false, 1)),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        let entries = sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].media_id, 1);
        assert!(state.db.get_entry(2).unwrap().is_none());
        assert_eq!(state.db.cached_user().unwrap().unwrap().name, "Renamed");
        assert_eq!(requests.recv().await.unwrap()["variables"]["accountId"], 7);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn ordinary_sync_refreshes_the_score_profile_and_scores_together() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_snapshot(80.0, false, 1)),
            (200, score_profile("POINT_5")),
            (200, score_snapshot(4.0, false, 1)),
            (200, score_profile("POINT_5")),
        ])
        .await;
        let state = state(api);
        let entries = sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .unwrap();
        assert_eq!(entries[0].score, Some(4.0));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(4.0));
        for source in [state.user.lock().clone(), state.db.cached_user().unwrap()] {
            assert_eq!(source.unwrap().score_format.as_deref(), Some("POINT_5"));
        }
        let first = requests.recv().await.unwrap();
        assert_eq!(first["variables"]["scoreFormat"], "POINT_100");
        assert!(first["query"]
            .as_str()
            .unwrap()
            .contains("score(format: $scoreFormat)"));
        requests.recv().await.unwrap();
        let fresh = requests.recv().await.unwrap();
        assert_eq!(fresh["variables"]["scoreFormat"], "POINT_5");
        assert_eq!(fresh["variables"]["accountId"], 7);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn snapshot_write_failure_preserves_scores_and_profile() {
        let (api, _) = anilist::mock_api(vec![
            (200, score_snapshot(80.0, false, 1)),
            (200, score_profile("POINT_5")),
            (200, score_snapshot(4.0, false, 1)),
            (200, score_profile("POINT_5")),
        ])
        .await;
        let state = state(api);
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_snapshot BEFORE INSERT ON list_entry
             BEGIN SELECT RAISE(ABORT, 'snapshot rejected'); END;",
            )
            .unwrap();
        let error = sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .unwrap_err();
        assert!(error.contains("snapshot rejected"), "{error}");
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
        for profile in [state.user.lock().clone(), state.db.cached_user().unwrap()] {
            assert_eq!(profile.unwrap().score_format.as_deref(), Some("POINT_100"));
        }
    }

    #[tokio::test]
    async fn sync_recovers_a_legacy_profile_before_publishing_scores() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_snapshot(80.0, false, 1)),
            (200, score_profile("POINT_100")),
            (200, score_snapshot(80.0, false, 1)),
            (200, score_profile("POINT_100")),
        ])
        .await;
        let state = state(api);
        state.db.delete_setting(PROFILE_KEY).unwrap();
        assert_eq!(
            sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .unwrap()[0]
                .score,
            Some(80.0)
        );
        assert_eq!(
            state
                .db
                .cached_user()
                .unwrap()
                .unwrap()
                .score_format
                .as_deref(),
            Some("POINT_100")
        );
        assert!(requests.recv().await.unwrap()["variables"]["scoreFormat"].is_null());
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"]["scoreFormat"],
            "POINT_100"
        );
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn sync_retries_another_format_change_and_pins_every_chunk() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, score_snapshot(80.0, false, 1)),
            (200, score_profile("POINT_5")),
            (200, score_snapshot(4.0, false, 1)),
            (200, score_profile("POINT_10_DECIMAL")),
            (200, score_snapshot(8.0, true, 1)),
            (200, score_snapshot(9.0, false, 2)),
            (200, score_profile("POINT_10_DECIMAL")),
        ])
        .await;
        let state = state(api);
        let entries = sync_my_list_inner(&state, ListPreferences::Preserve)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(8.0));
        assert_eq!(state.db.get_entry(2).unwrap().unwrap().score, Some(9.0));
        let mut chunks = Vec::new();
        while let Ok(request) = requests.try_recv() {
            if request["query"]
                .as_str()
                .unwrap()
                .contains("MediaListCollection")
            {
                chunks.push(request["variables"].clone());
            }
        }
        assert_eq!(
            chunks
                .iter()
                .map(|v| v["scoreFormat"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "POINT_100",
                "POINT_5",
                "POINT_10_DECIMAL",
                "POINT_10_DECIMAL"
            ]
        );
        assert_eq!(chunks[3]["chunk"], 2);
    }

    #[tokio::test]
    async fn unstable_sync_or_failed_refetch_preserves_the_previous_snapshot() {
        for refetch_fails in [false, true] {
            let mut replies = vec![
                (200, score_snapshot(80.0, false, 2)),
                (200, score_profile("POINT_5")),
            ];
            if refetch_fails {
                replies.push((503, json!({"errors":[{"message":"Unavailable"}]})));
            } else {
                replies.extend([
                    (200, score_snapshot(4.0, false, 2)),
                    (200, score_profile("POINT_10_DECIMAL")),
                    (200, score_snapshot(8.0, false, 2)),
                    (200, score_profile("POINT_3")),
                ]);
            }
            let count = replies.len();
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api);
            assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
            assert!(state.db.get_entry(2).unwrap().is_none());
            let profile = state.db.cached_user().unwrap().unwrap();
            assert_eq!(profile.name, "Tester");
            assert_eq!(profile.score_format.as_deref(), Some("POINT_100"));
            for _ in 0..count {
                requests.recv().await.unwrap();
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn foreground_score_postflight_retains_original_intent() {
        for failure in [false, true] {
            let mut saved = remote();
            saved["score"] = json!(4);
            saved["progress"] = json!(13);
            let postflight = if failure {
                (503, json!({"errors":[{"message":"Unavailable"}]}))
            } else {
                (200, score_profile("POINT_10_DECIMAL"))
            };
            let (api, mut requests) = anilist::mock_api(vec![
                (200, score_profile("POINT_100")),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":saved}}}),
                ),
                postflight,
            ])
            .await;
            let state = state(api);
            let entry =
                save_entry_inner(&state, 1, Some("COMPLETED".into()), None, Some(4.0), None)
                    .await
                    .unwrap();
            assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
            assert_eq!(entry.progress, 13);
            assert_eq!(entry.score, if failure { Some(4.0) } else { None });
            let pending = state.db.pending(7).unwrap().remove(0);
            assert_eq!(pending.patch.score, Some(4.0));
            assert_eq!(pending.score_format.as_deref(), Some("POINT_100"));
            assert_eq!(pending.conflict, !failure);
            assert!(pending.error.is_some());
            requests.recv().await.unwrap();
            assert_eq!(requests.recv().await.unwrap()["variables"]["score"], 4.0);
            assert_eq!(requests.recv().await.unwrap()["variables"]["mediaId"], 1);
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn sync_profile_failures_do_not_publish_a_snapshot() {
        for (status, profile) in [
            (
                401,
                json!({"errors":[{"message":"Invalid token","status":401}]}),
            ),
            (500, json!({"errors":[{"message":"Unavailable"}]})),
            (200, json!({"data":{"Viewer":{"id":8,"name":"Other"}}})),
        ] {
            let (api, _) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"MediaListCollection":{"user":{"id":7},"hasNextChunk":false,"lists":[]}}}),
                ),
                (status, profile),
            ])
            .await;
            let state = state(api);
            assert!(sync_my_list_inner(&state, ListPreferences::Preserve)
                .await
                .is_err());
            if status == 401 {
                assert!(state.db.entries_with_media().unwrap().is_empty());
                assert!(state.user.lock().is_none());
            } else {
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(70.0));
                assert_eq!(state.user.lock().as_ref().unwrap().id, 7);
            }
        }
    }

    #[tokio::test]
    async fn signed_out_mutations_are_rejected_without_touching_cached_entries() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api);
        let before = state.db.get_entry(1).unwrap().unwrap().progress;
        state.anilist.lock().set_token(None);
        assert!(delete_entry_cmd(1, &state).await.is_err());
        assert!(set_progress_inner(&state, 1, 5, Some(before))
            .await
            .is_err());
        assert!(save_entry_inner(&state, 1, None, Some(5), None, None)
            .await
            .is_err());
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, before);
    }

    #[tokio::test]
    async fn rewatch_completion_clamps_repeat_at_maximum() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "REPEATING".into();
        entry.progress = 11;
        entry.repeat = 1000;
        state.db.upsert_entry(&entry).unwrap();
        let next = compute_set_progress(&state, 1, 12).unwrap();
        assert_eq!(next.status, "COMPLETED");
        assert_eq!(next.repeat, 1000);
        validate_entry_values(Some(next.progress), None, Some(next.repeat)).unwrap();
    }

    #[tokio::test]
    async fn auth_intent_change_cancels_in_flight_user_stats_promptly() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) = anilist::mock_api_gated(
            vec![(200, statistics_reply(json!(7), 99))],
            Some(gate.clone()),
        )
        .await;
        let state = state(api);
        let reading = state.clone();
        let mut job = tokio::spawn(async move { get_user_stats_inner(&reading).await });
        identity_request_while_running(&mut requests, &mut job).await;

        state.begin_auth_intent();

        let result = tokio::time::timeout(Duration::from_secs(1), &mut job).await;
        let result = match result {
            Ok(result) => {
                drop(requests);
                result.unwrap()
            }
            Err(_) => {
                gate.add_permits(1);
                let late = tokio::time::timeout(Duration::from_secs(5), job)
                    .await
                    .expect("get_user_stats stalled after the reply")
                    .unwrap();
                panic!("get_user_stats waited for the server after auth intent changed: {late:?}");
            }
        };
        assert_eq!(result.unwrap_err(), AUTH_REPLACED);
        assert!(state.db.get_setting("user_stats:7").unwrap().is_none());
    }

    #[tokio::test]
    async fn untracked_save_missing_media_commits_once_without_full_pending_refresh() {
        let lookup = (
            200,
            json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":null}}}),
        );
        let missing = (404, json!({"errors":[{"message":"Not Found"}]}));

        let media = Media {
            id: 1,
            title_romaji: Some("Show".into()),
            episodes: Some(12),
            ..Default::default()
        };

        {
            let (api, mut requests) =
                anilist::mock_api(vec![lookup.clone(), missing.clone()]).await;
            let state = state(api);
            state.db.upsert_media(&media).unwrap();
            state.db.delete_entry(1).unwrap();
            state
                .db
                .set_setting("media_detail:1", "{\"media\":{\"id\":1}}")
                .unwrap();
            assert!(state.db.get_media(1).unwrap().is_some());
            let mut events = state.events.subscribe();
            let commits = crate::db::durable_commit_count();
            assert_eq!(
                save_entry_inner(&state, 1, Some("CURRENT".into()), Some(1), None, None)
                    .await
                    .unwrap_err(),
                MEDIA_GONE
            );
            assert_eq!(crate::db::durable_commit_count() - commits, 2);
            let mut pending_events = Vec::new();
            while let Ok(event) = events.try_recv() {
                if let crate::EventKind::PendingChanged(ids) = event.kind {
                    pending_events.push(ids);
                }
            }
            assert_eq!(pending_events, vec![Some(vec![1])]);
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_entry(1).unwrap().is_none());
            assert!(state.db.get_media(1).unwrap().is_none());
            assert_eq!(
                state.db.get_setting("media_detail:1").unwrap().as_deref(),
                Some("null")
            );
            assert!(requests.try_recv().is_ok());
            assert!(requests.try_recv().is_ok());
            assert!(requests.try_recv().is_err());
        }

        {
            let gate = Arc::new(tokio::sync::Semaphore::new(1));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![lookup, missing], Some(gate.clone())).await;
            let state = state(api);
            state.db.upsert_media(&media).unwrap();
            state.db.delete_entry(1).unwrap();
            state
                .db
                .set_setting("media_detail:1", "{\"media\":{\"id\":1}}")
                .unwrap();
            assert!(state.db.get_media(1).unwrap().is_some());
            let saving = state.clone();
            let mut job = tokio::spawn(async move {
                save_entry_inner(&saving, 1, Some("CURRENT".into()), Some(1), None, None).await
            });
            let lookup_request = identity_request_while_running(&mut requests, &mut job).await;
            assert!(lookup_request["query"]
                .as_str()
                .unwrap()
                .contains("mediaListEntry"));
            let save_request = identity_request_while_running(&mut requests, &mut job).await;
            assert!(save_request["query"]
                .as_str()
                .unwrap()
                .contains("SaveMediaListEntry"));
            state.cancel_login();
            gate.add_permits(1);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), job)
                    .await
                    .expect("superseded save timed out")
                    .unwrap()
                    .unwrap_err(),
                AUTH_REPLACED
            );
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_media(1).unwrap().is_some());
            assert_eq!(
                state.db.get_setting("media_detail:1").unwrap().as_deref(),
                Some("{\"media\":{\"id\":1}}")
            );
        }
    }

    async fn wait_for_intent_receivers(
        state: &AppState,
        ready: impl Fn(usize) -> bool,
        context: &str,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready(state.auth_intent.receiver_count()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{context} timed out"));
    }

    #[tokio::test]
    async fn auth_intent_change_cancels_rate_limited_stats_and_media_detail_promptly() {
        for (rate_limited, signed_out) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            if !signed_out && rate_limited {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let (api, mut requests) = anilist::mock_api_gated(
                    vec![(429, json!({})), (200, statistics_reply(json!(7), 99))],
                    Some(gate.clone()),
                )
                .await;
                let state = state(api);
                let reading = state.clone();
                let mut job = tokio::spawn(async move { get_user_stats_inner(&reading).await });
                identity_request_while_running(&mut requests, &mut job).await;
                let waiting = state.auth_intent.receiver_count();
                assert!(waiting >= 2);
                gate.add_permits(1);
                wait_for_intent_receivers(
                    &state,
                    |count| count > waiting,
                    "rate limited get_user_stats backoff",
                )
                .await;
                state.begin_auth_intent();
                let result = tokio::time::timeout(Duration::from_millis(750), &mut job)
                    .await
                    .expect("rate limited get_user_stats did not cancel promptly")
                    .unwrap();
                assert_eq!(result.unwrap_err(), AUTH_REPLACED);
                assert!(requests.try_recv().is_err());
                assert!(state.db.get_setting("user_stats:7").unwrap().is_none());
            }

            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let replies = if rate_limited {
                vec![
                    (429, json!({})),
                    (200, json!({"data":{"Media":anilist::mock_media(1)}})),
                ]
            } else {
                vec![(200, json!({"data":{"Media":anilist::mock_media(1)}}))]
            };
            let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
            let state = state(api);
            let cached = serde_json::to_string(&MediaDetail {
                media: Media {
                    id: 1,
                    title_english: Some("Cached show".into()),
                    episodes: Some(12),
                    ..Default::default()
                },
                relations: vec![],
                characters: vec![],
                staff: vec![],
                cached_at: Some(1),
                warning: None,
                unavailable_sections: vec![],
            })
            .unwrap();
            state.db.set_setting("media_detail:1", &cached).unwrap();
            if signed_out {
                state.anilist.lock().set_token(None);
            }
            let reading = state.clone();
            let mut job = tokio::spawn(async move { get_media_detail_inner(1, &reading).await });
            identity_request_while_running(&mut requests, &mut job).await;
            if rate_limited {
                let waiting = state.auth_intent.receiver_count();
                assert!(waiting >= 2);
                gate.add_permits(1);
                wait_for_intent_receivers(
                    &state,
                    |count| count > waiting,
                    "rate limited get_media_detail backoff",
                )
                .await;
            }
            state.begin_auth_intent();
            let result = tokio::time::timeout(Duration::from_millis(750), &mut job)
                .await
                .expect("get_media_detail did not cancel promptly")
                .unwrap();
            assert_eq!(
                result.unwrap_err(),
                AUTH_REPLACED,
                "rate_limited={rate_limited}, signed_out={signed_out}"
            );
            assert!(requests.try_recv().is_err());
            assert_eq!(
                state.db.get_setting("media_detail:1").unwrap().as_deref(),
                Some(cached.as_str())
            );
        }
    }

    #[tokio::test]
    async fn untracked_save_missing_media_failure_clears_projected_entry_and_marks_missing() {
        let lookup = (
            200,
            json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":null}}}),
        );
        let missing = (404, json!({"errors":[{"message":"Not Found"}]}));
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let (api, mut requests) =
            anilist::mock_api_gated(vec![lookup, missing], Some(gate.clone())).await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_romaji: Some("Show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state.db.delete_entry(1).unwrap();
        state.refresh_matchers();
        assert!(state.matchers.lock().is_empty());

        let mut events = state.events.subscribe();
        let saving = state.clone();
        let mut job = tokio::spawn(async move {
            save_entry_inner(&saving, 1, Some("CURRENT".into()), Some(1), None, None).await
        });
        let _ = identity_request_while_running(&mut requests, &mut job).await;
        let _ = identity_request_while_running(&mut requests, &mut job).await;

        assert_eq!(get_entry(1, &state).await.unwrap().unwrap().progress, 1);
        assert_eq!(state.matchers.lock().len(), 1);
        state.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 2,
            title: "Show".into(),
            raw_title: "Show - 02".into(),
            progress: 1,
        }));
        assert!(state.retained_tracking_prompt().is_some());

        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_media_delete BEFORE DELETE ON media
                 BEGIN SELECT RAISE(ABORT, 'media cleanup failed'); END;",
            )
            .unwrap();
        gate.add_permits(1);

        let error = tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .expect("failed missing media save timed out")
            .unwrap()
            .unwrap_err();
        assert!(error.contains("media cleanup failed"), "{error}");
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.missing_media && pending.conflict);
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.matchers.lock().is_empty());
        assert!(state.retained_tracking_prompt().is_none());
        assert!(state.db.get_media(1).unwrap().is_some());
        let mut pending_events = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let crate::EventKind::PendingChanged(ids) = event.kind {
                pending_events.push(ids);
            }
        }
        assert_eq!(pending_events, vec![Some(vec![1])]);
    }

    #[tokio::test]
    async fn restore_rejected_entry_preserves_replacement_account_list_entry() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();
        let origin = state.origin();
        let (_, pending, _) = crate::sync_queue::stage(
            &state,
            1,
            crate::sync_queue::Patch {
                status: Some("CURRENT".into()),
                progress: Some(1),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        state.db.set_setting(USER_ID_KEY, "8").unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(88),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 9,
                ..Default::default()
            })
            .unwrap();

        restore_rejected_entry(&state, 7, &pending, "Not Found", origin).unwrap();

        assert!(state.db.pending(7).unwrap().is_empty());
        let replacement = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(replacement.id, Some(88));
        assert_eq!(replacement.progress, 9);
    }

    #[tokio::test]
    async fn superseded_confirmed_save_settles_entry_total_and_pending() {
        for mode in [
            "cancel_plain",
            "cancel_total",
            "logout_total",
            "switch_account",
        ] {
            let confirm_total = matches!(mode, "cancel_total" | "logout_total");
            let mut saved = remote();
            if confirm_total {
                saved["status"] = json!("COMPLETED");
                saved["progress"] = json!(13);
            } else {
                saved["status"] = json!("PAUSED");
                saved["progress"] = json!(11);
            }
            let mut responses = vec![(200, json!({"data":{"SaveMediaListEntry":saved.clone()}}))];
            if confirm_total {
                responses.push((
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":saved}}}),
                ));
            }
            let gate = Arc::new(tokio::sync::Semaphore::new(usize::from(confirm_total)));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    title_romaji: Some("Fixture Show".into()),
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "CURRENT".into();
            base.progress = 5;
            state.db.upsert_entry(&base).unwrap();
            state.refresh_matchers();
            let mut events = state.events.subscribe();

            let editing = state.clone();
            let mut task = tokio::spawn(async move {
                if confirm_total {
                    save_entry_inner(&editing, 1, Some("COMPLETED".into()), None, None, None).await
                } else {
                    save_entry_inner(&editing, 1, Some("PAUSED".into()), Some(11), None, None).await
                }
            });
            identity_request_while_running(&mut requests, &mut task).await;
            let mut signing_out = None;
            if confirm_total {
                identity_request_while_running(&mut requests, &mut task).await;
                let entry_guard = state.entry_lock.lock().await;
                let waiting = state.auth_intent.receiver_count();
                assert!(waiting >= 2);
                gate.add_permits(1);
                wait_for_intent_receivers(&state, |count| count < waiting, "confirm_total lookup")
                    .await;
                if mode == "logout_total" {
                    let mut job = Box::pin(logout(&state));
                    tokio::select! {
                        biased;
                        result = &mut job => panic!("logout bypassed remote_lock: {result:?}"),
                        _ = std::future::ready(()) => {}
                    }
                    signing_out = Some(job);
                } else {
                    state.cancel_login();
                }
                drop(entry_guard);
            } else if mode == "switch_account" {
                state.db.set_setting(USER_ID_KEY, "8").unwrap();
                state
                    .db
                    .upsert_entry(&ListEntry {
                        id: Some(88),
                        media_id: 1,
                        status: "CURRENT".into(),
                        progress: 9,
                        ..Default::default()
                    })
                    .unwrap();
                gate.add_permits(1);
            } else {
                state.cancel_login();
                gate.add_permits(1);
            }
            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED, "{mode}");
            assert!(events.try_recv().is_err(), "{mode}");
            assert!(state.db.pending(7).unwrap().is_empty(), "{mode}");

            if let Some(job) = signing_out {
                job.await.unwrap();
                assert!(!state.anilist.lock().has_token());
                assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
                assert!(state.db.pending(7).unwrap().is_empty());
            } else if mode == "switch_account" {
                let replacement = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!((replacement.id, replacement.progress), (Some(88), 9));
            } else {
                let cached = state.db.get_entry(1).unwrap().unwrap();
                if confirm_total {
                    assert_eq!((cached.status.as_str(), cached.progress), ("COMPLETED", 13));
                    assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
                    assert_eq!(cached.media.unwrap().episodes, Some(13));
                } else {
                    assert_eq!((cached.status.as_str(), cached.progress), ("PAUSED", 11));
                }
                crate::sync_queue::flush_background(&state).await.unwrap();
                assert!(requests.try_recv().is_err(), "{mode}");
            }
        }
    }

    #[tokio::test]
    async fn superseded_confirmed_delete_settles_entry_and_does_not_resurrect_queue() {
        for mode in [
            "cancel_deleted",
            "logout_deleted",
            "absent_none",
            "absent_404",
            "live_retry",
            "switch_account",
        ] {
            let absent_lookup = matches!(mode, "absent_none" | "absent_404");
            let responses = match mode {
                "absent_none" => vec![
                    (404, json!({"errors":[{"message":"Not Found"}]})),
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
                    ),
                ],
                "absent_404" => vec![
                    (404, json!({"errors":[{"message":"Not Found"}]})),
                    (404, json!({"errors":[{"message":"Not Found"}]})),
                ],
                "live_retry" => {
                    let mut live = remote();
                    live["id"] = json!(23);
                    vec![
                        (404, json!({"errors":[{"message":"Not Found"}]})),
                        (
                            200,
                            json!({"data":{"Media":{"id":1,"mediaListEntry":live}}}),
                        ),
                        (
                            200,
                            json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
                        ),
                    ]
                }
                _ => vec![(
                    200,
                    json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
                )],
            };
            let initial_permits = match mode {
                "live_retry" => 2,
                _ if absent_lookup => 1,
                _ => 0,
            };
            let gate = Arc::new(tokio::sync::Semaphore::new(initial_permits));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    title_romaji: Some("Fixture Show".into()),
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();
            state.refresh_matchers();
            assert!(!state.matchers.lock().is_empty());
            if absent_lookup || mode == "live_retry" {
                crate::sync_queue::stage(
                    &state,
                    1,
                    crate::sync_queue::Patch {
                        progress: Some(11),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            let editing = state.clone();
            let mut task = tokio::spawn(async move { delete_entry_inner(1, &editing).await });
            identity_request_while_running(&mut requests, &mut task).await;
            let mut events = state.events.subscribe();
            let mut signing_out = None;
            if absent_lookup {
                identity_request_while_running(&mut requests, &mut task).await;
                let entry_guard = state.entry_lock.lock().await;
                let waiting = state.auth_intent.receiver_count();
                assert!(waiting >= 2);
                gate.add_permits(1);
                wait_for_intent_receivers(&state, |count| count < waiting, "absent delete lookup")
                    .await;
                state.cancel_login();
                drop(entry_guard);
            } else if mode == "live_retry" {
                identity_request_while_running(&mut requests, &mut task).await;
                let retry_delete = identity_request_while_running(&mut requests, &mut task).await;
                assert_eq!(retry_delete["variables"]["id"], 23);
                state.cancel_login();
                gate.add_permits(1);
            } else if mode == "switch_account" {
                state.db.set_setting(USER_ID_KEY, "8").unwrap();
                state
                    .db
                    .upsert_entry(&ListEntry {
                        id: Some(88),
                        media_id: 1,
                        status: "CURRENT".into(),
                        progress: 9,
                        ..Default::default()
                    })
                    .unwrap();
                gate.add_permits(1);
            } else if mode == "logout_deleted" {
                let mut job = Box::pin(logout(&state));
                tokio::select! {
                    biased;
                    result = &mut job => panic!("logout bypassed remote_lock: {result:?}"),
                    _ = std::future::ready(()) => {}
                }
                signing_out = Some(job);
                gate.add_permits(1);
            } else {
                state.cancel_login();
                gate.add_permits(1);
            }
            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED, "{mode}");
            assert!(events.try_recv().is_err(), "{mode}");
            assert!(state.db.pending(7).unwrap().is_empty(), "{mode}");
            if let Some(job) = signing_out {
                job.await.unwrap();
                assert!(!state.anilist.lock().has_token());
                assert!(state.db.pending(7).unwrap().is_empty());
            } else if mode == "switch_account" {
                let replacement = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!((replacement.id, replacement.progress), (Some(88), 9));
            } else {
                assert!(state.db.get_entry(1).unwrap().is_none(), "{mode}");
                assert!(state.matchers.lock().is_empty(), "{mode}");
                crate::sync_queue::flush_background(&state).await.unwrap();
                assert!(requests.try_recv().is_err(), "{mode}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incompatible_score_write_stays_conflicted_on_postflight_failure_and_supersession() {
        let expected_refusal = "AniList did not confirm the requested score or rewatch count. Review this saved change.";
        for mode in [
            "viewer_replaced",
            "total_replaced",
            "viewer_401",
            "viewer_503",
            "viewer_400",
            "viewer_other_account",
        ] {
            let mut completed = remote();
            completed["status"] = json!("COMPLETED");
            completed["progress"] = json!(13);
            completed["score"] = json!(70);
            let mut responses = vec![(200, score_profile("POINT_100"))];
            match mode {
                "total_replaced" => {
                    responses.push((
                        200,
                        json!({"data":{"SaveMediaListEntry":completed.clone()}}),
                    ));
                    responses.push((
                        200,
                        json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":completed}}}),
                    ));
                }
                "viewer_401" => {
                    responses.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
                    responses.push((
                        401,
                        json!({"errors":[{"message":"Invalid token","status":401}]}),
                    ));
                    responses.push((200, score_profile("POINT_100")));
                }
                "viewer_503" => {
                    responses.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
                    responses.push((503, json!({"errors":[{"message":"Unavailable"}]})));
                }
                "viewer_400" => {
                    responses.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
                    responses.push((
                        400,
                        json!({"errors":[{"message":"Invalid Viewer request","status":400}]}),
                    ));
                }
                "viewer_other_account" => {
                    let mut other = score_profile("POINT_100");
                    other["data"]["Viewer"]["id"] = json!(8);
                    responses.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
                    responses.push((200, other));
                }
                _ => {
                    responses.push((200, json!({"data":{"SaveMediaListEntry":remote()}})));
                    responses.push((200, score_profile("POINT_5")));
                }
            }
            let gated = matches!(mode, "viewer_replaced" | "total_replaced");
            let gate = Arc::new(tokio::sync::Semaphore::new(if gated { 2 } else { 10 }));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "CURRENT".into();
            base.progress = 5;
            state.db.upsert_entry(&base).unwrap();
            let mut events = state.events.subscribe();

            let editing = state.clone();
            let mut task = tokio::spawn(async move {
                if mode == "total_replaced" {
                    save_entry_inner(
                        &editing,
                        1,
                        Some("COMPLETED".into()),
                        None,
                        Some(80.0),
                        None,
                    )
                    .await
                } else {
                    save_entry_inner(&editing, 1, None, None, Some(80.0), None).await
                }
            });
            if gated {
                for _ in 0..3 {
                    identity_request_while_running(&mut requests, &mut task).await;
                }
                if mode == "total_replaced" {
                    let entry_guard = state.entry_lock.lock().await;
                    let waiting = state.auth_intent.receiver_count();
                    assert!(waiting >= 2);
                    gate.add_permits(1);
                    wait_for_intent_receivers(
                        &state,
                        |count| count < waiting,
                        "total_replaced lookup",
                    )
                    .await;
                    state.cancel_login();
                    drop(entry_guard);
                } else {
                    state.cancel_login();
                    gate.add_permits(1);
                }
                assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED, "{mode}");
                assert!(events.try_recv().is_err(), "{mode}");
            } else if mode == "viewer_401" {
                let auth_expired = async {
                    loop {
                        if let crate::EventKind::AuthExpired = events.recv().await.unwrap().kind {
                            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
                            assert!(pending.conflict);
                            assert_eq!(pending.error.as_deref(), Some(expected_refusal));
                            assert_eq!(pending.retry_error.as_deref(), Some(SESSION_EXPIRED));
                            break;
                        }
                    }
                };
                let (result, ()) = tokio::join!(task, auth_expired);
                assert_eq!(result.unwrap().unwrap_err(), SESSION_EXPIRED);
                assert!(!state.anilist.lock().has_token());
                login_with_token("renewed-session".into(), &state, ListPreferences::Preserve)
                    .await
                    .unwrap();
            } else if mode == "viewer_503" {
                task.await.unwrap().unwrap();
            } else if mode == "viewer_other_account" {
                assert_eq!(task.await.unwrap().unwrap_err(), "Account changed");
                let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                    matches!(
                        event.kind,
                        crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                    )
                });
                assert!(notified, "{mode}");
            } else {
                assert!(task.await.unwrap().is_err(), "{mode}");
            }

            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict, "{mode}");
            assert_eq!(pending.error.as_deref(), Some(expected_refusal), "{mode}");
            assert_eq!(
                pending.retry_error.is_some(),
                matches!(mode, "viewer_401" | "viewer_503" | "viewer_400"),
                "{mode}"
            );
            assert_eq!(pending.patch.score, Some(80.0), "{mode}");
            while requests.try_recv().is_ok() {}
            crate::sync_queue::flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err(), "{mode}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn superseded_confirmed_entry_details_save_updates_offline_cache() {
        let initial = EntryDetails {
            notes: "Old note".into(),
            started_at: Some(FuzzyDate {
                year: Some(2024),
                month: Some(1),
                day: Some(2),
            }),
            completed_at: None,
            custom_lists: vec!["OldList".into()],
            available_custom_lists: vec!["Favorites".into(), "OldList".into()],
            cached_at: Some(100),
            warning: None,
        };
        let initial_json = serde_json::to_string(&initial).unwrap();
        let confirmed_started = FuzzyDate {
            year: Some(2024),
            month: Some(3),
            day: Some(4),
        };
        let confirmed_completed = FuzzyDate {
            year: Some(2024),
            month: Some(4),
            day: Some(5),
        };
        let lookup_reply = (
            200,
            json!({
                "data": {
                    "Media": {
                        "mediaListEntry": {
                            "id": 22,
                            "mediaId": 1,
                            "notes": "Old note",
                            "startedAt": {"year": 2024, "month": 1, "day": 2},
                            "completedAt": null,
                            "customLists": {"Favorites": false, "Rewatch": false}
                        }
                    },
                    "Viewer": {
                        "mediaListOptions": {
                            "animeList": {"customLists": ["Favorites", "Rewatch"]}
                        }
                    }
                }
            }),
        );
        let save_reply = (
            200,
            json!({
                "data": {
                    "SaveMediaListEntry": {
                        "id": 22,
                        "mediaId": 1,
                        "notes": "Confirmed note",
                        "startedAt": {"year": 2024, "month": 3, "day": 4},
                        "completedAt": {"year": 2024, "month": 4, "day": 5},
                        "customLists": {"Favorites": true, "Rewatch": false}
                    }
                }
            }),
        );

        for mode in [
            "cancel_save",
            "logout_save",
            "cancel_lookup",
            "save_503_cancel",
            "save_404_cancel",
            "save_401_logout",
        ] {
            let responses = match mode {
                "cancel_lookup" => vec![lookup_reply.clone()],
                "save_503_cancel" => vec![
                    lookup_reply.clone(),
                    (503, json!({"errors": [{"message": "Unavailable"}]})),
                ],
                "save_404_cancel" => vec![
                    lookup_reply.clone(),
                    (
                        404,
                        json!({"errors": [{"message": "Not Found", "status": 404}]}),
                    ),
                ],
                "save_401_logout" => vec![
                    lookup_reply.clone(),
                    (
                        401,
                        json!({"errors": [{"message": "Invalid token", "status": 401}]}),
                    ),
                ],
                _ => vec![lookup_reply.clone(), save_reply.clone()],
            };
            let initial_permits = usize::from(mode != "cancel_lookup");
            let gate = Arc::new(tokio::sync::Semaphore::new(initial_permits));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            state
                .db
                .set_setting("entry_details:7:1", &initial_json)
                .unwrap();
            let mut events = state.events.subscribe();

            let editing = state.clone();
            let started = confirmed_started.clone();
            let completed = confirmed_completed.clone();
            let mut task = tokio::spawn(async move {
                update_entry_details_inner(
                    1,
                    Some("Confirmed note".into()),
                    Some(started),
                    Some(completed),
                    Some(vec!["Favorites".into()]),
                    &editing,
                )
                .await
            });

            let lookup = identity_request_while_running(&mut requests, &mut task).await;
            assert!(lookup["query"].as_str().unwrap().starts_with("query"));
            let mut signing_out = None;
            if mode == "cancel_lookup" {
                state.cancel_login();
                gate.add_permits(1);
            } else {
                let mutation = identity_request_while_running(&mut requests, &mut task).await;
                assert!(
                    mutation["query"].as_str().unwrap().starts_with("mutation"),
                    "{mode}"
                );
                if matches!(mode, "logout_save" | "save_401_logout") {
                    let mut job = Box::pin(logout(&state));
                    tokio::select! {
                        biased;
                        result = &mut job => panic!("logout bypassed remote_lock: {result:?}"),
                        _ = std::future::ready(()) => {}
                    }
                    signing_out = Some(job);
                } else {
                    state.cancel_login();
                }
                gate.add_permits(1);
            }

            assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED, "{mode}");
            assert!(events.try_recv().is_err(), "{mode}");
            assert!(requests.try_recv().is_err(), "{mode}");

            if let Some(job) = signing_out {
                job.await.unwrap();
                assert!(!state.anilist.lock().has_token(), "{mode}");
            } else {
                assert!(state.anilist.lock().has_token(), "{mode}");
            }

            let raw = state.db.get_setting("entry_details:7:1").unwrap().unwrap();
            if matches!(mode, "cancel_save" | "logout_save") {
                let cached: EntryDetails = serde_json::from_str(&raw).unwrap();
                assert_eq!(cached.notes, "Confirmed note", "{mode}");
                assert_eq!(cached.started_at, Some(confirmed_started.clone()), "{mode}");
                assert_eq!(
                    cached.completed_at,
                    Some(confirmed_completed.clone()),
                    "{mode}"
                );
                assert_eq!(cached.custom_lists, vec!["Favorites".to_string()], "{mode}");
                assert_eq!(
                    cached.available_custom_lists,
                    vec!["Favorites".to_string(), "Rewatch".to_string()],
                    "{mode}"
                );
                assert!(cached.cached_at.is_some_and(|ts| ts > 100), "{mode}");
            } else {
                assert_eq!(raw, initial_json, "{mode}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scored_save_postflight_account_mismatch_notifies_staged_pending() {
        for mode in ["viewer_other_account", "viewer_replaced"] {
            let mut saved = remote();
            saved["status"] = json!("CURRENT");
            saved["progress"] = json!(5);
            saved["score"] = json!(80);
            let postflight = if mode == "viewer_other_account" {
                let mut other = score_profile("POINT_100");
                other["data"]["Viewer"]["id"] = json!(8);
                other
            } else {
                score_profile("POINT_100")
            };
            let responses = vec![
                (200, score_profile("POINT_100")),
                (200, json!({"data": {"SaveMediaListEntry": saved}})),
                (200, postflight),
            ];
            let gated = mode == "viewer_replaced";
            let gate = Arc::new(tokio::sync::Semaphore::new(if gated { 2 } else { 10 }));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "CURRENT".into();
            base.progress = 5;
            state.db.upsert_entry(&base).unwrap();
            let mut events = state.events.subscribe();

            let editing = state.clone();
            let mut task = tokio::spawn(async move {
                save_entry_inner(&editing, 1, None, None, Some(80.0), None).await
            });
            if gated {
                for _ in 0..3 {
                    identity_request_while_running(&mut requests, &mut task).await;
                }
                state.cancel_login();
                gate.add_permits(1);
                assert_eq!(task.await.unwrap().unwrap_err(), AUTH_REPLACED, "{mode}");
                assert!(events.try_recv().is_err(), "{mode}");
            } else {
                assert_eq!(
                    task.await.unwrap().unwrap_err(),
                    "Account changed",
                    "{mode}"
                );
                let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                    matches!(
                        event.kind,
                        crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                    )
                });
                assert!(notified, "{mode}");
            }

            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(!pending.conflict, "{mode}");
            assert_eq!(pending.patch.score, Some(80.0), "{mode}");
            assert_eq!(
                state.db.get_entry(1).unwrap().unwrap().score,
                Some(70.0),
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn refused_foreground_write_stays_conflicted_when_cache_write_fails() {
        for fail_media in [true, false] {
            let mut clamped = remote();
            clamped["status"] = json!("COMPLETED");
            clamped["progress"] = json!(10);
            let (api, mut requests) = anilist::mock_api(vec![
                (200, json!({"data":{"SaveMediaListEntry":clamped}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":10,"mediaListEntry":clamped}}}),
                ),
            ])
            .await;
            let state = state(api);
            let mut entry = state.db.get_entry(1).unwrap().unwrap();
            entry.status = "CURRENT".into();
            entry.progress = 11;
            state.db.upsert_entry(&entry).unwrap();
            let trigger = if fail_media {
                "CREATE TRIGGER fail_cache BEFORE UPDATE ON media
                 BEGIN SELECT RAISE(FAIL, 'media cache write failed'); END;"
            } else {
                "CREATE TRIGGER fail_cache BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(FAIL, 'entry cache write failed'); END;"
            };
            state.db.0.lock().execute_batch(trigger).unwrap();

            let mut events = state.events.subscribe();
            let error = save_entry_inner(&state, 1, None, Some(12), None, None)
                .await
                .unwrap_err();
            assert!(
                error.contains(if fail_media {
                    "media cache write failed"
                } else {
                    "entry cache write failed"
                }),
                "{error}"
            );
            assert!(requests.try_recv().is_ok());
            assert!(requests.try_recv().is_ok());
            assert!(requests.try_recv().is_err());
            let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                matches!(
                    event.kind,
                    crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                )
            });
            assert!(notified, "fail_media={fail_media}");
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict, "fail_media={fail_media}");
            assert!(
                pending.rejected_progress.is_some(),
                "fail_media={fail_media}"
            );

            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_cache")
                .unwrap();
            crate::sync_queue::flush_background(&state).await.unwrap();
            assert!(
                requests.try_recv().is_err(),
                "background sync must not replay a refused write (fail_media={fail_media})"
            );
        }
    }

    #[tokio::test]
    async fn offline_guarded_progress_rejects_out_of_range_progress() {
        let (api, mut requests) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Steins;Gate".into()),
                episodes: None,
                ..Default::default()
            })
            .unwrap();
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "CURRENT".into();
        entry.progress = i64::from(i32::MAX);
        state.db.upsert_entry(&entry).unwrap();

        let error = set_progress_inner_with_origin(
            &state,
            1,
            i64::from(i32::MAX) + 1,
            Some(i64::from(i32::MAX)),
            Some((Some(7), crate::EntryIdentity::Remote(22))),
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("Progress must be between 0 and 2147483647"),
            "{error}"
        );
        assert!(requests.try_recv().is_ok());
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending_change(7, 1).unwrap().is_none());
        assert_eq!(
            state.db.get_entry(1).unwrap().unwrap().progress,
            i64::from(i32::MAX)
        );
    }

    #[tokio::test]
    async fn fetch_torrents_waits_for_entry_lock_and_ignores_signed_out_matchers() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api);
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let feed = format!("http://{address}/feed");
        state
            .db
            .set_setting("rss_feeds", &json!([feed]).to_string())
            .unwrap();
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Steins;Gate".into()),
                episodes: Some(24),
                ..Default::default()
            })
            .unwrap();
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.status = "CURRENT".into();
        entry.progress = 2;
        state.db.upsert_entry(&entry).unwrap();
        state.refresh_matchers();

        let body = "<rss><channel><item><title>[Group] Steins;Gate - 03 [1080p].mkv</title>\
                    <link>https://example.invalid/3</link><guid>ep-3</guid></item></channel></rss>";
        let first_served = Arc::new(tokio::sync::Notify::new());
        let served_notify = first_served.clone();
        let responder = tokio::spawn(async move {
            for index in 0..2 {
                let (socket, _) = server.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                while socket.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                if index == 0 {
                    served_notify.notify_one();
                }
            }
        });

        let guard = state.entry_lock.lock().await;
        let worker = state.clone();
        let mut fetch = tokio::spawn(async move { fetch_torrents(&worker).await });
        first_served.notified().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !fetch.is_finished(),
            "fetch_torrents must wait for entry_lock before reading matchers and list progress"
        );
        entry.progress = 3;
        state.db.upsert_entry(&entry).unwrap();
        drop(guard);

        let result = (&mut fetch).await.unwrap().unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].media_id, Some(1));
        assert!(
            !result.items[0].is_new,
            "episode 3 should not be marked new after progress reached 3"
        );

        state.anilist.lock().set_token(None);
        let signed_out = fetch_torrents(&state).await.unwrap();
        assert_eq!(signed_out.items.len(), 1);
        assert!(signed_out.items[0].media_id.is_none());
        assert!(!signed_out.items[0].is_new);
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn post_save_score_format_mismatch_projection_failure_falls_back_to_put_pending() {
        let mut saved = remote();
        saved["score"] = json!(80);
        let (api, _) = anilist::mock_api(vec![
            (200, score_profile("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":saved}})),
            (200, score_profile("POINT_10_DECIMAL")),
        ])
        .await;
        let state = state(api);
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_entry_projection BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(ABORT, 'list_entry projection blocked'); END;",
            )
            .unwrap();
        let mut events = state.events.subscribe();

        let err = save_entry_inner(&state, 1, None, None, Some(80.0), None)
            .await
            .unwrap_err();
        assert!(err.contains("list_entry projection blocked"), "{err}");

        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.conflict);
        assert!(pending.error.is_some());
        let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            matches!(
                event.kind,
                crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
            )
        });
        assert!(
            notified,
            "PendingChanged must be emitted after fallback put_pending"
        );
    }

    #[tokio::test]
    async fn refused_save_refreshes_matchers_when_projection_fails_after_episode_total_change() {
        for fail_fallback in [false, true] {
            let mut saved = remote();
            saved["status"] = json!("COMPLETED");
            saved["progress"] = json!(11);
            let confirm = json!({"data":{"Media":{
                "id":1,"episodes":13,
                "mediaListEntry":{"id":22,"mediaId":1,"status":"COMPLETED","progress":11,"score":70,"repeat":2,"updatedAt":101}
            }}});
            let (api, _) = anilist::mock_api(vec![
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (200, confirm),
            ])
            .await;
            let state = state(api);
            state
                .db
                .upsert_media(&Media {
                    id: 1,
                    title_english: Some("Steins;Gate".into()),
                    episodes: Some(12),
                    ..Default::default()
                })
                .unwrap();
            let mut entry = state.db.get_entry(1).unwrap().unwrap();
            entry.status = "CURRENT".into();
            entry.progress = 3;
            state.db.upsert_entry(&entry).unwrap();
            state.refresh_matchers();
            assert_eq!(state.matchers.lock()[0].episodes, Some(12));

            let sql = if fail_fallback {
                "CREATE TEMP TRIGGER fail_entry_projection BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(ABORT, 'list_entry blocked'); END;
                 CREATE TEMP TRIGGER fail_pending_fallback BEFORE INSERT ON pending_change
                 WHEN NEW.payload LIKE '%\"conflict\":true%'
                 BEGIN SELECT RAISE(ABORT, 'pending fallback blocked'); END;"
            } else {
                "CREATE TEMP TRIGGER fail_entry_projection BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(ABORT, 'list_entry blocked'); END;"
            };
            state.db.0.lock().execute_batch(sql).unwrap();

            let err = save_entry_inner(&state, 1, Some("COMPLETED".into()), Some(15), None, None)
                .await
                .unwrap_err();
            if fail_fallback {
                assert!(
                    err.contains("pending fallback blocked")
                        && err.contains("failed to mark pending change conflicted"),
                    "{err}"
                );
            } else {
                assert!(err.contains("list_entry blocked"), "{err}");
                assert!(state.db.pending_change(7, 1).unwrap().unwrap().conflict);
            }
            assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
            assert_eq!(
                state.matchers.lock()[0].episodes,
                Some(13),
                "fail_fallback={fail_fallback}"
            );
        }
    }

    #[tokio::test]
    async fn authenticated_catalog_reads_expire_session_on_401_and_protect_replacement_session() {
        let auth_err = || (401, json!({"errors":[{"message":"Invalid token"}]}));
        for kind in ["search", "season", "recs", "media", "schedule"] {
            let (api, _) = anilist::mock_api(vec![auth_err()]).await;
            let state = state(api);
            let (_, pending, _) = crate::sync_queue::stage(
                &state,
                1,
                crate::sync_queue::Patch {
                    progress: Some(11),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            state.db.put_pending(7, &pending).unwrap();
            let mut events = state.events.subscribe();

            let err = match kind {
                "search" => search_anime_page("test".into(), 1, &state)
                    .await
                    .unwrap_err(),
                "season" => get_season("SPRING".into(), 2026, &state).await.unwrap_err(),
                "recs" => get_recommendations(1, &state).await.unwrap_err(),
                "media" => get_media(999, &state).await.unwrap_err(),
                "schedule" => get_airing_schedule(100, 200, &state).await.unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(err, SESSION_EXPIRED, "{kind}");
            assert!(state.anilist.lock().token().is_none(), "{kind}");
            assert_eq!(
                state.db.pending(7).unwrap().len(),
                1,
                "pending queue must stay intact on 401 for {kind}"
            );
            let expired = std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event.kind, crate::EventKind::AuthExpired));
            assert!(expired, "AuthExpired missing for {kind}");

            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![auth_err()], Some(gate.clone())).await;
            let state = state_with_api(api);
            let worker = state.clone();
            let task = tokio::spawn(async move {
                match kind {
                    "search" => search_anime_page("test".into(), 1, &worker)
                        .await
                        .map(|_| ()),
                    "season" => get_season("SPRING".into(), 2026, &worker).await.map(|_| ()),
                    "recs" => get_recommendations(1, &worker).await.map(|_| ()),
                    "media" => get_media(999, &worker).await.map(|_| ()),
                    "schedule" => get_airing_schedule(100, 200, &worker).await.map(|_| ()),
                    _ => unreachable!(),
                }
            });
            tokio::time::timeout(Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            state
                .anilist
                .lock()
                .set_token(Some("replacement-token".into()));
            gate.add_permits(1);
            let err = task.await.unwrap().unwrap_err();
            assert_eq!(err, "Your AniList account changed. Try again.", "{kind}");
            assert_eq!(
                state.anilist.lock().token().as_deref(),
                Some("replacement-token"),
                "late 401 expired replacement session for {kind}"
            );
        }

        fn state_with_api(api: AniList) -> Arc<AppState> {
            state(api)
        }
    }

    #[tokio::test]
    async fn auth_intent_cancels_catalog_reads_and_season_page_sleep() {
        for kind in ["search", "recs", "media"] {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![(200, json!({"data":{}}))], Some(gate.clone())).await;
            let state = state(api);
            let worker = state.clone();
            let mut task = tokio::spawn(async move {
                match kind {
                    "search" => search_anime_page("test".into(), 1, &worker)
                        .await
                        .map(|_| ()),
                    "recs" => get_recommendations(1, &worker).await.map(|_| ()),
                    "media" => get_media(999, &worker).await.map(|_| ()),
                    _ => unreachable!(),
                }
            });
            tokio::time::timeout(Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            state.begin_auth_intent();
            let result = tokio::time::timeout(Duration::from_millis(750), &mut task).await;
            gate.add_permits(1);
            assert_eq!(
                result
                    .expect("catalog read waited for the old session")
                    .unwrap()
                    .unwrap_err(),
                AUTH_REPLACED,
                "{kind}"
            );
        }

        let page1 = (
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":true},
                "media":[anilist::mock_media(10)]
            }}}),
        );
        let page2 = (
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":false},
                "media":[anilist::mock_media(11)]
            }}}),
        );
        let (api, mut requests) = anilist::mock_api(vec![page1, page2]).await;
        let state = state(api);
        let worker = state.clone();
        let mut task =
            tokio::spawn(async move { get_season("SPRING".into(), 2026, &worker).await });
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        // Let page 1 finish and enter the 300ms pause before page 2.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        state.begin_auth_intent();
        let result = tokio::time::timeout(Duration::from_millis(200), &mut task)
            .await
            .expect("get_season did not cancel inter-page sleep promptly")
            .unwrap();
        assert_eq!(result.unwrap_err(), AUTH_REPLACED);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(
            requests.try_recv().is_err(),
            "page 2 should not have been requested after cancellation"
        );
    }

    #[tokio::test]
    async fn cached_media_detail_normalizes_non_positive_episode_totals() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api);
        let detail = MediaDetail {
            media: Media {
                id: 77,
                title_english: Some("Zero Total Show".into()),
                episodes: Some(0),
                ..Default::default()
            },
            relations: vec![crate::models::MediaRelation {
                relation: "SEQUEL".into(),
                media: Media {
                    id: 78,
                    title_english: Some("Zero Total Sequel".into()),
                    episodes: Some(-4),
                    ..Default::default()
                },
            }],
            cached_at: Some(1_700_000_000),
            characters: vec![],
            staff: vec![],
            warning: None,
            unavailable_sections: vec![],
        };
        state
            .db
            .set_setting("media_detail:77", &serde_json::to_string(&detail).unwrap())
            .unwrap();

        let loaded = get_media_detail(77, &state).await.unwrap();
        assert!(loaded.warning.is_some());
        assert_eq!(loaded.media.episodes, None);
        assert_eq!(loaded.relations.len(), 1);
        assert_eq!(loaded.relations[0].media.episodes, None);
    }

    #[tokio::test]
    async fn update_discovery_status_hydrates_media_when_remote_entry_already_exists() {
        let lookup = json!({"data":{"Media":{
            "id":1,"episodes":12,
            "mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":4,"score":70,"repeat":0,"updatedAt":150}
        }}});
        let (api, _) = anilist::mock_api(vec![(200, lookup)]).await;
        let state = state(api);
        state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Steins;Gate".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        state.db.delete_entry(1).unwrap();
        let mut events = state.events.subscribe();

        let err = save_entry_inner_checked_impl(
            &state,
            1,
            Some("PLANNING".into()),
            None,
            None,
            None,
            None,
            None,
            Some(EntryWriteTarget::Discovery {
                account_id: Some(7),
                identity: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, DISCOVERY_ENTRY_CHANGED);

        let updated =
            std::iter::from_fn(|| events.try_recv().ok()).find_map(|event| match event.kind {
                crate::EventKind::EpisodeUpdated(entry) => Some(entry),
                _ => None,
            });
        let entry = updated.expect("EpisodeUpdated event should be emitted");
        assert_eq!(entry.media_id, 1);
        assert_eq!(
            entry
                .media
                .as_ref()
                .and_then(|m| m.title_english.as_deref()),
            Some("Steins;Gate")
        );
    }

    #[tokio::test]
    async fn dispatched_retryable_save_records_error_and_notifies_when_local_cache_fails() {
        for mode in ["post_viewer_503", "save_503", "unconfirmed_episode_cache"] {
            let replies = match mode {
                "post_viewer_503" => {
                    let mut saved = remote();
                    saved["score"] = json!(80);
                    vec![
                        (
                            200,
                            json!({"data":{"Viewer":{"id":7,"name":"User",
                                "mediaListOptions":{"scoreFormat":"POINT_100"}}}}),
                        ),
                        (200, json!({"data":{"SaveMediaListEntry":saved}})),
                        (503, json!({"errors":[{"message":"Viewer unavailable"}]})),
                    ]
                }
                "save_503" => vec![(503, json!({"errors":[{"message":"AniList unavailable"}]}))],
                "unconfirmed_episode_cache" => {
                    let mut saved = remote();
                    saved["status"] = json!("COMPLETED");
                    saved["progress"] = json!(13);
                    let mut readback = remote();
                    readback["status"] = json!("COMPLETED");
                    readback["progress"] = json!(12);
                    vec![
                        (200, json!({"data":{"SaveMediaListEntry":saved}})),
                        (
                            200,
                            json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":readback}}}),
                        ),
                    ]
                }
                _ => unreachable!(),
            };
            let (api, _) = anilist::mock_api(replies).await;
            let state = state(api);
            let sql = if mode == "unconfirmed_episode_cache" {
                "CREATE TEMP TRIGGER fail_cache BEFORE UPDATE ON media
                 WHEN NEW.episodes = 13
                 BEGIN SELECT RAISE(ABORT, 'media blocked'); END;"
            } else {
                "CREATE TEMP TRIGGER fail_cache BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(ABORT, 'list_entry blocked'); END;"
            };
            state.db.0.lock().execute_batch(sql).unwrap();
            let mut events = state.events.subscribe();

            let err = match mode {
                "post_viewer_503" => save_entry_inner(&state, 1, None, None, Some(80.0), None)
                    .await
                    .unwrap_err(),
                "save_503" => save_entry_inner(&state, 1, None, Some(4), None, None)
                    .await
                    .unwrap_err(),
                "unconfirmed_episode_cache" => {
                    save_entry_inner(&state, 1, Some("COMPLETED".into()), None, None, None)
                        .await
                        .unwrap_err()
                }
                _ => unreachable!(),
            };
            assert!(
                err.contains(if mode == "unconfirmed_episode_cache" {
                    "media blocked"
                } else {
                    "list_entry blocked"
                }),
                "{mode}: {err}"
            );
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(!pending.conflict, "{mode}");
            assert!(pending.error.is_some(), "{mode}: error not recorded");
            let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                matches!(
                    event.kind,
                    crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                )
            });
            assert!(notified, "{mode}: PendingChanged should be emitted");
        }
    }

    #[tokio::test]
    async fn absent_queued_add_removal_notifies_when_local_cleanup_fails() {
        let absent = (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
        );
        let (api, _) = anilist::mock_api(vec![absent]).await;
        let state = state(api);
        state.db.delete_entry(1).unwrap();
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
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_absent_removal BEFORE DELETE ON pending_change
                 BEGIN SELECT RAISE(FAIL, 'fixture absence cleanup failure'); END;",
            )
            .unwrap();
        let mut events = state.events.subscribe();

        let error = delete_entry_inner(1, &state).await.unwrap_err();
        assert!(error.contains("fixture absence cleanup failure"), "{error}");
        let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
            matches!(
                event.kind,
                crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
            )
        });
        assert!(
            notified,
            "PendingChanged should be emitted after staging absence"
        );
    }

    #[tokio::test]
    async fn delete_auth_rejection_preserves_prior_queue_and_avoids_phantom_conflict() {
        let auth_401 = (401, json!({"errors":[{"message":"Invalid token"}]}));
        for mode in [
            "clean_delete_401",
            "clean_lookup_401",
            "queued_edit_delete_401",
            "queued_add_lookup_401",
            "queued_delete_flush_401",
        ] {
            let replies = if mode == "queued_delete_flush_401" {
                vec![
                    auth_401.clone(),
                    (
                        200,
                        json!({"data":{"Viewer":{"id":7,"name":"User",
                            "mediaListOptions":{"scoreFormat":"POINT_100"}}}}),
                    ),
                    (
                        200,
                        json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
                    ),
                ]
            } else {
                vec![auth_401.clone()]
            };
            let (api, _) = anilist::mock_api(replies).await;
            let state = state(api);
            match mode {
                "clean_lookup_401" => {
                    state.db.delete_entry(1).unwrap();
                }
                "queued_edit_delete_401" => {
                    crate::sync_queue::stage(
                        &state,
                        1,
                        crate::sync_queue::Patch {
                            progress: Some(4),
                            score: Some(80.0),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                "queued_add_lookup_401" => {
                    state.db.delete_entry(1).unwrap();
                    crate::sync_queue::stage(
                        &state,
                        1,
                        crate::sync_queue::Patch {
                            progress: Some(4),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                "queued_delete_flush_401" => {
                    let pending = crate::sync_queue::Pending {
                        media_id: 1,
                        deleting: true,
                        deletion_id: Some(22),
                        base: state.db.get_entry(1).unwrap(),
                        patch: Default::default(),
                        error: None,
                        retry_error: None,
                        conflict: false,
                        missing_media: false,
                        score_format: None,
                        playback: false,
                        base_score_format: None,
                        rejected_progress: None,
                        rejected_total: None,
                    };
                    state.db.put_pending(7, &pending).unwrap();
                }
                _ => {}
            }
            let mut events = state.events.subscribe();

            let err = if mode == "queued_delete_flush_401" {
                crate::sync_queue::flush_background(&state)
                    .await
                    .unwrap_err()
            } else {
                delete_entry_inner(1, &state).await.unwrap_err()
            };
            assert_eq!(err, SESSION_EXPIRED, "{mode}");
            assert!(!state.anilist.lock().has_token(), "{mode}");
            assert!(
                std::iter::from_fn(|| events.try_recv().ok())
                    .any(|event| matches!(event.kind, crate::EventKind::AuthExpired)),
                "{mode}: AuthExpired expected"
            );

            let pending = state.db.pending_change(7, 1).unwrap();
            match mode {
                "clean_delete_401" | "clean_lookup_401" => {
                    assert!(pending.is_none(), "{mode}: phantom deletion created");
                }
                "queued_edit_delete_401" => {
                    let pending = pending.expect("queued edit should survive 401 delete");
                    assert!(!pending.deleting && !pending.conflict, "{mode}");
                    assert_eq!(pending.patch.progress, Some(4), "{mode}");
                    assert_eq!(pending.patch.score, Some(80.0), "{mode}");
                }
                "queued_add_lookup_401" => {
                    let pending = pending.expect("queued add should survive 401 lookup");
                    assert!(!pending.deleting && !pending.conflict, "{mode}");
                    assert_eq!(pending.patch.progress, Some(4), "{mode}");
                }
                "queued_delete_flush_401" => {
                    let pending = pending.expect("pending delete should remain queued");
                    assert!(pending.deleting, "{mode}");
                    assert!(!pending.conflict, "{mode}: 401 must not mark conflict");
                    assert_eq!(pending.error.as_deref(), Some(SESSION_EXPIRED), "{mode}");
                    login_with_token("renewed-token".into(), &state, ListPreferences::Preserve)
                        .await
                        .unwrap();
                    crate::sync_queue::flush_background(&state).await.unwrap();
                    assert!(state.db.pending_change(7, 1).unwrap().is_none());
                    assert!(state.db.get_entry(1).unwrap().is_none());
                }
                _ => unreachable!(),
            }
        }
    }
}
