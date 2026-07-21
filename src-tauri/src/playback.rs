//! Playback detection. Polls the OS media sessions — MPRIS2 on Linux
//! (MPV/VLC/Celluloid/…), GSMTC on Windows (mpv.net/VLC/…) — every few seconds,
//! matches the playing title against the user's cached list, and — per the
//! tracking mode — either prompts after N minutes of playback or auto-updates
//! progress at X% watched. Other platforms get a no-op stub.
//!
//! Title cleaning / episode parsing / list matching live in `recognize.rs` (shared
//! with the library scanner). Only `read_now` is platform-specific; the payloads,
//! tick state machine, and event flow are identical on every OS.
//!
//! Feedback is in-app only: we emit Tauri events for a "Now Playing" banner and a
//! prompt modal. No desktop / tray notifications, by request.
//!
//! Media-session calls are blocking round-trips (D-Bus / WinRT), so each tick's
//! reads happen inside a `spawn_blocking` task; the accumulated-play state machine
//! + the network push stay on the async runtime.

use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use mpris::{PlaybackStatus, PlayerFinder};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, AppState, TrackingConfig};
#[cfg(target_os = "linux")]
use crate::recognize::basename;
#[cfg_attr(not(any(target_os = "linux", windows)), allow(unused_imports))]
use crate::recognize::{match_title, parse_episode_after, parse_episode_guess};

/// Poll interval. 5s is responsive enough for a 2-minute prompt threshold while
/// keeping D-Bus chatter negligible.
const TICK: Duration = Duration::from_secs(5);
/// Bus-name / identity substrings of MPRIS players we never treat as anime
/// players: web browsers (YouTube & co. in Firefox/Librewolf shouldn't drive the
/// banner or tracking). Covers the Firefox and Chromium families. Matched against
/// the D-Bus bus name + identity on Linux, the source AppUserModelId on Windows.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
const BROWSER_PLAYERS: &[&str] = &[
    "firefox", "librewolf", "mozilla", "zen", "waterfox", "floorp", "chrome", "chromium", "brave",
    "vivaldi", "opera", "edge",
];

// ─────────────────────────── event payloads ───────────────────────────

/// Emitted every tick while something is (or was) playing. `active=false` means
/// playback stopped — the frontend hides the banner.
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

/// Emitted in prompt mode once the threshold is reached for a given track.
/// `progress` is the entry's current local progress, so the modal can offer
/// "set to Ep N" only when that's actually ahead.
#[derive(Serialize, Clone)]
struct TrackingPrompt {
    media_id: i64,
    episode: i64,
    title: String,
    raw_title: String,
    progress: i64,
}

// ─────────────────────────── track state machine ───────────────────────────

/// Per-playing-track state. Reset whenever the MPRIS trackid / title changes.
struct ActiveTrack {
    key: String,
    accumulated: Duration,
    last_tick: Instant,
    was_playing: bool,
    prompted: bool,
    incremented: bool,
}

impl ActiveTrack {
    fn new(key: String) -> Self {
        Self { key, accumulated: Duration::ZERO, last_tick: Instant::now(), was_playing: false, prompted: false, incremented: false }
    }
}

// ─────────────────────────── entrypoint ───────────────────────────

/// Launch the background watcher. Runs for the app's lifetime. Each tick is
/// supervised as its own task: a tick ERROR is logged and skipped, and even a
/// PANIC is caught at the join boundary — it costs the per-track state, but the
/// loop itself keeps running (a single bad tick must not end tracking silently).
///
/// Spawned via `tauri::async_runtime` (not `tokio::spawn`) because the call site
/// is Tauri's `setup()` closure, where no Tokio reactor is entered. The Tauri
/// runtime is Tokio, so `tokio::time::sleep` / `spawn_blocking` work inside it.
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
                    active = None; // per-track state died with the panicked task
                    log::warn!("playback tick panicked (watcher continues): {e}");
                }
            }
        }
    });
}

// ─────────────────────────── tick ───────────────────────────

/// Snapshot of what a player is playing right now (read on a blocking thread).
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
    // Do all blocking D-Bus + DB reads off the async runtime.
    let app_for_blocking = app.clone();
    // `??`: first unwraps the JoinHandle's JoinError, then read_now's anyhow error.
    let info = tokio::task::spawn_blocking(move || read_now(&app_for_blocking)).await??;

    let Some(info) = info else {
        // Nothing playing (or paused-but-nothing): drop the banner, reset state.
        if active.is_some() {
            let _ = app.emit("kurisu://now-playing", idle());
            *active = None;
        }
        return Ok(());
    };

    // Banner — emitted every tick so the progress bar stays live.
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

    // Advance / reset the per-track state machine.
    let key = if !info.trackid.is_empty() { info.trackid.clone() } else { info.title.clone() };
    if active.as_ref().map(|t| &t.key) != Some(&key) {
        *active = Some(ActiveTrack::new(key));
    }
    let track = active.as_mut().unwrap();
    // Credit the interval only when playing at BOTH ticks: sampling every 5s
    // can't see intra-interval pauses, so counting a pause→resume interval in
    // full would reach the prompt threshold early. Under-counting (a slightly
    // late prompt) is the safe direction.
    if info.playing && track.was_playing {
        track.accumulated += track.last_tick.elapsed();
    }
    track.was_playing = info.playing;
    track.last_tick = Instant::now();

    // Tracking only applies once we've matched a list entry AND parsed an episode.
    let Some(media_id) = info.media_id else { return Ok(()) };
    let Some(episode) = info.episode else { return Ok(()) };
    let progress = app
        .state::<AppState>()
        .db
        .get_entry(media_id)
        .ok()
        .flatten()
        .map(|e| e.progress)
        .unwrap_or(0);

    let cfg = read_config(app);
    match cfg.mode.as_str() {
        "prompt" if info.playing => {
            if !track.prompted && track.accumulated >= Duration::from_secs(cfg.prompt_seconds) {
                track.prompted = true;
                let _ = app.emit(
                    "kurisu://tracking-prompt",
                    TrackingPrompt {
                        media_id,
                        episode,
                        title: info.matched_title.clone().unwrap_or_else(|| info.title.clone()),
                        raw_title: info.title.clone(),
                        progress,
                    },
                );
            }
        }
        "auto" if info.playing => {
            let pct = if info.length_us > 0 {
                (info.position_us as f64 / info.length_us as f64) * 100.0
            } else {
                0.0
            };
            // Set progress to the detected episode (never rewind): identical to +1
            // for sequential viewing, catches up on skips, and leaves rewatches alone.
            if !track.incremented && pct >= cfg.auto_percent as f64 && episode > progress {
                let st = app.state::<AppState>();
                // watcher_set_progress re-checks "episode > progress" under the
                // write lock: the user may have rewound while we were deciding.
                // `incremented` is set only once the outcome is known, so a failed
                // push (offline hiccup) retries on a later tick instead of never
                // firing for this track.
                match commands::watcher_set_progress(st.inner(), media_id, episode).await {
                    Ok(Some(entry)) => {
                        track.incremented = true;
                        let _ = app.emit("kurisu://episode-updated", entry);
                    }
                    Ok(None) => track.incremented = true, // rewound past `episode` between check and write
                    Err(e) => log::warn!("auto progress-update of {} failed: {e}", media_id),
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

// ─────────────────────────── blocking reads ───────────────────────────

/// Linux (MPRIS2): find the most relevant player (prefer Playing, fall back to
/// Paused so we don't lose accumulated progress on a pause), read its current
/// track, and match it against the cached list. All blocking.
#[cfg(target_os = "linux")]
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return Ok(None), // no session bus / D-Bus unavailable
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
    let Some((player, playing)) = picked else { return Ok(None) };

    let md = match player.get_metadata() {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };

    let title = md.title().map(|t| t.to_string()).unwrap_or_default();
    let url = md.url().map(|u| u.to_string()).unwrap_or_default();
    let length = md.length().unwrap_or(Duration::ZERO);
    let position = player.get_position().unwrap_or(Duration::ZERO);
    let identity = player.identity().to_string();
    // mpris 2.x's Metadata has no `trackid` accessor, so synthesize a stable
    // per-track key: the file URL is unique per file (which is exactly when we
    // want to reset the tracker), falling back to the title.
    let trackid = if !url.is_empty() { url.clone() } else { title.clone() };

    let state = app.state::<AppState>();
    // Matchers come from the shared cache (rebuilt on every list mutation) —
    // rebuilding them from the DB every 5s tick was the hot path's main cost.
    let matchers = state.matchers.lock().clone();
    let matched = match_title(&matchers, &title, &url);
    let episode = matched
        .and_then(|m| parse_episode_after(&title, &m.variants).or_else(|| parse_episode_after(&basename(&url), &m.variants)))
        .or_else(|| parse_episode_guess(&title))
        .or_else(|| parse_episode_guess(&basename(&url)));

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
}

fn read_config(app: &AppHandle) -> TrackingConfig {
    TrackingConfig::load(&app.state::<AppState>().db)
}

/// True if a player identifier (D-Bus bus name + identity on Linux, source
/// AppUserModelId on Windows) belongs to a web browser — YouTube/Twitch playback
/// must not drive the banner or tracking.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn is_browser_str(id: &str) -> bool {
    let id = id.to_lowercase();
    BROWSER_PLAYERS.iter().any(|b| id.contains(b))
}

#[cfg(target_os = "linux")]
fn is_browser(player: &mpris::Player) -> bool {
    is_browser_str(&format!("{} {}", player.bus_name(), player.identity()))
}

/// Windows: read the Global System Media Transport Controls (GSMTC) sessions —
/// the OS-level "what's playing" API. Same pick policy as MPRIS (Playing first,
/// else Paused). Bare MPV doesn't register with GSMTC; mpv.net and VLC do.
/// GSMTC exposes no file URL, so the title is the only match input and doubles
/// as the track key.
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
    let Some((session, playing)) = picked.or(paused) else { return Ok(None) };

    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let title = props.Title().map(|h| h.to_string_lossy()).unwrap_or_default();
    let player = session
        .SourceAppUserModelId()
        .map(|h| h.to_string_lossy())
        .unwrap_or_default();
    let timeline = session.GetTimelineProperties()?;
    // TimeSpan.Duration is in 100 ns units → microseconds.
    let length_us = timeline.EndTime().map(|t| t.Duration / 10).unwrap_or(0);
    let position_us = timeline.Position().map(|t| t.Duration / 10).unwrap_or(0);

    let state = app.state::<AppState>();
    let matchers = state.matchers.lock().clone();
    let matched = match_title(&matchers, &title, "");
    let episode = matched
        .and_then(|m| parse_episode_after(&title, &m.variants))
        .or_else(|| parse_episode_guess(&title));

    Ok(Some(TickInfo {
        playing,
        player,
        trackid: String::new(), // no URL from GSMTC; tick keys the track by title
        title,
        length_us,
        position_us,
        media_id: matched.map(|m| m.media_id),
        matched_title: matched.map(|m| m.display.clone()),
        episode,
    }))
}

/// Platforms without a media-session API we support (macOS, …): no playback
/// detection. Everything else (AniList sync, library, seasons) works unchanged.
#[cfg(not(any(target_os = "linux", windows)))]
fn read_now(_app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    Ok(None)
}
