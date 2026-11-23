use super::*;
use gpui::{Modifiers, TestAppContext, VisualTestContext};
use kurisu_core::{Db, UpdateChannel};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[path = "torrent_close_tests.rs"]
mod torrent_close_tests;

static NEXT_PROFILE: AtomicU64 = AtomicU64::new(0);

struct LocalPlatform;

impl Platform for LocalPlatform {
    fn open_url(&self, _: &str) -> Result<(), String> {
        panic!("unexpected external URL");
    }
    fn open_path(&self, _: &Path) -> Result<(), String> {
        panic!("unexpected external path");
    }
    fn reveal_path(&self, _: &Path) -> Result<(), String> {
        panic!("unexpected reveal");
    }
}

struct Profile {
    path: PathBuf,
    app: TestAppContext,
}

impl Drop for Profile {
    fn drop(&mut self) {
        self.app.quit();
        self.app.run_until_parked();
        if std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }
}

struct Fixture {
    core: Core,
    db: Db,
    runtime: tokio::runtime::Runtime,
    profile: Profile,
}

impl Fixture {
    fn new(tc: &TestAppContext) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = NEXT_PROFILE.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kurisu-shell-close-{}-{seq}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        let profile = Profile {
            path,
            app: tc.clone(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::open(
            profile.path.clone(),
            runtime.handle().clone(),
            Arc::new(LocalPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        runtime
            .block_on(core.set_tracking_config("off".into(), 120, 80, true, "".into(), true))
            .unwrap();
        let db = Db::open(&profile.path.join("kurisu.db")).unwrap();
        db.set_setting("anilist_client_id", "controlled-custom-client")
            .unwrap();
        Self {
            core,
            db,
            runtime,
            profile,
        }
    }

    fn wait(&self, cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            self.runtime.block_on(async {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            });
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "bounded close wait expired");
        }
    }

    fn load(&self, shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<Workspace> {
        cx.simulate_resize(gpui::size(px(1100.), px(1800.)));
        shell.update_in(cx, |s, win, cx| s.navigate(Route::Settings, win, cx));
        let workspace = shell.read_with(cx, |s, _| s.workspace.clone());
        self.wait(cx, |cx| {
            workspace.read_with(cx, |w, _| w.close_test_loaded())
        });
        workspace
    }

    fn reject_tracking(&self) {
        self.db.0.lock().execute_batch("CREATE TRIGGER reject_tracking BEFORE INSERT ON settings WHEN NEW.key='tracking_auto_percent' BEGIN SELECT RAISE(ABORT, 'controlled tracking failure'); END;").unwrap();
    }
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|win, app| win.draw(app).clear(app));
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    draw(cx);
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing rendered {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
}

#[gpui::test]
fn rapid_modal_reopen_restores_the_last_selected_control(tc: &mut TestAppContext) {
    tc.update(bind_keys);
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    fixture.load(&shell, cx);
    draw(cx);
    let original = shell.update_in(cx, |shell, win, cx| {
        win.focus(&shell.focus, cx);
        win.focus_next(cx);
        let first = win.focused(cx);
        win.focus_next(cx);
        let original = win.focused(cx).unwrap();
        assert_ne!(Some(original.clone()), first);
        assert_ne!(original, shell.focus);
        original
    });
    let mut revision = 0;
    let mut open = |cx: &mut VisualTestContext| {
        revision += 1;
        let update = kurisu_core::UpdateInfo {
            available: true,
            can_install: false,
            restart_pending: false,
            version: revision.to_string(),
            tag: format!("v{revision}"),
            html_url: String::new(),
            body: String::new(),
            current: String::new(),
        };
        fixture
            .core
            .emit(kurisu_core::EventKind::UpdateAvailable(update));
        fixture.wait(cx, |cx| {
            shell.read_with(cx, |shell, _| shell.modal_layer == Some(ModalLayer::Update))
        });
    };
    let frame = |cx: &mut VisualTestContext| {
        cx.update(|win, app| {
            win.simulate_next_frame(app);
            win.draw(app).clear(app);
        });
    };

    for (change_focus, reopen) in [(false, true), (true, false), (true, true)] {
        cx.update(|win, app| win.focus(&original, app));
        open(cx);
        frame(cx);
        cx.simulate_keystrokes("tab");
        let first = cx.update(|win, app| win.focused(app).unwrap());
        cx.simulate_keystrokes("tab");
        let second = cx.update(|win, app| win.focused(app).unwrap());
        assert_ne!(first, second);
        frame(cx);
        assert!(cx.update(|win, _| second.is_focused(win)));
        cx.simulate_keystrokes("escape");
        shell.read_with(cx, |shell, _| assert_eq!(shell.modal_layer, None));
        frame(cx);
        let expected = if change_focus {
            cx.simulate_keystrokes("tab");
            let selected = cx.update(|win, app| win.focused(app).unwrap());
            assert_ne!(selected, original);
            assert_ne!(selected, shell.read_with(cx, |s, _| s.focus.clone()));
            selected
        } else {
            original.clone()
        };
        if reopen {
            open(cx);
            draw(cx);
            cx.simulate_keystrokes("escape");
            shell.read_with(cx, |shell, _| assert_eq!(shell.modal_layer, None));
            frame(cx);
            assert!(!cx.update(|win, _| expected.is_focused(win)));
        }
        frame(cx);
        assert!(cx.update(|win, _| expected.is_focused(win)));
    }
}

#[gpui::test]
fn rendered_save_is_fenced_until_final_quit(tc: &mut TestAppContext) {
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    workspace.update(cx, |w, cx| w.close_test_draft(cx));
    fixture.reject_tracking();
    let (release, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| s.close_test_gate = Some(gate));
    shell.update(cx, |s, _| s.tray = None);
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    click(cx, "tracking-save");
    workspace.read_with(cx, |w, _| {
        let (fenced, writes, error) = w.close_test_state();
        assert_eq!(writes, 0, "rendered Save admitted work after readiness");
        assert!(error.is_empty());
        assert!(fenced);
    });
    shell.update_in(cx, |s, win, cx| s.request_close(CloseIntent::Quit, win, cx));
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    shell.read_with(cx, |s, _| {
        assert!(!s.close_test_quit, "repeated close bypassed the drain")
    });
    release.send(()).unwrap();
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
    assert!(
        fixture
            .runtime
            .block_on(fixture.core.get_tracking_config())
            .discord_enabled
    );
}

#[gpui::test]
fn rendered_preference_is_fenced_until_final_quit(tc: &mut TestAppContext) {
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    workspace.update(cx, |w, cx| {
        w.close_test_list(cx);
    });
    fixture.db.0.lock().execute_batch("CREATE TRIGGER reject_preference BEFORE INSERT ON settings WHEN NEW.key='native_preference:kurisu.list.dir' BEGIN SELECT RAISE(ABORT, 'controlled preference failure'); END;").unwrap();
    let (release, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| s.close_test_gate = Some(gate));
    shell.update(cx, |s, _| s.tray = None);
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    click(cx, "sort-direction");
    // A queued preference callback must obey the same boundary as pointer input.
    workspace.update_in(cx, |w, win, cx| w.close_test_preference(win, cx));
    workspace.read_with(cx, |w, _| {
        assert!(
            !w.close_test_preference_work(),
            "preference admitted work after readiness"
        )
    });
    release.send(()).unwrap();
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
    assert_eq!(
        fixture
            .runtime
            .block_on(fixture.core.get_preference("kurisu.list.dir".into()))
            .unwrap(),
        None
    );
}

#[gpui::test]
fn pending_folder_picker_is_fenced_until_final_quit(tc: &mut TestAppContext) {
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    workspace.update(cx, |w, cx| {
        w.close_test_list(cx);
    });
    shell.update_in(cx, |s, win, cx| s.navigate(Route::Library, win, cx));
    let first = fixture.profile.path.join("first-folder");
    let late = fixture.profile.path.join("late-folder");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&late).unwrap();

    click(cx, "library-add-folder");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|_| Some(vec![first.clone()]));
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| w.close_test_state().1 == 0)
    });
    let expected = vec![first.to_string_lossy().into_owned()];
    assert_eq!(
        fixture
            .runtime
            .block_on(fixture.core.get_library_folders())
            .unwrap(),
        expected
    );

    click(cx, "library-add-folder");
    assert!(cx.did_prompt_for_paths());
    let (release, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| {
        s.close_test_gate = Some(gate);
        s.tray = None;
    });
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    cx.simulate_path_prompt_response(|_| Some(vec![late]));
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(cx, |w, _| w.close_test_state().1),
        0,
        "folder picker admitted work after close readiness"
    );
    release.send(()).unwrap();
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
    assert_eq!(
        fixture
            .runtime
            .block_on(fixture.core.get_library_folders())
            .unwrap(),
        expected
    );
}

#[gpui::test]
fn canceled_close_keeps_rendered_save_and_preferences_usable(tc: &mut TestAppContext) {
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    workspace.update(cx, |w, cx| w.close_test_draft(cx));
    fixture.reject_tracking();
    click(cx, "tracking-save");
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| {
            w.close_test_state()
                .2
                .contains("controlled tracking failure")
        })
    });
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| {
        shell.read_with(cx, |s, _| s.modal_layer == Some(ModalLayer::BarrierError))
    });
    click(cx, "cancel-failed-barrier");
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| !s.closing));
    fixture
        .db
        .0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    click(cx, "tracking-save");
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| w.close_test_state() == (false, 0, String::new()))
    });
    assert!(
        !fixture
            .runtime
            .block_on(fixture.core.get_tracking_config())
            .discord_enabled
    );
    workspace.update(cx, |w, cx| {
        w.close_test_list(cx);
    });
    click(cx, "sort-direction");
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| !w.close_test_preference_work())
    });
    assert!(
        fixture
            .runtime
            .block_on(fixture.core.get_preference("kurisu.list.dir".into()))
            .unwrap()
            .is_some()
    );
    shell.read_with(cx, |s, _| assert!(!s.close_test_quit));
}

#[cfg(feature = "fixture-runtime")]
#[gpui::test]
fn sync_install_and_freshness_obey_close_barrier_and_readiness(tc: &mut TestAppContext) {
    const CHILD: &str = "KURISU_CLOSE_SYNC_INSTALL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "shell::close_tests::sync_install_and_freshness_obey_close_barrier_and_readiness",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("KURISU_FIXTURE_GRAPHQL", "http://127.0.0.1:9/graphql")
            .env("KURISU_FIXTURE_RELEASES", "http://127.0.0.1:9/releases")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    workspace.update_in(cx, |w, win, cx| w.close_test_start_sync(win, cx));
    assert_eq!(workspace.read_with(cx, |w, _| w.close_test_state().1), 1);
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    assert!(
        !shell.read_with(cx, |s, _| s.stopping),
        "in-flight sync_list did not hold close barrier"
    );
    assert!(
        workspace.update_in(cx, |w, win, cx| w.close_test_try_overwrite_barrier(win, cx)),
        "start_barrier overwrote an active close barrier"
    );
    workspace.update_in(cx, |w, win, cx| w.test_cancel_torrent_close(win, cx));
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| w.close_test_state().1 == 0)
            && shell.read_with(cx, |s, _| !s.closing)
    });

    workspace.update_in(cx, |w, win, cx| w.close_test_start_install(win, cx));
    assert_eq!(workspace.read_with(cx, |w, _| w.close_test_state().1), 1);
    let (release, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| {
        s.close_test_gate = Some(gate);
        s.tray = None;
    });
    assert!(!cx.simulate_close());
    cx.run_until_parked();
    assert!(
        !shell.read_with(cx, |s, _| s.stopping),
        "in-flight perform_install did not hold close barrier"
    );
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));
    let freshness = workspace.update_in(cx, |w, win, cx| w.close_test_freshness(win, cx));
    assert_eq!(
        freshness,
        (false, false, false, 0),
        "freshness admitted work after close readiness"
    );
    release.send(()).unwrap();
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
}

#[gpui::test]
fn submitted_inputs_and_set_value_are_fenced_after_close_readiness(tc: &mut TestAppContext) {
    tc.update(bind_keys);
    let fixture = Fixture::new(tc);
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            fixture.core.clone(),
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    let workspace = fixture.load(&shell, cx);
    let [list_query, search, year, torrent] =
        workspace.update_in(cx, |w, win, cx| w.close_test_prepare_inputs(win, cx));
    for (route, field, which) in [
        (Route::Search, &search, 0usize),
        (Route::Seasons, &year, 1usize),
        (Route::Torrents, &torrent, 2usize),
    ] {
        workspace.update(cx, |w, cx| w.close_test_set_route_for_input(route, cx));
        draw(cx);
        let handle = field.read_with(cx, |input, cx| input.focus_handle(cx));
        cx.update(|win, app| win.focus(&handle, app));
        draw(cx);
        assert!(cx.update(|win, _| handle.is_focused(win)));
        cx.simulate_keystrokes("enter");
        let (_, searching, season_loading, torrent_searching) =
            workspace.read_with(cx, |w, _| w.close_test_reads_state());
        assert!(
            match which {
                0 => searching,
                1 => season_loading,
                _ => torrent_searching,
            },
            "unfenced Enter positive control did not fire for route {which}"
        );
        workspace.update(cx, |w, _| w.close_test_reset_submitted_reads());
    }
    workspace.update(cx, |w, cx| {
        w.close_test_set_route_for_input(Route::MyList, cx)
    });
    draw(cx);

    let (release, gate) = tokio::sync::oneshot::channel();
    shell.update(cx, |s, _| {
        s.close_test_gate = Some(gate);
        s.tray = None;
    });
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.stopping));

    for (route, field) in [
        (Route::Search, &search),
        (Route::Seasons, &year),
        (Route::Torrents, &torrent),
    ] {
        workspace.update(cx, |w, cx| w.close_test_set_route_for_input(route, cx));
        draw(cx);
        let handle = field.read_with(cx, |input, cx| input.focus_handle(cx));
        cx.update(|win, app| win.focus(&handle, app));
        draw(cx);
        assert!(cx.update(|win, _| handle.is_focused(win)));
        cx.simulate_keystrokes("enter");
    }
    workspace.update(cx, |w, cx| {
        w.close_test_set_route_for_input(Route::MyList, cx)
    });
    draw(cx);
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    let (query_node, _) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some("kept"))
        .expect("list query in accessibility tree");
    cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
        action: gpui::AccessibleAction::SetValue,
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: *query_node,
        data: Some(gpui::accesskit::ActionData::Value("mutated".into())),
    });
    assert_eq!(
        list_query.read_with(cx, |input, _| input.value().to_owned()),
        "kept"
    );
    assert_eq!(
        workspace.read_with(cx, |w, _| w.close_test_reads_state()),
        ("kept".into(), false, false, false)
    );
    release.send(()).unwrap();
    fixture.wait(cx, |cx| shell.read_with(cx, |s, _| s.close_test_quit));
}

#[cfg(feature = "fixture-runtime")]
#[gpui::test]
fn window_close_completes_admitted_reads_and_replays_deferred_refreshes_on_resume(
    tc: &mut TestAppContext,
) {
    const CHILD: &str = "KURISU_CLOSE_TRAY_READS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "shell::close_tests::window_close_completes_admitted_reads_and_replays_deferred_refreshes_on_resume",
                "--exact",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("KURISU_FIXTURE_GRAPHQL", "http://127.0.0.1:9/graphql")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let fixture = Fixture::new(tc);
    let user = kurisu_core::User {
        id: 7,
        name: "Fixture".into(),
        ..Default::default()
    };
    fixture.db.replace_account("tray-session", &user).unwrap();
    fixture.db.set_setting("close_to_tray", "1").unwrap();
    fixture
        .db
        .upsert_media(&kurisu_core::models::Media {
            id: 11,
            title_english: Some("Steins;Gate".into()),
            episodes: Some(24),
            ..Default::default()
        })
        .unwrap();
    fixture
        .db
        .upsert_entry(&kurisu_core::models::ListEntry {
            media_id: 11,
            status: "CURRENT".into(),
            progress: 4,
            ..Default::default()
        })
        .unwrap();
    fixture
        .db
        .0
        .lock()
        .execute_batch(
            r#"INSERT INTO pending_change (account_id, media_id, payload)
               VALUES (7, 11, '{"media_id":11,"base":null,"patch":{"status":"CURRENT","progress":4,"score":null,"repeat":null},"error":null,"conflict":false}');"#,
        )
        .unwrap();

    let signed_in_core = Core::open(
        fixture.profile.path.clone(),
        fixture.runtime.handle().clone(),
        Arc::new(LocalPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let instance = Arc::new(InstanceGuard::acquire(&fixture.profile.path).unwrap());
    let (shell, cx) = tc.add_window_view(|win, cx| {
        Shell::new(
            signed_in_core,
            Arc::new(Mutex::new(None)),
            instance,
            win,
            cx,
        )
    });
    shell.update(cx, |s, _| s.close_test_has_tray = true);
    let workspace = fixture.load(&shell, cx);

    workspace.update_in(cx, |w, win, cx| {
        w.close_test_begin_window_reads(false, win, cx);
    });
    assert!(!cx.simulate_close());
    workspace.read_with(cx, |w, _| {
        assert!(
            w.close_test_state().0,
            "window close should reach close_ready"
        );
    });
    fixture.wait(cx, |cx| {
        shell.read_with(cx, |s, _| !s.closing && s.hidden_to_tray)
            && workspace.read_with(cx, |w, _| {
                let (loading, loaded, visible, _, pending) = w.close_test_list_and_pending_state();
                !loading && loaded && visible == 1 && pending == 1
            })
    });

    shell.update_in(cx, |s, win, cx| {
        assert!(s.show_from_tray(win, cx));
    });
    fixture
        .db
        .upsert_media(&kurisu_core::models::Media {
            id: 12,
            title_english: Some("Robotics;Notes".into()),
            episodes: Some(22),
            ..Default::default()
        })
        .unwrap();
    fixture
        .db
        .upsert_entry(&kurisu_core::models::ListEntry {
            media_id: 12,
            status: "CURRENT".into(),
            progress: 2,
            ..Default::default()
        })
        .unwrap();
    fixture
        .db
        .0
        .lock()
        .execute_batch("DELETE FROM pending_change WHERE account_id = 7;")
        .unwrap();
    workspace.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_test_state().0);
        w.close_test_queue_deferred_refreshes(win, cx);
        w.resume_after_close_visibility(false, win, cx);
    });
    fixture.wait(cx, |cx| {
        workspace.read_with(cx, |w, _| {
            let (loading, loaded, visible, _, pending) = w.close_test_list_and_pending_state();
            !loading && loaded && visible == 2 && pending == 0
        })
    });

    shell.update_in(cx, |s, win, cx| {
        let _ = win.set_visible(true);
        s.hidden_to_tray = false;
        cx.notify();
    });
    fixture
        .db
        .0
        .lock()
        .execute_batch(
            "UPDATE list_entry SET status = X'FF' WHERE media_id = 11;
             UPDATE settings SET value = X'00' WHERE key = 'close_to_tray';",
        )
        .unwrap();
    workspace.update_in(cx, |w, win, cx| {
        w.close_test_begin_window_reads(true, win, cx);
    });
    assert!(!cx.simulate_close());
    fixture.wait(cx, |cx| {
        shell.read_with(cx, |s, cx| {
            !s.closing && s.workspace.read(cx).has_tray_error()
        }) && workspace.read_with(cx, |w, _| !w.close_test_list_and_pending_state().0)
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    let (_, retry_button) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::Button && node.label() == Some("Retry list"))
        .expect("Calendar retry button must render on list error");
    assert!(
        !retry_button.is_disabled(),
        "Calendar retry stayed disabled after failed window close resume"
    );
}
