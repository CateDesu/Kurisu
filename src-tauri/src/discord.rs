//! Discord Rich Presence. While a matched episode plays, the local Discord
//! client shows it as the user's activity: the show, the episode, and the
//! time left. Cleared when playback stops or the show is not on the list.
//! Local IPC only, no network. The activity lives and dies with the socket
//! connection, so a crash never leaves a stale presence behind.
//!
//! Presence follows detection, not the tracking mode. Mode off still
//! announces. Only matched list entries are announced since an unmatched
//! title could be anything the player happens to have open.
//!
//! The manager is a static behind a non poisoning mutex so the playback
//! tick can reach it from a blocking thread. All socket I/O is sync and
//! must stay off the async runtime.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, Assets, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use parking_lot::Mutex;

/// Discord application the presence shows under. The name Discord displays
/// comes from the application with this ID in the developer portal.
/// KURISU_DISCORD_CLIENT_ID overrides for development.
const DEFAULT_CLIENT_ID: &str = "1544894569454506124";

/// What the playback tick wants Discord to show. The title is the matched
/// display title from the list, never the raw player title. cover_url is the
/// AniList cover from the media cache, total_episodes is the show's episode
/// count, None while AniList has none announced.
pub struct PresenceInfo {
    pub title: String,
    pub episode: Option<i64>,
    pub playing: bool,
    pub length_us: i64,
    pub position_us: i64,
    pub cover_url: Option<String>,
    pub total_episodes: Option<i64>,
}

/// What Discord currently shows, so steady playback does not rewrite the
/// same activity every 5s tick. Discord throttles activity updates and a
/// rewrite that changes nothing is pure churn.
struct Shown {
    title: String,
    episode: Option<i64>,
    playing: bool,
    /// End timestamp last sent, unix milliseconds. Seeks surface as drift
    /// here while steady playback keeps it nearly constant.
    end_ms: i64,
    /// Compared so a cover or episode count that lands in the cache after
    /// the first announce still makes it to Discord.
    cover_url: Option<String>,
    total_episodes: Option<i64>,
}

/// A seek smaller than this is not worth a rewrite. The countdown Discord
/// renders just drifts by that much until the next real change.
const SEEK_DRIFT_MS: i64 = 30_000;

struct Presence {
    client: Option<DiscordIpcClient>,
    shown: Option<Shown>,
    /// Consecutive connect or write failures and the earliest instant the
    /// next attempt may run. Discord is often simply not running and the
    /// socket path then fails instantly, so without a backoff every tick
    /// would log a failed connect while anything plays.
    fail_count: u32,
    retry_at: Option<Instant>,
}

static PRESENCE: Mutex<Presence> = Mutex::new(Presence {
    client: None,
    shown: None,
    fail_count: 0,
    retry_at: None,
});

/// Same ladder the auto progress push uses. 30s, 2m, 10m.
fn backoff(fail_count: u32) -> Duration {
    match fail_count {
        0 | 1 => Duration::from_secs(30),
        2 => Duration::from_secs(120),
        _ => Duration::from_secs(600),
    }
}

fn client_id() -> String {
    std::env::var("KURISU_DISCORD_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string())
}

/// Drive the presence toward the desired state. None clears. Blocking
/// socket I/O, call it off the async runtime. Cheap when nothing changed.
pub fn update(desired: Option<PresenceInfo>) {
    let mut p = PRESENCE.lock();
    match desired {
        None => p.clear(),
        Some(d) => p.set(&d),
    }
}

impl Presence {
    fn clear(&mut self) {
        if self.shown.take().is_none() {
            return;
        }
        if let Some(c) = self.client.as_mut() {
            // A dead socket here just means Discord already dropped the
            // activity itself. Drop the client so the next set reconnects.
            if c.clear_activity().is_err() {
                self.client = None;
            }
        }
    }

    fn set(&mut self, d: &PresenceInfo) {
        let end_ms = end_timestamp_ms(d);
        if self.client.is_some() && !changed(self.shown.as_ref(), d, end_ms) {
            return;
        }
        if let Some(t) = self.retry_at {
            if Instant::now() < t {
                return;
            }
        }
        if self.client.is_none() {
            let id = client_id();
            if id.is_empty() {
                log::debug!("discord presence: no client id configured, staying dormant");
                self.fail_count += 1;
                self.retry_at = Some(Instant::now() + backoff(self.fail_count));
                return;
            }
            let mut c = DiscordIpcClient::new(&id);
            if let Err(e) = c.connect() {
                self.fail_count += 1;
                self.retry_at = Some(Instant::now() + backoff(self.fail_count));
                log::debug!(
                    "discord presence: connect failed (attempt {}): {e}",
                    self.fail_count
                );
                return;
            }
            self.client = Some(c);
            self.fail_count = 0;
            self.retry_at = None;
        }
        let activity = build_activity(d, end_ms);
        // Provably Some, the block above either connected or returned.
        let Some(client) = self.client.as_mut() else {
            return;
        };
        match client.set_activity(activity) {
            Ok(()) => {
                self.shown = Some(Shown {
                    title: d.title.clone(),
                    episode: d.episode,
                    playing: d.playing,
                    end_ms,
                    cover_url: d.cover_url.clone(),
                    total_episodes: d.total_episodes,
                });
            }
            Err(e) => {
                // The socket died, Discord most likely quit. Drop the client
                // and clear the shown state so the next tick reconnects and
                // resends instead of trusting a presence nobody displays.
                log::debug!("discord presence: write failed, will reconnect: {e}");
                self.client = None;
                self.shown = None;
                self.fail_count += 1;
                self.retry_at = Some(Instant::now() + backoff(self.fail_count));
            }
        }
    }
}

/// True when the desired state differs enough from what Discord shows to
/// justify a rewrite. Anything playing counts as changed when nothing is
/// connected, the caller checks the client separately.
fn changed(shown: Option<&Shown>, d: &PresenceInfo, end_ms: i64) -> bool {
    let Some(s) = shown else {
        return true;
    };
    s.title != d.title
        || s.episode != d.episode
        || s.playing != d.playing
        || (s.end_ms - end_ms).abs() > SEEK_DRIFT_MS
        || s.cover_url != d.cover_url
        || s.total_episodes != d.total_episodes
}

/// Discord timestamps are unix MILLISECONDS, not seconds. Zero means no
/// countdown: paused playback must not keep ticking down on friends'
/// screens, and a player that reports no duration has nothing to count.
fn end_timestamp_ms(d: &PresenceInfo) -> i64 {
    if !d.playing || d.length_us <= 0 || d.position_us >= d.length_us {
        return 0;
    }
    let remaining_ms = (d.length_us - d.position_us) / 1_000;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.as_millis() as i64)
        .unwrap_or(0);
    now_ms + remaining_ms
}

/// The second line under the title. "Episode 6/10" when the show's total
/// is known, "Episode 6/-" when AniList has no count announced, and a bare
/// Watching or Paused when the episode number itself was not parsed.
fn state_line(episode: Option<i64>, total: Option<i64>, playing: bool) -> String {
    let base = match episode {
        Some(ep) => format!("Episode {ep}/{}", total.map(|t| t.to_string()).unwrap_or_else(|| "-".into())),
        None => return if playing { "Watching" } else { "Paused" }.to_string(),
    };
    if playing {
        base
    } else {
        format!("{base} · Paused")
    }
}

fn build_activity(d: &PresenceInfo, end_ms: i64) -> Activity<'static> {
    // Discord rejects fields over 128 chars. Long romaji titles can reach
    // that, so truncate rather than have the whole update rejected.
    let title: String = d.title.chars().take(128).collect();
    let mut activity = Activity::new()
        .details(title.clone())
        .state(state_line(d.episode, d.total_episodes, d.playing));
    // The AniList cover as the card image. Discord fetches external URLs
    // through its own proxy, no asset upload needed. Asset keys and URLs
    // cap at 256 chars, AniList covers sit well under.
    if let Some(url) = &d.cover_url {
        let url: String = url.chars().take(256).collect();
        activity = activity.assets(Assets::new().large_image(url).large_text(title));
    }
    if end_ms > 0 {
        activity = activity.timestamps(Timestamps::new().end(end_ms));
    }
    activity
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24 minute episode, 10 minutes in and playing. The baseline every
    /// case below perturbs by one field.
    fn playing() -> PresenceInfo {
        PresenceInfo {
            title: "Frieren".into(),
            episode: Some(12),
            playing: true,
            length_us: 24 * 60 * 1_000_000,
            position_us: 10 * 60 * 1_000_000,
            cover_url: Some("https://s4.anilist.co/cover.jpg".into()),
            total_episodes: Some(28),
        }
    }

    fn shown_for(d: &PresenceInfo) -> Shown {
        Shown {
            title: d.title.clone(),
            episode: d.episode,
            playing: d.playing,
            end_ms: end_timestamp_ms(d),
            cover_url: d.cover_url.clone(),
            total_episodes: d.total_episodes,
        }
    }

    #[test]
    fn steady_playback_does_not_rewrite() {
        let d = playing();
        let s = shown_for(&d);
        assert!(!changed(Some(&s), &d, end_timestamp_ms(&d)));
    }

    #[test]
    fn first_show_and_any_real_change_rewrite() {
        let d = playing();
        assert!(changed(None, &d, end_ms(&d)));
        let s = shown_for(&d);
        let paused = PresenceInfo {
            playing: false,
            ..playing()
        };
        assert!(changed(Some(&s), &paused, end_ms(&paused)));
        let next_ep = PresenceInfo {
            episode: Some(13),
            ..playing()
        };
        assert!(changed(Some(&s), &next_ep, end_ms(&next_ep)));
        let other_show = PresenceInfo {
            title: "Frieren 2".into(),
            ..playing()
        };
        assert!(changed(Some(&s), &other_show, end_ms(&other_show)));
    }

    #[test]
    fn small_seeks_do_not_rewrite_but_big_ones_do() {
        let d = playing();
        let s = shown_for(&d);
        // Ten seconds of jitter from sampling or a small seek stays under
        // the drift threshold.
        let jittered = end_ms(&d) - 10_000;
        assert!(!changed(Some(&s), &d, jittered));
        // Skipping the opening moves the end by well over the threshold.
        let skipped = end_ms(&d) - 90_000;
        assert!(changed(Some(&s), &d, skipped));
    }

    #[test]
    fn paused_and_unknown_duration_have_no_countdown() {
        assert!(end_ms(&playing()) > 0);
        let paused = PresenceInfo {
            playing: false,
            ..playing()
        };
        assert_eq!(end_ms(&paused), 0);
        let no_length = PresenceInfo {
            length_us: 0,
            position_us: 0,
            ..playing()
        };
        assert_eq!(end_ms(&no_length), 0);
        let at_the_end = PresenceInfo {
            position_us: 24 * 60 * 1_000_000,
            ..playing()
        };
        assert_eq!(end_ms(&at_the_end), 0);
    }

    fn end_ms(d: &PresenceInfo) -> i64 {
        end_timestamp_ms(d)
    }

    #[test]
    fn state_line_shows_episode_out_of_total() {
        assert_eq!(state_line(Some(6), Some(10), true), "Episode 6/10");
        // No total announced on AniList. A dash, not a made up count.
        assert_eq!(state_line(Some(6), None, true), "Episode 6/-");
        assert_eq!(state_line(Some(6), Some(10), false), "Episode 6/10 · Paused");
        // Episode number itself unparseable.
        assert_eq!(state_line(None, Some(10), true), "Watching");
        assert_eq!(state_line(None, None, false), "Paused");
    }

    #[test]
    fn late_cover_or_total_still_updates() {
        // The media cache can fill in after the first announce. Both fields
        // are part of the dedupe key so the card picks them up.
        let d = playing();
        let bare = PresenceInfo {
            cover_url: None,
            total_episodes: None,
            ..playing()
        };
        let s = shown_for(&bare);
        assert!(changed(Some(&s), &d, end_ms(&d)));
    }
}
