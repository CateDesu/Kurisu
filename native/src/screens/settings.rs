use gpui::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement, Role, Subscription, Toggled,
    Window, div, prelude::*, px, rgb,
};
use kurisu_core::models::{TrackingConfig, UpdateInfo};

use super::{BarrierAction, Workspace, joined};
use crate::{
    components::input::{TextInput, TextInputEvent},
    components::{button, page_heading},
    platform::NativePlatform,
    theme::{ACCENT, BASE, DIM, EDGE, ERROR, INK, PANEL_2, serif_font},
};
use kurisu_core::Platform;

#[derive(Clone, Copy)]
enum SettingKey {
    CloseToTray,
    AutoUpdate,
}

impl SettingKey {
    fn name(self) -> &'static str {
        match self {
            Self::CloseToTray => "close_to_tray",
            Self::AutoUpdate => "auto_update",
        }
    }
    fn read_group(self) -> &'static str {
        match self {
            Self::CloseToTray => "settings-close-to-tray",
            Self::AutoUpdate => "settings-auto-update",
        }
    }
}

struct Toggle {
    value: bool,
    loaded: bool,
    loading: bool,
    saving: bool,
    error: String,
    request: u64,
}

impl Toggle {
    fn new(value: bool) -> Self {
        Self {
            value,
            loaded: false,
            loading: false,
            saving: false,
            error: String::new(),
            request: 0,
        }
    }
}

pub(super) struct SettingsState {
    cfg: TrackingConfig,
    cfg_loaded: bool,
    cfg_loading: bool,
    cfg_saving: bool,
    cfg_error: String,
    save_error: String,
    saved: bool,
    revision: u64,
    cfg_request: u64,
    seconds: Option<Entity<TextInput>>,
    percent: Option<Entity<TextInput>>,
    socket: Option<Entity<TextInput>>,
    subscriptions: Vec<Subscription>,
    close_to_tray: Toggle,
    auto_update: Toggle,
    pub(super) update: Option<UpdateInfo>,
    update_checking: bool,
    update_error: String,
    update_status: String,
    update_request: u64,
}

impl Default for SettingsState {
    fn default() -> Self {
        Self {
            cfg: TrackingConfig::default(),
            cfg_loaded: false,
            cfg_loading: false,
            cfg_saving: false,
            cfg_error: String::new(),
            save_error: String::new(),
            saved: false,
            revision: 0,
            cfg_request: 0,
            seconds: None,
            percent: None,
            socket: None,
            subscriptions: Vec::new(),
            close_to_tray: Toggle::new(false),
            auto_update: Toggle::new(true),
            update: None,
            update_checking: false,
            update_error: String::new(),
            update_status: String::new(),
            update_request: 0,
        }
    }
}

impl SettingsState {
    fn toggle(&self, key: SettingKey) -> &Toggle {
        match key {
            SettingKey::CloseToTray => &self.close_to_tray,
            SettingKey::AutoUpdate => &self.auto_update,
        }
    }
    fn toggle_mut(&mut self, key: SettingKey) -> &mut Toggle {
        match key {
            SettingKey::CloseToTray => &mut self.close_to_tray,
            SettingKey::AutoUpdate => &mut self.auto_update,
        }
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.saved = false;
    }
}

impl Workspace {
    pub(super) fn enter_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.seconds.is_none() {
            let seconds = cx.new(|cx| {
                let mut input = TextInput::new("120", "120", window, cx);
                input.set_disabled(true, cx);
                input
            });
            let percent = cx.new(|cx| {
                let mut input = TextInput::new("80", "80", window, cx);
                input.set_disabled(true, cx);
                input
            });
            let socket = cx.new(|cx| {
                let mut input = TextInput::new(
                    "",
                    if cfg!(target_os = "windows") {
                        r"\\.\pipe\mpvsocket"
                    } else {
                        "/tmp/mpvsocket"
                    },
                    window,
                    cx,
                );
                input.set_accessible_label("MPV socket path", cx);
                input.set_disabled(true, cx);
                input
            });
            for input in [&seconds, &percent, &socket] {
                let subscription = cx.subscribe(input, |this, _, event, cx| {
                    if matches!(event, TextInputEvent::Changed(_)) {
                        this.settings.changed();
                        cx.notify();
                    }
                });
                self.settings.subscriptions.push(subscription);
            }
            self.settings.seconds = Some(seconds);
            self.settings.percent = Some(percent);
            self.settings.socket = Some(socket);
        }
        if !self.settings.cfg_loaded && !self.settings.cfg_loading {
            self.load_tracking_settings(window, cx);
        }
        for key in [SettingKey::CloseToTray, SettingKey::AutoUpdate] {
            let toggle = self.settings.toggle(key);
            if !toggle.loaded && !toggle.loading {
                self.load_app_toggle(key, window, cx);
            }
        }
    }

    pub(super) fn refresh_settings_covers(&mut self) {}

    fn load_tracking_settings(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.settings.cfg_loading {
            return;
        }
        self.settings.cfg_loading = true;
        self.settings.cfg_error.clear();
        self.settings.cfg_request = self.settings.cfg_request.wrapping_add(1);
        let request = self.settings.cfg_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_tracking_config().await });
        self.replace_reads("settings-tracking", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.settings.cfg_request != request
                {
                    return;
                }
                this.settings.cfg_loading = false;
                match result {
                    Ok(config) => {
                        this.settings.cfg = config;
                        this.settings.cfg_loaded = true;
                        this.settings.cfg_error.clear();
                        if let Some(input) = &this.settings.seconds {
                            input.update(cx, |input, cx| {
                                input.set_value(this.settings.cfg.prompt_seconds.to_string(), cx);
                                input.set_disabled(false, cx);
                            });
                        }
                        if let Some(input) = &this.settings.percent {
                            input.update(cx, |input, cx| {
                                input.set_value(this.settings.cfg.auto_percent.to_string(), cx);
                                input.set_disabled(false, cx);
                            });
                        }
                        if let Some(input) = &this.settings.socket {
                            input.update(cx, |input, cx| {
                                input.set_value(this.settings.cfg.mpv_ipc_socket.clone(), cx);
                                input.set_disabled(false, cx);
                            });
                        }
                    }
                    Err(error) => this.settings.cfg_error = error.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_app_toggle(&mut self, key: SettingKey, window: &Window, cx: &mut Context<Self>) {
        let toggle = self.settings.toggle_mut(key);
        if toggle.loading || toggle.loaded {
            return;
        }
        toggle.loading = true;
        toggle.error.clear();
        toggle.request = toggle.request.wrapping_add(1);
        let request = toggle.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_app_setting(key.name().into()).await });
        self.replace_reads(key.read_group(), vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.settings.toggle(key).request != request
                {
                    return;
                }
                let toggle = this.settings.toggle_mut(key);
                toggle.loading = false;
                match result {
                    Ok(value) => {
                        toggle.value = match key {
                            SettingKey::CloseToTray => value.as_deref() == Some("1"),
                            SettingKey::AutoUpdate => value.as_deref() != Some("0"),
                        };
                        toggle.loaded = true;
                    }
                    Err(error) => toggle.error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_app_setting(&mut self, key: SettingKey, window: &Window, cx: &mut Context<Self>) {
        let toggle = self.settings.toggle_mut(key);
        if !toggle.loaded || toggle.loading || toggle.saving {
            return;
        }
        let old = toggle.value;
        toggle.value = !old;
        toggle.saving = true;
        toggle.error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move {
                    worker
                        .set_app_setting(key.name().into(), if old { "0" } else { "1" }.into())
                        .await
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    let toggle = this.settings.toggle_mut(key);
                    toggle.saving = false;
                    if let Err(error) = result {
                        toggle.value = old;
                        toggle.error = error;
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn edit_tracking_mode(&mut self, mode: &'static str, cx: &mut Context<Self>) {
        if self.settings.cfg_loaded && !self.settings.cfg_saving {
            self.settings.cfg.mode = mode.into();
            self.settings.changed();
            cx.notify();
        }
    }

    fn toggle_tracking_option(&mut self, discord: bool, cx: &mut Context<Self>) {
        if self.settings.cfg_loaded && !self.settings.cfg_saving {
            if discord {
                self.settings.cfg.discord_enabled = !self.settings.cfg.discord_enabled;
            } else {
                self.settings.cfg.auto_ask = !self.settings.cfg.auto_ask;
            }
            self.settings.changed();
            cx.notify();
        }
    }

    fn save_tracking_settings(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.settings.cfg_loaded || self.settings.cfg_saving {
            return;
        }
        let config = self.settings.cfg.clone();
        let seconds = self
            .settings
            .seconds
            .as_ref()
            .map(|input| input.read(cx).value().to_owned())
            .unwrap_or_default();
        let percent = self
            .settings
            .percent
            .as_ref()
            .map(|input| input.read(cx).value().to_owned())
            .unwrap_or_default();
        let socket = self
            .settings
            .socket
            .as_ref()
            .map(|input| input.read(cx).value().to_owned())
            .unwrap_or_default();
        let prompt_seconds = bounded_number(&seconds, 120, 1, 3600);
        let auto_percent = bounded_number(&percent, 80, 1, 100);
        let mpv_ipc_socket = trim_utf8_bytes(&socket, 512);
        for (input, value) in [
            (self.settings.seconds.as_ref(), prompt_seconds.to_string()),
            (self.settings.percent.as_ref(), auto_percent.to_string()),
            (self.settings.socket.as_ref(), mpv_ipc_socket.clone()),
        ] {
            if let Some(input) = input {
                input.update(cx, |input, cx| input.set_value(value, cx));
            }
        }
        self.settings.cfg_saving = true;
        self.settings.save_error.clear();
        self.settings.saved = false;
        self.active_writes += 1;
        let revision = self.settings.revision;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move {
                    worker
                        .set_tracking_config(
                            config.mode,
                            prompt_seconds,
                            auto_percent,
                            config.auto_ask,
                            mpv_ipc_socket,
                            config.discord_enabled,
                        )
                        .await
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    this.settings.cfg_saving = false;
                    match result {
                        Ok(saved) => {
                            if this.settings.revision == revision {
                                this.settings.cfg = saved;
                                this.settings.saved = true;
                            }
                        }
                        Err(error) => this.settings.save_error = error,
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn check_settings_update(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.settings.update_checking || self.update_busy {
            return;
        }
        self.settings.update_checking = true;
        self.settings.update_error.clear();
        self.settings.update_status.clear();
        self.settings.update = None;
        self.settings.update_request = self.settings.update_request.wrapping_add(1);
        let request = self.settings.update_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.check_update().await });
        self.replace_reads("settings-update-check", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.settings.update_request != request
                {
                    return;
                }
                this.settings.update_checking = false;
                match result {
                    Ok(mut info) => {
                        if info.restart_pending {
                            this.note_restart_pending();
                        }
                        info.restart_pending |= this.restart_pending;
                        this.settings.update = Some(info);
                    }
                    Err(error) => this.settings.update_error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn install_settings_update(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(info) = self.settings.update.as_ref() else {
            return;
        };
        if self.update_busy
            || self.restart_pending
            || !info.available
            || !info.can_install
            || info.restart_pending
        {
            return;
        }
        self.show_update(info.clone());
        self.start_barrier(BarrierAction::Install, window, cx);
    }

    fn open_update_release(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(info) = self.settings.update.as_ref() else {
            return;
        };
        let url = info.html_url.clone();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = core
                .spawn(async move { NativePlatform.open_url(&url) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                    && let Err(error) = joined(result)
                {
                    this.settings.update_error = error;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_settings(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = &self.settings;
        let mut content = div()
            .id("settings")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(px(28.))
            .child(
                div()
                    .min_h(px(92.))
                    .mb(px(4.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(rgb(EDGE))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(ACCENT))
                            .child("PROGRAM"),
                    )
                    .child(page_heading("Settings"))
                    .child(div().text_color(rgb(DIM)).child("El Psy Congroo")),
            );

        let mut account = section("Account");
        if let Some(user) = &self.profile {
            account = account
                .child(
                    div()
                        .flex()
                        .child("Signed in as ")
                        .child(div().font_weight(FontWeight::BOLD).child(user.name.clone()))
                        .child("."),
                )
                .child(
                    button("settings-logout", "Log out", self.auth_busy)
                        .self_start()
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.auth_busy {
                                this.request_logout(window, cx);
                            }
                        })),
                );
        } else {
            account = account
                .child(div().text_color(rgb(DIM)).child("Not signed in."))
                .child(
                    button(
                        "settings-sign-in",
                        if self.auth_retry_available() {
                            "Open AniList again"
                        } else if self.auth_busy {
                            "Connecting…"
                        } else {
                            "Sign in with AniList"
                        },
                        self.auth_busy && !self.auth_retry_available(),
                    )
                    .debug_selector(|| "settings-sign-in".into())
                    .self_start()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.login(true, None, window, cx);
                    })),
                );
        }
        if self.auth_abort.is_some() {
            account = account.child(self.render_login_recovery(cx));
        }
        if !self.auth_error.is_empty() {
            account = account.child(
                div()
                    .id("settings-auth-error")
                    .role(Role::Alert)
                    .text_color(rgb(ERROR))
                    .child(self.auth_error.clone()),
            );
        }
        content = content.child(account);

        let disabled = !state.cfg_loaded || state.cfg_saving;
        let mut tracking = section("Playback tracking").child(
            div()
                .mb(px(12.))
                .text_color(rgb(DIM))
                .child("Detect playback in supported players and update your list."),
        );
        if !state.cfg_error.is_empty() {
            tracking = tracking.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!("Couldn't load settings: {}", state.cfg_error)),
            );
            tracking = tracking.child(
                button("settings-retry-tracking", "Retry", state.cfg_loading).on_click(
                    cx.listener(|this, _, window, cx| {
                        if !this.settings.cfg_loading {
                            this.load_tracking_settings(window, cx);
                        }
                    }),
                ),
            );
        }
        for (mode, label) in [
            ("off", "Off — don't track playback"),
            ("prompt", "Prompt — ask me after a while"),
            ("auto", "Auto — update silently at X% watched"),
        ] {
            tracking = tracking.child(
                radio_control(
                    format!("tracking-mode-{mode}"),
                    label,
                    state.cfg.mode == mode,
                    disabled,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.edit_tracking_mode(mode, cx))),
            );
        }
        if state.cfg.mode == "prompt"
            && let Some(input) = &state.seconds
        {
            tracking = tracking.child(
                input_row("Ask after", input.clone(), "seconds of playback").child(format!(
                    "({} min)",
                    bounded_number(input.read(cx).value(), 120, 1, 3600).div_ceil(60)
                )),
            );
        }
        if state.cfg.mode == "auto"
            && let Some(input) = &state.percent
        {
            tracking = tracking.child(input_row("Update progress at", input.clone(), "% watched"));
        }
        tracking =
            tracking
                .child(
                    toggle_button(
                        "tracking-auto-ask",
                        "Jump to the Currently Watching tab and ask to update after ~15s of playback",
                        state.cfg.auto_ask,
                        disabled,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_tracking_option(false, cx))),
                )
                .child(
                    div().ml(px(21.)).text_size(px(12.)).text_color(rgb(DIM)).child(
                        "On by default. When a show on your list is detected, Kurisu switches to the detection tab and prompts you — harder to miss than the banner alone.",
                    ),
                )
                .child(
                    toggle_button(
                        "tracking-discord",
                        "Discord Rich Presence",
                        state.cfg.discord_enabled,
                        disabled,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_tracking_option(true, cx))),
                );
        if let Some(input) = &state.socket {
            tracking = tracking.child(
                div()
                    .mt(px(12.))
                    .child("MPV socket path (optional)")
                    .child(
                        div()
                            .border_1()
                            .border_color(rgb(EDGE))
                            .p(px(5.))
                            .child(input.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(DIM))
                            .child("Leave blank to try common defaults. Set input-ipc-server in mpv.conf for bare MPV."),
                    ),
            );
        }
        tracking = tracking.child(
            div()
                .mt(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    button(
                        "tracking-save",
                        if state.cfg_saving {
                            "Saving…"
                        } else {
                            "Save tracking"
                        },
                        disabled,
                    )
                    .bg(rgb(ACCENT))
                    .text_color(rgb(BASE))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.save_tracking_settings(window, cx)),
                    ),
                )
                .when(state.saved, |row| {
                    row.child(div().text_color(rgb(ACCENT)).child("saved ✓"))
                }),
        );
        if !state.save_error.is_empty() {
            let error = format!("Save failed: {}", state.save_error);
            tracking = tracking.child(
                div()
                    .id("tracking-save-error")
                    .role(Role::Alert)
                    .aria_label(error.clone())
                    .text_color(rgb(ERROR))
                    .child(error),
            );
        }
        content = content.child(tracking);

        for (title, key, label, description) in [
            (
                "Window",
                SettingKey::CloseToTray,
                "Hide to system tray when closing the window",
                "Off by default. Quit is always available in the tray menu.",
            ),
            (
                "Updates",
                SettingKey::AutoUpdate,
                "Automatically check for updates",
                "On by default. Checks at startup and every hour. You choose when to install.",
            ),
        ] {
            let toggle = state.toggle(key);
            let blocked = !toggle.loaded || toggle.loading || toggle.saving;
            let mut row = section(title)
                .child(
                    toggle_button(key.name(), label, toggle.value, blocked).on_click(cx.listener(
                        move |this, _, window, cx| this.toggle_app_setting(key, window, cx),
                    )),
                )
                .child(
                    div()
                        .ml(px(21.))
                        .text_size(px(12.))
                        .text_color(rgb(DIM))
                        .child(description),
                );
            if !toggle.error.is_empty() {
                row = row.child(
                    div()
                        .text_color(rgb(ERROR))
                        .child(format!("{} setting: {}", title, toggle.error)),
                );
                if !toggle.loaded {
                    row = row.child(
                        button(format!("retry-{}", key.name()), "Retry", toggle.loading).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.load_app_toggle(key, window, cx)
                            }),
                        ),
                    );
                }
            }
            content = content.child(row);
        }

        let mut updates = div().mt(px(12.)).flex().flex_wrap().gap(px(8.)).child(
            button(
                "settings-check-update",
                if state.update_checking {
                    "Checking…"
                } else {
                    "Check for updates"
                },
                state.update_checking || self.update_busy,
            )
            .on_click(cx.listener(|this, _, window, cx| this.check_settings_update(window, cx))),
        );
        if let Some(info) = &state.update {
            if info.available && info.can_install && !self.restart_pending {
                updates = updates.child(
                    button(
                        "settings-install-update",
                        format!("Install {}", info.version),
                        self.update_busy,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.install_settings_update(window, cx)),
                    ),
                );
            }
            let summary = if self.restart_pending {
                "An update is installed. Restart Kurisu to finish.".to_string()
            } else if info.available {
                format!(
                    "Version {} is available. You're on {}.",
                    info.version, info.current
                )
            } else {
                format!("Up to date — you're on {}.", info.current)
            };
            updates = updates.child(div().w_full().text_color(rgb(ACCENT)).child(summary));
            if info.available && !info.can_install && !self.restart_pending {
                updates = updates.child(
                    button(
                        "settings-download-release",
                        "Download it from GitHub",
                        false,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_update_release(window, cx)),
                    ),
                );
            }
            if !info.body.is_empty() {
                updates = updates.child(
                    div()
                        .id("settings-release-notes")
                        .w_full()
                        .max_h(px(128.))
                        .overflow_y_scroll()
                        .p(px(8.))
                        .bg(rgb(PANEL_2))
                        .border_1()
                        .border_color(rgb(EDGE))
                        .child(info.body.clone()),
                );
            }
        } else if self.restart_pending {
            updates = updates.child(
                div()
                    .w_full()
                    .text_color(rgb(ACCENT))
                    .child("An update is installed. Restart Kurisu to finish."),
            );
        }
        if !state.update_error.is_empty() {
            updates = updates.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!("Update failed: {}", state.update_error)),
            );
        }
        if !state.update_status.is_empty() {
            updates = updates.child(
                div()
                    .text_color(rgb(ACCENT))
                    .child(state.update_status.clone()),
            );
        }
        content.child(updates).into_any()
    }
}

fn section(title: &'static str) -> gpui::Div {
    div()
        .mt(px(16.))
        .pt(px(16.))
        .border_t_1()
        .border_color(rgb(EDGE))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .id(title)
                .role(Role::Heading)
                .aria_level(2)
                .aria_label(title)
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .font(serif_font())
                        .text_size(px(20.))
                        .text_color(rgb(INK))
                        .child(title),
                )
                .child(div().flex_1().h(px(1.)).bg(rgb(EDGE))),
        )
}

fn radio_control(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    selected: bool,
    disabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let mut row = div()
        .id(id)
        .role(Role::RadioButton)
        .aria_label(label)
        .aria_toggled(Toggled::from(selected))
        .tab_stop(!disabled)
        .self_start()
        .flex()
        .items_center()
        .gap(px(8.))
        .min_h(px(22.))
        .text_color(rgb(if disabled { DIM } else { INK }))
        .child(
            div()
                .w(px(13.))
                .h(px(13.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_1()
                .border_color(rgb(if selected { ACCENT } else { EDGE }))
                .bg(rgb(if selected { ACCENT } else { PANEL_2 }))
                .when(selected, |circle| {
                    circle.child(div().w(px(5.)).h(px(5.)).rounded_full().bg(rgb(BASE)))
                }),
        )
        .child(label);
    if !disabled {
        row = row
            .focusable()
            .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
            .cursor_pointer();
    }
    row
}

fn input_row(label: &'static str, input: Entity<TextInput>, suffix: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(8.))
        .child(label)
        .child(
            div()
                .w(px(90.))
                .border_1()
                .border_color(rgb(EDGE))
                .p(px(4.))
                .child(input),
        )
        .child(suffix)
}

fn toggle_button(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    value: bool,
    disabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let mut row = div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(Toggled::from(value))
        .tab_stop(!disabled)
        .self_start()
        .flex()
        .items_center()
        .gap(px(8.))
        .min_h(px(22.))
        .text_color(rgb(if disabled { DIM } else { INK }))
        .child(
            div()
                .w(px(13.))
                .h(px(13.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(rgb(if value { ACCENT } else { EDGE }))
                .bg(rgb(if value { ACCENT } else { PANEL_2 }))
                .when(value, |box_| {
                    box_.child(div().text_size(px(11.)).text_color(rgb(BASE)).child("✓"))
                }),
        )
        .child(label);
    if !disabled {
        row = row
            .focusable()
            .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
            .cursor_pointer();
    }
    row
}

fn bounded_number(raw: &str, fallback: u64, min: u64, max: u64) -> u64 {
    let number = raw
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite());
    number
        .map(|number| number.round().clamp(min as f64, max as f64) as u64)
        .unwrap_or(fallback)
}

fn trim_utf8_bytes(raw: &str, max: usize) -> String {
    let trimmed = raw.trim();
    let mut end = trimmed.len().min(max);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_numeric_drafts_clamp_before_core_write() {
        assert_eq!(bounded_number("", 120, 1, 3600), 120);
        assert_eq!(bounded_number("0", 120, 1, 3600), 1);
        assert_eq!(bounded_number("3601", 120, 1, 3600), 3600);
        assert_eq!(bounded_number("75.6", 80, 1, 100), 76);
        assert_eq!(bounded_number("NaN", 80, 1, 100), 80);
    }

    #[test]
    fn socket_limit_never_splits_utf8() {
        let long = "あ".repeat(300);
        let safe = trim_utf8_bytes(&long, 512);
        assert!(safe.len() <= 512);
        assert!(safe.chars().all(|ch| ch == 'あ'));
    }
}
