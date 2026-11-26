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
    BarrierAction, CloseIntent, EditorInputs, PreferenceEdit, ProgressEdit, QueuedPreference,
    Route, SortKey, Workspace, freshness_plan, now_seconds,
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

#[gpui::test]
fn score_format_change_updates_visible_score_order_before_refresh(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, window, cx| {
        let user = User {
            id: 47,
            score_format: Some("POINT_10_DECIMAL".into()),
            ..Default::default()
        };
        w.profile = Some(user.clone());
        let entry = |id, title: &str, status: &str, score| ListEntry {
            media_id: id,
            status: status.into(),
            score,
            media: Some(Media {
                id,
                title_english: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        w.list.set_filter("PAUSED", 0);
        w.list.set_query(" show ".into(), 0);
        w.list.set_sort(SortKey::Score, 0);
        w.list.replace(
            vec![
                entry(1, "Show 10", "PAUSED", Some(8.4)),
                entry(2, "Show 2", "PAUSED", Some(8.4)),
                entry(3, "Show 30", "PAUSED", Some(8.6)),
                entry(4, "Show 1", "PAUSED", Some(0.0)),
                entry(5, "Show 0", "CURRENT", Some(9.9)),
                entry(6, "Other", "PAUSED", Some(9.8)),
                entry(7, "Show 3", "PAUSED", None),
            ],
            0,
        );
        assert_eq!(w.list.visible(), &[3, 2, 1, 4, 7]);
        w.list.set_desc(false, 0);
        w.reset_list_widget();
        assert_eq!(w.visible_ids.as_slice(), &[2, 1, 3, 4, 7]);
        let inset = gpui::px(13.);
        w.list_widget.scroll_to(gpui::ListOffset {
            item_ix: MyListSlot::item_ix(2),
            offset_in_item: inset,
        });
        w.set_profile(
            Some(User {
                score_format: Some("POINT_5".into()),
                ..user
            }),
            window,
            cx,
        );
        assert_eq!(w.visible_ids.as_slice(), &[4, 2, 7, 1, 3]);
        assert_eq!(w.list.filter, "PAUSED");
        assert_eq!(w.list.query, " show ");
        assert_eq!(w.list.sort, SortKey::Score);
        assert!(!w.list.desc);
        let top = w.list_widget.logical_scroll_top();
        assert_eq!(top.item_ix, MyListSlot::item_ix(4));
        assert_eq!(top.offset_in_item, inset);
        assert!(w.list_refresh_queued);
    });
    cx.update(|window, _| window.remove_window());
}

#[path = "auth_tests.rs"]
mod auth_tests;

#[cfg(feature = "fixture-runtime")]
#[path = "prompt_restore_tests.rs"]
pub(super) mod prompt_restore_tests;

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
        .block_on(core.set_workspace_preference(
            "kurisu.list.synced".into(),
            Some("123".into()),
            None,
        ))
        .unwrap();
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.synced".into()))
            .unwrap()
            .as_deref(),
        Some("123")
    );
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

pub(super) struct TestPlatform;

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

pub(super) struct FixtureDir(pub(super) PathBuf);

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn fixture() -> (Core, tokio::runtime::Runtime, FixtureDir) {
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

pub(super) fn settle(cx: &mut TestAppContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..16 {
        cx.run_until_parked();
        runtime.block_on(async { tokio::task::yield_now().await });
    }
}

pub(super) fn update_fixture() -> kurisu_core::models::UpdateInfo {
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

pub(super) fn tracking_fixture(episode: i64) -> kurisu_core::models::TrackingPrompt {
    kurisu_core::models::TrackingPrompt {
        media_id: 9,
        episode,
        title: "Synthetic".into(),
        raw_title: format!("Synthetic {episode}"),
        progress: 3,
    }
}

fn queued_value(edit: &Option<QueuedPreference>) -> Option<Option<&str>> {
    edit.as_ref().map(|edit| edit.value.as_deref())
}

fn queued_setting_value(edit: &Option<PreferenceEdit>) -> Option<Option<&str>> {
    match edit {
        Some(PreferenceEdit::Setting(edit)) => Some(edit.value.as_deref()),
        _ => None,
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
        assert_eq!(queued_value(&workspace.query_pending), Some(Some("x")));
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
    let (seed_core, runtime, dir) = fixture();
    drop(seed_core);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "token-47",
        &User {
            id: 47,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_setting("native_preference:kurisu.list.q", "old account")
        .unwrap();
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.restoring_session = false;
        workspace.stage_query(Some("old account".into()));
        workspace.save_query(window, cx);
        workspace.last_account = Some(47);
        db.replace_account(
            "token-48",
            &User {
                id: 48,
                ..Default::default()
            },
        )
        .unwrap();
        workspace.set_profile(
            Some(User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(queued_value(&workspace.query_pending), Some(None));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.query_write_active || w.query_pending.is_some()) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
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
        assert_eq!(
            queued_value(&workspace.query_pending),
            Some(Some("retained"))
        );
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
    db.set_setting("native_preference:kurisu.list.sort", "title")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.dir", "desc")
        .unwrap();
    db.0.lock().execute_batch(
        "CREATE TRIGGER fail_preferences BEFORE INSERT ON settings
        WHEN NEW.key IN ('native_preference:kurisu.list.filter', 'native_preference:kurisu.list.dir')
        BEGIN SELECT RAISE(FAIL, 'preference storage failure'); END;"
    ).unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    settle(cx, &runtime);
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
        assert!(
            workspace.preference_writes["kurisu.list.filter"]
                .error
                .as_deref()
                .is_some_and(|err| err.contains("preference storage failure"))
        );
        assert_eq!(
            queued_setting_value(&workspace.preference_writes["kurisu.list.filter"].pending),
            Some(Some("COMPLETED"))
        );
        let sort = &workspace.preference_writes["kurisu.list.sort"];
        assert!(sort.error.is_some());
        assert!(matches!(
            sort.pending,
            Some(PreferenceEdit::Sort {
                key: SortKey::Score,
                descending: false,
            })
        ));
    });
    let [sort, direction] = core.get_list_sort_preferences();
    assert_eq!(sort.unwrap().as_deref(), Some("title"));
    assert_eq!(direction.unwrap().as_deref(), Some("desc"));
    cx.update(|window, app| window.draw(app).clear(app));
    assert!(cx.debug_bounds("retry-barrier").is_some());

    workspace.update_in(cx, |workspace, window, cx| {
        workspace.cancel_barrier(window, cx)
    });
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
            workspace.cancel_barrier(window, cx);
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
    settle(cx, &runtime);
    let lock_db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    lock_db.0.lock().execute_batch("BEGIN IMMEDIATE;").unwrap();
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_sort(SortKey::Score, window, cx);
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.barrier.is_some());
    });
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert!(matches!(workspace.barrier, Some(BarrierAction::Close(_))));
        assert_eq!(workspace.preference_active, Some("kurisu.list.sort"));
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
            .block_on(core.get_preference("kurisu.list.sort".into()))
            .unwrap()
            .as_deref(),
        Some("score")
    );
}

#[gpui::test]
fn nonquery_reset_deletion_settles_after_older_value(cx: &mut TestAppContext) {
    let (seed_core, runtime, dir) = fixture();
    drop(seed_core);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "token-47",
        &User {
            id: 47,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_setting("native_preference:kurisu.list.filter", "COMPLETED")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.synced", "123")
        .unwrap();
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.restoring_session = false;
        workspace.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
        workspace.persist_preference("kurisu.list.synced", Some("123".into()), window, cx);
        workspace.last_account = Some(47);
        db.replace_account(
            "token-48",
            &User {
                id: 48,
                ..Default::default()
            },
        )
        .unwrap();
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
            queued_setting_value(&workspace.preference_writes["kurisu.list.filter"].pending),
            Some(None)
        );
        assert_eq!(
            queued_setting_value(&workspace.preference_writes["kurisu.list.synced"].pending),
            Some(None)
        );
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.preference_has_work()) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
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
    while workspace.read_with(cx, |w, _| w.preference_has_work()) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
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
    let (seed_core, runtime, dir) = fixture();
    drop(seed_core);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "token-47",
        &User {
            id: 47,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_setting("native_preference:kurisu.list.filter", "COMPLETED")
        .unwrap();
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.restoring_session = false;
        workspace.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
        workspace.last_account = Some(47);
        db.replace_account(
            "token-48",
            &User {
                id: 48,
                ..Default::default()
            },
        )
        .unwrap();
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
            queued_setting_value(&workspace.preference_writes["kurisu.list.filter"].pending),
            Some(None)
        );
        workspace.select_filter("PLANNING", window, cx);
        assert_eq!(
            queued_setting_value(&workspace.preference_writes["kurisu.list.filter"].pending),
            Some(Some("PLANNING"))
        );
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.preference_has_work()) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
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
            "CREATE TRIGGER fail_old_filter BEFORE INSERT ON settings
        WHEN NEW.key = 'native_preference:kurisu.list.filter' AND NEW.value = 'PLANNING'
        BEGIN
          SELECT RAISE(FAIL, 'old value rejected');
        END;
        CREATE TRIGGER fail_old_sort BEFORE INSERT ON settings
        WHEN NEW.key = 'native_preference:kurisu.list.sort' AND NEW.value = 'title'
        BEGIN
          SELECT RAISE(FAIL, 'old sort rejected');
        END;",
        )
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_sort(SortKey::Title, window, cx);
        assert_eq!(workspace.preference_active, Some("kurisu.list.sort"));
        workspace.select_sort(SortKey::Score, window, cx);
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
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("COMPLETED")
    );
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.sort".into()))
            .unwrap()
            .as_deref(),
        Some("score")
    );
}

#[gpui::test]
fn late_preference_read_preserves_new_local_selection(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    runtime
        .block_on(core.set_workspace_preference(
            "kurisu.list.filter".into(),
            Some("PLANNING".into()),
            None,
        ))
        .unwrap();
    runtime
        .block_on(core.set_preference("kurisu.list.sort".into(), Some("title".into())))
        .unwrap();
    runtime
        .block_on(core.set_preference("kurisu.list.dir".into(), Some("asc".into())))
        .unwrap();
    runtime
        .block_on(core.set_workspace_preference(
            "kurisu.list.synced".into(),
            Some("123".into()),
            None,
        ))
        .unwrap();
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.filter".into()))
            .unwrap()
            .as_deref(),
        Some("PLANNING")
    );
    assert_eq!(
        runtime
            .block_on(core.get_preference("kurisu.list.synced".into()))
            .unwrap()
            .as_deref(),
        Some("123")
    );
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.select_filter("COMPLETED", window, cx);
        workspace.list.synced_at = Some(456);
        workspace.persist_preference("kurisu.list.synced", Some("456".into()), window, cx);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.preferences_loaded);
        assert_eq!(workspace.list.filter, "COMPLETED");
        assert_eq!(
            (workspace.list.sort, workspace.list.desc),
            (SortKey::Title, false)
        );
        assert_eq!(workspace.list.synced_at, Some(456));
    });
    for (key, expected) in [
        ("kurisu.list.filter", "COMPLETED"),
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
fn unreadable_list_preferences_leave_other_values_and_can_reload(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.set_setting("native_preference:kurisu.list.filter", "COMPLETED")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.sort", "score")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.synced", "123")
        .unwrap();
    db.0.lock()
        .execute_batch(
            "INSERT INTO settings(key, value) VALUES
         ('native_preference:kurisu.list.q', X'FF'),
         ('native_preference:kurisu.list.dir', X'FF');",
        )
        .unwrap();
    let (view, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    view.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.list.loaded = true;
        cx.notify();
    });
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(w.preferences_loaded);
        assert!(!w.sort_preferences_loaded);
        assert_eq!(w.list.filter, "COMPLETED");
        assert_eq!(w.list.sort, SortKey::Score);
        assert_eq!(w.list.synced_at, Some(123));
        assert_eq!(w.preference_read_errors.len(), 2);
    });
    cx.simulate_accessibility_activation();
    cx.update(|window, app| window.draw(app).clear(app));
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(tree.nodes.iter().any(|(_, node)| {
        node.role() == gpui::Role::Alert
            && node.label().is_some_and(|label| {
                label.contains("Could not load the list search") && label.contains("sort direction")
            })
    }));

    view.update_in(cx, |w, window, cx| w.select_filter("PLANNING", window, cx));
    db.set_setting("native_preference:kurisu.list.q", "Synthetic")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.dir", "asc")
        .unwrap();
    let retry = cx.debug_bounds("retry-list-preference-read").unwrap();
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
    settle(cx, &runtime);
    view.read_with(cx, |w, app| {
        assert!(w.preference_read_errors.is_empty());
        assert!(w.sort_preferences_loaded);
        assert_eq!(w.list.filter, "PLANNING");
        assert_eq!(w.list.query, "Synthetic");
        assert_eq!(w.list_query.read(app).value(), "Synthetic");
        assert_eq!((w.list.sort, w.list.desc), (SortKey::Score, false));
    });
}

#[gpui::test]
fn retry_list_preferences_preserves_debounced_search(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock()
        .execute_batch(
            "INSERT INTO settings(key, value) VALUES
             ('native_preference:kurisu.list.q', X'FF'),
             ('native_preference:kurisu.list.dir', X'FF');",
        )
        .unwrap();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    view.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.list.replace(
            vec![
                ListEntry {
                    media_id: 7,
                    status: "CURRENT".into(),
                    ..Default::default()
                },
                ListEntry {
                    media_id: 8,
                    status: "CURRENT".into(),
                    ..Default::default()
                },
            ],
            0,
        );
        w.reset_list_widget();
        cx.notify();
    });
    settle(cx, &runtime);
    assert_eq!(view.read_with(cx, |w, _| w.preference_read_errors.len()), 2);
    view.update_in(cx, |w, window, cx| {
        w.list_query.update(cx, |input, cx| {
            input.replace_text_in_range(None, "#7", window, cx);
        });
    });
    db.set_setting("native_preference:kurisu.list.q", "#8")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.dir", "asc")
        .unwrap();
    cx.update(|window, app| window.draw(app).clear(app));
    let retry = cx.debug_bounds("retry-list-preference-read").unwrap();
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
    settle(cx, &runtime);
    view.read_with(cx, |w, app| {
        assert!(w.sort_preferences_loaded);
        assert!(w.preference_read_errors.is_empty());
        assert_eq!(w.list.query, "#7");
        assert_eq!(w.list_query.read(app).value(), "#7");
        assert_eq!(w.visible_ids.as_slice(), &[7]);
    });
    view.update_in(cx, |w, window, cx| w.save_query(window, cx));
    settle(cx, &runtime);
    assert_eq!(
        runtime.block_on(core.get_preference("kurisu.list.q".into())),
        Ok(Some("#7".into()))
    );
}

#[gpui::test]
fn retry_list_sort_uses_defaults_for_removed_settings(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.0.lock()
        .execute_batch(
            "INSERT INTO settings(key, value) VALUES
             ('native_preference:kurisu.list.sort', 'score'),
             ('native_preference:kurisu.list.dir', X'FF');",
        )
        .unwrap();
    let (view, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(!w.sort_preferences_loaded);
        assert_eq!((w.list.sort, w.list.desc), (SortKey::Score, true));
    });
    db.delete_setting("native_preference:kurisu.list.sort")
        .unwrap();
    db.delete_setting("native_preference:kurisu.list.dir")
        .unwrap();
    view.update_in(cx, |w, window, cx| w.load_preferences(window, cx));
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(w.sort_preferences_loaded);
        assert_eq!((w.list.sort, w.list.desc), (SortKey::Title, false));
    });
}

#[gpui::test]
fn sort_controls_are_disabled_until_preferences_restore(cx: &mut TestAppContext) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.set_setting("native_preference:kurisu.list.sort", "score")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.dir", "asc")
        .unwrap();
    let (view, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    view.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.list.loaded = true;
        cx.notify();
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    for (selector, label) in [
        ("sort-select", "Sort list"),
        ("sort-direction", "Ascending, click for descending"),
    ] {
        let (_, node) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap();
        assert!(node.is_disabled());
        assert!(!node.supports_action(gpui::AccessibleAction::Click));
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    }
    view.read_with(cx, |w, _| {
        assert!(!w.sort_open && !w.preference_has_work());
        assert_eq!((w.list.sort, w.list.desc), (SortKey::Title, false));
    });
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(w.sort_preferences_loaded);
        assert_eq!((w.list.sort, w.list.desc), (SortKey::Score, false));
    });
    let [_, direction] = core.get_list_sort_preferences();
    assert_eq!(direction.unwrap().as_deref(), Some("asc"));
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
        workspace.cancel_barrier(window, cx);
        assert!(workspace.preference_has_work());

        workspace.start_barrier(BarrierAction::Install, window, cx);
        assert_eq!(workspace.barrier, Some(BarrierAction::Install));
        workspace.cancel_barrier(window, cx);
        assert!(workspace.preference_has_work());

        workspace.login(false, Some("synthetic test token".into()), window, cx);
        assert_eq!(workspace.barrier, Some(BarrierAction::Login));
        assert!(workspace.queued_login.is_some());
        workspace.cancel_barrier(window, cx);
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
    let wait_for_preferences = |cx: &mut gpui::VisualTestContext| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while workspace.read_with(cx, |w, _| {
            w.preference_has_work() && w.preference_error().is_none()
        }) {
            assert!(
                Instant::now() < deadline,
                "Preference writes did not settle"
            );
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            cx.run_until_parked();
        }
    };
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
        wait_for_preferences(cx);
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
        workspace.cancel_barrier(window, cx);
    });
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
    }
    wait_for_preferences(cx);
    workspace.update_in(cx, |workspace, window, cx| {
        assert!(
            !workspace.preference_has_work(),
            "Active preference: {:?}, save error: {:?}",
            workspace.preference_active,
            workspace.preference_error(),
        );
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
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let payload = tracking_fixture(4);
        core.emit(EventKind::TrackingPrompt(payload.clone()));
        let retained = retained_prompt(&core);
        let event = Event {
            prompt_id: Some(retained.prompt_id.unwrap() + 1),
            ..retained.clone()
        };
        assert!(workspace.show_prompt(event.clone()));
        let first_revision = workspace.prompt_revision;
        assert!(!workspace.show_prompt(event.clone()));
        assert_eq!(workspace.prompt_revision, first_revision);
        assert!(same_prompt(
            workspace.prompt_event.as_ref().unwrap(),
            &event
        ));
        let ask = Event {
            kind: EventKind::TrackingAsk(payload),
            ..event
        };
        assert!(workspace.show_prompt(ask.clone()));
        assert!(same_prompt(workspace.prompt_event.as_ref().unwrap(), &ask));
        workspace.handle_event(retained.clone(), window, cx);
        assert!(
            workspace
                .prompt_event
                .as_ref()
                .is_some_and(|current| same_prompt(current, &retained)),
            "the retained offer must win regardless of ID allocation order"
        );
    });
}

#[gpui::test]
fn stale_prompt_confirm_does_not_ack_a_new_track(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let original = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    workspace.update_in(cx, |workspace, _, _| {
        workspace.show_prompt(original.clone())
    });
    core.emit(EventKind::TrackingPrompt(tracking_fixture(5)));
    let newer = retained_prompt(&core);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.confirm_prompt(workspace.prompt_revision, 4, window, cx);
        assert!(workspace.prompt.is_none());
        assert!(!workspace.prompt_busy);
    });
    assert!(core.event_is_current(&newer));
}

#[gpui::test]
fn successful_prompt_confirm_acknowledges_the_retained_event(cx: &mut TestAppContext) {
    let (initial, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 47,
            ..Default::default()
        },
    )
    .unwrap();
    drop(initial);
    let core = Core::from_db(
        db,
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    );
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let event = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.show_prompt(event.clone());
        workspace.confirm_prompt(workspace.prompt_revision, 4, window, cx);
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
fn skip_acknowledges_only_the_displayed_offer_across_reissue(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let first = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |w, win, cx| w.handle_event(first.clone(), win, cx));
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
    workspace.update(cx, |w, cx| {
        w.dismiss_prompt(w.prompt_revision, cx);
        assert!(w.prompt.is_none());
    });
    settle(cx, &runtime);
    assert!(core.event_is_current(&second));
    workspace.update_in(cx, |w, win, cx| {
        w.handle_event(second.clone(), win, cx);
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &second));
        w.dismiss_prompt(w.prompt_revision, cx);
        assert!(w.prompt.is_none());
    });
    settle(cx, &runtime);
    assert!(
        core.recovery_events()
            .iter()
            .all(|event| super::prompt_payload(event).is_none())
    );
    core.emit(EventKind::TrackingAsk(tracking_fixture(5)));
    let next = retained_prompt(&core);
    workspace.update_in(cx, |w, win, cx| w.handle_event(next.clone(), win, cx));
    assert!(core.event_is_current(&next));
    assert!(!core.event_is_current(&first));
}

#[gpui::test]
fn login_claims_core_intent_before_the_worker_and_cancel_converges(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |w, window, cx| {
        let before = core.generation();
        w.perform_login(false, Some("synthetic-token".into()), window, cx);
        assert_eq!(core.generation(), before + 1);
        w.cancel_login(window, cx);
        assert_eq!(core.generation(), before + 2);
        core.cancel_login();
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert!(!w.auth_busy);
        assert_eq!(w.generation, core.generation());
    });
}

#[gpui::test]
fn tracking_prompt_waits_for_deletion_editor_and_discovery_writes(cx: &mut TestAppContext) {
    for source in ["delete", "editor", "discovery", "review"] {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core.clone(), window, cx, false)
        });
        workspace.update_in(cx, |w, window, cx| {
            let prompt = tracking_fixture(4);
            let id = prompt.media_id;
            w.profile = Some(User {
                id: 71,
                ..Default::default()
            });
            if source == "delete" {
                assert!(w.progress.begin_delete(w.generation, id));
            } else if source == "editor" {
                w.list.apply_one(
                    ListEntry {
                        media_id: id,
                        status: "CURRENT".into(),
                        progress: 3,
                        ..Default::default()
                    },
                    now_seconds(),
                );
                w.open_editor(id, window, cx);
                w.editor.as_mut().unwrap().busy = true;
            } else if source == "review" {
                let row = ListEntry {
                    media_id: id,
                    progress: 3,
                    ..Default::default()
                };
                w.progress.entry(w.generation, &row).requires_review = true;
            } else {
                w.editor_adding.insert(id);
            }
            core.emit(EventKind::TrackingAsk(prompt));
            w.show_prompt(retained_prompt(&core));
            w.confirm_prompt(w.prompt_revision, 4, window, cx);
            assert!(!w.prompt_busy);
            assert_eq!(w.active_writes, 0);
            assert_eq!(w.prompt_progress_pending(id), source != "review");
            if source == "review" {
                assert!(w.prompt_error.contains("review your pending episode edit"));
            } else {
                assert!(w.prompt_error.is_empty());
                assert!(w.progress.edit(w.generation, id).is_none());
            }
        });
    }
}

#[gpui::test]
fn discovery_status_waits_for_progress_review_and_modal_dismissal(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| {
        Workspace::new_with_session_restore(core.clone(), window, cx, false)
    });
    workspace.update_in(cx, |w, window, cx| {
        w.profile = Some(User {
            id: 71,
            ..Default::default()
        });
        let row = ListEntry {
            media_id: 9,
            progress: 4,
            ..Default::default()
        };
        let media = Media {
            id: 9,
            ..Default::default()
        };
        for route in [Route::Search, Route::Seasons] {
            w.route = route.clone();
            w.progress.entry(w.generation, &row).requires_review = true;
            assert!(w.discovery_status_blocked(9));
            w.change_discovery_status(media.clone(), None, "PLANNING", route.clone(), window, cx);
            w.progress.entry(w.generation, &row).requires_review = false;
            assert!(!w.discovery_status_blocked(9));
            w.update = Some(update_fixture());
            assert!(w.discovery_status_blocked(9));
            w.change_discovery_status(media.clone(), None, "PLANNING", route, window, cx);
            w.add_media(9, "PLANNING", None, window, cx);
            w.update = None;
            assert!(!w.editor_adding.contains(&9));
            assert_eq!(w.active_writes, 0);
        }
    });
}

#[gpui::test]
fn sync_start_notifies_observers_before_work_completes(cx: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            id: 71,
            ..Default::default()
        });
        w.list.sync_error = "previous error".into();
        cx.notify();
    });
    let observed = std::rc::Rc::new(std::cell::Cell::new(false));
    let notified = observed.clone();
    let _subscription = cx.update(|_, app| app.observe(&workspace, move |_, _| notified.set(true)));
    workspace.update_in(cx, |w, window, cx| w.sync_list(window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |w, _| {
        assert!(w.list.syncing && w.list.sync_error.is_empty());
    });
    assert!(observed.get());
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
fn my_list_progress_conflict_keeps_the_two_buffered_steps(cx: &mut TestAppContext) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 7,
        offline: true,
        ..Default::default()
    };
    db.replace_account("synthetic-token", &user).unwrap();
    let media = Media {
        id: 9,
        episodes: Some(12),
        ..Default::default()
    };
    let before = ListEntry {
        id: Some(21),
        media_id: 9,
        status: "CURRENT".into(),
        progress: 5,
        media: Some(media.clone()),
        ..Default::default()
    };
    db.upsert_media(&media).unwrap();
    db.upsert_entry(&before).unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, window, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![before.clone()], 0);
        w.step_progress(9, 1, window, cx);
        w.step_progress(9, 1, window, cx);
        db.upsert_entry(&ListEntry {
            progress: 6,
            ..before
        })
        .unwrap();
        w.save_progress(9, window, cx);
    });
    settle(cx, &runtime);
    workspace.update_in(cx, |w, window, cx| {
        let edit = w.progress.edit(w.generation, 9).unwrap();
        assert!(!edit.in_flight && edit.requires_review);
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (5, 7, Some(6))
        );
        assert_eq!(w.list.entry(9).unwrap().progress, 6);
        assert!(!w.can_retry_progress_errors());
        let review = w.progress.review(w.generation, 9).unwrap();
        w.resolve_progress_review(&review, false, window, cx);
        let edit = w.progress.edit(w.generation, 9).unwrap();
        assert_eq!((edit.saved, edit.pending), (6, 6));
        assert!(!edit.requires_review);
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 6);
    cx.update(|window, _| window.remove_window());
}

#[gpui::test]
fn episode_event_reconciles_open_views_from_the_current_account_cache(cx: &mut TestAppContext) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 7,
        offline: true,
        ..Default::default()
    };
    db.replace_account("synthetic-token", &user).unwrap();
    let media = Media {
        id: 9,
        title_english: Some("Episode total fixture".into()),
        episodes: Some(12),
        ..Default::default()
    };
    let before = ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 3,
        media: Some(media.clone()),
        ..Default::default()
    };
    db.upsert_media(&Media {
        episodes: Some(14),
        ..media.clone()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        progress: 14,
        status: "COMPLETED".into(),
        ..before.clone()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.restoring_session = false;
        w.list.replace(vec![before.clone()], 0);
        w.route = Route::Anime(9);
        w.detail.start(9);
        w.detail.detail = Some(kurisu_core::models::MediaDetail {
            media: Media {
                description: Some("Full synopsis".into()),
                ..media.clone()
            },
            characters: vec![],
            relations: vec![],
            staff: vec![],
            cached_at: None,
            warning: None,
            unavailable_sections: vec![],
        });
        w.search.results.push(media);
        w.open_editor(9, win, cx);
        w.cancel_reads("editor-recs");
        w.editor.as_mut().unwrap().choose_status("COMPLETED");
        let inputs = w.editor_inputs.as_ref().unwrap();
        inputs
            .progress
            .update(cx, |input, cx| input.set_value("12", cx));
        inputs
            .notes
            .update(cx, |input, cx| input.set_value("Keep this draft", cx));
        let mut event = Event {
            generation: w.generation,
            account_id: Some(8),
            prompt_id: None,
            kind: EventKind::EpisodeUpdated(before),
        };
        w.handle_event(event.clone(), win, cx);
        assert!(!w.list_refresh_queued);
        event.account_id = Some(7);
        w.handle_event(event, win, cx);
        assert!(w.list_refresh_queued);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(350));
    // Supersede the first local read before its worker can run.
    workspace.update_in(cx, |w, win, cx| {
        w.queue_list_refresh(Some(vec![17]), win, cx)
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(350));
    settle(cx, &runtime);
    workspace.read_with(cx, |w, app| {
        let detail = &w.detail.detail.as_ref().unwrap().media;
        assert_eq!(detail.episodes, Some(14));
        assert_eq!(detail.description.as_deref(), Some("Full synopsis"));
        assert_eq!(w.detail.entry.as_ref().unwrap().progress, 14);
        assert_eq!(
            w.entry_for_edit(9).unwrap().media.unwrap().episodes,
            Some(14)
        );
        assert_eq!(w.search.results[0].episodes, Some(14));
        assert_eq!(w.editor.as_ref().unwrap().total(), Some(14));
        let inputs = w.editor_inputs.as_ref().unwrap();
        assert_eq!(inputs.progress.read(app).value(), "14");
        assert_eq!(inputs.notes.read(app).value(), "Keep this draft");
        assert!(!w.read_aborts.contains_key("detail"));
    });
}

#[gpui::test]
fn scoped_episode_total_clear_updates_discovery_and_unblocks_detail_progress(
    tc: &mut TestAppContext,
) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 7,
        offline: true,
        ..Default::default()
    };
    db.replace_account("synthetic-token", &user).unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let media = Media {
        id: 9,
        title_english: Some("Episode total fixture".into()),
        episodes: Some(12),
        ..Default::default()
    };
    let entry = ListEntry {
        id: Some(90),
        media_id: media.id,
        status: "CURRENT".into(),
        progress: 12,
        media: Some(media.clone()),
        ..Default::default()
    };
    db.upsert_entry(&entry).unwrap();
    let (workspace, cx) = tc
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, _, _| {
        w.offline = user.offline;
        w.last_reconnect_attempt = Some(Instant::now());
        w.profile = Some(user);
        w.route = Route::Anime(media.id);
        w.list.replace(vec![entry], 0);
        let request = w.detail.start(media.id);
        w.detail.finish_detail(
            media.id,
            request,
            Ok(kurisu_core::models::MediaDetail {
                media: media.clone(),
                relations: vec![],
                characters: vec![],
                staff: vec![],
                cached_at: None,
                warning: None,
                unavailable_sections: vec![],
            }),
        );
        w.search.results.push(media.clone());
        w.search.results[0].episodes = Some(13);
        w.seasons.media.push(media.clone());
    });
    let refresh = |cx: &mut gpui::VisualTestContext| {
        let read = workspace.update_in(cx, |w, window, cx| {
            assert!(!w.list.syncing);
            w.refresh_scoped(vec![media.id], 7, window, cx);
            w.read_aborts["list"][0].clone()
        });
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !read.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        settle(cx, &runtime);
        workspace.read_with(cx, |w, _| {
            assert!(w.list_read.is_none());
            assert!(w.list.error.is_empty(), "{}", w.list.error);
        });
    };
    let assert_total = |w: &Workspace, expected| {
        assert_eq!(w.detail.detail.as_ref().unwrap().media.episodes, expected);
        assert_eq!(w.seasons.media[0].episodes, expected);
    };
    refresh(cx);
    workspace.read_with(cx, |w, _| {
        assert_total(w, Some(12));
        assert_eq!(w.search.results[0].episodes, Some(13));
    });

    db.upsert_media(&media).unwrap();
    refresh(cx);
    workspace.read_with(cx, |w, _| {
        assert_total(w, Some(12));
        assert_eq!(w.search.results[0].episodes, Some(12));
    });

    db.upsert_media(&Media {
        episodes: None,
        ..media.clone()
    })
    .unwrap();
    refresh(cx);
    workspace.update_in(cx, |w, window, cx| {
        assert_total(w, None);
        assert_eq!(w.search.results[0].episodes, None);
        assert_eq!(
            w.list.entry(9).unwrap().media.as_ref().unwrap().episodes,
            None
        );
        w.step_progress(9, 1, window, cx);
        assert_eq!(w.progress.edit(w.generation, 9).unwrap().pending, 13);
    });
}

#[gpui::test]
fn deletion_refresh_clears_selected_detail_entry(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, _, cx| {
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
        workspace.reconcile_open_entries(None, cx);
        assert!(workspace.detail.entry_available);
        assert!(workspace.detail.entry.is_none());
    });
}

#[gpui::test]
fn offline_editor_refusal_blocks_close_immediately(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        let input = |value: &str, cx: &mut gpui::Context<Workspace>| {
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
        let input = |value: &str, cx: &mut gpui::Context<Workspace>| {
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
        workspace.navigate(Route::Search, window, cx);
        workspace.list_widget.scroll_to(gpui::ListOffset::default());
        workspace.back(window, cx);
        assert_eq!(workspace.list_widget.logical_scroll_top().item_ix, 0);
        assert_eq!(
            workspace.list_widget.logical_scroll_top().offset_in_item,
            gpui::px(35.)
        );

        workspace.list_widget.scroll_to(gpui::ListOffset {
            item_ix: MyListSlot::item_ix(12),
            offset_in_item: gpui::px(9.),
        });
        workspace.navigate(Route::Search, window, cx);
        workspace.list_widget.scroll_to(gpui::ListOffset::default());
        workspace.back(window, cx);
        let top = workspace.list_widget.logical_scroll_top();
        assert_eq!(workspace.list.visible()[top.item_ix - 1], 13);
        assert_eq!(top.offset_in_item, gpui::px(9.));
    });
}

#[gpui::test]
fn my_list_back_restores_latest_media_anchor(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, window, cx| {
        w.profile = Some(User {
            id: 47,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.list.replace(
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
        w.reset_list_widget();
        w.navigate(Route::Settings, window, cx);
        w.navigate(Route::MyList, window, cx);
        w.list_widget.scroll_to(gpui::ListOffset {
            item_ix: MyListSlot::item_ix(20),
            offset_in_item: gpui::px(7.),
        });
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |w, window, cx| {
        let before = w.list_widget.logical_scroll_top();
        assert_eq!(before.item_ix, 21);
        let media = w.list.visible()[before.item_ix - 1];
        w.back(window, cx);
        assert_eq!(w.route, Route::Settings);
        w.list.desc = !w.list.desc;
        w.list.rebuild(now_seconds());
        w.reset_list_widget();
        w.back(window, cx);
        assert_eq!(w.route, Route::MyList);
        let after = w.list_widget.logical_scroll_top();
        assert_ne!(after.item_ix, before.item_ix);
        assert_eq!(w.list.visible()[after.item_ix - 1], media);
        assert_eq!(after.offset_in_item, before.offset_in_item);
    });
    cx.run_until_parked();
    workspace.read_with(cx, |w, _| {
        let top = w.list_widget.logical_scroll_top();
        assert_eq!(w.list.visible()[top.item_ix - 1], 21);
        assert_eq!(top.offset_in_item, gpui::px(7.));
    });
}

#[gpui::test]
fn my_list_renders_episode_totals_above_i64_max(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx
        .add_window_view(|window, cx| Workspace::new_with_session_restore(core, window, cx, false));
    workspace.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            id: 47,
            offline: true,
            ..Default::default()
        });
        w.list.replace(
            [(1, i64::MAX), (2, 2)]
                .into_iter()
                .map(|(media_id, progress)| ListEntry {
                    media_id,
                    progress,
                    status: "CURRENT".into(),
                    ..Default::default()
                })
                .collect(),
            0,
        );
        w.reset_list_widget();
        cx.notify();
    });
    cx.update(|window, app| window.draw(app).clear(app));
    assert!(cx.debug_bounds("my-list-heading").is_some());
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.list.tracked_episodes(), i128::from(i64::MAX) + 2);
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

#[gpui::test]
fn set_profile_and_reconnect_recover_dropped_tracking_ask_without_resurrecting_dismissed(
    cx: &mut TestAppContext,
) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        episodes: Some(12),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 3,
        ..Default::default()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    workspace.update(cx, |w, _| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 0,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
    });
    settle(cx, &runtime);

    core.emit(EventKind::TrackingAsk(tracking_fixture(4)));
    let expected = retained_prompt(&core);
    core.emit(EventKind::UpdateFailed("consumed".into()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.update_failed != "consumed") {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
    }
    workspace.update_in(cx, |w, win, cx| {
        assert!(w.prompt.is_none());
        w.update_failed.clear();
        w.set_profile(
            Some(User {
                id: 7,
                name: "Saved".into(),
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        assert!(w.prompt_event.is_none(), "prompt recovery must be deferred");
    });
    settle(cx, &runtime);
    workspace.update_in(cx, |w, win, cx| {
        assert_eq!(w.route, Route::Now);
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &expected));
        w.dismiss_prompt(w.prompt_revision, cx);
        assert!(w.prompt.is_none());
        w.restore_session(win, cx);
        assert!(w.prompt.is_none());
    });
    settle(cx, &runtime);
    assert!(workspace.read_with(cx, |w, _| w.prompt.is_none()));
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, db, core, runtime));
}

#[gpui::test]
fn dismissed_tracking_ask_lag_replay_does_not_navigate_or_resurrect_while_ack_is_pending(
    cx: &mut TestAppContext,
) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        episodes: Some(12),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 3,
        ..Default::default()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.set_profile(
            Some(User {
                id: 7,
                name: "Saved".into(),
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
    });
    settle(cx, &runtime);
    assert_eq!(
        workspace.read_with(cx, |w, _| w.route.clone()),
        Route::MyList
    );

    core.emit(EventKind::TrackingAsk(tracking_fixture(4)));
    let expected = retained_prompt(&core);
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.prompt.is_none()) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
    }
    assert_eq!(workspace.read_with(cx, |w, _| w.route.clone()), Route::Now);

    for ep in 0..300 {
        core.emit(EventKind::WatchHistoryUpdated(ep));
    }
    cx.run_until_parked();
    runtime.block_on(async { tokio::task::yield_now().await });

    workspace.update_in(cx, |w, win, cx| {
        w.dismiss_prompt(w.prompt_revision, cx);
        w.navigate(Route::MyList, win, cx);
        assert_eq!(w.route, Route::MyList);
        assert!(w.prompt.is_none());
    });
    assert!(
        core.event_is_current(&expected),
        "acknowledgement must still be pending in flight"
    );

    cx.run_until_parked();
    assert!(
        core.event_is_current(&expected),
        "lagged replay must run while acknowledgement is still in flight"
    );
    workspace.read_with(cx, |w, _| {
        assert_eq!(
            w.route,
            Route::MyList,
            "lagged replay of already-dismissed TrackingAsk navigated back to Now"
        );
        assert!(w.prompt.is_none());
    });

    while core.event_is_current(&expected) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, db, core, runtime));
}

#[gpui::test]
fn library_add_folder_and_watching_update_button_respect_busy_and_close_fences(
    cx: &mut TestAppContext,
) {
    let (core, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        title_english: Some("Steins;Gate".into()),
        episodes: Some(24),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        id: Some(90),
        media_id: 9,
        status: "CURRENT".into(),
        progress: 2,
        ..Default::default()
    })
    .unwrap();
    drop(core);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let folder = dir.0.join("anime");
    std::fs::create_dir(&folder).unwrap();
    let (workspace, cx) =
        cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    cx.simulate_resize(gpui::size(gpui::px(1100.), gpui::px(900.)));
    settle(cx, &runtime);

    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.set_profile(
            Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        w.auth_busy = true;
        w.add_library_folder(folder.to_string_lossy().into_owned(), win, cx);
        assert_eq!(
            w.active_writes, 0,
            "add_library_folder admitted write during auth_busy"
        );
        w.auth_busy = false;
        w.update_busy = true;
        w.add_library_folder(folder.to_string_lossy().into_owned(), win, cx);
        assert_eq!(
            w.active_writes, 0,
            "add_library_folder admitted write during update_busy"
        );
        w.update_busy = false;

        w.now_playing = Some(kurisu_core::models::NowPlaying {
            active: true,
            player: "mpv".into(),
            title: "Steins;Gate".into(),
            matched: Some("Steins;Gate".into()),
            media_id: Some(9),
            episode: Some(3),
            position_us: 0,
            length_us: 1_000_000,
        });
        w.route = Route::Now;
        w.enter_watching(win, cx);
    });
    cx.simulate_accessibility_activation();
    let deadline = Instant::now() + Duration::from_secs(5);
    let open_btn = loop {
        settle(cx, &runtime);
        if let Some(tree) = cx.take_accessibility_tree_update()
            && let Some((_, button)) = tree.nodes.into_iter().find(|(_, node)| {
                node.role() == gpui::Role::Button && node.label() == Some("Update to Ep 3")
            })
        {
            break button;
        }
        assert!(
            Instant::now() < deadline,
            "watching update button before close"
        );
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    };
    assert!(!open_btn.is_disabled());

    workspace.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Quit, win, cx);
        assert!(w.close_ready);
    });
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        tree.nodes
            .iter()
            .all(|(_, node)| node.label() != Some("Update to Ep 3") || node.is_hidden()),
        "watching update button stayed accessible during close"
    );
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, runtime));
}

#[gpui::test]
fn recovery_prompts_defer_during_close_ready_and_replay_on_resume(cx: &mut TestAppContext) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        episodes: Some(12),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 3,
        ..Default::default()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    core.emit(EventKind::TrackingAsk(tracking_fixture(4)));
    let expected = retained_prompt(&core);
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 7,
            name: "Saved".into(),
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        w.set_profile(
            Some(User {
                id: 7,
                name: "Saved".into(),
                offline: false,
                ..Default::default()
            }),
            win,
            cx,
        );
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert!(w.close_ready);
        assert!(w.prompt.is_none());
        assert!(w.prompt_event.is_none());
    });
    workspace.update_in(cx, |w, win, cx| {
        w.resume_after_close_visibility(true, win, cx);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.route, Route::Now);
        assert!(same_prompt(
            w.prompt_event
                .as_ref()
                .expect("deferred recovery prompt was not replayed on resume"),
            &expected
        ));
    });
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, db, core, runtime));
}

#[gpui::test]
fn post_quit_events_do_not_spawn_reads_on_watching_route(cx: &mut TestAppContext) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        title_english: Some("Steins;Gate".into()),
        episodes: Some(24),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 2,
        ..Default::default()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.set_profile(
            Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        w.route = Route::Now;
        w.enter_watching(win, cx);
    });
    settle(cx, &runtime);
    workspace.update_in(cx, |w, _, _| {
        w.cancel_reads_for_quit();
        assert!(
            w.event_stop.lock().unwrap().is_none(),
            "cancel_reads_for_quit left event subscription running"
        );
    });
    let updated = ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 3,
        ..Default::default()
    };
    core.emit(EventKind::EpisodeUpdated(updated.clone()));
    core.emit(EventKind::NowPlaying(kurisu_core::models::NowPlaying {
        active: true,
        player: "mpv".into(),
        title: "Steins;Gate".into(),
        matched: Some("Steins;Gate".into()),
        media_id: Some(9),
        episode: Some(4),
        position_us: 0,
        length_us: 1_000_000,
    }));
    workspace.update_in(cx, |w, win, cx| {
        w.handle_event(
            Event {
                generation: w.generation,
                account_id: Some(7),
                prompt_id: None,
                kind: EventKind::EpisodeUpdated(updated),
            },
            win,
            cx,
        );
    });
    cx.executor().advance_clock(Duration::from_millis(350));
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert!(w.read_aborts.is_empty());
        assert!(w.read_lifetime.lock().unwrap().is_empty());
        assert!(w.event_stop.lock().unwrap().is_none());
    });
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, db, core, runtime));
}

#[gpui::test]
fn active_barrier_save_failure_exposes_discard_barrier_even_with_inactive_drafts(
    cx: &mut TestAppContext,
) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|win, cx| Workspace::new(core, win, cx));
    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 47,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        let mut draft = ProgressEdit::new(2, Some(12));
        draft.pending = 3;
        draft.requires_review = true;
        draft.error = Some("unsaved".into());
        w.progress_drafts.insert(48, [(9, draft)].into());
        w.query_error = "controlled query failure".into();
        w.request_close(CloseIntent::Quit, win, cx);
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(
        cx.debug_bounds("discard-barrier").is_some(),
        "Close BarrierError hid Close without saving behind inactive account drafts"
    );
    assert!(cx.debug_bounds("retry-barrier").is_some());
    let cancel = cx.debug_bounds("cancel-failed-barrier").unwrap();
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());

    workspace.update_in(cx, |w, win, cx| {
        w.query_error = "controlled query failure".into();
        w.start_barrier(BarrierAction::Logout, win, cx);
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(
        cx.debug_bounds("discard-barrier").is_some(),
        "Logout BarrierError hid Continue without saving behind inactive account drafts"
    );
    assert!(cx.debug_bounds("retry-barrier").is_some());
    let cancel = cx.debug_bounds("cancel-failed-barrier").unwrap();
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());

    workspace.update_in(cx, |w, win, cx| {
        w.query_error.clear();
        w.request_close(CloseIntent::Quit, win, cx);
        assert!(
            w.barrier_error
                .contains("Unsaved drafts belong to another account")
        );
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(cx.debug_bounds("discard-barrier").is_none());
    assert!(cx.debug_bounds("retry-barrier").is_none());
    assert!(cx.debug_bounds("cancel-failed-barrier").is_some());
}

#[gpui::test]
fn discard_barrier_clears_failed_progress_behind_inflight_closes_dirty_editor_and_drops_pending_prefs(
    cx: &mut TestAppContext,
) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|win, cx| Workspace::new(core.clone(), win, cx));
    workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 47,
            score_format: Some("POINT_100".into()),
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        let show_a = ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            progress: 2,
            media: Some(Media {
                id: 1,
                episodes: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        };
        let show_b = ListEntry {
            media_id: 2,
            status: "CURRENT".into(),
            progress: 4,
            media: Some(Media {
                id: 2,
                episodes: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        };
        w.list.replace(vec![show_a.clone(), show_b.clone()], 0);
        let edit_a = w.progress.entry(w.generation, &show_a);
        assert!(edit_a.step(1, Instant::now()));
        assert_eq!(edit_a.begin_save(), Some((3, 2)));
        w.active_writes = 1;
        let edit_b = w.progress.entry(w.generation, &show_b);
        assert!(edit_b.step(1, Instant::now()));
        assert_eq!(edit_b.begin_save(), Some((5, 4)));
        edit_b.finish_save(Err("offline".into()));

        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.barrier_error.contains("offline"));
        w.discard_barrier(win, cx);
        assert!(
            w.barrier_error.is_empty(),
            "discard_barrier left failed progress error while sibling was in flight"
        );
        assert!(w.progress.edit(w.generation, 2).is_none());
        assert!(!w.close_ready);

        w.active_writes = 0;
        w.progress
            .edit_mut(w.generation, 1)
            .unwrap()
            .finish_save(Ok(3));
        w.check_barrier(win, cx);
        assert!(w.close_ready);

        w.resume_after_close_visibility(true, win, cx);
        w.open_editor(1, win, cx);
        w.editor.as_mut().unwrap().edit_progress(7);
        w.editor_inputs
            .as_ref()
            .unwrap()
            .progress
            .update(cx, |input, cx| input.set_value("7", cx));
        w.stage_query(Some("unsubmitted".into()));
        w.query_error = "query failed".into();
        let pref = w.preference_writes.entry("kurisu.list.sort").or_default();
        pref.pending = Some(PreferenceEdit::Sort {
            key: SortKey::Score,
            descending: true,
        });

        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.barrier_error.contains("query failed"));
        w.discard_barrier(win, cx);
        assert!(
            w.editor.is_none(),
            "discard_barrier did not close clean-error dirty editor"
        );
        assert!(w.query_pending.is_none());
        assert!(!w.preference_has_work());
        assert_eq!(w.active_writes, 0);
        assert!(w.close_ready);
    });
}

#[cfg(feature = "fixture-runtime")]
#[gpui::test]
fn hidden_resume_after_close_skips_cover_prefetch_and_list_sync_on_replayed_list(
    cx: &mut TestAppContext,
) {
    const CHILD: &str = "KURISU_HIDDEN_RESUME_LIST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "screens::workspace_tests::hidden_resume_after_close_skips_cover_prefetch_and_list_sync_on_replayed_list",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("KURISU_FIXTURE_GRAPHQL", "http://127.0.0.1:9/graphql")
            .status()
            .unwrap();
        assert!(status.success(), "hidden resume child failed");
        return;
    }
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 11,
        title_english: Some("Steins;Gate".into()),
        episodes: Some(24),
        cover_medium: Some("http://127.0.0.1:9/cover.jpg".into()),
        ..Default::default()
    })
    .unwrap();
    db.upsert_entry(&ListEntry {
        media_id: 11,
        status: "CURRENT".into(),
        progress: 4,
        ..Default::default()
    })
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) =
        cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !workspace.read_with(cx, |w, _| w.preferences_loaded) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    let sync_before = workspace.update_in(cx, |w, win, cx| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 7,
            name: "Saved".into(),
            offline: false,
            ..Default::default()
        });
        w.offline = false;
        w.last_sync_attempt = None;
        assert!(w.sync_due());
        w.cover_regions.clear();
        w.cover_tickets.clear();
        w.close_ready = true;
        w.queue_list_refresh(None, win, cx);
        assert!(w.list_refresh_deferred);
        let sync_before = w.sync_request;
        w.resume_after_close_visibility(false, win, cx);
        sync_before
    });
    while !workspace.read_with(cx, |w, _| w.list.loaded) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.list.len(), 1);
        assert!(
            w.cover_tickets.is_empty(),
            "hidden resume_after_close_visibility prefetched covers"
        );
        assert!(
            w.last_sync_attempt.is_none() && w.sync_request == sync_before,
            "hidden resume_after_close_visibility started list sync"
        );
    });
    cx.update(|win, _| win.remove_window());
    cx.run_until_parked();
    drop((workspace, db, runtime));
}

#[gpui::test]
fn bucket_b_stale_recovery_keeps_new_live_prompt(cx: &mut TestAppContext) {
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 7,
        ..Default::default()
    };
    db.replace_account("synthetic-token", &user).unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let (older, worker) = workspace.update_in(cx, |w, win, cx| {
        w.stop_event_subscription();
        w.profile = Some(user);
        w.route = Route::Settings;
        core.emit(EventKind::TrackingAsk(tracking_fixture(4)));
        let older = retained_prompt(&core);
        w.refresh_recovery_prompts(win, cx);
        (older, w.read_aborts["recovery-prompts"][0].clone())
    });
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !worker.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
    core.emit(EventKind::TrackingPrompt(tracking_fixture(5)));
    let newer = retained_prompt(&core);
    assert!(newer.prompt_id > older.prompt_id);
    workspace.update_in(cx, |w, win, cx| w.handle_event(newer.clone(), win, cx));
    cx.run_until_parked();
    workspace.update_in(cx, |w, win, cx| {
        assert_eq!(
            w.prompt_event.as_ref().and_then(|e| e.prompt_id),
            newer.prompt_id,
            "stale recovery erased the live prompt"
        );
        assert_eq!(w.route, Route::Settings);
        w.prompt = None;
        w.prompt_event = None;
        w.handle_event(older, win, cx);
        assert_eq!(w.route, Route::Settings, "stale ask took over the route");
    });
}

#[gpui::test]
fn bucket_b_completed_editor_caps_progress_input(cx: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (workspace, cx) =
        cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    workspace.update_in(cx, |w, win, cx| {
        w.list.replace(vec![completed_fixture()], 0);
        w.open_editor(9, win, cx);
        let input = w.editor_inputs.as_ref().unwrap().progress.clone();
        input.update(cx, |input, cx| {
            input.replace_text_in_range(Some(0..2), "13", win, cx)
        });
    });
    workspace.read_with(cx, |w, cx| {
        let editor = w.editor.as_ref().unwrap();
        assert_eq!(editor.status, "COMPLETED");
        assert_eq!(
            editor.progress, 12,
            "Completed editor exceeded its episode total"
        );
        assert_eq!(
            w.editor_inputs.as_ref().unwrap().progress.read(cx).value(),
            "12"
        );
    });
}

fn completed_fixture() -> ListEntry {
    ListEntry {
        media_id: 9,
        status: "COMPLETED".into(),
        progress: 12,
        media: Some(Media {
            id: 9,
            episodes: Some(12),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[gpui::test]
fn cancel_failed_close_save_restores_editor_input(tc: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(core, win, cx));
    settle(cx, &runtime);
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.list.replace(
            vec![ListEntry {
                media_id: 9,
                progress: 2,
                status: "CURRENT".into(),
                ..Default::default()
            }],
            0,
        );
        w.open_editor(9, win, cx);
        w.editor_inputs
            .as_ref()
            .unwrap()
            .progress
            .update(cx, |field, cx| {
                field.replace_text_in_range(Some(0..1), "3", win, cx);
            });
        w.request_close(CloseIntent::Window, win, cx);
        assert_eq!(w.active_writes, 1);
    });
    settle(cx, &runtime);
    view.update_in(cx, |w, win, cx| {
        assert!(w.barrier_error.contains("Could not save entry"));
        w.cancel_barrier(win, cx);
        w.editor_inputs
            .as_ref()
            .unwrap()
            .progress
            .update(cx, |field, cx| {
                assert_eq!(field.value(), "3");
                field.replace_text_in_range(Some(0..1), "4", win, cx);
                assert_eq!(field.value(), "4");
            });
    });
}

#[gpui::test]
fn scoped_list_read_cannot_change_workspace_after_quit(tc: &mut TestAppContext) {
    use super::{PendingEditor, PendingEditorWait};

    let (core, runtime, _dir) = fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.pending_editor = Some(PendingEditor {
            id: 9,
            route: w.route.clone(),
            wait: PendingEditorWait::Refresh,
            auto_open: true,
        });
        w.refresh_scoped(vec![9], 7, win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
        assert!(w.stopping);
    });
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(w.list.error.is_empty());
        assert!(matches!(
            w.pending_editor.as_ref().unwrap().wait,
            PendingEditorWait::Refresh
        ));
    });
}

#[gpui::test]
fn detail_and_pending_completions_leave_stopping_workspace_alone(tc: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let request = view.update_in(cx, |w, win, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.route = Route::Anime(9);
        w.load_detail(9, win, cx);
        w.pending_error = "Pending state before quit".into();
        w.refresh_pending(win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
        assert!(w.stopping);
        let request = w.detail.request();
        w.load_detail(9, win, cx);
        w.refresh_pending(win, cx);
        assert!(!w.read_aborts.contains_key("detail") && !w.read_aborts.contains_key("pending"));
        request
    });
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert_eq!(w.detail.request(), request);
        assert!(w.detail.detail_error.is_empty());
        assert!(w.detail.entry_error.is_empty());
        assert!(w.detail.recommendation_error.is_empty());
        assert_eq!(w.pending_error, "Pending state before quit");
    });
}

#[gpui::test]
fn quit_aborted_secondary_screen_reads_do_not_populate_errors(tc: &mut TestAppContext) {
    let (core, runtime, _dir) = fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(User {
            id: 7,
            offline: false,
            ..Default::default()
        });
        w.offline = false;
        w.enter_calendar(win, cx);
        w.enter_seasons(win, cx);
        w.search.resume = Some(("Steins".into(), false));
        w.enter_search(win, cx);
        w.enter_stats(win, cx);
        w.enter_notifications(win, cx);
        for group in [
            "calendar-preference",
            "calendar",
            "seasons",
            "search",
            "stats",
            "notifications-page",
        ] {
            assert!(
                w.read_aborts.contains_key(group),
                "expected active read group {group}"
            );
        }
        w.cancel_reads_for_quit();
        assert!(w.stopping && !w.writes_fenced());
        assert!(w.read_aborts.is_empty());
    });
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert!(
            w.calendar.error.is_empty(),
            "calendar: {}",
            w.calendar.error
        );
        assert!(!w.calendar.preference_loaded);
        assert!(!w.calendar.loaded_once);
        assert!(w.seasons.error.is_empty(), "seasons: {}", w.seasons.error);
        assert!(w.search.error.is_empty(), "search: {}", w.search.error);
        assert!(w.stats.error.is_empty(), "stats: {}", w.stats.error);
        assert!(
            w.notifications.error.is_empty(),
            "notifications: {}",
            w.notifications.error
        );
    });
    view.update_in(cx, |w, win, cx| {
        w.calendar.preference_loading = false;
        w.calendar.loading = false;
        w.calendar.offset = 2;
        w.seasons.loading = false;
        w.seasons.selected_year = "invalid".into();
        let season_before = (w.seasons.season.clone(), w.seasons.year);
        w.search.searching = false;
        w.stats.loading = false;
        w.notifications.loading = false;
        w.notifications.unread = Some(3);
        w.enter_calendar(win, cx);
        w.shift_calendar(1, win, cx);
        w.reset_calendar_today(win, cx);
        w.enter_seasons(win, cx);
        w.shift_season(1, win, cx);
        w.jump_season(win, cx);
        w.search.resume = Some(("Steins".into(), false));
        w.enter_search(win, cx);
        w.enter_stats(win, cx);
        w.enter_notifications(win, cx);
        w.mark_notifications_read(win, cx);
        let year_input = w.seasons.year_input.clone().unwrap();
        let title_input = w.seasons.title_input.clone().unwrap();
        let search_input = w.search.input.clone().unwrap();
        year_input.update(cx, |input, cx| {
            input.set_value("2030", cx);
            cx.emit(crate::components::input::TextInputEvent::Changed(
                "2030".into(),
            ));
        });
        title_input.update(cx, |input, cx| {
            input.set_value("gate", cx);
            cx.emit(crate::components::input::TextInputEvent::Changed(
                "gate".into(),
            ));
        });
        search_input.update(cx, |input, cx| {
            input.set_value("clannad", cx);
            cx.emit(crate::components::input::TextInputEvent::Changed(
                "clannad".into(),
            ));
        });
        assert!(!w.calendar.preference_loading);
        assert!(!w.calendar.loading);
        assert_eq!(w.calendar.offset, 2);
        assert!(!w.seasons.loading);
        assert_eq!((w.seasons.season.clone(), w.seasons.year), season_before);
        assert!(w.seasons.error.is_empty());
        assert!(!w.search.searching);
        assert_eq!(w.search.resume, Some(("Steins".into(), false)));
        assert!(!w.stats.loading);
        assert!(!w.notifications.loading);
        assert!(!w.notifications.marking);
        assert_eq!(w.active_writes, 0);
        assert!(w.read_aborts.is_empty());
    });
    cx.run_until_parked();
    view.read_with(cx, |w, _| {
        assert_eq!(w.seasons.selected_year, "invalid");
        assert!(w.seasons.title_filter.is_empty());
        assert!(w.search.query.is_empty());
    });
}

#[gpui::test]
fn editor_status_choices_and_alerts_are_available_inside_modal(tc: &mut TestAppContext) {
    use gpui::accesskit::{Action, ActionRequest, TreeId};

    let (core, _runtime, _dir) = fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, _, cx| {
        w.profile = Some(User {
            id: 7,
            offline: true,
            ..Default::default()
        });
        w.list.replace(
            vec![ListEntry {
                media_id: 9,
                progress: 2,
                status: "CURRENT".into(),
                media: Some(Media {
                    id: 9,
                    episodes: Some(12),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            0,
        );
        w.global_error = "Could not open the sample file".into();
        w.set_tray_error(Some("Could not hide Kurisu".into()), cx);
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(tree.nodes.iter().any(|(_, node)| {
        node.label() == Some("Could not hide Kurisu")
            && node.role() == gpui::Role::Alert
            && !node.is_hidden()
    }));
    view.update_in(cx, |w, win, cx| w.open_editor(9, win, cx));
    cx.update(|win, app| win.draw(app).clear(app));
    let tree = cx.take_accessibility_tree_update().unwrap();
    let click = |cx: &gpui::VisualTestContext, tree: &gpui::accesskit::TreeUpdate, label| {
        let (id, node) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap();
        assert!(
            !node.is_hidden() && node.supports_action(Action::Click),
            "{label}"
        );
        cx.simulate_accessibility_action(ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: *id,
            data: None,
        });
    };
    for label in ["Could not open the sample file", "Could not hide Kurisu"] {
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some(label) && node.role() == gpui::Role::Alert && !node.is_hidden()
        }));
    }
    click(cx, &tree, "Dismiss");
    view.read_with(cx, |w, _| {
        assert!(w.global_error.is_empty() && w.editor.is_some())
    });
    let status = cx.debug_bounds("editor-status").unwrap();
    cx.simulate_click(status.center(), gpui::Modifiers::default());
    cx.update(|win, app| win.draw(app).clear(app));
    let tree = cx.take_accessibility_tree_update().unwrap();
    click(cx, &tree, "Completed");
    view.read_with(cx, |w, _| {
        let editor = w.editor.as_ref().unwrap();
        assert_eq!(editor.status, "COMPLETED");
        assert_eq!(editor.progress, 12);
    });
}
