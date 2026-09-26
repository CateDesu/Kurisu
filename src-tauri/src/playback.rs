use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use mpris::{PlaybackStatus, PlayerFinder};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, AppState, TrackingConfig};
use crate::discord;
use crate::mpvipc;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(unused_imports))]
use crate::recognize::basename;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(unused_imports))]
use crate::recognize::{match_title, resolve_episode};

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
    key: String,
    accumulated: Duration,
    last_tick: Instant,
    was_playing: bool,
    prompted: bool,
    incremented: bool,
    asked: bool,
    fail_count: u32,
    retry_at: Option<Instant>,
}

impl ActiveTrack {
    fn new(key: String) -> Self {
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
        }
    }
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
            && self.was_playing_before
            && self.accumulated >= min_watch_time(self.length_us)
            && self.retry_due
            && self.fail_count < MAX_AUTO_PUSH_FAILURES
            && self.pct() >= self.auto_percent as f64
            && self.episode > self.progress
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

async fn tick(app: &AppHandle, active: &mut Option<ActiveTrack>) -> anyhow::Result<()> {
    let app_for_blocking = app.clone();
    let info = tokio::task::spawn_blocking(move || read_now(&app_for_blocking)).await??;

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

    let key = if !info.trackid.is_empty() {
        info.trackid.clone()
    } else {
        info.title.clone()
    };
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
        "auto" if info.playing => {
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
            if gate.should_push() {
                let st = app.state::<AppState>();
                match commands::watcher_set_progress(st.inner(), media_id, episode).await {
                    Ok(Some(entry)) => {
                        track.incremented = true;
                        let _ = app.emit("kurisu://episode-updated", entry);
                    }
                    Ok(None) => track.incremented = true,
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
        }
        _ => {}
    }

    Ok(())
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
    mpvipc::probe(&paths).map(|s| mpv_tick_info(app, &s))
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
        match_title(&matchers, &title, &snap.path)
    };
    let base = basename(&snap.path);
    let episode = matched.and_then(|m| resolve_episode(m, &[title.as_str(), base.as_str()]));
    TickInfo {
        playing: snap.playing,
        player: "mpv".into(),
        trackid: snap.path.clone(),
        title,
        length_us: snap.duration_us,
        position_us: snap.position_us,
        media_id: matched.map(|m| m.media_id),
        matched_title: matched.map(|m| m.display.clone()),
        episode,
    }
}

#[cfg(target_os = "linux")]
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };
    let players: Vec<_> = finder
        .find_all()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !is_browser(p))
        .collect();

    let picked = players
        .iter()
        .find(|p| matches!(p.get_playback_status(), Ok(PlaybackStatus::Playing)))
        .map(|p| (p, true))
        .or_else(|| {
            players
                .iter()
                .find(|p| matches!(p.get_playback_status(), Ok(PlaybackStatus::Paused)))
                .map(|p| (p, false))
        });

    // A playing IPC source outranks any paused MPRIS source, including another MPV.
    let mpris_playing = picked
        .as_ref()
        .map(|(_, playing)| *playing)
        .unwrap_or(false);
    let mut ipc: Option<TickInfo> = None;
    if !mpris_playing {
        ipc = probe_mpv(app);
        if ipc.as_ref().is_some_and(|i| i.playing) {
            return Ok(ipc);
        }
    }

    if let Some((player, playing)) = picked {
        // Keep track state through transient metadata failures.
        let md = player.get_metadata().unwrap_or_default();

        let title = md.title().map(|t| t.to_string()).unwrap_or_default();
        let url = md.url().map(|u| u.to_string()).unwrap_or_default();
        let length = md.length().unwrap_or(Duration::ZERO);
        let position = player.get_position().unwrap_or(Duration::ZERO);
        let identity = player.identity().to_string();
        // The file URL supplies a stable track key when MPRIS has no trackid accessor.
        let trackid = if !url.is_empty() {
            url.clone()
        } else {
            title.clone()
        };

        let state = app.state::<AppState>();
        let matchers = state.matchers.lock().clone();
        let matched = if is_audio_url(&url) {
            None
        } else {
            match_title(&matchers, &title, &url)
        };
        let base = basename(&url);
        let episode = matched.and_then(|m| resolve_episode(m, &[title.as_str(), base.as_str()]));

        Ok(Some(TickInfo {
            playing,
            player: identity,
            trackid,
            title,
            length_us: length.as_micros() as i64,
            position_us: position.as_micros() as i64,
            media_id: matched.map(|m| m.media_id),
            matched_title: matched.map(|m| m.display.clone()),
            episode,
        }))
    } else if let Some(info) = ipc {
        Ok(Some(info))
    } else {
        Ok(None)
    }
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

    let manager = SessionManager::RequestAsync()?.join()?;
    let sessions = manager.GetSessions()?;

    let mut paused = None;
    let mut picked = None;
    for session in sessions {
        let aumid = session
            .SourceAppUserModelId()
            .map(|h| h.to_string_lossy())
            .unwrap_or_default();
        if is_browser_str(&aumid) {
            continue;
        }
        match session.GetPlaybackInfo().and_then(|i| i.PlaybackStatus()) {
            Ok(PlaybackStatus::Playing) => {
                picked = Some((session, true));
                break;
            }
            Ok(PlaybackStatus::Paused) if paused.is_none() => {
                paused = Some((session, false));
            }
            _ => {}
        }
    }
    let mut ipc: Option<TickInfo> = None;
    if picked.is_none() {
        ipc = probe_mpv(app);
        if ipc.as_ref().is_some_and(|i| i.playing) {
            return Ok(ipc);
        }
    }

    if let Some((session, playing)) = picked.or(paused) {
        let title = session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.join())
            .and_then(|props| props.Title())
            .map(|h| h.to_string_lossy())
            .unwrap_or_default();
        let player = session
            .SourceAppUserModelId()
            .map(|h| h.to_string_lossy())
            .unwrap_or_default();
        let timeline = session.GetTimelineProperties().ok();
        // WinRT uses 100 ns units. Convert to microseconds.
        let length_us = timeline
            .as_ref()
            .and_then(|t| t.EndTime().ok())
            .map(|t| t.Duration / 10)
            .unwrap_or(0);
        let position_us = timeline
            .as_ref()
            .and_then(|t| t.Position().ok())
            .map(|t| t.Duration / 10)
            .unwrap_or(0);

        let state = app.state::<AppState>();
        let matchers = state.matchers.lock().clone();
        let matched = match_title(&matchers, &title, "");
        let episode = matched.and_then(|m| resolve_episode(m, &[title.as_str()]));

        Ok(Some(TickInfo {
            playing,
            player,
            trackid: String::new(), // GSMTC has no file URL
            title,
            length_us,
            position_us,
            media_id: matched.map(|m| m.media_id),
            matched_title: matched.map(|m| m.display.clone()),
            episode,
        }))
    } else if let Some(info) = ipc {
        Ok(Some(info))
    } else {
        Ok(None)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn read_now(_app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
