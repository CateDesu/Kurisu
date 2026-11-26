use super::*;
use crate::{Db, ListEntry, Media, Platform, User};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_PROFILE: AtomicU64 = AtomicU64::new(0);

struct LocalPlatform;

impl Platform for LocalPlatform {
    fn open_url(&self, _: &str) -> Result<(), String> {
        panic!("unexpected external URL")
    }
    fn open_path(&self, _: &Path) -> Result<(), String> {
        panic!("unexpected external path")
    }
    fn reveal_path(&self, _: &Path) -> Result<(), String> {
        panic!("unexpected external reveal")
    }
}

struct Profile(PathBuf);

impl Drop for Profile {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        } else {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

struct Fixture {
    core: Core,
    profile: Profile,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = NEXT_PROFILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kurisu-config-{}-{seq}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        let db = Db::open(&path.join("kurisu.db")).unwrap();
        db.replace_account(
            "test-session",
            &User {
                id: 7,
                name: "Fixture".into(),
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("Controlled series".into()),
            episodes: Some(12),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            id: Some(22),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 1,
            ..Default::default()
        })
        .unwrap();
        let core = Core::from_db(
            db,
            path.clone(),
            tokio::runtime::Handle::current(),
            Arc::new(LocalPlatform),
        );
        let fixture = Self {
            core,
            profile: Profile(path),
        };
        let state = fixture.core.state();
        let sample = fixture.sample();
        assert!(state.anilist.lock().has_token());
        let matched =
            crate::recognize::match_title(&state.matchers.lock(), &sample.title, &sample.location)
                .expect("fixture playback title must match")
                .media_id;
        assert_eq!(Some(matched), sample.media_id);
        fixture
    }

    fn sample(&self) -> TickInfo {
        TickInfo {
            playing: true,
            eof_reached: false,
            player: "controlled".into(),
            player_id: None,
            player_track_id: None,
            mpv_endpoint: None,
            process_id: None,
            mpv_entry_id: None,
            location: self
                .profile
                .0
                .join("episode02.mkv")
                .to_str()
                .unwrap()
                .into(),
            title: "Controlled series 02".into(),
            length_us: 100_000_000,
            position_us: 90_000_000,
            media_id: Some(1),
            matched_title: Some("Controlled series".into()),
            episode: Some(2),
        }
    }

    fn fail_read(&self) -> String {
        self.core
            .state()
            .db
            .0
            .lock()
            .execute_batch("UPDATE settings SET value=X'00' WHERE key='tracking_prompt_seconds'")
            .unwrap();
        let error = self
            .core
            .state()
            .db
            .get_settings_batch(&["tracking_prompt_seconds"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("Invalid column type Blob"), "{error}");
        assert_eq!(
            self.core
                .state()
                .db
                .get_setting("tracking_discord")
                .unwrap()
                .as_deref(),
            Some("0")
        );
        error
    }

    fn repair_read(&self) {
        self.core
            .state()
            .db
            .set_setting("tracking_prompt_seconds", "91")
            .unwrap();
    }

    async fn tick(
        &self,
        work: &PlaybackWork,
        active: &mut Option<ActiveTrack>,
    ) -> anyhow::Result<()> {
        self.tick_sample(work, active, Some(self.sample())).await
    }

    async fn tick_sample(
        &self,
        work: &PlaybackWork,
        active: &mut Option<ActiveTrack>,
        sample: Option<TickInfo>,
    ) -> anyhow::Result<()> {
        let mut cache = RecognitionCache {
            probe_result: Some(sample),
            ..Default::default()
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            tick(&self.core, work, active, &mut cache),
        )
        .await
        .unwrap()
    }
}

fn original(mode: &str) -> TrackingConfig {
    TrackingConfig {
        mode: mode.into(),
        prompt_seconds: 91,
        auto_percent: 77,
        auto_ask: false,
        mpv_ipc_socket: "/controlled-only.sock".into(),
        discord_enabled: false,
    }
}

fn value(config: &TrackingConfig) -> Value {
    serde_json::to_value(config).unwrap()
}

fn tracking_events(events: &mut tokio::sync::broadcast::Receiver<crate::Event>) -> Vec<EventKind> {
    std::iter::from_fn(|| events.try_recv().ok())
        .map(|event| event.kind)
        .filter(|kind| {
            matches!(
                kind,
                EventKind::TrackingAsk(_) | EventKind::TrackingPrompt(_)
            )
        })
        .collect()
}

#[tokio::test]
async fn tracking_read_errors_preserve_opt_outs_and_recover() {
    let f = Fixture::new();
    let saved = original("prompt");
    saved.save(&f.core.state().db).unwrap();
    let mut presence = f.core.discord_updates.subscribe();
    let mut events = f.core.subscribe();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick(&work, &mut active).await.unwrap();
    assert!(presence.borrow_and_update().is_none());
    assert!(tracking_events(&mut events).is_empty());

    let mut enabled = saved.clone();
    enabled.discord_enabled = true;
    enabled.save(&f.core.state().db).unwrap();
    f.tick(&work, &mut active).await.unwrap();
    assert_eq!(
        presence.borrow_and_update().as_ref().unwrap().title,
        "Controlled series"
    );
    saved.save(&f.core.state().db).unwrap();
    active.as_mut().unwrap().accumulated = Duration::from_secs(20);
    let expected = f.fail_read();
    let error = f.tick(&work, &mut active).await.unwrap_err();
    assert!(presence.has_changed().unwrap());
    assert!(
        presence.borrow_and_update().is_none(),
        "read failure enabled Discord"
    );
    assert!(
        tracking_events(&mut events).is_empty(),
        "read failure enabled auto ask"
    );
    assert_eq!(error.to_string(), expected);
    let track = active.as_ref().unwrap();
    assert!(!track.asked && track.prompt_watch.is_none() && track.push.is_none());
    assert!(track.accumulated >= Duration::from_secs(20));

    f.repair_read();
    f.tick(&work, &mut active).await.unwrap();
    assert!(presence.borrow_and_update().is_none());
    assert!(tracking_events(&mut events).is_empty());
    active.as_mut().unwrap().accumulated = Duration::from_secs(95);
    let expected = f.fail_read();
    assert_eq!(
        f.tick(&work, &mut active).await.unwrap_err().to_string(),
        expected
    );
    assert!(active.as_ref().unwrap().prompt_watch.is_none());
    assert!(tracking_events(&mut events).is_empty());
    f.repair_read();
    f.tick(&work, &mut active).await.unwrap();
    assert!(
        matches!(tracking_events(&mut events).as_slice(), [EventKind::TrackingPrompt(prompt)] if prompt.media_id == 1 && prompt.episode == 2)
    );
    assert!(presence.borrow_and_update().is_none());
    assert_eq!(
        value(&f.core.try_get_tracking_config().await.unwrap()),
        value(&saved)
    );
    assert_eq!(f.core.state().db.get_entry(1).unwrap().unwrap().progress, 1);
    tokio::time::timeout(Duration::from_secs(5), work.wait_idle())
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_tracking_keys_still_enable_default_optional_actions() {
    let f = Fixture::new();
    let mut presence = f.core.discord_updates.subscribe();
    let mut events = f.core.subscribe();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick(&work, &mut active).await.unwrap();
    assert!(presence.borrow_and_update().is_some());
    active.as_mut().unwrap().accumulated = Duration::from_secs(20);
    f.tick(&work, &mut active).await.unwrap();
    assert!(
        matches!(tracking_events(&mut events).as_slice(), [EventKind::TrackingAsk(prompt)] if prompt.episode == 2)
    );
    assert!(active.as_ref().unwrap().push.is_none());
    tokio::time::timeout(Duration::from_secs(5), work.wait_idle())
        .await
        .unwrap();
}

#[tokio::test]
async fn tracking_load_defaults_and_legacy_contract_are_unchanged() {
    let f = Fixture::new();
    let db = &f.core.state().db;
    let defaults = value(&TrackingConfig::default());
    assert_eq!(
        value(&f.core.try_get_tracking_config().await.unwrap()),
        defaults
    );
    for (seconds, percent) in [("bad", "101"), ("0", "0"), ("-1", "invalid")] {
        db.set_settings(&[
            ("tracking_auto_ask", "0"),
            ("tracking_discord", "0"),
            ("tracking_prompt_seconds", seconds),
            ("tracking_auto_percent", percent),
        ])
        .unwrap();
        let expected = TrackingConfig {
            auto_ask: false,
            discord_enabled: false,
            ..TrackingConfig::default()
        };
        assert_eq!(
            value(&TrackingConfig::try_load(db).unwrap()),
            value(&expected)
        );
    }
    let saved = original("auto");
    saved.save(db).unwrap();
    assert_eq!(value(&f.core.get_tracking_config().await), value(&saved));
    f.fail_read();
    assert!(f.core.try_get_tracking_config().await.is_err());
    assert_eq!(value(&TrackingConfig::load(db)), defaults);
    assert_eq!(value(&f.core.get_tracking_config().await), defaults);
    f.repair_read();
    assert_eq!(
        value(&f.core.try_get_tracking_config().await.unwrap()),
        value(&saved)
    );
}

#[tokio::test]
async fn failed_tracking_read_defers_new_writes_but_preserves_accepted_write() {
    let f = Fixture::new();
    std::fs::write(f.sample().location, []).unwrap();
    let state = f.core.state();
    let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
    let (mut api, mut requests) = crate::anilist::mock_api(vec![
        (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":remote(1)}}}),
        ),
        (200, json!({"data":{"SaveMediaListEntry":remote(2)}})),
    ])
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    original("auto").save(&state.db).unwrap();
    let work = PlaybackWork::default();
    let mut track = ActiveTrack::new(f.sample().key(state.anilist.lock().token()), work.clone());
    track.was_playing = true;
    track.accumulated = Duration::from_secs(60);
    let mut active = Some(track);
    f.fail_read();
    assert!(f.tick(&work, &mut active).await.is_err());
    assert!(active.as_ref().unwrap().push.is_none());
    assert!(requests.try_recv().is_err());

    let remote_guard = state.remote_lock.lock().await;
    f.repair_read();
    f.tick(&work, &mut active).await.unwrap();
    let task_id = active.as_ref().unwrap().push.as_ref().unwrap().id();
    f.fail_read();
    assert!(f.tick(&work, &mut active).await.is_err());
    let push = active.as_ref().unwrap().push.as_ref().unwrap();
    assert_eq!(push.id(), task_id);
    assert!(!push.is_finished());
    drop(remote_guard);
    tokio::time::timeout(Duration::from_secs(5), work.wait_idle())
        .await
        .unwrap();
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
    let sent = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["variables"]["progress"], 2);
    assert_eq!(state.db.watch_history(7, 10, None).unwrap().len(), 1);
    assert!(f.tick(&work, &mut active).await.is_err());
    assert!(active
        .as_ref()
        .unwrap()
        .push
        .as_ref()
        .unwrap()
        .is_finished());
    f.repair_read();
    f.tick(&work, &mut active).await.unwrap();
    let track = active.as_ref().unwrap();
    assert!(
        track.incremented && track.history_recorded.load(Ordering::Relaxed) && track.push.is_none()
    );
    assert!(requests.try_recv().is_err());
    assert!(state.db.pending(7).unwrap().is_empty());
}

#[tokio::test]
async fn playback_switch_and_stop_retire_an_offer_before_its_preflight_finishes() {
    for switch_endpoint in [false, true] {
        let f = Fixture::new();
        let state = f.core.state();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (mut api, mut requests) = crate::anilist::mock_api_gated(
            vec![(
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":{
                    "id":22,"mediaId":1,"status":"CURRENT","progress":1,"repeat":0,"score":0
                }}}}),
            )],
            Some(gate.clone()),
        )
        .await;
        api.record_viewer(7);
        *state.anilist.lock() = api;
        TrackingConfig {
            prompt_seconds: 1,
            ..original("prompt")
        }
        .save(&state.db)
        .unwrap();
        let work = PlaybackWork::default();
        let mut active = None;
        let mut sample = f.sample();
        sample.mpv_endpoint = Some("first".into());
        f.tick_sample(&work, &mut active, Some(sample.clone()))
            .await
            .unwrap();
        active.as_mut().unwrap().accumulated = Duration::from_secs(2);
        f.tick_sample(&work, &mut active, Some(sample.clone()))
            .await
            .unwrap();
        let offer = state.retained_tracking_prompt().unwrap();
        sample.playing = false;
        f.tick_sample(&work, &mut active, Some(sample.clone()))
            .await
            .unwrap();
        assert!(state.event_is_current(&offer));

        let core = f.core.clone();
        let confirm = tokio::spawn(async move { core.confirm_tracking_prompt(offer, 2).await });
        let lookup = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lookup["variables"]["mediaId"], 1);
        sample.playing = true;
        sample.mpv_endpoint = Some("second".into());
        f.tick_sample(&work, &mut active, switch_endpoint.then_some(sample))
            .await
            .unwrap();
        assert_eq!(active.is_some(), switch_endpoint);
        if let Some(track) = active {
            assert_eq!(track.accumulated, Duration::ZERO);
        }
        assert!(state.retained_tracking_prompt().is_none());
        gate.add_permits(1);
        assert!(tokio::time::timeout(Duration::from_secs(5), confirm)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 1);
        assert!(state.db.watch_history(7, 10, None).unwrap().is_empty());
    }
}

#[tokio::test]
async fn an_independent_player_cannot_finish_watch_history_for_mpv() {
    for ipc_first in [true, false] {
        let f = Fixture::new();
        let state = f.core.state();
        original("auto").save(&state.db).unwrap();
        let mut mpv = f.sample();
        std::fs::write(&mpv.location, []).unwrap();
        mpv.episode = Some(1);
        mpv.process_id = Some(8);
        mpv.mpv_endpoint = Some("fixture-socket".into());
        let mut vlc = f.sample();
        vlc.episode = Some(1);
        vlc.player = "VLC".into();
        vlc.player_id = Some("org.mpris.MediaPlayer2.vlc :1.9".into());
        vlc.location = reqwest::Url::from_file_path(&mpv.location).unwrap().into();
        let (first, second) = if ipc_first { (mpv, vlc) } else { (vlc, mpv) };
        let work = PlaybackWork::default();
        let mut active = None;
        f.tick_sample(&work, &mut active, Some(first))
            .await
            .unwrap();
        active.as_mut().unwrap().accumulated = Duration::from_secs(25);
        f.tick_sample(&work, &mut active, Some(second.clone()))
            .await
            .unwrap();
        assert_eq!(active.as_ref().unwrap().accumulated, Duration::ZERO);
        assert!(state.db.watch_history(7, 10, None).unwrap().is_empty());

        active.as_mut().unwrap().accumulated = Duration::from_secs(25);
        f.tick_sample(&work, &mut active, Some(second))
            .await
            .unwrap();
        assert_eq!(state.db.watch_history(7, 10, None).unwrap().len(), 1);
    }
}

#[tokio::test]
async fn mpv_eof_can_finish_at_100_percent_without_adding_watch_time() {
    for (earned, same_player, expected_progress) in [(24, true, 1), (25, false, 1), (25, true, 2)] {
        let f = Fixture::new();
        std::fs::write(f.sample().location, []).unwrap();
        let state = f.core.state();
        let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
        let (mut api, mut requests) = crate::anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(1)}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":remote(2)}})),
        ])
        .await;
        api.record_viewer(7);
        *state.anilist.lock() = api;
        TrackingConfig {
            auto_percent: 100,
            ..original("auto")
        }
        .save(&state.db)
        .unwrap();

        let work = PlaybackWork::default();
        let mut active = None;
        let mut sample = f.sample();
        let endpoint = "fixture-socket".to_string();
        sample.mpv_endpoint = Some(if same_player {
            endpoint.clone()
        } else {
            "independent-socket".into()
        });
        sample.position_us = 79_000_000;
        f.tick_sample(&work, &mut active, Some(sample.clone()))
            .await
            .unwrap();
        let track = active.as_mut().unwrap();
        track.accumulated = Duration::from_secs(earned);
        track.last_tick -= TICK;
        sample.playing = false;
        sample.eof_reached = true;
        sample.mpv_endpoint = Some(endpoint);
        sample.position_us = 99_000_000;
        f.tick_sample(&work, &mut active, Some(sample))
            .await
            .unwrap();
        assert_eq!(
            active.as_ref().unwrap().accumulated,
            Duration::from_secs(if same_player { earned } else { 0 })
        );
        drop(active);
        tokio::time::timeout(Duration::from_secs(5), work.join_retired(false))
            .await
            .unwrap();

        assert_eq!(
            state.db.get_entry(1).unwrap().unwrap().progress,
            expected_progress
        );
        let advanced = expected_progress == 2;
        assert_eq!(
            state.db.watch_history(7, 10, None).unwrap().len(),
            usize::from(advanced)
        );
        assert_eq!(
            std::iter::from_fn(|| requests.try_recv().ok()).count(),
            if advanced { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn an_mpv_eof_retry_keeps_earned_watch_time_without_opening_a_prompt() {
    let f = Fixture::new();
    std::fs::write(f.sample().location, []).unwrap();
    let state = f.core.state();
    TrackingConfig {
        auto_percent: 80,
        auto_ask: true,
        ..original("auto")
    }
    .save(&state.db)
    .unwrap();
    let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
    let (mut api, mut requests) = crate::anilist::mock_api(vec![
        (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":remote(1)}}}),
        ),
        (200, json!({"data":{"SaveMediaListEntry":remote(2)}})),
    ])
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    let work = PlaybackWork::default();
    let mut active = None;
    let mut sample = f.sample();
    sample.mpv_endpoint = Some("fixture-socket".into());
    sample.position_us = 79_000_000;
    f.tick_sample(&work, &mut active, Some(sample.clone()))
        .await
        .unwrap();
    let track = active.as_mut().unwrap();
    track.accumulated = Duration::from_secs(25);
    track.fail_count = 1;
    track.retry_at = Some(Instant::now() + TICK);
    sample.playing = false;
    sample.eof_reached = true;
    sample.position_us = 99_000_000;
    f.tick_sample(&work, &mut active, Some(sample.clone()))
        .await
        .unwrap();
    let track = active.as_mut().unwrap();
    assert!(track.push.is_none());
    track.retry_at = Some(Instant::now());
    track.last_tick -= TICK;
    f.tick_sample(&work, &mut active, Some(sample))
        .await
        .unwrap();
    let track = active.as_ref().unwrap();
    assert_eq!(track.accumulated, Duration::from_secs(25));
    assert!(state.retained_tracking_prompt().is_none());
    assert!(track.push.is_some());
    tokio::time::timeout(Duration::from_secs(5), work.wait_idle())
        .await
        .unwrap();
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
    assert_eq!(state.db.watch_history(7, 10, None).unwrap().len(), 1);
    assert_eq!(std::iter::from_fn(|| requests.try_recv().ok()).count(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn history_preserves_a_playable_alias_for_utf8_and_non_utf8_targets() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};

    for target in [b"recording.mkv".as_slice(), b"recording-\xff.mkv"] {
        let f = Fixture::new();
        let mut sample = f.sample();
        let target = f.profile.0.join(std::ffi::OsStr::from_bytes(target));
        std::fs::write(&target, []).unwrap();
        symlink(target, &sample.location).unwrap();
        sample.episode = Some(1);
        original("auto").save(&f.core.state().db).unwrap();

        let work = PlaybackWork::default();
        let mut active = None;
        f.tick_sample(&work, &mut active, Some(sample.clone()))
            .await
            .unwrap();
        active.as_mut().unwrap().accumulated = Duration::from_secs(25);
        let expected_path = sample.location.clone();
        f.tick_sample(&work, &mut active, Some(sample))
            .await
            .unwrap();

        let rows = f.core.get_watch_history(None, None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, expected_path);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_pending_prompt_uses_a_utf8_alias_reported_after_it_qualified() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};

    let f = Fixture::new();
    let state = f.core.state();
    let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
    let (mut api, _) = crate::anilist::mock_api(vec![
        (
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":remote(1)}}}),
        ),
        (200, json!({"data":{"SaveMediaListEntry":remote(2)}})),
    ])
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    TrackingConfig {
        prompt_seconds: 1,
        ..original("prompt")
    }
    .save(&state.db)
    .unwrap();

    let mut sample = f.sample();
    let alias = PathBuf::from(&sample.location);
    let target = f
        .profile
        .0
        .join(std::ffi::OsStr::from_bytes(b"video-\xff.mkv"));
    std::fs::write(&target, []).unwrap();
    symlink(&target, &alias).unwrap();
    sample.location = reqwest::Url::from_file_path(target).unwrap().into();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick_sample(&work, &mut active, Some(sample.clone()))
        .await
        .unwrap();
    active.as_mut().unwrap().accumulated = Duration::from_secs(25);
    f.tick_sample(&work, &mut active, Some(sample))
        .await
        .unwrap();
    let offer = state.retained_tracking_prompt().unwrap();
    f.tick(&work, &mut active).await.unwrap();
    f.core.confirm_tracking_prompt(offer, 2).await.unwrap();

    let rows = f.core.get_watch_history(None, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(Path::new(&rows[0].path), alias);
}

#[cfg(unix)]
#[tokio::test]
async fn delayed_automatic_history_keeps_the_sampled_file_after_stop_and_retargeting() {
    assert_delayed_history(false, WatchQualification::BeforeRequest).await;
}

#[cfg(unix)]
#[tokio::test]
async fn delayed_prompt_history_keeps_the_sampled_file_after_stop_and_retargeting() {
    for qualification in [
        WatchQualification::BeforeRequest,
        WatchQualification::DuringPreflight,
        WatchQualification::DuringMutation,
        WatchQualification::AfterSwitch,
        WatchQualification::Never,
    ] {
        assert_delayed_history(true, qualification).await;
    }
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum WatchQualification {
    BeforeRequest,
    DuringPreflight,
    DuringMutation,
    AfterSwitch,
    Never,
}

#[cfg(unix)]
async fn assert_delayed_history(confirm_prompt: bool, qualification: WatchQualification) {
    use std::os::unix::fs::symlink;

    let f = Fixture::new();
    let state = f.core.state();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let remote = |progress| json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"repeat":0,"score":0});
    let (mut api, mut requests) = crate::anilist::mock_api_gated(
        vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(1)}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":remote(2)}})),
        ],
        Some(gate.clone()),
    )
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    TrackingConfig {
        prompt_seconds: 1,
        ..original(if confirm_prompt { "prompt" } else { "auto" })
    }
    .save(&state.db)
    .unwrap();
    let alias = PathBuf::from(f.sample().location);
    let watched = f.profile.0.join("watched.mkv");
    let replacement = f.profile.0.join("replacement.mkv");
    std::fs::write(&watched, []).unwrap();
    std::fs::write(&replacement, []).unwrap();
    symlink(&watched, &alias).unwrap();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick(&work, &mut active).await.unwrap();
    active.as_mut().unwrap().accumulated =
        Duration::from_secs(if qualification == WatchQualification::BeforeRequest {
            60
        } else {
            2
        });
    f.tick(&work, &mut active).await.unwrap();
    let confirm = confirm_prompt.then(|| {
        let offer = state.retained_tracking_prompt().unwrap();
        let core = f.core.clone();
        tokio::spawn(async move { core.confirm_tracking_prompt(offer, 2).await })
    });
    let first = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["variables"]["mediaId"], 1);
    if qualification == WatchQualification::DuringPreflight {
        active.as_mut().unwrap().accumulated = Duration::from_secs(60);
        f.tick(&work, &mut active).await.unwrap();
    }
    gate.add_permits(1);
    let second = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second["variables"]["progress"], 2);
    if qualification == WatchQualification::DuringMutation {
        active.as_mut().unwrap().accumulated = Duration::from_secs(60);
        f.tick(&work, &mut active).await.unwrap();
    }
    if qualification == WatchQualification::AfterSwitch {
        let mut other = f.sample();
        other.location = replacement.to_str().unwrap().into();
        f.tick_sample(&work, &mut active, Some(other.clone()))
            .await
            .unwrap();
        active.as_mut().unwrap().accumulated = Duration::from_secs(60);
        f.tick_sample(&work, &mut active, Some(other))
            .await
            .unwrap();
        assert!(state.retained_tracking_prompt().is_some());
    }

    std::fs::remove_file(&alias).unwrap();
    symlink(&replacement, &alias).unwrap();
    f.tick_sample(&work, &mut active, None).await.unwrap();
    assert!(active.is_none());
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), work.join_retired(false))
        .await
        .unwrap();
    if let Some(confirm) = confirm {
        tokio::time::timeout(Duration::from_secs(5), confirm)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
    let history = state.db.watch_history(7, 10, None).unwrap();
    assert_eq!(
        history.len(),
        usize::from(matches!(
            qualification,
            WatchQualification::BeforeRequest
                | WatchQualification::DuringPreflight
                | WatchQualification::DuringMutation
        )),
        "{qualification:?}"
    );
    if let Some(entry) = history.first() {
        assert_eq!(Path::new(&entry.path), watched.canonicalize().unwrap());
    }
}

#[tokio::test]
async fn manual_progress_records_history_only_after_qualifying_playback() {
    let f = Fixture::new();
    std::fs::write(f.sample().location, []).unwrap();
    let state = f.core.state();
    let (mut api, mut requests) = crate::anilist::mock_api(vec![(
        200,
        json!({"data":{"SaveMediaListEntry":{
            "id":22,"mediaId":1,"status":"CURRENT","progress":2,"repeat":0,"score":0
        }}}),
    )])
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    original("prompt").save(&state.db).unwrap();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick(&work, &mut active).await.unwrap();
    commands::set_progress(1, 2, Some(1), state).await.unwrap();
    assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 2);
    assert!(state.db.watch_history(7, 10, None).unwrap().is_empty());
    active.as_mut().unwrap().accumulated = Duration::from_secs(95);
    let mut events = f.core.subscribe();
    f.tick(&work, &mut active).await.unwrap();
    assert_eq!(state.db.watch_history(7, 10, None).unwrap().len(), 1);
    assert!(tracking_events(&mut events).is_empty());
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn short_prompt_delay_suppresses_later_auto_ask_and_keeps_confirmation_valid() {
    let f = Fixture::new();
    let state = f.core.state();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (mut api, mut requests) = crate::anilist::mock_api_gated(
        vec![
            (
                200,
                json!({"data":{"Media":{
                    "id":1,"episodes":12,
                    "mediaListEntry":{
                        "id":22,"mediaId":1,"status":"CURRENT","progress":1,"repeat":0,"score":0
                    }
                }}}),
            ),
            (
                200,
                json!({"data":{"SaveMediaListEntry":{
                    "id":22,"mediaId":1,"status":"CURRENT","progress":2,"repeat":0,"score":0
                }}}),
            ),
        ],
        Some(gate.clone()),
    )
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    TrackingConfig {
        mode: "prompt".into(),
        prompt_seconds: 5,
        auto_ask: true,
        discord_enabled: false,
        ..TrackingConfig::default()
    }
    .save(&state.db)
    .unwrap();
    let mut events = f.core.subscribe();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick(&work, &mut active).await.unwrap();
    active.as_mut().unwrap().accumulated = Duration::from_secs(6);
    f.tick(&work, &mut active).await.unwrap();
    assert!(matches!(
        tracking_events(&mut events).as_slice(),
        [EventKind::TrackingPrompt(prompt)] if prompt.media_id == 1 && prompt.episode == 2
    ));
    let offer = state.retained_tracking_prompt().unwrap();
    let confirm = {
        let core = f.core.clone();
        let offer = offer.clone();
        tokio::spawn(async move { core.confirm_tracking_prompt(offer, 2).await })
    };
    let first = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first["variables"]["mediaId"], 1);

    active.as_mut().unwrap().accumulated = Duration::from_secs(20);
    f.tick(&work, &mut active).await.unwrap();
    assert!(
        tracking_events(&mut events).is_empty(),
        "auto_ask must not emit after TrackingPrompt for the same episode"
    );
    assert_eq!(
        state.retained_tracking_prompt().and_then(|e| e.prompt_id),
        offer.prompt_id
    );

    gate.add_permits(2);
    let confirmed = tokio::time::timeout(Duration::from_secs(5), confirm)
        .await
        .unwrap()
        .unwrap();
    assert!(
        confirmed.is_ok(),
        "prompt confirmation failed: {confirmed:?}"
    );
    let second = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(second["query"]
        .as_str()
        .unwrap()
        .contains("SaveMediaListEntry"));
    assert_eq!(second["variables"]["progress"], 2);
    assert!(state.db.pending(7).unwrap().is_empty());
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
}

#[tokio::test]
async fn inflight_auto_push_suppresses_auto_ask() {
    let f = Fixture::new();
    let state = f.core.state();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (mut api, mut requests) = crate::anilist::mock_api_gated(
        vec![
            (
                200,
                json!({"data":{"Media":{
                    "id":1,"episodes":12,
                    "mediaListEntry":{
                        "id":22,"mediaId":1,"status":"CURRENT","progress":1,"repeat":0,"score":0
                    }
                }}}),
            ),
            (
                200,
                json!({"data":{"SaveMediaListEntry":{
                    "id":22,"mediaId":1,"status":"CURRENT","progress":2,"repeat":0,"score":0
                }}}),
            ),
        ],
        Some(gate.clone()),
    )
    .await;
    api.record_viewer(7);
    *state.anilist.lock() = api;
    TrackingConfig {
        mode: "auto".into(),
        auto_percent: 80,
        auto_ask: true,
        discord_enabled: false,
        ..TrackingConfig::default()
    }
    .save(&state.db)
    .unwrap();
    let mut short = f.sample();
    short.length_us = 20_000_000;
    short.position_us = 18_000_000;
    let mut events = f.core.subscribe();
    let work = PlaybackWork::default();
    let mut active = None;
    f.tick_sample(&work, &mut active, Some(short.clone()))
        .await
        .unwrap();
    active.as_mut().unwrap().accumulated = Duration::from_secs(6);
    f.tick_sample(&work, &mut active, Some(short.clone()))
        .await
        .unwrap();
    assert!(active.as_ref().unwrap().push.is_some());
    let _ = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();

    active.as_mut().unwrap().accumulated = Duration::from_secs(20);
    f.tick_sample(&work, &mut active, Some(short))
        .await
        .unwrap();
    assert!(
        tracking_events(&mut events).is_empty(),
        "auto_ask must not fire while an automatic progress push is in flight"
    );
    assert!(state.retained_tracking_prompt().is_none());

    gate.add_permits(2);
    drop(active);
    tokio::time::timeout(Duration::from_secs(5), work.join_retired(false))
        .await
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(second["query"]
        .as_str()
        .unwrap()
        .contains("SaveMediaListEntry"));
    assert_eq!(second["variables"]["progress"], 2);
    assert!(state.db.pending(7).unwrap().is_empty());
    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 2);
}
