use super::*;
use crate::{Db, ListEntry, Media, Platform, UpdateChannel, User};
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
            UpdateChannel::Native,
        );
        let fixture = Self {
            core,
            profile: Profile(path),
        };
        let state = fixture.core.state();
        let sample = fixture.sample();
        assert!(state.anilist.lock().has_token());
        let matched =
            crate::recognize::match_title(&state.matchers.lock(), &sample.title, &sample.trackid)
                .expect("fixture playback title must match")
                .media_id;
        assert_eq!(Some(matched), sample.media_id);
        fixture
    }

    fn sample(&self) -> TickInfo {
        TickInfo {
            playing: true,
            player: "controlled".into(),
            trackid: self
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
        let mut cache = RecognitionCache {
            probe_result: Some(self.sample()),
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
    assert!(!track.asked && !track.prompted && track.push.is_none());
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
    assert!(!active.as_ref().unwrap().prompted);
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
    assert!(track.incremented && track.history_recorded && track.push.is_none());
    assert!(requests.try_recv().is_err());
    assert!(state.db.pending(7).unwrap().is_empty());
}
