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
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff",
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

/// Isolate each tick so a panic cannot stop tracking. Setup requires the Tauri runtime.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut active: Option<ActiveTrack> = None;
        loop {
            tokio::time::sleep(TICK).await;
            let app_tick = app.clone();
            let prev = active.take();
            let joined = tauri::async_runtime::spawn(async move {
                let mut track = prev;
                let result = tick(&app_tick, &mut track).await;
                (track, result)
            })
            .await;
            match joined {
                Ok((track, result)) => {
                    active = track;
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

async fn tick(app: &AppHandle, active: &mut Option<ActiveTrack>) -> anyhow::Result<()> {
    let state = app.state::<AppState>();
    let expected_token = state.anilist.lock().token();
    let app_for_blocking = app.clone();
    let info = tokio::task::spawn_blocking(move || read_now(&app_for_blocking)).await??;
    let session_guard = state.entry_lock.lock().await;
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
        track.accumulated += track.last_tick.elapsed();
    }
    track.was_playing = info.playing;
    track.last_tick = Instant::now();

    let cfg = read_config(app);

    let desired = if cfg.discord_enabled {
        match (info.matched_title.as_ref(), info.media_id) {
            (Some(title), Some(media_id)) => {
                let (cover_url, total_episodes) = app
                    .state::<AppState>()
                    .db
                    .get_entry(media_id)
                    .ok()
                    .flatten()
                    .and_then(|e| e.media)
                    .map(|m| (m.cover_large.or(m.cover_medium), m.episodes))
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
    let progress = app
        .state::<AppState>()
        .db
        .get_entry(media_id)
        .ok()
        .flatten()
        .map(|e| e.progress)
        .unwrap_or(0);

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
        "auto" if info.playing && gate.should_push() => {
            drop(session_guard);
            let st = app.state::<AppState>();
            match commands::watcher_set_progress(
                st.inner(),
                media_id,
                episode,
                expected_token.as_deref(),
            )
            .await
            {
                Ok(Some(entry)) => {
                    track.incremented = true;
                    let _session = st.entry_lock.lock().await;
                    let same_account = st.anilist.lock().token() == expected_token;
                    if same_account {
                        record_history(app, st.inner(), track, &info, expected_token.as_deref());
                        let _ = app.emit("kurisu://episode-updated", entry);
                    }
                }
                Ok(None) => {
                    track.incremented = true;
                    let _session = st.entry_lock.lock().await;
                    if st
                        .db
                        .get_entry(media_id)
                        .ok()
                        .flatten()
                        .is_some_and(|entry| entry.progress >= episode)
                    {
                        record_history(app, st.inner(), track, &info, expected_token.as_deref());
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
    if track.history_recorded
        || expected_token.is_none()
        || state.anilist.lock().token().as_deref() != expected_token
    {
        return;
    }
    let (Some(media_id), Some(episode), Some(path)) =
        (info.media_id, info.episode, local_video_path(&info.trackid))
    else {
        return;
    };
    let Some(account_id) = state
        .db
        .get_setting("anilist_user_id")
        .ok()
        .flatten()
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0)
    else {
        return;
    };
    match state.db.record_watch(account_id, &path, media_id, episode) {
        Ok(()) => {
            track.history_recorded = true;
            let _ = app.emit("kurisu://watch-history-updated", media_id);
        }
        Err(error) => log::warn!("could not save local watch history: {error}"),
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
fn probe_mpv(app: &AppHandle) -> Option<TickInfo> {
    let cfg = read_config(app);
    let mut paths: Vec<String> = Vec::new();
    let custom = cfg.mpv_ipc_socket.trim().to_string();
    if !custom.is_empty() {
        paths.push(custom);
    }
    for p in mpvipc::default_socket_paths() {
        if !paths.contains(&p) {
            paths.push(p);
        }
    }
    preferred_source(mpvipc::probe(&paths).iter().map(|s| mpv_tick_info(app, s)))
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn mpv_tick_info(app: &AppHandle, snap: &mpvipc::MpvSnapshot) -> TickInfo {
    let state = app.state::<AppState>();
    let matchers = state.matchers.lock().clone();
    let title = if snap.media_title.is_empty() {
        snap.filename.clone()
    } else {
        snap.media_title.clone()
    };
    let matched = if is_audio_url(&snap.path) || is_audio_url(&snap.filename) {
        None
    } else {
        match_playing(app, &matchers, &title, &snap.path)
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
fn match_playing<'a>(
    app: &AppHandle,
    matchers: &'a [crate::recognize::Matcher],
    title: &str,
    path: &str,
) -> Option<(
    &'a crate::recognize::Matcher,
    Option<library::LibraryBinding>,
)> {
    let bindings = library::get_bindings(&app.state::<AppState>().db);
    library::bound_match(matchers, &bindings, path)
        .map(|(m, binding)| (m, Some(binding)))
        .or_else(|| match_title(matchers, title, path).map(|m| (m, None)))
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
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    let mut sources = Vec::new();
    if let Ok(finder) = PlayerFinder::new() {
        let state = app.state::<AppState>();
        let matchers = state.matchers.lock().clone();
        for player in finder
            .find_all()
            .unwrap_or_default()
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
            let matched = if is_audio_url(&url) {
                None
            } else {
                match_playing(app, &matchers, &title, &url)
            };
            let base = basename(&url);
            let episode =
                matched.and_then(|(m, binding)| playing_episode(m, &title, &base, binding));
            let position = player.get_position().ok();
            sources.push(TickInfo {
                playing,
                player: player.identity().to_string(),
                trackid: if url.is_empty() { title.clone() } else { url },
                title,
                length_us: position
                    .and(md.length())
                    .unwrap_or(Duration::ZERO)
                    .as_micros() as i64,
                position_us: position.unwrap_or(Duration::ZERO).as_micros() as i64,
                media_id: matched.map(|(m, _)| m.media_id),
                matched_title: matched.map(|(m, _)| m.display.clone()),
                episode,
            });
        }
    }
    sources.extend(probe_mpv(app));
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

#[cfg(windows)]
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as SessionManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    };
    let mut sources = Vec::new();
    let sessions = SessionManager::RequestAsync()
        .and_then(|request| request.join())
        .and_then(|manager| manager.GetSessions());
    if let Ok(sessions) = sessions {
        let state = app.state::<AppState>();
        let matchers = state.matchers.lock().clone();
        for session in sessions {
            let player = session
                .SourceAppUserModelId()
                .map(|h| h.to_string_lossy())
                .unwrap_or_default();
            if is_browser_str(&player) {
                continue;
            }
            let playing = match session.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()) {
                Ok(PlaybackStatus::Playing) => true,
                Ok(PlaybackStatus::Paused) => false,
                _ => continue,
            };
            let title = session
                .TryGetMediaPropertiesAsync()
                .and_then(|op| op.join())
                .and_then(|props| props.Title())
                .map(|h| h.to_string_lossy())
                .unwrap_or_default();
            let timeline = session.GetTimelineProperties().ok();
            let position = timeline
                .as_ref()
                .and_then(|t| t.Position().ok())
                .map(|t| t.Duration / 10)
                .filter(|position| *position >= 0);
            let matched = match_title(&matchers, &title, "");
            let episode =
                matched.and_then(|m| library::resolve_bound_episode(m, &[title.as_str()], None));
            sources.push(TickInfo {
                playing,
                player,
                trackid: String::new(),
                title,
                length_us: timeline
                    .as_ref()
                    .filter(|_| position.is_some())
                    .and_then(|t| t.EndTime().ok())
                    .map(|t| t.Duration / 10)
                    .unwrap_or(0),
                position_us: position.unwrap_or(0),
                media_id: matched.map(|m| m.media_id),
                matched_title: matched.map(|m| m.display.clone()),
                episode,
            });
        }
    }
    sources.extend(probe_mpv(app));
    Ok(preferred_source(sources))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn read_now(_app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let matchers = crate::recognize::build_matchers(&db);
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
