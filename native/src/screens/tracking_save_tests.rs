use super::*;
use crate::contracts::{CloseIntent, Route, WorkspaceEvent};
use gpui::{TestAppContext, VisualTestContext};
use kurisu_core::{Core, Db};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[path = "tracking_read_tests.rs"]
mod tracking_read_tests;

static NEXT_PROFILE: AtomicU64 = AtomicU64::new(0);

#[gpui::test]
fn tracking_threshold_accessibility_keeps_names_units_and_values(tc: &mut TestAppContext) {
    let fixture = Fixture::new(tc);
    let (workspace, cx) =
        tc.add_window_view(|win, cx| Workspace::new(fixture.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1400.)));
    fixture.load(&workspace, cx);
    for (mode, label, value, draft) in [
        (
            "prompt",
            "Ask after this many seconds of playback",
            "120",
            "91",
        ),
        (
            "auto",
            "Update progress at this percentage watched",
            "80",
            "77",
        ),
    ] {
        workspace.update_in(cx, |w, _, cx| w.edit_tracking_mode(mode, cx));
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (id, input) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::TextInput && node.label() == Some(label))
            .unwrap_or_else(|| panic!("missing {label} input"));
        assert_eq!(input.value(), Some(value));
        assert_eq!(input.placeholder(), Some(value));
        assert!(!input.is_disabled());
        assert!(input.supports_action(gpui::AccessibleAction::SetValue));
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::SetValue,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *id,
            data: Some(gpui::accesskit::ActionData::Value(draft.into())),
        });
        assert_eq!(published_control(cx, label).value(), Some(draft));

        workspace.update_in(cx, |w, win, cx| {
            w.request_close(CloseIntent::Window, win, cx)
        });
        let input = published_control(cx, label);
        assert!(input.is_disabled(), "{label} remains editable during close");
        assert_eq!(input.value(), Some(draft));
        workspace.update_in(cx, |w, win, cx| w.resume_after_close(win, cx));
        let input = published_control(cx, label);
        assert!(!input.is_disabled());
        assert_eq!(input.value(), Some(draft));
    }
}

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
    db: Arc<Db>,
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
            "kurisu-tracking-save-{}-{seq}-{nonce}",
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
        )
        .unwrap();
        let db = Arc::new(Db::open(&profile.path.join("kurisu.db")).unwrap());
        runtime
            .block_on(core.set_tracking_config("off".into(), 120, 80, true, "".into(), true))
            .unwrap();
        // Refuse OAuth before opening a listener or contacting a service.
        db.set_setting("anilist_client_id", "controlled-custom-client")
            .unwrap();
        Self {
            core,
            db,
            runtime,
            profile,
        }
    }

    fn wait(
        &self,
        w: &Entity<Workspace>,
        cx: &mut VisualTestContext,
        label: &str,
        done: impl Fn(&Workspace) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cx.run_until_parked();
            self.runtime
                .block_on(async { tokio::task::yield_now().await });
            cx.run_until_parked();
            if w.read_with(cx, |w, _| done(w)) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            self.runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(1)).await;
            });
        }
    }

    fn load(&self, w: &Entity<Workspace>, cx: &mut VisualTestContext) {
        w.update_in(cx, |w, win, cx| {
            w.route = Route::Settings;
            w.enter_settings(win, cx);
        });
        self.wait(w, cx, "settings load", |w| {
            w.settings.cfg_loaded && !w.restoring_session && w.preferences_loaded
        });
    }

    fn reject_tracking(&self) {
        self.db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER reject_tracking BEFORE INSERT ON settings
             WHEN NEW.key='tracking_auto_percent'
             BEGIN SELECT RAISE(ABORT, 'controlled tracking failure'); END;",
            )
            .unwrap();
    }

    fn rejected(&self, w: &Entity<Workspace>, cx: &mut VisualTestContext) {
        self.wait(w, cx, "failed tracking save", |w| {
            w.active_writes == 0
                && !w.settings.cfg_saving
                && w.settings
                    .save_error
                    .contains("controlled tracking failure")
        });
        self.persisted(false);
    }

    fn persisted(&self, saved: bool) {
        let cfg = self.runtime.block_on(self.core.get_tracking_config());
        assert_eq!(cfg.mode, if saved { "auto" } else { "off" });
        assert_eq!(cfg.prompt_seconds, if saved { 91 } else { 120 });
        assert_eq!(cfg.auto_percent, if saved { 77 } else { 80 });
        assert_eq!(cfg.auto_ask, !saved);
        assert_eq!(cfg.discord_enabled, !saved);
        assert_eq!(
            cfg.mpv_ipc_socket,
            if saved {
                "/tmp/kurisu-fixture.sock"
            } else {
                ""
            }
        );
        let reopened = Db::open(&self.profile.path.join("kurisu.db")).unwrap();
        assert_eq!(
            reopened.get_setting("tracking_mode").unwrap().as_deref(),
            Some(cfg.mode.as_str())
        );
        assert_eq!(
            reopened
                .get_setting("tracking_prompt_seconds")
                .unwrap()
                .as_deref(),
            Some(cfg.prompt_seconds.to_string().as_str())
        );
        assert_eq!(
            reopened
                .get_setting("tracking_auto_percent")
                .unwrap()
                .as_deref(),
            Some(cfg.auto_percent.to_string().as_str())
        );
    }
}

struct Transition {
    auth_intent: u64,
    update_busy: bool,
    mode: String,
}

struct Observers {
    events: Arc<Mutex<Vec<(WorkspaceEvent, String)>>>,
    transitions: Arc<Mutex<Vec<Transition>>>,
    _events: Subscription,
    _transitions: Subscription,
}

impl Observers {
    fn new(f: &Fixture, w: &Entity<Workspace>, cx: &mut VisualTestContext) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let copy = events.clone();
        let db = f.db.clone();
        let subscription = cx.update(|_, app| {
            app.subscribe(w, move |_, event: &WorkspaceEvent, _| {
                copy.lock().unwrap().push((
                    event.clone(),
                    db.get_setting("tracking_mode").unwrap().unwrap(),
                ));
            })
        });
        let copy = transitions.clone();
        let db = f.db.clone();
        let intent = w.read_with(cx, |w, _| w.auth_intent);
        let observation = cx.update(|_, app| {
            app.observe(w, move |w, cx| {
                let w = w.read(cx);
                if w.auth_intent != intent || w.update_busy {
                    copy.lock().unwrap().push(Transition {
                        auth_intent: w.auth_intent,
                        update_busy: w.update_busy,
                        mode: db.get_setting("tracking_mode").unwrap().unwrap(),
                    });
                }
            })
        });
        Self {
            events,
            transitions,
            _events: subscription,
            _transitions: observation,
        }
    }

    fn ready(&self) -> Vec<(CloseIntent, String)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(e, mode)| {
                if let WorkspaceEvent::ReadyToClose(intent) = e {
                    Some((*intent, mode.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn held(&self) {
        assert!(
            self.ready().is_empty(),
            "failed submitted tracking save released close readiness"
        );
        assert!(
            self.transitions.lock().unwrap().is_empty(),
            "failed submitted tracking save dispatched a transition"
        );
    }
}

fn draft(w: &mut Workspace, cx: &mut Context<Workspace>) {
    w.edit_tracking_mode("auto", cx);
    w.settings.cfg.auto_ask = false;
    w.settings.cfg.discord_enabled = false;
    for (input, value) in [
        (w.settings.seconds.as_ref().unwrap(), "91"),
        (w.settings.percent.as_ref().unwrap(), "77"),
        (
            w.settings.socket.as_ref().unwrap(),
            " /tmp/kurisu-fixture.sock ",
        ),
    ] {
        input.update(cx, |input, cx| input.set_value(value, cx));
    }
}

fn saved(w: &Workspace) {
    assert!(w.settings.saved);
    assert!(w.settings.save_error.is_empty());
    assert!(w.barrier_error.is_empty());
    assert!(w.barrier.is_none());
    assert_eq!(w.active_writes, 0);
}

#[gpui::test]
fn successful_tracking_save_precedes_close_readiness(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
        assert_eq!(w.active_writes, 1);
        assert!(w.barrier.is_some());
    });
    f.wait(&w, cx, "successful close", |_| obs.ready().len() == 1);
    w.read_with(cx, |w, _| saved(w));
    assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "auto".into())]);
    f.persisted(true);
}

fn failed_close(tc: &mut TestAppContext, intent: CloseIntent) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.request_close(intent, win, cx);
        assert_eq!(w.active_writes, 1);
        assert!(w.barrier.is_some());
    });
    f.rejected(&w, cx);
    obs.held();
    w.read_with(cx, |w, _| {
        assert_eq!(w.barrier, Some(BarrierAction::Close(intent)));
        assert!(
            w.barrier_error
                .contains("playback tracking: controlled tracking failure")
        );
        assert_eq!(w.settings.cfg.mode, "auto");
    });
    cx.update(|win, app| win.draw(app).clear(app));
    let retry = cx
        .debug_bounds("retry-barrier")
        .expect("Retry saves must be visible");
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
    w.read_with(cx, |w, _| {
        assert!(
            w.settings.cfg_saving,
            "Retry saves did not resubmit failed tracking"
        );
    });
    f.rejected(&w, cx);
    obs.held();
    w.read_with(cx, |w, _| {
        assert!(w.barrier_error.contains("controlled tracking failure"))
    });
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    w.update_in(cx, |w, win, cx| {
        w.retry_barrier(win, cx);
        assert!(w.settings.cfg_saving);
        assert!(w.barrier.is_some());
    });
    f.wait(&w, cx, "tracking retry and close", |_| {
        obs.ready().len() == 1
    });
    w.read_with(cx, |w, _| saved(w));
    assert_eq!(obs.ready(), vec![(intent, "auto".into())]);
    f.persisted(true);
}

#[gpui::test]
fn failed_tracking_save_holds_queued_quit_and_retries(tc: &mut TestAppContext) {
    failed_close(tc, CloseIntent::Quit);
}

#[gpui::test]
fn failed_tracking_save_holds_queued_window_close_and_retries(tc: &mut TestAppContext) {
    failed_close(tc, CloseIntent::Window);
}

#[gpui::test]
fn failed_tracking_before_close_survives_cancel_and_manual_retry(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
        w.cancel_barrier(win, cx);
    });
    f.rejected(&w, cx);
    obs.held();
    w.update_in(cx, |w, win, cx| w.request_close(CloseIntent::Quit, win, cx));
    cx.run_until_parked();
    obs.held();
    w.update_in(cx, |w, win, cx| {
        assert!(w.barrier_error.contains("controlled tracking failure"));
        w.cancel_barrier(win, cx);
        assert!(w.barrier.is_none());
        assert!(!w.settings.save_error.is_empty());
    });
    cx.run_until_parked();
    assert_eq!(
        obs.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| matches!(e, WorkspaceEvent::CloseCancelled))
            .count(),
        2
    );
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    w.update_in(cx, |w, win, cx| w.save_tracking_settings(win, cx));
    f.wait(&w, cx, "manual tracking retry", |w| {
        w.settings.saved && w.active_writes == 0
    });
    obs.held();
    w.read_with(cx, |w, _| saved(w));
    f.persisted(true);
    w.update_in(cx, |w, win, cx| w.request_close(CloseIntent::Quit, win, cx));
    f.wait(&w, cx, "new close", |_| obs.ready().len() == 1);
}

fn unsubmitted_draft(tc: &mut TestAppContext, query_failure: bool) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    if query_failure {
        reject_other_saves(&f);
    }
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        if query_failure {
            w.stage_query(Some("retained query".into()));
        }
        w.request_close(CloseIntent::Quit, win, cx);
    });
    if query_failure {
        f.wait(&w, cx, "query failure", |w| {
            w.active_writes == 0 && !w.query_error.is_empty()
        });
        obs.held();
        f.db.0
            .lock()
            .execute_batch("DROP TRIGGER reject_query; DROP TRIGGER reject_preference;")
            .unwrap();
        w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
    }
    f.wait(&w, cx, "abandoned unsubmitted draft", |_| {
        obs.ready().len() == 1
    });
    assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "off".into())]);
    f.persisted(false);
}

#[gpui::test]
fn unsubmitted_tracking_draft_can_be_abandoned(tc: &mut TestAppContext) {
    unsubmitted_draft(tc, false);
}

#[gpui::test]
fn retrying_other_saves_does_not_submit_tracking_draft(tc: &mut TestAppContext) {
    unsubmitted_draft(tc, true);
}

fn reject_other_saves(f: &Fixture) {
    f.db.0
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_query BEFORE INSERT ON settings
         WHEN NEW.key='native_preference:kurisu.list.q'
         BEGIN SELECT RAISE(ABORT, 'controlled query failure'); END;
         CREATE TRIGGER reject_preference BEFORE INSERT ON settings
         WHEN NEW.key='native_preference:kurisu.list.filter'
         BEGIN SELECT RAISE(ABORT, 'controlled preference failure'); END;",
        )
        .unwrap();
}

fn combined_errors(tc: &mut TestAppContext, tracking_first: bool) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    reject_other_saves(&f);
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.stage_query(Some("retained query".into()));
        w.select_filter("COMPLETED", win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.wait(&w, cx, "three failed saves", |w| {
        w.active_writes == 0
            && w.preference_active.is_none()
            && !w.query_error.is_empty()
            && w.preference_error().is_some()
            && !w.settings.save_error.is_empty()
    });
    obs.held();
    w.read_with(cx, |w, _| {
        assert!(w.barrier_error.contains("controlled query failure"))
    });
    if tracking_first {
        f.db.0
            .lock()
            .execute_batch("DROP TRIGGER reject_tracking;")
            .unwrap();
        w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
        f.wait(&w, cx, "tracking recovery with other failures", |w| {
            w.settings.saved && w.active_writes == 0 && w.preference_active.is_none()
        });
        f.persisted(true);
        obs.held();
        w.read_with(cx, |w, _| {
            assert!(w.settings.save_error.is_empty());
            assert!(w.barrier_error.contains("controlled query failure"));
        });
        // A later retry must leave the successfully submitted tracking save alone.
        f.reject_tracking();
    }
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_query;")
        .unwrap();
    w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
    f.wait(&w, cx, "query recovery with preference failure", |w| {
        w.active_writes == 0
            && w.query_pending.is_none()
            && w.preference_active.is_none()
            && w.barrier_error.contains("controlled preference failure")
    });
    obs.held();
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_preference;")
        .unwrap();
    w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
    if !tracking_first {
        f.wait(&w, cx, "remaining tracking failure becomes visible", |w| {
            w.active_writes == 0
                && !w.preference_has_work()
                && w.barrier_error.contains("controlled tracking failure")
        });
        obs.held();
        f.persisted(false);
        f.db.0
            .lock()
            .execute_batch("DROP TRIGGER reject_tracking;")
            .unwrap();
        w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
    }
    f.wait(&w, cx, "all submitted saves recovered", |_| {
        obs.ready().len() == 1
    });
    w.read_with(cx, |w, _| saved(w));
    assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "auto".into())]);
    f.persisted(true);
    assert_eq!(
        f.db.get_setting("native_preference:kurisu.list.q")
            .unwrap()
            .as_deref(),
        Some("retained query")
    );
    assert_eq!(
        f.db.get_setting("native_preference:kurisu.list.filter")
            .unwrap()
            .as_deref(),
        Some("COMPLETED")
    );
}

#[gpui::test]
fn combined_failures_expose_remaining_tracking_error(tc: &mut TestAppContext) {
    combined_errors(tc, false);
}

#[gpui::test]
fn tracking_recovery_does_not_hide_other_save_failures(tc: &mut TestAppContext) {
    combined_errors(tc, true);
}

fn transition(tc: &mut TestAppContext, action: BarrierAction, completed_failure: bool) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
    });
    if completed_failure {
        f.rejected(&w, cx);
    }
    w.update_in(cx, |w, win, cx| {
        if action == BarrierAction::Login {
            w.login(true, None, win, cx);
        } else {
            w.start_barrier(action, win, cx);
        }
        assert_eq!(
            w.barrier,
            Some(action),
            "failed tracking must hold the requested transition"
        );
    });
    f.rejected(&w, cx);
    obs.held();
    w.read_with(cx, |w, _| {
        assert_eq!(w.barrier, Some(action));
        assert!(w.barrier_error.contains("controlled tracking failure"));
        assert_eq!(w.queued_login.is_some(), action == BarrierAction::Login);
        assert!(!w.auth_busy && !w.update_busy);
    });
    if action == BarrierAction::Install {
        w.update_in(cx, |w, win, cx| w.cancel_barrier(win, cx));

        return;
    }
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    w.update_in(cx, |w, win, cx| w.retry_barrier(win, cx));
    f.wait(&w, cx, "deferred account transition", |w| {
        w.barrier.is_none()
            && w.auth_intent > 0
            && !w.auth_busy
            && !w.restoring_session
            && w.settings.cfg_loaded
    });
    assert!(!obs.transitions.lock().unwrap().is_empty());
    for outcome in obs.transitions.lock().unwrap().iter() {
        assert!(outcome.auth_intent > 0);
        assert!(!outcome.update_busy);
        assert_eq!(
            outcome.mode, "auto",
            "account transition ran before tracking persisted"
        );
    }
    w.read_with(cx, |w, _| {
        assert!(w.queued_login.is_none());
        assert!(w.barrier_error.is_empty());
        assert!(w.settings.save_error.is_empty());
        assert_eq!(w.settings.cfg.mode, "auto");
        if action == BarrierAction::Login {
            assert!(w.auth_error.contains("registered AniList client"));
        } else {
            assert!(w.auth_error.is_empty());
        }
    });
    assert!(obs.ready().is_empty());
    f.persisted(true);
}

#[gpui::test]
fn failed_tracking_holds_queued_login_until_retry(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Login, false);
}

#[gpui::test]
fn completed_tracking_failure_enters_login_barrier(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Login, true);
}

#[gpui::test]
fn failed_tracking_holds_logout_until_retry(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Logout, false);
}

#[gpui::test]
fn completed_tracking_failure_holds_logout_until_retry(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Logout, true);
}

#[gpui::test]
fn failed_tracking_holds_install(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Install, false);
}

#[gpui::test]
fn completed_tracking_failure_holds_install(tc: &mut TestAppContext) {
    transition(tc, BarrierAction::Install, true);
}

fn edit_inputs(w: &mut Workspace, cx: &mut Context<Workspace>, values: [&str; 3]) {
    for (input, value) in [
        w.settings.seconds.as_ref().unwrap(),
        w.settings.percent.as_ref().unwrap(),
        w.settings.socket.as_ref().unwrap(),
    ]
    .into_iter()
    .zip(values)
    {
        input.update(cx, |input, cx| {
            input.set_value(value, cx);
            cx.emit(TextInputEvent::Changed(value.into()));
        });
    }
}

fn config_eq(actual: &TrackingConfig, expected: &TrackingConfig) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

fn assert_config(f: &Fixture, expected: &TrackingConfig) {
    config_eq(&f.runtime.block_on(f.core.get_tracking_config()), expected);
    let db = Db::open(&f.profile.path.join("kurisu.db")).unwrap();
    config_eq(&TrackingConfig::load(&db), expected);
}

fn published_control(cx: &mut VisualTestContext, label: &str) -> gpui::accesskit::Node {
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    cx.take_accessibility_tree_update()
        .expect("Settings accessibility tree")
        .nodes
        .into_iter()
        .find(|(_, node)| node.label() == Some(label))
        .unwrap_or_else(|| panic!("missing Settings control {label}"))
        .1
}

fn click_retry(cx: &mut VisualTestContext) {
    cx.update(|win, app| win.draw(app).clear(app));
    let retry = cx
        .debug_bounds("retry-barrier")
        .expect("Retry saves must be visible");
    cx.simulate_click(retry.center(), gpui::Modifiers::default());
}

#[gpui::test]
fn retry_preserves_normalized_submission_and_newer_draft(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, _, cx| {
        w.edit_tracking_mode("prompt", cx);
        edit_inputs(w, cx, ["99999", "0", " /tmp/submitted.sock "]);
    });
    cx.run_until_parked();
    let discord = published_control(cx, "Discord Rich Presence");
    assert_eq!(discord.role(), Role::CheckBox);
    assert_eq!(discord.toggled(), Some(Toggled::True));
    let bounds = discord.bounds().unwrap();
    let scale = cx.update(|win, _| win.scale_factor());
    cx.simulate_click(
        gpui::point(
            px(((bounds.x0 + bounds.x1) / 2.) as f32 / scale),
            px(((bounds.y0 + bounds.y1) / 2.) as f32 / scale),
        ),
        gpui::Modifiers::default(),
    );
    w.read_with(cx, |w, _| {
        assert!(w.settings.cfg.auto_ask);
        assert!(!w.settings.cfg.discord_enabled);
    });
    w.update_in(cx, |w, win, cx| {
        w.save_tracking_settings(win, cx);
        w.request_close(CloseIntent::Window, win, cx);
    });
    f.rejected(&w, cx);
    let submitted = TrackingConfig {
        mode: "prompt".into(),
        prompt_seconds: 3600,
        auto_percent: 1,
        discord_enabled: false,
        mpv_ipc_socket: "/tmp/submitted.sock".into(),
        ..TrackingConfig::default()
    };
    w.update_in(cx, |w, win, cx| {
        w.cancel_barrier(win, cx);
        w.edit_tracking_mode("auto", cx);
        w.toggle_tracking_option(false, cx);
        w.toggle_tracking_option(true, cx);
        edit_inputs(w, cx, ["23", "99", "/tmp/unsubmitted.sock"]);
    });
    cx.run_until_parked();
    w.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Window, win, cx)
    });
    click_retry(cx);
    f.rejected(&w, cx);
    obs.held();
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    click_retry(cx);
    f.wait(&w, cx, "original submission retry", |_| {
        obs.ready().len() == 1
    });
    assert_config(&f, &submitted);
    assert_eq!(obs.ready(), vec![(CloseIntent::Window, "prompt".into())]);
    w.read_with(cx, |w, cx| {
        assert_eq!(w.settings.cfg.mode, "auto");
        assert!(!w.settings.cfg.auto_ask);
        assert!(
            w.settings.cfg.discord_enabled,
            "retry overwrote the newer Discord draft"
        );
        for (input, value) in [
            (w.settings.seconds.as_ref().unwrap(), "23"),
            (w.settings.percent.as_ref().unwrap(), "99"),
            (w.settings.socket.as_ref().unwrap(), "/tmp/unsubmitted.sock"),
        ] {
            assert_eq!(input.read(cx).value(), value);
        }
        assert!(!w.settings.saved, "retry marked a newer draft as saved");
        assert!(w.settings.save_error.is_empty() && w.barrier_error.is_empty());
    });
    w.update_in(cx, |w, win, cx| {
        w.resume_after_close_visibility(false, win, cx)
    });
    displayed_account(&f, &w, cx);
    w.read_with(cx, |w, _| config_eq(&w.settings.cfg, &submitted));
    assert_config(&f, &submitted);
}

fn displayed_account(f: &Fixture, w: &Entity<Workspace>, cx: &mut VisualTestContext) {
    w.update_in(cx, |w, win, cx| {
        w.set_profile(
            Some(kurisu_core::models::User {
                id: 47,
                name: "Controlled cached account".into(),
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        )
    });
    f.load(w, cx);
}

fn expire(w: &Entity<Workspace>, cx: &mut VisualTestContext) {
    // Simulate the event boundary. Core verification and Settings reset are real.
    w.update_in(cx, |w, win, cx| {
        w.handle_event(
            kurisu_core::Event {
                generation: w.generation,
                account_id: Some(47),
                prompt_id: None,
                kind: kurisu_core::EventKind::AuthExpired,
            },
            win,
            cx,
        )
    });
}

#[gpui::test]
fn completed_tracking_failure_survives_expiration_and_generation_change(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    displayed_account(&f, &w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.rejected(&w, cx);
    expire(&w, cx);
    f.wait(&w, cx, "expiration reset", |w| {
        w.profile.is_none() && w.settings.cfg_loaded
    });
    w.read_with(cx, |w, _| {
        assert!(
            w.settings.tracking_save_error().is_some(),
            "expiration erased submitted failure"
        )
    });
    f.core.cancel_login();
    w.update_in(cx, |w, win, cx| w.set_profile(None, win, cx));
    click_retry(cx);
    f.rejected(&w, cx);
    obs.held();
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    click_retry(cx);
    f.wait(&w, cx, "retry after expiration", |_| obs.ready().len() == 1);
    f.persisted(true);
    assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "auto".into())]);
}

#[gpui::test]
fn explicit_tracking_save_replaces_failed_payload_before_retry(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    let obs = Observers::new(&f, &w, cx);
    f.reject_tracking();
    w.update_in(cx, |w, win, cx| {
        draft(w, cx);
        w.save_tracking_settings(win, cx);
    });
    f.rejected(&w, cx);
    w.update_in(cx, |w, _, cx| {
        w.edit_tracking_mode("prompt", cx);
        w.toggle_tracking_option(false, cx);
        w.toggle_tracking_option(true, cx);
        edit_inputs(w, cx, ["222", "42", " /tmp/replacement.sock "]);
    });
    cx.run_until_parked();
    w.update_in(cx, |w, win, cx| w.save_tracking_settings(win, cx));
    f.rejected(&w, cx);
    w.update_in(cx, |w, _, cx| {
        edit_inputs(w, cx, ["17", "18", "/tmp/unsent.sock"])
    });
    cx.run_until_parked();
    w.update_in(cx, |w, win, cx| w.request_close(CloseIntent::Quit, win, cx));
    click_retry(cx);
    f.rejected(&w, cx);
    obs.held();
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tracking;")
        .unwrap();
    click_retry(cx);
    f.wait(&w, cx, "explicit replacement retry", |_| {
        obs.ready().len() == 1
    });
    assert_config(
        &f,
        &TrackingConfig {
            mode: "prompt".into(),
            prompt_seconds: 222,
            auto_percent: 42,
            mpv_ipc_socket: "/tmp/replacement.sock".into(),
            ..TrackingConfig::default()
        },
    );
    w.read_with(cx, |w, cx| {
        assert!(!w.settings.saved);
        assert_eq!(w.settings.seconds.as_ref().unwrap().read(cx).value(), "17");
        assert_eq!(w.settings.percent.as_ref().unwrap().read(cx).value(), "18");
        assert_eq!(
            w.settings.socket.as_ref().unwrap().read(cx).value(),
            "/tmp/unsent.sock"
        );
        assert!(w.settings.save_error.is_empty() && w.barrier_error.is_empty());
    });
}

pub(super) struct SaveGate {
    after_write: bool,
    entered: Arc<std::sync::atomic::AtomicBool>,
    release: tokio::sync::oneshot::Receiver<()>,
}

pub(super) async fn pause_save(gate: &mut Option<SaveGate>, after_write: bool) {
    if gate
        .as_ref()
        .is_some_and(|gate| gate.after_write == after_write)
    {
        let gate = gate.take().unwrap();
        gate.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = gate.release.await;
    }
}

fn inflight_reset(tc: &mut TestAppContext, failure: bool, after_write: bool) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    displayed_account(&f, &w, cx);
    let obs = Observers::new(&f, &w, cx);
    if failure {
        f.reject_tracking();
    }
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, paused) = tokio::sync::oneshot::channel();
    w.update_in(cx, |w, win, cx| {
        w.settings.save_gate = Some(SaveGate {
            after_write,
            entered: entered.clone(),
            release: paused,
        });
        draft(w, cx);
        w.save_tracking_settings(win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.wait(&w, cx, "save gate reached", |_| {
        entered.load(std::sync::atomic::Ordering::SeqCst)
    });
    f.persisted(after_write && !failure);
    w.update_in(cx, |w, win, cx| {
        w.handle_event(
            kurisu_core::Event {
                generation: w.generation,
                account_id: Some(48),
                prompt_id: None,
                kind: kurisu_core::EventKind::AuthExpired,
            },
            win,
            cx,
        )
    });
    cx.run_until_parked();
    w.read_with(cx, |w, _| {
        assert_eq!(w.profile.as_ref().map(|u| u.id), Some(47))
    });
    expire(&w, cx);
    f.wait(
        &w,
        cx,
        "authentication invalidated while save paused",
        |w| w.profile.is_none() && w.settings.cfg_loaded,
    );
    assert!(!f.runtime.block_on(f.core.is_logged_in()));
    let generation = f.core.generation();
    f.core.cancel_login();
    assert_ne!(f.core.generation(), generation);
    w.update_in(cx, |w, win, cx| {
        w.set_profile(
            Some(kurisu_core::models::User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        assert_eq!(w.generation, f.core.generation());
        assert!(
            w.settings.cfg_saving,
            "account reset forgot the in-flight save"
        );
        assert!(w.settings.save_error.is_empty());
        w.save_tracking_settings(win, cx);
        w.retry_barrier(win, cx);
        if after_write {
            w.cancel_barrier(win, cx);
            edit_inputs(w, cx, ["33", "12", "/tmp/newer-inflight.sock"]);
        }
    });
    f.wait(&w, cx, "account reset writes settled behind save", |w| {
        w.active_writes == 1 && !w.preference_has_work()
    });
    obs.held();
    release.send(()).unwrap();
    f.wait(&w, cx, "global save callback after account reset", |w| {
        w.active_writes == 0
    });
    w.read_with(cx, |w, _| {
        assert!(
            !w.settings.cfg_saving,
            "global tracking completion ignored after account change"
        )
    });
    if failure {
        f.rejected(&w, cx);
        obs.held();
        w.read_with(cx, |w, _| {
            assert_eq!(w.barrier.is_none(), after_write);
            assert_eq!(w.barrier_error.is_empty(), after_write);
        });
        if after_write {
            w.update_in(cx, |w, win, cx| w.login(true, None, win, cx));
            w.read_with(cx, |w, _| assert_eq!(w.barrier, Some(BarrierAction::Login)));
        }
        click_retry(cx);
        f.rejected(&w, cx);
        obs.held();
        f.db.0
            .lock()
            .execute_batch("DROP TRIGGER reject_tracking;")
            .unwrap();
        click_retry(cx);
        if after_write {
            f.wait(&w, cx, "login after recovered global save", |w| {
                w.auth_intent > 0 && !w.auth_busy && !w.restoring_session
            });
            assert!(obs.ready().is_empty());
            assert!(!obs.transitions.lock().unwrap().is_empty());
            for transition in obs.transitions.lock().unwrap().iter() {
                assert_eq!(transition.mode, "auto");
            }
            w.read_with(cx, |w, _| {
                assert!(w.auth_error.contains("registered AniList client"))
            });
        } else {
            f.wait(&w, cx, "close after recovered global save", |_| {
                obs.ready().len() == 1
            });
        }
    } else {
        if after_write {
            obs.held();
            w.read_with(cx, |w, cx| {
                assert!(!w.settings.saved);
                assert_eq!(w.settings.seconds.as_ref().unwrap().read(cx).value(), "33");
                assert_eq!(w.settings.percent.as_ref().unwrap().read(cx).value(), "12");
                assert_eq!(
                    w.settings.socket.as_ref().unwrap().read(cx).value(),
                    "/tmp/newer-inflight.sock"
                );
            });
            w.update_in(cx, |w, win, cx| w.request_close(CloseIntent::Quit, win, cx));
            f.wait(&w, cx, "fresh close after cancelled completion", |_| {
                obs.ready().len() == 1
            });
        } else {
            w.read_with(cx, |w, _| assert!(w.settings.saved));
            assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "auto".into())]);
        }
    }
    f.persisted(true);
    w.read_with(cx, |w, _| {
        assert!(w.settings.save_error.is_empty() && w.barrier_error.is_empty())
    });
}

#[gpui::test]
fn pending_tracking_write_survives_expiration_and_generation_change(tc: &mut TestAppContext) {
    inflight_reset(tc, false, false);
}

#[gpui::test]
fn pending_tracking_failure_survives_expiration_and_generation_change(tc: &mut TestAppContext) {
    inflight_reset(tc, true, false);
}

#[gpui::test]
fn pending_tracking_success_ack_preserves_cancel_and_newer_draft(tc: &mut TestAppContext) {
    inflight_reset(tc, false, true);
}

#[gpui::test]
fn pending_tracking_failure_ack_survives_reset_and_cancel_then_login(tc: &mut TestAppContext) {
    inflight_reset(tc, true, true);
}

#[gpui::test]
fn account_reset_abandons_truly_unsubmitted_tracking_draft(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    displayed_account(&f, &w, cx);
    let obs = Observers::new(&f, &w, cx);
    w.update_in(cx, |w, _, cx| draft(w, cx));
    expire(&w, cx);
    f.wait(&w, cx, "unsubmitted draft reset", |w| {
        w.profile.is_none() && w.settings.cfg_loaded
    });
    w.update_in(cx, |w, win, cx| {
        assert_eq!(w.settings.cfg.mode, "off");
        assert!(w.settings.tracking_save_error().is_none());
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.wait(&w, cx, "unsubmitted draft close", |_| {
        obs.ready().len() == 1
    });
    assert_eq!(obs.ready(), vec![(CloseIntent::Quit, "off".into())]);
    f.persisted(false);
}

#[gpui::test]
fn failed_app_toggle_holds_window_close_until_retry_and_clears_queued_login(
    tc: &mut TestAppContext,
) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    f.wait(&w, cx, "app toggles loaded", |w| {
        w.settings.close_to_tray.loaded && w.settings.auto_update.loaded
    });
    let obs = Observers::new(&f, &w, cx);
    f.db.0
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_tray BEFORE INSERT ON settings
         WHEN NEW.key='close_to_tray'
         BEGIN SELECT RAISE(ABORT, 'controlled tray failure'); END;",
        )
        .unwrap();
    w.update_in(cx, |w, win, cx| {
        w.toggle_app_setting(SettingKey::CloseToTray, win, cx);
        w.request_close(CloseIntent::Window, win, cx);
        assert_eq!(w.active_writes, 1);
    });
    f.wait(&w, cx, "failed tray save", |w| {
        w.active_writes == 0
            && w.settings
                .close_to_tray
                .save_error
                .contains("controlled tray failure")
    });
    obs.held();
    w.update_in(cx, |w, win, cx| {
        assert!(!w.settings.close_to_tray.value);
        assert_eq!(w.settings.close_to_tray.submitted, Some(true));
        assert!(
            w.barrier_error
                .contains("the window setting: controlled tray failure")
        );
        w.cancel_barrier(win, cx);
        w.login(false, Some("queued-secret-token".into()), win, cx);
        assert_eq!(w.barrier, Some(BarrierAction::Login));
        assert!(w.queued_login.is_some());
        w.request_close(CloseIntent::Window, win, cx);
        assert_eq!(w.barrier, Some(BarrierAction::Close(CloseIntent::Window)));
        assert!(w.queued_login.is_none());
    });
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tray;")
        .unwrap();
    click_retry(cx);
    f.wait(&w, cx, "tray retry and window close", |_| {
        obs.ready().len() == 1
    });
    w.read_with(cx, |w, _| {
        assert!(w.settings.close_to_tray.value);
        assert!(w.settings.close_to_tray.submitted.is_none());
        assert!(w.settings.close_to_tray.save_error.is_empty());
        assert!(w.queued_login.is_none());
    });
    assert_eq!(
        f.runtime
            .block_on(f.core.get_app_setting("close_to_tray".into()))
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert_eq!(obs.ready(), vec![(CloseIntent::Window, "off".into())]);
}

#[gpui::test]
fn pending_app_toggle_survives_expiration_and_generation_change(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&w, cx);
    displayed_account(&f, &w, cx);
    f.wait(&w, cx, "app toggles loaded", |w| {
        w.settings.close_to_tray.loaded
    });
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, paused) = tokio::sync::oneshot::channel();
    w.update_in(cx, |w, win, cx| {
        w.settings.toggle_gate = Some(SaveGate {
            after_write: false,
            entered: entered.clone(),
            release: paused,
        });
        w.toggle_app_setting(SettingKey::CloseToTray, win, cx);
    });
    f.wait(&w, cx, "toggle gate reached", |_| {
        entered.load(std::sync::atomic::Ordering::SeqCst)
    });
    expire(&w, cx);
    f.wait(&w, cx, "expiration reset during toggle save", |w| {
        w.profile.is_none() && w.settings.cfg_loaded
    });
    f.core.cancel_login();
    w.update_in(cx, |w, win, cx| {
        w.set_profile(
            Some(kurisu_core::models::User {
                id: 48,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        assert!(w.settings.close_to_tray.saving);
        assert!(!w.settings.close_to_tray.loading);
        assert_eq!(w.settings.close_to_tray.submitted, Some(true));
    });
    release.send(()).unwrap();
    f.wait(&w, cx, "toggle save settled", |w| w.active_writes == 0);
    w.read_with(cx, |w, _| {
        assert!(!w.settings.close_to_tray.saving);
        assert!(w.settings.close_to_tray.value);
        assert!(w.settings.close_to_tray.submitted.is_none());
    });
    assert_eq!(
        f.runtime
            .block_on(f.core.get_app_setting("close_to_tray".into()))
            .unwrap()
            .as_deref(),
        Some("1")
    );
}

#[gpui::test]
fn fenced_settings_set_value_reverts_text_and_preserves_revision(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    f.runtime
        .block_on(f.core.set_tracking_config(
            "prompt".into(),
            91,
            77,
            true,
            "/controlled.sock".into(),
            false,
        ))
        .unwrap();
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    f.load(&w, cx);

    let initial_rev = w.read_with(cx, |w, _| w.settings.revision);
    w.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
    });
    cx.simulate_accessibility_activation();
    cx.run_until_parked();
    let tree = cx.take_accessibility_tree_update().unwrap();
    let (seconds_id, _) = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some("91"))
        .expect("seconds input in accessibility tree");
    let (socket_id, _) = tree
        .nodes
        .iter()
        .find(|(_, node)| {
            node.role() == Role::TextInput && node.value() == Some("/controlled.sock")
        })
        .expect("socket input in accessibility tree");
    let seconds_id = *seconds_id;
    let socket_id = *socket_id;
    for (target_node, mutated) in [(seconds_id, "999"), (socket_id, "/mutated.sock")] {
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::SetValue,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node,
            data: Some(gpui::accesskit::ActionData::Value(mutated.into())),
        });
    }
    w.read_with(cx, |w, app| {
        assert_eq!(w.settings.revision, initial_rev);
        assert_eq!(
            w.settings.seconds.as_ref().unwrap().read(app).value(),
            "91",
            "fenced SetValue mutated seconds text while suppressing revision"
        );
        assert_eq!(
            w.settings.socket.as_ref().unwrap().read(app).value(),
            "/controlled.sock",
            "fenced SetValue mutated socket text while suppressing revision"
        );
    });
    w.update_in(cx, |w, win, cx| {
        w.resume_after_close(win, cx);
        w.auth_busy = true;
        cx.notify();
    });
    cx.run_until_parked();
    let auth_tree = cx.take_accessibility_tree_update().unwrap();
    let (auth_seconds_id, _) = auth_tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some("91"))
        .expect("seconds input during auth_busy");
    let auth_seconds_id = *auth_seconds_id;
    cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
        action: gpui::AccessibleAction::SetValue,
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: auth_seconds_id,
        data: Some(gpui::accesskit::ActionData::Value("555".into())),
    });
    w.read_with(cx, |w, app| {
        assert_eq!(w.settings.revision, initial_rev);
        assert_eq!(w.settings.seconds.as_ref().unwrap().read(app).value(), "91");
    });
}

#[gpui::test]
fn persistent_app_toggle_failure_can_be_abandoned_to_close(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    f.load(&w, cx);
    f.wait(&w, cx, "app toggles loaded", |w| {
        w.settings.close_to_tray.loaded
    });
    let obs = Observers::new(&f, &w, cx);
    f.db.0
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_tray_forever BEFORE INSERT ON settings
         WHEN NEW.key='close_to_tray'
         BEGIN SELECT RAISE(ABORT, 'disk read-only'); END;",
        )
        .unwrap();
    w.update_in(cx, |w, win, cx| {
        w.toggle_app_setting(SettingKey::CloseToTray, win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.wait(&w, cx, "failed tray save", |w| {
        w.active_writes == 0 && !w.settings.close_to_tray.save_error.is_empty()
    });
    obs.held();

    cx.update(|win, app| win.draw(app).clear(app));
    let cancel = cx
        .debug_bounds("cancel-failed-barrier")
        .expect("Cancel button on BarrierError");
    cx.simulate_click(cancel.center(), gpui::Modifiers::default());
    cx.update(|win, app| win.draw(app).clear(app));
    let dismiss_inline = cx
        .debug_bounds("dismiss-save-close_to_tray")
        .expect("Settings row must expose Dismiss for a failed app toggle");
    cx.simulate_click(dismiss_inline.center(), gpui::Modifiers::default());
    w.read_with(cx, |w, _| {
        assert!(!w.settings.close_to_tray.value);
        assert!(w.settings.close_to_tray.submitted.is_none());
        assert!(w.settings.close_to_tray.save_error.is_empty());
    });

    w.update_in(cx, |w, win, cx| {
        w.toggle_app_setting(SettingKey::CloseToTray, win, cx);
        w.request_close(CloseIntent::Quit, win, cx);
    });
    f.wait(&w, cx, "second failed tray save", |w| {
        w.active_writes == 0 && !w.settings.close_to_tray.save_error.is_empty()
    });
    obs.held();
    cx.update(|win, app| win.draw(app).clear(app));
    let discard = cx
        .debug_bounds("discard-barrier")
        .expect("BarrierError must offer a discard/close-without-saving action");
    cx.simulate_click(discard.center(), gpui::Modifiers::default());
    f.wait(&w, cx, "abandoned toggle closes cleanly", |_| {
        obs.ready().len() == 1
    });
    w.read_with(cx, |w, _| {
        assert!(!w.settings.close_to_tray.value);
        assert!(w.settings.close_to_tray.submitted.is_none());
        assert!(w.settings.close_to_tray.save_error.is_empty());
        assert!(w.close_ready);
    });
}

#[gpui::test]
fn settings_controls_and_inputs_respect_all_edit_fences(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    f.runtime
        .block_on(f.core.set_tracking_config(
            "prompt".into(),
            91,
            77,
            true,
            "/controlled.sock".into(),
            true,
        ))
        .unwrap();
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1600.)));
    f.load(&w, cx);
    f.wait(&w, cx, "app toggles loaded", |w| {
        w.settings.close_to_tray.loaded && w.settings.auto_update.loaded
    });

    f.db.0
        .lock()
        .execute_batch(
            "CREATE TRIGGER reject_tray_fence BEFORE INSERT ON settings
         WHEN NEW.key='close_to_tray'
         BEGIN SELECT RAISE(ABORT, 'tray failure'); END;",
        )
        .unwrap();
    w.update_in(cx, |w, win, cx| {
        w.toggle_app_setting(SettingKey::CloseToTray, win, cx);
    });
    f.wait(&w, cx, "failed tray save", |w| {
        w.active_writes == 0 && !w.settings.close_to_tray.save_error.is_empty()
    });
    f.db.0
        .lock()
        .execute_batch("DROP TRIGGER reject_tray_fence;")
        .unwrap();

    cx.simulate_accessibility_activation();
    for mode in ["update_busy", "auth_busy", "barrier"] {
        let initial_rev = w.update_in(cx, |w, _, cx| {
            w.update_busy = mode == "update_busy";
            w.auth_busy = mode == "auth_busy";
            w.barrier = (mode == "barrier").then_some(BarrierAction::Close(CloseIntent::Window));
            cx.notify();
            w.settings.revision
        });
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (seconds_id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::TextInput && node.value() == Some("91"))
            .expect("seconds input in accessibility tree");
        let seconds_id = *seconds_id;
        for label in [
            "Save tracking",
            "Automatically check for updates",
            "Discord Rich Presence",
            "Off — don't track playback",
        ] {
            let (_, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing {label} under {mode}"));
            assert!(
                node.is_disabled(),
                "Settings control {label} stayed enabled during {mode}"
            );
        }
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::SetValue,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: seconds_id,
            data: Some(gpui::accesskit::ActionData::Value("777".into())),
        });
        w.update_in(cx, |w, win, cx| {
            assert_eq!(
                w.settings.revision, initial_rev,
                "revision mutated during {mode}"
            );
            assert_eq!(
                w.settings.seconds.as_ref().unwrap().read(cx).value(),
                "91",
                "seconds text mutated during {mode}"
            );
            w.toggle_app_setting(SettingKey::AutoUpdate, win, cx);
            w.dismiss_app_setting_error(SettingKey::CloseToTray, cx);
            w.edit_tracking_mode("auto", cx);
            w.toggle_tracking_option(true, cx);
            w.save_tracking_settings(win, cx);
            assert_eq!(w.active_writes, 0, "settings write admitted during {mode}");
            assert_eq!(w.settings.cfg.mode, "prompt");
            assert!(w.settings.cfg.discord_enabled);
            assert!(w.settings.auto_update.value);
            assert_eq!(w.settings.close_to_tray.submitted, Some(true));
            assert!(!w.settings.close_to_tray.save_error.is_empty());
        });
    }
    w.update_in(cx, |w, _, cx| {
        w.update_busy = false;
        w.auth_busy = false;
        w.barrier = None;
        cx.notify();
    });
}
