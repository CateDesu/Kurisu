use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    App, Context, CursorStyle, Div, Entity, FocusHandle, Focusable, FontWeight, KeyBinding,
    MouseButton, Render, RenderImage, ResizeEdge, Role, Stateful, Subscription, Window, actions,
    div, img, prelude::*, px, rgb, svg,
};
use kurisu_core::{Core, Platform, Services, User};
use tray_icon::{
    Icon, MouseButton as TrayMouseButton, MouseButtonState, TrayIcon, TrayIconBuilder,
    TrayIconEvent, TrayIconId,
    menu::{Menu, MenuEvent, MenuId, MenuItem},
};

use crate::{
    components::tracked_mono,
    contracts::{CloseIntent, Route, WorkspaceEvent},
    platform::{InstanceGuard, NativePlatform},
    screens::{ModalLayer, Workspace},
    theme::{ACCENT, BASE, DIM, EDGE, INK, PANEL_2, mono_font, sans_font},
};

actions!(kurisu_shell, [TabForward, TabBackward, Quit, Escape]);

const RESIZE_BORDER: f32 = 12.;
const RESIZE_CORNER: f32 = 20.;

pub fn bind_keys(cx: &mut App) {
    crate::components::input::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("tab", TabForward, None),
        KeyBinding::new("shift-tab", TabBackward, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("escape", Escape, None),
    ]);
}

struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

#[derive(Default)]
struct TrayClickFilter {
    suppress_next_left_up: bool,
}

impl TrayClickFilter {
    fn accepts(&mut self, event: &TrayIconEvent, icon_id: &TrayIconId) -> bool {
        if event.id() != icon_id {
            return false;
        }
        match event {
            TrayIconEvent::DoubleClick {
                button: TrayMouseButton::Left,
                ..
            } => {
                self.suppress_next_left_up = true;
                false
            }
            TrayIconEvent::Click {
                button: TrayMouseButton::Left,
                button_state: MouseButtonState::Down,
                ..
            } => {
                self.suppress_next_left_up = false;
                false
            }
            TrayIconEvent::Click {
                button: TrayMouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => !std::mem::take(&mut self.suppress_next_left_up),
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TrayWindowAction {
    Hide,
    Show,
    RestoreMinimized,
}

fn tray_window_action(
    hidden_to_tray: bool,
    minimized_by_program: bool,
    platform_minimized: Option<bool>,
    closing: bool,
) -> TrayWindowAction {
    if hidden_to_tray || closing {
        TrayWindowAction::Show
    } else if platform_minimized == Some(true)
        || (platform_minimized.is_none() && minimized_by_program)
    {
        TrayWindowAction::RestoreMinimized
    } else {
        TrayWindowAction::Hide
    }
}

fn tray_icon() -> Result<Tray, String> {
    let menu = Menu::new();
    let show = MenuItem::new("Show Kurisu", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append(&show).map_err(|error| error.to_string())?;
    menu.append(&quit).map_err(|error| error.to_string())?;
    let mut pixels = vec![0u8; 32 * 32 * 4];
    for y in 0..32usize {
        for x in 0..32usize {
            let idx = (y * 32 + x) * 4;
            let copper = (4..28).contains(&x) && (4..28).contains(&y);
            let mark = ((10..=12).contains(&x) && (8..=24).contains(&y))
                || ((30..=33).contains(&(x + y)) && (12..=22).contains(&x) && y <= 18)
                || (x > y && x <= y + 4 && x >= 12 && y >= 15);
            if copper {
                pixels[idx..idx + 4].copy_from_slice(if mark {
                    &[0, 0, 0, 255]
                } else {
                    &[193, 139, 107, 255]
                });
            }
        }
    }
    let icon = Icon::from_rgba(pixels, 32, 32).map_err(|error| error.to_string())?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("Kurisu")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_icon(icon)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(Tray {
        _icon: tray,
        show_id: show.id().clone(),
        quit_id: quit.id().clone(),
    })
}

pub struct Shell {
    core: Core,
    workspace: Entity<Workspace>,
    route: Route,
    profile: Option<User>,
    profile_avatar: Option<Arc<RenderImage>>,
    offline: bool,
    modal_layer: Option<ModalLayer>,
    pre_modal_focus: Option<FocusHandle>,
    closing: bool,
    close_intent: Option<CloseIntent>,
    close_epoch: u64,
    hidden_to_tray: bool,
    minimized_by_program: bool,
    tray_error: Option<String>,
    tray_clicks: TrayClickFilter,
    restore_activation: Arc<AtomicBool>,
    services: Arc<Mutex<Option<Services>>>,
    tray: Option<Tray>,
    instance: Arc<InstanceGuard>,
    focus: FocusHandle,
    _subscription: Subscription,
    _modal_subscription: Subscription,
    _focus_lost_subscription: Subscription,
    _visibility_subscription: Subscription,
}

impl Shell {
    pub fn new(
        core: Core,
        services: Arc<Mutex<Option<Services>>>,
        instance: Arc<InstanceGuard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = cx.new(|cx| Workspace::new(core.clone(), window, cx));
        let route = workspace.read(cx).route().clone();
        let profile = workspace.read(cx).profile().cloned();
        let profile_avatar = workspace.read(cx).profile_avatar();
        let offline = workspace.read(cx).is_offline();
        let modal_layer = workspace.read(cx).active_modal();
        let subscription =
            cx.subscribe_in(&workspace, window, |this, workspace, event, window, cx| {
                this.on_workspace_event(workspace, event, window, cx);
            });
        let modal_subscription =
            cx.observe_in(&workspace, window, |this, workspace, window, cx| {
                let next = workspace.read(cx).active_modal();
                let avatar = workspace.read(cx).profile_avatar();
                let avatar_changed = match (&this.profile_avatar, &avatar) {
                    (Some(previous), Some(current)) => !Arc::ptr_eq(previous, current),
                    (None, None) => false,
                    _ => true,
                };
                if avatar_changed {
                    this.profile_avatar = avatar;
                    cx.notify();
                }
                if next == this.modal_layer {
                    return;
                }
                if this.modal_layer.is_none() && next.is_some() {
                    this.pre_modal_focus = window.focused(cx);
                }
                this.modal_layer = next;
                cx.notify();
                if next.is_some() {
                    workspace.read(cx).modal_focus().hold(window, cx);
                } else {
                    window.focus(&this.focus, cx);
                }
                cx.on_next_frame(window, move |this, window, cx| {
                    if this.modal_layer != next {
                        return;
                    }
                    cx.on_next_frame(window, move |this, window, cx| {
                        if this.modal_layer != next {
                            return;
                        }
                        if next.is_some() {
                            this.workspace.update(cx, |workspace, cx| {
                                workspace.focus_active_modal(window, cx)
                            });
                        } else {
                            let prior = this.pre_modal_focus.take();
                            if let Some(prior) =
                                prior.filter(|handle| this.focus.contains(handle, window))
                            {
                                window.focus(&prior, cx);
                            } else {
                                window.focus(&this.focus, cx);
                                window.focus_next(cx);
                            }
                        }
                    });
                });
            });
        let focus_lost_subscription = cx.on_focus_lost(window, |_, window, cx| {
            let target = window.focus_lost_restore_target(cx);
            cx.defer_in(window, move |this, window, cx| {
                if this.focus.contains_focused(window, cx) {
                    return;
                }
                if this.modal_layer.is_some() {
                    this.workspace.read(cx).modal_focus().hold(window, cx);
                } else {
                    window.focus(target.as_ref().unwrap_or(&this.focus), cx);
                }
            });
        });
        let restore_activation = Arc::new(AtomicBool::new(false));
        let visibility_flag = restore_activation.clone();
        let visibility_subscription =
            window.observe_window_visibility(move |visibility, window, _| {
                if visibility.is_visible() && visibility_flag.swap(false, Ordering::AcqRel) {
                    window.activate_window();
                }
            });
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let _ = weak.update(cx, |this, cx| {
                this.request_close(CloseIntent::Window, window, cx);
            });
            false
        });
        let tray = match tray_icon() {
            Ok(tray) => Some(tray),
            Err(error) => {
                log::warn!("Tray unavailable: {error}");
                None
            }
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut shell = Self {
            core,
            workspace,
            route,
            profile,
            profile_avatar,
            offline,
            modal_layer,
            pre_modal_focus: None,
            closing: false,
            close_intent: None,
            close_epoch: 0,
            hidden_to_tray: false,
            minimized_by_program: false,
            tray_error: None,
            tray_clicks: TrayClickFilter::default(),
            restore_activation,
            services,
            tray,
            instance,
            focus,
            _subscription: subscription,
            _modal_subscription: modal_subscription,
            _focus_lost_subscription: focus_lost_subscription,
            _visibility_subscription: visibility_subscription,
        };
        shell.watch_tray(window, cx);
        shell
    }

    fn watch_tray(&mut self, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
                let alive = this.update_in(cx, |this, window, cx| {
                    if this.instance.take_activation() {
                        this.show_from_tray(window, cx);
                    }
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        if let Some(tray) = &this.tray {
                            if event.id == tray.show_id {
                                this.show_from_tray(window, cx);
                            } else if event.id == tray.quit_id {
                                this.show_from_tray(window, cx);
                                this.request_close(CloseIntent::Quit, window, cx);
                            }
                        }
                    }
                    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                        let activate = this
                            .tray
                            .as_ref()
                            .is_some_and(|tray| this.tray_clicks.accepts(&event, tray._icon.id()));
                        if activate {
                            this.toggle_from_tray(window, cx);
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_workspace_event(
        &mut self,
        workspace: &Entity<Workspace>,
        event: &WorkspaceEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorkspaceEvent::StateChanged => {
                let view = workspace.read(cx);
                self.route = view.route().clone();
                self.profile = view.profile().cloned();
                self.offline = view.is_offline();
                cx.notify();
            }
            WorkspaceEvent::ReadyToClose(intent) => {
                if !self.closing {
                    return;
                }
                if *intent == CloseIntent::Quit
                    || self.close_intent == Some(CloseIntent::Quit)
                    || self.tray.is_none()
                {
                    self.stop_and_quit(cx);
                } else {
                    self.finish_window_close(window, cx);
                }
            }
            WorkspaceEvent::CloseCancelled => {
                self.closing = false;
                self.close_intent = None;
                self.close_epoch = self.close_epoch.wrapping_add(1);
                cx.notify();
            }
            WorkspaceEvent::UpdateInstallerStarted => {
                self.request_close(CloseIntent::Quit, window, cx);
            }
        }
    }

    fn finish_window_close(&mut self, window: &Window, cx: &mut Context<Self>) {
        let core = self.core.clone();
        let epoch = self.close_epoch;
        cx.spawn_in(window, async move |this, cx| {
            let preference = core
                .clone()
                .spawn(async move { core.get_app_setting("close_to_tray".into()).await })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.closing || this.close_epoch != epoch {
                    return;
                }
                if this.close_intent == Some(CloseIntent::Quit) {
                    this.stop_and_quit(cx);
                } else {
                    match preference {
                        Ok(value) if value.as_deref() == Some("1") && this.tray.is_some() => {
                            match this.hide_window_to_tray(window, cx) {
                                Ok(()) => {
                                    this.clear_close_guard(cx);
                                }
                                Err(error) => this.close_failed(error, cx),
                            }
                        }
                        Ok(_) => this.stop_and_quit(cx),
                        Err(error) => {
                            this.close_failed(format!("Could not read window setting: {error}"), cx)
                        }
                    }
                }
            });
        })
        .detach();
    }

    fn clear_close_guard(&mut self, cx: &mut Context<Self>) {
        self.closing = false;
        self.close_intent = None;
        self.close_epoch = self.close_epoch.wrapping_add(1);
        cx.notify();
    }

    fn close_failed(&mut self, error: String, cx: &mut Context<Self>) {
        log::error!("Window close failed: {error}");
        self.tray_error = Some(error);
        self.clear_close_guard(cx);
    }

    fn tray_failed(&mut self, action: &str, error: String, cx: &mut Context<Self>) {
        let message = format!("Could not {action} Kurisu: {error}");
        log::error!("{message}");
        self.tray_error = Some(message);
        cx.notify();
    }

    fn hide_window_to_tray(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        hide_to_tray(window, cx)?;
        self.hidden_to_tray = true;
        self.minimized_by_program = false;
        self.restore_activation.store(false, Ordering::Release);
        self.tray_error = None;
        cx.notify();
        Ok(())
    }

    fn toggle_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match tray_window_action(
            self.hidden_to_tray,
            self.minimized_by_program,
            window.is_minimized(),
            self.closing,
        ) {
            TrayWindowAction::Hide => match self.hide_window_to_tray(window, cx) {
                Ok(()) => true,
                Err(error) => {
                    self.tray_failed("hide", error, cx);
                    false
                }
            },
            TrayWindowAction::Show | TrayWindowAction::RestoreMinimized => {
                self.show_from_tray(window, cx)
            }
        }
    }

    fn show_from_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let platform_minimized = window.is_minimized();
        let needs_show = self.hidden_to_tray
            || platform_minimized == Some(true)
            || (platform_minimized.is_none() && self.minimized_by_program);
        #[cfg(target_os = "linux")]
        if needs_show
            && !self.hidden_to_tray
            && self.minimized_by_program
            && platform_minimized.is_none()
        {
            if let Err(error) = hide_to_tray(window, cx) {
                self.tray_failed("restore", error, cx);
                return false;
            }
            self.hidden_to_tray = true;
        }
        if needs_show {
            self.restore_activation.store(true, Ordering::Release);
            if let Err(error) = show_from_tray(window, cx) {
                self.restore_activation.store(false, Ordering::Release);
                self.tray_failed("show", error, cx);
                return false;
            }
            self.hidden_to_tray = false;
        }
        self.minimized_by_program = false;
        self.tray_error = None;
        cx.activate(true);
        window.activate_window();
        cx.notify();
        true
    }

    fn request_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing
            && (intent != CloseIntent::Quit || self.close_intent == Some(CloseIntent::Quit))
        {
            return;
        }
        self.closing = true;
        self.close_intent = Some(intent);
        self.close_epoch = self.close_epoch.wrapping_add(1);
        self.workspace.update(cx, |workspace, cx| {
            workspace.request_close(intent, window, cx)
        });
        cx.notify();
    }

    fn minimize_window(&mut self, window: &mut Window) {
        // A queued launch must not override this later Minimize click.
        self.instance.take_activation();
        self.minimized_by_program = true;
        window.minimize_window();
    }

    fn stop_and_quit(&mut self, cx: &mut Context<Self>) {
        let services = self.services.lock().unwrap().take();
        cx.spawn(async move |_, cx| {
            if let Some(services) = services {
                services.shutdown().await;
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_layer.is_some() {
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| workspace.navigate(route, window, cx));
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_layer.is_some() {
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| workspace.back(window, cx));
    }

    fn logout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_layer.is_some() {
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| workspace.request_logout(window, cx));
    }

    fn open_profile(&self, cx: &mut Context<Self>) {
        if self.modal_layer.is_some() {
            return;
        }
        if let Some(user) = &self.profile {
            let encoded = user
                .name
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>();
            let url = format!("https://anilist.co/user/{encoded}");
            let core = self.core.clone();
            core.spawn(async move {
                let _ = NativePlatform.open_url(&url);
            });
            cx.notify();
        }
    }

    fn advance_focus(&self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_layer.is_some() {
            let focus = self.workspace.read(cx).modal_focus();
            focus.advance(forward, window, cx);
        } else if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
    }

    fn title_button(
        id: &'static str,
        label: &'static str,
        icon: &'static str,
        tab_stop: bool,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .role(Role::Button)
            .aria_label(label)
            .tab_stop(tab_stop)
            .when(tab_stop, |button| {
                button
                    .focusable()
                    .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
            })
            .w(px(42.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(DIM))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(EDGE)).text_color(rgb(INK)))
            .child(
                svg()
                    .path(format!("icons/{icon}.svg"))
                    .size(px(28.))
                    .text_color(rgb(DIM)),
            )
    }

    fn titlebar(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let maximized = window.is_maximized();
        div()
            .h(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .bg(rgb(0x020202))
            .border_b_1()
            .border_color(rgb(EDGE))
            .child(
                div()
                    .id("title-drag")
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .pl(px(20.))
                    .font(mono_font())
                    .text_size(px(12.))
                    .text_color(rgb(DIM))
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(div().text_color(rgb(ACCENT)).child("＋"))
                    .child(tracked_mono("KURISU", 12., 12. * 0.17))
                    .child(tracked_mono(" / Anime Tracker", 12., 12. * 0.04)),
            )
            .child(
                div()
                    .h_full()
                    .flex()
                    .child(
                        Self::title_button(
                            "minimize",
                            "Minimize",
                            "minimize",
                            self.modal_layer.is_none(),
                        )
                        .on_click(cx.listener(|this, _, window, _| this.minimize_window(window))),
                    )
                    .child(
                        Self::title_button(
                            "maximize",
                            if maximized { "Restore" } else { "Maximize" },
                            if maximized { "restore" } else { "maximize" },
                            self.modal_layer.is_none(),
                        )
                        .on_click(|_, window, _| window.zoom_window()),
                    )
                    .child(
                        Self::title_button("close", "Close", "x", self.modal_layer.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.request_close(CloseIntent::Window, window, cx)
                            })),
                    ),
            )
    }

    fn nav_item(
        route: Route,
        active: bool,
        icon: &'static str,
        height: f32,
        compact: bool,
        modal_open: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let label = route.label();
        div()
            .id(format!("nav-{label}"))
            .role(Role::Button)
            .aria_label(label)
            .aria_selected(active)
            .focusable()
            .tab_stop(!modal_open)
            .focus_visible(|style| style.border_color(rgb(ACCENT)))
            .flex()
            .items_center()
            .relative()
            .h(px(height))
            .gap(px(if compact { 5. } else { 8. }))
            .px(px(if compact { 5. } else { 9. }))
            .mb(px(2.))
            .border_1()
            .border_color(rgb(if active { 0x82543d } else { 0x040303 }))
            .bg(rgb(if active { 0x160d08 } else { 0x040303 }))
            .text_color(rgb(if active { INK } else { DIM }))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(PANEL_2)).text_color(rgb(INK)))
            .when(active, |item| {
                item.child(
                    div()
                        .absolute()
                        .left(px(-1.))
                        .top(px(7.))
                        .bottom(px(7.))
                        .w(px(2.))
                        .bg(rgb(ACCENT)),
                )
            })
            .child(
                div()
                    .w(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .child(
                        svg()
                            .path(format!("icons/{icon}.svg"))
                            .w(px(18.))
                            .h(px(18.))
                            .text_color(rgb(if active { ACCENT } else { DIM })),
                    ),
            )
            .child(div().min_w_0().text_size(px(15.)).child(label))
            .on_click(
                cx.listener(move |this, _, window, cx| this.navigate(route.clone(), window, cx)),
            )
    }

    fn sidebar(&self, width: f32, window: &Window, cx: &mut Context<Self>) -> Div {
        let viewport_height = f32::from(window.viewport_size().height);
        let dense_nav = viewport_height < 800.;
        let compact_brand = viewport_height < 700.;
        let modal_open = self.modal_layer.is_some();
        let nav = [
            (Route::MyList, "list"),
            (Route::Now, "play"),
            (Route::Library, "folder"),
            (Route::Torrents, "download"),
            (Route::Seasons, "sun"),
            (Route::Calendar, "calendar"),
            (Route::Search, "search"),
            (Route::Stats, "chart"),
            (Route::Notifications, "bell"),
            (Route::Settings, "sliders"),
        ];
        let mut links = div()
            .id("sidebar-nav")
            .role(Role::Navigation)
            .aria_label("Main navigation")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(12.))
            .pt(px(7.));
        for (index, (route, icon)) in nav.into_iter().enumerate() {
            if index == 0 || index == 4 || index == 9 {
                links = links.child(
                    div()
                        .px(px(12.))
                        .pt(px(if compact_brand {
                            6.
                        } else if dense_nav {
                            9.
                        } else {
                            18.
                        }))
                        .pb(px(if compact_brand {
                            3.
                        } else if dense_nav {
                            5.
                        } else {
                            9.
                        }))
                        .font(mono_font())
                        .text_size(px(11.))
                        .text_color(rgb(DIM))
                        .child(tracked_mono(
                            match index {
                                0 => "YOUR SHELF",
                                4 => "DISCOVER",
                                _ => "PROGRAM",
                            },
                            11.,
                            11. * 0.17,
                        )),
                );
            }
            links = links.child(Self::nav_item(
                route.clone(),
                self.route == route,
                icon,
                if compact_brand {
                    26.
                } else if dense_nav {
                    34.
                } else {
                    40.
                },
                width < 244.,
                modal_open,
                cx,
            ));
        }
        let profile = if let Some(user) = &self.profile {
            let avatar = self.profile_avatar.clone();
            div()
                .id("sidebar-profile")
                .flex()
                .items_center()
                .gap(px(12.))
                .px(px(16.))
                .py(px(16.))
                .border_t_1()
                .border_color(rgb(EDGE))
                .child(
                    div()
                        .w(px(36.))
                        .h(px(36.))
                        .flex_shrink_0()
                        .bg(rgb(PANEL_2))
                        .border_1()
                        .border_color(rgb(EDGE))
                        .overflow_hidden()
                        .when_some(avatar, |frame, image| {
                            frame.child(img(image).size_full().object_fit(gpui::ObjectFit::Cover))
                        }),
                )
                .child(
                    div()
                        .id("profile-link")
                        .role(Role::Button)
                        .aria_label(format!("Open {}'s AniList profile", user.name))
                        .focusable()
                        .tab_stop(!modal_open)
                        .cursor_pointer()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_color(rgb(INK))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(user.name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(rgb(DIM))
                                .child("AniList profile ↗"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.open_profile(cx))),
                )
                .child(
                    div()
                        .id("logout")
                        .role(Role::Button)
                        .aria_label("Log out")
                        .focusable()
                        .tab_stop(!modal_open)
                        .cursor_pointer()
                        .px(px(4.))
                        .child(
                            svg()
                                .path("icons/logout.svg")
                                .w(px(18.))
                                .h(px(18.))
                                .text_color(rgb(DIM)),
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.logout(window, cx))),
                )
        } else {
            let status = if self.workspace.read(cx).is_restoring_session() {
                "Loading account…"
            } else {
                "Not signed in"
            };
            div()
                .id("sidebar-session-status")
                .role(Role::Status)
                .aria_label(status)
                .px(px(16.))
                .py(px(20.))
                .border_t_1()
                .border_color(rgb(EDGE))
                .text_color(rgb(DIM))
                .child(status)
        };
        div()
            .relative()
            .w(px(width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(0x040303))
            .border_r_1()
            .border_color(rgb(EDGE))
            .child(
                div()
                    .relative()
                    .h(px(if compact_brand { 92. } else { 133. }))
                    .flex_shrink_0()
                    .px(px(if width < 244. { 17. } else { 22. }))
                    .pt(px(if compact_brand { 12. } else { 25. }))
                    .pb(px(if compact_brand { 12. } else { 23. }))
                    .border_b_1()
                    .border_color(rgb(EDGE))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .font(mono_font())
                            .text_size(px(14.))
                            .line_height(px(21.))
                            .text_color(rgb(DIM))
                            .child(tracked_mono("アニメの記録", 14., 14. * 0.14))
                            .child(tracked_mono("クリス", 14., 14. * 0.14).text_color(rgb(ACCENT))),
                    )
                    .child(
                        div()
                            .id("wordmark")
                            .relative()
                            .role(Role::Label)
                            .aria_label("Kurisu")
                            .a11y_synthetic_children(|builder| {
                                builder.parent_node().set_value("Kurisu");
                            })
                            .mt(px(if compact_brand { 8. } else { 15. }))
                            .w(px(if compact_brand { 112.632 } else { 147.252 }))
                            .h(px(if compact_brand { 38.2 } else { 47.7 }))
                            .child(
                                img(if compact_brand {
                                    "icons/wordmark-compact.svg"
                                } else {
                                    "icons/wordmark.svg"
                                })
                                .absolute()
                                .left(px(-2.))
                                .top(px(-2.))
                                .w(px(if compact_brand { 117. } else { 152. }))
                                .h(px(if compact_brand {
                                    43.
                                } else {
                                    52.
                                })),
                            ),
                    )
                    .child(
                        div()
                            .id("back")
                            .absolute()
                            .right(px(14.))
                            .bottom(px(15.))
                            .w(px(28.))
                            .h(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .role(Role::Button)
                            .aria_label("Back")
                            .focusable()
                            .tab_stop(!modal_open)
                            .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
                            .cursor_pointer()
                            .child(
                                svg()
                                    .path("icons/back.svg")
                                    .w(px(18.))
                                    .h(px(18.))
                                    .text_color(rgb(DIM)),
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                    ),
            )
            .child(links)
            .when(
                f32::from(window.viewport_size().height) >= 940.,
                |sidebar| {
                    sidebar.child(
                        div()
                            .relative()
                            .h(px(130.))
                            .min_h_0()
                            .flex_shrink_1()
                            .mx(px(18.))
                            .mb(px(14.))
                            .overflow_hidden()
                            .opacity(0.85)
                            .child(
                                svg()
                                    .path("icons/gearwork.svg")
                                    .absolute()
                                    .top(px(-8.))
                                    .left(px(-5.))
                                    .w(px(210.))
                                    .h(px(124.))
                                    .text_color(rgb(ACCENT)),
                            ),
                    )
                },
            )
            .child(profile)
            .when(modal_open, |sidebar| {
                sidebar.child(
                    div()
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_scroll(|_, _, cx| cx.stop_propagation()),
                )
            })
    }
}

fn hide_to_tray(window: &mut Window, cx: &mut Context<Shell>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        cx.hide();
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cx;
        window.set_visible(false).map_err(|error| error.to_string())
    }
}

fn show_from_tray(window: &mut Window, cx: &mut Context<Shell>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let _ = (window, cx);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cx;
        window.set_visible(true).map_err(|error| error.to_string())
    }
}

fn window_resize_handles() -> impl Iterator<Item = Stateful<Div>> {
    [
        ResizeEdge::Top,
        ResizeEdge::Bottom,
        ResizeEdge::Left,
        ResizeEdge::Right,
        ResizeEdge::TopLeft,
        ResizeEdge::TopRight,
        ResizeEdge::BottomLeft,
        ResizeEdge::BottomRight,
    ]
    .into_iter()
    .map(|edge| {
        let handle = div().absolute().occlude();
        let handle = match edge {
            ResizeEdge::Top => handle
                .top_0()
                .left(px(RESIZE_CORNER))
                .right(px(30.))
                .h(px(RESIZE_BORDER)),
            ResizeEdge::Bottom => handle
                .bottom_0()
                .left(px(RESIZE_CORNER))
                .right(px(RESIZE_CORNER))
                .h(px(RESIZE_BORDER)),
            ResizeEdge::Left => handle
                .left_0()
                .top(px(RESIZE_CORNER))
                .bottom(px(RESIZE_CORNER))
                .w(px(RESIZE_BORDER)),
            ResizeEdge::Right => handle
                .right_0()
                .top(px(RESIZE_CORNER))
                .bottom(px(RESIZE_CORNER))
                .w(px(RESIZE_BORDER)),
            ResizeEdge::TopLeft => handle.top_0().left_0().size(px(RESIZE_CORNER)),
            ResizeEdge::TopRight => handle
                .top_0()
                .right_0()
                .w(px(RESIZE_BORDER))
                .h(px(RESIZE_CORNER)),
            ResizeEdge::BottomLeft => handle.bottom_0().left_0().size(px(RESIZE_CORNER)),
            ResizeEdge::BottomRight => handle.bottom_0().right_0().size(px(RESIZE_CORNER)),
        };
        handle
            .id(format!("window-resize-{edge:?}"))
            .cursor(match edge {
                ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
                ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
                ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
            })
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            })
    })
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = if f32::from(window.viewport_size().width) <= 960. {
            216.
        } else {
            244.
        };
        div()
            .id("kurisu-shell")
            .relative()
            .role(Role::Application)
            .aria_label("Kurisu")
            .track_focus(&self.focus)
            .size_full()
            .border_1()
            .border_color(rgb(EDGE))
            .flex()
            .flex_col()
            .bg(rgb(BASE))
            .text_color(rgb(INK))
            .font(sans_font())
            .text_size(px(14.))
            .on_action(
                cx.listener(|this, _: &TabForward, window, cx| {
                    this.advance_focus(true, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &TabBackward, window, cx| {
                this.advance_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Quit, window, cx| {
                this.request_close(CloseIntent::Quit, window, cx)
            }))
            .child(self.titlebar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.sidebar(width, window, cx))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .child(self.workspace.clone())
                            .when(
                                self.modal_layer.is_none()
                                    && !matches!(
                                        self.route,
                                        Route::MyList
                                            | Route::Now
                                            | Route::Library
                                            | Route::Stats
                                            | Route::Search
                                            | Route::Seasons
                                            | Route::Calendar
                                            | Route::Anime(_)
                                    ),
                                |area| {
                                    area.child(
                                        div()
                                            .absolute()
                                            .right(px(28.))
                                            .top(px(20.))
                                            .w(px(145.))
                                            .h(px(85.))
                                            .opacity(0.85)
                                            .child(
                                                svg()
                                                    .path("icons/gearwork.svg")
                                                    .absolute()
                                                    .size_full()
                                                    .text_color(rgb(ACCENT)),
                                            ),
                                    )
                                },
                            ),
                    )
                    .child(div().w(px(RESIZE_BORDER)).h_full().flex_shrink_0()),
            )
            .when_some(self.tray_error.clone(), |root, error| {
                root.child(
                    div()
                        .id("tray-error")
                        .role(Role::Alert)
                        .absolute()
                        .top(px(40.))
                        .right(px(20.))
                        .max_w(px(420.))
                        .p(px(10.))
                        .border_1()
                        .border_color(rgb(crate::theme::ERROR))
                        .bg(rgb(PANEL_2))
                        .text_color(rgb(crate::theme::ERROR))
                        .child(error),
                )
            })
            .when(!window.is_maximized() && !window.is_fullscreen(), |root| {
                root.children(window_resize_handles())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn platform_close_reaches_the_workspace_barrier(cx: &mut gpui::TestAppContext) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("kurisu-close-{nonce}"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::open(
            path.clone(),
            runtime.handle().clone(),
            Arc::new(NativePlatform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let instance = Arc::new(InstanceGuard::acquire(&path).unwrap());
        let (shell, cx) = cx.add_window_view(|window, cx| {
            Shell::new(core, Arc::new(Mutex::new(None)), instance, window, cx)
        });

        assert!(!cx.simulate_close(), "the save barrier owns window closure");
        shell.read_with(cx, |shell, _| {
            assert!(shell.closing, "the platform close request was dropped");
            assert_eq!(shell.close_intent, Some(CloseIntent::Window));
            assert_eq!(shell.close_epoch, 1);
        });
        assert!(!cx.simulate_close());
        shell.read_with(cx, |shell, _| assert_eq!(shell.close_epoch, 1));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn wordmark_artwork_renders_without_clipping() {
        let renderer = gpui::SvgRenderer::new(Arc::new(()));
        for bytes in [
            include_bytes!("../assets/wordmark.svg").as_slice(),
            include_bytes!("../assets/wordmark-compact.svg").as_slice(),
        ] {
            let image = renderer.render_single_frame(bytes, 1.).unwrap();
            let width = image.size(0).width.0 as usize;
            let height = image.size(0).height.0 as usize;
            let pixels = image.as_bytes(0).unwrap();
            let alpha = |x: usize, y: usize| pixels[(y * width + x) * 4 + 3];
            assert!((0..width).all(|x| alpha(x, 0) == 0 && alpha(x, height - 1) == 0));
            assert!((0..height).all(|y| alpha(0, y) == 0 && alpha(width - 1, y) == 0));
            assert!(
                pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[3] > 200)
                    .count()
                    > 1000
            );
        }
    }

    #[test]
    fn heading_art_retains_readable_strokes_at_page_size() {
        let renderer = gpui::SvgRenderer::new(Arc::new(()));
        let count_strong_pixels = |bytes: &[u8]| {
            renderer
                .render_single_frame(bytes, 145. / 340.)
                .expect("gearwork SVG must render")
                .as_bytes(0)
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] > 160)
                .count()
        };
        let heading = count_strong_pixels(include_bytes!("../assets/gearwork.svg"));
        let sidebar = count_strong_pixels(include_bytes!("../assets/gearwork-heading.svg"));
        assert!(
            heading > sidebar * 2,
            "heading={heading}, sidebar={sidebar}"
        );
    }

    fn click(
        id: &TrayIconId,
        button: TrayMouseButton,
        button_state: MouseButtonState,
    ) -> TrayIconEvent {
        TrayIconEvent::Click {
            id: id.clone(),
            position: tray_icon::dpi::PhysicalPosition::new(0., 0.),
            rect: tray_icon::Rect::default(),
            button,
            button_state,
        }
    }

    #[test]
    fn tray_left_up_is_one_activation_per_click_sequence() {
        let owned = TrayIconId::new("owned");
        let other = TrayIconId::new("other");
        let mut filter = TrayClickFilter::default();
        assert!(!filter.accepts(
            &click(&other, TrayMouseButton::Left, MouseButtonState::Up),
            &owned
        ));
        assert!(!filter.accepts(
            &click(&owned, TrayMouseButton::Right, MouseButtonState::Up),
            &owned
        ));
        assert!(filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Up),
            &owned
        ));
        assert!(!filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Down),
            &owned
        ));
        assert!(filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Up),
            &owned
        ));
        assert!(!filter.accepts(
            &TrayIconEvent::DoubleClick {
                id: owned.clone(),
                position: tray_icon::dpi::PhysicalPosition::new(0., 0.),
                rect: tray_icon::Rect::default(),
                button: TrayMouseButton::Left,
            },
            &owned
        ));
        assert!(!filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Up),
            &owned
        ));
        assert!(!filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Down),
            &owned
        ));
        assert!(filter.accepts(
            &click(&owned, TrayMouseButton::Left, MouseButtonState::Up),
            &owned
        ));
    }

    #[test]
    fn tray_visibility_decision_uses_exact_minimized_state() {
        assert_eq!(
            tray_window_action(false, false, Some(false), false),
            TrayWindowAction::Hide
        );
        assert_eq!(
            tray_window_action(true, false, Some(false), false),
            TrayWindowAction::Show
        );
        assert_eq!(
            tray_window_action(false, false, Some(true), false),
            TrayWindowAction::RestoreMinimized
        );
        assert_eq!(
            tray_window_action(false, true, None, false),
            TrayWindowAction::RestoreMinimized
        );
        assert_eq!(
            tray_window_action(false, true, Some(false), false),
            TrayWindowAction::Hide
        );
        assert_eq!(
            tray_window_action(false, false, None, true),
            TrayWindowAction::Show
        );
    }
}
