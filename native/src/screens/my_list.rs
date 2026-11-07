use gpui::{
    AnyElement, Focusable, FontWeight, RenderImage, WeakEntity, Window, deferred, div, img, list,
    prelude::*, px, rgb, svg,
};
use kurisu_core::models::ListEntry;
use std::sync::Arc;

use super::Workspace;
use super::state::{STATUSES, SortKey, status_label};
use crate::components::scrollbar::list_scrollbar;
use crate::components::{button, eyebrow, select, tracked_mono};
use crate::contracts::Route;
use crate::theme::{
    ACCENT, BASE, BLUE, DIM, EDGE, ERROR, INK, OCHRE, PANEL, PANEL_2, RUST, SAGE, mono_font,
    serif_font,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MyListSlot {
    Header,
    Media(usize),
    Footer,
}

impl MyListSlot {
    pub(super) fn count(media_count: usize) -> usize {
        media_count + 2
    }

    pub(super) fn at(item_ix: usize, media_count: usize) -> Self {
        if item_ix == 0 {
            Self::Header
        } else if item_ix <= media_count {
            Self::Media(item_ix - 1)
        } else {
            Self::Footer
        }
    }

    pub(super) fn item_ix(media_ix: usize) -> usize {
        media_ix + 1
    }

    pub(super) fn footer_ix(media_count: usize) -> usize {
        media_count + 1
    }

    pub(super) fn media_range(
        item_range: std::ops::Range<usize>,
        media_count: usize,
    ) -> std::ops::Range<usize> {
        item_range.start.saturating_sub(1).min(media_count)
            ..item_range.end.saturating_sub(1).min(media_count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MyListAnchor {
    Header(gpui::Pixels),
    Media(i64, gpui::Pixels),
    Footer(gpui::Pixels),
    End,
}

fn tab_color(status: &str) -> u32 {
    match status {
        "CURRENT" => SAGE,
        "PLANNING" => BLUE,
        "COMPLETED" => OCHRE,
        "PAUSED" => ACCENT,
        "DROPPED" => 0xb18992,
        _ => 0xaa98bd,
    }
}

fn grouped_count(value: impl ToString) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.chars().rev().collect()
}

fn sync_label(at: Option<i64>, now: i64, flash: bool) -> String {
    if flash {
        return "Synced ✓".into();
    }
    let Some(at) = at.filter(|at| *at > 0) else {
        return "Cached on this device".into();
    };
    let seconds = now.saturating_sub(at);
    if seconds < 60 {
        return "Synced just now".into();
    }
    let age = if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else if seconds < 604800 {
        format!("{}d", seconds / 86400)
    } else {
        chrono::DateTime::from_timestamp(at, 0)
            .map(|date| {
                date.with_timezone(&chrono::Local)
                    .format("%b %-d")
                    .to_string()
            })
            .unwrap_or_default()
    };
    format!("Synced {age} ago")
}

impl Workspace {
    pub(super) fn render_workspace(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        if self.sort_open
            && (self.modal_open()
                || self.route != Route::MyList
                || (self.sort_menu_focus_subscription.is_some()
                    && !self.sort_menu_focus.contains_focused(window, cx)))
        {
            self.sort_open = false;
        }
        let content = if self.profile.is_none() && self.route != Route::Settings {
            self.render_login(cx)
        } else {
            match self.route.clone() {
                Route::MyList => self.render_my_list(window, cx),
                Route::Anime(id) => self.render_detail(id, window, cx),
                Route::Now => self.render_watching(window, cx),
                Route::Library => self.render_library(window, cx),
                Route::Stats => self.render_stats(window, cx),
                Route::Settings => self.render_settings(window, cx),
                Route::Torrents => self.render_torrents(window, cx),
                Route::Notifications => self.render_notifications(window, cx),
                Route::Search => self.render_search(window, cx),
                Route::Seasons => self.render_seasons(window, cx),
                Route::Calendar => self.render_calendar(window, cx),
            }
        };
        div()
            .id("workspace")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BASE))
            .text_color(rgb(INK))
            .child(self.render_pending_bar(cx))
            .child(self.render_now_playing())
            .child(content)
            .child(self.render_overlays(window, cx))
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.route == Route::MyList && this.sort_open {
                    this.sort_open = false;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.route == Route::MyList && this.sort_open && event.keystroke.key == "escape"
                {
                    this.sort_open = false;
                    window.focus(&this.sort_menu_focus, cx);
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if event.keystroke.key != "escape" {
                    return;
                }
                if this.prompt.is_some() && !this.prompt_busy {
                    this.dismiss_prompt(cx);
                } else if this.logout_confirming {
                    this.logout_confirming = false;
                    cx.notify();
                } else if this.editor.is_some() {
                    this.close_editor(cx);
                } else if this.update.is_some() && !this.update_busy {
                    this.update = None;
                    cx.notify();
                } else {
                    this.back(window, cx);
                }
            }))
            .into_any()
    }

    fn render_my_list(&mut self, window: &Window, cx: &mut gpui::Context<Self>) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) < 960.;
        let ids = self.visible_ids.clone();
        let footer_ix = MyListSlot::footer_ix(ids.len());
        self.list_widget.remeasure_items(0..1);
        self.list_widget.remeasure_items(footer_ix..footer_ix + 1);
        let weak = cx.weak_entity();
        let scroll_weak = weak.clone();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                list(
                    self.list_widget.clone(),
                    move |item_ix, window, app| match MyListSlot::at(item_ix, ids.len()) {
                        MyListSlot::Header => weak
                            .update(app, |this, cx| this.render_my_list_header(window, cx))
                            .unwrap_or_else(|_| div().into_any()),
                        MyListSlot::Footer => weak
                            .read_with(app, |this, _| this.render_my_list_footer(compact))
                            .unwrap_or_else(|_| div().into_any()),
                        MyListSlot::Media(index) => {
                            let Some(&id) = ids.get(index) else {
                                return div().into_any();
                            };
                            let data = weak.read_with(app, |this, _| {
                                let entry = this.list.entry(id).cloned();
                                let pending = this.progress.edit(this.generation, id).cloned();
                                let cover = this.cover_images.get(&(id, 46, 63)).cloned();
                                let format = this
                                    .profile
                                    .as_ref()
                                    .and_then(|user| user.score_format.clone());
                                (entry, pending, cover, format)
                            });
                            let Ok((Some(entry), pending, cover, format)) = data else {
                                return div().into_any();
                            };
                            div()
                                .w_full()
                                .px(px(if compact { 20. } else { 28. }))
                                .child(render_list_row(
                                    index,
                                    entry,
                                    pending,
                                    cover,
                                    format.as_deref(),
                                    compact,
                                    weak.clone(),
                                ))
                                .into_any()
                        }
                    },
                )
                .size_full(),
            )
            .child(list_scrollbar(
                "my-list-scrollbar",
                "My List scroll position",
                self.list_widget.clone(),
                move |_first, window, app| {
                    let _ = scroll_weak.update(app, |this, cx| {
                        if this.generation == generation
                            && this.profile.as_ref().map(|user| user.id) == account
                        {
                            this.prefetch_visible_covers(window, cx);
                            cx.notify();
                        }
                    });
                },
            ))
            .into_any()
    }

    fn render_my_list_header(
        &mut self,
        window: &Window,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) < 960.;
        let mut tabs = div()
            .flex()
            .flex_wrap()
            .gap_x(px(if compact { 12. } else { 16. }))
            .gap_y(px(0.))
            .border_b_1()
            .border_color(rgb(EDGE))
            .mb(px(16.));
        for status in STATUSES {
            let count = self.list.count(status);
            let selected = self.list.filter == status;
            let color = tab_color(status);
            tabs = tabs.child(
                div()
                    .id(format!("tab-{status}"))
                    .debug_selector(move || format!("tab-{status}"))
                    .role(gpui::Role::Button)
                    .aria_label(format!("{} {count}", status_label(status)))
                    .aria_toggled(gpui::accesskit::Toggled::from(selected))
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(|style| style.bg(rgb(PANEL)).border_color(rgb(ACCENT)))
                    .cursor_pointer()
                    .flex()
                    .items_baseline()
                    .whitespace_nowrap()
                    .gap(px(6.))
                    .pt(px(10.))
                    .pb(px(12.))
                    .border_b_2()
                    .border_color(rgb(if selected { color } else { BASE }))
                    .mb(px(-1.))
                    .text_color(rgb(if selected { INK } else { DIM }))
                    .child(
                        div()
                            .w(px(5.))
                            .h(px(5.))
                            .rounded(px(3.))
                            .border_1()
                            .border_color(rgb(color))
                            .bg(rgb(if selected { color } else { BASE }))
                            .self_center(),
                    )
                    .child(status_label(status))
                    .child(
                        div()
                            .font(mono_font())
                            .text_size(px(13.))
                            .text_color(rgb(color))
                            .pl(px(3.))
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_filter(status, window, cx)
                    }))
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                cx.stop_propagation();
                                this.select_filter(status, window, cx);
                            }
                        },
                    )),
            );
        }
        let query_focus = self.list_query.read(cx).focus_handle(cx);
        let search = div()
            .id("my-list-search")
            .debug_selector(|| "my-list-search".into())
            .track_focus(&query_focus)
            .focus(|style| style.border_color(rgb(ACCENT)))
            .h(px(45.))
            .min_w(px(160.))
            .flex_1()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .border_1()
            .border_color(rgb(EDGE))
            .bg(rgb(PANEL))
            .cursor(gpui::CursorStyle::IBeam)
            .on_click(cx.listener(|this, _, window, cx| {
                cx.focus_view(&this.list_query, window);
            }))
            .child(
                div()
                    .debug_selector(|| "my-list-search-icon".into())
                    .w(px(15.))
                    .h(px(15.))
                    .flex_shrink_0()
                    .child(
                        svg()
                            .path("icons/search.svg")
                            .size_full()
                            .text_color(rgb(DIM)),
                    ),
            )
            .child(div().flex_1().min_w_0().child(self.list_query.clone()));
        let mut toolbar = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .min_h(px(42.))
            .mb(px(22.))
            .child(search);
        let sort = self.list.sort;
        let desc = self.list.desc;
        let mut sort_control =
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .ml(px(if compact { 0. } else { 12. }));
        if !compact {
            sort_control = sort_control.child(eyebrow("SORT BY"));
        }
        let mut sort_select = select("sort-select", "Sort list")
            .aria_value(sort.label())
            .aria_description(format!("Selected sort: {}", sort.label()))
            .relative()
            .track_focus(&self.sort_menu_focus)
            .debug_selector(|| "sort-select".into())
            .aria_expanded(self.sort_open)
            .w(px(144.))
            .h(px(42.))
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(15.))
            .child(sort.label())
            .child(div().text_color(rgb(DIM)).child("▾"))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "up" | "down" | "arrowup" | "arrowdown" => {
                        cx.stop_propagation();
                        let index = SortKey::ALL
                            .iter()
                            .position(|key| *key == this.list.sort)
                            .unwrap_or(0);
                        let next = if matches!(event.keystroke.key.as_str(), "up" | "arrowup") {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(SortKey::ALL.len() - 1)
                        };
                        this.select_sort(SortKey::ALL[next], window, cx);
                    }
                    _ => {}
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.sort_menu_focus_subscription.is_none() {
                    let focus = this.sort_menu_focus.clone();
                    this.sort_menu_focus_subscription =
                        Some(cx.on_blur(&focus, window, |this, _, cx| {
                            if this.sort_open {
                                this.sort_open = false;
                                cx.notify();
                            }
                        }));
                }
                this.sort_open = !this.sort_open;
                cx.notify();
            }));
        if self.sort_open {
            let mut choices = div()
                .id("sort-choices")
                .role(gpui::Role::ListBox)
                .aria_label("Sort choices")
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .absolute()
                .top(px(42.))
                .left(px(0.))
                .w(px(144.))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .occlude()
                .flex()
                .flex_col();
            for key in SortKey::ALL {
                choices = choices.child(
                    button(format!("sort-{}", key.key()), key.label(), false)
                        .role(gpui::Role::ListBoxOption)
                        .aria_selected(key == sort)
                        .debug_selector(move || format!("sort-option-{}", key.key()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.select_sort(key, window, cx);
                        })),
                );
            }
            sort_select = sort_select.child(deferred(choices).priority(1));
        }
        let sort_menu = div()
            .relative()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(sort_select);
        sort_control = sort_control.child(sort_menu);
        toolbar = toolbar
            .child(sort_control)
            .child(
                button("sort-direction", if desc { "↓" } else { "↑" }, false)
                    .w(px(40.))
                    .h(px(42.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(BASE))
                    .aria_label(if desc {
                        "Descending, click for ascending"
                    } else {
                        "Ascending, click for descending"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.flip_sort(window, cx))),
            )
            .child(
                button(
                    "sync-list",
                    if self.list.syncing {
                        "Syncing…"
                    } else {
                        "⟳ Sync"
                    },
                    self.list.syncing || self.offline,
                )
                .w(px(80.))
                .h(px(42.))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(BASE))
                .text_color(rgb(ACCENT))
                .on_click(cx.listener(|this, _, window, cx| this.sync_list(window, cx))),
            );
        let title_count = self.list.len();
        let episodes = self.list.tracked_episodes();
        let visible_count = self.visible_ids.len();
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .px(px(if compact { 20. } else { 28. }))
            .pt(px(20.))
            .child(
                div()
                    .relative()
                    .w_full()
                    .min_h(px(95.))
                    .pb(px(12.))
                    .border_b_1()
                    .border_color(rgb(EDGE))
                    .mb(px(18.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .py(px(2.))
                            .child(
                                tracked_mono("YOUR COLLECTION", 13., 13. * 0.14)
                                    .line_height(px(19.5))
                                    .text_color(rgb(DIM)),
                            )
                            .child(
                                div()
                                    .id("my-list-heading")
                                    .debug_selector(|| "my-list-heading".into())
                                    .role(gpui::Role::Heading)
                                    .aria_level(1)
                                    .aria_label("My List")
                                    .flex()
                                    .items_center()
                                    .font(serif_font())
                                    .font_weight(FontWeight::NORMAL)
                                    .text_size(px(30.))
                                    .line_height(px(34.5))
                                    .my(px(5.))
                                    .child("My List")
                                    .child(div().text_color(rgb(ACCENT)).child(".")),
                            )
                            .child(
                                div()
                                    .text_color(rgb(DIM))
                                    .text_size(px(13.))
                                    .line_height(px(18.2))
                                    .child(format!(
                                        "{} Titles · {} Episodes Tracked",
                                        grouped_count(title_count),
                                        grouped_count(episodes)
                                    )),
                            ),
                    )
                    .child(
                        svg()
                            .path("icons/gearwork.svg")
                            .absolute()
                            .right(px(0.))
                            .top(px(if compact { 4. } else { 0. }))
                            .w(px(if compact { 125. } else { 145. }))
                            .h(px(if compact { 73.5 } else { 85.3 }))
                            .opacity(if compact { 0.75 } else { 0.85 })
                            .text_color(rgb(ACCENT)),
                    )
                    .child(
                        div()
                            .absolute()
                            .right(px(0.))
                            .bottom(px(0.))
                            .font(mono_font())
                            .text_size(px(14.))
                            .text_color(rgb(RUST))
                            .child("＋"),
                    ),
            )
            .child(toolbar);
        if !self.list.error.is_empty() {
            content = content.child(
                div()
                    .border_1()
                    .border_color(rgb(0xab5d55).opacity(0.3))
                    .bg(rgb(0xab5d55).opacity(0.1))
                    .rounded(px(3.))
                    .text_color(rgb(ERROR))
                    .p(px(8.))
                    .mb(px(16.))
                    .child(self.list.error.clone()),
            );
        }
        if self.offline {
            content = content.child(
                div()
                    .border_1()
                    .border_color(rgb(EDGE))
                    .p(px(10.))
                    .mb(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child("Showing your saved list offline.")
                    .child(
                        button(
                            "reconnect-list",
                            if self.reconnect_busy {
                                "Connecting…"
                            } else {
                                "Reconnect"
                            },
                            self.reconnect_busy,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.reconnect_account(true, window, cx)
                        })),
                    )
                    .when(!self.auth_error.is_empty(), |row| {
                        row.child(div().text_color(rgb(ERROR)).child(self.auth_error.clone()))
                    }),
            );
        }
        if !self.list.step_error.is_empty() {
            let mut warning = div()
                .border_1()
                .border_color(rgb(0xab5d55).opacity(0.3))
                .bg(rgb(0xab5d55).opacity(0.1))
                .rounded(px(3.))
                .text_color(rgb(ERROR))
                .p(px(8.))
                .mb(px(16.))
                .child(format!(
                    "Episode update needs attention: {}",
                    self.list.step_error
                ));
            if let Some((id, remote)) = self.progress.first_review(self.generation) {
                let title = self
                    .list
                    .entry(id)
                    .and_then(|entry| entry.media.as_ref())
                    .map(|media| media.display_title())
                    .unwrap_or_else(|| format!("Anime #{id}"));
                warning = warning
                    .child(format!(
                        "{title}: {}",
                        remote.map_or_else(
                            || "no longer on your list".into(),
                            |progress| format!("latest progress is {progress}")
                        )
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(
                                button(format!("keep-latest-{id}"), "Keep latest", false).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        this.resolve_progress_review(id, false, window, cx)
                                    }),
                                ),
                            )
                            .when(remote.is_some(), |actions| {
                                actions.child(
                                    button(format!("use-my-progress-{id}"), "Use my edit", false)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.resolve_progress_review(id, true, window, cx)
                                        })),
                                )
                            }),
                    );
            } else {
                warning = warning.child(button("retry-progress", "Retry", false).on_click(
                    cx.listener(|this, _, window, cx| this.retry_progress_errors(window, cx)),
                ));
            }
            content = content.child(warning);
        }
        content = content.child(tabs);
        if self.list.loading && !self.list.loaded {
            content = content.child(
                div()
                    .py(px(40.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(DIM))
                    .child("Loading…"),
            );
        } else if visible_count == 0 {
            let searching = !self.list.query.trim().is_empty();
            let mut empty = div()
                .w_full()
                .border_b_1()
                .border_color(rgb(EDGE))
                .py(px(50.))
                .px(px(15.))
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(eyebrow(if searching {
                    "NO MATCHING TITLES"
                } else {
                    "A LITTLE ROOM ON THE SHELF"
                }))
                .child(
                    div()
                        .mt(px(13.))
                        .font(serif_font())
                        .text_size(px(24.))
                        .whitespace_normal()
                        .child(if searching {
                            "Try another title."
                        } else {
                            "Your next story is still out there."
                        }),
                );
            if !searching {
                empty = empty.child(
                    div()
                        .id("find-something-to-watch")
                        .role(gpui::Role::Button)
                        .aria_label("Find something to watch")
                        .focusable()
                        .tab_stop(true)
                        .cursor_pointer()
                        .mt(px(20.))
                        .text_color(rgb(ACCENT))
                        .child("Find something to watch ↗")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.navigate(Route::Search, window, cx)
                        })),
                );
            }
            content = content.child(empty);
        } else {
            content = content
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .pl(px(38.))
                        .pr(px(16.))
                        .pt(px(4.))
                        .pb(px(12.))
                        .text_color(rgb(DIM))
                        .text_size(px(12.))
                        .child(tracked_mono(
                            format!("TITLE / {}", status_label(&self.list.filter).to_uppercase()),
                            12.,
                            12. * 0.17,
                        ))
                        .child(tracked_mono("EPISODE PROGRESS", 12., 12. * 0.17)),
                )
                .child(div().w_full().border_t_1().border_color(rgb(EDGE)));
        }
        content.into_any()
    }

    fn render_my_list_footer(&self, compact: bool) -> AnyElement {
        let sync_label = sync_label(
            self.list.synced_at,
            super::now_seconds(),
            self.list_sync_flash.is_some(),
        );
        div()
            .id("my-list-footer")
            .role(gpui::Role::Footer)
            .aria_label("List summary")
            .debug_selector(|| "my-list-footer".into())
            .w_full()
            .flex()
            .flex_wrap()
            .gap(px(12.))
            .justify_between()
            .px(px(if compact { 20. } else { 28. }))
            .pt(px(18.))
            .pb(px(38.))
            .text_color(rgb(DIM))
            .child(tracked_mono(
                format!(
                    "{} {} / {}",
                    self.visible_ids.len(),
                    if self.visible_ids.len() == 1 {
                        "title"
                    } else {
                        "titles"
                    },
                    status_label(&self.list.filter)
                )
                .to_uppercase(),
                12.,
                12. * 0.06,
            ))
            .child(
                tracked_mono(sync_label.to_uppercase(), 12., 12. * 0.06)
                    // Repeated completions need a new live node even when the text matches.
                    .id(("my-list-sync-status", self.list_sync_flash.unwrap_or(0)))
                    .role(gpui::Role::Status)
                    .aria_label(sync_label)
                    .a11y_synthetic_children(|builder| {
                        builder
                            .parent_node()
                            .set_live(gpui::accesskit::Live::Polite);
                    }),
            )
            .into_any()
    }
}

fn render_list_row(
    index: usize,
    entry: ListEntry,
    pending: Option<super::state::ProgressEdit>,
    cover: Option<Arc<RenderImage>>,
    score_format: Option<&str>,
    compact: bool,
    weak: WeakEntity<Workspace>,
) -> AnyElement {
    let id = entry.media_id;
    let media = entry.media.as_ref();
    let title = media
        .map(|m| m.display_title())
        .unwrap_or_else(|| format!("#{id}"));
    let mut meta = Vec::new();
    if let Some(format) = media.and_then(|m| m.format.as_ref()) {
        meta.push((format.replace('_', " "), DIM));
    }
    if let Some(year) = media.and_then(|m| m.season_year) {
        meta.push((year.to_string(), DIM));
    }
    if let Some(score) = entry.score.filter(|score| *score > 0.) {
        let label = match score_format {
            Some("POINT_3") => ["", "😞", "😐", "😊"]
                .get(score.round() as usize)
                .copied()
                .map(str::to_owned)
                .unwrap_or_else(|| score.to_string()),
            Some("POINT_5") => "★".repeat(score.round().clamp(0., 5.) as usize),
            _ => format!("★ {score}"),
        };
        meta.push((label, OCHRE));
    }
    let now = super::now_seconds();
    if let Some(at) = media.and_then(|m| m.next_airing_at).filter(|at| *at > now)
        && let Some(ep) = media
            .and_then(|m| m.next_airing_episode)
            .filter(|ep| *ep > 0)
    {
        let seconds = at - now;
        let label = if seconds < 3600 {
            format!("Ep {ep} airs in {}m", ((seconds + 30) / 60).max(1))
        } else if seconds < 86400 {
            format!("Ep {ep} airs in {}h", (seconds + 3599) / 3600)
        } else if seconds < 7 * 86400 {
            format!("Ep {ep} airs in {}d", (seconds + 86399) / 86400)
        } else {
            chrono::DateTime::from_timestamp(at, 0).map_or_else(
                || format!("Ep {ep} airs in {}d", (seconds + 86399) / 86400),
                |date| {
                    format!(
                        "Ep {ep} airs {}",
                        date.with_timezone(&chrono::Local).format("%b %-d")
                    )
                },
            )
        };
        meta.push((label, BLUE));
    }
    let progress = pending.as_ref().map_or(entry.progress, |edit| edit.pending);
    let total = media.and_then(|m| m.episodes);
    let saving = pending.as_ref().is_some_and(|edit| edit.in_flight);
    let deleting = pending.as_ref().is_some_and(|edit| edit.deleting);
    let at_min = progress <= 0;
    let at_max = progress >= total.unwrap_or(9999);
    let cover_view: AnyElement = if let Some(image) = cover {
        img(image)
            .size_full()
            .object_fit(gpui::ObjectFit::Cover)
            .into_any()
    } else {
        div()
            .size_full()
            .bg(rgb(PANEL_2))
            .text_color(rgb(SAGE))
            .text_size(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .child("ク")
            .into_any()
    };
    let weak_cover = weak.clone();
    let weak_less = weak.clone();
    let weak_more = weak.clone();
    let weak_row = weak.clone();
    let weak_keys = weak.clone();
    let weak_nav = weak.clone();
    let mut meta_row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.))
        .gap_y(px(4.))
        .mt(px(7.))
        .font(mono_font())
        .text_size(px(13.));
    for (index, (label, color)) in meta.into_iter().enumerate() {
        let mut item = div().flex().items_baseline().text_color(rgb(color));
        if index > 0 {
            item = item.child(div().mr(px(8.)).text_color(rgb(DIM)).child("/"));
        }
        meta_row = meta_row.child(item.child(label));
    }
    let mut stepper = div()
        .id(format!("stepper-{id}"))
        .debug_selector(move || format!("stepper-{id}"))
        .focusable()
        .tab_stop(true)
        .on_key_down(move |event, window, app| {
            let delta = match event.keystroke.key.as_str() {
                "left" | "down" | "arrowleft" | "arrowdown" => -1,
                "right" | "up" | "arrowright" | "arrowup" => 1,
                _ => return,
            };
            app.stop_propagation();
            if saving || deleting || (delta < 0 && at_min) || (delta > 0 && at_max) {
                return;
            }
            let _ = weak_keys.update(app, |this, cx| this.step_progress(id, delta, window, cx));
        })
        .flex()
        .items_center()
        .gap(px(4.));
    let less_disabled = at_min || saving || deleting;
    let more_disabled = at_max || saving || deleting;
    let mut progress_value = div()
        .w(px(44.))
        .text_size(px(14.))
        .flex()
        .items_baseline()
        .justify_center()
        .text_color(rgb(if pending.as_ref().is_some_and(|edit| edit.dirty()) {
            ACCENT
        } else {
            INK
        }))
        .child(progress.to_string());
    if let Some(total) = total.filter(|total| *total > 0) {
        progress_value =
            progress_value.child(div().text_color(rgb(DIM)).child(format!("/{total}")));
    }
    stepper = stepper
        .child(
            div()
                .w(px(12.))
                .flex_shrink_0()
                .text_size(px(10.))
                .flex()
                .justify_center()
                .text_color(rgb(
                    if pending.as_ref().is_some_and(|edit| edit.error.is_some()) {
                        ERROR
                    } else {
                        ACCENT
                    },
                ))
                .child(if saving {
                    "…"
                } else if pending.as_ref().is_some_and(|edit| edit.error.is_some()) {
                    "!"
                } else if pending.as_ref().is_some_and(|edit| edit.dirty()) {
                    "●"
                } else {
                    ""
                }),
        )
        .child(
            button(format!("less-{id}"), "−", less_disabled)
                .w(px(24.))
                .h(px(24.))
                .px(px(0.))
                .py(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .bg(rgb(0x221a16))
                .border_color(rgb(0x221a16))
                .opacity(if less_disabled { 0.3 } else { 1. })
                .aria_label("One less episode")
                .on_click(move |_, window, app| {
                    app.stop_propagation();
                    if less_disabled {
                        return;
                    }
                    let _ =
                        weak_less.update(app, |this, cx| this.step_progress(id, -1, window, cx));
                }),
        )
        .child(progress_value)
        .child(
            button(format!("more-{id}"), "+", more_disabled)
                .w(px(24.))
                .h(px(24.))
                .px(px(0.))
                .py(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .bg(rgb(0x221a16))
                .border_color(rgb(0x221a16))
                .opacity(if more_disabled { 0.3 } else { 1. })
                .aria_label("One more episode")
                .on_click(move |_, window, app| {
                    app.stop_propagation();
                    if more_disabled {
                        return;
                    }
                    let _ = weak_more.update(app, |this, cx| this.step_progress(id, 1, window, cx));
                }),
        );
    let progress_width = if compact { 145. } else { 150. };
    let mut right = div()
        .debug_selector(move || format!("row-progress-{id}"))
        .w(px(progress_width))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(4.))
        .child(stepper);
    if let Some(total) = total.filter(|total| *total > 0) {
        right = right.child(
            div()
                .w(px(progress_width - 16.))
                .h(px(2.))
                .ml(px(16.))
                .mt(px(5.))
                .bg(rgb(EDGE))
                .child(
                    div()
                        .w(gpui::relative(
                            (progress as f32 / total as f32).clamp(0., 1.),
                        ))
                        .h_full()
                        .bg(rgb(tab_color(&entry.status))),
                ),
        );
    }
    div()
        .id(format!("media-row-{id}"))
        .debug_selector(move || format!("media-row-{id}"))
        .role(gpui::Role::Button)
        .aria_label(format!("Edit {title}"))
        .w_full()
        .min_h(px(90.))
        .border_b_1()
        .border_color(rgb(EDGE))
        .flex()
        .items_center()
        .gap(px(if compact { 10. } else { 15. }))
        .py(px(11.))
        .pr(px(13.))
        .hover(|style| style.bg(rgb(PANEL)))
        .cursor_pointer()
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.bg(rgb(PANEL)).border_color(rgb(ACCENT)))
        .on_key_down(move |event: &gpui::KeyDownEvent, window, app| {
            let jump_end = match event.keystroke.key.as_str() {
                "home" => false,
                "end" => true,
                _ => return,
            };
            app.stop_propagation();
            let _ = weak_nav.update(app, |this, cx| {
                if this.route != Route::MyList || this.modal_open() {
                    return;
                }
                if jump_end {
                    this.list_widget.scroll_to_end();
                    let count = this.visible_ids.len();
                    this.prefetch_covers(count..count, window, cx);
                } else {
                    this.list_widget.scroll_to(gpui::ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.),
                    });
                    this.prefetch_covers(0..32, window, cx);
                }
                cx.notify();
            });
        })
        .on_click(move |_, window, app| {
            let _ = weak_row.update(app, |this, cx| this.open_editor(id, window, cx));
        })
        .child(
            div()
                .w(px(if compact { 18. } else { 23. }))
                .flex_shrink_0()
                .font(mono_font())
                .text_color(rgb(DIM))
                .text_size(px(12.))
                .text_center()
                .child(format!("{:02}", index + 1)),
        )
        .child(
            div()
                .id(format!("cover-{id}"))
                .debug_selector(move || format!("cover-{id}"))
                .w(px(46.))
                .h(px(63.))
                .flex_shrink_0()
                .overflow_hidden()
                .border_1()
                .border_color(rgb(EDGE))
                .role(gpui::Role::Button)
                .aria_label(format!("Open details for {title}"))
                .focusable()
                .tab_stop(true)
                .focus_visible(|style| style.border_color(rgb(ACCENT)))
                .cursor_pointer()
                .on_click(move |_, window, app| {
                    app.stop_propagation();
                    let _ = weak_cover
                        .update(app, |this, cx| this.navigate(Route::Anime(id), window, cx));
                })
                .child(cover_view),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_weight(FontWeight::NORMAL)
                        .text_size(px(15.))
                        .line_height(px(21.))
                        .whitespace_normal()
                        .child(title),
                )
                .child(meta_row),
        )
        .child(right)
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Focusable, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, TestAppContext,
        VisualTestContext,
    };
    use kurisu_core::models::{Media, User};
    use kurisu_core::{Core, Platform, UpdateChannel};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sync_status_uses_production_age_boundaries() {
        let at = 1_700_000_000;
        assert_eq!(sync_label(None, at, false), "Cached on this device");
        assert_eq!(sync_label(Some(0), at, false), "Cached on this device");
        assert_eq!(sync_label(Some(at), at, true), "Synced ✓");
        for (elapsed, expected) in [
            (-1, "Synced just now"),
            (59, "Synced just now"),
            (60, "Synced 1m ago"),
            (3599, "Synced 59m ago"),
            (3600, "Synced 1h ago"),
            (86400, "Synced 1d ago"),
            (604799, "Synced 6d ago"),
        ] {
            assert_eq!(sync_label(Some(at), at + elapsed, false), expected);
        }
    }

    struct TestPlatform;

    impl Platform for TestPlatform {
        fn open_url(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn reveal_path(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    struct FixtureDir(PathBuf);

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Core, tokio::runtime::Runtime, FixtureDir) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kurisu-native-list-select-{}-{nonce}",
            std::process::id()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::open(
            path.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        (core, runtime, FixtureDir(path))
    }

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

    #[gpui::test]
    fn search_label_fills_toolbar_and_focuses_from_icon(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.loaded = true;
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1100.), px(750.)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        let search = cx.debug_bounds("my-list-search").unwrap();
        let icon = cx.debug_bounds("my-list-search-icon").unwrap();
        let sort = cx.debug_bounds("sort-select").unwrap();
        assert_eq!(search.size.height, px(45.));
        assert!(search.size.width > px(420.));
        assert!(icon.left() >= search.left() && icon.right() <= search.right());
        assert!(search.right() < sort.left());
        cx.simulate_click(icon.center(), Modifiers::default());
        assert!(workspace.update_in(cx, |workspace, window, cx| {
            workspace
                .list_query
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
        }));

        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        let compact = cx.debug_bounds("my-list-search").unwrap();
        let compact_sort = cx.debug_bounds("sort-select").unwrap();
        assert!(compact.size.width >= px(160.));
        assert!(compact.right() <= px(800.));
        assert!(compact.right() <= compact_sort.left() || compact.bottom() <= compact_sort.top());
    }

    #[gpui::test]
    fn row_cover_and_progress_keep_production_columns_when_compact(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.replace(
                vec![ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    progress: 3,
                    media: Some(Media {
                        id: 9,
                        title_english: Some("Synthetic Journey Through Many Connected Episodes And Several More Stories ".repeat(3)),
                        episodes: Some(24),
                        format: Some("TV".into()),
                        season_year: Some(2026),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                super::super::now_seconds(),
            );
            workspace.reset_list_widget();
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(1100.), px(750.)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        let row = cx.debug_bounds("media-row-9").unwrap();
        let cover = cx.debug_bounds("cover-9").unwrap();
        let progress = cx.debug_bounds("row-progress-9").unwrap();
        assert_eq!(cover.size, gpui::size(px(46.), px(63.)));
        assert_eq!(progress.size.width, px(150.));
        assert!(row.left() < cover.left() && cover.right() < progress.left());
        assert!(progress.right() <= row.right());

        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        let compact_row = cx.debug_bounds("media-row-9").unwrap();
        let compact_progress = cx.debug_bounds("row-progress-9").unwrap();
        assert_eq!(compact_progress.size.width, px(145.));
        assert!(compact_row.size.height > px(90.));
        assert!(compact_progress.right() <= compact_row.right());
    }

    #[gpui::test]
    fn sort_is_between_query_and_direction_in_both_tab_directions(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.loaded = true;
            cx.notify();
        });
        cx.run_until_parked();
        let query = workspace.read_with(cx, |workspace, _| workspace.list_query.clone());
        query.update_in(cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx)
        });
        cx.update(|window, app| window.draw(app).clear(app));
        workspace.update_in(cx, |_, window, cx| window.focus_next(cx));
        assert!(workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));
        cx.update(|window, app| window.draw(app).clear(app));
        workspace.update_in(cx, |_, window, cx| window.focus_next(cx));
        assert!(!workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));
        let before = workspace.read_with(cx, |workspace, _| workspace.list.desc);
        press(cx, "enter");
        assert_ne!(
            workspace.read_with(cx, |workspace, _| workspace.list.desc),
            before
        );
        workspace.update_in(cx, |_, window, cx| window.focus_prev(cx));
        assert!(workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));
    }

    #[gpui::test]
    fn sort_select_keyboard_and_outside_pointer_have_one_activation_path(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.loaded = true;
            workspace.route = Route::MyList;
            cx.notify();
        });
        cx.run_until_parked();
        let sort = cx.debug_bounds("sort-select").unwrap();
        cx.simulate_click(sort.center(), Modifiers::default());
        cx.update(|window, app| window.draw(app).clear(app));
        assert!(workspace.read_with(cx, |workspace, _| workspace.sort_open));
        assert!(workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));

        press(cx, "enter");
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));
        press(cx, "space");
        assert!(workspace.read_with(cx, |workspace, _| workspace.sort_open));
        assert!(workspace.read_with(cx, |workspace, _| {
            workspace.sort_menu_focus_subscription.is_some()
        }));
        press(cx, "escape");
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));
        assert!(workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));
        press(cx, "space");
        assert!(workspace.read_with(cx, |workspace, _| workspace.sort_open));
        cx.update(|window, app| window.draw(app).clear(app));

        let score = cx.debug_bounds("sort-option-score").unwrap();
        cx.simulate_click(score.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.list.sort),
            SortKey::Score
        );
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));

        cx.simulate_click(sort.center(), Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| workspace.sort_open));
        let heading = cx.debug_bounds("my-list-heading").unwrap();
        cx.simulate_click(heading.center(), Modifiers::default());
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));

        cx.simulate_click(sort.center(), Modifiers::default());
        workspace.update_in(cx, |_, window, cx| window.focus_next(cx));
        cx.update(|window, app| window.draw(app).clear(app));
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));

        cx.simulate_click(sort.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        assert!(workspace.update_in(cx, |workspace, window, _| {
            workspace.sort_menu_focus.is_focused(window)
        }));
        let query = workspace.read_with(cx, |workspace, _| workspace.list_query.clone());
        query.update_in(cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx)
        });
        cx.update(|window, app| window.draw(app).clear(app));
        cx.update(|window, app| window.draw(app).clear(app));
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));

        cx.simulate_click(sort.center(), Modifiers::default());
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.select_filter("PLANNING", window, cx)
        });
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));

        cx.simulate_click(sort.center(), Modifiers::default());
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.route = Route::Settings;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!workspace.read_with(cx, |workspace, _| workspace.sort_open));
    }
}
