use super::*;

fn original() -> TrackingConfig {
    TrackingConfig {
        mode: "auto".into(),
        prompt_seconds: 91,
        auto_percent: 77,
        auto_ask: false,
        discord_enabled: false,
        mpv_ipc_socket: "/controlled-only.sock".into(),
    }
}

fn open_failed_read(f: &Fixture, w: &Entity<Workspace>, cx: &mut VisualTestContext) {
    original().save(&f.db).unwrap();
    f.db.0
        .lock()
        .execute_batch("UPDATE settings SET value=X'00' WHERE key='tracking_prompt_seconds'")
        .unwrap();
    assert!(
        f.db.get_settings_batch(&["tracking_prompt_seconds"])
            .is_err()
    );
    w.update_in(cx, |w, win, cx| {
        w.route = Route::Settings;
        w.enter_settings(win, cx);
        cx.notify();
    });
    f.wait(w, cx, "tracking read failure", |w| {
        !w.settings.cfg_loading && !w.restoring_session && w.preferences_loaded
    });
    w.read_with(cx, |w, _| {
        assert!(!w.settings.cfg_loaded, "failed read was marked loaded");
        assert!(w.settings.cfg_error.contains("Invalid column type Blob"));
        assert!(w.settings.save_error.is_empty() && w.settings.submitted.is_none());
        assert_eq!(w.active_writes, 0);
    });
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    cx.update(|win, app| win.draw(app).clear(app));
    let bounds = cx
        .debug_bounds(selector)
        .expect("tracking control must render");
    cx.simulate_click(bounds.center(), gpui::Modifiers::default());
}

#[gpui::test]
fn tracking_read_failure_disables_save_and_retry_restores_real_values(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    open_failed_read(&f, &w, cx);
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(
        cx.debug_bounds("tracking-load-error").is_some(),
        "load error must be visible"
    );
    let off = published_control(cx, "Off — don't track playback");
    assert_eq!(off.role(), Role::RadioButton);
    assert_eq!(off.toggled(), Some(Toggled::True));
    assert!(off.is_disabled());
    assert!(!off.supports_action(gpui::AccessibleAction::Click));
    assert!(!off.supports_action(gpui::AccessibleAction::Focus));
    click(cx, "tracking-save");
    w.read_with(cx, |w, _| {
        assert!(!w.settings.cfg_saving && w.settings.submitted.is_none());
        assert_eq!(w.active_writes, 0);
    });
    w.update_in(cx, |w, win, cx| w.save_tracking_settings(win, cx));
    assert!(
        f.runtime
            .block_on(f.core.try_get_tracking_config())
            .is_err()
    );
    click(cx, "settings-retry-tracking");
    f.wait(
        &w,
        cx,
        "retry still fails while storage is unreadable",
        |w| !w.settings.cfg_loading,
    );
    w.read_with(cx, |w, _| {
        assert!(!w.settings.cfg_loaded && !w.settings.cfg_error.is_empty());
        assert!(w.settings.submitted.is_none());
    });

    f.db.set_setting("tracking_prompt_seconds", "91").unwrap();
    assert_config(&f, &original());
    click(cx, "tracking-save");
    w.read_with(cx, |w, _| {
        assert!(!w.settings.cfg_saving && w.settings.submitted.is_none())
    });
    click(cx, "settings-retry-tracking");
    f.wait(&w, cx, "tracking retry recovers saved values", |w| {
        w.settings.cfg_loaded
    });
    w.read_with(cx, |w, app| {
        config_eq(&w.settings.cfg, &original());
        assert!(w.settings.cfg_error.is_empty() && !w.settings.saved);
        assert_eq!(w.settings.seconds.as_ref().unwrap().read(app).value(), "91");
        assert_eq!(w.settings.percent.as_ref().unwrap().read(app).value(), "77");
        assert_eq!(
            w.settings.socket.as_ref().unwrap().read(app).value(),
            "/controlled-only.sock"
        );
    });
    click(cx, "tracking-save");
    f.wait(&w, cx, "save becomes available after retry", |w| {
        w.settings.saved && w.active_writes == 0
    });
    assert_config(&f, &original());
}

#[gpui::test]
fn tracking_read_failure_does_not_create_a_submitted_close_barrier(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (w, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    open_failed_read(&f, &w, cx);
    let obs = Observers::new(&f, &w, cx);
    for intent in [CloseIntent::Quit, CloseIntent::Window] {
        w.update_in(cx, |w, win, cx| w.request_close(intent, win, cx));
        cx.run_until_parked();
        w.read_with(cx, |w, _| {
            assert!(w.close_ready);
            assert!(w.barrier.is_none() && w.barrier_error.is_empty());
            assert!(w.settings.tracking_save_error().is_none());
            assert!(!w.settings.cfg_loaded && !w.settings.cfg_error.is_empty());
        });
        w.update_in(cx, |w, win, cx| w.resume_after_close(win, cx));
    }
    assert_eq!(
        obs.ready(),
        vec![
            (CloseIntent::Quit, "auto".into()),
            (CloseIntent::Window, "auto".into())
        ]
    );
    f.db.set_setting("tracking_prompt_seconds", "91").unwrap();
    assert_config(&f, &original());
}

#[gpui::test]
fn tracking_read_during_close_restores_editable_fields(tc: &mut TestAppContext) {
    use gpui::EntityInputHandler;
    let f = Fixture::new(tc);
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    view.update_in(cx, |w, win, cx| {
        w.navigate(Route::Settings, win, cx);
        assert!(w.settings.cfg_loading);
        w.request_close(CloseIntent::Window, win, cx);
    });
    f.wait(&view, cx, "tracking read during close", |w| {
        w.close_ready && w.settings.cfg_loaded
    });
    view.update_in(cx, |w, win, cx| {
        w.resume_after_close(win, cx);
        for (field, value) in [
            (&w.settings.seconds, "95"),
            (&w.settings.percent, "75"),
            (&w.settings.socket, "/tmp/new.sock"),
        ] {
            field.as_ref().unwrap().update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..input.value().len()), value, win, cx);
                assert_eq!(input.value(), value);
            });
        }
    });
}

#[gpui::test]
fn account_restored_during_close_reloads_settings(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    f.load(&view, cx);
    view.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        w.set_profile(
            Some(kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        assert!(w.settings.socket.is_none());
        w.resume_after_close(win, cx);
        assert!(w.settings.socket.is_some());
    });
    f.wait(&view, cx, "restored settings", |w| {
        w.settings.cfg_loaded && w.settings.close_to_tray.loaded && w.settings.auto_update.loaded
    });
}

#[gpui::test]
fn quit_aborted_settings_reads_do_not_populate_errors(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    view.update_in(cx, |w, win, cx| {
        w.route = Route::Settings;
        w.enter_settings(win, cx);
        w.check_settings_update(win, cx);
        assert!(w.settings.cfg_loading);
        assert!(w.settings.close_to_tray.loading);
        assert!(w.settings.auto_update.loading);
        assert!(w.settings.update_checking);
        w.cancel_reads_for_quit();
    });
    for _ in 0..8 {
        cx.run_until_parked();
        f.runtime.block_on(async { tokio::task::yield_now().await });
        cx.run_until_parked();
    }
    view.read_with(cx, |w, _| {
        assert!(w.stopping);
        assert!(w.settings.cfg_error.is_empty());
        assert!(w.settings.close_to_tray.load_error.is_empty());
        assert!(w.settings.auto_update.load_error.is_empty());
        assert!(w.settings.update_error.is_empty());
    });
}

#[gpui::test]
fn fenced_settings_retry_and_check_buttons_admit_no_reads(tc: &mut TestAppContext) {
    let f = Fixture::new(tc);
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    open_failed_read(&f, &view, cx);
    view.update_in(cx, |w, win, cx| {
        w.settings.close_to_tray.loaded = false;
        w.settings.close_to_tray.load_error = "storage read failed".into();
        w.settings.auto_update.loaded = false;
        w.settings.auto_update.load_error = "storage read failed".into();
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        cx.notify();
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(published_control(cx, "Check for updates").is_disabled());
    click(cx, "settings-retry-tracking");
    view.update_in(cx, |w, win, cx| {
        w.load_tracking_settings(win, cx);
        w.load_app_toggle(SettingKey::CloseToTray, win, cx);
        w.load_app_toggle(SettingKey::AutoUpdate, win, cx);
        w.check_settings_update(win, cx);
    });
    view.read_with(cx, |w, _| {
        assert!(!w.settings.cfg_loading);
        assert!(!w.settings.close_to_tray.loading);
        assert!(!w.settings.auto_update.loading);
        assert!(!w.settings.update_checking);
    });
}

#[gpui::test]
fn fenced_and_stopping_settings_block_update_actions_and_resume_signed_out_entry(
    tc: &mut TestAppContext,
) {
    let f = Fixture::new(tc);
    let (view, cx) = tc.add_window_view(|win, cx| Workspace::new(f.core.clone(), win, cx));
    cx.simulate_resize(gpui::size(px(1100.), px(1500.)));
    f.wait(&view, cx, "initial restore", |w| !w.restoring_session);
    view.update_in(cx, |w, win, cx| {
        w.set_profile(
            Some(kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            }),
            win,
            cx,
        );
        w.navigate(Route::Settings, win, cx);
    });
    f.wait(&view, cx, "initial settings load", |w| {
        w.settings.cfg_loaded && w.settings.close_to_tray.loaded && w.settings.auto_update.loaded
    });
    view.update_in(cx, |w, win, cx| {
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        w.set_profile(None, win, cx);
        assert!(w.route_entry_deferred);
        assert!(!w.settings.cfg_loading && !w.settings.cfg_loaded);
        w.resume_after_close(win, cx);
        assert!(!w.route_entry_deferred);
    });
    f.wait(&view, cx, "signed out settings entry resumes", |w| {
        w.settings.cfg_loaded && w.settings.close_to_tray.loaded && w.settings.auto_update.loaded
    });

    let update_info = |can_install| kurisu_core::UpdateInfo {
        available: true,
        current: "1.0.0".into(),
        version: "9.9.9".into(),
        tag: "v9.9.9".into(),
        body: String::new(),
        html_url: "https://github.com/CateDesu/Kurisu/releases/tag/v9.9.9".into(),
        can_install,
        restart_pending: false,
    };

    view.update_in(cx, |w, win, cx| {
        w.settings.update = Some(update_info(true));
        w.request_close(CloseIntent::Window, win, cx);
        assert!(w.close_ready);
        cx.notify();
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(published_control(cx, "Install 9.9.9").is_disabled());

    view.update_in(cx, |w, _, cx| {
        w.settings.update = Some(update_info(false));
        cx.notify();
    });
    cx.update(|win, app| win.draw(app).clear(app));
    assert!(published_control(cx, "Download it from GitHub").is_disabled());

    view.update_in(cx, |w, win, cx| {
        w.resume_after_close(win, cx);
        w.settings.update = Some(update_info(true));
        w.cancel_reads_for_quit();
        w.install_settings_update(win, cx);
        assert!(w.barrier.is_none());
        assert!(!w.update_busy);
    });
}
