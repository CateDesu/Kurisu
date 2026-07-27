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
use crate::recognize::{match_title, resolve_episode};

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
    // Bridges that re-expose another device's or app's media on the bus under
    // their own name, bypassing the browser check above: KDE's browser
    // integration (Firefox/Chrome tabs appear as plasma.browser.integration),
    // KDE Connect (the paired phone's media sessions), and playerctld (mirrors
    // whatever player it last controlled, browsers included).
    "browser", "kdeconnect", "playerctld",
    // Opaque AppUserModelIDs. A name-substring denylist cannot catch a browser
    // whose AUMID carries no name: the classic (non-Store) Firefox installer
    // registers this hash, so YouTube in Firefox would otherwise drive tracking
    // on Windows even though the Linux side catches it via the MPRIS bus name.
    "308046b0af4a39cb", // Firefox, default install path
    "e7cf176e110c211b", // Firefox, common alternate install hash
];

/// Windows GSMTC exposes only an AppUserModelId, so a denylist can never be
/// complete. Anything on this list is treated as a real video player even if a
/// future denylist entry would otherwise match it.
#[cfg_attr(not(windows), allow(dead_code))]
const KNOWN_VIDEO_PLAYERS: &[&str] = &[
    "mpv", "vlc", "mpc-hc", "mpc-be", "potplayer", "celluloid", "haruna", "smplayer",
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
    /// Consecutive failed auto-pushes for this track, and the earliest instant
    /// the next attempt may run. Without these a failing push retries on every
    /// 5s tick for as long as the file plays.
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
            fail_count: 0,
            retry_at: None,
        }
    }
}

/// Give up on a track after this many consecutive failed auto-pushes. The user
/// can still set progress by hand, and the next file starts a fresh track.
const MAX_AUTO_PUSH_FAILURES: u32 = 4;

/// Backoff before retrying a failed auto-push: 30s, 2m, 10m.
fn auto_push_backoff(fail_count: u32) -> Duration {
    match fail_count {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        _ => Duration::from_secs(600),
    }
}

/// Everything the auto arm needs to decide whether to write progress, lifted out
/// of `tick` so the ONE code path that mutates the user's AniList list without
/// asking is testable without D-Bus, a database, or the network.
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
    /// Fraction of the file played, 0.0 when the player reports no duration.
    fn pct(&self) -> f64 {
        if self.length_us > 0 {
            (self.position_us as f64 / self.length_us as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Position alone is NOT evidence of watching. Require that the track was
    /// already playing on the previous tick and has actually played for
    /// `min_watch_time`, so a file that merely RESUMES past the threshold (mpv
    /// watch_later, VLC continue-where-you-left-off) or is seeked straight to the
    /// credits cannot write progress from a single 5s sample.
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

/// Minimum time a track must have actually PLAYED before an auto-push may fire.
/// Position alone is not evidence of watching: mpv's `watch_later` and VLC's
/// "continue where you left off" both reopen a file at the position it was
/// closed at, and a single seek to the credits reaches any percentage instantly.
/// A quarter of the runtime, capped at a minute, so short specials still track.
fn min_watch_time(length_us: i64) -> Duration {
    if length_us <= 0 {
        return Duration::from_secs(60);
    }
    let quarter = Duration::from_micros((length_us / 4) as u64);
    quarter.min(Duration::from_secs(60))
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
    // Provably Some (the branch above just set it if it wasn't), but spelled as
    // a let-else so a future edit to that condition can't turn it into a panic.
    let Some(track) = active.as_mut() else { return Ok(()) };
    // Credit the interval only when playing at BOTH ticks: sampling every 5s
    // can't see intra-interval pauses, so counting a pause→resume interval in
    // full would reach the prompt threshold early. Under-counting (a slightly
    // late prompt) is the safe direction.
    // Capture the PREVIOUS tick's playing state before overwriting it: the auto
    // arm needs "was already playing one tick ago", not "is playing right now".
    let was_playing_before = track.was_playing;
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
            // Set progress to the detected episode (never rewind): identical to +1
            // for sequential viewing, catches up on skips, and leaves rewatches alone.
            if gate.should_push() {
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
    // A known player always wins: "mpv" must not be excluded by a substring that
    // happens to appear in its install path or package family name.
    if KNOWN_VIDEO_PLAYERS.iter().any(|p| id.contains(p)) {
        return false;
    }
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
    let episode = matched.and_then(|m| resolve_episode(m, &[title.as_str()]));

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24-minute episode, played to 90%, on a track that has been running long
    /// enough to count. The baseline every case below perturbs by ONE field.
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
        // The whole point of the accumulated-time gate: a file opened and dragged
        // straight to 90% has position but no playback behind it.
        let g = AutoGate { accumulated: Duration::from_secs(5), ..watched() };
        assert!(!g.should_push());
        // ...and one 5s sample of a file that only just appeared cannot push
        // either, however far into it the player resumed.
        let g = AutoGate {
            was_playing_before: false,
            accumulated: Duration::ZERO,
            ..watched()
        };
        assert!(!g.should_push());
    }

    #[test]
    fn resume_on_open_does_not_complete_a_show() {
        // mpv watch_later / VLC continue-where-you-left-off reopen at the saved
        // position. First tick: playing, already at 97%, nothing accumulated.
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
        // min_watch_time caps at a quarter of the runtime, so a 4-minute short
        // does not need a full minute of playback to count.
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
        // No duration reported: fall back to a flat minute rather than 0.
        assert_eq!(min_watch_time(0), Duration::from_secs(60));
    }

    #[test]
    fn below_threshold_or_already_done_does_not_push() {
        let g = AutoGate { position_us: 24 * 60 * 1_000_000 / 2, ..watched() };
        assert!(!g.should_push(), "50% is under the 80% threshold");
        let g = AutoGate { incremented: true, ..watched() };
        assert!(!g.should_push(), "already pushed for this track");
        let g = AutoGate { episode: 4, progress: 4, ..watched() };
        assert!(!g.should_push(), "never rewinds or rewrites the same episode");
        let g = AutoGate { episode: 2, progress: 9, ..watched() };
        assert!(!g.should_push(), "rewatching an earlier episode must not rewind");
    }

    #[test]
    fn a_player_with_no_duration_never_auto_pushes() {
        // pct() is 0 without a length, so the percentage gate can never open.
        let g = AutoGate { length_us: 0, position_us: 0, ..watched() };
        assert_eq!(g.pct(), 0.0);
        assert!(!g.should_push());
    }

    #[test]
    fn failed_pushes_back_off_and_then_give_up() {
        // Backed off: the retry instant has not arrived yet.
        let g = AutoGate { fail_count: 1, retry_due: false, ..watched() };
        assert!(!g.should_push());
        // Backoff elapsed: try again.
        let g = AutoGate { fail_count: 1, retry_due: true, ..watched() };
        assert!(g.should_push());
        // Too many consecutive failures: stop hammering AniList for this track.
        let g = AutoGate { fail_count: MAX_AUTO_PUSH_FAILURES, retry_due: true, ..watched() };
        assert!(!g.should_push());
        assert!(auto_push_backoff(1) < auto_push_backoff(2));
        assert!(auto_push_backoff(2) < auto_push_backoff(3));
    }

    #[test]
    fn browsers_are_excluded_but_known_players_are_not() {
        assert!(is_browser_str("org.mpris.MediaPlayer2.firefox.instance_1 Firefox"));
        assert!(is_browser_str("308046B0AF4A39CB"), "opaque Firefox AUMID");
        assert!(is_browser_str("Chromium"));
        // Bridges re-exposing browser/phone media under their own name.
        assert!(is_browser_str("org.mpris.MediaPlayer2.plasma.browser.integration Plasma Browser Integration"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.kdeconnect.pixel_7 KDE Connect"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.playerctld playerctld"));
        assert!(!is_browser_str("org.mpris.MediaPlayer2.mpv mpv"));
        assert!(!is_browser_str("io.github.celluloid_player.Celluloid"));
        assert!(!is_browser_str("VLC media player"));
        // A known player wins even when its path contains a denylisted word.
        assert!(!is_browser_str(r"C:\Users\opera\AppData\mpv.net\mpvnet.exe"));
    }

    /// Same drift guard as models.rs, for the event payloads the watcher emits
    /// (mirrored by hand as `NowPlaying` / `TrackingPrompt` in types.ts).
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
