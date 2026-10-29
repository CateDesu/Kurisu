use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use mpris::{PlaybackStatus, PlayerFinder};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, AppState, TrackingConfig};
use crate::discord;
use crate::library;
use crate::mpvipc;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(unused_imports))]
use crate::recognize::basename;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(unused_imports))]
use crate::recognize::match_title;

const TICK: Duration = Duration::from_secs(5);
const AUTO_ASK_DELAY: Duration = Duration::from_secs(15);
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
const BROWSER_PLAYERS: &[&str] = &[
    "firefox",
    "librewolf",
    "mozilla",
    "zen",
    "waterfox",
    "floorp",
    "chrome",
    "chromium",
    "brave",
    "vivaldi",
    "opera",
    "edge",
    "browser",
    "kdeconnect",
    "playerctld",
    // Firefox registers opaque Windows IDs that a name filter cannot recognize.
    "308046b0af4a39cb", // Firefox default install
    "e7cf176e110c211b", // Firefox alternate install
    "spotify",
    // Music titles can resemble episodes. Windows exposes no file URL to check audio extensions.
    "amberol",
    "lollypop",
    "rhythmbox",
    "elisa",
    "audacious",
    "foobar2000",
    "musicbee",
    "clementine",
    "strawberry",
    "quodlibet",
    "deadbeef",
    "sayonara",
    "gmusicbrowser",
    "cmus",
    "mpd",
];

/// Known video players override substring exclusions in Windows IDs.
#[cfg_attr(not(windows), allow(dead_code))]
const KNOWN_VIDEO_PLAYERS: &[&str] = &[
    "mpv",
    "vlc",
    "mpc-hc",
    "mpc-be",
    "potplayer",
    "celluloid",
    "haruna",
    "smplayer",
];

const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "m4b", "mka", "aac", "ogg", "oga", "opus", "wav", "wma", "aif", "aiff",
    "aifc", "ape", "wv", "wvp", "tta",
];

fn is_audio_url(url: &str) -> bool {
    let parsed = reqwest::Url::parse(url).ok();
    let path = parsed
        .as_ref()
        .filter(|u| matches!(u.scheme(), "http" | "https" | "file"))
        .map_or(url, |u| u.path());
    let decoded = crate::recognize::percent_decode(path);
    let name = decoded.rsplit(['/', '\\']).next().unwrap_or_default();
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    AUDIO_EXTS.contains(&ext.to_lowercase().as_str())
}

#[derive(Serialize, Clone)]
struct NowPlaying {
    active: bool,
    player: String,
    title: String,
    matched: Option<String>,
    media_id: Option<i64>,
    episode: Option<i64>,
    length_us: i64,
    position_us: i64,
}

#[derive(Serialize, Clone)]
struct TrackingPrompt {
    media_id: i64,
    episode: i64,
    title: String,
    raw_title: String,
    progress: i64,
}

struct ActiveTrack {
    key: TrackKey,
    accumulated: Duration,
    last_tick: Instant,
    was_playing: bool,
    prompted: bool,
    incremented: bool,
    asked: bool,
    fail_count: u32,
    retry_at: Option<Instant>,
    history_recorded: bool,
    push: Option<tokio::task::JoinHandle<Result<bool, String>>>,
}

impl ActiveTrack {
    fn new(key: TrackKey) -> Self {
        Self {
            key,
            accumulated: Duration::ZERO,
            last_tick: Instant::now(),
            was_playing: false,
            prompted: false,
            incremented: false,
            asked: false,
            fail_count: 0,
            retry_at: None,
            history_recorded: false,
            push: None,
        }
    }
}

#[derive(PartialEq, Eq)]
struct TrackKey {
    source: String,
    media_id: Option<i64>,
    episode: Option<i64>,
    session: Option<String>,
}

const MAX_AUTO_PUSH_FAILURES: u32 = 4;

fn auto_push_backoff(fail_count: u32) -> Duration {
    match fail_count {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        _ => Duration::from_secs(600),
    }
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
struct AutoGate {
    incremented: bool,
    was_playing_before: bool,
    accumulated: Duration,
    fail_count: u32,
    retry_due: bool,
    length_us: i64,
    position_us: i64,
    auto_percent: u64,
    episode: i64,
    progress: i64,
}

impl AutoGate {
    fn pct(&self) -> f64 {
        if self.length_us > 0 {
            (self.position_us as f64 / self.length_us as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Resuming or seeking past the threshold is not evidence of watching.
    fn should_push(&self) -> bool {
        !self.incremented
            && self.watched_enough()
            && self.retry_due
            && self.fail_count < MAX_AUTO_PUSH_FAILURES
            && self.episode > self.progress
    }

    fn watched_enough(&self) -> bool {
        self.was_playing_before
            && self.accumulated >= min_watch_time(self.length_us)
            && self.pct() >= self.auto_percent as f64
    }
}

fn min_watch_time(length_us: i64) -> Duration {
    if length_us <= 0 {
        return Duration::from_secs(60);
    }
    let quarter = Duration::from_micros((length_us / 4) as u64);
    quarter.min(Duration::from_secs(60))
}

/// Isolate tick panics. Requires the Tauri runtime.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut active: Option<ActiveTrack> = None;
        let mut recognition = RecognitionCache::default();
        loop {
            tokio::time::sleep(TICK).await;
            let app_tick = app.clone();
            let prev = active.take();
            let mut cache = std::mem::take(&mut recognition);
            let joined = tauri::async_runtime::spawn(async move {
                let mut track = prev;
                let result = tick(&app_tick, &mut track, &mut cache).await;
                (track, cache, result)
            })
            .await;
            match joined {
                Ok((track, cache, result)) => {
                    active = track;
                    recognition = cache;
                    if let Err(e) = result {
                        log::debug!("playback tick error: {e}");
                    }
                }
                Err(e) => {
                    active = None;
                    log::warn!("playback tick panicked (watcher continues): {e}");
                }
            }
        }
    });
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
#[derive(Clone)]
struct TickInfo {
    playing: bool,
    player: String,
    trackid: String,
    title: String,
    length_us: i64,
    position_us: i64,
    media_id: Option<i64>,
    matched_title: Option<String>,
    episode: Option<i64>,
}

impl TickInfo {
    fn key(&self, session: Option<String>) -> TrackKey {
        TrackKey {
            source: if self.trackid.is_empty() {
                self.title.clone()
            } else {
                video_path(&self.trackid)
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| self.trackid.clone())
            },
            media_id: self.media_id,
            episode: self.episode,
            session,
        }
    }
}

async fn tick(
    app: &AppHandle,
    active: &mut Option<ActiveTrack>,
    recognition: &mut RecognitionCache,
) -> anyhow::Result<()> {
    let state = app.state::<AppState>();
    let expected_token = state.anilist.lock().token();
    let app_for_blocking = app.clone();
    let mut cache = std::mem::take(recognition);
    let (cache, info) = tokio::task::spawn_blocking(move || {
        let info = read_now(&app_for_blocking, &mut cache);
        (cache, info)
    })
    .await?;
    *recognition = cache;
    let info = info?;
    let _session_guard = state.entry_lock.lock().await;
    if state.anilist.lock().token() != expected_token {
        *active = None;
        let _ = app.emit("kurisu://now-playing", idle());
        discord::update(None);
        return Ok(());
    }

    let Some(info) = info else {
        if active.is_some() {
            let _ = app.emit("kurisu://now-playing", idle());
            *active = None;
            discord::update(None);
        }
        return Ok(());
    };

    let _ = app.emit(
        "kurisu://now-playing",
        NowPlaying {
            active: true,
            player: info.player.clone(),
            title: info.title.clone(),
            matched: info.matched_title.clone(),
            media_id: info.media_id,
            episode: info.episode,
            length_us: info.length_us,
            position_us: info.position_us,
        },
    );

    let key = info.key(expected_token.clone());
    if active.as_ref().map(|t| &t.key) != Some(&key) {
        *active = Some(ActiveTrack::new(key));
    }
    let Some(track) = active.as_mut() else {
        return Ok(());
    };
    // Credit only intervals playing at both ends. Pauses between samples are invisible.
    let was_playing_before = track.was_playing;
    if info.playing && track.was_playing {
        track.accumulated += track.last_tick.elapsed().min(TICK * 2);
    }
    track.was_playing = info.playing;
    track.last_tick = Instant::now();

    let cfg = read_config(app);
    let entry = info
        .media_id
        .filter(|_| cfg.discord_enabled || info.episode.is_some())
        .and_then(|media_id| state.db.get_entry(media_id).ok().flatten());

    let desired = if cfg.discord_enabled {
        match (info.matched_title.as_ref(), info.media_id) {
            (Some(title), Some(_)) => {
                let (cover_url, total_episodes) = entry
                    .as_ref()
                    .and_then(|e| e.media.as_ref())
                    .map(|m| {
                        (
                            m.cover_large.clone().or_else(|| m.cover_medium.clone()),
                            m.episodes,
                        )
                    })
                    .unwrap_or_default();
                Some(discord::PresenceInfo {
                    title: title.clone(),
                    episode: info.episode,
                    playing: info.playing,
                    length_us: info.length_us,
                    position_us: info.position_us,
                    cover_url,
                    total_episodes,
                })
            }
            _ => None,
        }
    } else {
        None
    };
    discord::update(desired);

    let Some(media_id) = info.media_id else {
        return Ok(());
    };
    let Some(episode) = info.episode else {
        return Ok(());
    };
    let progress = entry.as_ref().map_or(0, |e| e.progress);

    // Auto ask is independent of mode and consumes the prompt to avoid asking twice.
    if cfg.auto_ask
        && info.playing
        && episode > progress
        && !track.asked
        && track.accumulated >= AUTO_ASK_DELAY
    {
        track.asked = true;
        track.prompted = true;
        let _ = app.emit(
            "kurisu://tracking-ask",
            TrackingPrompt {
                media_id,
                episode,
                title: info
                    .matched_title
                    .clone()
                    .unwrap_or_else(|| info.title.clone()),
                raw_title: info.title.clone(),
                progress,
            },
        );
    }

    if track.push.as_ref().is_some_and(|push| push.is_finished()) {
        let result = track
            .push
            .take()
            .unwrap()
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
        match result {
            Ok(history_recorded) => {
                track.incremented = true;
                track.history_recorded |= history_recorded;
                if state
                    .db
                    .get_entry(media_id)
                    .ok()
                    .flatten()
                    .is_some_and(|entry| entry.progress >= episode)
                {
                    record_history(app, state.inner(), track, &info, expected_token.as_deref());
                }
            }
            Err(e) => {
                track.fail_count += 1;
                track.retry_at = Some(Instant::now() + auto_push_backoff(track.fail_count));
                if track.fail_count >= MAX_AUTO_PUSH_FAILURES {
                    log::warn!(
                        "auto progress-update of {media_id} failed {} times, giving up on this track: {e}",
                        track.fail_count
                    );
                } else {
                    log::warn!(
                        "auto progress-update of {media_id} failed (attempt {}), retrying later: {e}",
                        track.fail_count
                    );
                }
            }
        }
    }

    let gate = AutoGate {
        incremented: track.incremented,
        was_playing_before,
        accumulated: track.accumulated,
        fail_count: track.fail_count,
        retry_due: track.retry_at.map(|t| Instant::now() >= t).unwrap_or(true),
        length_us: info.length_us,
        position_us: info.position_us,
        auto_percent: cfg.auto_percent,
        episode,
        progress,
    };
    if info.playing && progress >= episode && gate.watched_enough() {
        record_history(app, state.inner(), track, &info, expected_token.as_deref());
    }
    match cfg.mode.as_str() {
        "prompt" if info.playing => {
            if !track.prompted && track.accumulated >= Duration::from_secs(cfg.prompt_seconds) {
                track.prompted = true;
                let _ = app.emit(
                    "kurisu://tracking-prompt",
                    TrackingPrompt {
                        media_id,
                        episode,
                        title: info
                            .matched_title
                            .clone()
                            .unwrap_or_else(|| info.title.clone()),
                        raw_title: info.title.clone(),
                        progress,
                    },
                );
            }
        }
        "auto" if info.playing && gate.should_push() && track.push.is_none() => {
            let handle = app.clone();
            let watched = info.clone();
            let history_recorded = track.history_recorded;
            track.push = Some(tokio::spawn(async move {
                let state = handle.state::<AppState>();
                let result = commands::watcher_set_progress(
                    state.inner(),
                    media_id,
                    episode,
                    expected_token.as_deref(),
                )
                .await?;
                let _guard = state.entry_lock.lock().await;
                if state.anilist.lock().token() != expected_token {
                    return Ok(false);
                }
                if let Some(entry) = result {
                    let _ = handle.emit("kurisu://episode-updated", entry);
                }
                let accepted = state
                    .db
                    .get_entry(media_id)
                    .ok()
                    .flatten()
                    .is_some_and(|entry| entry.progress >= episode);
                Ok(history_recorded
                    || (accepted
                        && save_history(
                            &handle,
                            state.inner(),
                            &watched,
                            expected_token.as_deref(),
                        )))
            }));
        }
        _ => {}
    }

    Ok(())
}

fn video_path(source: &str) -> Option<std::path::PathBuf> {
    let path = if std::path::Path::new(source).is_absolute() {
        std::path::PathBuf::from(source)
    } else if let Ok(url) = reqwest::Url::parse(source) {
        if url.scheme() != "file" {
            return None;
        }
        url.to_file_path().ok()?
    } else {
        std::path::PathBuf::from(source)
    };
    if !path.is_absolute()
        || !path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| {
                crate::recognize::VIDEO_EXTS
                    .iter()
                    .any(|video| ext.eq_ignore_ascii_case(video))
            })
    {
        return None;
    }
    Some(path)
}

fn local_video_path(source: &str) -> Option<String> {
    let path = video_path(source)?;
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    path.to_str().map(str::to_string)
}

fn record_history(
    app: &AppHandle,
    state: &AppState,
    track: &mut ActiveTrack,
    info: &TickInfo,
    expected_token: Option<&str>,
) {
    if !track.history_recorded {
        track.history_recorded = save_history(app, state, info, expected_token);
    }
}

fn save_history(
    app: &AppHandle,
    state: &AppState,
    info: &TickInfo,
    expected_token: Option<&str>,
) -> bool {
    if expected_token.is_none() || state.anilist.lock().token().as_deref() != expected_token {
        return false;
    }
    let (Some(media_id), Some(episode), Some(path)) =
        (info.media_id, info.episode, local_video_path(&info.trackid))
    else {
        return false;
    };
    let Some(account_id) = state
        .db
        .get_setting("anilist_user_id")
        .ok()
        .flatten()
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0)
    else {
        return false;
    };
    match state.db.record_watch(account_id, &path, media_id, episode) {
        Ok(()) => {
            let _ = app.emit("kurisu://watch-history-updated", media_id);
            true
        }
        Err(error) => {
            log::warn!("could not save local watch history: {error}");
            false
        }
    }
}

fn idle() -> NowPlaying {
    NowPlaying {
        active: false,
        player: String::new(),
        title: String::new(),
        matched: None,
        media_id: None,
        episode: None,
        length_us: 0,
        position_us: 0,
    }
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn probe_mpv(app: &AppHandle, matches: &PlaybackMatches<'_>) -> Option<TickInfo> {
    let paths = mpv_socket_paths(&app.state::<AppState>().db);
    preferred_source(
        mpvipc::probe(&paths)
            .iter()
            .map(|s| mpv_snapshot_info(matches, s)),
    )
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn mpv_socket_paths(db: &crate::db::Db) -> Vec<String> {
    let socket = match db.get_setting("tracking_mpv_socket") {
        Ok(socket) => socket.unwrap_or_default(),
        Err(error) => {
            log::warn!("MPV socket read failed, using defaults: {error}");
            String::new()
        }
    };
    let mut paths: Vec<String> = Vec::new();
    let custom = socket.trim().to_string();
    if !custom.is_empty() {
        paths.push(custom);
    }
    for p in mpvipc::default_socket_paths() {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn mpv_snapshot_info(matches: &PlaybackMatches<'_>, snap: &mpvipc::MpvSnapshot) -> TickInfo {
    let title = if snap.media_title.is_empty() {
        snap.filename.clone()
    } else {
        snap.media_title.clone()
    };
    let matched = if is_audio_url(&snap.filename) {
        None
    } else {
        matches.match_playing(&title, &snap.path)
    };
    let base = basename(&snap.path);
    let episode = matched.and_then(|(m, binding)| playing_episode(m, &title, &base, binding));
    TickInfo {
        playing: snap.playing,
        player: "mpv".into(),
        trackid: snap.path.clone(),
        title,
        length_us: snap.duration_us,
        position_us: snap.position_us,
        media_id: matched.map(|(m, _)| m.media_id),
        matched_title: matched.map(|(m, _)| m.display.clone()),
        episode,
    }
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
#[derive(Default)]
struct RecognitionCache {
    // Weak identity prevents address reuse without retaining old matchers.
    matchers: std::sync::Weak<Vec<crate::recognize::Matcher>>,
    account: Option<i64>,
    entries: std::collections::VecDeque<RecognizedTitle>,
    #[cfg(test)]
    recognitions: usize,
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
struct RecognizedTitle {
    title: String,
    path: String,
    index: Option<usize>,
    created: Instant,
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
impl RecognitionCache {
    const CAPACITY: usize = 16;
    const LIFETIME: Duration = Duration::from_secs(60);

    fn prepare(
        &mut self,
        matchers: &std::sync::Arc<Vec<crate::recognize::Matcher>>,
        account: Option<i64>,
    ) {
        let identity = std::sync::Arc::downgrade(matchers);
        if !self.matchers.ptr_eq(&identity) || self.account != account {
            self.entries.clear();
            self.matchers = identity;
            self.account = account;
        }
    }

    fn match_unbound<'a>(
        &mut self,
        matchers: &'a [crate::recognize::Matcher],
        title: &str,
        path: &str,
    ) -> Option<&'a crate::recognize::Matcher> {
        let now = Instant::now();
        self.entries
            .retain(|entry| now.duration_since(entry.created) < Self::LIFETIME);
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.title == title && entry.path == path)
        {
            return entry.index.and_then(|index| matchers.get(index));
        }
        #[cfg(test)]
        {
            self.recognitions += 1;
        }
        let matched = match_title(matchers, title, path);
        let index = matched.and_then(|matched| {
            matchers
                .iter()
                .position(|candidate| std::ptr::eq(candidate, matched))
        });
        if self.entries.len() == Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(RecognizedTitle {
            title: title.into(),
            path: path.into(),
            index,
            created: now,
        });
        matched
    }
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
struct PlaybackMatches<'a> {
    db: &'a crate::db::Db,
    matchers: &'a [crate::recognize::Matcher],
    bindings: std::cell::OnceCell<Option<library::BindingPaths>>,
    recognition: std::cell::RefCell<&'a mut RecognitionCache>,
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
impl<'a> PlaybackMatches<'a> {
    // Recreate each poll so edits and filesystem changes become visible.
    fn new(
        db: &'a crate::db::Db,
        matchers: &'a std::sync::Arc<Vec<crate::recognize::Matcher>>,
        recognition: &'a mut RecognitionCache,
    ) -> Self {
        recognition.prepare(matchers, crate::sync_queue::account_id(db));
        Self {
            db,
            matchers,
            bindings: std::cell::OnceCell::new(),
            recognition: std::cell::RefCell::new(recognition),
        }
    }

    fn match_unbound(&self, title: &str, path: &str) -> Option<&'a crate::recognize::Matcher> {
        self.recognition
            .borrow_mut()
            .match_unbound(self.matchers, title, path)
    }

    fn match_playing(
        &self,
        title: &str,
        path: &str,
    ) -> Option<(
        &'a crate::recognize::Matcher,
        Option<library::LibraryBinding>,
    )> {
        if is_audio_url(path) {
            return None;
        }
        let bindings = self
            .bindings
            .get_or_init(|| match library::get_bindings(self.db) {
                Ok(bindings) => Some(library::BindingPaths::new(&bindings)),
                Err(error) => {
                    log::warn!("cannot read library bindings, skipping playback match: {error}");
                    None
                }
            })
            .as_ref()?;
        bindings
            .bound_match(self.matchers, path)
            .map(|(m, binding)| (m, Some(binding)))
            .or_else(|| self.match_unbound(title, path).map(|m| (m, None)))
    }
}

fn playing_episode(
    matched: &crate::recognize::Matcher,
    title: &str,
    filename: &str,
    binding: Option<library::LibraryBinding>,
) -> Option<i64> {
    let candidates = if binding.is_some() {
        [filename, title]
    } else {
        [title, filename]
    };
    library::resolve_bound_episode(matched, &candidates, binding)
}

fn preferred_source(sources: impl IntoIterator<Item = TickInfo>) -> Option<TickInfo> {
    let rank = |info: &TickInfo| {
        (
            info.playing,
            info.media_id.is_some(),
            info.episode.is_some(),
            info.length_us > 0 && info.position_us >= 0,
            video_path(&info.trackid).is_some(),
        )
    };
    sources.into_iter().reduce(|best, next| {
        if rank(&next) > rank(&best) {
            next
        } else {
            best
        }
    })
}

#[cfg(target_os = "linux")]
fn mpris_players(finder: &PlayerFinder) -> Vec<mpris::Player> {
    finder
        .iter_players()
        .map(|players| players.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn mpris_timeline(player: &mpris::Player, metadata: &mpris::Metadata) -> (i64, i64) {
    let microseconds = |duration: Duration| i64::try_from(duration.as_micros()).ok();
    let position = player.get_position().ok().and_then(microseconds);
    (
        position
            .and(metadata.length())
            .and_then(microseconds)
            .unwrap_or(0),
        position.unwrap_or(0),
    )
}

#[cfg(target_os = "linux")]
fn read_now(app: &AppHandle, cache: &mut RecognitionCache) -> anyhow::Result<Option<TickInfo>> {
    let state = app.state::<AppState>();
    let matchers = state.matchers.lock().clone();
    let matches = PlaybackMatches::new(&state.db, &matchers, cache);
    let mut sources = Vec::new();
    if let Ok(finder) = PlayerFinder::new() {
        for player in mpris_players(&finder)
            .into_iter()
            .filter(|p| !is_browser(p))
        {
            let playing = match player.get_playback_status() {
                Ok(PlaybackStatus::Playing) => true,
                Ok(PlaybackStatus::Paused) => false,
                _ => continue,
            };
            let md = player.get_metadata().unwrap_or_default();
            let title = md.title().map(|t| t.to_string()).unwrap_or_default();
            let url = md.url().map(|u| u.to_string()).unwrap_or_default();
            let matched = matches.match_playing(&title, &url);
            let base = basename(&url);
            let episode =
                matched.and_then(|(m, binding)| playing_episode(m, &title, &base, binding));
            let (length_us, position_us) = mpris_timeline(&player, &md);
            sources.push(TickInfo {
                playing,
                player: player.identity().to_string(),
                trackid: if url.is_empty() { title.clone() } else { url },
                title,
                length_us,
                position_us,
                media_id: matched.map(|(m, _)| m.media_id),
                matched_title: matched.map(|(m, _)| m.display.clone()),
                episode,
            });
        }
    }
    sources.extend(probe_mpv(app, &matches));
    Ok(preferred_source(sources))
}

fn read_config(app: &AppHandle) -> TrackingConfig {
    TrackingConfig::load(&app.state::<AppState>().db)
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn is_browser_str(id: &str) -> bool {
    let id = id.to_lowercase();
    if KNOWN_VIDEO_PLAYERS.iter().any(|p| id.contains(p)) {
        return false;
    }
    BROWSER_PLAYERS.iter().any(|b| id.contains(b))
}

#[cfg(target_os = "linux")]
fn is_browser(player: &mpris::Player) -> bool {
    is_browser_str(&format!("{} {}", player.bus_name(), player.identity()))
}

#[cfg(any(windows, test))]
fn windows_timeline(start: Option<i64>, end: Option<i64>, position: Option<i64>) -> (i64, i64) {
    let Some(start) = start else {
        return (0, 0);
    };
    let relative = |timestamp: i64| {
        timestamp
            .checked_sub(start)
            .filter(|value| *value >= 0)
            .map(|value| value / 10)
    };
    let position = position.and_then(relative);
    (
        position.and(end).and_then(relative).unwrap_or(0),
        position.unwrap_or(0),
    )
}

#[cfg(any(windows, test))]
fn is_windows_game_client(id: &str) -> bool {
    let id = id.to_lowercase();
    let app = id.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    const CLIENTS: &[&str] = &[
        "vrchat",
        "vrchatinc.vrchat",
        "com.vrchat",
        "riotclient",
        "riotclientservices",
        "riotclientux",
        "riotclientuxrender",
        "leagueclient",
        "leagueclientux",
        "leagueclientuxrender",
        "com.riotgames",
        "riotgames",
        "steam",
        "steamwebhelper",
        "steamvr",
        "com.valvesoftware.steam",
        "valve.steam",
        "epicgameslauncher",
        "epicwebhelper",
        "epic games launcher",
        "com.epicgames",
        "battle.net",
        "blizzard battle.net",
        "blizzard.battlenet",
        "blizzardentertainment.battle.net",
        "com.blizzard",
        "eadesktop",
        "ealauncher",
        "eabackgroundservice",
        "ea desktop",
        "ea app",
        "origin",
        "originwebhelperservice",
        "com.ea",
        "ubisoftconnect",
        "ubisoftgamelauncher",
        "ubisoft connect",
        "uplay",
        "uplaywebcore",
        "com.ubisoft",
        "galaxyclient",
        "gog galaxy",
        "gog.galaxy",
        "com.gog.galaxy",
        "microsoft.gamingapp",
        "microsoft.xboxapp",
        "microsoft.xboxgamingoverlay",
        "xbox",
        "itch",
        "io.itch",
        "playnite",
        "prismlauncher",
        "multimc",
        "minecraftlauncher",
        "minecraft launcher",
        "com.mojang.minecraftlauncher",
        "robloxplayerbeta",
        "robloxplayerlauncher",
        "robloxcorporation.roblox",
        "rockstargameslauncher",
        "rockstar games launcher",
        "com.rockstargames.launcher",
        "amazon games",
        "amazongames",
        "hoyoplay",
        "oculusclient",
    ];
    CLIENTS.iter().any(|name| {
        app.strip_prefix(name).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with(['.', '_', '!', '-', ' '])
        })
    })
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionMediaType {
    Video,
    Music,
    Other,
}

#[cfg(any(windows, test))]
fn windows_video_session(
    player: &str,
    title: &str,
    video_types: [Option<SessionMediaType>; 2],
) -> bool {
    let player = player.to_lowercase();
    let app = player.rsplit(['/', '\\']).next().unwrap_or_default();
    if app.trim().is_empty()
        || title.trim().is_empty()
        || is_browser_str(app)
        || is_windows_game_client(app)
        || is_audio_url(title)
    {
        return false;
    }
    let vlc = matches!(app, "vlc" | "vlc.exe" | "vlc media player")
        || app
            .strip_prefix("videolan.vlc")
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['_', '!']));
    if video_types.contains(&Some(SessionMediaType::Other))
        || video_types.contains(&Some(SessionMediaType::Music)) && !vlc
    {
        return false;
    }
    // VLC's SMTC plugin labels video metadata as Music to display the artist.
    video_types.contains(&Some(SessionMediaType::Video))
        || vlc
        || KNOWN_VIDEO_PLAYERS.iter().any(|name| app.contains(name))
}

#[cfg(windows)]
fn read_now(app: &AppHandle, cache: &mut RecognitionCache) -> anyhow::Result<Option<TickInfo>> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as SessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    };
    use windows::Media::MediaPlaybackType;
    let state = app.state::<AppState>();
    let matchers = state.matchers.lock().clone();
    let matches = PlaybackMatches::new(&state.db, &matchers, cache);
    let mut sources = Vec::new();
    let sessions = SessionManager::RequestAsync()
        .and_then(|request| request.join())
        .and_then(|manager| manager.GetSessions());
    if let Ok(sessions) = sessions {
        for session in sessions {
            let player = session
                .SourceAppUserModelId()
                .map(|h| h.to_string_lossy())
                .unwrap_or_default();
            let app = player.rsplit(['/', '\\']).next().unwrap_or_default();
            if is_browser_str(app) || is_windows_game_client(app) {
                continue;
            }
            let Ok(playback) = session.GetPlaybackInfo() else {
                continue;
            };
            let playing = match playback.PlaybackStatus() {
                Ok(PlaybackStatus::Playing) => true,
                Ok(PlaybackStatus::Paused) => false,
                _ => continue,
            };
            let Ok(properties) = session
                .TryGetMediaPropertiesAsync()
                .and_then(|op| op.join())
            else {
                continue;
            };
            let title = properties
                .Title()
                .map(|h| h.to_string_lossy())
                .unwrap_or_default();
            let video_type = |kind: Option<MediaPlaybackType>| {
                kind.filter(|kind| *kind != MediaPlaybackType::Unknown)
                    .map(|kind| match kind {
                        MediaPlaybackType::Video => SessionMediaType::Video,
                        MediaPlaybackType::Music => SessionMediaType::Music,
                        _ => SessionMediaType::Other,
                    })
            };
            if !windows_video_session(
                &player,
                &title,
                [
                    video_type(playback.PlaybackType().and_then(|kind| kind.Value()).ok()),
                    video_type(properties.PlaybackType().and_then(|kind| kind.Value()).ok()),
                ],
            ) {
                continue;
            }
            let timeline = session.GetTimelineProperties().ok();
            let (length_us, position_us) = windows_timeline(
                timeline
                    .as_ref()
                    .and_then(|t| t.StartTime().ok())
                    .map(|t| t.Duration),
                timeline
                    .as_ref()
                    .and_then(|t| t.EndTime().ok())
                    .map(|t| t.Duration),
                timeline
                    .as_ref()
                    .and_then(|t| t.Position().ok())
                    .map(|t| t.Duration),
            );
            let matched = matches.match_unbound(&title, "");
            let episode =
                matched.and_then(|m| library::resolve_bound_episode(m, &[title.as_str()], None));
            sources.push(TickInfo {
                playing,
                player,
                trackid: String::new(),
                title,
                length_us,
                position_us,
                media_id: matched.map(|m| m.media_id),
                matched_title: matched.map(|m| m.display.clone()),
                episode,
            });
        }
    }
    sources.extend(probe_mpv(app, &matches));
    Ok(preferred_source(sources))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn read_now(_app: &AppHandle, _cache: &mut RecognitionCache) -> anyhow::Result<Option<TickInfo>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SqlCount<'a> {
        db: &'a crate::db::Db,
        count: Box<std::cell::Cell<usize>>,
    }

    impl<'a> SqlCount<'a> {
        fn new(db: &'a crate::db::Db) -> Self {
            unsafe extern "C" fn record(
                _: u32,
                context: *mut std::ffi::c_void,
                _: *mut std::ffi::c_void,
                _: *mut std::ffi::c_void,
            ) -> i32 {
                let count = unsafe { &*context.cast::<std::cell::Cell<usize>>() };
                count.set(count.get() + 1);
                0
            }
            let mut trace = Self {
                db,
                count: Box::default(),
            };
            let result = unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    db.0.lock().handle(),
                    rusqlite::ffi::SQLITE_TRACE_STMT,
                    Some(record),
                    std::ptr::from_mut(trace.count.as_mut()).cast(),
                )
            };
            assert_eq!(result, rusqlite::ffi::SQLITE_OK);
            trace
        }
    }

    impl Drop for SqlCount<'_> {
        fn drop(&mut self) {
            unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    self.db.0.lock().handle(),
                    0,
                    None,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    #[test]
    fn mpv_socket_discovery_reads_one_fresh_setting_instead_of_six() {
        let db = playback_binding_db();
        let defaults = mpvipc::default_socket_paths();
        assert_eq!(mpv_socket_paths(&db), defaults);
        for custom in ["  \t\n", " /custom/mpv.sock \n", " /changed/mpv.sock "] {
            db.set_setting("tracking_mpv_socket", custom).unwrap();
            let trace = SqlCount::new(&db);
            let config = TrackingConfig::load(&db);
            assert_eq!(trace.count.get(), 6);
            let paths = mpv_socket_paths(&db);
            assert_eq!(trace.count.get(), 7);
            let mut expected = defaults.clone();
            if !custom.trim().is_empty() {
                expected.insert(0, custom.trim().into());
            }
            assert_eq!(paths, expected);
            assert_eq!(config.mpv_ipc_socket, custom);
        }
        if let Some(default) = defaults.first() {
            db.set_setting("tracking_mpv_socket", default).unwrap();
            assert_eq!(mpv_socket_paths(&db), defaults);
        }
        db.0.lock()
            .execute(
                "UPDATE settings SET value=X'FF' WHERE key='tracking_mpv_socket'",
                [],
            )
            .unwrap();
        assert_eq!(mpv_socket_paths(&db), defaults);
        db.set_setting("tracking_mpv_socket", "/recovered/mpv.sock")
            .unwrap();
        db.set_setting("tracking_mode", "auto").unwrap();
        assert_eq!(mpv_socket_paths(&db)[0], "/recovered/mpv.sock");
        assert_eq!(TrackingConfig::load(&db).mode, "auto");
        db.delete_setting("tracking_mpv_socket").unwrap();
        assert_eq!(mpv_socket_paths(&db), defaults);
        db.0.lock().execute_batch("DROP TABLE settings").unwrap();
        assert_eq!(mpv_socket_paths(&db), defaults);
        println!("MPV socket discovery: 1 settings query instead of 6");
    }

    #[test]
    fn unchanged_sources_reuse_recognition_but_keep_timing_and_selection_fresh() {
        let db = playback_binding_db();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        for poll in 0..10 {
            let matches = PlaybackMatches::new(&db, &matchers, &mut cache);
            let sources = [
                ("Some Show - 03", Some(1), Some(3)),
                ("Other Show - 07", Some(2), Some(7)),
                ("Unknown Program", None, None),
            ]
            .map(|(title, media_id, episode)| {
                let snapshot = mpvipc::MpvSnapshot {
                    playing: poll % 2 == 1 && media_id == Some(2),
                    path: format!("/videos/{title}.mkv"),
                    media_title: title.into(),
                    filename: format!("{title}.mkv"),
                    duration_us: 1_200_000_000 + poll,
                    position_us: poll * 5_000_000,
                };
                let info = mpv_snapshot_info(&matches, &snapshot);
                assert_eq!((info.media_id, info.episode), (media_id, episode));
                assert_eq!(info.playing, snapshot.playing);
                assert_eq!(info.length_us, snapshot.duration_us);
                assert_eq!(info.position_us, snapshot.position_us);
                info
            });
            assert_eq!(
                preferred_source(sources).unwrap().media_id,
                Some(if poll % 2 == 0 { 1 } else { 2 })
            );
            assert_eq!(cache.recognitions, 3);
        }
        assert_eq!(cache.entries.len(), 3);
        assert!(cache.entries.iter().any(|entry| entry.index.is_none()));
        println!("3 sources over 10 polls: 3 recognitions instead of 30");
    }

    #[test]
    fn recognition_cache_tracks_exact_titles_paths_matchers_and_accounts() {
        let db = playback_binding_db();
        let mut matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let lookup = |matchers: &std::sync::Arc<Vec<crate::recognize::Matcher>>,
                      cache: &mut RecognitionCache,
                      title: &str,
                      path: &str| {
            PlaybackMatches::new(&db, matchers, cache)
                .match_playing(title, path)
                .map(|(matched, _)| matched.media_id)
        };
        for (title, path) in [
            ("Some Show - 03", "/video.mkv"),
            ("Some Show - 04", "/video.mkv"),
            ("Some Show - 04", "/alternate.mkv"),
            ("Some Show - 04 ", "/alternate.mkv"),
        ] {
            assert_eq!(lookup(&matchers, &mut cache, title, path), Some(1));
        }
        assert_eq!(cache.recognitions, 4);
        for account in [Some("7"), Some("8"), None, Some("invalid"), Some("7")] {
            if let Some(account) = account {
                db.set_setting("anilist_user_id", account).unwrap();
            } else {
                db.delete_setting("anilist_user_id").unwrap();
            }
            let before = cache.recognitions;
            assert_eq!(
                lookup(&matchers, &mut cache, "Some Show - 04", "/alternate.mkv"),
                Some(1)
            );
            assert_eq!(
                cache.recognitions - before,
                usize::from(account != Some("invalid"))
            );
        }
        let old = std::sync::Arc::downgrade(&matchers);
        db.delete_entry(1).unwrap();
        matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        assert!(old.upgrade().is_none());
        let before = cache.recognitions;
        assert_eq!(
            lookup(&matchers, &mut cache, "Some Show - 04", "/alternate.mkv"),
            None
        );
        assert_eq!(cache.recognitions, before + 1);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(std::sync::Arc::strong_count(&matchers), 1);
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        assert_eq!(
            lookup(&matchers, &mut cache, "Some Show - 04", "/alternate.mkv"),
            Some(1)
        );
        assert_eq!(cache.recognitions, before + 2);
    }

    #[test]
    fn recognition_cache_bounds_capacity_and_age_including_misses() {
        let db = playback_binding_db();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        for index in 0..=RecognitionCache::CAPACITY {
            assert!(PlaybackMatches::new(&db, &matchers, &mut cache)
                .match_playing(&format!("Unknown Program {index}"), "")
                .is_none());
            assert!(cache.entries.len() <= RecognitionCache::CAPACITY);
        }
        let before = cache.recognitions;
        assert!(PlaybackMatches::new(&db, &matchers, &mut cache)
            .match_playing("Unknown Program 0", "")
            .is_none());
        assert_eq!(cache.recognitions, before + 1);
        assert_eq!(cache.entries.len(), RecognitionCache::CAPACITY);
        for entry in &mut cache.entries {
            entry.created = Instant::now() - RecognitionCache::LIFETIME;
        }
        assert!(PlaybackMatches::new(&db, &matchers, &mut cache)
            .match_playing("Unknown Program 0", "")
            .is_none());
        assert_eq!(cache.recognitions, before + 2);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn cached_recognition_preserves_seasons_ranges_and_single_episode_movies() {
        let db = playback_binding_db();
        for (id, title, episodes, format) in [
            (3, "Some Show Season 2", 12, "TV"),
            (4, "Example Movie", 1, "MOVIE"),
        ] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(episodes),
                format: Some(format.into()),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let cases = [
            ("Some Show - 03", Some(1), Some(3)),
            ("Some Show S02E04", Some(3), Some(4)),
            ("Some Show - 01-12", Some(1), None),
            ("Example Movie", Some(4), Some(1)),
        ];
        for _ in 0..3 {
            let matches = PlaybackMatches::new(&db, &matchers, &mut cache);
            for (title, media_id, episode) in cases {
                let path = format!("/videos/{title}.mkv");
                let matched = matches.match_playing(title, &path);
                let actual_episode = matched
                    .and_then(|(m, binding)| playing_episode(m, title, &basename(&path), binding));
                assert_eq!(
                    (matched.map(|(m, _)| m.media_id), actual_episode),
                    (media_id, episode),
                    "{title}"
                );
            }
            assert_eq!(cache.recognitions, cases.len());
        }
    }

    #[test]
    fn windows_media_timestamps_are_relative_to_the_start_of_the_item() {
        let timeline = |start_seconds: i64, end_seconds: i64, position_seconds: i64| {
            windows_timeline(
                Some(start_seconds * 10_000_000),
                Some(end_seconds * 10_000_000),
                Some(position_seconds * 10_000_000),
            )
        };
        assert_eq!(timeline(0, 100, 75), (100_000_000, 75_000_000));
        for start in [100, -100] {
            let (length_us, position_us) = timeline(start, start + 100, start + 75);
            assert_eq!((length_us, position_us), (100_000_000, 75_000_000));
            assert!(!AutoGate {
                length_us,
                position_us,
                ..watched()
            }
            .should_push());
        }
    }

    #[test]
    fn unavailable_or_invalid_windows_timestamps_do_not_trigger_auto_tracking() {
        for (start, end, position, expected) in [
            (Some(0), Some(100), None, (0, 0)),
            (None, Some(100), Some(75), (0, 0)),
            (Some(50), Some(100), Some(49), (0, 0)),
            (Some(50), None, Some(75), (0, 2)),
            (Some(50), Some(49), Some(75), (0, 2)),
            (Some(i64::MIN), Some(i64::MAX), Some(0), (0, 0)),
        ] {
            let (length_us, position_us) = windows_timeline(start, end, position);
            assert_eq!((length_us, position_us), expected);
            assert!(!AutoGate {
                length_us,
                position_us,
                ..watched()
            }
            .should_push());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broken_mpris_sessions_and_invalid_timelines_do_not_poison_tracking() {
        use dbus::arg::Variant;
        use dbus::channel::{MatchingReceiver, Sender};
        use std::io::BufRead;
        use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
        use std::sync::Arc;

        struct Bus(std::process::Child);
        impl Drop for Bus {
            fn drop(&mut self) {
                self.0.kill().ok();
                self.0.wait().ok();
            }
        }
        let mut bus = Bus(std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("install dbus-daemon to run the MPRIS integration test"));
        let mut address = String::new();
        std::io::BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim().to_string();
        let broken = Arc::new(AtomicBool::new(true));
        let service_broken = broken.clone();
        let position = Arc::new(AtomicI64::new(42_000_000));
        let service_position = position.clone();
        let length = Arc::new(AtomicI64::new(1_200_000_000));
        let service_length = length.clone();
        let service_address = address.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let service = std::thread::spawn(move || {
            let connection = dbus::blocking::Connection::new_address(&service_address).unwrap();
            for name in ["aaa_healthy", "bbb_broken", "ccc_healthy"] {
                connection
                    .request_name(format!("org.mpris.MediaPlayer2.{name}"), false, false, true)
                    .unwrap();
            }
            connection.start_receive(
                dbus::message::MatchRule::new_method_call(),
                Box::new(move |message, connection| {
                    if message
                        .member()
                        .is_some_and(|member| member == "Introspect")
                    {
                        connection
                            .send(message.method_return().append1("<node/>"))
                            .unwrap();
                        return true;
                    }
                    let (_, property) = message.read2::<String, String>().unwrap();
                    let failed = message
                        .destination()
                        .is_some_and(|name| name.ends_with("bbb_broken"))
                        && service_broken.load(Ordering::SeqCst);
                    let response = match property.as_str() {
                        "Identity" if !failed => message.method_return().append1(Variant("VLC")),
                        "HasTrackList" => message.method_return().append1(Variant(false)),
                        "PlaybackStatus" => message.method_return().append1(Variant("Playing")),
                        "Position" => message
                            .method_return()
                            .append1(Variant(service_position.load(Ordering::SeqCst))),
                        "Metadata" => {
                            let metadata: dbus::arg::PropMap = std::collections::HashMap::from([
                                (
                                    "mpris:length".into(),
                                    Variant(Box::new(service_length.load(Ordering::SeqCst))
                                        as Box<dyn dbus::arg::RefArg>),
                                ),
                                (
                                    "xesam:title".into(),
                                    Variant(Box::new("Some Show - 03".to_string())
                                        as Box<dyn dbus::arg::RefArg>),
                                ),
                                (
                                    "xesam:url".into(),
                                    Variant(Box::new(
                                        "file:///anime/Some%20Show%20-%2003.mkv".to_string(),
                                    )
                                        as Box<dyn dbus::arg::RefArg>),
                                ),
                            ]);
                            message.method_return().append1(Variant(metadata))
                        }
                        _ => message.error(
                            &"org.freedesktop.DBus.Error.UnknownProperty".into(),
                            c"session is unavailable",
                        ),
                    };
                    connection.send(response).unwrap();
                    true
                }),
            );
            ready_tx.send(()).unwrap();
            while matches!(
                stop_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                connection.process(Duration::from_millis(20)).unwrap();
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let connection = dbus::ffidisp::Connection::open_private(&address).unwrap();
        connection.register().unwrap();
        let finder = PlayerFinder::for_connection(connection);
        let players = mpris_players(&finder);
        let healthy = players
            .iter()
            .map(|player| {
                (
                    player.bus_name().to_string(),
                    player.get_playback_status().unwrap(),
                    player.get_metadata().unwrap().title().unwrap().to_string(),
                    player.get_position().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        broken.store(false, Ordering::SeqCst);
        let recovered = mpris_players(&finder).len();
        let timeline = || {
            let metadata = players[0].get_metadata().unwrap();
            mpris_timeline(&players[0], &metadata)
        };
        let valid_timeline = timeline();
        let invalid_positions = [-1, i64::MIN].map(|invalid| {
            position.store(invalid, Ordering::SeqCst);
            timeline()
        });
        position.store(42_000_000, Ordering::SeqCst);
        let invalid_lengths = [-1, i64::MIN].map(|invalid| {
            length.store(invalid, Ordering::SeqCst);
            timeline()
        });
        stop_tx.send(()).unwrap();
        service.join().unwrap();

        assert_eq!(healthy.len(), 2);
        assert!(healthy[0].0.ends_with("aaa_healthy"));
        assert!(healthy[1].0.ends_with("ccc_healthy"));
        for (_, status, title, position) in healthy {
            assert_eq!(status, PlaybackStatus::Playing);
            assert_eq!(title, "Some Show - 03");
            assert_eq!(position, Duration::from_secs(42));
        }
        assert_eq!(recovered, 3);
        assert_eq!(valid_timeline, (1_200_000_000, 42_000_000));
        assert_eq!(invalid_positions, [(0, 0); 2]);
        assert_eq!(invalid_lengths, [(0, 42_000_000); 2]);
    }

    #[test]
    fn separated_release_titles_keep_episode_numbers_in_scans_and_playback() {
        struct Folder(std::path::PathBuf);
        impl Drop for Folder {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let folder = Folder(
            std::env::temp_dir().join(format!("kurisu-separated-titles-{}", std::process::id())),
        );
        std::fs::create_dir(&folder.0).unwrap();
        let cases = [
            ("Some.Show.-.05.-.Part.2.mkv", Some(5)),
            ("Some_Show_-_05_-_Part_2.mkv", Some(5)),
            ("Some_Show_-_05_-_1-2-3_Go.mkv", Some(5)),
            ("Some_Show_01-12.mkv", None),
            ("Some Show_01-12.mkv", None),
            ("Some_Show_E01-E12.mkv", None),
            ("Some_Show_01-12_1080p.mkv", None),
            ("Some.Show.mkv", None),
            ("Some_Show.mkv", None),
        ];
        for (filename, _) in cases {
            std::fs::write(folder.0.join(filename), []).unwrap();
        }
        let scan = library::ScanCache::default().scan(
            &[folder.0.to_str().unwrap().into()],
            &matchers,
            &Default::default(),
        );
        assert!(scan.unreadable.is_empty());
        assert_eq!(scan.files.len(), cases.len());
        for file in scan.files {
            let filename = std::path::Path::new(&file.path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            let expected = cases.iter().find(|(name, _)| *name == filename).unwrap().1;
            assert_eq!(file.media_id, Some(1), "{}", file.path);
            assert_eq!(file.episode, expected, "{}", file.path);
            assert!(!file.bound);
            for title in ["", "Some Show"] {
                let info = mpv_snapshot_info(
                    &PlaybackMatches::new(&db, &matchers, &mut cache),
                    &mpvipc::MpvSnapshot {
                        playing: true,
                        path: file.path.clone(),
                        media_title: title.into(),
                        filename: filename.into(),
                        duration_us: 1_200_000_000,
                        position_us: 1_100_000_000,
                    },
                );
                assert_eq!(
                    (info.media_id, info.episode),
                    (Some(1), expected),
                    "{filename}, title: {title}"
                );
            }
        }
    }

    #[test]
    fn bound_playback_uses_library_numbering_despite_embedded_titles() {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let binding = Some(library::LibraryBinding {
            media_id: 1,
            episode_offset: -12,
        });
        for title in ["Some Show - Episode 1", "Some Show - Episode 24"] {
            assert_eq!(
                playing_episode(&matchers[0], title, "ep13.mkv", binding),
                Some(1)
            );
        }
        assert_eq!(
            playing_episode(&matchers[0], "Some Show - Episode 13", "video.mkv", binding),
            Some(1)
        );
        assert_eq!(
            playing_episode(&matchers[0], "Some Show - Episode 13", "ep25.mkv", binding),
            None
        );
    }

    struct BindingFolder(std::path::PathBuf);

    impl BindingFolder {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kurisu-playback-bindings-{label}-{}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for BindingFolder {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn playback_binding_db() -> crate::db::Db {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, title) in [(1, "Some Show"), (2, "Other Show")] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        db
    }

    #[test]
    fn playback_binding_snapshots_refresh_edits_and_failed_reads_each_poll() {
        let db = playback_binding_db();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let folder = BindingFolder::new("edits");
        let path = folder.0.join("ep13.mkv");
        std::fs::write(&path, []).unwrap();
        let snapshot = mpvipc::MpvSnapshot {
            playing: false,
            path: path.to_str().unwrap().into(),
            media_title: "Other Show - 03".into(),
            filename: "ep13.mkv".into(),
            duration_us: 1_200_000_000,
            position_us: 1_100_000_000,
        };
        let bind = |offset| {
            let bindings = std::collections::HashMap::from([(
                folder.0.to_str().unwrap(),
                library::LibraryBinding {
                    media_id: 1,
                    episode_offset: offset,
                },
            )]);
            db.set_setting(
                "library_bindings",
                &serde_json::to_string(&bindings).unwrap(),
            )
            .unwrap();
        };
        let result = |matches: &PlaybackMatches<'_>| {
            let info = mpv_snapshot_info(matches, &snapshot);
            assert!(!info.playing);
            (info.media_id, info.episode)
        };

        assert_eq!(
            result(&PlaybackMatches::new(&db, &matchers, &mut cache)),
            (Some(2), Some(3))
        );
        assert_eq!(cache.recognitions, 1);
        bind(-12);
        let first_poll = PlaybackMatches::new(&db, &matchers, &mut cache);
        assert_eq!(result(&first_poll), (Some(1), Some(1)));
        bind(-11);
        assert_eq!(result(&first_poll), (Some(1), Some(1)));
        assert_eq!(
            result(&PlaybackMatches::new(&db, &matchers, &mut cache)),
            (Some(1), Some(2))
        );

        db.set_setting("library_bindings", "{invalid").unwrap();
        let failed_poll = PlaybackMatches::new(&db, &matchers, &mut cache);
        assert_eq!(result(&failed_poll), (None, None));
        bind(-12);
        assert_eq!(result(&failed_poll), (None, None));
        assert_eq!(
            result(&PlaybackMatches::new(&db, &matchers, &mut cache)),
            (Some(1), Some(1))
        );

        library::unbind_media(&db, 1).unwrap();
        assert_eq!(
            result(&PlaybackMatches::new(&db, &matchers, &mut cache)),
            (Some(2), Some(3))
        );
        assert_eq!(cache.recognitions, 1);
        bind(-12);
        let current_matchers = std::sync::Arc::new(
            crate::recognize::build_matchers(&db)
                .into_iter()
                .filter(|matcher| matcher.media_id == 2)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            result(&PlaybackMatches::new(&db, &current_matchers, &mut cache)),
            (Some(2), Some(3))
        );
        assert_eq!(cache.recognitions, 2);
    }

    #[cfg(unix)]
    #[test]
    fn playback_binding_snapshots_refresh_symlinks_and_missing_paths() {
        use std::os::unix::fs::symlink;

        let db = playback_binding_db();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let folder = BindingFolder::new("symlinks");
        let first = folder.0.join("first");
        let second = folder.0.join("second");
        let alias = folder.0.join("alias");
        for dir in [&first, &second] {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("ep13.mkv"), []).unwrap();
        }
        symlink(&first, &alias).unwrap();
        library::bind_path(&db, alias.to_str().unwrap(), 1, -12).unwrap();
        let matched = |matches: &PlaybackMatches<'_>, path: &std::path::Path| {
            matches
                .match_playing("", path.to_str().unwrap())
                .map(|(matcher, binding)| (matcher.media_id, binding.unwrap().episode_offset))
        };
        let first_poll = PlaybackMatches::new(&db, &matchers, &mut cache);
        assert_eq!(
            matched(&first_poll, &alias.join("ep13.mkv")),
            Some((1, -12))
        );
        std::fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert_eq!(matched(&first_poll, &alias.join("ep13.mkv")), None);

        let next_poll = PlaybackMatches::new(&db, &matchers, &mut cache);
        assert_eq!(matched(&next_poll, &first.join("ep13.mkv")), None);
        assert_eq!(
            matched(&next_poll, &second.join("ep13.mkv")),
            Some((1, -12))
        );
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(
            matched(
                &PlaybackMatches::new(&db, &matchers, &mut cache),
                &second.join("ep13.mkv")
            ),
            None
        );
        symlink(&first, &alias).unwrap();
        assert_eq!(
            matched(
                &PlaybackMatches::new(&db, &matchers, &mut cache),
                &first.join("ep13.mkv")
            ),
            Some((1, -12))
        );
    }

    fn source(
        player: &str,
        playing: bool,
        media_id: Option<i64>,
        episode: Option<i64>,
    ) -> TickInfo {
        TickInfo {
            playing,
            player: player.into(),
            trackid: String::new(),
            title: "Title".into(),
            length_us: 0,
            position_us: 0,
            media_id,
            matched_title: media_id.map(|_| "Anime".into()),
            episode,
        }
    }

    #[test]
    fn windows_sessions_reject_riot_clients_even_when_previews_claim_to_be_video() {
        for player in [
            "com.riotgames.RiotGames.RiotClient",
            "RiotClientServices.exe",
            "LeagueClientUx.exe",
            "COM.RIOTGAMES.RIOTGAMES.RIOTCLIENT",
        ] {
            for title in ["", "League of Legends", "Some Show - 03"] {
                for types in [
                    [None, None],
                    [Some(SessionMediaType::Video), None],
                    [Some(SessionMediaType::Video), Some(SessionMediaType::Video)],
                ] {
                    assert!(
                        !windows_video_session(player, title, types),
                        "{player}: {title}"
                    );
                }
            }
        }
    }

    #[test]
    fn windows_sessions_reject_game_clients_and_vrchat() {
        for player in [
            "VRChat",
            "VRCHAT.EXE",
            "com.vrchat.client",
            "VRChatInc.VRChat!App",
            r"C:\Program Files (x86)\Steam\steamapps\common\VRChat\VRChat.exe",
            "Steam.exe",
            "Steam.Client!App",
            "steamwebhelper.exe",
            "com.valvesoftware.steam",
            "SteamVR.exe",
            "EpicGamesLauncher.exe",
            "EpicWebHelper.exe",
            "com.epicgames.launcher",
            "Battle.net.exe",
            "Blizzard Battle.net Launcher",
            "com.blizzard.battlenet",
            "EADesktop.exe",
            "EALauncher.exe",
            "EABackgroundService.exe",
            "EA Desktop",
            "Origin.exe",
            "OriginWebHelperService.exe",
            "com.ea.origin",
            "UbisoftConnect.exe",
            "UbisoftGameLauncher.exe",
            "Ubisoft Connect",
            "uplay.exe",
            "UplayWebCore.exe",
            "com.ubisoft.connect",
            "GalaxyClient.exe",
            "GOG Galaxy",
            "com.gog.galaxy",
            "Microsoft.GamingApp_8wekyb3d8bbwe!Microsoft.Xbox.App",
            "Microsoft.XboxApp_8wekyb3d8bbwe!Microsoft.XboxApp",
            "Microsoft.XboxGamingOverlay_8wekyb3d8bbwe!App",
            "Xbox.exe",
            "itch.exe",
            "io.itch.itch",
            "Playnite.DesktopApp.exe",
            "Playnite.FullscreenApp.exe",
            "PrismLauncher.exe",
            "MultiMC.exe",
            "MinecraftLauncher.exe",
            "com.mojang.minecraftlauncher",
            "RobloxPlayerBeta.exe",
            "RobloxPlayerLauncher.exe",
            "ROBLOXCORPORATION.ROBLOX_55nm5eh3cm0pr!App",
            "Rockstar Games Launcher",
            "com.rockstargames.launcher",
            "Amazon Games",
            "HoYoPlay.exe",
            "OculusClient.exe",
        ] {
            for types in [
                [None, None],
                [Some(SessionMediaType::Video), None],
                [None, Some(SessionMediaType::Video)],
                [Some(SessionMediaType::Video), Some(SessionMediaType::Video)],
            ] {
                assert!(
                    !windows_video_session(player, "Some Show - 03", types),
                    "{player}: {types:?}"
                );
            }
        }
    }

    #[test]
    fn windows_sessions_game_exclusions_do_not_match_folders_or_video_titles() {
        for player in [
            r"C:\Users\VRChat\Programs\vlc.exe",
            r"C:\Program Files (x86)\Steam\steamapps\common\Video Player\mpv.exe",
            r"C:\Users\EpicGamesLauncher\Programs\CustomVideoPlayer.exe",
            "OriginStoryPlayer.exe",
            "SteamshipVideo.exe",
            "itchyPlayer.exe",
        ] {
            for title in ["VRChat", "Steam - 03", "Origin - 04", "League of Legends"] {
                assert!(
                    windows_video_session(player, title, [Some(SessionMediaType::Video), None]),
                    "{player}: {title}"
                );
            }
        }
    }

    #[test]
    fn windows_sessions_game_clients_cannot_displace_video_playback() {
        let clients = ["VRChat.exe", "Steam.exe", "EpicGamesLauncher.exe"];
        let filter = |info: &TickInfo| {
            windows_video_session(
                &info.player,
                &info.title,
                [Some(SessionMediaType::Video), Some(SessionMediaType::Video)],
            )
        };
        for client in clients {
            let selected = preferred_source(
                [
                    source(client, true, Some(1), Some(3)),
                    source("VLC", false, Some(1), Some(3)),
                ]
                .into_iter()
                .filter(filter),
            )
            .unwrap();
            assert_eq!(selected.player, "VLC", "{client}");
            assert!(!selected.playing);
        }
        assert!(preferred_source(
            clients
                .into_iter()
                .map(|client| source(client, true, Some(1), Some(3)))
                .filter(filter)
        )
        .is_none());
    }

    #[test]
    fn windows_sessions_require_video_evidence_and_usable_titles() {
        assert!(!windows_video_session(
            "",
            "Some Show - 03",
            [Some(SessionMediaType::Video), None]
        ));
        for title in ["", " \t\n", "\u{2003}"] {
            assert!(!windows_video_session(
                "VLC",
                title,
                [Some(SessionMediaType::Video), Some(SessionMediaType::Video)]
            ));
        }
        for title in ["Some Show - 03", "League of Legends"] {
            assert!(!windows_video_session("UnknownApp", title, [None, None]));
            assert!(windows_video_session(
                "CustomVideoPlayer",
                title,
                [Some(SessionMediaType::Video), None]
            ));
            assert!(windows_video_session(
                "CustomVideoPlayer",
                title,
                [None, Some(SessionMediaType::Video)]
            ));
        }
        assert!(windows_video_session(
            r"C:\Users\opera\Programs\CustomVideoPlayer.exe",
            "Some Show - 03",
            [Some(SessionMediaType::Video), None],
        ));
        for player in [
            "VLC media player",
            "mpv.net",
            "MPC-HC64.exe",
            "MPC-BE64.exe",
            "PotPlayerMini64.exe",
        ] {
            assert!(
                windows_video_session(player, "Unlisted Show - 03", [None, None]),
                "{player}"
            );
        }
    }

    #[test]
    fn windows_sessions_use_application_names_instead_of_parent_folders() {
        assert!(windows_video_session(
            r"C:\Users\RiotGames\AppData\mpv.net\mpvnet.exe",
            "Some Show - 03",
            [None, None],
        ));
        assert!(!windows_video_session(
            r"C:\Users\mpv\Games\UnknownGame.exe",
            "Some Show - 03",
            [None, None],
        ));
        for player in [
            r"C:\Users\mpv\Programs\msedge.exe",
            r"C:\Users\vlc\Programs\foobar2000.exe",
            r"C:\Riot Games\RiotClientServices.exe",
            "RiotGames.RiotClient!App",
        ] {
            assert!(!windows_video_session(
                player,
                "Some Show - 03",
                [Some(SessionMediaType::Video), None]
            ));
        }
    }

    #[test]
    fn windows_sessions_reject_music_images_and_conflicting_media_types() {
        for player in ["CustomVideoPlayer", "Microsoft.ZuneMusic"] {
            for types in [
                [Some(SessionMediaType::Music), None],
                [None, Some(SessionMediaType::Music)],
                [Some(SessionMediaType::Video), Some(SessionMediaType::Music)],
                [Some(SessionMediaType::Music), Some(SessionMediaType::Video)],
            ] {
                assert!(
                    !windows_video_session(player, "Some Show - 03", types),
                    "{player}: {types:?}"
                );
            }
        }
        for player in [
            "Firefox",
            "308046B0AF4A39CB",
            "msedge.exe",
            "Spotify",
            "foobar2000",
        ] {
            assert!(!windows_video_session(
                player,
                "Some Show - 03",
                [Some(SessionMediaType::Video), Some(SessionMediaType::Video)]
            ));
        }
        assert!(!windows_video_session(
            "VLC",
            "Some Show - 03.flac",
            [None, None]
        ));
        assert!(windows_video_session(
            "VLC",
            "Some Show - 03.mkv",
            [None, None]
        ));
    }

    #[test]
    fn windows_sessions_accept_vlc_plugin_music_metadata_but_not_images_or_audio_files() {
        for player in [
            "VLC",
            "vlc.exe",
            "VLC media player",
            "VideoLAN.VLC_paz6r1rewnh0a!App",
        ] {
            for types in [
                [None, Some(SessionMediaType::Music)],
                [Some(SessionMediaType::Music), None],
                [Some(SessionMediaType::Music), Some(SessionMediaType::Music)],
                [Some(SessionMediaType::Video), Some(SessionMediaType::Music)],
            ] {
                assert!(
                    windows_video_session(player, "Some Show - 03", types),
                    "{player}: {types:?}"
                );
                assert!(!windows_video_session(player, "Some Show - 03.flac", types));
            }
            assert!(!windows_video_session(
                player,
                "Some Show - 03",
                [Some(SessionMediaType::Other), None]
            ));
            assert!(!windows_video_session(
                player,
                "Some Show - 03",
                [Some(SessionMediaType::Music), Some(SessionMediaType::Other)]
            ));
        }
        assert!(!windows_video_session(
            "SomeVlcGame.exe",
            "Some Show - 03",
            [Some(SessionMediaType::Music), None]
        ));
    }

    #[test]
    fn windows_sessions_do_not_let_a_playing_launcher_displace_a_paused_video() {
        let mut riot = source("com.riotgames.RiotGames.RiotClient", true, None, None);
        riot.title = "Some Show - 03".into();
        riot.length_us = 10_000_000;
        let paused = source("VLC", false, Some(1), Some(3));
        let accepted = [riot, paused].into_iter().filter(|info| {
            windows_video_session(
                &info.player,
                &info.title,
                [Some(SessionMediaType::Video), Some(SessionMediaType::Video)],
            )
        });
        let selected = preferred_source(accepted).unwrap();
        assert_eq!(selected.player, "VLC");
        assert!(!selected.playing);
        assert!(preferred_source(
            [source(
                "com.riotgames.RiotGames.RiotClient",
                true,
                None,
                None
            )]
            .into_iter()
            .filter(|info| windows_video_session(
                &info.player,
                &info.title,
                [None, None]
            ))
        )
        .is_none());
    }

    #[test]
    fn recognized_playing_sources_outrank_unrelated_media_sessions() {
        let selected = preferred_source([
            source("vlc", true, None, None),
            source("mpv-ipc", true, Some(1), Some(3)),
            source("other", true, None, None),
        ])
        .unwrap();
        assert_eq!(selected.player, "mpv-ipc");
        let selected = preferred_source([
            source("matched-paused", false, Some(1), Some(3)),
            source("playing", true, None, None),
        ])
        .unwrap();
        assert_eq!(selected.player, "playing");
        let selected = preferred_source([
            source("unknown-episode", true, Some(1), None),
            source("known-episode", true, Some(1), Some(3)),
            source("same-rank", true, Some(2), Some(4)),
        ])
        .unwrap();
        assert_eq!(selected.player, "known-episode");
    }

    #[test]
    fn watch_history_accepts_local_video_paths_but_not_stream_urls_or_titles() {
        let file = std::env::temp_dir().join("kurisu history episode 03.mkv");
        let url = reqwest::Url::from_file_path(&file).unwrap();
        assert_eq!(
            local_video_path(url.as_str()),
            Some(file.to_str().unwrap().into())
        );
        assert_eq!(
            local_video_path(file.to_str().unwrap()),
            Some(file.to_str().unwrap().into())
        );
        for source in [
            "https://example.org/episode.mkv",
            "Some Show - 03",
            "relative.mkv",
            "file:///tmp/song.flac",
        ] {
            assert_eq!(local_video_path(source), None, "{source}");
        }
    }

    #[test]
    fn a_local_path_breaks_equal_player_ties_for_watch_history() {
        let session = source("media-session", true, Some(1), Some(3));
        let mut mpv = source("mpv-ipc", true, Some(1), Some(3));
        mpv.trackid = std::env::temp_dir()
            .join("episode.mkv")
            .to_str()
            .unwrap()
            .into();
        assert_eq!(preferred_source([session, mpv]).unwrap().player, "mpv-ipc");
    }

    #[test]
    fn switching_file_url_and_ipc_sources_preserves_watch_time() {
        let file = std::env::temp_dir().join("kurisu episode 03.mkv");
        let mut session = source("media-session", true, Some(1), Some(3));
        session.trackid = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let mut ipc = source("mpv-ipc", true, Some(1), Some(3));
        ipc.trackid = file.to_str().unwrap().into();
        let account = Some("account-token".into());
        let mut track = ActiveTrack::new(session.key(account.clone()));
        track.accumulated = Duration::from_secs(55);
        if track.key != ipc.key(account.clone()) {
            track = ActiveTrack::new(ipc.key(account.clone()));
        }
        assert_eq!(track.accumulated, Duration::from_secs(55));
        assert!(session.key(account.clone()) != ipc.key(Some("other-account".into())));
        ipc.episode = Some(4);
        assert!(session.key(account.clone()) != ipc.key(account.clone()));
        ipc.episode = Some(3);
        ipc.trackid = std::env::temp_dir()
            .join("another release 03.mkv")
            .to_str()
            .unwrap()
            .into();
        assert!(session.key(account.clone()) != ipc.key(account));
    }

    #[test]
    fn usable_timing_breaks_recognized_player_ties_for_tracking() {
        let file = std::env::temp_dir().join("episode.mkv");
        let mut session = source("media-session", true, Some(1), Some(3));
        session.trackid = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let mut mpv = source("mpv-ipc", true, Some(1), Some(3));
        mpv.trackid = file.to_str().unwrap().into();
        mpv.length_us = 1_200_000_000;
        mpv.position_us = 1_080_000_000;
        let selected = preferred_source([session, mpv]).unwrap();
        let gate = AutoGate {
            length_us: selected.length_us,
            position_us: selected.position_us,
            ..watched()
        };
        assert!(
            gate.should_push(),
            "selected {} with no usable timing",
            selected.player
        );
        assert_eq!(selected.player, "mpv-ipc");
    }

    #[test]
    fn timing_does_not_override_playback_or_recognition_priority() {
        for playing in [true, false] {
            let mut timed = source("timed", playing, None, None);
            timed.length_us = 1_200_000_000;
            timed.position_us = 1_080_000_000;
            let recognized = source("recognized", true, Some(1), Some(3));
            assert_eq!(
                preferred_source([timed, recognized]).unwrap().player,
                "recognized"
            );
        }
        let mut paused = source("paused", false, Some(1), Some(3));
        paused.length_us = 1_200_000_000;
        paused.position_us = 1_080_000_000;
        let playing = source("playing", true, Some(1), Some(3));
        assert_eq!(
            preferred_source([paused, playing]).unwrap().player,
            "playing"
        );
        let mut invalid = source("invalid-position", true, Some(1), Some(3));
        invalid.length_us = 1_200_000_000;
        invalid.position_us = -1;
        let mut valid = source("valid", true, Some(1), Some(3));
        valid.length_us = 1_200_000_000;
        assert_eq!(preferred_source([invalid, valid]).unwrap().player, "valid");
    }

    #[test]
    fn watched_history_can_record_existing_progress_without_another_remote_update() {
        let already_recorded = AutoGate {
            episode: 4,
            progress: 4,
            ..watched()
        };
        assert!(already_recorded.watched_enough());
        assert!(!already_recorded.should_push());
        let just_resumed = AutoGate {
            accumulated: Duration::from_secs(5),
            ..already_recorded
        };
        assert!(!just_resumed.watched_enough());
    }

    fn watched() -> AutoGate {
        AutoGate {
            incremented: false,
            was_playing_before: true,
            accumulated: Duration::from_secs(600),
            fail_count: 0,
            retry_due: true,
            length_us: 24 * 60 * 1_000_000,
            position_us: 24 * 60 * 1_000_000 * 9 / 10,
            auto_percent: 80,
            episode: 5,
            progress: 4,
        }
    }

    #[test]
    fn stream_episodes_and_sessions_reset_tracking_state() {
        let mut info = TickInfo {
            playing: true,
            player: "mpv".into(),
            trackid: "https://example.org/stream.m3u8".into(),
            title: "Some Show - 05".into(),
            length_us: 1_440_000_000,
            position_us: 0,
            media_id: Some(1),
            matched_title: Some("Some Show".into()),
            episode: Some(5),
        };
        let first = info.key(Some("first-session".into()));
        assert!(first == info.key(Some("first-session".into())));
        info.episode = Some(6);
        assert!(first != info.key(Some("first-session".into())));
        info.episode = Some(5);
        assert!(first != info.key(Some("second-session".into())));
        info.media_id = Some(2);
        assert!(first != info.key(Some("first-session".into())));
    }

    #[test]
    fn auto_push_fires_for_a_genuinely_watched_episode() {
        assert!(watched().should_push());
    }

    #[test]
    fn seeking_to_the_credits_does_not_push() {
        let g = AutoGate {
            accumulated: Duration::from_secs(5),
            ..watched()
        };
        assert!(!g.should_push());
        let g = AutoGate {
            was_playing_before: false,
            accumulated: Duration::ZERO,
            ..watched()
        };
        assert!(!g.should_push());
    }

    #[test]
    fn resume_on_open_does_not_complete_a_show() {
        let g = AutoGate {
            was_playing_before: false,
            accumulated: Duration::ZERO,
            position_us: 24 * 60 * 1_000_000 * 97 / 100,
            episode: 24,
            progress: 3,
            ..watched()
        };
        assert!(!g.should_push());
    }

    #[test]
    fn short_specials_still_track() {
        let len = 4 * 60 * 1_000_000_i64;
        let g = AutoGate {
            length_us: len,
            position_us: len * 9 / 10,
            accumulated: Duration::from_secs(65),
            ..watched()
        };
        assert!(g.should_push());
        assert_eq!(min_watch_time(len), Duration::from_secs(60));
        assert_eq!(min_watch_time(2 * 60 * 1_000_000), Duration::from_secs(30));
        assert_eq!(min_watch_time(0), Duration::from_secs(60));
    }

    #[test]
    fn below_threshold_or_already_done_does_not_push() {
        let g = AutoGate {
            position_us: 24 * 60 * 1_000_000 / 2,
            ..watched()
        };
        assert!(!g.should_push(), "50% is under the 80% threshold");
        let g = AutoGate {
            incremented: true,
            ..watched()
        };
        assert!(!g.should_push(), "already pushed for this track");
        let g = AutoGate {
            episode: 4,
            progress: 4,
            ..watched()
        };
        assert!(
            !g.should_push(),
            "never rewinds or rewrites the same episode"
        );
        let g = AutoGate {
            episode: 2,
            progress: 9,
            ..watched()
        };
        assert!(
            !g.should_push(),
            "rewatching an earlier episode must not rewind"
        );
    }

    #[test]
    fn a_player_with_no_duration_never_auto_pushes() {
        let g = AutoGate {
            length_us: 0,
            position_us: 0,
            ..watched()
        };
        assert_eq!(g.pct(), 0.0);
        assert!(!g.should_push());
    }

    #[test]
    fn failed_pushes_back_off_and_then_give_up() {
        let g = AutoGate {
            fail_count: 1,
            retry_due: false,
            ..watched()
        };
        assert!(!g.should_push());
        let g = AutoGate {
            fail_count: 1,
            retry_due: true,
            ..watched()
        };
        assert!(g.should_push());
        let g = AutoGate {
            fail_count: MAX_AUTO_PUSH_FAILURES,
            retry_due: true,
            ..watched()
        };
        assert!(!g.should_push());
        assert!(auto_push_backoff(1) < auto_push_backoff(2));
        assert!(auto_push_backoff(2) < auto_push_backoff(3));
    }

    #[test]
    fn browsers_are_excluded_but_known_players_are_not() {
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.firefox.instance_1 Firefox"
        ));
        assert!(is_browser_str("308046B0AF4A39CB"), "opaque Firefox AUMID");
        assert!(is_browser_str("Chromium"));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.plasma.browser.integration Plasma Browser Integration"
        ));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.kdeconnect.pixel_7 KDE Connect"
        ));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.playerctld playerctld"
        ));
        assert!(!is_browser_str("org.mpris.MediaPlayer2.mpv mpv"));
        assert!(!is_browser_str("io.github.celluloid_player.Celluloid"));
        assert!(!is_browser_str("VLC media player"));
        assert!(!is_browser_str(
            r"C:\Users\opera\AppData\mpv.net\mpvnet.exe"
        ));
    }

    #[test]
    fn spotify_never_drives_the_banner_or_tracking() {
        assert!(is_browser_str("org.mpris.MediaPlayer2.spotify Spotify"));
        assert!(is_browser_str("SpotifyAB.SpotifyMusic-hz89res2p8targ!App"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.spotifyd spotifyd"));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.spotify-qt spotify-qt"
        ));
    }

    #[test]
    fn local_music_players_are_excluded() {
        assert!(is_browser_str("org.mpris.MediaPlayer2.amberol Amberol"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.lollypop Lollypop"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.rhythmbox Rhythmbox"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.elisa Elisa"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.audacious Audacious"));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.clementine Clementine"
        ));
        assert!(is_browser_str(
            "org.mpris.MediaPlayer2.strawberry Strawberry"
        ));
        assert!(is_browser_str("org.mpris.MediaPlayer2.mpd mpd"));
        assert!(is_browser_str("foobar2000"));
        assert!(!is_browser_str("org.mpris.MediaPlayer2.vlc VLC"));
        assert!(!is_browser_str("org.mpris.MediaPlayer2.mpv mpv"));
    }

    #[test]
    fn audio_urls_never_match() {
        assert!(is_audio_url("file:///music/Sousou no Frieren - 05.flac"));
        assert!(is_audio_url("file:///music/ost.MP3"));
        assert!(is_audio_url("http://127.0.0.1:8000/stream.opus"));
        assert!(is_audio_url("https://example.com/track.MP3?token=abc#part"));
        assert!(is_audio_url("file:///music/track%2Emp3"));
        assert!(is_audio_url(r"C:\Music\track.FLAC"));
        assert!(!is_audio_url("file:///anime/[Group] Show - 05 [1080p].mkv"));
        assert!(!is_audio_url("https://example.com/show.mkv?name=track.mp3"));
        assert!(!is_audio_url("https://example.com/stream"));
        assert!(!is_audio_url("file:///anime/Show.mp3.mkv"));
        assert!(is_audio_url("file:///music/Show.mkv.mp3"));
        assert!(!is_audio_url("file:///anime/movie.2024"));
    }

    #[test]
    fn audio_only_containers_cannot_supply_recognizable_episode_titles() {
        let db = crate::db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_english: Some("Some Show".into()),
            episodes: Some(12),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let matchers = std::sync::Arc::new(crate::recognize::build_matchers(&db));
        let mut cache = RecognitionCache::default();
        let title = "Some Show - 03";
        let folder = std::env::temp_dir().join(format!("kurisu-audio-only-{}", std::process::id()));
        library::bind_path(&db, folder.to_str().unwrap(), 1, -12).unwrap();
        let snapshot = |path: String, filename: String| mpvipc::MpvSnapshot {
            playing: true,
            path,
            filename,
            media_title: title.into(),
            duration_us: 1_200_000_000,
            position_us: 1_100_000_000,
        };
        let video = snapshot(
            reqwest::Url::from_file_path(folder.join("ep13.mkv"))
                .unwrap()
                .into(),
            "ep13.mkv".into(),
        );
        let recognized =
            mpv_snapshot_info(&PlaybackMatches::new(&db, &matchers, &mut cache), &video);
        assert_eq!(recognized.media_id, Some(1));
        assert_eq!(recognized.matched_title.as_deref(), Some("Some Show"));
        assert_eq!(recognized.episode, Some(1));
        for extension in [
            "mka", "m4b", "aif", "aifc", "ape", "wv", "wvp", "tta", "oga",
        ] {
            let url: String =
                reqwest::Url::from_file_path(folder.join(format!("ep13.{extension}")))
                    .unwrap()
                    .into();
            let https = format!(
                "https://example.com/Some%20Show%20-%2003.{}?token=1#part",
                extension.to_ascii_uppercase()
            );
            for (path, filename) in [
                (url.clone(), "video.mkv".into()),
                (https, "video.mkv".into()),
                (
                    "https://example.com/stream".into(),
                    format!("ep13.{extension}"),
                ),
            ] {
                let info = mpv_snapshot_info(
                    &PlaybackMatches::new(&db, &matchers, &mut cache),
                    &snapshot(path.clone(), filename),
                );
                assert!(info.playing);
                assert_eq!(info.title, title);
                assert_eq!(info.length_us, 1_200_000_000);
                assert_eq!(info.position_us, 1_100_000_000);
                assert_eq!(info.media_id, None, "{path} reached episode recognition");
                assert_eq!(info.matched_title, None);
                assert_eq!(info.episode, None);
            }
            assert!(PlaybackMatches::new(&db, &matchers, &mut cache)
                .match_playing(title, &url)
                .is_none());
        }
        db.set_setting("library_bindings", "{invalid").unwrap();
        let unrecognized =
            mpv_snapshot_info(&PlaybackMatches::new(&db, &matchers, &mut cache), &video);
        assert_eq!(unrecognized.media_id, None);
        assert_eq!(unrecognized.episode, None);
    }

    #[test]
    fn event_payload_field_names_exist_in_types_ts() {
        crate::models::assert_ts_declares("NowPlaying", &serde_json::to_value(idle()).unwrap());
        crate::models::assert_ts_declares(
            "TrackingPrompt",
            &serde_json::to_value(TrackingPrompt {
                media_id: 0,
                episode: 0,
                title: String::new(),
                raw_title: String::new(),
                progress: 0,
            })
            .unwrap(),
        );
    }
}
