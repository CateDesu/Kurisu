//! Presence follows detection independently of progress tracking. Announce only matched titles.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, Assets, Timestamps};
use tokio::sync::watch;

#[path = "discord_transport.rs"]
mod transport;

/// KURISU_DISCORD_CLIENT_ID overrides this for development.
const DEFAULT_CLIENT_ID: &str = "1544894569454506124";

#[derive(Clone)]
pub struct PresenceInfo {
    pub title: String,
    pub episode: Option<i64>,
    pub playing: bool,
    pub length_us: i64,
    pub position_us: i64,
    pub cover_url: Option<String>,
    pub total_episodes: Option<i64>,
}

struct Shown {
    title: String,
    episode: Option<i64>,
    playing: bool,
    /// Unix milliseconds. Seeks change this while steady playback leaves it nearly constant.
    end_ms: i64,
    cover_url: Option<String>,
    total_episodes: Option<i64>,
}

/// Ignore small seeks and timing jitter.
const SEEK_DRIFT_MS: i64 = 30_000;

#[derive(Default)]
struct Presence {
    client: Option<transport::Client>,
    shown: Option<Shown>,
    fail_count: u32,
    retry_at: Option<Instant>,
}

static UPDATES: OnceLock<watch::Sender<Option<PresenceInfo>>> = OnceLock::new();
const IPC_DEADLINE: Duration = Duration::from_secs(3);

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

pub fn update(desired: Option<PresenceInfo>) {
    let updates = UPDATES.get_or_init(|| {
        let (tx, rx) = watch::channel(None);
        tauri::async_runtime::spawn(run_worker(
            rx,
            transport::socket_paths(),
            IPC_DEADLINE,
            backoff,
        ));
        tx
    });
    updates.send_replace(desired);
}

async fn run_worker(
    mut updates: watch::Receiver<Option<PresenceInfo>>,
    paths: Vec<PathBuf>,
    deadline: Duration,
    retry_delay: fn(u32) -> Duration,
) {
    let mut presence = Presence::default();
    loop {
        let desired = updates.borrow_and_update().clone();
        tokio::select! {
            changed = updates.changed() => {
                presence.clear();
                if changed.is_err() { break; }
                continue;
            }
            result = tokio::time::timeout(deadline, presence.apply(desired, &paths)) => {
                if !matches!(result, Ok(Ok(()))) {
                    presence.failed(retry_delay);
                    log::debug!("Discord presence failed or timed out");
                }
            }
        }
        loop {
            tokio::select! {
                changed = updates.changed() => {
                    if changed.is_err() { return; }
                    break;
                }
                received = async {
                    match presence.client.as_mut() {
                        Some(client) => client.receive().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let result = match received {
                        Ok(Some(payload)) => {
                            let client = presence.client.as_mut().unwrap();
                            tokio::time::timeout(deadline, client.pong(&payload)).await
                        }
                        Ok(None) => continue,
                        Err(error) => Ok(Err(error)),
                    };
                    if !matches!(result, Ok(Ok(()))) {
                        presence.failed(retry_delay);
                        if updates.changed().await.is_err() { return; }
                        break;
                    }
                }
            }
        }
    }
}

impl Presence {
    fn failed(&mut self, retry_delay: fn(u32) -> Duration) {
        self.clear();
        self.fail_count = self.fail_count.saturating_add(1);
        self.retry_at = Some(Instant::now() + retry_delay(self.fail_count));
    }

    fn clear(&mut self) {
        self.shown = None;
        self.client = None;
    }

    async fn apply(
        &mut self,
        desired: Option<PresenceInfo>,
        paths: &[PathBuf],
    ) -> std::io::Result<()> {
        match desired {
            Some(d) => self.set(&d, paths).await,
            None => {
                self.clear();
                Ok(())
            }
        }
    }

    async fn set(&mut self, d: &PresenceInfo, paths: &[PathBuf]) -> std::io::Result<()> {
        let end_ms = end_timestamp_ms(d);
        if self.client.is_some() && !changed(self.shown.as_ref(), d, end_ms) {
            return Ok(());
        }
        if let Some(t) = self.retry_at {
            if Instant::now() < t {
                return Ok(());
            }
        }
        if self.client.is_none() {
            let id = client_id();
            self.client = Some(transport::Client::connect(paths, &id).await?);
        }
        let activity = build_activity(d, end_ms);
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        match client.set_activity(activity).await {
            Ok(()) => {
                self.shown = Some(Shown {
                    title: d.title.clone(),
                    episode: d.episode,
                    playing: d.playing,
                    end_ms,
                    cover_url: d.cover_url.clone(),
                    total_episodes: d.total_episodes,
                });
                self.fail_count = 0;
                self.retry_at = None;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

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

fn state_line(episode: Option<i64>, total: Option<i64>, playing: bool) -> String {
    let base = match episode {
        Some(ep) => format!(
            "Episode {ep}/{}",
            total.map(|t| t.to_string()).unwrap_or_else(|| "-".into())
        ),
        None => return if playing { "Watching" } else { "Paused" }.to_string(),
    };
    if playing {
        base
    } else {
        format!("{base} · Paused")
    }
}

fn build_activity(d: &PresenceInfo, end_ms: i64) -> Activity<'static> {
    // Discord limits title fields to 128 characters.
    let title: String = d.title.chars().take(128).collect();
    let mut activity = Activity::new().details(title.clone()).state(state_line(
        d.episode,
        d.total_episodes,
        d.playing,
    ));
    // Discord fetches cover URLs through its image proxy.
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

    #[cfg(unix)]
    async fn read_frame(socket: &mut tokio::net::UnixStream) -> serde_json::Value {
        use tokio::io::AsyncReadExt;
        let mut header = [0; 8];
        socket.read_exact(&mut header).await.unwrap();
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        let mut body = vec![0; size];
        socket.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[cfg(unix)]
    async fn ready(socket: &mut tokio::net::UnixStream) {
        use tokio::io::AsyncWriteExt;
        let body = br#"{"evt":"READY"}"#;
        socket.write_all(&1u32.to_le_bytes()).await.unwrap();
        socket
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .unwrap();
        socket.write_all(body).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_handshakes_close_on_deadline_or_disable() {
        use tokio::io::AsyncReadExt;
        for disable in [false, true] {
            let path = std::env::temp_dir()
                .join(format!("kurisu-discord-{}-{disable}", std::process::id()));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let (tx, rx) = watch::channel(Some(playing()));
            let worker = tokio::spawn(run_worker(
                rx,
                vec![path.clone()],
                Duration::from_millis(100),
                backoff,
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                assert_eq!(read_frame(&mut socket).await["v"], 1);
                if disable {
                    tx.send_replace(None);
                }
                assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
                tx.send_replace(None);
                drop(tx);
                worker.await.unwrap();
            })
            .await
            .unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn newer_presence_cancels_a_stalled_handshake_and_can_publish() {
        use tokio::io::AsyncReadExt;
        let path =
            std::env::temp_dir().join(format!("kurisu-discord-latest-{}", std::process::id()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = watch::channel(Some(playing()));
        let worker = tokio::spawn(run_worker(
            rx,
            vec![path.clone()],
            Duration::from_secs(1),
            backoff,
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut stale, _) = listener.accept().await.unwrap();
            read_frame(&mut stale).await;
            let mut latest = playing();
            latest.episode = Some(13);
            tx.send_replace(Some(latest));
            assert_eq!(stale.read(&mut [0]).await.unwrap(), 0);
            let (mut socket, _) = listener.accept().await.unwrap();
            read_frame(&mut socket).await;
            ready(&mut socket).await;
            let activity = read_frame(&mut socket).await;
            assert_eq!(activity["cmd"], "SET_ACTIVITY");
            assert_eq!(activity["args"]["activity"]["state"], "Episode 13/28");
            tx.send_replace(None);
            assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
            drop(tx);
            worker.await.unwrap();
        })
        .await
        .unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disconnected_presence_reconnects_without_a_title_change() {
        let path =
            std::env::temp_dir().join(format!("kurisu-discord-reconnect-{}", std::process::id()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = watch::channel(Some(playing()));
        let worker = tokio::spawn(run_worker(
            rx,
            vec![path.clone()],
            Duration::from_secs(1),
            |_| Duration::ZERO,
        ));
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_frame(&mut socket).await;
                ready(&mut socket).await;
                assert_eq!(read_frame(&mut socket).await["cmd"], "SET_ACTIVITY");
                drop(socket);
                tokio::time::sleep(Duration::from_millis(20)).await;
                tx.send_replace(Some(playing()));
            }
        })
        .await;
        drop(tx);
        worker.await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(result.is_ok(), "unchanged playback must reconnect");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_connections_and_rejected_activity_back_off() {
        for reject in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "kurisu-discord-backoff-{}-{reject}",
                std::process::id()
            ));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let (tx, rx) = watch::channel(Some(playing()));
            let worker = tokio::spawn(run_worker(
                rx,
                vec![path.clone()],
                Duration::from_secs(1),
                |_| Duration::from_secs(30),
            ));
            let (mut socket, _) = listener.accept().await.unwrap();
            read_frame(&mut socket).await;
            ready(&mut socket).await;
            read_frame(&mut socket).await;
            if reject {
                use tokio::io::AsyncWriteExt;
                let body = br#"{"evt":"ERROR"}"#;
                socket.write_all(&1u32.to_le_bytes()).await.unwrap();
                socket
                    .write_all(&(body.len() as u32).to_le_bytes())
                    .await
                    .unwrap();
                socket.write_all(body).await.unwrap();
            } else {
                drop(socket);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            for _ in 0..3 {
                tx.send_replace(Some(playing()));
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), listener.accept())
                        .await
                        .is_err()
                );
            }
            drop(tx);
            worker.await.unwrap();
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn partial_frames_survive_updates_and_ping_payloads_are_echoed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path =
            std::env::temp_dir().join(format!("kurisu-discord-partial-{}", std::process::id()));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = watch::channel(Some(playing()));
        let worker = tokio::spawn(run_worker(
            rx,
            vec![path.clone()],
            Duration::from_secs(1),
            backoff,
        ));
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_frame(&mut socket).await;
            ready(&mut socket).await;
            read_frame(&mut socket).await;
            socket.write_all(&1u32.to_le_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send_replace(Some(PresenceInfo {
                episode: Some(13),
                ..playing()
            }));
            assert_eq!(
                read_frame(&mut socket).await["args"]["activity"]["state"],
                "Episode 13/28"
            );
            let reply = br#"{"cmd":"SET_ACTIVITY"}"#;
            socket
                .write_all(&(reply.len() as u32).to_le_bytes())
                .await
                .unwrap();
            socket.write_all(reply).await.unwrap();
            let ping = b"keepalive";
            socket.write_all(&3u32.to_le_bytes()).await.unwrap();
            socket
                .write_all(&(ping.len() as u32).to_le_bytes())
                .await
                .unwrap();
            socket.write_all(ping).await.unwrap();
            let mut header = [0; 8];
            socket.read_exact(&mut header).await.unwrap();
            assert_eq!(u32::from_le_bytes(header[..4].try_into().unwrap()), 4);
            assert_eq!(
                u32::from_le_bytes(header[4..].try_into().unwrap()),
                ping.len() as u32
            );
            let mut pong = [0; 9];
            socket.read_exact(&mut pong).await.unwrap();
            assert_eq!(&pong, ping);
        })
        .await;
        drop(tx);
        worker.await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_handshakes_are_closed_without_waiting_for_the_claimed_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (index, opcode, size, body) in [
            (0, 1u32, u32::MAX, &b""[..]),
            (1, 2, 0, &b""[..]),
            (2, 1, 15, &br#"{"evt":"ERROR"}"#[..]),
            (3, 3, 100, &b""[..]),
            (4, 4, 100, &b""[..]),
        ] {
            let path = std::env::temp_dir().join(format!(
                "kurisu-discord-invalid-{}-{index}",
                std::process::id()
            ));
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let (tx, rx) = watch::channel(Some(playing()));
            let worker = tokio::spawn(run_worker(
                rx,
                vec![path.clone()],
                Duration::from_secs(10),
                backoff,
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_frame(&mut socket).await;
                socket.write_all(&opcode.to_le_bytes()).await.unwrap();
                socket.write_all(&size.to_le_bytes()).await.unwrap();
                socket.write_all(body).await.unwrap();
                assert_eq!(socket.read(&mut [0]).await.unwrap(), 0);
                drop(tx);
                worker.await.unwrap();
            })
            .await
            .unwrap();
            std::fs::remove_file(path).unwrap();
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
        let jittered = end_ms(&d) - 10_000;
        assert!(!changed(Some(&s), &d, jittered));
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
        assert_eq!(state_line(Some(6), None, true), "Episode 6/-");
        assert_eq!(
            state_line(Some(6), Some(10), false),
            "Episode 6/10 · Paused"
        );
        assert_eq!(state_line(None, Some(10), true), "Watching");
        assert_eq!(state_line(None, None, false), "Paused");
    }

    #[test]
    fn late_cover_or_total_still_updates() {
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
