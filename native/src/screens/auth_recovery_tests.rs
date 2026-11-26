use super::*;

fn wait_for_core(runtime: &tokio::runtime::Runtime, done: impl Fn() -> bool) {
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    });
}

fn logout_case(tc: &mut TestAppContext, read: &str, failure: &str, close: bool) {
    if fixture_server("route").is_none() {
        return;
    }
    let (initial, runtime, dir) = fixture();
    drop(initial);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            score_format: Some("POINT_5".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
    )
    .unwrap();
    let user = runtime.block_on(core.current_user()).unwrap().unwrap();
    match failure {
        "cleanup" => db.0.lock().execute_batch(
            "CREATE TRIGGER reject_logout BEFORE DELETE ON settings
             WHEN OLD.key='anilist_user_id' BEGIN SELECT RAISE(ABORT, 'identity cleanup failed'); END;"
        ).unwrap(),
        "token" => db.0.lock().execute_batch(
            "CREATE TRIGGER reject_token_delete BEFORE DELETE ON settings
             WHEN OLD.key='anilist_token' BEGIN SELECT RAISE(ABORT, 'token delete failed'); END;
             CREATE TRIGGER reject_token_update BEFORE UPDATE ON settings
             WHEN OLD.key='anilist_token' BEGIN SELECT RAISE(ABORT, 'token update failed'); END;
             CREATE TRIGGER reject_token_insert BEFORE INSERT ON settings
             WHEN NEW.key='anilist_token' BEGIN SELECT RAISE(ABORT, 'token insert failed'); END;"
        ).unwrap(),
        _ => {}
    }
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let gate = runtime.block_on(core.hold_remote_lock_for_test());
    let (generation, pending_job) = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.route = Route::Calendar;
        w.calendar.loaded_once = true;
        let pending_job = (read == "pending").then(|| {
            w.refresh_pending(win, cx);
            w.read_aborts["pending"][0].clone()
        });
        if read == "lag" {
            w.restore_session(win, cx);
        }
        w.start_barrier(BarrierAction::Logout, win, cx);
        assert!(w.auth_busy && w.barrier.is_none());
        (w.generation, pending_job)
    });
    cx.run_until_parked();
    wait_for_core(&runtime, || core.generation() != generation);
    assert_eq!(
        runtime.block_on(core.current_user()).unwrap().unwrap().id,
        7
    );
    if let Some(job) = pending_job {
        wait_for_core(&runtime, || job.is_finished());
    }
    let auth_request = view.read_with(cx, |w, _| w.auth_request);
    if read == "lag" {
        assert!(view.read_with(cx, |w, _| !w.pending_refresh_deferred));
        for episode in 0..300 {
            core.emit(EventKind::WatchHistoryUpdated(episode));
        }
        wait(cx, &runtime, |cx| {
            view.read_with(cx, |w, _| w.pending_refresh_deferred)
        });
    }
    view.update_in(cx, |w, win, cx| match read {
        "late pending" => {
            let request = w.pending_request;
            w.refresh_pending(win, cx);
            assert!(w.pending_refresh_deferred);
            assert_eq!(w.pending_request, request);
        }
        "lag" => assert_eq!(w.auth_request, auth_request),
        _ => {}
    });
    settle(cx, &runtime);
    view.update_in(cx, |w, win, cx| {
        assert!(w.auth_busy);
        assert_eq!(w.generation, generation);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
        assert!(w.calendar.loaded_once && !w.route_entry_deferred);
        if close {
            w.request_close(CloseIntent::Quit, win, cx);
            assert!(w.barrier.is_some());
        }
    });
    drop(gate);
    let account = (failure == "token").then_some(7);
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            !w.auth_busy
                && w.generation == core.generation()
                && w.profile.as_ref().map(|user| user.id) == account
                && (account.is_none() || w.calendar.loaded_once)
        })
    });
    assert_eq!(
        runtime
            .block_on(core.current_user())
            .unwrap()
            .map(|user| user.id),
        account
    );
    view.read_with(cx, |w, _| {
        match failure {
            "cleanup" => assert!(w.auth_error.contains("identity cleanup failed")),
            "token" => assert!(w.auth_error.contains("token delete failed")),
            _ => assert!(w.auth_error.is_empty()),
        }
        assert_eq!(w.route, Route::Calendar);
        assert!(!w.route_entry_deferred && !w.pending_refresh_deferred);
        if close {
            assert!(w.barrier.is_none() && !w.close_ready);
            assert!(w.global_error.contains("Could not finish signing out"));
        }
        if account.is_some() {
            assert_eq!(w.calendar.items[0].media.id, 42);
        } else {
            assert!(!w.calendar.loading && !w.calendar.loaded_once);
        }
    });
}

#[gpui::test]
fn pending_refresh_waits_for_logout_cleanup_failure(cx: &mut TestAppContext) {
    logout_case(cx, "pending", "cleanup", false);
}

#[gpui::test]
fn pending_refresh_waits_for_logout_token_failure(cx: &mut TestAppContext) {
    logout_case(cx, "pending", "token", false);
}

#[gpui::test]
fn late_pending_refresh_waits_for_successful_logout(cx: &mut TestAppContext) {
    logout_case(cx, "late pending", "", false);
}

#[gpui::test]
fn lag_recovery_waits_for_logout(cx: &mut TestAppContext) {
    logout_case(cx, "lag", "cleanup", false);
}

#[gpui::test]
fn failed_logout_cancels_queued_close_and_restores_session(cx: &mut TestAppContext) {
    logout_case(cx, "pending", "cleanup", true);
}

fn oauth_case(tc: &mut TestAppContext, cancel: bool) {
    if fixture_server("route").is_none() {
        return;
    }
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (initial, runtime, dir) = fixture_with_platform(browser.clone());
    drop(initial);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            score_format: Some("POINT_5".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let core = Core::open(dir.0.clone(), runtime.handle().clone(), browser.clone()).unwrap();
    let user = runtime.block_on(core.current_user()).unwrap().unwrap();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let (generation, pending_job) = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.route = Route::Calendar;
        w.calendar.loaded_once = true;
        w.refresh_pending(win, cx);
        let pending_job = w.read_aborts["pending"][0].clone();
        w.perform_login(true, None, win, cx);
        (w.generation, pending_job)
    });
    wait_for_core(&runtime, || {
        core.generation() != generation && pending_job.is_finished()
    });
    settle_browser_login(cx, &runtime, &browser, 1);
    view.read_with(cx, |w, _| {
        assert!(w.auth_busy && !w.route_entry_deferred && w.calendar.loaded_once);
        assert_eq!(w.generation, generation);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
    });
    if cancel {
        view.update_in(cx, |w, win, cx| w.cancel_login(win, cx));
    } else {
        deny_browser_login(&browser.0.lock().unwrap()[0]);
    }
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            !w.auth_busy
                && w.generation == core.generation()
                && w.calendar.loaded_once
                && !w.calendar.items.is_empty()
        })
    });
    view.read_with(cx, |w, _| {
        assert!(!w.route_entry_deferred);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
        assert_eq!(w.calendar.items[0].media.id, 42);
    });
}

#[gpui::test]
fn pending_refresh_preserves_calendar_until_oauth_denial(cx: &mut TestAppContext) {
    oauth_case(cx, false);
}

#[gpui::test]
fn pending_refresh_preserves_calendar_until_oauth_cancel(cx: &mut TestAppContext) {
    oauth_case(cx, true);
}

#[gpui::test]
fn replacement_account_rejects_a_completed_pending_read(tc: &mut TestAppContext) {
    if fixture_server("route").is_none() {
        return;
    }
    let (core, runtime, _) = signed_in_fixture(None);
    let user = runtime.block_on(core.current_user()).unwrap().unwrap();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let (generation, pending_job) = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.pending_error = "Awaiting current refresh".into();
        w.refresh_pending(win, cx);
        (w.generation, w.read_aborts["pending"][0].clone())
    });
    wait_for_core(&runtime, || pending_job.is_finished());
    let replacement = runtime
        .block_on(core.login_with_token("replacement-fixture-token".into()))
        .unwrap();
    assert_eq!(replacement.id, 9);
    assert!(
        runtime
            .block_on(core.get_pending_changes_for(generation, 7))
            .unwrap()
            .is_none()
    );
    assert!(
        runtime
            .block_on(core.get_pending_changes_for(core.generation(), 7))
            .unwrap()
            .is_none()
    );
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert_eq!(w.pending_error, "Awaiting current refresh");
        assert_eq!(w.generation, generation);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
    });
    view.update_in(cx, |w, win, cx| w.set_profile(Some(replacement), win, cx));
    settle(cx, &runtime);
    view.read_with(cx, |w, _| {
        assert_eq!(w.profile.as_ref().unwrap().id, 9);
        assert_eq!(w.generation, core.generation());
        assert!(w.pending.is_empty());
    });
}

fn settlement_case(tc: &mut TestAppContext, close_window: bool) {
    if fixture_server("route").is_none() {
        return;
    }
    let _port = browser_sign_in_guard();
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (initial, runtime, dir) = fixture_with_platform(browser.clone());
    drop(initial);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "synthetic-token",
        &User {
            id: 7,
            name: "Saved".into(),
            score_format: Some("POINT_5".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.upsert_media(&Media {
        id: 9,
        title_english: Some("Fixture show".into()),
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
    let payload = serde_json::json!({
        "media_id": 9,
        "base": null,
        "patch": {
            "status": null,
            "progress": 4,
            "score": null,
            "repeat": null
        },
        "error": "Temporary outage",
        "conflict": false
    })
    .to_string();
    db.0.lock()
        .execute(
            "INSERT INTO pending_change (account_id, media_id, payload) VALUES (7, 9, ?1)",
            [&payload],
        )
        .unwrap();
    let core = Core::open(dir.0.clone(), runtime.handle().clone(), browser.clone()).unwrap();
    assert_eq!(
        runtime.block_on(core.get_pending_changes()).unwrap().len(),
        1
    );
    let user = runtime.block_on(core.current_user()).unwrap().unwrap();
    let mut events = core.subscribe();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let generation = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.last_account = Some(7);
        w.last_sync_attempt = Some(Instant::now());
        w.list.synced_at = Some(now_seconds());
        w.route = Route::MyList;
        let generation = w.generation;
        w.perform_login(true, None, win, cx);
        generation
    });
    wait_for_core(&runtime, || core.generation() != generation);
    settle_browser_login(cx, &runtime, &browser, 1);

    let gate = runtime.block_on(core.hold_remote_lock_for_test());
    assert!(core.remote_write_active());
    view.update_in(cx, |w, win, cx| {
        if close_window {
            w.request_close(CloseIntent::Window, win, cx);
            assert!(w.close_ready);
        } else {
            w.cancel_login(win, cx);
        }
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            !w.auth_busy
                && w.generation == core.generation()
                && w.profile.as_ref().map(|user| user.id) == Some(7)
                && (close_window || (w.list.loaded && w.pending.len() == 1))
        })
    });
    let settlement_job = view.read_with(cx, |w, _| {
        if close_window {
            assert!(w.close_ready && !w.list.loaded);
            assert!(w.list_refresh_deferred && w.pending_refresh_deferred);
        } else {
            assert_eq!(w.list.entry(9).map(|entry| entry.progress), Some(4));
            assert_eq!(w.pending.len(), 1);
            assert_eq!(w.pending[0].media_id, 9);
        }
        w.read_aborts
            .get("remote-settlement")
            .and_then(|handles| handles.first())
            .cloned()
            .expect("missing remote-settlement watcher")
    });

    db.upsert_entry(&ListEntry {
        media_id: 9,
        status: "CURRENT".into(),
        progress: 5,
        ..Default::default()
    })
    .unwrap();
    db.0.lock()
        .execute(
            "DELETE FROM pending_change WHERE account_id = 7 AND media_id = 9",
            [],
        )
        .unwrap();
    assert!(events.try_recv().is_err());
    drop(gate);
    wait_for_core(&runtime, || settlement_job.is_finished());

    if close_window {
        settle(cx, &runtime);
        view.read_with(cx, |w, _| {
            assert!(w.close_ready && !w.list.loaded);
            assert!(w.list_refresh_deferred && w.pending_refresh_deferred);
            assert!(!w.read_aborts.contains_key("list") && !w.read_aborts.contains_key("pending"));
        });
        view.update_in(cx, |w, win, cx| w.resume_after_close(win, cx));
    } else {
        wait(cx, &runtime, |cx| {
            view.read_with(cx, |w, _| w.list_refresh_queued && w.pending.is_empty())
        });
        cx.executor().advance_clock(Duration::from_millis(350));
    }
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            w.list.loaded
                && w.list.entry(9).map(|entry| entry.progress) == Some(5)
                && w.pending.is_empty()
        })
    });
    assert!(events.try_recv().is_err());
    view.read_with(cx, |w, _| {
        assert!(!w.list_refresh_deferred && !w.pending_refresh_deferred);
        assert_eq!(w.list.entry(9).map(|entry| entry.progress), Some(5));
        assert!(w.pending.is_empty());
    });
}

#[gpui::test]
fn cancel_login_reconciles_list_and_pending_after_superseded_remote_write(cx: &mut TestAppContext) {
    settlement_case(cx, false);
}

#[gpui::test]
fn remote_settlement_defers_refresh_while_close_ready_and_replays_on_resume(
    cx: &mut TestAppContext,
) {
    settlement_case(cx, true);
}
