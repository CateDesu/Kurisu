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

pub use commands::{check_bridge_update, install_bridge_update, TrackingConfig};
pub use db::Db;
pub use library::LibraryBinding;
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
        if let Ok(Some(token)) = db.get_setting("anilist_token") {
            if !token.is_empty() {
                anilist.set_token(Some(token));
            }
        }
        let matchers = recognize::build_matchers(&db);
        let (events, _) = broadcast::channel(128);
        Self {
            state: Arc::new(commands::AppState {
                anilist: Mutex::new(anilist),
                db: Arc::new(db),
                user: Mutex::new(None),
                auth_intent: tokio::sync::watch::channel(0).0,
                entry_lock: tokio::sync::Mutex::new(()),
                remote_lock: tokio::sync::Mutex::new(()),
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
        let key = preference_key(&key)?;
        self.state.db.get_setting(&key).map_err(|e| e.to_string())
    }
    pub async fn set_preference(&self, key: String, value: Option<String>) -> Result<(), String> {
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
        commands::login_with_token(token, &self.state).await
    }
    pub async fn login_oauth(&self) -> Result<User, String> {
        commands::login_oauth(self.platform.as_ref(), &self.state).await
    }
    pub fn cancel_login(&self) {
        self.state.cancel_login();
    }
    pub async fn logout(&self) -> Result<(), String> {
        commands::logout(&self.state).await
    }
    pub async fn current_user(&self) -> Result<Option<User>, String> {
        commands::current_user(&self.state).await
    }
    pub async fn saved_account_id(&self) -> Option<i64> {
        sync_queue::account_id(&self.state.db)
    }
    pub async fn refresh_current_user(&self) -> Result<Option<User>, String> {
        commands::refresh_current_user(&self.state).await
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
        commands::sync_my_list(&self.state).await
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
    ) -> Result<(), String> {
        commands::bind_library_path(path, media_id, episode_offset, &self.state).await
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
        sync_queue::get_pending_changes(&self.state)
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
    pub async fn take_update_failed(&self) -> Option<String> {
        {
            let failed = self.failed_update.lock().take();
            if failed.is_some() {
                let _ = updater::take_update_failed();
            }
            failed
        }
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
        let (api, _) = anilist::mock_api(vec![(
            200,
            serde_json::json!({"data":{"SaveMediaListEntry":{
                "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                "score":null,"updatedAt":123
            }}}),
        )])
        .await;
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
                "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                "score":null,"updatedAt":123
            }}})
        } else {
            serde_json::json!({"errors":[{"message":"Synthetic rejection","status":status}]})
        };
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (api, mut requests) =
            anilist::mock_api_gated(vec![(status, body)], Some(gate.clone())).await;
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
            let (api, _) = anilist::mock_api(vec![(
                200,
                serde_json::json!({
                    "data":{"SaveMediaListEntry":{
                        "id":1011,"status":"CURRENT","progress":3,"repeat":0,
                        "score":null,"updatedAt":124
                    }}
                }),
            )])
            .await;
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
}
