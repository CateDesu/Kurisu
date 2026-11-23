use super::*;
use kurisu_core::models::TorrentItem;

fn release() -> TorrentItem {
    TorrentItem {
        title: "Controlled release".into(),
        guid: "controlled-release-guid".into(),
        link: "https://example.invalid/release".into(),
        ..Default::default()
    }
}

fn opener_close(tc: &mut TestAppContext, outcome: &str) {
    let f = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&f.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            f.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = f.load(&shell, cx);
    workspace.update(cx, |w, cx| w.close_test_list(cx));
    let item = (outcome != "itemless").then(release);
    let opener = workspace.update_in(cx, |w, win, cx| w.test_open_torrent(item, win, cx));
    assert_eq!(
        workspace.read_with(cx, |w, _| w.close_test_state().1),
        usize::from(outcome != "itemless")
    );
    let (quit, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| {
        s.close_test_gate = Some(gate);
        s.tray = None;
    });
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |s, _| s.stopping),
        outcome == "itemless"
    );
    if outcome == "success" {
        let refused = workspace.update_in(cx, |w, win, cx| {
            w.test_open_torrent(Some(release()), win, cx)
        });
        assert!(refused.is_closed());
        assert_eq!(workspace.read_with(cx, |w, _| w.close_test_state().1), 1);
    }
    if outcome == "account" {
        workspace.update(cx, |w, _| w.test_torrent_owner(0, 100));
    }
    if outcome == "generation" {
        workspace.update(cx, |w, _| w.test_torrent_owner(1, 99));
    }
    if outcome == "cancel_close" {
        workspace.update_in(cx, |w, win, cx| w.test_cancel_torrent_close(win, cx));
        cx.run_until_parked();
        assert!(!shell.read_with(cx, |s, _| s.closing));
    }
    if outcome == "seen_error" {
        f.db.0.lock().execute_batch("CREATE TRIGGER reject_seen BEFORE INSERT ON rss_seen BEGIN SELECT RAISE(ABORT, 'controlled seen failure'); END;").unwrap();
    }
    if outcome == "canceled" {
        drop(opener);
    } else {
        opener
            .send(if outcome == "error" {
                Err("controlled opener failure".into())
            } else {
                Ok(())
            })
            .unwrap();
    }
    // Poll only the opener result, leaving the new Core persistence task queued.
    f.runtime.block_on(async { tokio::task::yield_now().await });
    cx.run_until_parked();
    let has_seen_write = matches!(outcome, "success" | "cancel_close" | "seen_error");
    assert_eq!(
        workspace.read_with(cx, |w, _| w.close_test_state().1),
        usize::from(has_seen_write)
    );
    assert!(
        !f.db
            .rss_seen_set()
            .unwrap()
            .contains("controlled-release-guid")
    );
    if has_seen_write {
        assert!(
            !shell.read_with(cx, |s, _| s.stopping),
            "readiness overtook queued seen persistence"
        );
    }
    f.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| w.close_test_state().1 == 0)
    });
    let persists = matches!(outcome, "success" | "cancel_close");
    assert_eq!(
        f.db.rss_seen_set()
            .unwrap()
            .contains("controlled-release-guid"),
        persists
    );
    workspace.read_with(cx, |w, _| {
        let error = w.test_torrent_error();
        assert_eq!(
            !error.is_empty(),
            matches!(outcome, "error" | "canceled" | "seen_error")
        );
        if outcome == "seen_error" {
            assert!(error.contains("controlled seen failure"));
        }
    });
    if outcome == "cancel_close" {
        assert!(!shell.read_with(cx, |s, _| s.stopping));
        assert!(!workspace.read_with(cx, |w, _| w.close_test_state().0));
        let mut next = release();
        next.guid = "after-canceled-close".into();
        next.link = "https://example.invalid/next".into();
        let opener = workspace.update_in(cx, |w, win, cx| w.test_open_torrent(Some(next), win, cx));
        assert!(!opener.is_closed());
        opener.send(Ok(())).unwrap();
        f.wait(cx, |cx| {
            workspace.read_with(cx, |w, _| w.close_test_state().1 == 0)
        });
        assert!(
            f.db.rss_seen_set()
                .unwrap()
                .contains("after-canceled-close")
        );
        assert!(!cx.simulate_close());
    }
    f.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    if outcome == "success" {
        let refused = workspace.update_in(cx, |w, win, cx| {
            w.test_open_torrent(Some(release()), win, cx)
        });
        assert!(refused.is_closed());
        assert_eq!(workspace.read_with(cx, |w, _| w.close_test_state().1), 0);
    }
    quit.send(()).unwrap();
    f.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
    assert_eq!(
        f.db.rss_seen_set()
            .unwrap()
            .contains("controlled-release-guid"),
        persists
    );
}

#[gpui::test]
fn accepted_opener_keeps_close_owned_through_seen_persistence(cx: &mut TestAppContext) {
    opener_close(cx, "success");
}

macro_rules! opener_case {
    ($name:ident, $outcome:literal) => {
        #[gpui::test]
        fn $name(cx: &mut TestAppContext) {
            opener_close(cx, $outcome);
        }
    };
}
opener_case!(opener_error_releases_close, "error");
opener_case!(canceled_opener_releases_close, "canceled");
opener_case!(stale_account_releases_close, "account");
opener_case!(stale_generation_releases_close, "generation");
opener_case!(seen_error_preserves_existing_close_behavior, "seen_error");
opener_case!(itemless_link_does_not_hold_close, "itemless");

#[gpui::test]
fn canceled_close_keeps_the_accepted_opener_usable(cx: &mut TestAppContext) {
    opener_close(cx, "cancel_close");
}

#[gpui::test]
fn rss_enter_respects_final_close_and_cancel(tc: &mut TestAppContext) {
    tc.update(bind_keys);
    let f = Fixture::new(tc);
    f.db.set_setting("rss_feeds", "[]").unwrap();
    let endpoint = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/feed", endpoint.local_addr().unwrap());
    let instance = Arc::new(InstanceGuard::acquire(&f.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            f.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = f.load(&shell, cx);
    workspace.update(cx, |w, cx| w.close_test_list(cx));
    shell.update_in(cx, |s, win, cx| s.navigate(Route::Torrents, win, cx));
    let input = workspace.read_with(cx, |w, _| w.test_feed_input());
    let focus = input.update(cx, |field, cx| {
        field.set_value(&url, cx);
        field.focus_handle(cx)
    });
    let submit = |cx: &mut VisualTestContext| {
        cx.update(|win, app| win.focus(&focus, app));
        assert!(cx.update(|win, _| focus.is_focused(win)));
        cx.simulate_keystrokes("enter");
    };
    let writes =
        |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.close_test_state().1);
    let (quit, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| {
        s.close_test_gate = Some(gate);
        s.tray = None;
    });

    f.db.0.lock().execute_batch("CREATE TRIGGER reject_feed BEFORE INSERT ON settings WHEN NEW.key='rss_feeds' BEGIN SELECT RAISE(ABORT, 'controlled feed failure'); END;").unwrap();
    submit(cx);
    assert_eq!(writes(cx), 1);
    assert!(!cx.simulate_close());
    assert!(!shell.read_with(cx, |s, _| s.stopping));
    workspace.update_in(cx, |w, win, cx| w.test_cancel_torrent_close(win, cx));
    f.wait(cx, |cx| writes(cx) == 0);
    assert!(!shell.read_with(cx, |s, _| s.closing));
    assert!(workspace.read_with(cx, |w, _| {
        w.test_torrent_error().contains("controlled feed failure")
    }));
    assert_eq!(
        f.db.get_setting("rss_feeds").unwrap().as_deref(),
        Some("[]")
    );

    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_feed")
        .unwrap();
    f.db.0
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_pref BEFORE INSERT ON settings
         WHEN NEW.key='native_preference:kurisu.list.dir'
         BEGIN SELECT RAISE(ABORT, 'controlled pref failure'); END;",
        )
        .unwrap();
    workspace.update_in(cx, |w, win, cx| w.close_test_preference(win, cx));
    assert!(!cx.simulate_close());
    f.wait(cx, |cx| {
        shell.read_with(cx, |s, _| s.modal_layer == Some(ModalLayer::BarrierError))
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let barrier_tree = cx.take_accessibility_tree_update().unwrap();
    let (barrier_field_id, _) = barrier_tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some(&url))
        .expect("RSS field in the barrier accessibility tree");
    let barrier_field_id = *barrier_field_id;
    cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
        action: gpui::AccessibleAction::Focus,
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: barrier_field_id,
        data: None,
    });
    cx.simulate_keystrokes("enter");
    workspace.update_in(cx, |w, win, cx| w.test_remove_torrent_feed(win, cx));
    assert_eq!(writes(cx), 0, "barrier admitted a torrent feed write");
    assert_eq!(
        f.db.get_setting("rss_feeds").unwrap().as_deref(),
        Some("[]")
    );
    click(cx, "cancel-failed-barrier");
    f.wait(cx, |cx| shell.read_with(cx, |s, _| !s.closing));
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_pref")
        .unwrap();
    click(cx, "retry-preferences");
    f.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| !w.close_test_preference_work())
    });

    submit(cx);
    assert_eq!(writes(cx), 1);
    assert!(!cx.simulate_close());
    assert!(!shell.read_with(cx, |s, _| s.stopping));
    f.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    let saved = vec![url.clone()];
    assert_eq!(f.runtime.block_on(f.core.get_rss_feeds()).unwrap(), saved);

    let late = format!("{url}/late");
    input.update(cx, |field, cx| field.set_value(&late, cx));
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        tree.nodes.iter().any(|(_, node)| {
            node.role() == Role::Dialog && node.label() == Some("Closing Kurisu")
        }),
        "closing dialog missing from the accessibility tree"
    );
    let (field_id, field) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some(&late))
        .expect("RSS field in the closing accessibility tree");
    let field_id = *field_id;
    assert!(field.supports_action(gpui::AccessibleAction::Focus));
    assert!(!field.is_disabled());
    assert!(workspace.read_with(cx, |w, _| w.close_test_state().0));
    cx.simulate_keystrokes("enter");
    assert_eq!(writes(cx), 0);
    cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
        action: gpui::AccessibleAction::Focus,
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: field_id,
        data: None,
    });
    cx.simulate_keystrokes("enter");
    assert_eq!(writes(cx), 0, "late RSS submission admitted a write");
    quit.send(()).unwrap();
    f.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
    assert_eq!(f.runtime.block_on(f.core.get_rss_feeds()).unwrap(), saved);
}
