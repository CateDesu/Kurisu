mod anilist;
mod commands;
mod db;
mod discord;
mod library;
pub mod models;
mod mpvipc;
mod playback;
pub mod profile_transfer;
mod recognize;
mod rss;
mod show_torrents;
mod sync_queue;
pub mod updater;

use parking_lot::Mutex;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

pub use anilist::valid_account_id;
pub use commands::{check_bridge_update, install_bridge_update, TrackingConfig};
pub use db::Db;
pub use library::{library_binding_folder, LibraryBinding, LibraryBindingScope};
pub use models::*;
pub use playback::{NowPlaying, TrackingPrompt};
pub use sync_queue::{PendingChange, WatchHistoryItem};

pub trait Platform: Send + Sync {
    fn open_url(&self, url: &str) -> Result<(), String>;
    fn open_path(&self, path: &Path) -> Result<(), String>;
    fn reveal_path(&self, path: &Path) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateChannel {
    Legacy,
    Native,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub generation: u64,
    pub account_id: Option<i64>,
    pub prompt_id: Option<u64>,
    pub kind: EventKind,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EventKind {
    AuthExpired,
    NowPlaying(NowPlaying),
    TrackingPrompt(TrackingPrompt),
    TrackingAsk(TrackingPrompt),
    EpisodeUpdated(ListEntry),
    PendingChanged(Option<Vec<i64>>),
    WatchHistoryUpdated(i64),
    UpdateAvailable(UpdateInfo),
    UpdateFailed(String),
}

impl EventKind {
    fn is_global(&self) -> bool {
        matches!(self, Self::UpdateAvailable(_) | Self::UpdateFailed(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EventOrigin {
    pub generation: u64,
    pub account_id: Option<i64>,
}

#[derive(Clone)]
pub struct Core {
    state: Arc<commands::AppState>,
    runtime: tokio::runtime::Handle,
    platform: Arc<dyn Platform>,
    channel: UpdateChannel,
    data_dir: PathBuf,
    latest_update: Arc<Mutex<Option<UpdateInfo>>>,
    failed_update: Arc<Mutex<Option<String>>>,
    services_started: Arc<AtomicBool>,
    discord_updates: tokio::sync::watch::Sender<Option<discord::PresenceInfo>>,
}

impl Core {
    pub fn open(
        data_dir: PathBuf,
        runtime: tokio::runtime::Handle,
        platform: Arc<dyn Platform>,
        channel: UpdateChannel,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        let db = db::Db::open(&data_dir.join("kurisu.db")).map_err(|e| e.to_string())?;
        Ok(Self::from_db(db, data_dir, runtime, platform, channel))
    }

    pub fn from_db(
        db: db::Db,
        data_dir: PathBuf,
        runtime: tokio::runtime::Handle,
        platform: Arc<dyn Platform>,
        channel: UpdateChannel,
    ) -> Self {
        let mut anilist = anilist::AniList::new();
        if let Ok(Some(token)) = db.saved_token() {
            anilist.set_token(Some(token));
        }
        let matchers = if anilist.has_token() {
            recognize::build_matchers(&db)
        } else {
            Vec::new()
        };
        let (events, _) = broadcast::channel(128);
        Self {
            state: Arc::new(commands::AppState {
                anilist: Mutex::new(anilist),
                db: Arc::new(db),
                user: Mutex::new(None),
                auth_intent: tokio::sync::watch::channel(0).0,
                entry_lock: Default::default(),
                remote_lock: Default::default(),
                matchers: Mutex::new(Arc::new(matchers)),
                library_cache: Default::default(),
                events,
                last_auth_expired: Mutex::new(None),
                last_now_playing: Mutex::new(None),
                last_tracking_prompt: Mutex::new(None),
            }),
            runtime,
            platform,
            channel,
            data_dir,
            latest_update: Arc::new(Mutex::new(None)),
            failed_update: Arc::new(Mutex::new(None)),
            services_started: Arc::new(AtomicBool::new(false)),
            discord_updates: tokio::sync::watch::channel(None).0,
        }
    }

    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.state.events.subscribe()
    }
    pub fn generation(&self) -> u64 {
        *self.state.auth_intent.borrow()
    }
    pub fn remote_write_active(&self) -> bool {
        self.state.remote_lock.try_lock().is_err()
    }
    /// Waits until active remote writes settle while generation remains current.
    pub async fn wait_for_remote_settlement(&self, generation: u64) -> bool {
        if self.generation() != generation {
            return false;
        }
        let mut changes = self.state.auth_intent.subscribe();
        tokio::select! {
            biased;
            _ = changes.wait_for(|current| *current != generation) => false,
            guard = self.state.remote_lock.lock() => {
                drop(guard);
                self.generation() == generation
            }
        }
    }
    #[cfg(feature = "test-support")]
    pub async fn hold_remote_lock_for_test(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.state.remote_lock.clone().lock_owned().await
    }
    #[cfg(feature = "test-support")]
    pub fn set_failed_update_for_test(&self, message: Option<String>) {
        *self.failed_update.lock() = message;
    }
    pub(crate) fn state(&self) -> &commands::AppState {
        &self.state
    }
    pub fn legacy_setting(&self, key: &str) -> Option<String> {
        if !["close_to_tray", "auto_update"].contains(&key) {
            return None;
        }
        self.state.db.get_setting(key).ok().flatten()
    }
    pub fn channel(&self) -> UpdateChannel {
        self.channel
    }
    pub fn emit(&self, kind: EventKind) {
        self.state.emit(kind);
    }

    pub fn event_is_current(&self, event: &Event) -> bool {
        self.state.event_is_current(event)
    }

    pub fn cached_tracking_target(&self, prompt: &TrackingPrompt) -> Option<i64> {
        self.state
            .db
            .cached_episode_total(prompt.media_id)
            .ok()
            .flatten()
            .map(|total| total.map_or(prompt.episode, |total| prompt.episode.min(total)))
    }

    pub async fn acknowledge_tracking_prompt(&self, event: Event) -> bool {
        self.state.acknowledge_tracking_prompt(event).await
    }

    pub fn recovery_events(&self) -> Vec<Event> {
        let mut events = Vec::with_capacity(6);
        let expired = self.state.last_auth_expired.lock().clone();
        if let Some(expired) = expired {
            if self.event_is_current(&expired) {
                events.push(expired);
            }
        }
        let origin = self.state.origin();
        events.push(Event {
            generation: origin.generation,
            account_id: origin.account_id,
            prompt_id: None,
            kind: EventKind::PendingChanged(None),
        });
        let now = self.state.last_now_playing.lock().clone();
        events.push(
            now.filter(|event| self.event_is_current(event))
                .unwrap_or(Event {
                    generation: origin.generation,
                    account_id: origin.account_id,
                    prompt_id: None,
                    kind: EventKind::NowPlaying(playback::idle()),
                }),
        );
        if let Some(prompt) = self.state.retained_tracking_prompt() {
            events.push(prompt);
        }
        if let Some(info) = self.latest_update.lock().clone() {
            events.push(Event {
                generation: origin.generation,
                account_id: None,
                prompt_id: None,
                kind: EventKind::UpdateAvailable(info),
            });
        }
        if let Some(message) = self.failed_update.lock().clone() {
            events.push(Event {
                generation: origin.generation,
                account_id: None,
                prompt_id: None,
                kind: EventKind::UpdateFailed(message),
            });
        }
        events
    }
    pub(crate) fn update_discord(&self, desired: Option<discord::PresenceInfo>) {
        self.discord_updates.send_replace(desired);
    }

    pub async fn get_preference(&self, key: String) -> Result<Option<String>, String> {
        let db_key = preference_key(&key)?;
        if is_scoped_list_preference(&key) {
            let _entry = self.state.entry_lock.lock().await;
            let has_token = self.state.anilist.lock().has_token();
            return self
                .state
                .db
                .get_scoped_preference(&db_key, has_token)
                .map_err(|e| e.to_string());
        }
        self.state
            .db
            .get_setting(&db_key)
            .map_err(|e| e.to_string())
    }

    pub fn get_list_sort_preferences(&self) -> [Result<Option<String>, String>; 2] {
        self.state
            .db
            .get_list_sort_preferences()
            .map(|value| value.map_err(|error| error.to_string()))
    }

    pub fn set_list_sort_preferences(&self, sort: &str, descending: bool) -> Result<(), String> {
        self.state
            .db
            .set_settings(&[
                ("native_preference:kurisu.list.sort", sort),
                (
                    "native_preference:kurisu.list.dir",
                    if descending { "desc" } else { "asc" },
                ),
            ])
            .map_err(|error| error.to_string())
    }

    pub async fn confirm_tracking_prompt(
        &self,
        event: Event,
        displayed_target: i64,
    ) -> Result<(), String> {
        commands::confirm_tracking_prompt(event, displayed_target, &self.state).await
    }
    pub async fn set_preference(&self, key: String, value: Option<String>) -> Result<(), String> {
        if is_scoped_list_preference(&key) {
            return Err("Scoped list preferences require set_workspace_preference".into());
        }
        let key = preference_key(&key)?;
        match value {
            Some(value) => self
                .state
                .db
                .set_setting(&key, &value)
                .map_err(|e| e.to_string()),
            None => self
                .state
                .db
                .delete_setting(&key)
                .map_err(|e| e.to_string()),
        }
    }

    pub async fn set_workspace_preference(
        &self,
        key: String,
        value: Option<String>,
        account: Option<i64>,
    ) -> Result<(), String> {
        if is_scoped_list_preference(&key) {
            let db_key = preference_key(&key)?;
            // Account replacement and queued list writes must agree on the owner at commit.
            let _entry = self.state.entry_lock.lock().await;
            let has_token = self.state.anilist.lock().has_token();
            return self
                .state
                .db
                .set_scoped_preference(&db_key, value.as_deref(), account, has_token)
                .map_err(|e| e.to_string());
        }
        self.set_preference(key, value).await
    }
}

fn is_scoped_list_preference(key: &str) -> bool {
    matches!(
        key,
        "kurisu.list.q" | "kurisu.list.filter" | "kurisu.list.synced"
    )
}

fn preference_key(key: &str) -> Result<String, String> {
    const KEYS: &[&str] = &[
        "kurisu.list.sort",
        "kurisu.list.dir",
        "kurisu.list.filter",
        "kurisu.list.q",
        "kurisu.list.synced",
        "kurisu.cal.mine",
        "kurisu.torrents.new",
        "kurisu.torrents.statuses",
        "kurisu.torrents.filters",
    ];
    if KEYS.contains(&key) {
        Ok(format!("native_preference:{key}"))
    } else {
        Err(format!("not a UI preference key: {key}"))
    }
}

pub struct Services {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    playback: Option<playback::PlaybackTask>,
    started: Option<Arc<AtomicBool>>,
}

impl Services {
    pub async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some(playback) = self.playback.take() {
            playback.shutdown().await;
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.playback.take();
        if let Some(started) = self.started.take() {
            started.store(false, Ordering::SeqCst);
        }
    }
}

impl Core {
    pub fn start_services(&self) -> Services {
        if self.services_started.swap(true, Ordering::SeqCst) {
            return Services {
                tasks: Vec::new(),
                playback: None,
                started: None,
            };
        }
        self.discord_updates.send_replace(None);
        let discord_rx = self.discord_updates.subscribe();
        let discord = self.spawn(async move {
            discord::run(discord_rx).await;
        });
        let playback = playback::spawn(self.clone());
        let core = self.clone();
        let sync = self.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                if let Err(error) = sync_queue::flush_background(core.state()).await {
                    log::debug!("pending sync: {error}");
                }
            }
        });
        let core = self.clone();
        let updates = self.spawn(async move {
            let data_dir = core.data_dir.clone();
            let failed = tokio::task::spawn_blocking(move || {
                updater::sweep_update_leftovers(&data_dir);
                updater::sweep_install_dir()
            })
            .await
            .unwrap_or(false);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if failed {
                let message = updater::FAILED_MESSAGE.to_string();
                *core.failed_update.lock() = Some(message.clone());
                core.emit(EventKind::UpdateFailed(message));
            }
            if !updater::auto_check_eligible() {
                return;
            }
            let check = core.clone();
            let notify = core.clone();
            updater::watch_updates_channel(
                move || check.legacy_setting("auto_update").as_deref() != Some("0"),
                core.channel,
                move |info| {
                    *notify.latest_update.lock() = Some(info.clone());
                    notify.emit(EventKind::UpdateAvailable(info));
                },
            )
            .await;
        });
        Services {
            tasks: vec![discord, sync, updates],
            playback: Some(playback),
            started: Some(self.services_started.clone()),
        }
    }
}

impl Core {
    pub async fn get_tracking_config(&self) -> TrackingConfig {
        commands::get_tracking_config(&self.state)
    }
    pub async fn try_get_tracking_config(&self) -> Result<TrackingConfig, String> {
        commands::try_get_tracking_config(&self.state)
    }
    pub async fn set_tracking_config(
        &self,
        mode: String,
        prompt_seconds: u64,
        auto_percent: u64,
        auto_ask: bool,
        mpv_ipc_socket: String,
        discord_enabled: bool,
    ) -> Result<TrackingConfig, String> {
        commands::set_tracking_config(
            mode,
            prompt_seconds,
            auto_percent,
            auto_ask,
            mpv_ipc_socket,
            discord_enabled,
            &self.state,
        )
    }
    pub async fn is_logged_in(&self) -> bool {
        commands::is_logged_in(&self.state)
    }
    pub async fn get_app_setting(&self, key: String) -> Result<Option<String>, String> {
        commands::get_app_setting(key, &self.state)
    }
    pub async fn set_app_setting(&self, key: String, value: String) -> Result<(), String> {
        commands::set_app_setting(key, value, &self.state)
    }
    pub async fn login_with_token(&self, token: String) -> Result<User, String> {
        self.login(Some(token), None).await
    }
    pub async fn login_oauth(&self) -> Result<User, String> {
        self.login(None, None).await
    }

    fn list_preferences(&self, last_account: Option<i64>) -> db::ListPreferences {
        match self.channel {
            UpdateChannel::Native => db::ListPreferences::Native { last_account },
            UpdateChannel::Legacy => db::ListPreferences::Preserve,
        }
    }

    /// An absent token starts browser sign-in. The native list owner can outlive sign-out.
    pub async fn login(
        &self,
        token: Option<String>,
        last_account: Option<i64>,
    ) -> Result<User, String> {
        self.login_for_intent(self.begin_login_intent(), token, last_account)
            .await
    }

    pub fn begin_login_intent(&self) -> u64 {
        self.state.begin_auth_intent()
    }

    pub async fn login_for_intent(
        &self,
        intent: u64,
        token: Option<String>,
        last_account: Option<i64>,
    ) -> Result<User, String> {
        let preferences = self.list_preferences(last_account);
        match token {
            Some(token) => {
                commands::login_with_token_inner(token, &self.state, intent, preferences).await
            }
            None => {
                commands::login_oauth(self.platform.as_ref(), &self.state, intent, preferences)
                    .await
            }
        }
    }
    pub fn cancel_login(&self) {
        self.state.cancel_login();
    }
    pub async fn logout(&self) -> Result<(), String> {
        commands::logout(&self.state).await
    }
    pub async fn current_user(&self) -> Result<Option<User>, String> {
        commands::current_user_mode(&self.state, false, self.list_preferences(None)).await
    }
    pub async fn saved_account_id(&self) -> Option<i64> {
        self.state.db.saved_list_owner()
    }
    pub async fn refresh_current_user(&self) -> Result<Option<User>, String> {
        commands::current_user_mode(&self.state, true, self.list_preferences(None)).await
    }
    pub async fn search_anime_page(&self, query: String, page: i64) -> Result<SearchPage, String> {
        commands::search_anime_page(query, page, &self.state).await
    }
    pub async fn get_season(&self, season: String, year: i64) -> Result<Vec<Media>, String> {
        commands::get_season(season, year, &self.state).await
    }
    pub async fn get_recommendations(&self, media_id: i64) -> Result<Vec<Media>, String> {
        commands::get_recommendations(media_id, &self.state).await
    }
    pub async fn get_media(&self, id: i64) -> Result<Media, String> {
        commands::get_media(id, &self.state).await
    }
    pub async fn get_media_detail(&self, id: i64) -> Result<MediaDetail, String> {
        commands::get_media_detail(id, &self.state).await
    }
    pub async fn get_airing_schedule(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Vec<AiringItem>, String> {
        commands::get_airing_schedule(start, end, &self.state).await
    }
    pub async fn sync_my_list(&self) -> Result<Vec<ListEntry>, String> {
        commands::sync_my_list_mode(&self.state, self.list_preferences(None)).await
    }
    pub async fn local_entries(&self) -> Result<Vec<ListEntry>, String> {
        commands::local_entries(&self.state).await
    }
    pub async fn local_entry_updates(
        &self,
        media_ids: Vec<i64>,
        account_id: i64,
    ) -> Result<Vec<Option<ListEntry>>, String> {
        commands::local_entry_updates(media_ids, account_id, &self.state).await
    }
    pub async fn get_entry(&self, media_id: i64) -> Result<Option<ListEntry>, String> {
        commands::get_entry(media_id, &self.state).await
    }
    pub async fn update_entry(
        &self,
        media_id: i64,
        status: Option<String>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
    ) -> Result<ListEntry, String> {
        commands::update_entry(media_id, status, progress, score, repeat, &self.state).await
    }
    pub async fn update_entry_checked(
        &self,
        media_id: i64,
        status: Option<String>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
        expected_progress: Option<i64>,
    ) -> Result<ListEntry, String> {
        commands::save_entry_inner_checked(
            &self.state,
            media_id,
            status,
            progress,
            score,
            repeat,
            expected_progress,
        )
        .await
    }
    pub async fn set_progress(
        &self,
        media_id: i64,
        progress: i64,
        expected: Option<i64>,
    ) -> Result<ListEntry, String> {
        commands::set_progress(media_id, progress, expected, &self.state).await
    }
    pub async fn delete_entry_cmd(&self, media_id: i64) -> Result<(), String> {
        commands::delete_entry_cmd(media_id, &self.state).await
    }
    pub async fn get_library_folders(&self) -> Result<Vec<String>, String> {
        commands::get_library_folders(&self.state).await
    }
    pub async fn add_library_folder(&self, path: String) -> Result<Vec<String>, String> {
        commands::add_library_folder(path, &self.state).await
    }
    pub async fn remove_library_folder(&self, path: String) -> Result<Vec<String>, String> {
        commands::remove_library_folder(path, &self.state).await
    }
    pub async fn scan_library(&self) -> Result<LibraryScan, String> {
        commands::scan_library(&self.state).await
    }
    pub async fn bind_library_path(
        &self,
        path: String,
        media_id: i64,
        episode_offset: Option<i64>,
        scope: LibraryBindingScope,
    ) -> Result<(), String> {
        commands::bind_library_path(path, media_id, episode_offset, scope, &self.state).await
    }
    pub async fn library_binding_for(&self, path: String) -> Result<Option<i64>, String> {
        commands::library_binding_for(path, &self.state).await
    }
    pub async fn library_binding_details(
        &self,
        path: String,
    ) -> Result<Option<LibraryBinding>, String> {
        commands::library_binding_details(path, &self.state).await
    }
    pub async fn get_watch_history(
        &self,
        limit: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<WatchHistoryItem>, String> {
        commands::get_watch_history(limit, before, &self.state).await
    }
    pub async fn unbind_library_media(&self, media_id: i64) -> Result<(), String> {
        commands::unbind_library_media(media_id, &self.state).await
    }
    pub async fn get_rss_feeds(&self) -> Result<Vec<String>, String> {
        commands::get_rss_feeds(&self.state)
    }
    pub async fn add_rss_feed(&self, url: String) -> Result<Vec<String>, String> {
        commands::add_rss_feed(url, &self.state)
    }
    pub async fn remove_rss_feed(&self, url: String) -> Result<Vec<String>, String> {
        commands::remove_rss_feed(url, &self.state)
    }
    pub async fn fetch_torrents(&self) -> Result<TorrentFetch, String> {
        commands::fetch_torrents(&self.state).await
    }
    pub async fn mark_torrents_seen(&self, guids: Vec<String>) -> Result<(), String> {
        commands::mark_torrents_seen(guids, &self.state)
    }
    pub async fn search_torrents(
        &self,
        query: String,
        category: Option<String>,
        filter: Option<String>,
    ) -> Result<Vec<TorrentItem>, String> {
        commands::search_torrents(query, category, filter, &self.state).await
    }
    pub async fn find_show_torrents(
        &self,
        media_id: i64,
        category: Option<String>,
        filter: Option<String>,
    ) -> Result<ShowTorrents, String> {
        commands::find_show_torrents(media_id, category, filter, &self.state).await
    }
    pub async fn get_user_stats(&self) -> Result<UserStats, String> {
        commands::get_user_stats(&self.state).await
    }
    pub async fn get_notifications_page(&self, page: i64) -> Result<NotificationPage, String> {
        commands::get_notifications_page(page, &self.state).await
    }
    pub async fn mark_notifications_read(&self) -> Result<(), String> {
        commands::mark_notifications_read(&self.state).await
    }
    pub async fn get_entry_details(&self, media_id: i64) -> Result<EntryDetails, String> {
        commands::get_entry_details(media_id, &self.state).await
    }
    pub async fn update_entry_details(
        &self,
        media_id: i64,
        notes: Option<String>,
        started_at: Option<FuzzyDate>,
        completed_at: Option<FuzzyDate>,
        custom_lists: Option<Vec<String>>,
    ) -> Result<EntryDetails, String> {
        commands::update_entry_details(
            media_id,
            notes,
            started_at,
            completed_at,
            custom_lists,
            &self.state,
        )
        .await
    }
    pub async fn check_update(&self) -> Result<UpdateInfo, String> {
        commands::check_update(self.channel).await
    }
    pub async fn install_update(&self) -> Result<String, String> {
        commands::install_update(self.data_dir.clone(), self.channel).await
    }
    pub async fn get_pending_changes(&self) -> Result<Vec<PendingChange>, String> {
        sync_queue::get_pending_changes(&self.state).await
    }
    /// Returns no result if the requested session is no longer current.
    pub async fn get_pending_changes_for(
        &self,
        generation: u64,
        account: i64,
    ) -> Result<Option<Vec<PendingChange>>, String> {
        sync_queue::read_pending_changes(
            &self.state,
            Some(EventOrigin {
                generation,
                account_id: Some(account),
            }),
        )
        .await
    }
    pub async fn sync_pending_changes(&self) -> Result<Vec<PendingChange>, String> {
        sync_queue::sync_pending_changes(&self.state).await
    }
    pub async fn resolve_pending_change(
        &self,
        media_id: i64,
        keep_local: bool,
    ) -> Result<(), String> {
        sync_queue::resolve_pending_change(media_id, keep_local, &self.state).await
    }
    pub fn pending_update_failed(&self) -> Option<String> {
        self.failed_update.lock().clone()
    }
    pub fn clear_update_failed(&self) {
        let had = self.failed_update.lock().take().is_some();
        if had {
            drop(self.runtime.spawn_blocking(|| {
                let _ = updater::take_update_failed();
            }));
        }
    }
    pub async fn take_update_failed(&self) -> Option<String> {
        let failed = self.failed_update.lock().take();
        if failed.is_some() {
            let _ = tokio::task::spawn_blocking(updater::take_update_failed).await;
        }
        failed
    }
    pub async fn take_pending_update(&self) -> Option<UpdateInfo> {
        self.latest_update.lock().take()
    }
}

#[cfg(test)]
mod core_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestPlatform;
    impl Platform for TestPlatform {
        fn open_url(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn reveal_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    fn open(label: &str) -> (Core, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kurisu-core-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let core = Core::open(
            dir.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        (core, dir)
    }

    fn logout_fixture(db: Db) -> Core {
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("Fixture series".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        Core::from_db(
            db,
            PathBuf::new(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
    }

    #[tokio::test]
    async fn partial_logout_keeps_the_cleared_list_empty_until_reconnection() {
        for key in ["anilist_username", "anilist_user_id"] {
            let db = Db::open(Path::new(":memory:")).unwrap();
            db.replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Fixture".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let core = logout_fixture(db);
            assert_eq!(core.state.matchers.lock()[0].media_id, 1);
            let db = &core.state.db;
            let (_, pending, _) = sync_queue::stage(
                &core.state,
                1,
                sync_queue::Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(core.local_entries().await.unwrap()[0].progress, 4);
            db.0.lock()
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_logout_cleanup BEFORE DELETE ON settings
                 WHEN OLD.key = '{key}'
                 BEGIN SELECT RAISE(ABORT, 'fixture identity cleanup failed'); END;"
                ))
                .unwrap();

            let error = core.logout().await.unwrap_err();
            assert!(error.contains("fixture identity cleanup failed"), "{error}");
            assert!(core.current_user().await.unwrap().is_none());
            assert!(db.get_setting("anilist_token").unwrap().is_none());
            assert!(db.get_entry(1).unwrap().is_none());
            assert!(core.state.matchers.lock().is_empty());
            assert!(core.local_entries().await.unwrap().is_empty());
            assert!(core.get_entry(1).await.unwrap().is_none());
            assert!(core.get_pending_changes().await.unwrap().is_empty());
            db.record_watch(7, "/anime/ep01.mkv", 1, 1).unwrap();
            assert!(core.get_watch_history(None, None).await.unwrap().is_empty());
            assert_eq!(
                core.find_show_torrents(1, None, None).await.unwrap_err(),
                "This show is no longer on your list"
            );
            assert_eq!(
                core.bind_library_path("/anime".into(), 1, None, LibraryBindingScope::Folder)
                    .await
                    .unwrap_err(),
                "only shows on your list can be linked"
            );
            assert_eq!(
                core.library_binding_details("/anime/ep01.mkv".into())
                    .await
                    .unwrap(),
                None
            );
            assert!(core.sync_pending_changes().await.unwrap().is_empty());
            assert!(db.get_entry(1).unwrap().is_none());
            assert!(core.state.matchers.lock().is_empty());
            assert_eq!(
                serde_json::to_value(&db.pending(7).unwrap()[0]).unwrap(),
                serde_json::to_value(pending).unwrap()
            );

            db.0.lock()
                .execute_batch("DROP TRIGGER fail_logout_cleanup")
                .unwrap();
            core.logout().await.unwrap();
            assert!(db.cached_user().unwrap().is_none());
            let (mut api, _requests) = anilist::mock_api(vec![(
                200,
                serde_json::json!({
                    "data":{"Viewer":{"id":7,"name":"Fixture"}}
                }),
            )])
            .await;
            api.set_token(None);
            *core.state.anilist.lock() = api;
            assert_eq!(
                core.login_with_token("test-session".into())
                    .await
                    .unwrap()
                    .id,
                7
            );
            let entries = core.local_entries().await.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!((entries[0].media_id, entries[0].progress), (1, 4));
            assert_eq!(core.state.matchers.lock()[0].media_id, 1);
        }
    }

    fn persistence_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap()
    }

    async fn occupy_blocking_worker(
        wait: std::time::Duration,
    ) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let occupied = tokio::task::spawn_blocking(move || {
            entered.send(()).unwrap();
            blocked.recv_timeout(wait).unwrap();
        });
        tokio::time::timeout(wait, started).await.unwrap().unwrap();
        (release, occupied)
    }

    #[test]
    fn admitted_public_logout_finishes_after_its_caller_is_dropped() {
        use std::{
            task::{Context, Waker},
            time::Duration,
        };
        persistence_runtime().block_on(async {
            let db = Db::open(Path::new(":memory:")).unwrap();
            db.replace_account(
                "fixture-session",
                &User {
                    id: 7,
                    name: "Fixture".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let core = logout_fixture(db);
            assert_eq!(core.state.matchers.lock()[0].media_id, 1);
            let wait = Duration::from_secs(5);
            let (release, occupied) = occupy_blocking_worker(wait).await;
            let mut logout = Box::pin(core.logout());
            assert!(logout
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending());
            drop(logout);
            assert!(core.state.remote_lock.try_lock().is_err());
            assert!(core.state.entry_lock.try_lock().is_err());
            release.send(()).unwrap();
            occupied.await.unwrap();
            let _remote = tokio::time::timeout(wait, core.state.remote_lock.lock())
                .await
                .unwrap();
            let _entry = tokio::time::timeout(wait, core.state.entry_lock.lock())
                .await
                .unwrap();
            assert!(core.state.db.get_entry(1).unwrap().is_none());
            assert!(core
                .state
                .db
                .get_setting("anilist_user_id")
                .unwrap()
                .is_none());
            assert!(core.state.matchers.lock().is_empty());
        });
    }

    #[test]
    fn admitted_public_sync_finishes_after_its_caller_is_dropped() {
        use serde_json::json;
        use std::{future::poll_fn, task::Poll, time::Duration};
        use tokio::time::timeout;

        persistence_runtime().block_on(async {
            let db = Db::open(Path::new(":memory:")).unwrap();
            db.replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Fixture".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let core = logout_fixture(db);
            let state = &core.state;
            assert_eq!(state.matchers.lock()[0].display, "Fixture series");
            let mut entry = state.db.get_entry(1).unwrap().unwrap();
            entry.id = Some(22);
            state.db.upsert_entry(&entry).unwrap();
            let (_, mut pending, _) = sync_queue::stage(
                state,
                1,
                sync_queue::Patch {
                    progress: Some(8),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            pending.conflict = true;
            state.db.put_pending(7, &pending).unwrap();

            let mut media = anilist::mock_media(1);
            media["title"]["english"] = json!("Renamed fixture series");
            let snapshot = json!({"data":{"MediaListCollection":{
                "user":{"id":7}, "hasNextChunk":false, "lists":[{"entries":[{
                    "id":22, "mediaId":1, "status":"CURRENT", "progress":3,
                    "score":0, "repeat":0, "media":media
                }]}]
            }}});
            let viewer = json!({"data":{"Viewer":{"id":7,"name":"Fixture"}}});
            let (api, _requests) = anilist::mock_api(vec![(200, snapshot), (200, viewer)]).await;
            *state.anilist.lock() = api;

            let wait = Duration::from_secs(5);
            let (release, occupied) = occupy_blocking_worker(wait).await;
            let mut sync = Box::pin(core.sync_my_list());
            timeout(
                wait,
                poll_fn(|cx| {
                    if let Poll::Ready(result) = sync.as_mut().poll(cx) {
                        panic!("sync finished before admission: {result:?}");
                    }
                    // Only the queued persistence job holds the entry lock between polls.
                    if state.entry_lock.try_lock().is_err() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                }),
            )
            .await
            .expect("sync must reach persistence");
            assert!(state.user.lock().is_none());
            drop(sync);
            assert!(state.remote_lock.try_lock().is_err());
            assert!(state.entry_lock.try_lock().is_err());
            release.send(()).unwrap();
            occupied.await.unwrap();
            let _remote = timeout(wait, state.remote_lock.lock()).await.unwrap();
            let _entry = timeout(wait, state.entry_lock.lock()).await.unwrap();
            assert_eq!(state.user.lock().as_ref().unwrap().id, 7);
            assert_eq!(
                state.db.get_entry(1).unwrap().unwrap().progress,
                8,
                "pending progress overlay"
            );
            assert_eq!(
                state.matchers.lock()[0].display,
                "Renamed fixture series",
                "snapshot matcher refresh"
            );
        });
    }

    #[tokio::test]
    async fn failed_cache_clear_keeps_signed_out_lists_and_matchers_empty() {
        for expired in [false, true] {
            let db = Db::open(Path::new(":memory:")).unwrap();
            db.replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Fixture".into(),
                    ..Default::default()
                },
            )
            .unwrap();
            let core = logout_fixture(db);
            assert_eq!(core.state.matchers.lock()[0].media_id, 1);
            let db = &core.state.db;
            assert_eq!(core.local_entries().await.unwrap()[0].media_id, 1);
            db.0.lock()
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_list_clear BEFORE DELETE ON list_entry
                 BEGIN SELECT RAISE(ABORT, 'fixture list clear failed'); END;",
                )
                .unwrap();

            let mut media = anilist::mock_media(1);
            media["title"]["english"] = serde_json::json!("Fixture series");
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(
                vec![(
                    200,
                    serde_json::json!({"data":{"Page":{
                        "pageInfo":{"hasNextPage":false}, "media":[media]
                    }}}),
                )],
                Some(gate.clone()),
            )
            .await;
            *core.state.anilist.lock() = api;
            let caller = core.clone();
            let search =
                tokio::spawn(
                    async move { caller.search_anime_page("Fixture series".into(), 1).await },
                );
            let wait = std::time::Duration::from_secs(5);
            let request = tokio::time::timeout(wait, requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request["variables"]["search"], "Fixture series");

            if expired {
                let (api, _requests) = anilist::mock_api(vec![(
                    401,
                    serde_json::json!({"errors":[{"message":"Invalid token"}]}),
                )])
                .await;
                *core.state.anilist.lock() = api;
                let mut events = core.subscribe();
                assert!(core.refresh_current_user().await.unwrap().is_none());
                let event = events.try_recv().unwrap();
                assert!(matches!(event.kind, EventKind::AuthExpired));
                assert!(core.event_is_current(&event));
            } else {
                let error = core.logout().await.unwrap_err();
                assert!(error.contains("fixture list clear failed"), "{error}");
            }
            assert!(core.current_user().await.unwrap().is_none());
            assert!(db.get_entry(1).unwrap().is_some());
            assert!(core.state.matchers.lock().is_empty(), "expired={expired}");
            assert!(core.local_entries().await.unwrap().is_empty());
            assert!(core.get_entry(1).await.unwrap().is_none());
            assert!(core.get_pending_changes().await.unwrap().is_empty());
            assert!(core.get_watch_history(None, None).await.unwrap().is_empty());
            assert_eq!(
                core.find_show_torrents(1, None, None).await.unwrap_err(),
                "This show is no longer on your list"
            );
            assert_eq!(
                core.bind_library_path("/anime".into(), 1, None, LibraryBindingScope::Folder)
                    .await
                    .unwrap_err(),
                "only shows on your list can be linked"
            );
            assert_eq!(
                core.library_binding_details("/anime/ep01.mkv".into())
                    .await
                    .unwrap(),
                None
            );
            assert!(core.sync_pending_changes().await.unwrap().is_empty());
            assert!(db.get_setting("anilist_username").unwrap().is_none());
            assert!(db.get_setting("anilist_user_id").unwrap().is_none());
            assert!(db.get_setting("anilist_profile").unwrap().is_none());
            core.state.refresh_matchers();
            assert!(core.state.matchers.lock().is_empty());

            gate.add_permits(1);
            let error = tokio::time::timeout(wait, search)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            let expected = if expired {
                "Your AniList account changed. Try again."
            } else {
                "A newer sign-in or sign-out replaced this attempt."
            };
            assert_eq!(error, expected, "expired={expired}");
            assert!(
                core.state.matchers.lock().is_empty(),
                "search restored signed-out recognition, expired={expired}"
            );
        }
    }

    #[tokio::test]
    async fn opening_cache_without_a_saved_token_keeps_it_hidden() {
        let core = logout_fixture(Db::open(Path::new(":memory:")).unwrap());
        assert!(core.state.db.get_entry(1).unwrap().is_some());
        assert!(core.state.matchers.lock().is_empty());
        library::bind_path(&core.state.db, "/anime", 1, -2).unwrap();
        core.state
            .db
            .put_pending(
                7,
                &sync_queue::Pending {
                    media_id: 1,
                    deleting: false,
                    deletion_id: None,
                    base: core.state.db.get_entry(1).unwrap(),
                    patch: sync_queue::Patch {
                        progress: Some(5),
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
        core.state.db.set_setting("anilist_user_id", "7").unwrap();
        assert!(core.local_entries().await.unwrap().is_empty());
        assert!(core.get_entry(1).await.unwrap().is_none());
        assert!(core.get_pending_changes().await.unwrap().is_empty());
        assert!(core.sync_pending_changes().await.unwrap().is_empty());
        assert!(core.get_watch_history(None, None).await.unwrap().is_empty());
        *core.state.matchers.lock() = Arc::new(recognize::build_matchers(&core.state.db));
        assert!(!core.state.matchers.lock().is_empty());
        assert_eq!(
            core.find_show_torrents(1, None, None).await.unwrap_err(),
            "This show is no longer on your list"
        );
        assert_eq!(
            core.library_binding_details("/anime/ep03.mkv".into())
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            core.bind_library_path("/anime".into(), 1, None, LibraryBindingScope::Folder)
                .await
                .unwrap_err(),
            "only shows on your list can be linked"
        );
    }

    #[tokio::test]
    async fn failed_token_delete_blanks_token_stays_signed_out_and_cleans_up() {
        let (core, dir) = open("logout-blank-fallback");
        let user = User {
            id: 7,
            name: "Fixture".into(),
            ..Default::default()
        };
        core.state
            .db
            .replace_account("test-session", &user)
            .unwrap();
        core.state
            .db
            .upsert_media(&Media {
                id: 1,
                title_english: Some("Fixture series".into()),
                ..Default::default()
            })
            .unwrap();
        core.state
            .db
            .upsert_entry(&ListEntry {
                media_id: 1,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        core.state
            .anilist
            .lock()
            .set_token(Some("test-session".into()));
        *core.state.user.lock() = Some(user);
        core.state.refresh_matchers();
        assert_eq!(core.state.matchers.lock()[0].media_id, 1);

        let db = &core.state.db;
        db.0.lock()
            .execute_batch(
                "CREATE TRIGGER fail_token_delete BEFORE DELETE ON settings
                 WHEN OLD.key = 'anilist_token'
                 BEGIN SELECT RAISE(ABORT, 'fixture token delete failed'); END;",
            )
            .unwrap();

        let error = core.logout().await.unwrap_err();
        assert!(error.contains("fixture token delete failed"), "{error}");
        assert!(!core.is_logged_in().await);
        assert!(core.current_user().await.unwrap().is_none());
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("")
        );
        assert!(db.get_entry(1).unwrap().is_none());
        assert!(db.get_setting("anilist_username").unwrap().is_none());
        assert!(db.get_setting("anilist_user_id").unwrap().is_none());
        assert!(db.get_setting("anilist_profile").unwrap().is_none());
        assert!(core.state.matchers.lock().is_empty());
        drop(core);

        let reopened = Core::open(
            dir.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        assert!(!reopened.is_logged_in().await);
        assert!(reopened.current_user().await.unwrap().is_none());
        assert!(reopened.state.matchers.lock().is_empty());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_token_delete_and_blank_restores_in_memory_session_for_retry() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let user = User {
            id: 7,
            name: "Fixture".into(),
            ..Default::default()
        };
        db.replace_account("test-session", &user).unwrap();
        let core = logout_fixture(db);
        *core.state.user.lock() = Some(user);
        let db = &core.state.db;
        db.0.lock()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_token_delete BEFORE DELETE ON settings
                 WHEN OLD.key = 'anilist_token'
                 BEGIN SELECT RAISE(ABORT, 'fixture token delete failed'); END;
                 CREATE TEMP TRIGGER fail_token_insert BEFORE INSERT ON settings
                 WHEN NEW.key = 'anilist_token'
                 BEGIN SELECT RAISE(ABORT, 'fixture token blank failed'); END;
                 CREATE TEMP TRIGGER fail_token_update BEFORE UPDATE ON settings
                 WHEN NEW.key = 'anilist_token'
                 BEGIN SELECT RAISE(ABORT, 'fixture token blank failed'); END;",
            )
            .unwrap();

        let error = core.logout().await.unwrap_err();
        assert!(error.contains("fixture token delete failed"), "{error}");
        assert!(core.is_logged_in().await);
        assert_eq!(core.current_user().await.unwrap().unwrap().id, 7);
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("test-session")
        );
        assert!(db.get_entry(1).unwrap().is_some());
        assert_eq!(core.state.matchers.lock()[0].media_id, 1);

        db.0.lock()
            .execute_batch(
                "DROP TRIGGER fail_token_delete;
                 DROP TRIGGER fail_token_insert;
                 DROP TRIGGER fail_token_update;",
            )
            .unwrap();
        core.logout().await.unwrap();
        assert!(!core.is_logged_in().await);
        assert!(core.current_user().await.unwrap().is_none());
        assert!(db.get_setting("anilist_token").unwrap().is_none());
        assert!(db.get_entry(1).unwrap().is_none());
    }

    #[tokio::test]
    async fn rejected_token_cleanup_handles_database_failures_across_restart() {
        for (blank_blocked, marker_failure) in [
            (false, None),
            (true, None),
            (true, Some("ABORT, 'fixture rejection marker failed'")),
            (true, Some("IGNORE")),
        ] {
            let (core, dir) = open("rejected-token-cleanup");
            let user = User {
                id: 7,
                name: "Fixture".into(),
                ..Default::default()
            };
            let db = &core.state.db;
            db.replace_account("test-session", &user).unwrap();
            db.0.lock()
                .execute_batch(
                    "CREATE TRIGGER refuse_token_delete BEFORE DELETE ON settings
                     WHEN OLD.key = 'anilist_token'
                     BEGIN SELECT RAISE(ABORT, 'fixture delete failed'); END;",
                )
                .unwrap();
            if blank_blocked {
                db.0.lock()
                    .execute_batch(
                        "CREATE TRIGGER refuse_token_blank BEFORE INSERT ON settings
                         WHEN NEW.key = 'anilist_token' AND NEW.value = ''
                         BEGIN SELECT RAISE(ABORT, 'fixture blank failed'); END;",
                    )
                    .unwrap();
            }
            if let Some(failure) = marker_failure {
                db.0.lock()
                    .execute_batch(&format!(
                        "CREATE TRIGGER refuse_token_marker BEFORE INSERT ON settings
                         WHEN NEW.key = 'anilist_token_rejected'
                         BEGIN SELECT RAISE({failure}); END;"
                    ))
                    .unwrap();
            }
            let (api, _) = anilist::mock_api(vec![(
                401,
                serde_json::json!({"errors":[{"message":"Invalid token"}]}),
            )])
            .await;
            *core.state.anilist.lock() = api;
            let result = core.refresh_current_user().await;
            if marker_failure.is_some() {
                assert!(result
                    .unwrap_err()
                    .contains("could not disable the saved token"));
            } else {
                assert!(result.unwrap().is_none());
            }
            assert!(!core.is_logged_in().await);
            assert_eq!(
                db.get_setting(db::TOKEN_KEY).unwrap().as_deref(),
                Some(if blank_blocked { "test-session" } else { "" })
            );
            drop(core);

            let reopen = || {
                Core::open(
                    dir.clone(),
                    tokio::runtime::Handle::current(),
                    Arc::new(TestPlatform),
                    UpdateChannel::Native,
                )
                .unwrap()
            };
            let reopened = reopen();
            assert_eq!(reopened.is_logged_in().await, marker_failure.is_some());
            let viewer = (
                200,
                serde_json::json!({"data":{"Viewer":{"id":7,"name":"Fixture"}}}),
            );
            let rejected_token_retained = blank_blocked && marker_failure.is_none();
            let (api, _) =
                anilist::mock_api(vec![viewer; 1 + usize::from(rejected_token_retained)]).await;
            *reopened.state.anilist.lock() = api;
            if rejected_token_retained {
                reopened
                    .state
                    .db
                    .0
                    .lock()
                    .execute_batch(
                        "CREATE TRIGGER refuse_marker_removal BEFORE DELETE ON settings
                     WHEN OLD.key = 'anilist_token_rejected' BEGIN SELECT RAISE(IGNORE); END;",
                    )
                    .unwrap();
                assert!(reopened
                    .login_with_token("unsaved-session".into())
                    .await
                    .is_err());
                assert_eq!(reopen().state.db.saved_token().unwrap(), None);
                reopened
                    .state
                    .db
                    .0
                    .lock()
                    .execute_batch("DROP TRIGGER refuse_marker_removal")
                    .unwrap();
            }
            assert_eq!(
                reopened
                    .login_with_token("new-session".into())
                    .await
                    .unwrap()
                    .id,
                7
            );
            drop(reopened);
            assert!(reopen().is_logged_in().await);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn rejection_disables_saved_token_before_settling_the_interrupted_write() {
        let (core, dir) = open("rejected-token-before-settlement");
        let state = &core.state;
        let user = User {
            id: 7,
            name: "Fixture".into(),
            ..Default::default()
        };
        state.db.replace_account("test-session", &user).unwrap();
        let (api, _) = anilist::mock_api(vec![(
            401,
            serde_json::json!({"errors":[{"message":"Invalid token"}]}),
        )])
        .await;
        let error = api.viewer().await.unwrap_err();
        *state.anilist.lock() = api;
        let token = state.anilist.lock().token();
        let _remote = state.remote_lock.lock().await;
        let _entry = state.entry_lock.lock().await;
        commands::settle_write_error(state, state.origin(), &token, &error, |_| {
            let reopened = Db::open(&dir.join("kurisu.db")).unwrap();
            assert!(reopened.saved_token().unwrap().is_none());
            assert_eq!(sync_queue::account_id(&state.db), Some(7));
            Ok(())
        })
        .unwrap();
        drop(_entry);
        drop(_remote);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn saved_list_owner_prefers_persisted_owner_over_legacy_account_replacement() {
        let (core, dir) = open("saved-list-owner-precedence");
        let viewer_a = serde_json::json!({"data":{"Viewer":{"id":71,"name":"Alice"}}});
        let (api, _) = anilist::mock_api(vec![(200, viewer_a)]).await;
        *core.state.anilist.lock() = api;
        assert_eq!(core.login(Some("tok-a".into()), None).await.unwrap().id, 71);
        core.set_workspace_preference("kurisu.list.q".into(), Some("alice query".into()), Some(71))
            .await
            .unwrap();
        core.state
            .db
            .replace_account(
                "legacy-tok-b",
                &User {
                    id: 72,
                    name: "Bob".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            core.state
                .db
                .get_setting("anilist_user_id")
                .unwrap()
                .as_deref(),
            Some("72")
        );
        assert_eq!(
            core.state
                .db
                .get_setting(db::LIST_OWNER_KEY)
                .unwrap()
                .as_deref(),
            Some("71")
        );
        assert_eq!(core.saved_account_id().await, Some(71));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn failed_list_owner_preservation_clears_scoped_preferences_on_logout_and_expiry() {
        for expired in [false, true] {
            let (core, dir) = open(&format!("preserve-owner-failure-{expired}"));
            let viewer_a = serde_json::json!({"data":{"Viewer":{"id":71,"name":"Alice"}}});
            let (api, _) = anilist::mock_api(vec![(200, viewer_a)]).await;
            *core.state.anilist.lock() = api;
            assert_eq!(core.login(Some("tok-a".into()), None).await.unwrap().id, 71);
            for (key, val) in [
                ("kurisu.list.q", "alice private query"),
                ("kurisu.list.filter", "PAUSED"),
                ("kurisu.list.synced", "999"),
            ] {
                core.set_workspace_preference(key.into(), Some(val.into()), Some(71))
                    .await
                    .unwrap();
            }
            core.state.db.delete_setting(db::LIST_OWNER_KEY).unwrap();
            core.state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_owner_preserve BEFORE INSERT ON settings
                     WHEN NEW.key = 'native_preference:kurisu.list.owner'
                     BEGIN SELECT RAISE(ABORT, 'fixture owner preserve failed'); END;",
                )
                .unwrap();

            if expired {
                let (api, _) = anilist::mock_api(vec![(
                    401,
                    serde_json::json!({"errors":[{"message":"Invalid token"}]}),
                )])
                .await;
                *core.state.anilist.lock() = api;
                assert!(core.refresh_current_user().await.unwrap().is_none());
            } else {
                let error = core.logout().await.unwrap_err();
                assert!(error.contains("fixture owner preserve failed"), "{error}");
            }
            for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
                assert!(
                    core.get_preference(key.into()).await.unwrap().is_none(),
                    "expired={expired}, key={key}"
                );
            }
            core.state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_owner_preserve")
                .unwrap();
            let viewer_b = serde_json::json!({"data":{"Viewer":{"id":72,"name":"Bob"}}});
            let (api, _) = anilist::mock_api(vec![(200, viewer_b)]).await;
            *core.state.anilist.lock() = api;
            assert_eq!(core.login(Some("tok-b".into()), None).await.unwrap().id, 72);
            assert!(core
                .get_preference("kurisu.list.q".into())
                .await
                .unwrap()
                .is_none());
            drop(core);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn get_pending_changes_waits_for_entry_lock_account_transition() {
        use std::task::{Context, Waker};

        let db = Db::open(Path::new(":memory:")).unwrap();
        db.replace_account(
            "tok-7",
            &User {
                id: 7,
                name: "Seven".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let core = logout_fixture(db);
        sync_queue::stage(
            &core.state,
            1,
            sync_queue::Patch {
                progress: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        let transition = core.state.entry_lock.lock().await;
        let mut pending = Box::pin(core.get_pending_changes());
        assert!(
            pending
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "get_pending_changes must serialize with entry_lock"
        );
        core.state
            .db
            .replace_account(
                "tok-8",
                &User {
                    id: 8,
                    name: "Eight".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        core.state.anilist.lock().set_token(Some("tok-8".into()));
        drop(transition);
        assert!(pending.await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_preference_owner_survives_sign_out_and_restart_and_blocks_late_writes() {
        for expired in [false, true] {
            let (core, dir) = open(&format!("restart-list-owner-{expired}"));
            let viewer_a = serde_json::json!({"data":{"Viewer":{"id":71,"name":"Alice"}}});
            let (api, _) = anilist::mock_api(vec![(200, viewer_a.clone())]).await;
            *core.state.anilist.lock() = api;
            assert_eq!(core.login(Some("tok-a".into()), None).await.unwrap().id, 71);
            for (key, val) in [
                ("kurisu.list.q", "private search"),
                ("kurisu.list.filter", "DROPPED"),
                ("kurisu.list.synced", "12345"),
            ] {
                core.set_workspace_preference(key.into(), Some(val.into()), Some(71))
                    .await
                    .unwrap();
            }
            core.set_preference("kurisu.list.sort".into(), Some("title".into()))
                .await
                .unwrap();

            if expired {
                let (api, _) = anilist::mock_api(vec![(
                    401,
                    serde_json::json!({"errors":[{"message":"Invalid token"}]}),
                )])
                .await;
                *core.state.anilist.lock() = api;
                assert!(core.refresh_current_user().await.unwrap().is_none());
            } else {
                core.logout().await.unwrap();
            }
            for late_account in [Some(71), None] {
                core.set_workspace_preference(
                    "kurisu.list.q".into(),
                    Some("late signed-out leak".into()),
                    late_account,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                core.get_preference("kurisu.list.q".into())
                    .await
                    .unwrap()
                    .as_deref(),
                Some("private search")
            );
            drop(core);

            let reopened = Core::open(
                dir.clone(),
                tokio::runtime::Handle::current(),
                Arc::new(TestPlatform),
                UpdateChannel::Native,
            )
            .unwrap();
            assert_eq!(reopened.saved_account_id().await, Some(71));
            let (api, _) = anilist::mock_api(vec![(200, viewer_a)]).await;
            *reopened.state.anilist.lock() = api;
            assert_eq!(
                reopened
                    .login(Some("tok-a2".into()), None)
                    .await
                    .unwrap()
                    .id,
                71
            );
            assert_eq!(
                reopened
                    .get_preference("kurisu.list.q".into())
                    .await
                    .unwrap()
                    .as_deref(),
                Some("private search")
            );
            reopened.logout().await.unwrap();
            drop(reopened);

            let reopened = Core::open(
                dir.clone(),
                tokio::runtime::Handle::current(),
                Arc::new(TestPlatform),
                UpdateChannel::Native,
            )
            .unwrap();
            assert_eq!(reopened.saved_account_id().await, Some(71));
            let viewer_b = serde_json::json!({"data":{"Viewer":{"id":72,"name":"Bob"}}});
            let (api, _) = anilist::mock_api(vec![(200, viewer_b)]).await;
            *reopened.state.anilist.lock() = api;
            assert_eq!(
                reopened.login(Some("tok-b".into()), None).await.unwrap().id,
                72
            );
            for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
                assert!(
                    reopened.get_preference(key.into()).await.unwrap().is_none(),
                    "expired={expired}, key={key}"
                );
            }
            assert_eq!(
                reopened
                    .get_preference("kurisu.list.sort".into())
                    .await
                    .unwrap()
                    .as_deref(),
                Some("title")
            );
            assert_eq!(reopened.saved_account_id().await, Some(72));
            drop(reopened);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_login_before_a_job_starts_keeps_the_existing_account() {
        let (core, dir) = open("cancel-unstarted-login");
        let user = User {
            id: 7,
            name: "Existing".into(),
            ..Default::default()
        };
        core.state
            .db
            .replace_account("existing-token", &user)
            .unwrap();
        core.state
            .anilist
            .lock()
            .set_token(Some("existing-token".into()));
        *core.state.user.lock() = Some(user);
        let waiting = core.clone();
        let job = tokio::spawn(async move { waiting.login_oauth().await });
        core.cancel_login();
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        assert_eq!(core.generation(), 1);
        assert_eq!(
            core.state.anilist.lock().token().as_deref(),
            Some("existing-token")
        );
        assert_eq!(
            core.state
                .db
                .get_setting("anilist_token")
                .unwrap()
                .as_deref(),
            Some("existing-token")
        );
        assert_eq!(core.state.db.cached_user().unwrap().unwrap().id, 7);
        assert_eq!(core.state.user.lock().as_ref().unwrap().id, 7);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn events_keep_their_core_and_session_identity() {
        let (a, a_dir) = open("a");
        let (b, b_dir) = open("b");
        let mut a_events = a.subscribe();
        let mut b_events = b.subscribe();
        a.emit(EventKind::PendingChanged(Some(vec![17])));
        let first = a_events.recv().await.unwrap();
        assert_eq!(first.generation, 0);
        assert_eq!(first.account_id, None);
        assert!(matches!(first.kind, EventKind::PendingChanged(Some(ids)) if ids == vec![17]));
        assert!(matches!(
            b_events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        a.state.auth_intent.send_replace(1);
        a.state.db.set_setting("anilist_user_id", "77").unwrap();
        a.emit(EventKind::AuthExpired);
        let next = a_events.recv().await.unwrap();
        assert_eq!(next.generation, 1);
        assert_eq!(next.account_id, Some(77));
        drop((a_events, b_events, a, b));
        std::fs::remove_dir_all(a_dir).unwrap();
        std::fs::remove_dir_all(b_dir).unwrap();
    }

    #[tokio::test]
    async fn lag_recovery_uses_current_account_and_global_update_state() {
        let (core, dir) = open("recovery");
        core.state.db.set_setting("anilist_user_id", "7").unwrap();
        let mut playing = playback::idle();
        playing.active = true;
        playing.media_id = Some(11);
        core.emit(EventKind::NowPlaying(playing));
        core.state.db.set_setting("anilist_user_id", "8").unwrap();
        let update = UpdateInfo {
            available: true,
            can_install: false,
            restart_pending: false,
            version: "2.0.0".into(),
            tag: "v2.0.0".into(),
            html_url: String::new(),
            body: String::new(),
            current: "1.0.0".into(),
        };
        *core.latest_update.lock() = Some(update);
        *core.failed_update.lock() = Some("failed".into());
        let recovered = core.recovery_events();
        assert!(recovered.iter().any(|event| matches!(
            &event.kind,
            EventKind::PendingChanged(None)
        ) && event.account_id == Some(8)));
        assert!(recovered
            .iter()
            .any(|event| matches!(&event.kind, EventKind::NowPlaying(now) if !now.active)));
        assert!(recovered
            .iter()
            .any(|event| matches!(&event.kind, EventKind::UpdateAvailable(_))
                && event.account_id.is_none()));
        assert!(recovered
            .iter()
            .any(|event| matches!(&event.kind, EventKind::UpdateFailed(_))
                && event.account_id.is_none()));
        assert!(recovered.iter().all(|event| core.event_is_current(event)));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn lag_recovery_replays_active_account_expiry() {
        let (api, _) = anilist::mock_api(vec![(
            401,
            serde_json::json!({"errors": [{"message": "Invalid Token"}]}),
        )])
        .await;
        let (core, dir) = open("expired-recovery");
        *core.state.anilist.lock() = api;
        core.state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Test".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let error = core
            .update_entry(1, Some("PLANNING".into()), None, None, None)
            .await
            .unwrap_err();
        assert!(error.contains("expired"));
        let recovered = core.recovery_events();
        assert!(recovered
            .iter()
            .any(|event| matches!(&event.kind, EventKind::AuthExpired)
                && event.account_id == Some(7)));
        assert!(recovered.iter().all(|event| core.event_is_current(event)));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn prompt(media_id: i64, episode: i64) -> TrackingPrompt {
        TrackingPrompt {
            media_id,
            episode,
            title: "Synthetic show".into(),
            raw_title: format!("Synthetic show - {episode:02}"),
            progress: episode - 1,
        }
    }

    #[tokio::test]
    async fn retained_prompt_recovery_acknowledgement_and_buffered_delivery() {
        let (core, dir) = open("prompt-ack");
        core.state.db.set_setting("anilist_user_id", "7").unwrap();
        let mut receiver = core.subscribe();
        core.emit(EventKind::TrackingPrompt(prompt(11, 3)));
        let buffered = receiver.recv().await.unwrap();
        assert!(core.event_is_current(&buffered));
        for _ in 0..2 {
            let recovered: Vec<_> = core
                .recovery_events()
                .into_iter()
                .filter(|event| matches!(event.kind, EventKind::TrackingPrompt(_)))
                .collect();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].generation, buffered.generation);
            assert_eq!(recovered[0].account_id, buffered.account_id);
        }
        let wrong_variant = Event {
            kind: EventKind::TrackingAsk(prompt(11, 3)),
            ..buffered.clone()
        };
        assert!(!core.acknowledge_tracking_prompt(wrong_variant).await);
        assert!(core.acknowledge_tracking_prompt(buffered.clone()).await);
        assert!(!core.event_is_current(&buffered));
        assert!(!core.acknowledge_tracking_prompt(buffered.clone()).await);
        assert!(core.recovery_events().iter().all(|event| !matches!(
            event.kind,
            EventKind::TrackingPrompt(_) | EventKind::TrackingAsk(_)
        )));
        core.emit(EventKind::TrackingPrompt(prompt(11, 5)));
        let replaced = receiver.recv().await.unwrap();
        assert!(core.event_is_current(&replaced));
        core.emit(EventKind::TrackingPrompt(prompt(12, 2)));
        let next_track = receiver.recv().await.unwrap();
        assert!(!core.event_is_current(&replaced));
        assert!(!core.acknowledge_tracking_prompt(replaced).await);
        assert!(core.event_is_current(&next_track));
        core.emit(EventKind::TrackingAsk(prompt(11, 4)));
        assert!(core
            .recovery_events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::TrackingAsk(_))));
        core.emit(EventKind::NowPlaying(playback::idle()));
        assert!(core.recovery_events().iter().all(|event| !matches!(
            event.kind,
            EventKind::TrackingPrompt(_) | EventKind::TrackingAsk(_)
        )));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn retained_prompt_rejects_replaced_account_and_advanced_progress() {
        let (core, dir) = open("prompt-origin");
        core.state.db.set_setting("anilist_user_id", "7").unwrap();
        let mut receiver = core.subscribe();
        core.emit(EventKind::TrackingPrompt(prompt(11, 3)));
        let old = receiver.recv().await.unwrap();
        core.state.auth_intent.send_replace(1);
        core.state.db.set_setting("anilist_user_id", "8").unwrap();
        assert!(!core.event_is_current(&old));
        assert!(!core.acknowledge_tracking_prompt(old).await);
        core.emit(EventKind::TrackingPrompt(prompt(11, 3)));
        let current = receiver.recv().await.unwrap();
        assert_eq!(current.generation, 1);
        assert_eq!(current.account_id, Some(8));
        assert!(core.event_is_current(&current));
        core.state
            .db
            .upsert_media(&Media {
                id: 11,
                ..Default::default()
            })
            .unwrap();
        core.state
            .db
            .upsert_entry(&ListEntry {
                media_id: 11,
                status: "CURRENT".into(),
                progress: 3,
                ..Default::default()
            })
            .unwrap();
        assert!(!core.event_is_current(&current));
        core.state
            .db
            .upsert_entry(&ListEntry {
                media_id: 11,
                status: "REPEATING".into(),
                progress: 0,
                ..Default::default()
            })
            .unwrap();
        assert!(!core.event_is_current(&current));
        assert!(!core.acknowledge_tracking_prompt(current).await);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn identical_prompt_reissue_rejects_delayed_acknowledgement() {
        let (core, dir) = open("prompt-reissue");
        core.state.db.set_setting("anilist_user_id", "7").unwrap();
        let mut events = core.subscribe();
        let payload = prompt(11, 3);
        core.emit(EventKind::TrackingPrompt(payload.clone()));
        let first = events.recv().await.unwrap();
        assert!(core.event_is_current(&first));
        core.emit(EventKind::NowPlaying(playback::idle()));
        let _idle = events.recv().await.unwrap();
        core.emit(EventKind::TrackingPrompt(payload));
        let second = events.recv().await.unwrap();
        assert_eq!(first.generation, second.generation);
        assert_eq!(first.account_id, second.account_id);
        assert_ne!(first.prompt_id, second.prompt_id);
        assert!(!core.event_is_current(&first));
        assert!(!core.acknowledge_tracking_prompt(first).await);
        assert!(core.event_is_current(&second));
        assert!(core.acknowledge_tracking_prompt(second).await);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn accepted_progress_retires_prompt_before_a_rewatch() {
        let (mut api, _) = anilist::mock_api(vec![(
            200,
            serde_json::json!({"data":{"SaveMediaListEntry":{
                "mediaId": 11,
                "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                "score":null,"updatedAt":123
            }}}),
        )])
        .await;
        api.record_viewer(7);
        let (core, dir) = open("prompt-progress");
        *core.state.anilist.lock() = api;
        core.state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        core.state
            .db
            .upsert_media(&Media {
                id: 11,
                ..Default::default()
            })
            .unwrap();
        core.state
            .db
            .upsert_entry(&ListEntry {
                id: Some(1011),
                media_id: 11,
                status: "CURRENT".into(),
                progress: 2,
                ..Default::default()
            })
            .unwrap();
        let mut receiver = core.subscribe();
        core.emit(EventKind::TrackingPrompt(prompt(11, 3)));
        let prior = receiver.recv().await.unwrap();
        assert_eq!(
            core.update_entry(11, None, Some(3), None, None)
                .await
                .unwrap()
                .progress,
            3
        );
        core.state
            .db
            .upsert_entry(&ListEntry {
                id: Some(1011),
                media_id: 11,
                status: "REPEATING".into(),
                progress: 0,
                ..Default::default()
            })
            .unwrap();
        assert!(!core.event_is_current(&prior));
        assert!(!core.acknowledge_tracking_prompt(prior).await);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn prompt_during_pending_read(status: u16) {
        use std::time::Duration;

        let body = if status == 200 {
            serde_json::json!({"data":{"SaveMediaListEntry":{
                "mediaId": 11,
                "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                "score":null,"updatedAt":123
            }}})
        } else {
            serde_json::json!({"errors":[{"message":"Synthetic rejection","status":status}]})
        };
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (mut api, mut requests) =
            anilist::mock_api_gated(vec![(status, body)], Some(gate.clone())).await;
        api.record_viewer(7);
        let (core, dir) = open(&format!("prompt-pending-{status}"));
        *core.state.anilist.lock() = api;
        core.state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        core.state
            .db
            .upsert_media(&Media {
                id: 11,
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
        core.state
            .db
            .upsert_entry(&ListEntry {
                id: Some(1011),
                media_id: 11,
                status: "CURRENT".into(),
                progress: 2,
                ..Default::default()
            })
            .unwrap();
        let mut events = core.subscribe();
        core.emit(EventKind::TrackingPrompt(prompt(11, 3)));
        let offered = events.recv().await.unwrap();
        let worker = core.clone();
        let save = tokio::spawn(async move { worker.set_progress(11, 3, Some(2)).await });
        tokio::time::timeout(Duration::from_secs(3), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!save.is_finished());
        assert_eq!(core.local_entries().await.unwrap()[0].progress, 3);
        assert!(core.event_is_current(&offered));
        assert_eq!(
            core.recovery_events()
                .iter()
                .filter(|event| { event.prompt_id == offered.prompt_id })
                .count(),
            1
        );
        gate.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(3), save)
            .await
            .unwrap()
            .unwrap();
        if status == 400 {
            assert!(result.is_err());
            assert_eq!(core.get_entry(11).await.unwrap().unwrap().progress, 2);
            assert!(core.get_pending_changes().await.unwrap().is_empty());
            assert!(core.event_is_current(&offered));
            let (mut api, _) = anilist::mock_api(vec![(
                200,
                serde_json::json!({
                    "data":{"SaveMediaListEntry":{
                        "mediaId": 11,
                        "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                        "score":null,"updatedAt":124
                    }}
                }),
            )])
            .await;
            api.record_viewer(7);
            *core.state.anilist.lock() = api;
            assert_eq!(core.set_progress(11, 3, Some(2)).await.unwrap().progress, 3);
            assert!(core.get_pending_changes().await.unwrap().is_empty());
        } else {
            assert_eq!(result.unwrap().progress, 3);
            assert_eq!(
                core.get_pending_changes().await.unwrap().len(),
                usize::from(status == 503)
            );
        }
        assert!(!core.event_is_current(&offered));
        assert!(!core
            .recovery_events()
            .iter()
            .any(|event| event.prompt_id == offered.prompt_id));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejected_progress_after_pending_read_keeps_prompt_for_retry() {
        prompt_during_pending_read(400).await;
    }

    #[tokio::test]
    async fn successful_progress_after_pending_read_retires_prompt() {
        prompt_during_pending_read(200).await;
    }

    #[tokio::test]
    async fn queued_progress_after_pending_read_retires_prompt() {
        prompt_during_pending_read(503).await;
    }

    #[tokio::test]
    async fn services_cancel_owned_tasks_and_release_start_guard() {
        let (core, dir) = open("cancel");
        let dropped = Arc::new(AtomicUsize::new(0));
        struct Flag(Arc<AtomicUsize>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let marker = dropped.clone();
        let task = core.spawn(async move {
            let _flag = Flag(marker);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        core.services_started.store(true, Ordering::SeqCst);
        Services {
            tasks: vec![task],
            playback: None,
            started: Some(core.services_started.clone()),
        }
        .shutdown()
        .await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(!core.services_started.load(Ordering::SeqCst));
        let running = core.start_services();
        let duplicate = core.start_services();
        assert!(duplicate.tasks.is_empty());
        drop(duplicate);
        running.shutdown().await;
        assert!(!core.services_started.load(Ordering::SeqCst));
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn offline_preference_writes_distinguish_invalid_and_verified_owners() {
        let (core, dir) = open("offline-preference-owner");
        let key = "kurisu.list.filter";
        core.set_workspace_preference(key.into(), Some("PAUSED".into()), None)
            .await
            .unwrap();
        core.state
            .db
            .set_setting("anilist_user_id", "2147483648")
            .unwrap();
        core.set_workspace_preference(key.into(), Some("COMPLETED".into()), Some(0))
            .await
            .unwrap();
        let offline_value = core.get_preference(key.into()).await.unwrap();

        core.state.db.set_setting("anilist_user_id", "71").unwrap();
        core.set_workspace_preference(key.into(), Some("CURRENT".into()), Some(0))
            .await
            .unwrap();
        let stale_value = core.get_preference(key.into()).await.unwrap();
        core.set_workspace_preference(key.into(), Some("CURRENT".into()), None)
            .await
            .unwrap();
        let signed_out_value = core.get_preference(key.into()).await.unwrap();
        assert!(!core.state.anilist.lock().has_token());
        core.set_workspace_preference(key.into(), Some("TOKENLESS".into()), Some(71))
            .await
            .unwrap();
        let tokenless_value = core.get_preference(key.into()).await.unwrap();

        core.state.anilist.lock().set_token(Some("tok-71".into()));
        core.state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_owner_commit BEFORE INSERT ON settings
                 WHEN NEW.key = 'native_preference:kurisu.list.owner'
                 BEGIN SELECT RAISE(ABORT, 'fixture owner commit failed'); END;",
            )
            .unwrap();
        let atomic_err = core
            .set_workspace_preference(key.into(), Some("UNOWNED".into()), Some(71))
            .await
            .unwrap_err();
        assert!(
            atomic_err.contains("fixture owner commit failed"),
            "{atomic_err}"
        );
        let rolled_back_value = core.get_preference(key.into()).await.unwrap();
        core.state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER fail_owner_commit")
            .unwrap();

        core.set_workspace_preference(key.into(), Some("PAUSED".into()), Some(71))
            .await
            .unwrap();
        let verified_value = core.get_preference(key.into()).await.unwrap();
        let recorded_owner = core.state.db.get_setting(db::LIST_OWNER_KEY).unwrap();
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(offline_value.as_deref(), Some("COMPLETED"));
        assert_eq!(stale_value.as_deref(), Some("COMPLETED"));
        assert_eq!(signed_out_value.as_deref(), Some("COMPLETED"));
        assert_eq!(tokenless_value.as_deref(), Some("COMPLETED"));
        assert_eq!(rolled_back_value.as_deref(), Some("COMPLETED"));
        assert_eq!(verified_value.as_deref(), Some("PAUSED"));
        assert_eq!(recorded_owner.as_deref(), Some("71"));
    }

    #[tokio::test]
    async fn preferences_accept_only_known_scoped_keys() {
        let (core, dir) = open("preferences");
        core.set_preference("kurisu.list.sort".into(), Some("title".into()))
            .await
            .unwrap();
        assert_eq!(
            core.get_preference("kurisu.list.sort".into())
                .await
                .unwrap()
                .as_deref(),
            Some("title")
        );
        assert!(core
            .set_preference("anilist_token".into(), Some("secret".into()))
            .await
            .is_err());
        assert!(core.get_preference("kurisu.unknown".into()).await.is_err());
        assert_eq!(core.state.db.get_setting("anilist_token").unwrap(), None);
        core.set_preference("kurisu.list.sort".into(), None)
            .await
            .unwrap();
        assert_eq!(
            core.get_preference("kurisu.list.sort".into())
                .await
                .unwrap(),
            None
        );
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sort_preferences_roll_back_when_either_write_is_ignored() {
        let (core, dir) = open("sort-preferences-ignored-write");
        core.set_list_sort_preferences("title", false).unwrap();
        for key in ["sort", "dir"] {
            core.state
                .db
                .0
                .lock()
                .execute_batch(&format!(
                    "CREATE TRIGGER ignore_sort_write BEFORE INSERT ON settings
                     WHEN NEW.key = 'native_preference:kurisu.list.{key}'
                     BEGIN SELECT RAISE(IGNORE); END;"
                ))
                .unwrap();
            let result = core.set_list_sort_preferences("score", true);
            assert_eq!(
                core.get_list_sort_preferences(),
                [Ok(Some("title".into())), Ok(Some("asc".into()))]
            );
            assert!(result.is_err());
            core.state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER ignore_sort_write")
                .unwrap();
        }
        core.set_list_sort_preferences("score", true).unwrap();
        drop(core);
        let db = Db::open(&dir.join("kurisu.db")).unwrap();
        let [sort, direction] = db.get_list_sort_preferences();
        assert_eq!(sort.unwrap().as_deref(), Some("score"));
        assert_eq!(direction.unwrap().as_deref(), Some("desc"));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn set_preference_rejects_scoped_list_keys_without_modifying_db() {
        let (core, dir) = open("reject-scoped-set-preference");
        core.set_workspace_preference("kurisu.list.q".into(), Some("keep".into()), None)
            .await
            .unwrap();
        for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
            assert!(core
                .set_preference(key.into(), Some("overwrite".into()))
                .await
                .is_err());
            assert!(core.set_preference(key.into(), None).await.is_err());
        }
        assert_eq!(
            core.state
                .db
                .get_setting("native_preference:kurisu.list.q")
                .unwrap()
                .as_deref(),
            Some("keep")
        );
        assert_eq!(
            core.state
                .db
                .get_setting("native_preference:kurisu.list.filter")
                .unwrap(),
            None
        );
        assert_eq!(
            core.state
                .db
                .get_setting("native_preference:kurisu.list.synced")
                .unwrap(),
            None
        );
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sync_my_list_applies_native_preference_ownership_on_adoption_and_account_replacement()
    {
        for channel in [UpdateChannel::Native, UpdateChannel::Legacy] {
            for cold_adoption in [true, false] {
                let viewer_b = serde_json::json!({
                    "data": {
                        "Viewer": {
                            "id": 72,
                            "name": "Bob",
                            "mediaListOptions": { "scoreFormat": "POINT_100" }
                        }
                    }
                });
                let list_b = serde_json::json!({
                    "data": {
                        "MediaListCollection": {
                            "user": { "id": 72 },
                            "hasNextChunk": false,
                            "lists": []
                        }
                    }
                });
                let replies = if cold_adoption {
                    vec![(200, viewer_b.clone()), (200, list_b), (200, viewer_b)]
                } else {
                    vec![(200, list_b), (200, viewer_b)]
                };
                let (api, _) = anilist::mock_api(replies).await;
                let db = Db::open(Path::new(":memory:")).unwrap();
                db.replace_account_with_preferences(
                    "tok-alice",
                    &User {
                        id: 71,
                        name: "Alice".into(),
                        score_format: Some("POINT_100".into()),
                        ..Default::default()
                    },
                    db::ListPreferences::Native { last_account: None },
                )
                .unwrap();
                for (key, value) in [
                    ("native_preference:kurisu.list.q", "alice-search"),
                    ("native_preference:kurisu.list.filter", "DROPPED"),
                    ("native_preference:kurisu.list.synced", "12345"),
                    ("native_preference:kurisu.list.sort", "title"),
                ] {
                    db.set_setting(key, value).unwrap();
                }
                if cold_adoption {
                    db.delete_setting("anilist_user_id").unwrap();
                    db.delete_setting("anilist_profile").unwrap();
                    db.set_setting("anilist_token", "tok-bob").unwrap();
                } else {
                    db.replace_account(
                        "tok-bob",
                        &User {
                            id: 72,
                            name: "Bob".into(),
                            score_format: Some("POINT_100".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                let core = Core::from_db(
                    db,
                    PathBuf::new(),
                    tokio::runtime::Handle::current(),
                    Arc::new(TestPlatform),
                    channel,
                );
                *core.state.anilist.lock() = api;
                core.state.anilist.lock().set_token(Some("tok-bob".into()));

                for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
                    assert_eq!(
                        core.get_preference(key.into()).await.unwrap(),
                        None,
                        "before sync channel={channel:?}, cold_adoption={cold_adoption}, key={key}"
                    );
                }

                assert!(core.sync_my_list().await.unwrap().is_empty());
                let expected = match channel {
                    UpdateChannel::Native => [None, None, None],
                    UpdateChannel::Legacy => [Some("alice-search"), Some("DROPPED"), Some("12345")],
                };
                for (key, want) in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"]
                    .into_iter()
                    .zip(expected)
                {
                    assert_eq!(
                        core.state
                            .db
                            .get_setting(&format!("native_preference:{key}"))
                            .unwrap()
                            .as_deref(),
                        want,
                        "channel={channel:?}, cold_adoption={cold_adoption}, key={key}"
                    );
                    assert_eq!(
                        core.get_preference(key.into()).await.unwrap(),
                        None,
                        "channel={channel:?}, cold_adoption={cold_adoption}, key={key}"
                    );
                }
                assert_eq!(
                    core.get_preference("kurisu.list.sort".into())
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("title")
                );
                assert_eq!(
                    core.saved_account_id().await,
                    Some(match channel {
                        UpdateChannel::Native => 72,
                        UpdateChannel::Legacy => 71,
                    }),
                    "channel={channel:?}, cold_adoption={cold_adoption}"
                );
            }
        }
    }

    #[tokio::test]
    async fn scoped_preference_reads_serialize_with_entry_lock_and_hide_corrupt_saved_account() {
        use std::task::{Context, Waker};

        let (core, dir) = open("scoped-pref-read-guard");
        core.state
            .db
            .replace_account_with_preferences(
                "tok-71",
                &User {
                    id: 71,
                    name: "Alice".into(),
                    ..Default::default()
                },
                db::ListPreferences::Native { last_account: None },
            )
            .unwrap();
        core.state.anilist.lock().set_token(Some("tok-71".into()));
        core.set_workspace_preference("kurisu.list.q".into(), Some("alice".into()), Some(71))
            .await
            .unwrap();

        let transition = core.state.entry_lock.lock().await;
        let mut read = Box::pin(core.get_preference("kurisu.list.q".into()));
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "scoped get_preference must serialize with entry_lock"
        );
        core.state
            .db
            .set_setting("anilist_user_id", "2147483648")
            .unwrap();
        drop(transition);
        assert_eq!(read.await.unwrap(), None);

        core.state.db.delete_setting("anilist_user_id").unwrap();
        assert_eq!(
            core.get_preference("kurisu.list.q".into()).await.unwrap(),
            None
        );

        core.state.anilist.lock().set_token(None);
        assert_eq!(
            core.get_preference("kurisu.list.q".into())
                .await
                .unwrap()
                .as_deref(),
            Some("alice")
        );
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn remote_settlement_tracks_lock_and_generation_changes() {
        use std::task::{Context, Waker};
        use std::time::Duration;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let core = Core::from_db(
            db,
            PathBuf::new(),
            tokio::runtime::Handle::current(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        );
        let initial = core.generation();
        assert!(!core.remote_write_active());
        assert!(core.wait_for_remote_settlement(initial).await);
        assert!(!core.remote_write_active());

        let remote = core.state.remote_lock.clone().lock_owned().await;
        assert!(core.remote_write_active());
        assert!(!core.wait_for_remote_settlement(initial + 1).await);

        let mut waiting = Box::pin(core.wait_for_remote_settlement(initial));
        assert!(waiting
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(remote);
        assert!(tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap());
        assert!(!core.remote_write_active());

        let held = core.state.remote_lock.clone().lock_owned().await;
        assert!(core.remote_write_active());
        let waiter_core = core.clone();
        let waiter =
            tokio::spawn(async move { waiter_core.wait_for_remote_settlement(initial).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while core.state.auth_intent.receiver_count() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let next = core.begin_login_intent();
        assert_ne!(next, initial);
        assert!(!tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap());
        assert!(core.remote_write_active());

        let mut cancelled = Box::pin(core.wait_for_remote_settlement(next));
        assert!(cancelled
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(cancelled);
        drop(held);
        assert!(!core.remote_write_active());
        assert!(core.wait_for_remote_settlement(next).await);

        let raced = core.state.remote_lock.clone().lock_owned().await;
        let mut simultaneous = Box::pin(core.wait_for_remote_settlement(next));
        assert!(simultaneous
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        drop(raced);
        let final_gen = core.begin_login_intent();
        assert_ne!(final_gen, next);
        assert!(!tokio::time::timeout(Duration::from_secs(2), simultaneous)
            .await
            .unwrap());
        assert!(!core.remote_write_active());
        assert!(core.wait_for_remote_settlement(final_gen).await);
    }

    #[tokio::test]
    async fn wait_for_remote_settlement_observes_superseded_background_flush_without_events() {
        use serde_json::json;
        use std::task::{Context, Waker};
        use std::time::Duration;

        let db = Db::open(Path::new(":memory:")).unwrap();
        let user = User {
            id: 7,
            name: "Fixture".into(),
            score_format: Some("POINT_100".into()),
            ..Default::default()
        };
        db.replace_account("fixture-token", &user).unwrap();
        let core = logout_fixture(db);
        core.state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 3,
                score: Some(0.0),
                repeat: 0,
                updated_at: Some(100),
                media: None,
                queued_creation: None,
            })
            .unwrap();
        *core.state.user.lock() = Some(user);

        let (_, pending, _) = sync_queue::stage(
            core.state(),
            1,
            sync_queue::Patch {
                progress: Some(8),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        core.state.db.put_pending(7, &pending).unwrap();

        let preflight = json!({
            "data": {
                "Media": {
                    "id": 1,
                    "episodes": 12,
                    "mediaListEntry": {
                        "id": 22,
                        "mediaId": 1,
                        "status": "CURRENT",
                        "progress": 3,
                        "score": 0.0,
                        "repeat": 0,
                        "updatedAt": 100
                    }
                }
            }
        });
        let saved = json!({
            "data": {
                "SaveMediaListEntry": {
                    "id": 22,
                    "mediaId": 1,
                    "status": "CURRENT",
                    "progress": 8,
                    "score": 0.0,
                    "repeat": 0,
                    "updatedAt": 200
                }
            }
        });
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (mut api, mut requests) =
            anilist::mock_api_gated(vec![(200, preflight), (200, saved)], Some(gate.clone())).await;
        api.set_token(Some("fixture-token".into()));
        api.record_viewer(7);
        *core.state.anilist.lock() = api;

        let flush_core = core.clone();
        let flush =
            tokio::spawn(async move { sync_queue::flush_background(flush_core.state()).await });

        let first = tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(first["query"].as_str().unwrap().contains("mediaListEntry"));
        gate.add_permits(1);

        let second = tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(second["query"]
            .as_str()
            .unwrap()
            .contains("SaveMediaListEntry"));

        let mut events = core.subscribe();
        core.cancel_login();
        let restored_gen = core.generation();
        assert!(core.remote_write_active());
        assert_eq!(core.current_user().await.unwrap().unwrap().id, 7);
        assert_eq!(
            core.get_pending_changes_for(restored_gen, 7)
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        let pre_settlement = core.local_entries().await.unwrap();
        assert_eq!(pre_settlement[0].updated_at, Some(100));

        let mut settlement = Box::pin(core.wait_for_remote_settlement(restored_gen));
        assert!(settlement
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());

        gate.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(2), settlement)
            .await
            .unwrap());
        assert!(!core.remote_write_active());
        assert!(core
            .get_pending_changes_for(restored_gen, 7)
            .await
            .unwrap()
            .unwrap()
            .is_empty());
        let entries = core.local_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, Some(22));
        assert_eq!(entries[0].progress, 8);
        assert_eq!(entries[0].updated_at, Some(200));

        assert!(tokio::time::timeout(Duration::from_secs(2), flush)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn update_failed_clear_and_take_consume_recovery_and_marker_only_when_present() {
        struct TempDir(PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        const CHILD: &str = "KURISU_TEST_UPDATE_FAILED_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let exe = std::env::current_exe().unwrap();
            let dir = TempDir(std::env::temp_dir().join(format!(
                "kurisu-update-failed-child-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
            std::fs::create_dir(&dir.0).unwrap();
            let child = dir.0.join(exe.file_name().unwrap());
            std::fs::copy(&exe, &child).unwrap();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new(&child)
                    .args([
                        "--exact",
                        "core_tests::update_failed_clear_and_take_consume_recovery_and_marker_only_when_present",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .output()
            })
            .await
            .unwrap();
            drop(dir);
            let output = output.unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        updater::init_install_path().unwrap();
        let install_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let marker = install_dir.join(updater::FAILED_MARKER);
        std::fs::write(&marker, b"failed").unwrap();
        assert!(updater::sweep_install_dir());

        let _data_cleanup;
        let (core, data_dir) = open("update-failed-lifecycle");
        _data_cleanup = TempDir(data_dir);
        assert!(core.pending_update_failed().is_none());
        core.clear_update_failed();
        assert!(core.take_update_failed().await.is_none());
        assert!(marker.exists());

        *core.failed_update.lock() = Some(updater::FAILED_MESSAGE.to_string());
        assert_eq!(
            core.pending_update_failed().as_deref(),
            Some(updater::FAILED_MESSAGE)
        );
        assert!(core.recovery_events().iter().any(|event| matches!(
            &event.kind,
            EventKind::UpdateFailed(message) if message == updater::FAILED_MESSAGE
        )));

        core.clear_update_failed();
        assert!(core.pending_update_failed().is_none());
        assert!(!core
            .recovery_events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::UpdateFailed(_))));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while marker.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "clear_update_failed did not remove the failure marker"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        std::fs::write(&marker, b"failed-again").unwrap();
        assert!(updater::sweep_install_dir());
        *core.failed_update.lock() = Some(updater::FAILED_MESSAGE.to_string());
        assert_eq!(
            core.take_update_failed().await.as_deref(),
            Some(updater::FAILED_MESSAGE)
        );
        assert!(core.pending_update_failed().is_none());
        assert!(!core
            .recovery_events()
            .iter()
            .any(|event| matches!(event.kind, EventKind::UpdateFailed(_))));
        assert!(!marker.exists());
    }
}
