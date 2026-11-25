use gpui::{AnyElement, Context, InteractiveElement, Window, deferred, div, prelude::*, px, rgb};
use kurisu_core::Platform;

use super::state::status_label;
use super::{BarrierAction, ModalFocus, ModalLayer, Workspace};
use crate::components::{button, dialog, eyebrow, heading};
use crate::theme::{ACCENT, BASE, DIM, EDGE, ERROR, PANEL, PANEL_2};

impl Workspace {
    pub(super) fn escape_active_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_modal() {
            Some(ModalLayer::Barrier | ModalLayer::BarrierError) => self.cancel_barrier(window, cx),
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
            Some(ModalLayer::Prompt) => self.dismiss_prompt(self.prompt_revision, cx),
            Some(ModalLayer::Logout) => {
                self.logout_confirming = false;
                cx.notify();
            }
            Some(ModalLayer::Editor) if self.editor_status_open => {
                self.editor_status_open = false;
                window.focus(&self.editor_status_focus, cx);
                cx.notify();
            }
            Some(ModalLayer::Editor) => self.dismiss_editor(cx),
            Some(
                ModalLayer::LibraryRemove | ModalLayer::LibraryUnlink | ModalLayer::LibraryBind,
            ) => self.dismiss_library_dialog(cx),
            Some(ModalLayer::TorrentRemove) => self.dismiss_torrent_remove(cx),
            _ => {}
        }
        self.resume_overdue_progress(window, cx);
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
        let actions_disabled = self.pending_actions_disabled();
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
                            self.modal_open() || self.edits_fenced(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.modal_open() || this.edits_fenced() || this.profile.is_none() {
                                return;
                            }
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
                            actions_disabled,
                        )
                        .debug_selector(|| "pending-retry".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.retry_pending(window, cx)),
                        ),
                    ),
            );
        if !self.pending_error.is_empty() {
            bar = bar.child(
                div()
                    .id("pending-error")
                    .role(gpui::Role::Alert)
                    .aria_label(self.pending_error.clone())
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
                let summary = format!(
                    "{}{}",
                    item.title,
                    item.progress
                        .map_or_else(String::new, |value| format!(" · Episode {value}"))
                );
                let details = format!(
                    "{}{}{}",
                    item.status.as_deref().map(status_label).unwrap_or(""),
                    item.score
                        .map_or_else(String::new, |score| format!(" · Score {score}")),
                    item.repeat
                        .map_or_else(String::new, |repeat| format!(" · Rewatches {repeat}"))
                );
                let description = match &item.error {
                    Some(error) if details.is_empty() => error.clone(),
                    Some(error) => format!("{details}. {error}"),
                    None => details.clone(),
                };
                let action_description = format!("Saved change for {summary}. {description}");
                let mut row = div()
                    .id(format!("pending-item-{id}"))
                    .role(gpui::Role::Group)
                    .aria_label(format!("Saved change for {summary}"))
                    .aria_description(description.clone())
                    .border_t_1()
                    .border_color(rgb(EDGE))
                    .py(px(6.))
                    .child(summary.clone())
                    .child(div().text_color(rgb(DIM)).text_size(px(11.)).child(details));
                if let Some(error) = &item.error {
                    row = row.child(
                        div()
                            .text_color(rgb(ERROR))
                            .text_size(px(11.))
                            .child(error.clone()),
                    );
                }
                if item.conflict {
                    let remote_action = if item.missing_media {
                        "Discard saved change"
                    } else {
                        "Keep AniList version"
                    };
                    let mut actions = div().flex().gap(px(6.)).child(
                        button(
                            format!("pending-remote-{id}"),
                            remote_action,
                            actions_disabled,
                        )
                        .aria_label(format!("{remote_action} for {}", item.title))
                        .aria_description(action_description.clone())
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.resolve_pending(id, false, window, cx),
                        )),
                    );
                    if !item.missing_media {
                        actions = actions.child(
                            button(
                                format!("pending-local-{id}"),
                                "Send saved change",
                                actions_disabled,
                            )
                            .aria_label(format!("Send saved change for {}", item.title))
                            .aria_description(action_description)
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
        let preference_error = (active.is_none() && !self.preference_alert_dismissed)
            .then(|| self.preference_error_message())
            .flatten();
        let mut notices = Vec::new();
        if let Some(error) = &self.tray_error {
            let tray_alert = div()
                .id("tray-error")
                .role(gpui::Role::Alert)
                .aria_label(error.clone())
                .absolute()
                .top(px(10.))
                .right(px(20.))
                .max_w(px(420.))
                .p(px(10.))
                .border_1()
                .border_color(rgb(ERROR))
                .bg(rgb(PANEL_2))
                .text_color(rgb(ERROR))
                .child(error.clone());
            notices.push(if active.is_none() {
                deferred(tray_alert).into_any()
            } else {
                tray_alert.into_any()
            });
        }
        if matches!(active, None | Some(ModalLayer::Editor))
            && preference_error.is_none()
            && !self.global_error.is_empty()
        {
            notices.push(
                error_alert(self.global_error.clone())
                    .child(
                        button("dismiss-global-error", "Dismiss", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.global_error.clear();
                                cx.notify();
                            },
                        )),
                    )
                    .into_any(),
            );
        }
        let mut present = |content, layer, shade| {
            let foreground = active == Some(layer);
            let notices = if foreground {
                std::mem::take(&mut notices)
            } else {
                Vec::new()
            };
            backdrop(content, foreground, &focus, shade, notices)
        };
        let mut layers = div().id("workspace-overlays").absolute().inset_0();
        if let Some(layer) = self.library_modal_layer() {
            layers = layers.child(present(self.render_library_overlay(cx), layer, true));
        }
        if self.torrent_remove_open() {
            layers = layers.child(present(
                self.render_torrent_remove(cx),
                ModalLayer::TorrentRemove,
                true,
            ));
        }
        if self.editor.is_some() {
            layers = layers.child(present(
                self.render_editor(window, cx),
                ModalLayer::Editor,
                true,
            ));
        }
        if self.logout_confirming {
            layers =
                layers.child(present(
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
                                        if this.active_modal() == Some(ModalLayer::Logout) {
                                            this.logout_confirming = false;
                                            cx.notify();
                                        }
                                    }),
                                ))
                                .child(
                                    button(
                                        "logout-confirm",
                                        "Sign out",
                                        self.edits_fenced() || active != Some(ModalLayer::Logout),
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            if this.edits_fenced()
                                                || this.active_modal() != Some(ModalLayer::Logout)
                                            {
                                                return;
                                            }
                                            this.logout_confirming = false;
                                            this.start_barrier(BarrierAction::Logout, window, cx);
                                        },
                                    )),
                                ),
                        )
                        .into_any(),
                    ModalLayer::Logout,
                    true,
                ));
        }
        if let Some(prompt) = self.prompt.as_ref() {
            let revision = self.prompt_revision;
            let title = prompt.title.clone();
            let episode = prompt.episode;
            let progress = self.prompt_current_progress(prompt);
            let progress_text = progress.map_or_else(
                || "This title is no longer on your list".to_string(),
                |value| format!("Current progress: {value}"),
            );
            let rewatch =
                episode <= prompt.progress || progress.is_some_and(|value| episode <= value);
            let context = format!(
                "Detected playback: {title}. Episode {episode}. {progress_text}.{}",
                if rewatch { " Rewatch." } else { "" }
            );
            let mut contents = dialog(("tracking-prompt", revision), "Update your list?")
                .aria_description(context)
                .w(px(460.))
                .flex()
                .flex_col()
                .gap(px(9.))
                .child(heading("Update your list?"))
                .child(div().text_color(rgb(DIM)).child("Detected playback:"))
                .child(div().child(title.clone()))
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Episode {episode}")),
                )
                .child(div().text_color(rgb(DIM)).child(progress_text));
            if rewatch {
                contents = contents.child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Rewatch of Ep {episode}")),
                );
            }
            if !self.prompt_error.is_empty() {
                contents = contents.child(
                    div()
                        .id("prompt-error")
                        .role(gpui::Role::Alert)
                        .aria_label(format!("Update failed: {}", self.prompt_error))
                        .text_color(rgb(ERROR))
                        .child(format!("Update failed: {}", self.prompt_error)),
                );
            }
            let disabled = !self.prompt_action_allowed(revision);
            let mut actions = div().flex().gap(px(8.)).child(
                button("prompt-skip", "Skip", disabled)
                    .aria_description(format!("Skip the playback update for {title}."))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.dismiss_prompt(revision, cx);
                    })),
            );
            if let Some(target) = self.prompt_target(prompt) {
                let settling_progress = self.prompt_progress_pending(prompt.media_id);
                actions = actions.child(
                    button(
                        ("prompt-confirm", target as u64),
                        if self.prompt_busy {
                            "Updating…".into()
                        } else if settling_progress {
                            "Saving list change…".into()
                        } else {
                            format!("Set progress to Ep {target}")
                        },
                        disabled || settling_progress,
                    )
                    .aria_description(format!("Set {title} progress to episode {target}."))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm_prompt(revision, target, window, cx);
                    })),
                );
            }
            layers = layers.child(present(
                contents.child(actions).into_any(),
                ModalLayer::Prompt,
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
                            self.update_busy || active != Some(ModalLayer::Update),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.update_busy || this.active_modal() != Some(ModalLayer::Update) {
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
                            self.edits_fenced()
                                || restart_pending
                                || active != Some(ModalLayer::Update),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.edits_fenced()
                                    || this.active_modal() != Some(ModalLayer::Update)
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
                                } else {
                                    let url = url.clone();
                                    let core = this.core.clone();
                                    cx.spawn_in(window, async move |this, cx| {
                                        let result = super::joined(
                                            core.spawn(async move {
                                                tokio::task::spawn_blocking(move || {
                                                    crate::platform::NativePlatform.open_url(&url)
                                                })
                                                .await
                                                .map_err(|error| error.to_string())?
                                            })
                                            .await,
                                        );
                                        let _ = this.update_in(cx, |this, _, cx| {
                                            if let Err(error) = result {
                                                this.update_failed = error;
                                                cx.notify();
                                            }
                                        });
                                    })
                                    .detach();
                                }
                            },
                        )),
                    ),
            );
            layers = layers.child(present(notice.into_any(), ModalLayer::Update, true));
        }
        if !self.update_failed.is_empty() {
            layers =
                layers.child(present(
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
                                if this.active_modal() == Some(ModalLayer::UpdateFailed) {
                                    this.update_failed.clear();
                                    cx.notify();
                                }
                            }),
                        ))
                        .into_any(),
                    ModalLayer::UpdateFailed,
                    true,
                ));
        }
        if self.close_ready {
            layers = layers.child(present(
                dialog("closing-window", "Closing Kurisu")
                    .w(px(400.))
                    .child("Closing Kurisu…")
                    .into_any(),
                ModalLayer::Barrier,
                true,
            ));
        } else if self.barrier.is_some() && self.barrier_error.is_empty() {
            layers = layers.child(present(
                dialog("saving-before-close", "Saving changes")
                    .w(px(400.))
                    .child("Saving changes before continuing…")
                    .child(button("cancel-barrier", "Cancel", false).on_click(
                        cx.listener(|this, _, window, cx| this.cancel_barrier(window, cx)),
                    ))
                    .into_any(),
                ModalLayer::Barrier,
                true,
            ));
        }
        let show_barrier_error = active == Some(ModalLayer::BarrierError);
        if let Some(error) = show_barrier_error
            .then(|| self.barrier_error.clone())
            .or(preference_error)
        {
            let mut alert = error_alert(error);
            if show_barrier_error {
                if self.barrier_blocked_by_inactive_drafts {
                    alert = alert.child(
                        button(
                            "discard-other-account-drafts",
                            "Discard other account drafts and continue",
                            false,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.discard_inactive_drafts(window, cx);
                        })),
                    );
                } else {
                    let discard_label = if matches!(
                        self.barrier,
                        Some(BarrierAction::Close(_) | BarrierAction::LogoutThenClose(_))
                    ) {
                        "Close without saving"
                    } else {
                        "Continue without saving"
                    };
                    alert = alert.child(
                        button("discard-barrier", discard_label, false)
                            .debug_selector(|| "discard-barrier".into())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.discard_barrier(window, cx)),
                            ),
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
                            .debug_selector(|| "cancel-failed-barrier".into())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.cancel_barrier(window, cx)),
                            ),
                    );
            } else {
                alert = alert
                    .child(
                        button(
                            "retry-preferences",
                            "Retry save",
                            self.edits_fenced() || self.modal_open(),
                        )
                        .debug_selector(|| "retry-preferences".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.retry_preferences(window, cx)),
                        ),
                    )
                    .child(
                        button("dismiss-preference-error", "Dismiss", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.preference_alert_dismissed = true;
                                cx.notify();
                            },
                        )),
                    );
            }
            if show_barrier_error {
                layers = layers.child(present(alert.into_any(), ModalLayer::BarrierError, false));
            } else {
                layers = layers.child(alert);
            }
        }
        layers.children(notices).into_any()
    }
}

fn error_alert(error: String) -> gpui::Stateful<gpui::Div> {
    div()
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
        .child(error)
}

fn backdrop(
    content: AnyElement,
    active: bool,
    focus: &ModalFocus,
    shade: bool,
    notices: Vec<AnyElement>,
) -> AnyElement {
    let mut layer = div()
        .absolute()
        .inset_0()
        .occlude()
        .on_scroll(|_, _, cx| cx.stop_propagation())
        .flex()
        .items_center()
        .justify_center()
        .p(px(20.));
    if shade {
        layer = layer.bg(gpui::rgba(0x000000b8));
    }
    modal_container(layer, content, active, focus, notices)
}

fn modal_container(
    layer: gpui::Div,
    content: AnyElement,
    active: bool,
    focus: &ModalFocus,
    notices: Vec<AnyElement>,
) -> AnyElement {
    if active {
        return layer
            .id("active-modal-backdrop")
            .role(gpui::Role::Group)
            .a11y_synthetic_children(|builder| builder.parent_node().set_modal())
            .track_focus(&focus.root)
            .tab_group()
            .child(div().id("modal-tab-start").track_focus(&focus.start))
            .child(content)
            .children(notices)
            .child(div().id("modal-tab-end").track_focus(&focus.end))
            .into_any();
    }
    layer.child(content).into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::accesskit::{Action, ActionRequest, TreeId};

    struct OverlappingButtons {
        modal: Option<ModalFocus>,
        clicks: usize,
    }

    impl Render for OverlappingButtons {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let bounds = |button: gpui::Stateful<gpui::Div>| button.absolute().inset_0();
            div()
                .relative()
                .size_full()
                .child(
                    bounds(button("covered", "Covered", false))
                        .on_click(cx.listener(|this, _, _, _| this.clicks += 1)),
                )
                .when_some(self.modal.as_ref(), |root, focus| {
                    root.child(modal_container(
                        div().absolute().inset_0().occlude(),
                        bounds(button("foreground", "Foreground", false))
                            .on_click(cx.listener(|this, _, _, _| this.clicks += 1))
                            .into_any(),
                        true,
                        focus,
                        Vec::new(),
                    ))
                })
        }
    }

    #[gpui::test]
    fn stale_click_cannot_hit_the_foreground_at_the_same_coordinates(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| OverlappingButtons {
            modal: None,
            clicks: 0,
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let button = |tree: gpui::accesskit::TreeUpdate, label| {
            tree.nodes
                .into_iter()
                .find(|(_, node)| node.label() == Some(label))
                .map(|(id, node)| (id, node.bounds()))
                .unwrap()
        };
        let click = |cx: &gpui::VisualTestContext, target_node| {
            cx.simulate_accessibility_action(ActionRequest {
                action: Action::Click,
                target_tree: TreeId::ROOT,
                target_node,
                data: None,
            });
        };
        let (covered, bounds) = button(cx.take_accessibility_tree_update().unwrap(), "Covered");
        view.update(cx, |view, cx| {
            view.modal = Some(ModalFocus {
                root: cx.focus_handle(),
                start: cx.focus_handle(),
                end: cx.focus_handle(),
            });
            cx.notify();
        });
        click(cx, covered);
        assert_eq!(view.read_with(cx, |view, _| view.clicks), 0);
        let (foreground, foreground_bounds) =
            button(cx.take_accessibility_tree_update().unwrap(), "Foreground");
        assert_eq!(foreground_bounds, bounds);
        click(cx, covered);
        assert_eq!(view.read_with(cx, |view, _| view.clicks), 0);
        click(cx, foreground);
        assert_eq!(view.read_with(cx, |view, _| view.clicks), 1);
    }

    #[gpui::test]
    fn accessibility_actions_require_the_current_published_activation(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(|_, _| OverlappingButtons {
            modal: None,
            clicks: 0,
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Covered"))
            .unwrap();
        let click = || ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: *id,
            data: None,
        };
        cx.queue_accessibility_action(click());
        cx.simulate_accessibility_deactivation();
        cx.simulate_accessibility_activation();
        cx.queue_accessibility_action(click());
        cx.update(|win, app| win.draw(app).clear(app));
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.clicks), 0);
        cx.simulate_accessibility_action(click());
        assert_eq!(view.read_with(cx, |view, _| view.clicks), 1);
    }
}
