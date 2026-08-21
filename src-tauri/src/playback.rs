//! Playback detection. Polls the OS media session every few seconds, matches
//! the playing title against the cached list, and either prompts after N
//! minutes of playback or auto updates progress at X% watched. MPRIS2 on
//! Linux, GSMTC on Windows. Other platforms get a no-op stub.
//!
//! Title cleaning, episode parsing, and list matching live in recognize.rs
//! and are shared with the library scanner. Only read_now is platform
//! specific. The payloads, tick state machine, and event flow are identical
//! on every OS.
//!
//! Feedback is in-app only. We emit Tauri events for a Now Playing banner
//! and a prompt modal. No desktop or tray notifications, by request.
//!
//! Media session calls are blocking round trips on D-Bus and WinRT, so each
//! tick's reads happen inside a spawn_blocking task. The accumulated play
//! state machine and the network push stay on the async runtime.

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

/// Poll interval. 5s is responsive for a 2min prompt threshold and keeps
/// D-Bus chatter low.
const TICK: Duration = Duration::from_secs(5);
/// How long a matched track must actually play before the auto ask fires
/// and switches the UI to the Currently Watching tab. Short enough to feel
/// instant, long enough that a quick skip or MPRIS blip doesn't trip it.
const AUTO_ASK_DELAY: Duration = Duration::from_secs(15);
/// Substrings of MPRIS player bus names and identities we never treat as
/// anime players. Mostly web browsers. YouTube in Firefox shouldn't drive
/// the banner or tracking. Matched against the D-Bus bus name and identity
/// on Linux, the source AppUserModelId on Windows.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
const BROWSER_PLAYERS: &[&str] = &[
    "firefox", "librewolf", "mozilla", "zen", "waterfox", "floorp", "chrome", "chromium", "brave",
    "vivaldi", "opera", "edge",
    // Bridges that forward another device or app's media on the bus under
    // their own name. KDE browser integration, KDE Connect for paired phones,
    // and playerctld which mirrors whatever it last controlled. These bypass
    // the browser check above.
    "browser", "kdeconnect", "playerctld",
    // Opaque AppUserModelIDs. A name substring denylist can't catch a browser
    // whose AUMID carries no name. The classic non-Store Firefox installer
    // registers this hash, so YouTube in Firefox would drive tracking on
    // Windows even though Linux catches it via the MPRIS bus name.
    "308046b0af4a39cb", // Firefox default install path
    "e7cf176e110c211b", // Firefox alternate install hash
];

/// Windows GSMTC exposes only an AppUserModelId so the denylist can never
/// be complete. Anything here is treated as a real video player even if a
/// future denylist entry would match it.
#[cfg_attr(not(windows), allow(dead_code))]
const KNOWN_VIDEO_PLAYERS: &[&str] = &[
    "mpv", "vlc", "mpc-hc", "mpc-be", "potplayer", "celluloid", "haruna", "smplayer",
];

// ─────────────────────────── payloads ───────────────────────────

/// Emitted every tick while something is or was playing. active=false means
/// playback stopped and the frontend hides the banner.
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

/// Emitted in prompt mode once the threshold is reached for a track.
/// progress is the entry's current local progress, so the modal can offer
/// set to Ep N only when that's ahead.
#[derive(Serialize, Clone)]
struct TrackingPrompt {
    media_id: i64,
    episode: i64,
    title: String,
    raw_title: String,
    progress: i64,
}

// ─────────────────────────── track state ───────────────────────────

/// Per track state. Reset when the MPRIS trackid or title changes.
struct ActiveTrack {
    key: String,
    accumulated: Duration,
    last_tick: Instant,
    was_playing: bool,
    prompted: bool,
    incremented: bool,
    /// The jump to Currently Watching and ask has fired for this track.
    /// Separate from prompted so the two never collide, and from incremented
    /// which is the auto mode push.
    asked: bool,
    /// Consecutive failed auto pushes for this track and the earliest instant
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
            asked: false,
            fail_count: 0,
            retry_at: None,
        }
    }
}

/// Give up on a track after this many consecutive failed auto pushes. The
/// user can still set progress by hand. The next file starts a fresh track.
const MAX_AUTO_PUSH_FAILURES: u32 = 4;

/// Backoff before retrying a failed auto push. 30s, 2m, 10m.
fn auto_push_backoff(fail_count: u32) -> Duration {
    match fail_count {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        _ => Duration::from_secs(600),
    }
}

/// Everything the auto arm needs to decide whether to write progress.
/// Lifted out of tick so the one code path that mutates the user's AniList
/// list without asking is testable without D-Bus, a database, or network.
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
    /// Fraction of the file played. 0.0 when the player reports no duration.
    fn pct(&self) -> f64 {
        if self.length_us > 0 {
            (self.position_us as f64 / self.length_us as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Position alone is not evidence of watching. Require the track was
    /// already playing on the previous tick and has actually played for
    /// min_watch_time. A file that merely resumes past the threshold from mpv
    /// watch_later or VLC continue mode, or is seeked to the credits, can't
    /// write progress from a single 5s sample.
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

/// Minimum time a track must have actually played before an auto push may
/// fire. Position alone is not evidence of watching. mpv watch_later and
/// VLC continue mode both reopen a file at the saved position, and a single
/// seek to the credits reaches any percentage instantly. A quarter of the
/// runtime, capped at a minute, so short specials still track.
fn min_watch_time(length_us: i64) -> Duration {
    if length_us <= 0 {
        return Duration::from_secs(60);
    }
    let quarter = Duration::from_micros((length_us / 4) as u64);
    quarter.min(Duration::from_secs(60))
}

// ─────────────────────────── entrypoint ───────────────────────────

/// Launch the background watcher. Runs for the app's lifetime. Each tick
/// is its own task. A tick error is logged and skipped. Even a panic is
/// caught at the join boundary. It costs the per track state but the loop
/// keeps running. A single bad tick must not end tracking silently.
///
/// Spawned via tauri::async_runtime rather than tokio::spawn because the
/// call site is Tauri's setup closure where no Tokio reactor is entered.
/// The Tauri runtime is Tokio, so tokio::time::sleep and spawn_blocking
/// work inside it.
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
                    active = None; // per track state died with the panicked task
                    log::warn!("playback tick panicked (watcher continues): {e}");
                }
            }
        }
    });
}

// ─────────────────────────── tick ───────────────────────────

/// Snapshot of what a player is playing right now. Read on a blocking thread.
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
    // Do all blocking D-Bus and DB reads off the async runtime.
    let app_for_blocking = app.clone();
    // ??. First unwraps the JoinHandle JoinError, then read_now's anyhow error.
    let info = tokio::task::spawn_blocking(move || read_now(&app_for_blocking)).await??;

    let Some(info) = info else {
        // Nothing playing or paused with nothing. Drop the banner and reset state.
        if active.is_some() {
            let _ = app.emit("kurisu://now-playing", idle());
            *active = None;
        }
        return Ok(());
    };

    // Banner. Emitted every tick so the progress bar stays live.
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

    // Advance or reset the per track state machine.
    let key = if !info.trackid.is_empty() { info.trackid.clone() } else { info.title.clone() };
    if active.as_ref().map(|t| &t.key) != Some(&key) {
        *active = Some(ActiveTrack::new(key));
    }
    // Provably Some since the branch above just set it. Spelled as a let else
    // so a future edit to that condition can't turn it into a panic.
    let Some(track) = active.as_mut() else { return Ok(()) };
    // Only credit the interval when playing at both ticks. We sample every 5s
    // so we can't see pauses within an interval. Under counting is the safe
    // direction since it just means a slightly late prompt.
    // Capture the previous tick's playing state before overwriting it. The auto
    // arm needs was already playing one tick ago, not is playing right now.
    let was_playing_before = track.was_playing;
    if info.playing && track.was_playing {
        track.accumulated += track.last_tick.elapsed();
    }
    track.was_playing = info.playing;
    track.last_tick = Instant::now();

    // Tracking only applies once we've matched a list entry and parsed an episode.
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

    // Auto ask. Independent of mode. After a few seconds of actual playback of
    // a matched episode that's ahead of progress, switch the UI to Currently
    // Watching and prompt. Uses the same accumulated play gate as the other
    // arms. Resume at position alone must not trigger it. Marks prompted too
    // so a later prompt mode cycle doesn't double prompt for the same track.
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
                title: info.matched_title.clone().unwrap_or_else(|| info.title.clone()),
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
            // Set progress to the detected episode and never rewind. Identical
            // to +1 for sequential viewing, catches up on skips, leaves
            // rewatches alone.
            if gate.should_push() {
                let st = app.state::<AppState>();
                // watcher_set_progress checks episode > progress again under
                // the write lock. The user may have rewound while we were
                // deciding. incremented is set only once the outcome is known,
                // so a failed push retries on a later tick instead of never
                // firing for this track.
                match commands::watcher_set_progress(st.inner(), media_id, episode).await {
                    Ok(Some(entry)) => {
                        track.incremented = true;
                        let _ = app.emit("kurisu://episode-updated", entry);
                    }
                        Ok(None) => track.incremented = true, // rewound past episode between check and write
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

/// Linux MPRIS2. Find the most relevant player. Prefer Playing, fall back
/// to Paused so we don't lose accumulated progress on a pause. Read its
/// current track and match against the cached list. All blocking.
#[cfg(target_os = "linux")]
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return Ok(None), // no session bus or D-Bus unavailable
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
    // mpris 2.x Metadata has no trackid accessor, so synthesize a stable per
    // track key. The file URL is unique per file which is exactly when we want
    // to reset the tracker. Falls back to the title.
    let trackid = if !url.is_empty() { url.clone() } else { title.clone() };

    let state = app.state::<AppState>();
    // Matchers come from the shared cache rebuilt on every list mutation.
    // Rebuilding them from the DB every 5s tick was the hot path's main cost.
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

/// True if a player identifier belongs to a web browser. The D-Bus bus name
/// and identity on Linux, the source AppUserModelId on Windows. YouTube and
/// Twitch playback must not drive the banner or tracking.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
fn is_browser_str(id: &str) -> bool {
    let id = id.to_lowercase();
    // A known player always wins. mpv must not be excluded by a substring that
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

/// Windows. Read the Global System Media Transport Controls sessions, the
/// OS level what's playing API. Same pick policy as MPRIS. Playing first,
/// else Paused. Bare MPV doesn't register with GSMTC. mpv.net and VLC do.
/// GSMTC exposes no file URL so the title is the only match input and
/// doubles as the track key.
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

    // One failed property call must not sink the tick. A player that answers
    // playback status but has no media properties or timeline would disable
    // the banner and both tracking modes for as long as its session lives.
    // Degrade to an empty title and zeroed times like the Linux path does.
    // Prompt and auto ask modes need no position data.
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
    // TimeSpan.Duration is in 100 ns units. Divide by 10 for microseconds.
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
        trackid: String::new(), // no URL from GSMTC. tick keys the track by title
        title,
        length_us,
        position_us,
        media_id: matched.map(|m| m.media_id),
        matched_title: matched.map(|m| m.display.clone()),
        episode,
    }))
}

/// Platforms without a media session API we support like macOS. No playback
/// detection. Everything else like AniList sync, library, seasons works
/// unchanged.
#[cfg(not(any(target_os = "linux", windows)))]
fn read_now(_app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24 minute episode played to 90%, on a track that has been running long
    /// enough to count. The baseline every case below perturbs by one field.
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
        // The whole point of the accumulated time gate. A file opened and
        // dragged straight to 90% has position but no playback behind it.
        let g = AutoGate { accumulated: Duration::from_secs(5), ..watched() };
        assert!(!g.should_push());
        // And one 5s sample of a file that only just appeared can't push
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
        // mpv watch_later or VLC continue mode reopen at the saved position.
        // First tick. Playing, already at 97%, nothing accumulated.
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
        // min_watch_time caps at a quarter of the runtime, so a 4 minute short
        // doesn't need a full minute of playback to count.
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
        // No duration reported. Fall back to a flat minute rather than 0.
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
        // Backed off. The retry instant hasn't arrived yet.
        let g = AutoGate { fail_count: 1, retry_due: false, ..watched() };
        assert!(!g.should_push());
        // Backoff elapsed. Try again.
        let g = AutoGate { fail_count: 1, retry_due: true, ..watched() };
        assert!(g.should_push());
        // Too many consecutive failures. Stop hammering AniList for this track.
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
        // Bridges that forward browser or phone media under their own name.
        assert!(is_browser_str("org.mpris.MediaPlayer2.plasma.browser.integration Plasma Browser Integration"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.kdeconnect.pixel_7 KDE Connect"));
        assert!(is_browser_str("org.mpris.MediaPlayer2.playerctld playerctld"));
        assert!(!is_browser_str("org.mpris.MediaPlayer2.mpv mpv"));
        assert!(!is_browser_str("io.github.celluloid_player.Celluloid"));
        assert!(!is_browser_str("VLC media player"));
        // A known player wins even when its path contains a denylisted word.
        assert!(!is_browser_str(r"C:\Users\opera\AppData\mpv.net\mpvnet.exe"));
    }

    /// Same drift guard as models.rs, for the event payloads the watcher emits.
    /// Mirrored by hand as NowPlaying and TrackingPrompt in types.ts.
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
