use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

#[path = "auth_recovery_tests.rs"]
mod auth_recovery_tests;

fn wait(
    cx: &mut TestAppContext,
    runtime: &tokio::runtime::Runtime,
    done: impl Fn(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "prompt restoration timed out");
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
    }
}

fn restored_prompt(cx: &mut TestAppContext, mode: &str) {
    let Some(gate) = fixture_server(mode) else {
        return;
    };
    let (first, runtime, dir) = fixture();
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
    let (workspace, cx) = cx.add_window_view(|win, cx| Workspace::new(core.clone(), win, cx));
    wait(cx, &runtime, |_| gate.with_extension("started").exists());
    assert!(workspace.read_with(cx, |w, _| w.restoring_session && w.profile.is_none()));
    core.emit(if mode == "prompt" {
        EventKind::TrackingPrompt(tracking_fixture(4))
    } else {
        EventKind::TrackingAsk(tracking_fixture(4))
    });
    let first = retained_prompt(&core);
    let expected = if mode == "takeover" {
        core.emit(EventKind::TrackingAsk(tracking_fixture(5)));
        let latest = retained_prompt(&core);
        assert_ne!(latest.prompt_id, first.prompt_id);
        assert!(!core.event_is_current(&first));
        latest
    } else {
        first
    };
    assert_eq!(expected.account_id, Some(7));
    assert!(expected.prompt_id.is_some());
    if mode == "acknowledged" {
        assert!(runtime.block_on(core.acknowledge_tracking_prompt(expected.clone())));
    }
    if mode == "idle" {
        let idle = core
            .recovery_events()
            .into_iter()
            .find(|e| matches!(&e.kind, EventKind::NowPlaying(now) if !now.active))
            .unwrap();
        core.emit(idle.kind);
    }
    core.emit(EventKind::UpdateFailed("subscription consumed".into()));
    wait(cx, &runtime, |cx| {
        workspace.read_with(cx, |w, _| w.update_failed == "subscription consumed")
    });
    workspace.update(cx, |w, cx| {
        w.update_failed.clear();
        cx.notify();
    });
    assert!(workspace.read_with(cx, |w, _| w.prompt.is_none() && w.restoring_session));
    std::fs::write(&gate, "release").unwrap();
    let recovers_prompt = matches!(mode, "ask" | "prompt" | "takeover");
    wait(cx, &runtime, |cx| {
        workspace.read_with(cx, |w, _| {
            !w.restoring_session
                && !w.recovery_prompts_loading
                && (!recovers_prompt
                    || w.prompt_event.as_ref().and_then(|event| event.prompt_id)
                        == expected.prompt_id)
        })
    });
    if recovers_prompt {
        assert!(core.event_is_current(&expected));
        assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
        workspace.update_in(cx, |w, win, cx| {
            assert_eq!(w.profile.as_ref().map(|u| u.id), Some(7));
            assert_eq!(
                w.route,
                if mode == "prompt" {
                    Route::MyList
                } else {
                    Route::Now
                }
            );
            assert!(same_prompt(
                w.prompt_event.as_ref().expect("retained prompt was lost"),
                &expected
            ));
            let revision = w.prompt_revision;
            w.handle_event(expected.clone(), win, cx);
            assert_eq!(w.prompt_revision, revision);
            assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &expected));
            w.dismiss_prompt(w.prompt_revision, cx);
            w.restore_session(win, cx);
        });
        wait(cx, &runtime, |cx| {
            workspace.read_with(cx, |w, _| {
                !w.restoring_session && !w.recovery_prompts_loading
            }) && !core.event_is_current(&expected)
        });
        assert!(workspace.read_with(cx, |w, _| w.prompt.is_none()));
    } else {
        assert!(!core.event_is_current(&expected));
        assert!(workspace.read_with(cx, |w, _| w.prompt_event.is_none()));
    }
    cx.update(|win, _| win.remove_window());
    settle(cx, &runtime);
}

// The parent serves loopback requests while the child runs the selected test.
pub(in crate::screens) fn fixture_server(mode: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("KURISU_PROMPT_RESTORE_GATE") {
        let gate = PathBuf::from(path);
        std::fs::write(gate.with_extension("entered"), []).unwrap();
        return Some(gate);
    }
    let thread = std::thread::current();
    let test_name = thread.name().expect("named test thread");
    let dir = FixtureDir(std::env::temp_dir().join(format!(
            "kurisu-prompt-gate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&dir.0).unwrap();
    let gate = dir.0.join("viewer");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env("KURISU_PROMPT_RESTORE_GATE", &gate)
        .env(
            "KURISU_FIXTURE_GRAPHQL",
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        'serve: while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "prompt child timed out");
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = None;
            let mut rejected_login = false;
            let mut replacement_login = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    continue 'serve;
                }
                if line == "\r\n" {
                    break;
                }
                rejected_login |=
                    line.eq_ignore_ascii_case("authorization: Bearer rejected-fixture-token\r\n");
                replacement_login |= line
                    .eq_ignore_ascii_case("authorization: Bearer replacement-fixture-token\r\n");
                if let Some((key, value)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut bytes = vec![0; length.unwrap()];
            reader.read_exact(&mut bytes).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let query = request["query"].as_str().unwrap();
            let viewer = query.contains("Viewer") && !query.contains("unreadNotificationCount");
            let offline = viewer && mode == "reconnect" && !gate.exists();
            let body = if offline {
                serde_json::json!({"errors":[{"message":"Unavailable","status":503}]})
            } else if viewer {
                if !matches!(mode, "route" | "progress" | "reconnect") {
                    std::fs::write(gate.with_extension("started"), "started").unwrap();
                    while !gate.exists() {
                        assert!(Instant::now() < deadline, "Viewer gate timed out");
                        assert!(child.try_wait().unwrap().is_none(), "child exited before consuming the subscription");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                if mode == "expired" || rejected_login {
                    serde_json::json!({"errors":[{"message":"Invalid token", "status":401}]})
                } else {
                    serde_json::json!({"data":{"Viewer":{"id":if replacement_login || matches!(mode, "other_account" | "quit-login" | "quit-restore" | "quit-restore-barrier") {9} else {7},"name":"Saved","mediaListOptions":{"scoreFormat":"POINT_5"}}}})
                }
            } else if mode == "route" {
                let media = serde_json::json!({
                    "id":42,"isAdult":false,"idMal":null,"title":{"romaji":"Fixture show","english":null,"native":null},
                    "coverImage":null,"episodes":null,"format":null,"status":null,"averageScore":null,
                    "season":null,"seasonYear":null,"description":null,"nextAiringEpisode":null
                });
                let at = request["variables"]["startExclusive"].as_i64().unwrap_or(0) + 1;
                serde_json::json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},
                    "airingSchedules":[{"airingAt":at,"episode":3,"media":media}],"media":[media],
                    "notifications":[]},"Viewer":{"unreadNotificationCount":0},"MediaListCollection":{"lists":[]}}})
            } else if query.contains("SaveMediaListEntry") {
                serde_json::json!({"data":{"SaveMediaListEntry":{
                    "id": 90,
                    "mediaId": 9,
                    "status": request["variables"]["status"].as_str().unwrap_or("CURRENT"),
                    "progress": request["variables"]["progress"],
                    "score": 0.0,
                    "repeat": 0,
                    "updatedAt": 1
                }}})
            } else {
                serde_json::json!({"data":{"Page":{"pageInfo":{"hasNextPage":false},"media":[],"airingSchedules":[],"notifications":[]},"MediaListCollection":{"lists":[]},"Viewer":{"unreadNotificationCount":0}}})
            }.to_string();
            let status = if offline {
                503
            } else if viewer && (mode == "expired" || rejected_login) {
                401
            } else {
                200
            };
            write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())
                .unwrap_or_else(|error| {
                    if (!viewer || mode.starts_with("quit-"))
                        && matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted)
                    {
                        return;
                    }
                    let phase = if viewer { "Viewer" } else { "background" };
                    panic!("writing {phase} response: {error}");
                });
            if viewer {
                std::fs::write(gate.with_extension("responded"), "responded").unwrap();
            }
        }
    }));
    if let Err(error) = result {
        let status_before_cleanup = child.try_wait();
        let kill = if matches!(status_before_cleanup, Ok(Some(_))) {
            None
        } else {
            Some(child.kill())
        };
        let output = child.wait_with_output().unwrap();
        eprintln!(
            "prompt child before cleanup: {status_before_cleanup:?}; kill: {kill:?}; collected status: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::panic::resume_unwind(error);
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "prompt child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        gate.with_extension("entered").is_file(),
        "child skipped {test_name}"
    );
    None
}

macro_rules! restoration_case {
    ($name:ident, $mode:literal) => {
        #[gpui::test]
        fn $name(cx: &mut TestAppContext) {
            restored_prompt(cx, $mode);
        }
    };
}
restoration_case!(restore_recovers_consumed_ask, "ask");
restoration_case!(restore_recovers_consumed_prompt, "prompt");
restoration_case!(restore_recovers_only_the_latest_offer, "takeover");
restoration_case!(restore_rejects_acknowledged_offer, "acknowledged");
restoration_case!(restore_rejects_inactive_playback, "idle");
restoration_case!(restore_rejects_expired_offer, "expired");
restoration_case!(restore_rejects_other_account, "other_account");

#[gpui::test]
fn reconnect_account_recovers_dropped_tracking_ask_and_rejects_dismissed_replay(
    cx: &mut TestAppContext,
) {
    let Some(gate) = fixture_server("reconnect") else {
        return;
    };
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.set_setting("anilist_token", "synthetic-token").unwrap();
    db.set_setting("anilist_username", "Saved").unwrap();
    db.set_setting("anilist_user_id", "7").unwrap();
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
    workspace.update(cx, |w, cx| {
        w.restoring_session = false;
        w.profile = Some(User {
            id: 7,
            name: "Saved".into(),
            offline: true,
            ..Default::default()
        });
        w.offline = true;
        cx.notify();
    });
    settle(cx, &runtime);
    workspace.read_with(cx, |w, _| {
        assert!(w.prompt.is_none());
        assert!(w.offline);
        assert_eq!(w.route, Route::MyList);
    });

    std::fs::write(&gate, "online").unwrap();
    workspace.update_in(cx, |w, win, cx| {
        w.reconnect_account(true, win, cx);
        assert!(w.reconnect_busy);
    });
    wait(cx, &runtime, |cx| {
        workspace.read_with(cx, |w, _| {
            !w.reconnect_busy
                && !w.offline
                && !w.recovery_prompts_loading
                && w.prompt_event.as_ref().and_then(|event| event.prompt_id) == expected.prompt_id
        })
    });
    workspace.update_in(cx, |w, win, cx| {
        assert_eq!(w.profile.as_ref().map(|u| u.id), Some(7));
        assert_eq!(w.route, Route::Now);
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &expected));
        w.dismiss_prompt(w.prompt_revision, cx);
        w.navigate(Route::MyList, win, cx);
        assert!(core.event_is_current(&expected));
        w.handle_event(expected.clone(), win, cx);
        assert_eq!(w.route, Route::MyList);
        assert!(w.prompt.is_none());
    });
    wait(cx, &runtime, |_| !core.event_is_current(&expected));
    cx.update(|win, _| win.remove_window());
    settle(cx, &runtime);
}

fn quit_pending_auth(cx: &mut TestAppContext, mode: &str) {
    let Some(gate) = fixture_server(mode) else {
        return;
    };
    let (first, runtime, dir) = fixture();
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    db.replace_account(
        "saved-token",
        &User {
            id: 7,
            name: "Saved".into(),
            ..Default::default()
        },
    )
    .unwrap();
    drop(first);
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    let login = mode == "quit-login";
    let (workspace, cx) = cx.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, !login)
    });
    if login {
        workspace.update_in(cx, |w, win, cx| {
            w.perform_login(false, Some("replacement-token".into()), win, cx);
        });
    }
    wait(cx, &runtime, |_| gate.with_extension("started").exists());
    let worker = workspace.update_in(cx, |w, win, cx| {
        let worker = if login {
            w.auth_abort.clone()
        } else {
            w.read_aborts
                .get("restore-session")
                .map(|handles| handles[0].clone())
        };
        if mode == "quit-restore" {
            w.cancel_reads_for_quit();
        } else {
            w.request_close(CloseIntent::Quit, win, cx);
            assert!(w.close_ready);
        }
        worker
    });
    std::fs::write(&gate, "release").unwrap();
    wait(cx, &runtime, |cx| {
        gate.with_extension("responded").exists()
            && workspace.read_with(cx, |w, _| {
                worker
                    .as_ref()
                    .map_or(!w.restoring_session, |worker| worker.is_finished())
            })
    });
    settle(cx, &runtime);
    assert_eq!(
        runtime.block_on(core.saved_account_id()),
        Some(7),
        "late authentication replaced the saved account after quit"
    );
    workspace.read_with(cx, |w, _| {
        assert!(
            w.profile.is_none(),
            "late authentication updated the profile after quit"
        );
    });
}

macro_rules! quit_case {
    ($name:ident, $mode:literal) => {
        #[gpui::test]
        fn $name(cx: &mut TestAppContext) {
            quit_pending_auth(cx, $mode);
        }
    };
}
quit_case!(bucket_b_quit_cancels_pending_login, "quit-login");
quit_case!(
    bucket_b_quit_teardown_cancels_pending_restore,
    "quit-restore"
);
quit_case!(
    bucket_b_quit_barrier_cancels_pending_restore,
    "quit-restore-barrier"
);

fn signed_in_fixture(entry: Option<&ListEntry>) -> (Core, tokio::runtime::Runtime, User) {
    let db = kurisu_core::Db::open(Path::new(":memory:")).unwrap();
    let user = User {
        id: 7,
        offline: true,
        ..Default::default()
    };
    db.replace_account("synthetic-token", &user).unwrap();
    if let Some(entry) = entry {
        db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
        db.upsert_entry(entry).unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let core = Core::from_db(
        db,
        std::env::temp_dir(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    );
    (core, runtime, user)
}

fn pending_editor_fixture() -> (
    Core,
    tokio::runtime::Runtime,
    FixtureDir,
    kurisu_core::Db,
    User,
    ListEntry,
) {
    let (initial, runtime, dir) = fixture();
    drop(initial);
    let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
    let user = User {
        id: 7,
        offline: true,
        ..Default::default()
    };
    let mut entry = completed_fixture();
    entry.id = Some(90);
    entry.status = "CURRENT".into();
    entry.progress = 2;
    db.replace_account("synthetic-token", &user).unwrap();
    db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
    db.upsert_entry(&entry).unwrap();
    let core = Core::open(
        dir.0.clone(),
        runtime.handle().clone(),
        Arc::new(TestPlatform),
        UpdateChannel::Native,
    )
    .unwrap();
    (core, runtime, dir, db, user, entry)
}

fn progress_settles_under_tracking_prompt(tc: &mut TestAppContext, overdue: bool) {
    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    cx.simulate_accessibility_activation();
    let offer = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.step_progress(9, 1, win, cx);
        assert_eq!(w.active_writes, 0);
        if overdue {
            w.progress.edit_mut(w.generation, 9).unwrap().due = Some(Instant::now());
        }
        let mut prompt = tracking_fixture(4);
        prompt.progress = 2;
        core.emit(EventKind::TrackingPrompt(prompt));
        let offer = retained_prompt(&core);
        w.handle_event(offer.clone(), win, cx);
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &offer));
        w.confirm_prompt(w.prompt_revision, 4, win, cx);
        assert!(!w.prompt_busy);
        assert_eq!(w.active_writes, usize::from(overdue));
        offer
    });
    cx.run_until_parked();
    if !overdue {
        cx.executor().advance_clock(Duration::from_secs(4));
        cx.run_until_parked();
    }
    assert!(view.read_with(cx, |w, _| w.progress.busy(w.generation, 9)));
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(tree.nodes.iter().any(|(_, node)| {
        node.label() == Some("Update your list?")
            && node
                .description()
                .is_some_and(|text| text.contains("Current progress: 3."))
    }));
    view.update_in(cx, |w, win, cx| {
        assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
        assert!(!w.progress.blocks_add(w.generation, 9));
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &offer));
        assert!(core.event_is_current(&offer));
        w.confirm_prompt(w.prompt_revision, 4, win, cx);
        assert!(w.prompt_busy);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.prompt.is_none())
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 4);
}

#[gpui::test]
fn tracking_prompt_allows_debounced_episode_edit(tc: &mut TestAppContext) {
    progress_settles_under_tracking_prompt(tc, false);
}

#[gpui::test]
fn tracking_prompt_replays_overdue_episode_edit(tc: &mut TestAppContext) {
    progress_settles_under_tracking_prompt(tc, true);
}

#[gpui::test]
fn tracking_prompt_uses_the_action_still_available_on_the_list(tc: &mut TestAppContext) {
    let (core, _runtime, _dir) = fixture();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let offer = retained_prompt(&core);
    view.update_in(cx, |w, _, cx| {
        assert!(w.show_prompt(offer.clone()));
        w.list.apply_one(
            ListEntry {
                media_id: 9,
                progress: 4,
                ..Default::default()
            },
            now_seconds(),
        );
        cx.notify();
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        tree.nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Skip"))
    );
    assert!(
        tree.nodes
            .iter()
            .all(|(_, node)| node.label() != Some("Set progress to Ep 4"))
    );
    view.update_in(cx, |w, win, cx| {
        w.confirm_prompt(w.prompt_revision, 4, win, cx);
        assert_eq!(w.active_writes, 0);
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &offer));
        w.list.replace(vec![], now_seconds());
        w.confirm_prompt(w.prompt_revision, 4, win, cx);
        assert_eq!(w.active_writes, 0, "the entry was removed");
        w.list.replace(
            vec![ListEntry {
                media_id: 9,
                progress: 2,
                media: Some(Media {
                    id: 9,
                    episodes: Some(3),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            now_seconds(),
        );
        w.confirm_prompt(w.prompt_revision, 4, win, cx);
        assert_eq!(w.active_writes, 0, "the displayed target changed");
        cx.notify();
    });
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        tree.nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Set progress to Ep 3"))
    );
    view.update_in(cx, |w, win, cx| {
        w.list.apply_one(
            ListEntry {
                media_id: 9,
                progress: 3,
                ..Default::default()
            },
            now_seconds(),
        );
        w.confirm_prompt(w.prompt_revision, 3, win, cx);
        assert_eq!(w.active_writes, 0, "the clamped target was reached");
    });
    assert!(core.event_is_current(&offer));
}

#[gpui::test]
fn unrelated_editor_keeps_discovery_write_blocked_until_completion(tc: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, _db, user, entry) = pending_editor_fixture();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    let gate = runtime.block_on(core.hold_remote_lock_for_test());
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.list.replace(vec![entry], 0);
        let media = Media {
            id: 10,
            ..Default::default()
        };
        w.change_discovery_status(media.clone(), "PLANNING", Route::Search, win, cx);
        assert!(w.discovery_status_blocked(10));
        w.open_editor(9, win, cx);
        assert!(w.editor.is_some());
        assert!(w.editor_adding.contains(&10));
        w.close_editor(cx);
        assert!(w.editor.is_none());
        assert!(w.discovery_status_blocked(10));
        w.change_discovery_status(media, "PLANNING", Route::Search, win, cx);
        assert_eq!(w.active_writes, 1);
    });
    drop(gate);
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    assert!(view.read_with(cx, |w, _| !w.discovery_status_blocked(10)));
}

#[gpui::test]
fn queued_accessibility_clicks_cannot_act_on_replacement_tracking_prompt(tc: &mut TestAppContext) {
    use gpui::accesskit::{Action, ActionRequest, TreeId, TreeUpdate};

    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    let (view, cx) = tc.add_window_view(|win, cx| {
        Workspace::new_with_session_restore(core.clone(), win, cx, false)
    });
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let first = retained_prompt(&core);
    let first_revision = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.handle_event(first, win, cx);
        w.prompt_revision
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let button = |tree: &TreeUpdate, label| {
        tree.nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap()
            .0
    };
    let click = |target_node| ActionRequest {
        action: Action::Click,
        target_tree: TreeId::ROOT,
        target_node,
        data: None,
    };
    let tree = cx.take_accessibility_tree_update().unwrap();
    let confirm = button(&tree, "Set progress to Ep 4");
    let skip = button(&tree, "Skip");
    cx.queue_accessibility_action(click(confirm));
    cx.queue_accessibility_action(click(skip));
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let replacement = retained_prompt(&core);
    view.update_in(cx, |w, win, cx| {
        w.handle_event(replacement.clone(), win, cx);
        w.confirm_prompt(first_revision, 4, win, cx);
        w.dismiss_prompt(first_revision, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |w, _| {
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &replacement));
        assert!(!w.prompt_busy);
        assert_eq!(w.active_writes, 0);
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 2);
    assert!(core.event_is_current(&replacement));
    let tree = cx.take_accessibility_tree_update().unwrap();
    let replacement_confirm = button(&tree, "Set progress to Ep 4");
    assert_ne!(replacement_confirm, confirm);
    assert_ne!(button(&tree, "Skip"), skip);
    let gate = runtime.block_on(core.hold_remote_lock_for_test());
    cx.simulate_accessibility_action(click(replacement_confirm));
    settle(cx, &runtime);
    assert!(view.read_with(cx, |w, _| w.prompt_busy && w.active_writes == 1));

    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let latest = retained_prompt(&core);
    view.update_in(cx, |w, win, cx| w.handle_event(latest.clone(), win, cx));
    drop(gate);
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    view.read_with(cx, |w, _| {
        assert!(same_prompt(w.prompt_event.as_ref().unwrap(), &latest));
        assert!(!w.prompt_busy && w.prompt_error.is_empty());
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 2);
    let tree = cx.take_accessibility_tree_update().unwrap();
    let gate = runtime.block_on(core.hold_remote_lock_for_test());
    cx.simulate_accessibility_action(click(button(&tree, "Set progress to Ep 4")));
    settle(cx, &runtime);
    assert!(view.read_with(cx, |w, _| w.prompt_busy));
    db.upsert_media(&Media {
        id: 9,
        episodes: Some(3),
        ..Default::default()
    })
    .unwrap();
    drop(gate);
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| !w.prompt_busy && !w.prompt_error.is_empty())
    });
    assert!(core.event_is_current(&latest));
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 2);
    cx.executor().advance_clock(Duration::from_secs(1));
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            w.prompt_target(w.prompt.as_ref().unwrap()) == Some(3)
        })
    });
    let tree = cx.take_accessibility_tree_update().unwrap();
    cx.simulate_accessibility_action(click(button(&tree, "Set progress to Ep 3")));
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.prompt.is_none())
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
}

#[gpui::test]
fn tracking_prompt_replacement_refocuses_and_requires_a_fresh_press(tc: &mut TestAppContext) {
    use crate::{platform::InstanceGuard, shell};
    use gpui::{KeyDownEvent, KeyUpEvent, MouseButton, VisualTestContext};
    use std::sync::Mutex;

    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, dir, db, _user, _entry) = pending_editor_fixture();
    let instance = Arc::new(InstanceGuard::acquire(&dir.0).unwrap());
    tc.update(shell::bind_keys);
    let (_view, cx) = tc.add_window_view(|win, cx| {
        shell::Shell::new(core.clone(), Arc::new(Mutex::new(None)), instance, win, cx)
    });
    cx.simulate_accessibility_activation();
    let scale = cx.update(|win, _| win.scale_factor());
    let focused_skip = |cx: &mut VisualTestContext, previous| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            settle(cx, &runtime);
            cx.update(|win, app| {
                win.simulate_next_frame(app);
                win.draw(app).clear(app);
            });
            if let Some(tree) = cx.take_accessibility_tree_update()
                && let Some((id, node)) = tree.nodes.iter().find(|(id, node)| {
                    Some(*id) != previous && *id == tree.focus && node.label() == Some("Skip")
                })
            {
                let bounds = node.bounds().unwrap();
                let center = gpui::point(
                    gpui::px(((bounds.x0 + bounds.x1) / 2.) as f32 / scale),
                    gpui::px(((bounds.y0 + bounds.y1) / 2.) as f32 / scale),
                );
                return (*id, center);
            }
            assert!(
                Instant::now() < deadline,
                "tracking prompt did not focus Skip"
            );
        }
    };
    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let (first, position) = focused_skip(cx, None);
    let keystroke = gpui::Keystroke::parse("enter").unwrap();
    let press = |is_held| KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held,
        prefer_character_input: false,
    };
    let modifiers = gpui::Modifiers::default();
    cx.simulate_mouse_down(position, MouseButton::Left, modifiers);
    cx.simulate_keystrokes("tab");
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        tree.nodes.iter().any(|(id, node)| {
            *id == tree.focus && node.label() == Some("Set progress to Ep 4")
        })
    );
    cx.simulate_event(press(false));

    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let second = retained_prompt(&core);
    let (second_node, second_position) = focused_skip(cx, Some(first));
    assert_eq!(second_position, position);
    cx.simulate_mouse_up(position, MouseButton::Left, modifiers);
    cx.simulate_event(press(true));
    cx.simulate_event(KeyUpEvent {
        keystroke: keystroke.clone(),
    });
    settle(cx, &runtime);
    assert!(core.event_is_current(&second));
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 2);
    cx.simulate_event(press(false));
    cx.simulate_event(press(true));
    cx.simulate_event(KeyUpEvent { keystroke });
    wait(cx, &runtime, |_| !core.event_is_current(&second));

    core.emit(EventKind::TrackingPrompt(tracking_fixture(4)));
    let third = retained_prompt(&core);
    let (_, position) = focused_skip(cx, Some(second_node));
    cx.simulate_event(KeyDownEvent {
        keystroke: gpui::Keystroke::parse("escape").unwrap(),
        is_held: true,
        prefer_character_input: false,
    });
    settle(cx, &runtime);
    assert!(core.event_is_current(&third));
    cx.simulate_click(position, modifiers);
    wait(cx, &runtime, |_| !core.event_is_current(&third));
}

#[gpui::test]
fn pending_editor_retries_failed_writes_and_covering_reads(tc: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let rename = |from, to| {
        db.0.lock()
            .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
            .unwrap();
    };
    rename("list_entry", "held_entries");
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry.clone()], 0);
        w.step_progress(9, 1, win, cx);
        w.open_editor(9, win, cx);
        assert_eq!(w.active_writes, 1);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.pending_editor_error(9).is_some())
    });
    rename("held_entries", "list_entry");
    view.update_in(cx, |w, win, cx| {
        let edit = w.progress.edit(w.generation, 9).unwrap();
        assert_eq!((edit.saved, edit.pending), (2, 3));
        w.open_editor(9, win, cx);
        w.open_editor(9, win, cx);
        assert_eq!(w.active_writes, 1);
        assert!(w.editor.is_none());
        w.list.apply_scoped(&[10], vec![None], 0);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            w.active_writes == 0
                && w.pending_editor.as_ref().is_some_and(|pending| {
                    matches!(pending.wait, super::super::PendingEditorWait::Refresh)
                })
        })
    });
    assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
    settle(cx, &runtime);
    for scope in [Some(vec![10]), Some(vec![9]), None] {
        rename("list_entry", "held_entries");
        view.update_in(cx, |w, win, cx| {
            let account = w.profile.as_ref().unwrap().id;
            if let Some(ids) = scope.clone() {
                w.refresh_scoped(ids, account, win, cx);
            } else {
                w.load_list(win, cx);
            }
        });
        wait(cx, &runtime, |cx| {
            view.read_with(cx, |w, _| w.list_read.is_none())
        });
        view.read_with(cx, |w, _| {
            assert!(w.editor.is_none());
            assert_eq!(w.pending_editor_error(9).is_some(), scope != Some(vec![10]));
        });
        rename("held_entries", "list_entry");
    }
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    let (id, _) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Retry edit") && !node.is_disabled())
        .expect("pending editor retry action");
    cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
        action: gpui::AccessibleAction::Click,
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: *id,
        data: None,
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(350));
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.editor.is_some())
    });
    view.read_with(cx, |w, _| {
        assert_eq!(w.editor.as_ref().unwrap().progress, 3);
        assert!(w.pending_editor.is_none());
        assert_eq!(w.active_writes, 0);
    });
}

#[gpui::test]
fn pending_editor_keeps_full_and_scoped_coverage_when_sync_fails(tc: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    use super::super::{ListReadScope, PendingEditor, PendingEditorWait};

    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    db.upsert_entry(&ListEntry {
        progress: 3,
        ..entry.clone()
    })
    .unwrap();
    db.0.lock()
        .execute_batch("UPDATE settings SET value = x'00' WHERE key = 'anilist_profile';")
        .unwrap();
    let sync_error = db.cached_user().unwrap_err().to_string();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    for full in [false, true] {
        view.update_in(cx, |w, win, cx| {
            w.profile = Some(user.clone());
            w.offline = false;
            w.list.replace(vec![entry.clone()], 0);
            w.pending_editor = Some(PendingEditor {
                id: 9,
                route: w.route.clone(),
                wait: PendingEditorWait::Refresh,
                auto_open: true,
            });
            if full {
                w.load_list(win, cx);
                w.refresh_scoped(vec![10], user.id, win, cx);
                assert!(matches!(w.list_read, Some(ListReadScope::Full)));
            } else {
                w.refresh_scoped(vec![9], user.id, win, cx);
            }
            w.sync_list(win, cx);
            assert!(w.list.syncing);
            assert!(w.editor.is_none());
        });
        wait(cx, &runtime, |cx| {
            view.read_with(cx, |w, _| !w.list.syncing && w.list_read.is_none())
        });
        view.update_in(cx, |w, _, cx| {
            assert_eq!(
                w.editor.as_ref().map(|editor| editor.progress),
                Some(3),
                "full={full}, sync error={}, read error={}, pending={}, modal={:?}",
                w.list.sync_error,
                w.list.error,
                w.pending_editor.is_some(),
                w.active_modal()
            );
            assert!(w.pending_editor.is_none() && w.list_read.is_none());
            assert_eq!(w.list.sync_error, sync_error);
            assert_eq!(w.active_writes, 0);
            w.close_editor(cx);
        });
    }
}

fn restore_route_after_tray_close(cx: &mut TestAppContext, route: Route) {
    if fixture_server("route").is_none() {
        return;
    }
    let (core, runtime, user) = signed_in_fixture(None);
    let (workspace, cx) =
        cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let calendar = route == Route::Calendar;
    workspace.update_in(cx, |w, win, cx| {
        w.route = route;
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        w.set_profile(Some(user), win, cx);
        assert!(!w.calendar.loading && !w.seasons.loading);
        w.resume_after_close_visibility(true, win, cx);
    });
    wait(cx, &runtime, |cx| {
        workspace.read_with(cx, |w, _| {
            if calendar {
                w.calendar.loaded_once && w.calendar.preference_loaded
            } else {
                w.seasons.loaded_once || !w.seasons.error.is_empty()
            }
        })
    });
    workspace.read_with(cx, |w, _| {
        if calendar {
            assert!(w.calendar.error.is_empty(), "{}", w.calendar.error);
            let items = &w.calendar.items;
            assert_eq!(items.len(), 1);
            assert_eq!((items[0].media.id, items[0].episode), (42, 3));
        } else {
            assert!(w.seasons.error.is_empty(), "{}", w.seasons.error);
            assert_eq!(w.seasons.media.len(), 1);
            assert_eq!(w.seasons.media[0].id, 42);
        }
    });
}

#[gpui::test]
fn bucket_b_calendar_loads_after_tray_close(cx: &mut TestAppContext) {
    restore_route_after_tray_close(cx, Route::Calendar);
}

#[gpui::test]
fn bucket_b_seasons_load_after_tray_close(cx: &mut TestAppContext) {
    restore_route_after_tray_close(cx, Route::Seasons);
}

#[gpui::test]
fn superseding_login_restore_replays_calendar_after_the_last_fence(tc: &mut TestAppContext) {
    if fixture_server("route").is_none() {
        return;
    }
    let (core, runtime, user) = signed_in_fixture(None);
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let generation = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.route = Route::Calendar;
        w.auth_busy = true;
        w.auth_abort = Some(w.core.spawn(std::future::pending::<()>()).abort_handle());
        w.query_error = "Search save failed".into();
        w.request_close(super::super::CloseIntent::Window, win, cx);
        w.enter_active_route(win, cx);
        w.cancel_login(win, cx);
        w.restore_session(win, cx);
        assert!(w.route_entry_deferred && !w.calendar.loading);
        w.generation
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.generation != generation)
    });
    view.update_in(cx, |w, win, cx| {
        assert_eq!(w.route, Route::Calendar);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
        assert!(w.route_entry_deferred && !w.calendar.loading);
        w.cancel_barrier(win, cx);
        assert!(!w.route_entry_deferred && w.calendar.loading);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            w.calendar.loaded_once && w.calendar.preference_loaded
        })
    });
    view.read_with(cx, |w, _| assert_eq!(w.calendar.items[0].media.id, 42));
}

#[gpui::test]
fn failed_login_resumes_the_deferred_calendar_for_the_saved_account(tc: &mut TestAppContext) {
    if fixture_server("route").is_none() {
        return;
    }
    let (core, runtime, user) = signed_in_fixture(None);
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    let generation = view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.route = Route::Calendar;
        w.perform_login(false, Some("rejected-fixture-token".into()), win, cx);
        w.enter_active_route(win, cx);
        assert!(w.auth_busy && w.route_entry_deferred && !w.calendar.loading);
        w.generation
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.calendar.loaded_once)
    });
    view.read_with(cx, |w, _| {
        assert!(!w.auth_busy && !w.route_entry_deferred);
        assert!(w.auth_error.contains("Invalid token"), "{}", w.auth_error);
        assert_ne!(w.generation, generation);
        assert_eq!(w.profile.as_ref().unwrap().id, 7);
        assert_eq!(w.calendar.items[0].media.id, 42);
    });
}

#[gpui::test]
fn bucket_b_completed_decrement_regroups_after_save(cx: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    let mut entry = completed_fixture();
    entry.id = Some(90);
    let (core, runtime, user) = signed_in_fixture(Some(&entry));
    let (workspace, cx) =
        cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    workspace.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.list.set_filter("COMPLETED", 0);
        w.reset_list_widget();
        w.step_progress(9, -1, win, cx);
        let edit = w.progress.edit(w.generation, 9).unwrap();
        assert!(edit.in_flight, "Completed decrement was delayed");
        assert_eq!(edit.pending, 11);
    });
    wait(cx, &runtime, |cx| {
        workspace.read_with(cx, |w, _| w.active_writes == 0)
    });
    workspace.update(cx, |w, _| {
        assert!(w.list.step_error.is_empty(), "{}", w.list.step_error);
        let entry = w.list.entry(9).unwrap();
        assert_eq!(entry.progress, 11);
        assert_eq!(entry.status, "CURRENT");
        assert!(w.visible_ids.is_empty());
        w.list.set_filter("CURRENT", 0);
        w.reset_list_widget();
        assert_eq!(w.visible_ids.as_slice(), &[9]);
    });
}

#[gpui::test]
fn progress_save_during_logout_allows_edit_after_cancel(tc: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    let mut entry = completed_fixture();
    entry.id = Some(90);
    entry.progress = 2;
    entry.status = "CURRENT".into();
    let (core, runtime, user) = signed_in_fixture(Some(&entry));
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.step_progress(9, 1, win, cx);
        w.open_editor(9, win, cx);
        assert!(w.pending_editor.is_some());
        w.request_logout(win, cx);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    view.update_in(cx, |w, win, cx| {
        assert!(w.logout_confirming && w.editor.is_none());
        assert_eq!(w.list.entry(9).unwrap().progress, 3);
        w.escape_active_modal(win, cx);
        w.open_editor(9, win, cx);
        assert_eq!(w.editor.as_ref().unwrap().progress, 3);
    });
}

#[derive(Clone, Copy)]
enum IdleProgressRecovery {
    Edit,
    Cancel,
    CancelWithModal,
}

fn resume_idle_progress(tc: &mut TestAppContext, recovery: IdleProgressRecovery) {
    use super::super::{CloseIntent, PendingEditor, PendingEditorWait};

    if fixture_server("progress").is_none() {
        return;
    }
    let explicit = matches!(recovery, IdleProgressRecovery::Edit);
    let modal = matches!(recovery, IdleProgressRecovery::CancelWithModal);
    let (core, runtime, _dir, _db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry.clone()], 0);
        w.step_progress(9, 1, win, cx);
        w.pending_editor = Some(PendingEditor {
            id: 9,
            route: w.route.clone(),
            wait: PendingEditorWait::Progress,
            auto_open: true,
        });
        w.query_error = "Earlier search save failed".into();
        w.request_close(CloseIntent::Window, win, cx);
        assert_eq!(w.active_writes, 0);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(4));
    cx.run_until_parked();
    view.update_in(cx, |w, win, cx| {
        let edit = w.progress.edit_mut(w.generation, 9).unwrap();
        assert!(edit.dirty() && !edit.busy() && edit.error.is_none());
        // The GPUI timer and the stored Instant use different clocks in tests.
        edit.due = (!explicit).then(Instant::now);
        w.logout_confirming = modal;
        w.cancel_barrier(win, cx);
        assert_eq!(w.query_error, "Earlier search save failed");
        assert!(!w.pending_editor.as_ref().unwrap().auto_open);
        if explicit {
            assert_eq!(w.active_writes, 0);
            w.open_editor(9, win, cx);
        } else if modal {
            assert_eq!(w.active_writes, 0);
            w.escape_active_modal(win, cx);
        }
        assert_eq!(w.active_writes, 1);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    view.update_in(cx, |w, win, cx| {
        assert_eq!(w.editor.is_some(), explicit);
        assert_eq!(w.list.entry(9).unwrap().progress, 3);
        if !explicit {
            w.open_editor(9, win, cx);
        }
        assert_eq!(w.editor.as_ref().unwrap().progress, 3);
    });
}

#[gpui::test]
fn explicit_edit_saves_idle_pending_progress(tc: &mut TestAppContext) {
    resume_idle_progress(tc, IdleProgressRecovery::Edit);
}

#[gpui::test]
fn cancel_barrier_saves_overdue_progress_without_opening_editor(tc: &mut TestAppContext) {
    resume_idle_progress(tc, IdleProgressRecovery::Cancel);
}

#[gpui::test]
fn cancel_barrier_leaves_overdue_progress_waiting_for_the_modal(tc: &mut TestAppContext) {
    resume_idle_progress(tc, IdleProgressRecovery::CancelWithModal);
}

#[gpui::test]
fn progress_edit_retries_failed_close_replay_after_explicit_request(tc: &mut TestAppContext) {
    use super::super::{CloseIntent, PendingEditor, PendingEditorWait};

    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.pending_editor = Some(PendingEditor {
            id: 9,
            route: w.route.clone(),
            wait: PendingEditorWait::Refresh,
            auto_open: true,
        });
        w.refresh_scoped(vec![9], 7, win, cx);
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.list_completed_while_closing)
    });
    db.0.lock()
        .execute_batch("ALTER TABLE list_entry RENAME TO held_entries")
        .unwrap();
    view.update_in(cx, |w, win, cx| {
        w.queue_list_refresh(Some(vec![9]), win, cx);
        w.resume_after_close_visibility(false, win, cx);
        assert!(!w.pending_editor.as_ref().unwrap().auto_open);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            w.pending_editor
                .as_ref()
                .is_some_and(|pending| matches!(pending.wait, PendingEditorWait::RefreshFailed(_)))
        })
    });
    view.read_with(cx, |w, _| {
        assert!(w.pending_editor_error(9).is_none() && w.editor.is_none());
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some("Retry edit"))
    );
    db.0.lock()
        .execute_batch("ALTER TABLE held_entries RENAME TO list_entry")
        .unwrap();
    view.update_in(cx, |w, win, cx| w.open_editor(9, win, cx));
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(350));
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.editor.is_some())
    });
    view.read_with(cx, |w, _| {
        assert_eq!(w.editor.as_ref().unwrap().progress, 2);
        assert!(w.pending_editor.is_none());
    });
}

#[gpui::test]
fn failed_progress_save_reviews_a_newer_row_before_retry(tc: &mut TestAppContext) {
    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    db.0.lock()
        .execute_batch("ALTER TABLE list_entry RENAME TO held_entries")
        .unwrap();
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry.clone()], 0);
        w.step_progress(9, 1, win, cx);
        w.open_editor(9, win, cx);
        assert_eq!(w.active_writes, 1);
        let mut latest = entry;
        latest.progress = 4;
        w.list.apply_one(latest, 0);
        w.progress.reconcile_entries(w.generation, &w.list);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    view.update_in(cx, |w, win, cx| {
        w.retry_progress_errors(win, cx);
        let edit = w.progress.edit(w.generation, 9).unwrap();
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (2, 3, Some(4))
        );
        assert!(edit.requires_review && !w.list.step_error.is_empty());
        assert_eq!(w.active_writes, 0);
    });
}

#[gpui::test]
fn superseded_progress_save_cannot_open_editor_after_tray_close(tc: &mut TestAppContext) {
    use super::super::{CloseIntent, PendingEditorWait};

    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, _db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.stop_event_subscription();
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry], 0);
        w.step_progress(9, 1, win, cx);
        w.open_editor(9, win, cx);
        w.list.apply_one(
            ListEntry {
                media_id: 10,
                ..Default::default()
            },
            0,
        );
        w.request_close(CloseIntent::Window, win, cx);
    });
    wait(cx, &runtime, |cx| view.read_with(cx, |w, _| w.close_ready));
    view.update_in(cx, |w, win, cx| {
        let pending = w.pending_editor.as_ref().unwrap();
        assert!(matches!(pending.wait, PendingEditorWait::Refresh));
        assert!(!pending.auto_open && w.editor.is_none());
        w.queue_list_refresh(Some(vec![9]), win, cx);
        w.resume_after_close_visibility(false, win, cx);
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| {
            assert!(w.editor.is_none());
            w.pending_editor.is_none() && w.list.entry(9).is_some_and(|row| row.progress == 3)
        })
    });
    view.update_in(cx, |w, win, cx| {
        w.open_editor(9, win, cx);
        assert_eq!(w.editor.as_ref().unwrap().progress, 3);
    });
}

#[gpui::test]
fn progress_retry_clears_warning_and_direct_success_completes_pending_edit(
    tc: &mut TestAppContext,
) {
    use super::super::PendingEditorWait;

    if fixture_server("progress").is_none() {
        return;
    }
    let (core, runtime, _dir, _db, user, entry) = pending_editor_fixture();
    let (view, cx) =
        tc.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
    view.update_in(cx, |w, win, cx| {
        w.profile = Some(user);
        w.offline = true;
        w.list.replace(vec![entry.clone()], 0);
        let edit = w.progress.entry(w.generation, &entry);
        assert!(edit.step(1, Instant::now()));
        assert_eq!(edit.begin_save(), Some((3, 2)));
        edit.finish_save(Err("Could not save episode".into()));
        w.update_step_error();
        assert!(w.can_retry_progress_errors());
        w.open_editor(9, win, cx);
        assert!(w.list.step_error.is_empty() && !w.can_retry_progress_errors());
        assert_eq!(w.active_writes, 1);
        w.pending_editor.as_mut().unwrap().wait =
            PendingEditorWait::RefreshFailed("Earlier read failed".into());
    });
    wait(cx, &runtime, |cx| {
        view.read_with(cx, |w, _| w.active_writes == 0)
    });
    view.read_with(cx, |w, _| {
        assert!(w.pending_editor.is_none());
        assert_eq!(w.editor.as_ref().unwrap().progress, 3);
    });
}
