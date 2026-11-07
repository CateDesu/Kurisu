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
    let browser = Arc::new(BrowserSignInProbe(std::sync::Mutex::new(vec![])));
    let (core, runtime, _dir) = fixture_with_platform(browser.clone());
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
    settle(cx, &runtime);
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.login(true, None, window, cx);
    });
    settle_browser_login(cx, &runtime, &browser, 1);
    let abort = workspace.read_with(cx, |workspace, _| {
        workspace.auth_abort.as_ref().unwrap().clone()
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
