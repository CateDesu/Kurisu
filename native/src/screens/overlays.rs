use gpui::{AnyElement, Context, InteractiveElement, Window, div, prelude::*, px, rgb};
use kurisu_core::Platform;

use super::state::status_label;
use super::{BarrierAction, ModalFocus, ModalLayer, Workspace};
use crate::components::{button, dialog, eyebrow, heading};
use crate::theme::{ACCENT, BASE, DIM, EDGE, ERROR, PANEL, PANEL_2};

impl Workspace {
    fn escape_active_modal(&mut self, cx: &mut Context<Self>) {
        match self.active_modal() {
            Some(ModalLayer::Barrier | ModalLayer::BarrierError) => self.cancel_barrier(cx),
            Some(ModalLayer::UpdateFailed) => {
                self.update_failed.clear();
                cx.notify();
            }
            Some(ModalLayer::Update) if !self.update_busy => {
                if let Some(update) = self.update.take() {
                    self.dismissed_update = update.tag;
                }
                cx.notify();
            }
            Some(ModalLayer::Prompt) if !self.prompt_busy => self.dismiss_prompt(cx),
            Some(ModalLayer::Logout) => {
                self.logout_confirming = false;
                cx.notify();
            }
            Some(ModalLayer::Editor) if self.editor_status_open => {
                self.editor_status_open = false;
                cx.notify();
            }
            Some(ModalLayer::Editor) => self.close_editor(cx),
            Some(
                ModalLayer::LibraryRemove | ModalLayer::LibraryUnlink | ModalLayer::LibraryBind,
            ) => self.dismiss_library_dialog(cx),
            _ => {}
        }
    }

    pub(super) fn render_now_playing(&self) -> AnyElement {
        let Some(now) = self.now_playing.as_ref() else {
            return div().into_any();
        };
        let label = now.matched.as_deref().unwrap_or(&now.title);
        let progress = if now.length_us > 0 {
            ((now.position_us as f32 / now.length_us as f32) * 100.).clamp(0., 100.)
        } else {
            0.
        };
        div()
            .border_b_1()
            .border_color(rgb(EDGE))
            .bg(rgb(PANEL))
            .px(px(16.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(8.))
            .child(div().text_color(rgb(ACCENT)).child("▶"))
            .child(div().flex_1().min_w_0().child(format!(
                    "{}{}",
                    label,
                    now.episode
                        .map_or_else(String::new, |episode| format!(" · Ep {episode}"))
                )))
            .child(
                div().w(px(90.)).h(px(3.)).bg(rgb(EDGE)).child(
                    div()
                        .w(gpui::relative(progress / 100.))
                        .h_full()
                        .bg(rgb(ACCENT)),
                ),
            )
            .child(
                div()
                    .text_color(rgb(DIM))
                    .text_size(px(11.))
                    .child(format!("{progress:.0}% · {}", now.player)),
            )
            .into_any()
    }

    pub(super) fn render_login(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut sheet = div()
            .w(px(460.))
            .max_w_full()
            .border_1()
            .border_color(rgb(EDGE))
            .border_t_2()
            .border_color(rgb(ACCENT))
            .bg(rgb(PANEL))
            .p(px(35.))
            .flex()
            .flex_col()
            .gap(px(13.))
            .child(eyebrow("Your own little anime archive"))
            .child(
                heading("Welcome to Kurisu.")
                    .id("login-heading")
                    .role(gpui::Role::Heading)
                    .aria_level(1)
                    .aria_label("Welcome to Kurisu."),
            )
            .child(
                div()
                    .text_color(rgb(DIM))
                    .child("Connect your AniList account to start tracking."),
            );
        if !self.auth_error.is_empty() {
            sheet = sheet.child(
                div()
                    .id("login-auth-error")
                    .role(gpui::Role::Alert)
                    .aria_label(self.auth_error.clone())
                    .text_color(rgb(ERROR))
                    .child(self.auth_error.clone()),
            );
        }
        sheet = sheet
            .child(
                button(
                    "login-oauth",
                    if self.auth_retry_available() {
                        "Open AniList again"
                    } else if self.auth_busy {
                        "Connecting…"
                    } else {
                        "Connect via AniList"
                    },
                    self.auth_busy && !self.auth_retry_available(),
                )
                .debug_selector(|| "login-oauth".into())
                .bg(rgb(ACCENT))
                .text_color(rgb(BASE))
                .on_click(cx.listener(|this, _, window, cx| this.login(true, None, window, cx))),
            )
            .when(self.auth_abort.is_some(), |sheet| {
                sheet.child(self.render_login_recovery(cx))
            })
            .child(
                button("login-token-toggle", "Paste a token instead", false).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.token_open = !this.token_open;
                        cx.notify();
                    },
                )),
            );
        if self.token_open {
            let token_row = div()
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .border_1()
                        .border_color(rgb(EDGE))
                        .bg(rgb(PANEL_2))
                        .p(px(5.))
                        .child(self.token.clone()),
                )
                .child(
                    button(
                        "login-token-use",
                        "Use",
                        self.auth_busy && !self.auth_retry_available(),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.login_with_token(window, cx))),
                );
            sheet =
                sheet
                    .child(token_row)
                    .child(div().text_color(rgb(DIM)).text_size(px(11.)).child(
                        "Use an AniList access token. This replaces a pending browser sign-in.",
                    ));
        }
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .child(sheet)
            .into_any()
    }

    pub(super) fn render_login_recovery(&self, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .when(self.auth_oauth, |content| {
                content.child(div().text_color(rgb(DIM)).child(
                    "If you signed in through an email link, open AniList again to finish connecting.",
                ))
            })
            .child(
                button("cancel-login", "Cancel sign-in", false)
                    .debug_selector(|| "cancel-login".into())
                    .self_start()
                    .on_click(cx.listener(|this, _, window, cx| this.cancel_login(window, cx))),
            )
    }

    pub(super) fn render_pending_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.profile.is_none() || self.pending.is_empty() && self.pending_error.is_empty() {
            return div().into_any();
        }
        let count = self.pending.len();
        let mut bar = div()
            .border_b_1()
            .border_color(rgb(EDGE))
            .bg(rgb(PANEL))
            .px(px(16.))
            .py(px(7.))
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .child(
                        button(
                            "pending-toggle",
                            if count > 0 {
                                format!(
                                    "{count} saved {} waiting to sync",
                                    if count == 1 { "change" } else { "changes" }
                                )
                            } else {
                                "Sync status unavailable".into()
                            },
                            false,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pending_expanded = !this.pending_expanded;
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "pending-retry",
                            if self.pending_busy {
                                "Syncing…"
                            } else {
                                "Retry sync"
                            },
                            self.pending_busy,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| this.retry_pending(window, cx)),
                        ),
                    ),
            );
        if !self.pending_error.is_empty() {
            bar = bar.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(self.pending_error.clone()),
            );
        }
        if self.pending_expanded {
            bar = bar.child(div().text_color(rgb(DIM)).text_size(px(12.)).child("Saved on this device for your account. Kurisu retries automatically when AniList is available."));
            let mut items = div()
                .id("pending-items")
                .max_h(px(180.))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            for item in &self.pending {
                let id = item.media_id;
                let mut row = div()
                    .border_t_1()
                    .border_color(rgb(EDGE))
                    .py(px(6.))
                    .child(format!(
                        "{}{}",
                        item.title,
                        item.progress
                            .map_or_else(String::new, |value| format!(" · Episode {value}"))
                    ))
                    .child(div().text_color(rgb(DIM)).text_size(px(11.)).child(format!(
                            "{}{}{}",
                            item.status.as_deref().map(status_label).unwrap_or(""),
                            item.score
                                .map_or_else(String::new, |score| format!(" · Score {score}")),
                            item.repeat.map_or_else(String::new, |repeat| format!(
                                " · Rewatches {repeat}"
                            ))
                        )));
                if let Some(error) = &item.error {
                    row = row.child(
                        div()
                            .text_color(rgb(ERROR))
                            .text_size(px(11.))
                            .child(error.clone()),
                    );
                }
                if item.conflict {
                    let mut actions = div().flex().gap(px(6.)).child(
                        button(
                            format!("pending-remote-{id}"),
                            if item.missing_media {
                                "Discard saved change"
                            } else {
                                "Keep AniList version"
                            },
                            self.pending_busy,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.resolve_pending(id, false, window, cx),
                        )),
                    );
                    if !item.missing_media {
                        actions = actions.child(
                            button(
                                format!("pending-local-{id}"),
                                "Send saved change",
                                self.pending_busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.resolve_pending(id, true, window, cx)
                                },
                            )),
                        );
                    }
                    row = row.child(actions);
                }
                items = items.child(row);
            }
            bar = bar.child(items);
        }
        bar.into_any()
    }

    pub(super) fn render_overlays(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = self.active_modal();
        let focus = self.modal_focus();
        if self.editor.is_none()
            && !self.logout_confirming
            && self.prompt.is_none()
            && self.update.is_none()
            && self.update_failed.is_empty()
            && self.barrier.is_none()
            && self.barrier_error.is_empty()
            && self.global_error.is_empty()
            && (self.preference_alert_dismissed || self.preference_error().is_none())
        {
            return div().into_any();
        }
        let mut layers = div()
            .id("workspace-overlays")
            .absolute()
            .inset_0()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.active_modal().is_some() {
                    this.escape_active_modal(cx);
                    cx.stop_propagation();
                }
            }));
        if self.editor.is_some() {
            layers = layers.child(backdrop(
                self.render_editor(window, cx),
                active == Some(ModalLayer::Editor),
                &focus,
                true,
            ));
        }
        if self.logout_confirming {
            layers =
                layers.child(backdrop(
                    dialog("logout-confirm-dialog", "Sign out")
                        .w(px(400.))
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .child(heading("Sign out?"))
                        .child(
                            div()
                                .text_color(rgb(DIM))
                                .child("Your saved changes stay with this AniList account."),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button("logout-cancel", "Cancel", false).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.logout_confirming = false;
                                        cx.notify();
                                    }),
                                ))
                                .child(button("logout-confirm", "Sign out", false).on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.logout_confirming = false;
                                        this.start_barrier(BarrierAction::Logout, window, cx);
                                    }),
                                )),
                        )
                        .into_any(),
                    active == Some(ModalLayer::Logout),
                    &focus,
                    true,
                ));
        }
        if let Some(prompt) = self.prompt.as_ref() {
            let title = prompt.title.clone();
            let episode = prompt.episode;
            let progress = prompt.progress;
            let mut contents = dialog("tracking-prompt", "Update your list?")
                .w(px(460.))
                .flex()
                .flex_col()
                .gap(px(9.))
                .child(heading("Update your list?"))
                .child(div().text_color(rgb(DIM)).child("Detected playback:"))
                .child(div().child(title))
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Episode {episode}")),
                );
            if episode <= progress {
                contents = contents.child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Already past Ep {episode} (rewatch)")),
                );
            }
            if !self.prompt_error.is_empty() {
                contents = contents.child(
                    div()
                        .text_color(rgb(ERROR))
                        .child(format!("Update failed: {}", self.prompt_error)),
                );
            }
            let mut actions = div().flex().gap(px(8.)).child(
                button("prompt-skip", "Skip", self.prompt_busy)
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss_prompt(cx))),
            );
            if episode > progress {
                actions = actions.child(
                    button(
                        "prompt-confirm",
                        if self.prompt_busy {
                            "Updating…".into()
                        } else {
                            format!("Set progress to Ep {episode}")
                        },
                        self.prompt_busy,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_prompt(window, cx))),
                );
            }
            layers = layers.child(backdrop(
                contents.child(actions).into_any(),
                active == Some(ModalLayer::Prompt),
                &focus,
                true,
            ));
        }
        if let Some(update) = self.update.as_ref() {
            let version = update.version.clone();
            let current = update.current.clone();
            let url = update.html_url.clone();
            let can_install = update.can_install;
            let restart_pending = self.restart_pending || update.restart_pending;
            let mut notice = dialog("update-available", "Update available")
                .w(px(460.))
                .flex()
                .flex_col()
                .gap(px(9.))
                .child(heading("Update available"));
            if restart_pending {
                notice = notice.child(
                    div()
                        .text_color(rgb(DIM))
                        .child("An update is installed. Restart Kurisu to finish."),
                );
            } else {
                notice = notice.child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Kurisu {version} is out. You're on {current}.")),
                );
                if !update.body.is_empty() {
                    notice = notice.child(
                        div()
                            .id("update-body")
                            .max_h(px(130.))
                            .overflow_y_scroll()
                            .bg(rgb(PANEL_2))
                            .p(px(8.))
                            .child(update.body.clone()),
                    );
                }
            }
            notice = notice.child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        button(
                            "update-later",
                            if restart_pending { "Got it" } else { "Later" },
                            self.update_busy,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.update_busy {
                                return;
                            }
                            if let Some(update) = this.update.take() {
                                this.dismissed_update = update.tag;
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        button(
                            "update-install",
                            if restart_pending {
                                "Installed"
                            } else if can_install {
                                "Download & install"
                            } else {
                                "View release"
                            },
                            self.update_busy || restart_pending,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.update_busy
                                    || this.restart_pending
                                    || this
                                        .update
                                        .as_ref()
                                        .is_none_or(|update| update.restart_pending)
                                {
                                    return;
                                }
                                if can_install {
                                    this.start_barrier(BarrierAction::Install, window, cx);
                                } else if let Err(error) =
                                    crate::platform::NativePlatform.open_url(&url)
                                {
                                    this.update_failed = error;
                                    cx.notify();
                                }
                            },
                        )),
                    ),
            );
            layers = layers.child(backdrop(
                notice.into_any(),
                active == Some(ModalLayer::Update),
                &focus,
                true,
            ));
        }
        if !self.update_failed.is_empty() {
            layers =
                layers.child(backdrop(
                    dialog("update-failed", "Update failed")
                        .w(px(440.))
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .child(heading("Update failed"))
                        .child(
                            div()
                                .text_color(rgb(ERROR))
                                .child(self.update_failed.clone()),
                        )
                        .child(button("dismiss-update-error", "Got it", false).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.update_failed.clear();
                                cx.notify();
                            }),
                        ))
                        .into_any(),
                    active == Some(ModalLayer::UpdateFailed),
                    &focus,
                    true,
                ));
        }
        if self.barrier.is_some() && self.barrier_error.is_empty() {
            layers = layers.child(backdrop(
                dialog("saving-before-close", "Saving changes")
                    .w(px(400.))
                    .child("Saving changes before continuing…")
                    .child(
                        button("cancel-barrier", "Cancel", false)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_barrier(cx))),
                    )
                    .into_any(),
                active == Some(ModalLayer::Barrier),
                &focus,
                true,
            ));
        }
        let preference_error = (!self.preference_alert_dismissed)
            .then(|| self.preference_error_message())
            .flatten();
        if !self.barrier_error.is_empty()
            || preference_error.is_some()
            || !self.global_error.is_empty()
        {
            let error = if !self.barrier_error.is_empty() {
                self.barrier_error.clone()
            } else if let Some(error) = &preference_error {
                error.clone()
            } else {
                self.global_error.clone()
            };
            let mut alert = div()
                .id("workspace-error")
                .role(gpui::Role::Alert)
                .aria_label(error.clone())
                .occlude()
                .absolute()
                .bottom(px(12.))
                .left(px(12.))
                .right(px(12.))
                .border_1()
                .border_color(rgb(ERROR))
                .bg(rgb(PANEL))
                .p(px(10.))
                .text_color(rgb(ERROR))
                .child(error);
            if self.barrier.is_some() {
                if self.has_inactive_drafts() {
                    alert = alert.child(
                        button(
                            "discard-other-account-drafts",
                            "Discard other account drafts and continue",
                            false,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.discard_inactive_drafts(window, cx)
                        })),
                    );
                }
                alert = alert
                    .child(
                        button("retry-barrier", "Retry saves", false)
                            .debug_selector(|| "retry-barrier".into())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.retry_barrier(window, cx)),
                            ),
                    )
                    .child(
                        button("cancel-failed-barrier", "Cancel", false)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_barrier(cx))),
                    );
            } else if preference_error.is_some() {
                alert = alert
                    .child(
                        button("retry-preferences", "Retry save", false)
                            .debug_selector(|| "retry-preferences".into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.retry_preferences(window, cx)
                            })),
                    )
                    .child(
                        button("dismiss-preference-error", "Dismiss", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.preference_alert_dismissed = true;
                                cx.notify();
                            },
                        )),
                    );
            } else {
                alert = alert.child(button("dismiss-global-error", "Dismiss", false).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.barrier_error.clear();
                        this.global_error.clear();
                        cx.notify();
                    }),
                ));
            }
            if active == Some(ModalLayer::BarrierError) {
                layers = layers.child(backdrop(alert.into_any(), true, &focus, false));
            } else {
                layers = layers.child(alert);
            }
        }
        layers.into_any()
    }
}

fn backdrop(content: AnyElement, active: bool, focus: &ModalFocus, shade: bool) -> AnyElement {
    let mut layer = div()
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .p(px(20.));
    if shade {
        layer = layer.bg(gpui::rgba(0x000000b8));
    }
    modal_container(layer, content, active, focus)
}

pub(super) fn modal_container(
    layer: gpui::Div,
    content: AnyElement,
    active: bool,
    focus: &ModalFocus,
) -> AnyElement {
    if active {
        return layer
            .id("active-modal-backdrop")
            .track_focus(&focus.root)
            .tab_group()
            .child(div().id("modal-tab-start").track_focus(&focus.start))
            .child(content)
            .child(div().id("modal-tab-end").track_focus(&focus.end))
            .into_any();
    }
    layer.child(content).into_any()
}
