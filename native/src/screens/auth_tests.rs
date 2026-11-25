use super::*;
use gpui::{KeyDownEvent, KeyUpEvent, Keystroke, VisualTestContext};

fn press(cx: &mut VisualTestContext, key: &str) {
    let os = cx.update(|_, app| app.platform_info().os);
    let keystroke = Keystroke::parse_with_os(key, os).unwrap();
    cx.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.simulate_event(KeyUpEvent { keystroke });
}

fn keyboard_login(cx: &mut TestAppContext, route: Route, key: &str) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, _, cx| {
        workspace.route = route;
        cx.notify();
    });
    cx.run_until_parked();
    workspace.update_in(cx, |_, window, cx| window.focus_next(cx));
    cx.run_until_parked();
    press(cx, key);
    settle_browser_login(cx, &runtime, &browser, 1);
    assert!(workspace.read_with(cx, |workspace, _| workspace.auth_retry_available()));
    let cancel = cx
        .debug_bounds("cancel-login")
        .expect("Recovery controls must render");
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());
    settle(cx, &runtime);
    workspace.read_with(cx, |workspace, _| {
        assert!(!workspace.auth_busy);
        assert!(workspace.auth_abort.is_none());
    });
}

#[gpui::test]
fn keyboard_login_sheet_enter_renders_recovery(cx: &mut TestAppContext) {
    keyboard_login(cx, Route::MyList, "enter");
}

#[gpui::test]
fn keyboard_login_sheet_space_renders_recovery(cx: &mut TestAppContext) {
    keyboard_login(cx, Route::MyList, "space");
}

#[gpui::test]
fn keyboard_settings_login_renders_recovery(cx: &mut TestAppContext) {
    keyboard_login(cx, Route::Settings, "enter");
}

#[gpui::test]
fn token_submission_keeps_one_job_and_can_replace_browser_login(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let (core, runtime, _dir) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.login(true, None, window, cx);
        let browser_intent = workspace.auth_intent;
        workspace.login(false, Some("synthetic-token".into()), window, cx);
        assert_eq!(workspace.auth_intent, browser_intent + 1);
        assert!(!workspace.auth_oauth);
        let token_job = workspace.auth_abort.as_ref().unwrap().id();
        workspace.login(false, Some("duplicate-token".into()), window, cx);
        assert_eq!(workspace.auth_intent, browser_intent + 1);
        assert_eq!(workspace.auth_abort.as_ref().unwrap().id(), token_job);
        workspace.cancel_login(window, cx);
    });
    settle(cx, &runtime);
}

#[gpui::test]
fn closing_auth_window_stops_listener_with_workspace_retained(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.login(true, None, window, cx);
    });
    settle_browser_login(cx, &runtime, &browser, 1);
    let abort = workspace.update_in(cx, |workspace, window, cx| {
        let abort = workspace.auth_abort.as_ref().unwrap().clone();
        workspace.request_close(CloseIntent::Quit, window, cx);
        assert!(workspace.close_ready);
        assert!(!workspace.auth_retry_available());
        abort
    });
    cx.update(|window, _| window.remove_window());
    settle(cx, &runtime);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if abort.is_finished() && std::net::TcpListener::bind("127.0.0.1:39417").is_ok() {
            break;
        }
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        cx.run_until_parked();
    }
    assert!(abort.is_finished());
    assert!(std::net::TcpListener::bind("127.0.0.1:39417").is_ok());
    workspace.read_with(cx, |workspace, _| {
        assert!(workspace.auth_abort.is_none());
        assert!(!workspace.auth_busy);
    });
}

#[gpui::test]
fn destroying_workspace_aborts_unstarted_auth(cx: &mut TestAppContext) {
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.login(true, None, window, cx);
    });
    let abort = workspace.read_with(cx, |workspace, _| {
        workspace.auth_abort.as_ref().unwrap().clone()
    });
    let weak = workspace.downgrade();
    cx.update(|window, _| window.remove_window());
    drop(workspace);
    settle(cx, &runtime);
    assert!(weak.upgrade().is_none());
    assert!(abort.is_finished());
    assert!(browser.0.lock().unwrap().is_empty());
    assert!(std::net::TcpListener::bind("127.0.0.1:39417").is_ok());
}

#[gpui::test]
fn confirmed_logout_survives_quit_and_preference_retry(cx: &mut TestAppContext) {
    let (first_core, runtime, directory) = fixture();
    drop(first_core);
    let path = directory.0.join("kurisu.db");
    let db = kurisu_core::Db::open(&path).unwrap();
    let user = User {
        id: 71,
        ..Default::default()
    };
    db.replace_account("saved-session", &user).unwrap();
    db.0.lock()
        .execute_batch(
            "CREATE TRIGGER reject_filter BEFORE INSERT ON settings
         WHEN NEW.key='native_preference:kurisu.list.filter'
         BEGIN SELECT RAISE(ABORT, 'filter unavailable'); END;",
        )
        .unwrap();
    let core = Core::open(
        directory.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, window, cx| {
        w.profile = Some(user);
        w.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
        w.start_barrier(BarrierAction::Logout, window, cx);
        w.request_close(CloseIntent::Quit, window, cx);
        assert!(!w.close_ready);
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert!(w.barrier_error.contains("filter unavailable"));
        assert!(!w.auth_busy);
        assert!(!w.close_ready);
    });
    assert!(db.cached_user().unwrap().is_some());
    db.0.lock()
        .execute_batch("DROP TRIGGER reject_filter")
        .unwrap();
    cx.update(|window, app| window.draw(app).clear(app));
    let retry = cx.debug_bounds("retry-barrier").unwrap();
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !workspace.read_with(cx, |w, _| w.close_ready) {
        assert!(Instant::now() < deadline, "quit did not finish");
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    drop(workspace);
    cx.run_until_parked();
    drop(db);
    drop(runtime);
    let db = kurisu_core::Db::open(&path).unwrap();
    assert!(db.cached_user().unwrap().is_none());
    assert!(db.get_setting("anilist_token").unwrap().is_none());
    assert_eq!(
        db.get_setting("native_preference:kurisu.list.filter")
            .unwrap()
            .as_deref(),
        Some("COMPLETED")
    );
}

#[gpui::test]
fn unknown_offline_identity_does_not_reset_known_list_owner(cx: &mut TestAppContext) {
    let (core, runtime, _directory) = fixture();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, window, cx| {
        w.last_account = Some(71);
        w.list.query = " Saved search ".into();
        w.list.filter = "PAUSED".into();
        let previous_reset = w.preference_reset;
        for id in [0, i64::from(i32::MAX) + 1] {
            w.set_profile(
                Some(User {
                    id,
                    offline: true,
                    ..Default::default()
                }),
                window,
                cx,
            );
            assert_eq!(w.last_account, Some(71));
            assert_eq!(w.preference_reset, previous_reset);
            assert_eq!(w.list.query, " Saved search ");
            assert_eq!(w.list.filter, "PAUSED");
        }
    });
    settle(cx, &runtime);
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
}

#[gpui::test]
fn queued_search_and_filter_keep_the_owner_that_edited_them(cx: &mut TestAppContext) {
    let (core, runtime, directory) = fixture();
    let path = directory.0.join("kurisu.db");
    let db = kurisu_core::Db::open(&path).unwrap();
    db.set_setting("anilist_user_id", "71").unwrap();
    db.set_setting("native_preference:kurisu.list.filter", "PAUSED")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.q", "verified search")
        .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, window, cx| {
        w.profile = Some(User {
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        w.route = Route::MyList;
        w.stage_query(Some("first offline search".into()));
        w.save_query(window, cx);
        w.stage_query(Some("latest offline search".into()));
        cx.notify();
    });
    for selector in ["tab-PLANNING", "tab-COMPLETED"] {
        cx.update(|window, app| window.draw(app).clear(app));
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
    }
    workspace.update_in(cx, |w, window, cx| {
        assert_eq!(w.list.filter, "COMPLETED");
        assert_eq!(w.preference_active, Some("kurisu.list.filter"));
        assert!(w.preference_writes["kurisu.list.filter"].pending.is_some());
        assert!(w.query_write_active && w.query_pending.is_some());
        w.set_profile(
            Some(User {
                id: 71,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        w.request_close(CloseIntent::Quit, window, cx);
        assert!(!w.close_ready);
    });
    settle(cx, &runtime);
    assert!(workspace.read_with(cx, |w, _| w.close_ready));
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    drop(workspace);
    cx.run_until_parked();
    drop(db);
    drop(core);
    drop(runtime);
    let reopened = kurisu_core::Db::open(&path).unwrap();
    assert_eq!(
        reopened
            .get_setting("native_preference:kurisu.list.filter")
            .unwrap()
            .as_deref(),
        Some("PAUSED")
    );
    assert_eq!(
        reopened
            .get_setting("native_preference:kurisu.list.q")
            .unwrap()
            .as_deref(),
        Some("verified search")
    );
}

#[gpui::test]
fn queued_list_preferences_cannot_follow_account_replacement(cx: &mut TestAppContext) {
    let (first_core, runtime, directory) = fixture();
    drop(first_core);
    let path = directory.0.join("kurisu.db");
    let db = kurisu_core::Db::open(&path).unwrap();
    let previous = User {
        id: 71,
        offline: true,
        ..Default::default()
    };
    let replacement = User {
        id: 72,
        ..Default::default()
    };
    db.replace_account("first-session", &previous).unwrap();
    let core = Core::open(
        directory.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, _, cx| {
        w.profile = Some(previous);
        w.offline = true;
        w.route = Route::MyList;
        cx.notify();
    });
    cx.run_until_parked();
    let completed = cx.debug_bounds("tab-COMPLETED").unwrap();
    cx.simulate_click(completed.center(), gpui::Modifiers::default());
    workspace.update_in(cx, |w, window, cx| {
        assert_eq!(w.preference_active, Some("kurisu.list.filter"));
        w.stage_query(Some("old search".into()));
        w.persist_preference("kurisu.list.synced", Some("123".into()), window, cx);
        w.select_sort(SortKey::Title, window, cx);
        w.request_close(CloseIntent::Quit, window, cx);
        assert!(!w.close_ready);
    });
    // A verified refresh can commit before the queued write or its UI callback runs.
    db.replace_account("second-session", &replacement).unwrap();
    settle(cx, &runtime);
    assert!(workspace.read_with(cx, |w, _| w.close_ready));
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    drop(workspace);
    cx.run_until_parked();
    drop(db);
    drop(runtime);
    let reopened = kurisu_core::Db::open(&path).unwrap();
    assert_eq!(reopened.cached_user().unwrap().unwrap().id, 72);
    for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
        assert!(
            reopened
                .get_setting(&format!("native_preference:{key}"))
                .unwrap()
                .is_none(),
            "{key}"
        );
    }
    assert_eq!(
        reopened
            .get_setting("native_preference:kurisu.list.sort")
            .unwrap()
            .as_deref(),
        Some("title")
    );
}

#[gpui::test]
fn failed_confirmed_logout_cancels_quit(cx: &mut TestAppContext) {
    let (first_core, runtime, directory) = fixture();
    drop(first_core);
    let db = kurisu_core::Db::open(&directory.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 71,
        ..Default::default()
    };
    db.replace_account("saved-session", &user).unwrap();
    db.0.lock()
        .execute_batch(
            "CREATE TRIGGER reject_logout BEFORE DELETE ON settings
         WHEN OLD.key='anilist_user_id' BEGIN SELECT RAISE(ABORT, 'identity cleanup failed'); END;",
        )
        .unwrap();
    let core = Core::open(
        directory.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |w, window, cx| {
        w.start_barrier(BarrierAction::Logout, window, cx);
        w.request_close(CloseIntent::Quit, window, cx);
        assert!(!w.close_ready);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(cx, |w, _| w.auth_busy) {
        assert!(Instant::now() < deadline);
        settle(cx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    workspace.read_with(cx, |w, _| {
        assert!(w.global_error.contains("identity cleanup failed"));
        assert!(w.barrier.is_none());
        assert!(!w.close_ready);
    });
    cx.update(|window, _| window.remove_window());
    cx.run_until_parked();
    drop(workspace);
    cx.run_until_parked();
    drop(runtime);
}

#[cfg(feature = "fixture-runtime")]
fn serve_viewer_for_auth_child(child_flag: &str, test_name: &str) {
    super::super::fixture_server::serve(
        test_name,
        child_flag,
        "KURISU_FIXTURE_GRAPHQL",
        |mut http, _| {
            let request = http.json();
            let auth = &http.authorization;
            let query = request["query"].as_str().unwrap();
            let viewer = query.contains("Viewer") && !query.contains("unreadNotificationCount");
            let id = if auth.contains("second-session") {
                72
            } else {
                71
            };
            let (status, body) = if viewer && auth.contains("offline-first") {
                (
                    503,
                    serde_json::json!({"errors":[{"message":"Unavailable","status":503}]}),
                )
            } else if viewer {
                (
                    200,
                    serde_json::json!({"data":{"Viewer":{"id":id,"name":format!("User {id}"),"mediaListOptions":{"scoreFormat":"POINT_100"}}}}),
                )
            } else {
                (
                    200,
                    serde_json::json!({"data":{"MediaListCollection":{"lists":[]},"Page":{"pageInfo":{"currentPage":1,"hasNextPage":false},"media":[],"notifications":[]},"Viewer":{"unreadNotificationCount":0}}}),
                )
            };
            let result = http.reply_json(status, body);
            if viewer {
                result.unwrap();
            }
        },
    );
}

#[gpui::test]
fn signed_out_restart_clears_list_preferences_for_replacement_and_blocks_logout_edits(
    cx: &mut TestAppContext,
) {
    #[cfg(feature = "fixture-runtime")]
    {
        const CHILD: &str = "KURISU_AUTH_SIGNED_OUT_RESTART_CHILD";
        if super::super::fixture_server::child_gate(CHILD).is_none() {
            serve_viewer_for_auth_child(
                CHILD,
                "screens::workspace_tests::auth_tests::signed_out_restart_clears_list_preferences_for_replacement_and_blocks_logout_edits",
            );
            return;
        }
    }
    let (seed_core, runtime, directory) = fixture();
    drop(seed_core);
    let path = directory.0.join("kurisu.db");
    let db = kurisu_core::Db::open(&path).unwrap();
    let first = User {
        id: 71,
        name: "User 71".into(),
        ..Default::default()
    };
    #[cfg(not(feature = "fixture-runtime"))]
    let second = User {
        id: 72,
        name: "User 72".into(),
        ..Default::default()
    };
    db.replace_account("offline-first", &first).unwrap();
    let core = Core::open(
        directory.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace, vcx) =
        cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    settle(vcx, &runtime);
    workspace.update_in(vcx, |w, window, cx| {
        w.profile = Some(first.clone());
        w.last_account = Some(71);
        w.stage_query(Some("private search".into()));
        w.save_query(window, cx);
        w.select_filter("DROPPED", window, cx);
        w.persist_preference("kurisu.list.synced", Some("1700000000".into()), window, cx);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(vcx, |w, _| w.active_writes > 0 || w.preference_has_work()) {
        assert!(Instant::now() < deadline);
        settle(vcx, &runtime);
    }
    workspace.update_in(vcx, |w, window, cx| {
        w.start_barrier(BarrierAction::Logout, window, cx);
        assert!(w.auth_busy);
        w.stage_query(Some("late logout search".into()));
        w.select_filter("PAUSED", window, cx);
        w.persist_preference("kurisu.list.synced", Some("999".into()), window, cx);
        assert!(w.query_pending.is_none());
        assert!(!w.preference_has_work());
        assert_eq!(w.list.filter, "DROPPED");
    });
    while workspace.read_with(vcx, |w, _| w.auth_busy || w.profile.is_some()) {
        assert!(Instant::now() < deadline);
        settle(vcx, &runtime);
    }
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    drop((workspace, db, core, runtime));

    let runtime2 = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let core2 = Core::open(
        directory.0.clone(),
        runtime2.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace2, vcx2) =
        cx.add_window_view(|window, cx| Workspace::new(core2.clone(), window, cx));
    while workspace2.read_with(vcx2, |w, _| !w.preferences_loaded || w.restoring_session) {
        assert!(Instant::now() < deadline);
        settle(vcx2, &runtime2);
    }
    workspace2.update_in(vcx2, |w, window, cx| {
        assert_eq!(w.last_account, Some(71));
        assert!(w.profile.is_none());
        w.set_profile(
            Some(User {
                id: 71,
                offline: true,
                ..Default::default()
            }),
            window,
            cx,
        );
        assert_eq!(w.list.filter, "DROPPED");
        assert_eq!(w.list.query, "private search");
        assert_eq!(w.list.synced_at, Some(1700000000));
    });
    vcx2.update(|window, _| window.remove_window());
    vcx2.run_until_parked();
    drop((workspace2, core2, runtime2));

    let runtime3 = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let core3 = Core::open(
        directory.0.clone(),
        runtime3.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let (workspace3, vcx3) =
        cx.add_window_view(|window, cx| Workspace::new(core3.clone(), window, cx));
    settle(vcx3, &runtime3);
    assert_eq!(workspace3.read_with(vcx3, |w, _| w.last_account), Some(71));
    let db3 = kurisu_core::Db::open(&path).unwrap();
    #[cfg(feature = "fixture-runtime")]
    {
        workspace3.update_in(vcx3, |w, window, cx| {
            w.login(false, Some("second-session".into()), window, cx);
            assert!(w.auth_busy);
        });
        while workspace3.read_with(vcx3, |w, _| {
            w.auth_busy
                || w.profile.as_ref().map(|u| u.id) != Some(72)
                || w.active_writes > 0
                || w.preference_has_work()
        }) {
            assert!(Instant::now() < deadline);
            settle(vcx3, &runtime3);
            runtime3.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
    }
    #[cfg(not(feature = "fixture-runtime"))]
    {
        db3.replace_account("second-session", &second).unwrap();
        workspace3.update_in(vcx3, |w, window, cx| {
            w.set_profile(Some(second), window, cx);
        });
        while workspace3.read_with(vcx3, |w, _| w.active_writes > 0 || w.preference_has_work()) {
            assert!(Instant::now() < deadline);
            settle(vcx3, &runtime3);
        }
    }
    workspace3.read_with(vcx3, |w, app| {
        assert_eq!(w.last_account, Some(72));
        assert_eq!(w.list.filter, "CURRENT");
        assert!(w.list.query.is_empty());
        assert_eq!(w.list_query.read(app).value(), "");
        assert_eq!(w.list.synced_at, None);
    });
    #[cfg(feature = "fixture-runtime")]
    for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
        assert_eq!(
            db3.get_setting(&format!("native_preference:{key}"))
                .unwrap(),
            None,
            "{key}"
        );
    }
    vcx3.update(|window, _| window.remove_window());
    vcx3.run_until_parked();
    drop((workspace3, db3, core3, runtime3));
}

#[cfg(feature = "fixture-runtime")]
#[gpui::test]
fn mixed_list_owner_and_saved_account_resets_concurrent_preference_read(cx: &mut TestAppContext) {
    const CHILD: &str = "KURISU_AUTH_MIXED_OWNER_CHILD";
    if super::super::fixture_server::child_gate(CHILD).is_none() {
        serve_viewer_for_auth_child(
            CHILD,
            "screens::workspace_tests::auth_tests::mixed_list_owner_and_saved_account_resets_concurrent_preference_read",
        );
        return;
    }
    let (seed_core, runtime, directory) = fixture();
    drop(seed_core);
    let path = directory.0.join("kurisu.db");
    let db = kurisu_core::Db::open(&path).unwrap();
    db.replace_account(
        "second-session",
        &User {
            id: 72,
            name: "User 72".into(),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_setting("native_preference:kurisu.list.owner", "71")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.q", "alice private")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.filter", "PAUSED")
        .unwrap();
    db.set_setting("native_preference:kurisu.list.synced", "1700000000")
        .unwrap();

    let core = Core::open(
        directory.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    assert_eq!(runtime.block_on(core.saved_account_id()), Some(71));

    let (workspace, vcx) =
        cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
    let deadline = Instant::now() + Duration::from_secs(5);
    while workspace.read_with(vcx, |w, _| {
        w.restoring_session
            || !w.preferences_loaded
            || w.profile.as_ref().map(|u| u.id) != Some(72)
            || w.active_writes > 0
            || w.preference_has_work()
    }) {
        assert!(Instant::now() < deadline);
        settle(vcx, &runtime);
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
    workspace.read_with(vcx, |w, app| {
        assert_eq!(w.last_account, Some(72));
        assert!(w.preference_reset >= 1);
        assert!(w.list.query.is_empty());
        assert_eq!(w.list_query.read(app).value(), "");
        assert_eq!(w.list.filter, "CURRENT");
        assert_eq!(w.list.synced_at, None);
    });
    for key in ["kurisu.list.q", "kurisu.list.filter", "kurisu.list.synced"] {
        assert_eq!(
            db.get_setting(&format!("native_preference:{key}")).unwrap(),
            None,
            "{key}"
        );
    }
    vcx.update(|window, _| window.remove_window());
    vcx.run_until_parked();
    drop((workspace, db, core, runtime));
}
