use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{AppContext, EntityInputHandler, Focusable, TestAppContext, WindowVisibility};
use kurisu_core::models::{EntryDetails, ListEntry, Media, User};
use kurisu_core::{Core, Event, EventKind, Platform, UpdateChannel};

use super::covers::CoverOwner;
use super::my_list::MyListSlot;
use super::same_prompt;
use super::{
    BarrierAction, CloseIntent, EditorInputs, ProgressEdit, Route, SortKey, Workspace,
    freshness_plan, now_seconds,
};
use crate::components::input::TextInput;

#[test]
fn my_list_slots_translate_header_media_and_footer_without_id_sentinels() {
    assert_eq!(MyListSlot::count(0), 2);
    assert_eq!(MyListSlot::at(0, 3), MyListSlot::Header);
    assert_eq!(MyListSlot::at(1, 3), MyListSlot::Media(0));
    assert_eq!(MyListSlot::at(3, 3), MyListSlot::Media(2));
    assert_eq!(MyListSlot::at(4, 3), MyListSlot::Footer);
    assert_eq!(MyListSlot::media_range(0..5, 3), 0..3);
    assert_eq!(MyListSlot::media_range(4..5, 3), 3..3);
}

#[path = "auth_tests.rs"]
mod auth_tests;

#[gpui::test]
fn startup_waits_for_session_before_showing_sign_in(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("login-oauth").is_none());
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.route = Route::Settings;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("settings-sign-in").is_none());
    settle(cx, &runtime);
    assert!(cx.debug_bounds("settings-sign-in").is_some());
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.route = Route::MyList;
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("login-oauth").is_some());
}

#[gpui::test]
fn startup_restored_account_rejects_the_older_signed_out_result(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("login-oauth").is_none());
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_profile(
            Some(User {
                id: 47,
                name: "Saved account".into(),
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
    });
    settle(cx, &runtime);
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace
            .profile
            .as_ref()
            .map(|user| user.id)),
        Some(47)
    );
    assert!(cx.debug_bounds("login-oauth").is_none());
    assert!(cx.debug_bounds("my-list-search").is_some());
}

#[gpui::test]
fn startup_restore_failure_still_exposes_sign_in(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("login-oauth").is_none());
    drop(runtime);
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, _| !workspace.auth_error.is_empty()));
    assert!(cx.debug_bounds("login-oauth").is_some());
}

#[gpui::test]
fn restored_account_retains_sync_age_but_replacement_clears_it(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.last_account = Some(47);
        workspace.list.synced_at = Some(1234);
        workspace.set_profile(
            Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(workspace.list.synced_at, Some(1234));
        workspace.list.synced_at = Some(now_seconds());
        assert!(workspace.sync_due());
        workspace.last_sync_attempt = Some(Instant::now());
        assert!(!workspace.sync_due());
        workspace.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(workspace.list.synced_at, None);
    });
}

#[gpui::test]
fn late_sync_age_read_cannot_cross_a_new_sign_in_intent(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    runtime
        .block_on(core.set_preference("kurisu.list.synced".into(), Some("123".into())))
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| workspace.auth_intent += 1);
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.preferences_loaded);
        assert_eq!(workspace.list.synced_at, None);
    });
}

#[gpui::test]
fn sync_confirmation_timer_keeps_the_newer_completion(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.sync_request = 1;
        workspace.mark_list_synced(window, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.sync_request = 2;
        workspace.mark_list_synced(window, cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(cx, |workspace, _| workspace.list_sync_flash),
        Some(2)
    );
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |workspace, _| workspace.list_sync_flash.is_none()));
}

#[gpui::test]
fn long_detail_keeps_banner_height(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.route = Route::Anime(9);
        workspace.detail.detail = Some(kurisu_core::models::MediaDetail {
            media: Media {
                id: 9,
                title_english: Some("Synthetic detail".into()),
                banner_image: Some("https://example.invalid/banner.png".into()),
                description: Some("A long description.\n".repeat(100)),
                ..Default::default()
            },
            relations: vec![],
            characters: vec![],
            staff: vec![],
            cached_at: None,
            warning: None,
            unavailable_sections: vec![],
        });
        cx.notify();
    });
    cx.run_until_parked();
    let height = cx.update(|window, _| {
        if f32::from(window.viewport_size().width) <= 960. {
            160.
        } else {
            192.
        }
    });
    assert_eq!(
        cx.debug_bounds("detail-banner").unwrap().size.height,
        gpui::px(height)
    );
}

struct TestPlatform;

impl Platform for TestPlatform {
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn open_path(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal_path(&self, _: &Path) -> Result<(), String> {
        Ok(())
    }
}

struct FixtureDir(PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> (Core, tokio::runtime::Runtime, FixtureDir) {
    fixture_with_platform(Arc::new(TestPlatform))
}

fn fixture_with_platform(
    platform: Arc<dyn Platform>,
) -> (Core, tokio::runtime::Runtime, FixtureDir) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "kurisu-native-workspace-{}-{nonce}",
        std::process::id()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let core = Core::open(
        path.clone(),
        runtime.handle().clone(),
        platform,
        UpdateChannel::Native,
    )
    .unwrap();
    (core, runtime, FixtureDir(path))
}

fn settle(cx: &mut TestAppContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..16 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
}

fn update_fixture() -> kurisu_core::models::UpdateInfo {
    serde_json::from_value(serde_json::json!({
        "available": true,
        "can_install": true,
        "restart_pending": false,
        "version": "2.0.0",
        "tag": "v2.0.0",
        "current": "1.0.0.77",
        "html_url": "https://example.invalid/release",
        "body": "Synthetic release"
    }))
    .unwrap()
}

fn tracking_fixture(episode: i64) -> kurisu_core::models::TrackingPrompt {
    kurisu_core::models::TrackingPrompt {
        media_id: 9,
        episode,
        title: "Synthetic".into(),
        raw_title: format!("Synthetic {episode}"),
        progress: 3,
    }
}

#[gpui::test]
fn quick_close_flushes_latest_query_before_the_debounce(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.list_query.update(cx, |input, cx| {
            input.replace_text_in_range(None, "x", window, cx);
        });
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        assert_eq!(workspace.query_pending, Some(Some("x".into())));
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
        assert!(workspace.query_write_active);
    });
    for _ in 0..8 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_none());
        assert!(workspace.query_error.is_empty());
    });
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.q".into()))
            .unwrap()
            .as_deref(),
        Some("x")
    );
}

#[gpui::test]
fn query_reset_and_new_edit_settle_after_an_older_write(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.stage_query(Some("old account".into()));
        workspace.save_query(window, cx);
        workspace.last_account = Some(47);
        workspace.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(workspace.query_pending, Some(None));
    });
    for _ in 0..12 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.q".into()))
            .unwrap(),
        None
    );
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.stage_query(Some("first".into()));
        workspace.save_query(window, cx);
        workspace.stage_query(Some("latest".into()));
        workspace.request_close(CloseIntent::Quit, window, cx);
    });
    for _ in 0..12 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.q".into()))
            .unwrap()
            .as_deref(),
        Some("latest")
    );
    workspace.read_with(cx, |workspace, _| assert!(workspace.barrier.is_none()));
}

#[gpui::test]
fn query_write_failure_blocks_close_until_retry(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock().execute_batch("CREATE TRIGGER fail_query BEFORE INSERT ON settings WHEN NEW.key = 'native_preference:kurisu.list.q' BEGIN SELECT RAISE(ABORT, 'query storage failure'); END;").unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.stage_query(Some("retained".into()));
        workspace.request_close(CloseIntent::Quit, window, cx);
    });
    for _ in 0..8 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_some());
        assert!(workspace.barrier_error.contains("query storage failure"));
        assert_eq!(workspace.query_pending, Some(Some("retained".into())));
    });
    db.0.lock()
        .execute_batch("DROP TRIGGER fail_query;")
        .unwrap();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.retry_barrier(window, cx)
    });
    for _ in 0..8 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
    workspace.read_with(cx, |workspace, _| assert!(workspace.barrier.is_none()));
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.q".into()))
            .unwrap()
            .as_deref(),
        Some("retained")
    );
}

#[gpui::test]
fn nonquery_failure_retains_latest_intent_and_blocks_close(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock().execute_batch("CREATE TABLE preference_attempts (key TEXT, value TEXT);
        CREATE TRIGGER fail_preferences BEFORE INSERT ON settings
        WHEN NEW.key IN ('native_preference:kurisu.list.filter', 'native_preference:kurisu.list.sort', 'native_preference:kurisu.list.dir')
        BEGIN
          INSERT INTO preference_attempts VALUES (NEW.key, NEW.value);
          SELECT RAISE(FAIL, 'preference storage failure');
        END;").unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("COMPLETED", window, cx);
        workspace.select_sort(SortKey::Score, window, cx);
        workspace.flip_sort(window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(matches!(workspace.barrier, Some(BarrierAction::Close(_))));
        assert!(
            workspace
                .barrier_error
                .contains("preference storage failure")
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.filter"].pending,
            Some(Some("COMPLETED".into()))
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.sort"].pending,
            Some(Some("score".into()))
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.dir"].pending,
            Some(Some("asc".into()))
        );
    });
    let attempts: i64 =
        db.0.lock()
            .query_row("SELECT COUNT(*) FROM preference_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
    assert_eq!(attempts, 3);
    let attempted_direction: String = db
        .0
        .lock()
        .query_row(
            "SELECT value FROM preference_attempts WHERE key = 'native_preference:kurisu.list.dir'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempted_direction, "asc");
    cx.update(|window, app| window.draw(app).clear(app));
    assert!(cx.debug_bounds("retry-barrier").is_some());

    workspace.update_in(cx, |workspace, _, cx| workspace.cancel_barrier(cx));
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_none());
        assert!(workspace.preference_has_work());
        assert!(workspace.preference_error().is_some());
    });
    cx.update(|window, app| window.draw(app).clear(app));
    let retry = cx.debug_bounds("retry-preferences").unwrap();
    db.0.lock()
        .execute_batch("DROP TRIGGER fail_preferences;")
        .unwrap();
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_none());
        assert!(workspace.preference_error().is_none());
        assert!(!workspace.preference_has_work());
    });
    for (key, expected) in [
        ("kurisu.list.filter", "COMPLETED"),
        ("kurisu.list.sort", "score"),
        ("kurisu.list.dir", "asc"),
    ] {
        assert_eq!(
            runtime
                .block_on(core.get_preference(key.into()))
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

#[gpui::test]
fn torrent_preference_failures_block_transitions_until_explicit_retry(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock()
        .execute_batch(
            "CREATE TRIGGER fail_torrent_preferences BEFORE INSERT ON settings
        WHEN NEW.key LIKE 'native_preference:kurisu.torrents.%'
        BEGIN SELECT RAISE(FAIL, 'torrent preference storage failure'); END;",
        )
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    let values = [
        ("kurisu.torrents.new", "1"),
        ("kurisu.torrents.statuses", "[\"CURRENT\",\"PLANNING\"]"),
        (
            "kurisu.torrents.filters",
            "{\"category\":\"1_2\",\"release\":\"2\"}",
        ),
    ];
    workspace.update_in(cx, |workspace, window, cx| {
        for (key, value) in values {
            workspace.persist_preference(key, Some(value.into()), window, cx);
        }
    });
    settle(cx, &runtime);
    for (key, _) in values {
        assert_eq!(
            runtime.block_on(core.get_preference(key.into())).unwrap(),
            None
        );
    }
    workspace.update_in(cx, |workspace, window, cx| {
        for action in [
            BarrierAction::Close(CloseIntent::Quit),
            BarrierAction::Install,
            BarrierAction::Logout,
            BarrierAction::Login,
        ] {
            workspace.start_barrier(action, window, cx);
            assert_eq!(workspace.barrier, Some(action));
            assert!(
                workspace
                    .barrier_error
                    .contains("torrent preference storage failure")
            );
            workspace.cancel_barrier(cx);
        }
    });
    db.0.lock()
        .execute_batch("DROP TRIGGER fail_torrent_preferences;")
        .unwrap();
    settle(cx, &runtime);
    for (key, _) in values {
        assert_eq!(
            runtime.block_on(core.get_preference(key.into())).unwrap(),
            None
        );
    }
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.retry_preferences(window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_none());
        assert!(workspace.preference_error().is_none());
    });
    let reopened = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    for (key, expected) in values {
        assert_eq!(
            reopened
                .get_setting(&format!("native_preference:{key}"))
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

#[gpui::test]
fn nonquery_close_waits_for_locked_sqlite_writer(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    let lock_db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    lock_db.0.lock().execute_batch("BEGIN IMMEDIATE;").unwrap();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("COMPLETED", window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert!(matches!(workspace.barrier, Some(BarrierAction::Close(_))));
        assert_eq!(workspace.preference_active, Some("kurisu.list.filter"));
    });
    let unlock = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        lock_db.0.lock().execute_batch("ROLLBACK;").unwrap();
    });
    let started = Instant::now();
    runtime.block_on(async { tokio::task::yield_now().await });
    unlock.join().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(80));
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| assert!(workspace.barrier.is_none()));
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("COMPLETED")
    );
}

#[gpui::test]
fn nonquery_reset_deletion_settles_after_older_value(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
        workspace.persist_preference("kurisu.list.synced", Some("123".into()), window, cx);
        workspace.last_account = Some(47);
        workspace.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.filter"].pending,
            Some(None)
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.synced"].pending,
            Some(None)
        );
    });
    settle(cx, &runtime);
    for key in ["kurisu.list.filter", "kurisu.list.synced"] {
        assert_eq!(
            runtime.block_on(core.get_preference(key.into())).unwrap(),
            None
        );
    }
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("PLANNING", window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
    });
    settle(cx, &runtime);
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("PLANNING")
    );
}

#[gpui::test]
fn nonquery_selection_after_reset_supersedes_pending_delete(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
        workspace.last_account = Some(47);
        workspace.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(
            workspace.preference_writes["kurisu.list.filter"].pending,
            Some(None)
        );
        workspace.select_filter("PLANNING", window, cx);
        assert_eq!(
            workspace.preference_writes["kurisu.list.filter"].pending,
            Some(Some("PLANNING".into()))
        );
    });
    settle(cx, &runtime);
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("PLANNING")
    );
}

#[gpui::test]
fn nonquery_superseded_failure_saves_newer_value(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock()
        .execute_batch(
            "CREATE TABLE old_filter_attempts (value TEXT);
        CREATE TRIGGER fail_old_filter BEFORE INSERT ON settings
        WHEN NEW.key = 'native_preference:kurisu.list.filter' AND NEW.value = 'PLANNING'
        BEGIN
          INSERT INTO old_filter_attempts VALUES (NEW.value);
          SELECT RAISE(FAIL, 'old value rejected');
        END;",
        )
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("PLANNING", window, cx);
        workspace.select_filter("COMPLETED", window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.barrier.is_none());
        assert!(workspace.preference_error().is_none());
        assert!(!workspace.preference_has_work());
    });
    let old_attempts: i64 =
        db.0.lock()
            .query_row("SELECT COUNT(*) FROM old_filter_attempts", [], |row| {
                row.get(0)
            })
            .unwrap();
    assert_eq!(old_attempts, 1);
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("COMPLETED")
    );
}

#[gpui::test]
fn late_preference_read_preserves_new_local_selection(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    runtime
        .block_on(core.set_preference("kurisu.list.filter".into(), Some("PLANNING".into())))
        .unwrap();
    runtime
        .block_on(core.set_preference("kurisu.list.sort".into(), Some("title".into())))
        .unwrap();
    runtime
        .block_on(core.set_preference("kurisu.list.dir".into(), Some("asc".into())))
        .unwrap();
    runtime
        .block_on(core.set_preference("kurisu.list.synced".into(), Some("123".into())))
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("COMPLETED", window, cx);
        workspace.select_sort(SortKey::Score, window, cx);
        workspace.list.synced_at = Some(456);
        workspace.persist_preference("kurisu.list.synced", Some("456".into()), window, cx);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.preferences_loaded);
        assert_eq!(workspace.list.filter, "COMPLETED");
        assert_eq!(workspace.list.sort, SortKey::Score);
        assert!(workspace.list.desc);
        assert_eq!(workspace.list.synced_at, Some(456));
    });
    for (key, expected) in [
        ("kurisu.list.filter", "COMPLETED"),
        ("kurisu.list.sort", "score"),
        ("kurisu.list.dir", "desc"),
        ("kurisu.list.synced", "456"),
    ] {
        assert_eq!(
            runtime
                .block_on(core.get_preference(key.into()))
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

#[gpui::test]
fn calendar_and_synced_preferences_use_tracked_writer(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_calendar_filter(false, window, cx);
        workspace.list.synced_at = Some(789);
        workspace.persist_preference("kurisu.list.synced", Some("789".into()), window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| assert!(workspace.barrier.is_none()));
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.cal.mine".into()))
            .unwrap()
            .as_deref(),
        Some("0")
    );
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.synced".into()))
            .unwrap()
            .as_deref(),
        Some("789")
    );
}

fn initial_global_preferences_survive_account_reset(cx: &mut TestAppContext, read_finished: bool) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    for (key, value) in [
        ("kurisu.list.sort", "score"),
        ("kurisu.list.dir", "desc"),
        ("kurisu.list.filter", "COMPLETED"),
        ("kurisu.list.q", "Old account query"),
        ("kurisu.list.synced", "123"),
    ] {
        db.set_setting(&format!("native_preference:{key}"), value)
            .unwrap();
    }
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    if read_finished {
        cx.run_until_parked();
        runtime.block_on(async {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        });
    }
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(!workspace.preferences_loaded);
        for id in [47, 48] {
            workspace.set_profile(
                Some(User {
                    id,
                    offline: true,
                    ..Default::default()
                }),
                window,
                cx,
            );
        }
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.preferences_loaded);
        assert_eq!(workspace.list.sort, SortKey::Score);
        assert!(workspace.list.desc);
        assert_eq!(workspace.list.filter, "CURRENT");
        assert!(workspace.list.query.is_empty());
        assert_eq!(workspace.list.synced_at, None);
    });
    for (key, expected) in [("kurisu.list.sort", "score"), ("kurisu.list.dir", "desc")] {
        assert_eq!(
            db.get_setting(&format!("native_preference:{key}"))
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

#[gpui::test]
fn pending_initial_global_preferences_survive_account_reset(cx: &mut TestAppContext) {
    initial_global_preferences_survive_account_reset(cx, false);
}

#[gpui::test]
fn completed_initial_global_preferences_survive_account_reset(cx: &mut TestAppContext) {
    initial_global_preferences_survive_account_reset(cx, true);
}

#[gpui::test]
fn nonquery_pending_write_holds_logout_install_and_login(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("COMPLETED", window, cx);
        workspace.start_barrier(BarrierAction::Logout, window, cx);
        assert_eq!(workspace.barrier, Some(BarrierAction::Logout));
        workspace.cancel_barrier(cx);
        assert!(workspace.preference_has_work());

        workspace.start_barrier(BarrierAction::Install, window, cx);
        assert_eq!(workspace.barrier, Some(BarrierAction::Install));
        workspace.cancel_barrier(cx);
        assert!(workspace.preference_has_work());

        workspace.login(false, Some("synthetic test token".into()), window, cx);
        assert_eq!(workspace.barrier, Some(BarrierAction::Login));
        assert!(workspace.queued_login.is_some());
        workspace.cancel_barrier(cx);
        assert!(workspace.preference_has_work());
        assert!(workspace.queued_login.is_none());
    });
}

fn calendar_choice_survives_account_reset(cx: &mut TestAppContext, reject: bool) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.set_setting("native_preference:kurisu.cal.mine", "1")
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_profile(
            Some(User {
                id: 47,
                ..Default::default()
            }),
            window,
            cx,
        );
        workspace.navigate(Route::Calendar, window, cx);
    });
    settle(cx, &runtime);
    if reject {
        db.0.lock()
            .execute_batch(
                "CREATE TRIGGER fail_calendar BEFORE INSERT ON settings
            WHEN NEW.key = 'native_preference:kurisu.cal.mine'
            BEGIN SELECT RAISE(FAIL, 'calendar storage failure'); END;",
            )
            .unwrap();
    }
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_calendar_filter(false, window, cx);
        assert!(workspace.preference_has_work());
    });
    if reject {
        settle(cx, &runtime);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.preference_error().is_some());
        });
    }
    workspace.update_in(cx, |workspace, window, cx| {
        workspace
            .calendar
            .items
            .push(kurisu_core::models::AiringItem {
                media: Media {
                    id: 9,
                    ..Default::default()
                },
                episode: 1,
                airing_at: now_seconds(),
            });
        workspace.refresh_calendar_visible();
        assert_eq!(workspace.calendar.visible.len(), 1);
        workspace.set_profile(
            Some(User {
                id: 48,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert!(!workspace.calendar.watching_only);
        assert!(workspace.calendar.preference_loaded);
        assert!(workspace.calendar.items.is_empty());
        assert!(workspace.calendar.visible.is_empty());
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
        workspace.cancel_barrier(cx);
    });
    settle(cx, &runtime);
    if reject {
        assert_eq!(
            db.get_setting("native_preference:kurisu.cal.mine")
                .unwrap()
                .as_deref(),
            Some("1")
        );
        db.0.lock()
            .execute_batch("DROP TRIGGER fail_calendar;")
            .unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.retry_preferences(window, cx);
        });
        settle(cx, &runtime);
    }
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(!workspace.preference_has_work());
        assert!(workspace.preference_error().is_none());
        workspace.navigate(Route::MyList, window, cx);
        workspace.navigate(Route::Calendar, window, cx);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.calendar.watching_only);
    });
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.cal.mine".into()))
            .unwrap()
            .as_deref(),
        Some("0")
    );
}

#[gpui::test]
fn rejected_calendar_choice_survives_account_reset_and_retry(cx: &mut TestAppContext) {
    calendar_choice_survives_account_reset(cx, true);
}

#[gpui::test]
fn pending_calendar_choice_survives_account_reset_and_close_cancel(cx: &mut TestAppContext) {
    calendar_choice_survives_account_reset(cx, false);
}

fn initial_calendar_choice_replaces_old_read(cx: &mut TestAppContext, read_finished: bool) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.set_setting("native_preference:kurisu.cal.mine", "0")
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_profile(
            Some(User {
                id: 47,
                ..Default::default()
            }),
            window,
            cx,
        );
        workspace.navigate(Route::Calendar, window, cx);
    });
    cx.run_until_parked();
    let old_read = workspace.read_with(cx, |workspace, _| {
        assert!(workspace.calendar.watching_only);
        assert!(!workspace.calendar.preference_loaded);
        assert!(workspace.calendar.preference_loading);
        workspace.read_aborts["calendar-preference"][0].clone()
    });
    if read_finished {
        runtime.block_on(async {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
        });
        assert!(old_read.is_finished());
    } else {
        assert!(!old_read.is_finished());
    }
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.set_calendar_filter(true, window, cx);
        assert!(workspace.calendar.preference_loaded);
        assert!(!workspace.calendar.preference_loading);
        assert!(!workspace.read_aborts.contains_key("calendar-preference"));
        assert!(workspace.preference_has_work());
    });
    settle(cx, &runtime);
    assert_eq!(
        db.get_setting("native_preference:kurisu.cal.mine")
            .unwrap()
            .as_deref(),
        Some("1")
    );
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(workspace.calendar.watching_only);
        assert!(!workspace.preference_has_work());
        let request = workspace.calendar.preference_request;
        workspace.set_calendar_filter(true, window, cx);
        assert_eq!(workspace.calendar.preference_request, request);
        assert!(!workspace.preference_has_work());
        workspace.navigate(Route::MyList, window, cx);
        workspace.navigate(Route::Calendar, window, cx);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.calendar.watching_only);
    });
}

#[gpui::test]
fn initial_calendar_choice_cancels_pending_preference_read(cx: &mut TestAppContext) {
    initial_calendar_choice_replaces_old_read(cx, false);
}

#[gpui::test]
fn initial_calendar_choice_rejects_completed_preference_read(cx: &mut TestAppContext) {
    initial_calendar_choice_replaces_old_read(cx, true);
}

fn retained_prompt(core: &Core) -> Event {
    core.recovery_events()
        .into_iter()
        .find(|event| {
            matches!(
                event.kind,
                EventKind::TrackingPrompt(_) | EventKind::TrackingAsk(_)
            )
        })
        .unwrap()
}

#[gpui::test]
fn workspace_drop_aborts_reads_but_not_unregistered_writes(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let read = core.spawn(std::future::pending::<()>());
    let write = core.spawn(std::future::pending::<()>());
    let read_abort = read.abort_handle();
    let write_abort = write.abort_handle();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.replace_reads("workspace-drop-test", vec![read_abort.clone()]);
    });
    let event_stop = workspace.read_with(cx, |workspace, _| workspace.event_stop.clone());
    let weak = workspace.downgrade();
    cx.update(|window, _| window.remove_window());
    drop(workspace);
    cx.run_until_parked();
    runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(2)).await });
    assert!(weak.upgrade().is_none());
    assert!(event_stop.lock().unwrap().is_none());
    let read_result = runtime
        .block_on(async { tokio::time::timeout(std::time::Duration::from_millis(50), read).await });
    assert!(matches!(read_result, Ok(Err(error)) if error.is_cancelled()));
    assert!(!write_abort.is_finished());
    write_abort.abort();
    runtime.block_on(async {
        let _ = write.await;
    });
}

#[gpui::test]
fn restart_state_survives_stale_notices_settings_reset_and_dismissal(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.show_update(update_fixture());
        workspace.settings.update = Some(update_fixture());
        workspace.note_restart_pending();
        assert!(workspace.update.as_ref().unwrap().restart_pending);
        assert!(workspace.settings.update.as_ref().unwrap().restart_pending);
        workspace.show_update(update_fixture());
        assert!(workspace.update.as_ref().unwrap().restart_pending);
        workspace.dismissed_update = workspace.update.take().unwrap().tag;
        workspace.set_profile(
            Some(User {
                id: 47,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert!(workspace.restart_pending);
        assert!(workspace.settings.update.is_none());
        workspace.show_update(update_fixture());
        assert!(workspace.update.is_none());
        workspace.settings.update = Some(update_fixture());
        workspace.install_settings_update(window, cx);
        assert!(!workspace.update_busy);
        assert!(workspace.barrier.is_none());
    });
}

#[gpui::test]
fn prompt_recovery_keeps_exact_event_identity_and_deduplicates(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        let payload = tracking_fixture(4);
        let event = Event {
            generation: workspace.generation,
            account_id: None,
            prompt_id: Some(1),
            kind: EventKind::TrackingPrompt(payload.clone()),
        };
        workspace.queue_prompt(event.clone());
        workspace.queue_prompt(event.clone());
        assert_eq!(workspace.prompt_queue.len(), 0);
        assert!(same_prompt(
            workspace.prompt_event.as_ref().unwrap(),
            &event
        ));
        let ask = Event {
            kind: EventKind::TrackingAsk(payload),
            ..event
        };
        workspace.queue_prompt(ask.clone());
        assert!(same_prompt(workspace.prompt_event.as_ref().unwrap(), &ask));
        assert_eq!(workspace.prompt_queue.len(), 0);
    });
}

#[gpui::test]
fn prompt_skip_acknowledges_only_the_original_retained_event(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let original = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.queue_prompt(original.clone());
        workspace.dismiss_prompt(cx);
        assert!(workspace.prompt.is_none());
    });
    runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(2)).await });
    assert!(core.recovery_events().into_iter().all(|event| !matches!(
        event.kind,
        EventKind::TrackingPrompt(_) | EventKind::TrackingAsk(_)
    )));
    core.emit(EventKind::TrackingAsk(tracking_fixture(5)));
    let newer = retained_prompt(&core);
    workspace.update_in(cx, |workspace, _, _| workspace.queue_prompt(newer.clone()));
    assert!(core.event_is_current(&newer));
    assert!(!core.event_is_current(&original));
}

#[gpui::test]
fn stale_prompt_confirm_does_not_ack_a_new_track(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let original = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.queue_prompt(original.clone())
    });
    core.emit(EventKind::TrackingPrompt(tracking_fixture(5)));
    let newer = retained_prompt(&core);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_prompt(window, cx);
        assert!(workspace.prompt.is_none());
        assert!(!workspace.prompt_busy);
    });
    assert!(core.event_is_current(&newer));
}

#[gpui::test]
fn successful_prompt_confirm_acknowledges_the_retained_event(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let event = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.queue_prompt(event.clone());
        workspace.confirm_prompt(window, cx);
        assert!(workspace.prompt_busy);
    });
    for _ in 0..12 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(2)).await });
    }
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.prompt.is_none());
        assert!(!workspace.prompt_busy);
    });
    assert!(!core.event_is_current(&event));
}

#[gpui::test]
fn delayed_skip_cannot_ack_identical_reissued_prompt(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let first = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    core.emit(EventKind::NowPlaying(kurisu_core::models::NowPlaying {
        active: false,
        player: String::new(),
        title: String::new(),
        matched: None,
        media_id: None,
        episode: None,
        length_us: 0,
        position_us: 0,
    }));
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let second = retained_prompt(&core);
    assert!(!same_prompt(&first, &second));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.queue_prompt(first);
        workspace.dismiss_prompt(cx);
    });
    runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_millis(2)).await });
    assert!(core.event_is_current(&second));
    workspace.update_in(cx, |workspace, _, _| workspace.queue_prompt(second.clone()));
    workspace.read_with(cx, |workspace, _| {
        assert!(same_prompt(
            workspace.prompt_event.as_ref().unwrap(),
            &second
        ));
    });
}

#[gpui::test]
fn inactive_progress_draft_blocks_close_until_explicit_discard(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let mut edit = ProgressEdit::new(2, Some(12));
        assert!(edit.step(1, Instant::now()));
        workspace.progress_drafts.insert(47, [(9, edit)].into());
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(matches!(
            workspace.barrier,
            Some(BarrierAction::Close(CloseIntent::Quit))
        ));
        assert!(workspace.barrier_error.contains("another account"));
        assert_eq!(workspace.progress_drafts.get(&47).unwrap()[&9].pending, 3);
        workspace.discard_inactive_drafts(window, cx);
        assert!(workspace.progress_drafts.is_empty());
        assert!(workspace.barrier.is_none());
    });
}

#[gpui::test]
fn account_change_keeps_progress_and_resets_transient_flags(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.profile = Some(User {
            id: 47,
            name: "Fixture".into(),
            ..Default::default()
        });
        workspace.generation = 99;
        let entry = ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            progress: 2,
            ..Default::default()
        };
        assert!(workspace.progress.entry(99, &entry).step(1, Instant::now()));
        workspace.pending_busy = true;
        workspace.prompt_busy = true;
        workspace.pending_error = "old pending failure".into();
        workspace.prompt_error = "old prompt failure".into();
        workspace.list_refresh_queued = true;
        workspace.list_scope = Some([9].into());
        workspace.set_profile(None, window, cx);
        assert_eq!(workspace.progress_drafts.get(&47).unwrap()[&9].pending, 3);
        assert!(!workspace.pending_busy && !workspace.prompt_busy);
        assert!(workspace.pending_error.is_empty() && workspace.prompt_error.is_empty());
        assert!(!workspace.list_refresh_queued && workspace.list_scope.is_none());
    });
}

#[gpui::test]
fn local_list_reads_do_not_finish_or_cancel_sync_identity(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.generation = 12;
        workspace.sync_request = 3;
        workspace.list.syncing = true;
        workspace.list_request = 18;
        assert!(workspace.sync_result_current(12, 3));
        workspace.list_request += 1;
        assert!(workspace.sync_result_current(12, 3));
        assert!(workspace.list.syncing);
        workspace.sync_request += 1;
        assert!(!workspace.sync_result_current(12, 3));
        workspace.generation += 1;
        assert!(!workspace.sync_result_current(12, 4));
    });
}

#[gpui::test]
fn delayed_profile_refresh_cannot_replace_a_new_login(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.generation = 12;
        workspace.auth_intent = 4;
        workspace.auth_request = 8;
        assert!(workspace.auth_refresh_current(12, 4, 8));
        workspace.auth_intent += 1;
        assert!(!workspace.auth_refresh_current(12, 4, 8));
        workspace.auth_request += 1;
        assert!(!workspace.auth_refresh_current(12, 5, 8));
        assert!(!workspace.auth_refresh_current(11, 5, 9));
    });
}

#[gpui::test]
fn cover_regions_retire_departed_rows_and_routes(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.route = Route::Search;
        let first = (101, 112, 160);
        let second = (102, 112, 160);
        let avatar = (i64::MIN, 36, 36);
        workspace.replace_cover_region(
            CoverOwner::Route(Route::Search),
            vec![(first, "invalid-url".into())],
            vec![],
        );
        assert!(workspace.cover_tickets.contains_key(&first));
        workspace.replace_cover_region(
            CoverOwner::Route(Route::Search),
            vec![(second, "invalid-url".into())],
            vec![],
        );
        assert!(!workspace.cover_tickets.contains_key(&first));
        assert!(!workspace.cover_images.contains_key(&first));
        assert!(workspace.cover_tickets.contains_key(&second));
        workspace.request_cover_for(CoverOwner::Avatar, avatar, "invalid-url");
        workspace.route = Route::Library;
        workspace.retain_cover_route();
        assert!(!workspace.cover_tickets.contains_key(&second));
        assert!(workspace.cover_tickets.contains_key(&avatar));
        assert_eq!(workspace.cover_regions.len(), 1);
        assert!(workspace.cover_regions.contains_key(&CoverOwner::Avatar));
    });
}

#[gpui::test]
fn returning_to_loaded_my_list_requeues_visible_covers(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.route = Route::Library;
        workspace.list.replace(
            vec![ListEntry {
                media_id: 9,
                status: "CURRENT".into(),
                media: Some(Media {
                    id: 9,
                    cover_medium: Some("invalid-url".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            0,
        );
        workspace.reset_list_widget();
        workspace.switch_route(Route::MyList, window, cx);
        assert!(workspace.cover_tickets.contains_key(&(9, 46, 63)));
        assert!(
            workspace
                .cover_regions
                .contains_key(&CoverOwner::Route(Route::MyList))
        );
    });
}

#[gpui::test]
fn my_list_header_rows_and_footer_share_one_measured_scroll(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            (1..=40)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id,
                        title_english: Some(format!("Synthetic series {id:02}")),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            0,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(600.)));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.list_widget.item_count(), MyListSlot::count(40));
        let header = workspace.list_widget.bounds_for_item(0).unwrap();
        let first = workspace
            .list_widget
            .bounds_for_item(MyListSlot::item_ix(0))
            .unwrap();
        assert!(header.size.height > gpui::px(180.));
        assert_eq!(header.bottom(), first.top());
        assert!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::count(40) - 1)
                .is_none()
        );
    });
    assert!(cx.debug_bounds("my-list-heading").is_some());
    assert!(cx.debug_bounds("my-list-footer").is_none());
    let (header_height, first_height) = workspace.read_with(cx, |workspace, _| {
        (
            workspace
                .list_widget
                .bounds_for_item(0)
                .unwrap()
                .size
                .height,
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(0))
                .unwrap()
                .size
                .height,
        )
    });
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list.step_error = "Synthetic failure".into();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert!(
            workspace
                .list_widget
                .bounds_for_item(0)
                .unwrap()
                .size
                .height
                > header_height
        );
        assert_eq!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(0))
                .unwrap()
                .size
                .height,
            first_height
        );
    });
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list_widget.scroll_by(gpui::px(650.));
        cx.notify();
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.list_widget.logical_scroll_top().item_ix > 0);
        assert!(workspace.list_widget.bounds_for_item(0).is_none());
    });
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list_widget.scroll_to_end();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        let footer = workspace
            .list_widget
            .bounds_for_item(MyListSlot::count(40) - 1)
            .unwrap();
        let viewport = workspace.list_widget.viewport_bounds();
        assert!(footer.top() < viewport.bottom() && footer.bottom() > viewport.top());
    });
    assert!(cx.debug_bounds("my-list-footer").is_some());
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: gpui::px(0.),
        });
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("my-list-heading").is_some());
    assert!(cx.debug_bounds("tab-CURRENT").is_some());
    let search = cx.debug_bounds("my-list-search-icon").unwrap();
    cx.simulate_click(search.center(), gpui::Modifiers::default());
    let focus = workspace.read_with(cx, |workspace, cx| {
        workspace.list_query.read(cx).focus_handle(cx)
    });
    cx.update(|window, _| assert!(focus.is_focused(window)));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list_widget.scroll_to_end();
        workspace.list.set_filter("PLANNING", now_seconds());
        workspace.reset_list_widget();
        assert_eq!(workspace.list_widget.logical_scroll_top().item_ix, 0);
        assert_eq!(workspace.list_widget.item_count(), MyListSlot::count(0));
        cx.notify();
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("my-list-heading").is_some());
}

#[gpui::test]
fn deletion_refresh_clears_selected_detail_entry(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        let entry = ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            progress: 3,
            ..Default::default()
        };
        workspace.list.replace(vec![entry.clone()], 0);
        workspace.detail.media_id = Some(9);
        workspace.detail.entry = Some(entry);
        workspace.detail.entry_available = true;
        workspace.list.apply_scoped(&[9], vec![None], 0);
        workspace.reconcile_detail_entry();
        assert!(workspace.detail.entry_available);
        assert!(workspace.detail.entry.is_none());
    });
}

#[gpui::test]
fn offline_editor_refusal_blocks_close_immediately(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let mut input = |value: &str, cx: &mut gpui::Context<Workspace>| {
            cx.new(|cx| TextInput::new(value, "", window, cx))
        };
        workspace.editor_inputs = Some(EditorInputs {
            progress: input("3", cx),
            score: input("", cx),
            repeat: input("0", cx),
            notes: input("new note", cx),
            started_year: input("", cx),
            started_month: input("", cx),
            started_day: input("", cx),
            completed_year: input("", cx),
            completed_month: input("", cx),
            completed_day: input("", cx),
        });
        let entry = ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            progress: 3,
            ..Default::default()
        };
        let mut editor = super::EditorState::new(entry);
        editor.apply_details(EntryDetails {
            notes: "old note".into(),
            ..Default::default()
        });
        workspace.editor = Some(editor);
        workspace.profile = Some(User {
            id: 47,
            offline: true,
            ..Default::default()
        });
        workspace.offline = true;
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(matches!(workspace.barrier, Some(BarrierAction::Close(_))));
        assert!(workspace.barrier_error.contains("Reconnect to AniList"));
        assert_eq!(workspace.active_writes, 0);
        workspace.retry_barrier(window, cx);
        assert!(workspace.barrier_error.contains("Reconnect to AniList"));
        assert!(matches!(workspace.barrier, Some(BarrierAction::Close(_))));
    });
}

#[gpui::test]
fn expired_busy_editor_restores_editable_text_inputs(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let mut input = |value: &str, cx: &mut gpui::Context<Workspace>| {
            cx.new(|cx| TextInput::new(value, "", window, cx))
        };
        workspace.editor_inputs = Some(EditorInputs {
            progress: input("2", cx),
            score: input("80", cx),
            repeat: input("0", cx),
            notes: input("", cx),
            started_year: input("", cx),
            started_month: input("", cx),
            started_day: input("", cx),
            completed_year: input("", cx),
            completed_month: input("", cx),
            completed_day: input("", cx),
        });
        let account = User {
            id: 47,
            score_format: Some("POINT_100".into()),
            ..Default::default()
        };
        workspace.profile = Some(account.clone());
        workspace.generation = 99;
        workspace.editor = Some(super::EditorState::new(ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            progress: 2,
            score: Some(70.),
            ..Default::default()
        }));
        workspace.editor.as_mut().unwrap().busy = true;
        workspace.active_writes = 1;
        workspace.set_editor_inputs_disabled(true, cx);
        let score = workspace.editor_inputs.as_ref().unwrap().score.clone();
        let notes = workspace.editor_inputs.as_ref().unwrap().notes.clone();
        score.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..2), "90", window, cx)
        });
        assert_eq!(score.read(cx).value(), "80");

        workspace.set_profile(None, window, cx);
        assert!(!workspace.editor_drafts.get(&47).unwrap().editor.busy);
        assert_eq!(
            workspace
                .editor_drafts
                .get(&47)
                .unwrap()
                .inputs
                .as_ref()
                .unwrap()
                .score
                .read(cx)
                .value(),
            "80"
        );
        workspace.active_writes = 0;
        workspace.set_profile(Some(account.clone()), window, cx);
        assert!(!workspace.editor.as_ref().unwrap().busy);
        score.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..2), "90", window, cx)
        });
        assert_eq!(score.read(cx).value(), "90");
        notes.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..0), "Recovered", window, cx)
        });
        assert_eq!(notes.read(cx).value(), "Recovered");

        workspace.editor.as_mut().unwrap().busy = true;
        workspace.set_editor_inputs_disabled(true, cx);
        workspace.set_profile(Some(account), window, cx);
        score.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..2), "60", window, cx)
        });
        assert_eq!(score.read(cx).value(), "90");
        notes.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..9), "Blocked", window, cx)
        });
        assert_eq!(notes.read(cx).value(), "Recovered");
    });
}

#[gpui::test]
fn visible_unfocused_window_retries_while_hidden_window_stays_quiet(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let handle = cx.add_window(|window, cx| Workspace::new(core, window, cx));
    {
        let other = cx.add_empty_window();
        other.update(|window, _| window.activate_window());
        other.run_until_parked();
    }
    handle
        .update(cx, |workspace, window, cx| {
            assert!(window.is_visible());
            assert!(!window.is_window_active());
            workspace.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            workspace.offline = true;
            workspace.last_reconnect_attempt = Some(Instant::now());
            workspace.visible_freshness(window, cx);
            assert!(!workspace.reconnect_busy);
            workspace.last_reconnect_attempt = None;
            workspace.visible_freshness(window, cx);
            assert!(workspace.reconnect_busy);
        })
        .unwrap();
    cx.simulate_window_visibility_change(handle.into(), WindowVisibility::Hidden);
    handle
        .update(cx, |workspace, window, cx| {
            assert!(!window.is_visible());
            workspace.reconnect_busy = false;
            workspace.last_reconnect_attempt = None;
            workspace.visible_freshness(window, cx);
            assert!(!workspace.reconnect_busy);
        })
        .unwrap();
}

#[test]
fn clock_plan_notifies_only_relevant_visible_routes() {
    for route in [Route::MyList, Route::Anime(9), Route::Calendar] {
        assert!(freshness_plan(true, &route, false, true).notify_clock);
        assert!(!freshness_plan(true, &route, false, false).notify_clock);
        assert!(!freshness_plan(false, &route, false, true).notify_clock);
    }
    assert!(!freshness_plan(true, &Route::Search, false, true).notify_clock);
    assert!(freshness_plan(true, &Route::Library, false, true).scan_library);
    assert!(freshness_plan(true, &Route::MyList, false, true).sync_list);
    assert!(freshness_plan(true, &Route::MyList, true, true).reconnect);
    assert_eq!(
        freshness_plan(false, &Route::Library, true, true),
        Default::default()
    );
}

#[gpui::test]
fn off_route_clock_tick_does_not_consume_airing_reorder(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let now = now_seconds();
        let first = ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            media: Some(Media {
                id: 1,
                next_airing_at: Some(now - 10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let second = ListEntry {
            media_id: 2,
            status: "CURRENT".into(),
            media: Some(Media {
                id: 2,
                next_airing_at: Some(now + 600),
                ..Default::default()
            }),
            ..Default::default()
        };
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.route = Route::Search;
        workspace.clock_at = now - 40;
        workspace.list.replace(vec![first, second], now - 40);
        workspace.list.set_sort(SortKey::Airing, now - 40);
        workspace.list.synced_at = Some(now);
        workspace.reset_list_widget();
        assert_eq!(workspace.list.visible(), &[1, 2]);
        workspace.visible_freshness(window, cx);
        assert_eq!(workspace.list.visible(), &[1, 2]);
        assert!(workspace.list.airing_order_due(now));
        workspace.switch_route(Route::MyList, window, cx);
        assert_eq!(workspace.list.visible(), &[2, 1]);
        assert!(!workspace.list.airing_order_due(now));
    });
}

#[gpui::test]
fn visible_list_sync_keeps_stale_and_retry_throttles(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(Vec::new(), now_seconds());
        workspace.list.synced_at = Some(now_seconds() - 301);
        workspace.last_sync_attempt = Some(Instant::now());
        workspace.visible_freshness(window, cx);
        assert!(!workspace.list.syncing);
        workspace.last_sync_attempt = None;
        workspace.visible_freshness(window, cx);
        assert!(workspace.list.syncing);
        let request = workspace.sync_request;
        workspace.visible_freshness(window, cx);
        assert_eq!(workspace.sync_request, request);
    });
}

#[gpui::test]
fn scoped_row_changes_keep_other_measurements_and_remeasure_wrapping_title(
    cx: &mut TestAppContext,
) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            ["A", "B", "C"]
                .into_iter()
                .enumerate()
                .map(|(index, title)| ListEntry {
                    media_id: index as i64 + 1,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id: index as i64 + 1,
                        title_english: Some(title.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            100,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    let first_height = workspace.read_with(cx, |workspace, _| {
        workspace
            .list_widget
            .bounds_for_item(MyListSlot::item_ix(0))
            .unwrap()
            .size
            .height
    });
    let second_height = workspace.read_with(cx, |workspace, _| {
        workspace
            .list_widget
            .bounds_for_item(MyListSlot::item_ix(1))
            .unwrap()
            .size
            .height
    });
    workspace.update_in(cx, |workspace, _, cx| {
        let mut updated = workspace.list.entry(2).unwrap().clone();
        updated.progress = 3;
        let change = workspace.list.apply_one(updated, 100);
        assert!(!change.order_changed);
        workspace.apply_list_change(change);
        assert_eq!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(0))
                .unwrap()
                .size
                .height,
            first_height
        );
        assert!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(1))
                .is_none()
        );
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, _, cx| {
        let mut updated = workspace.list.entry(2).unwrap().clone();
        updated.media.as_mut().unwrap().title_english =
            Some(format!("B {}", "Long title ".repeat(60)));
        let change = workspace.list.apply_one(updated, 100);
        assert!(!change.order_changed);
        assert_eq!(workspace.list.visible(), &[1, 2, 3]);
        workspace.apply_list_change(change);
        assert_eq!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(0))
                .unwrap()
                .size
                .height,
            first_height
        );
        assert!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(1))
                .is_none()
        );
        cx.notify();
    });
    cx.run_until_parked();
    let (first_after, second_after) = workspace.read_with(cx, |workspace, _| {
        (
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(0))
                .unwrap()
                .size
                .height,
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(1))
                .unwrap()
                .size
                .height,
        )
    });
    assert_eq!(first_after, first_height);
    assert!(second_after > second_height);
}

#[gpui::test]
fn row_only_scoped_update_keeps_scrolled_media_anchor(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            (1..=40)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id,
                        title_english: Some(format!("Show {id:02}")),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            100,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: MyListSlot::item_ix(10),
            offset_in_item: gpui::px(0.),
        });
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, _, cx| {
        let before = workspace.list_widget.logical_scroll_top();
        assert_eq!(workspace.list.visible()[before.item_ix - 1], 11);
        let mut updated = workspace.list.entry(12).unwrap().clone();
        updated.progress = 4;
        let change = workspace.list.apply_one(updated, 100);
        assert!(!change.order_changed);
        workspace.apply_list_change(change);
        let after = workspace.list_widget.logical_scroll_top();
        assert_eq!(workspace.list.visible()[after.item_ix - 1], 11);
        assert_eq!(after.offset_in_item, before.offset_in_item);
        cx.notify();
    });
}

#[gpui::test]
fn my_list_navigation_restores_header_and_media_anchors(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            (1..=30)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id,
                        title_english: Some(format!("Series {id:02}")),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            0,
        );
        workspace.list.synced_at = Some(now_seconds());
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: gpui::px(35.),
        });
        workspace.remember_scroll();
        workspace.route = Route::Search;
        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: gpui::px(0.),
        });
        workspace.switch_route(Route::MyList, window, cx);
        assert_eq!(workspace.list_widget.logical_scroll_top().item_ix, 0);
        assert_eq!(
            workspace.list_widget.logical_scroll_top().offset_in_item,
            gpui::px(35.)
        );

        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: MyListSlot::item_ix(12),
            offset_in_item: gpui::px(9.),
        });
        workspace.remember_scroll();
        workspace.route = Route::Search;
        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: gpui::px(0.),
        });
        workspace.switch_route(Route::MyList, window, cx);
        let top = workspace.list_widget.logical_scroll_top();
        assert_eq!(workspace.list.visible()[top.item_ix - 1], 13);
        assert_eq!(top.offset_in_item, gpui::px(9.));
    });
}

#[gpui::test]
fn end_jump_prefetches_actual_variable_height_visible_rows(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            (1..=100)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id,
                        title_english: Some(if id >= 97 {
                            format!("Series {id} {}", "A lengthy title ".repeat(200))
                        } else {
                            format!("Series {id}")
                        }),
                        cover_medium: Some("invalid-url".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            0,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.list_widget.scroll_to_end();
        let start = workspace.list_widget.logical_scroll_top().item_ix;
        assert_eq!(start, MyListSlot::count(workspace.list.visible().len()));
        let range = MyListSlot::media_range(
            start..start.saturating_add(32),
            workspace.list.visible().len(),
        );
        workspace.prefetch_covers(range, window, cx);
        cx.notify();
    });
    let first_callbacks = cx.update(|window, app| window.simulate_next_frame(app));
    assert!(first_callbacks > 0);
    assert!(!workspace.read_with(cx, |workspace, _| {
        workspace.cover_tickets.contains_key(&(100, 46, 63))
    }));
    let callbacks = cx.update(|window, app| {
        window.draw(app).clear(app);
        window.simulate_next_frame(app)
    });
    assert!(callbacks > 0);
    cx.run_until_parked();

    workspace.read_with(cx, |workspace, _| {
        let viewport = workspace.list_widget.viewport_bounds();
        let visible: Vec<_> = workspace
            .list
            .visible()
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                workspace
                    .list_widget
                    .bounds_for_item(MyListSlot::item_ix(index))
                    .and_then(|bounds| {
                        (bounds.top() < viewport.bottom() && bounds.bottom() > viewport.top())
                            .then_some(*id)
                    })
            })
            .collect();
        assert!(!visible.is_empty());
        assert_eq!(visible.last(), Some(&100));
        assert!(
            workspace
                .list_widget
                .bounds_for_item(MyListSlot::item_ix(100 - 1))
                .unwrap()
                .size
                .height
                > gpui::px(90.)
        );
        for id in visible {
            assert!(
                workspace.cover_tickets.contains_key(&(id, 46, 63)),
                "missing cover {id}"
            );
        }
    });
}

#[gpui::test]
fn empty_and_short_lists_use_measured_cover_regions(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(vec![], 0);
        workspace.reset_list_widget();
        workspace.prefetch_visible_covers(window, cx);
        assert!(workspace.cover_tickets.is_empty());
        workspace.list.replace(
            (1..=2)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id,
                        title_english: Some(format!("Series {id}")),
                        cover_medium: Some("invalid-url".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
            0,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.prefetch_visible_covers(window, cx);
        assert!(workspace.cover_tickets.contains_key(&(1, 46, 63)));
        assert!(workspace.cover_tickets.contains_key(&(2, 46, 63)));
    });
}

#[gpui::test]
fn delayed_end_cover_callback_drops_after_generation_or_route_change(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        workspace.list.replace(
            vec![ListEntry {
                media_id: 1,
                status: "CURRENT".into(),
                media: Some(Media {
                    id: 1,
                    cover_medium: Some("invalid-url".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            0,
        );
        workspace.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.list_widget.scroll_to_end();
        let start = workspace.list_widget.logical_scroll_top().item_ix;
        workspace.prefetch_covers(start..start + 32, window, cx);
        workspace.generation += 1;
        workspace.route = Route::Search;
        cx.notify();
    });
    cx.update(|window, app| {
        window.draw(app).clear(app);
        assert!(window.simulate_next_frame(app) > 0);
        assert!(window.simulate_next_frame(app) > 0);
    });
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.cover_tickets.contains_key(&(1, 46, 63)));
        assert!(
            !workspace
                .cover_regions
                .contains_key(&CoverOwner::Route(Route::MyList))
        );
    });
}

// These fixtures share the registered OAuth callback port.
static BROWSER_SIGN_IN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn browser_sign_in_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = BROWSER_SIGN_IN_LOCK.lock().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::net::TcpListener::bind("127.0.0.1:39417").is_err() {
        assert!(
            Instant::now() < deadline,
            "Previous OAuth listener must stop"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    guard
}

struct BrowserSignInProbe(std::sync::Mutex<Vec<String>>);

impl Platform for BrowserSignInProbe {
    fn open_url(&self, url: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(url.into());
        Ok(())
    }

    fn open_path(&self, _: &Path) -> Result<(), String> {
        Err("Not used by this test".into())
    }

    fn reveal_path(&self, _: &Path) -> Result<(), String> {
        Err("Not used by this test".into())
    }
}

fn settle_browser_login(
    cx: &mut TestAppContext,
    runtime: &tokio::runtime::Runtime,
    browser: &BrowserSignInProbe,
    count: usize,
) {
    for _ in 0..200 {
        settle(cx, runtime);
        if browser.0.lock().unwrap().len() == count {
            return;
        }
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
    }
    assert_eq!(browser.0.lock().unwrap().len(), count);
}

fn deny_browser_login(url: &str) {
    use std::io::{Read, Write};
    let url = reqwest::Url::parse(url).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let redirect = url
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let port = reqwest::Url::parse(&redirect)
        .unwrap()
        .port_or_known_default()
        .unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(
        stream,
        "GET /?error=access_denied&state={state} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.contains("Authorization failed."));
}

#[gpui::test]
fn browser_sign_in_retry_rejects_the_old_callback_and_recovers(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    let bounds = cx.debug_bounds("login-oauth").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle_browser_login(cx, &runtime, &browser, 1);
    let first = browser.0.lock().unwrap()[0].clone();
    assert!(workspace.read_with(cx, |workspace, _| workspace.auth_retry_available()));
    let bounds = cx.debug_bounds("login-oauth").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle_browser_login(cx, &runtime, &browser, 2);
    let second = browser.0.lock().unwrap()[1].clone();
    assert_ne!(first, second);
    deny_browser_login(&first);
    settle(cx, &runtime);
    assert!(workspace.read_with(cx, |workspace, _| workspace.auth_busy));
    deny_browser_login(&second);
    for _ in 0..200 {
        settle(cx, &runtime);
        if !workspace.read_with(cx, |workspace, _| workspace.auth_busy) {
            break;
        }
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
    }
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.auth_busy);
        assert!(workspace.auth_abort.is_none());
        assert!(workspace.auth_error.contains("access_denied"));
        assert!(workspace.profile.is_none());
    });
}

#[gpui::test]
fn settings_browser_sign_in_can_be_cancelled_and_retried(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.route = Route::Settings;
        cx.notify();
    });
    cx.run_until_parked();
    let bounds = cx.debug_bounds("settings-sign-in").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle_browser_login(cx, &runtime, &browser, 1);
    let bounds = cx.debug_bounds("cancel-login").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.auth_busy);
        assert!(workspace.auth_abort.is_none());
        assert!(workspace.auth_error.is_empty());
        assert!(workspace.profile.is_none());
    });
    let bounds = cx.debug_bounds("settings-sign-in").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle_browser_login(cx, &runtime, &browser, 2);
    let bounds = cx.debug_bounds("cancel-login").unwrap();
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    settle(cx, &runtime);
}

#[gpui::test]
fn cancelling_unstarted_login_never_opens_a_browser(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.login(true, None, window, cx);
        workspace.cancel_login(window, cx);
    });
    settle(cx, &runtime);
    assert!(browser.0.lock().unwrap().is_empty());
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.auth_busy);
        assert!(workspace.auth_abort.is_none());
        assert!(workspace.auth_error.is_empty());
    });
}

fn stage05_entries(count: i64) -> Vec<ListEntry> {
    (1..=count)
        .map(|id| ListEntry {
            media_id: id,
            status: "CURRENT".into(),
            progress: 3,
            media: Some(Media {
                id,
                title_english: Some(format!(
                    "Series {id:05} {}",
                    "夜空 Long Title ".repeat(if id % 7 == 0 { 100 } else { 0 })
                )),
                episodes: Some(24),
                cover_medium: (id % 13 != 0).then(|| format!("invalid-{id}")),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect()
}
#[allow(clippy::question_mark)]
fn stage05_visible(w: &Workspace) -> std::collections::HashSet<(i64, u32, u32)> {
    let viewport = w.list_widget.viewport_bounds();
    w.list
        .visible()
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            if w.list
                .entry(*id)
                .and_then(|e| e.media.as_ref())
                .and_then(|m| m.cover_medium.as_ref())
                .is_none()
            {
                return None;
            }
            w.list_widget.bounds_for_item(i + 1).and_then(|b| {
                (b.top() < viewport.bottom() && b.bottom() > viewport.top())
                    .then_some((*id, 46, 63))
            })
        })
        .collect()
}
#[gpui::test]
fn stage05_content_wheel_owners_follow_new_viewport(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    w.update_in(cx, |w, _, cx| {
        w.auth_intent += 1;
        w.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        w.list.replace(stage05_entries(2000), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    w.update_in(cx, |w, window, cx| w.prefetch_visible_covers(window, cx));
    let point = w.read_with(cx, |w, _| w.list_widget.viewport_bounds().center());
    cx.simulate_event(gpui::ScrollEvent {
        position: point,
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-20000.))),
        ..Default::default()
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, app| {
            window.draw(app).clear(app);
            window.simulate_next_frame(app);
        });
        cx.run_until_parked();
    }
    w.read_with(cx, |w, _| {
        let expected = stage05_visible(w);
        assert!(!expected.is_empty());
        let actual = w.cover_regions[&CoverOwner::Route(Route::MyList)].audit_visible();
        eprintln!(
            "wheel expected={expected:?} actual={actual:?} top={:?}",
            w.list_widget.logical_scroll_top()
        );
        assert_eq!(
            actual, &expected,
            "wheel must own the new measured viewport without another input"
        );
    });
}
#[gpui::test]
fn stage05_filter_owners_follow_new_items(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    w.update_in(cx, |w, _, cx| {
        w.auth_intent += 1;
        w.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        w.list.replace(stage05_entries(2000), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    w.update_in(cx, |w, window, cx| {
        w.prefetch_visible_covers(window, cx);
        w.list.set_query("Series 01999".into(), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    for _ in 0..3 {
        cx.update(|window, app| {
            window.draw(app).clear(app);
            window.simulate_next_frame(app);
        });
        cx.run_until_parked();
    }
    w.read_with(cx, |w, _| {
        let expected = stage05_visible(w);
        assert_eq!(expected, std::collections::HashSet::from([(1999, 46, 63)]));
        let actual = w.cover_regions[&CoverOwner::Route(Route::MyList)].audit_visible();
        eprintln!("filter expected={expected:?} actual={actual:?}");
        assert_eq!(
            actual, &expected,
            "filtered covers must follow the new items without later input"
        );
    });
}

#[gpui::test]
fn stage05_empty_filter_retires_cover_owners(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (w, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    w.update_in(cx, |w, _, cx| {
        w.auth_intent += 1;
        w.profile = Some(User {
            id: 47,
            ..Default::default()
        });
        w.list.replace(stage05_entries(250), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    w.update_in(cx, |w, window, cx| {
        w.prefetch_visible_covers(window, cx);
        w.list.set_query("no matching item".into(), 0);
        w.reset_list_widget();
        cx.notify();
    });
    cx.run_until_parked();
    w.read_with(cx, |w, _| {
        eprintln!(
            "empty visible={} retained_tickets={} retained_images={}",
            w.visible_ids.len(),
            w.cover_tickets.len(),
            w.cover_images.len()
        );
        assert!(w.visible_ids.is_empty());
        assert!(
            w.cover_tickets.is_empty(),
            "Empty filters must retire old route owners"
        );
    });
}

#[gpui::test]
fn profile_format_change_resets_editor_scale_and_clear_submits_zero(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.generation = workspace.core.generation();
        let user = User {
            id: 7,
            score_format: Some("POINT_100".into()),
            ..Default::default()
        };
        workspace.profile = Some(user.clone());
        workspace.list.apply_one(
            ListEntry {
                media_id: 9,
                status: "CURRENT".into(),
                score: Some(80.0),
                ..Default::default()
            },
            now_seconds(),
        );
        workspace.open_editor(9, window, cx);
        workspace
            .editor_inputs
            .as_ref()
            .unwrap()
            .progress
            .update(cx, |input, cx| input.set_value("5", cx));
        workspace.read_editor_inputs(cx).unwrap();
        workspace.set_profile(
            Some(User {
                score_format: Some("POINT_5".into()),
                ..user
            }),
            window,
            cx,
        );
        assert_eq!(
            workspace
                .editor
                .as_ref()
                .unwrap()
                .displayed_format
                .as_deref(),
            Some("POINT_5")
        );
        assert_eq!(
            workspace
                .editor_inputs
                .as_ref()
                .unwrap()
                .score
                .read(cx)
                .value(),
            ""
        );
        assert_eq!(workspace.list.entry(9).unwrap().score, None);
        workspace.clear_editor_score(cx);
        workspace.read_editor_inputs(cx).unwrap();
        let patch = workspace.editor.as_ref().unwrap().basic_patch(true);
        assert_eq!(patch.score, Some(0.0));
        assert_eq!(patch.progress, Some(5));
    });
}
