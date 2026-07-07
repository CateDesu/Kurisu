//! MPRIS2 playback detection. Polls active media players (MPV/VLC/Celluloid/…)
//! every few seconds, matches the playing title against the user's cached list, and
//! — per the tracking mode — either prompts after N minutes of playback or
//! auto-increments the episode at X% watched.
//!
//! Feedback is in-app only: we emit Tauri events for a "Now Playing" banner and a
//! prompt modal. No desktop / tray notifications, by request.
//!
//! MPRIS calls are blocking D-Bus round-trips, so each tick's reads happen inside a
//! `spawn_blocking` task; the accumulated-play state machine + the network push
//! stay on the async runtime.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use mpris::{PlaybackStatus, PlayerFinder};
use regex::Regex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::commands::{self, AppState, TrackingConfig};
use crate::db::Db;

/// Poll interval. 5s is responsive enough for a 2-minute prompt threshold while
/// keeping D-Bus chatter negligible.
const TICK: Duration = Duration::from_secs(5);
/// Player names we treat as anime players. Empty = accept any MPRIS player.
const _PLAYER_WHITELIST: &[&str] = &[];

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
#[derive(Serialize, Clone)]
struct TrackingPrompt {
    media_id: i64,
    episode: i64,
    title: String,
    raw_title: String,
}

// ─────────────────────────── regex toolkit ───────────────────────────

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(【][^\]\)】]*[\]\)】]").unwrap());
static RE_RES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(1080|720|480|360|2160|1440|4320)p?\b|\b(bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc|x264|h\.?264|avc|aac|flac|10bit|hi10|yuv420)\b").unwrap()
});
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·]?\s*(?:ep(?:isode)?\.?|e|#)?\s*0*\d{1,3}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
static RE_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

/// Resolutions / common bitrates to discard when picking the episode number.
const NOISE_NUMBERS: [i64; 7] = [360, 480, 720, 1080, 1440, 2160, 4320];

// ─────────────────────────── track state machine ───────────────────────────

/// Per-playing-track state. Reset whenever the MPRIS trackid / title changes.
struct ActiveTrack {
    key: String,
    accumulated: Duration,
    last_tick: Instant,
    prompted: bool,
    incremented: bool,
}

impl ActiveTrack {
    fn new(key: String) -> Self {
        Self { key, accumulated: Duration::ZERO, last_tick: Instant::now(), prompted: false, incremented: false }
    }
}

// ─────────────────────────── entrypoint ───────────────────────────

/// Launch the background watcher. Runs for the app's lifetime; never panics —
/// every tick swallows its own errors so a flaky player can't kill detection.
///
/// Spawned via `tauri::async_runtime` (not `tokio::spawn`) because the call site
/// is Tauri's `setup()` closure, where no Tokio reactor is entered. The Tauri
/// runtime is Tokio, so `tokio::time::sleep` / `spawn_blocking` work inside it.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut active: Option<ActiveTrack> = None;
        loop {
            tokio::time::sleep(TICK).await;
            if let Err(e) = tick(&app, &mut active).await {
                log::debug!("playback tick error: {e}");
            }
        }
    });
}

// ─────────────────────────── tick ───────────────────────────

/// Snapshot of what a player is playing right now (read on a blocking thread).
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
    let track = match active {
        Some(t) if t.key == key => t,
        _ => {
            *active = Some(ActiveTrack::new(key));
            active.as_mut().unwrap()
        }
    };
    if info.playing {
        track.accumulated += track.last_tick.elapsed();
    }
    track.last_tick = Instant::now();

    // Tracking only applies once we've matched a list entry AND parsed an episode.
    let Some(media_id) = info.media_id else { return Ok(()) };
    let Some(episode) = info.episode else { return Ok(()) };

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
            if !track.incremented && pct >= cfg.auto_percent as f64 {
                track.incremented = true;
                let st = app.state::<AppState>();
                match commands::increment_inner(st.inner(), media_id).await {
                    Ok(entry) => {
                        let _ = app.emit("kurisu://episode-updated", entry);
                    }
                    Err(e) => log::warn!("auto-increment of {} failed: {e}", media_id),
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

/// Find the most relevant player (prefer Playing, fall back to Paused so we don't
/// lose accumulated progress on a pause), read its current track, and match it
/// against the cached list. All blocking.
fn read_now(app: &AppHandle) -> anyhow::Result<Option<TickInfo>> {
    let finder = match PlayerFinder::new() {
        Ok(f) => f,
        Err(_) => return Ok(None), // no session bus / D-Bus unavailable
    };
    let players = finder.find_all().unwrap_or_default();

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
    let matchers = build_matchers(&state.db);
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

// ─────────────────────────── list matchers ───────────────────────────

struct Matcher {
    media_id: i64,
    display: String,
    variants: Vec<String>, // raw english / romaji / native titles
    norms: Vec<String>,    // normalized variants for comparison
}

fn build_matchers(db: &Db) -> Vec<Matcher> {
    let entries = db.entries_with_media().unwrap_or_default();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(m) = e.media else { continue };
        let mut variants = Vec::new();
        let mut norms = Vec::new();
        for v in [m.title_english.as_deref(), m.title_romaji.as_deref(), m.title_native.as_deref()] {
            if let Some(v) = v {
                if !v.trim().is_empty() {
                    let n = clean_title(v);
                    if !n.is_empty() {
                        variants.push(v.to_string());
                        norms.push(n);
                    }
                }
            }
        }
        if norms.is_empty() {
            continue;
        }
        out.push(Matcher { media_id: m.id, display: m.display_title(), variants, norms });
    }
    out
}

/// Match a now-playing string against the cached list. Tries the raw title first,
/// then the file basename. Returns the first matcher that matches exactly, then
/// falls back to substring containment.
fn match_title<'a>(matchers: &'a [Matcher], title: &str, url: &str) -> Option<&'a Matcher> {
    let candidates = [clean_title(title), clean_title(&basename(url))];
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        // exact normalized match
        if let Some(m) = matchers.iter().find(|m| m.norms.iter().any(|n| n == &cand)) {
            return Some(m);
        }
        // containment (one inside the other)
        if let Some(m) = matchers
            .iter()
            .find(|m| m.norms.iter().any(|n| n.contains(&cand) || cand.contains(n)))
        {
            return Some(m);
        }
    }
    None
}

// ─────────────────────────── parsing helpers ───────────────────────────

/// Normalize a title for fuzzy comparison: lowercase, split on non-alphanumeric,
/// drop the noise tokens, collapse spaces.
fn clean_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    let s = RE_EP_TAIL.replace(&s, "");
    normalize(&s)
}

fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Strip a trailing file extension, if any.
fn strip_ext(s: &str) -> String {
    match s.rfind('.') {
        Some(i) if i > 0 && !s[i..].contains('/') && !s[i..].contains('\\') => s[..i].to_string(),
        _ => s.to_string(),
    }
}

/// Last path segment of a `file://` URL (or any path-ish string), extension
/// stripped and percent-decoded.
fn basename(url: &str) -> String {
    let seg = url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(url);
    let seg = strip_ext(seg);
    percent_decode(&seg)
}

/// Minimal percent-decoding for `%20` etc. Decoded bytes are accumulated and then
/// interpreted as UTF-8 (so multi-byte sequences like `%E3%82%AF` → ク survive,
/// instead of being pushed byte-by-byte as garbage chars).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Once we know which media it is, parse the episode from the *remainder* after the
/// matched title variant is removed — avoids misreading a number in the title
/// itself (e.g. "91 Days").
fn parse_episode_after(playing: &str, variants: &[String]) -> Option<i64> {
    let lp = playing.to_lowercase();
    for v in variants {
        let lv = v.to_lowercase();
        if lv.is_empty() || lp.len() < lv.len() {
            continue;
        }
        if lp.contains(&lv) {
            let remainder = lp.replace(&lv, " ");
            if let Some(n) = parse_last_episode_number(&remainder) {
                return Some(n);
            }
        }
    }
    None
}

/// Fallback: pick the last plausible episode number from a raw string.
fn parse_episode_guess(s: &str) -> Option<i64> {
    parse_last_episode_number(s)
}

/// Last integer that looks like an episode (excludes resolutions and 4-digit years).
fn parse_last_episode_number(s: &str) -> Option<i64> {
    RE_NUM
        .find_iter(s)
        .filter_map(|m| m.as_str().parse::<i64>().ok())
        .filter(|n| !NOISE_NUMBERS.contains(n) && !(*n >= 1930 && *n <= 2099))
        .filter(|n| *n >= 1 && *n <= 9999)
        .last()
}
