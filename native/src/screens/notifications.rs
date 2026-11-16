use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, InteractiveElement, ListAlignment, ListState, RenderImage, WeakEntity,
    Window, div, img, list, prelude::*, px, rgb,
};
use kurisu_core::{Platform, models::Notification};
use reqwest::Url;

use super::{Workspace, covers::CoverOwner, joined};
use crate::{
    components::{button, page_heading},
    contracts::Route,
    platform::NativePlatform,
    theme::{ACCENT, DIM, EDGE, ERROR, INK, PANEL, PANEL_2},
};

const NOTIFICATION_ROW_HEIGHT: f32 = 84.;

pub(super) struct NotificationsState {
    items: Arc<Vec<Notification>>,
    widget: Option<ListState>,
    loaded: bool,
    loading: bool,
    marking: bool,
    error: String,
    unread: Option<i64>,
    page: i64,
    has_more: bool,
    request: u64,
}

impl Default for NotificationsState {
    fn default() -> Self {
        Self {
            items: Arc::new(Vec::new()),
            widget: None,
            loaded: false,
            loading: false,
            marking: false,
            error: String::new(),
            unread: None,
            page: 0,
            has_more: false,
            request: 0,
        }
    }
}

impl NotificationsState {
    fn replace_items(&mut self, items: Vec<Notification>) {
        if let Some(widget) = &self.widget {
            let mut offset = widget.logical_scroll_top();
            let at_end = widget.item_count() > 0 && offset.item_ix >= widget.item_count();
            let anchor = self.items.get(offset.item_ix).map(|item| item.id);
            if let Some(position) =
                anchor.and_then(|id| items.iter().position(|item| item.id == id))
            {
                offset.item_ix = position;
            } else {
                offset = gpui::ListOffset::default();
            }
            let old_count = self.items.len();
            if items.len() >= old_count
                && self
                    .items
                    .iter()
                    .zip(&items)
                    .all(|(old, new)| old.id == new.id)
            {
                if items.len() > old_count {
                    widget.splice(old_count..old_count, items.len() - old_count);
                    widget
                        .clone()
                        .with_uniform_item_height(px(NOTIFICATION_ROW_HEIGHT));
                } else {
                    widget.remeasure_items(0..items.len());
                }
            } else {
                widget.reset_with_uniform_height(items.len(), px(NOTIFICATION_ROW_HEIGHT));
            }
            if at_end {
                widget.scroll_to_end();
            } else {
                widget.scroll_to(offset);
            }
        }
        self.items = Arc::new(items);
    }
}

impl Workspace {
    pub(super) fn enter_notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.notifications.widget.is_none() {
            let widget = ListState::new(0, ListAlignment::Top, px(800.))
                .with_uniform_item_height(px(NOTIFICATION_ROW_HEIGHT));
            let weak = cx.weak_entity();
            widget.set_scroll_handler(move |range, _, app| {
                let _ = weak.update(app, |this, cx| {
                    this.prefetch_notification_covers(range.visible_range.clone());
                    cx.notify();
                });
            });
            self.notifications.widget = Some(widget);
        }
        if !self.notifications.loading {
            self.load_notifications(false, window, cx);
        }
        self.refresh_notifications_covers();
    }

    pub(super) fn refresh_notifications_covers(&mut self) {
        let start = self
            .notifications
            .widget
            .as_ref()
            .map_or(0, |widget| widget.logical_scroll_top().item_ix);
        self.prefetch_notification_covers(start..start.saturating_add(24));
    }

    fn prefetch_notification_covers(&mut self, range: std::ops::Range<usize>) {
        let start = range.start.saturating_sub(4);
        let end = range
            .end
            .saturating_add(8)
            .min(self.notifications.items.len());
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(end)..end {
            let item = &self.notifications.items[index];
            if let Some(url) = item.user_avatar.as_deref().or(item.media_cover.as_deref()) {
                let cover = ((cover_id(item.id), 32, 44), url.to_owned());
                if range.contains(&index) {
                    visible.push(cover);
                } else {
                    nearby.push(cover);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Notifications), visible, nearby);
    }

    fn load_notifications(&mut self, more: bool, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.notifications.loading || self.notifications.marking {
            return;
        }
        if self.offline {
            self.notifications.error = "Notifications require an AniList connection.".into();
            cx.notify();
            return;
        }
        self.notifications.loading = true;
        self.notifications.error.clear();
        self.notifications.request = self.notifications.request.wrapping_add(1);
        let request = self.notifications.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let page = if more { self.notifications.page + 1 } else { 1 };
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_notifications_page(page).await });
        self.replace_reads("notifications-page", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.notifications.request != request
                {
                    return;
                }
                this.notifications.loading = false;
                match result {
                    Ok(page) => {
                        let items = merge_page(&this.notifications.items, page.items, more);
                        this.notifications.replace_items(items);
                        this.notifications.page = page.page;
                        this.notifications.has_more = page.has_next_page;
                        this.notifications.unread = page.unread_count;
                        this.notifications.loaded = true;
                        this.refresh_notifications_covers();
                    }
                    Err(error) => this.notifications.error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn mark_notifications_read(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none()
            || self.offline
            || self.notifications.loading
            || self.notifications.marking
            || self.notifications.unread == Some(0)
        {
            return;
        }
        self.notifications.marking = true;
        self.notifications.error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move { worker.mark_notifications_read().await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    this.notifications.marking = false;
                    match result {
                        Ok(()) => this.notifications.unread = Some(0),
                        Err(error) => this.notifications.error = error,
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_notification(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.notifications.items.iter().find(|item| item.id == id) else {
            return;
        };
        if let Some(media_id) = item.media_id {
            self.navigate(Route::Anime(media_id), window, cx);
            return;
        }
        let url = notification_url(item);
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
                    this.notifications.error = error;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn render_notifications(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = &self.notifications;
        let disabled = state.loading || state.marking || self.offline;
        let mut content = div().flex_1().min_h_0().flex().flex_col().p(px(28.)).child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .min_h(px(92.))
                .mb(px(18.))
                .pr(px(155.))
                .pb(px(12.))
                .border_b_1()
                .border_color(rgb(EDGE))
                .child(
                    div()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(ACCENT))
                                .child("ANILIST UPDATES"),
                        )
                        .child(page_heading("Notifications"))
                        .child(
                            div()
                                .text_color(rgb(DIM))
                                .child("The latest from your account"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            button(
                                "notifications-refresh",
                                if state.loading {
                                    "Loading…"
                                } else {
                                    "↻ Refresh"
                                },
                                disabled,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    if !this.notifications.loading
                                        && !this.notifications.marking
                                        && !this.offline
                                    {
                                        this.load_notifications(false, window, cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            button(
                                "notifications-mark-read",
                                if state.marking {
                                    "Marking…"
                                } else {
                                    "Mark all read"
                                },
                                disabled || state.unread == Some(0),
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| this.mark_notifications_read(window, cx),
                            )),
                        ),
                ),
        );
        if let Some(unread) = state.unread {
            content = content.child(
                div()
                    .mb(px(12.))
                    .text_color(rgb(DIM))
                    .child(format!("{unread} unread on AniList")),
            );
        }
        if !state.error.is_empty() {
            content = content.child(
                div()
                    .id("notifications-error")
                    .role(gpui::Role::Alert)
                    .mb(px(12.))
                    .p(px(8.))
                    .border_1()
                    .border_color(rgb(0x5e2d29))
                    .bg(rgb(0x1a0b09))
                    .text_color(rgb(ERROR))
                    .child(state.error.clone()),
            );
        }
        if state.loading && state.items.is_empty() {
            content = content.child(
                div()
                    .py(px(40.))
                    .text_center()
                    .text_color(rgb(DIM))
                    .child("Loading…"),
            );
        } else if state.items.is_empty() && state.error.is_empty() {
            content = content.child(
                div()
                    .py(px(40.))
                    .text_center()
                    .text_color(rgb(DIM))
                    .child("No notifications."),
            );
        } else if let Some(widget) = &state.widget {
            let items = state.items.clone();
            let weak = cx.weak_entity();
            content = content.child(
                list(widget.clone(), move |index, _, app| {
                    let Some(item) = items.get(index).cloned() else {
                        return div().into_any();
                    };
                    let cover = weak
                        .read_with(app, |this, _| {
                            this.cover_images.get(&(cover_id(item.id), 32, 44)).cloned()
                        })
                        .ok()
                        .flatten();
                    render_notification_row(item, cover, weak.clone())
                })
                .flex_1()
                .min_h_0(),
            );
        }
        if state.has_more {
            content = content.child(
                div().pt(px(12.)).child(
                    button(
                        "notifications-more",
                        if state.loading {
                            "Loading…"
                        } else {
                            "Load older notifications"
                        },
                        disabled,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.notifications.loading
                            && !this.notifications.marking
                            && !this.offline
                        {
                            this.load_notifications(true, window, cx);
                        }
                    })),
                ),
            );
        }
        content.into_any()
    }
}

fn cover_id(id: i64) -> i64 {
    -500_000 - id
}

fn merge_page(previous: &[Notification], next: Vec<Notification>, more: bool) -> Vec<Notification> {
    let mut merged = if more { previous.to_vec() } else { Vec::new() };
    let mut index: HashMap<i64, usize> = merged
        .iter()
        .enumerate()
        .map(|(index, item)| (item.id, index))
        .collect();
    for item in next {
        if let Some(position) = index.get(&item.id).copied() {
            merged[position] = item;
        } else {
            index.insert(item.id, merged.len());
            merged.push(item);
        }
    }
    merged
}

fn render_notification_row(
    item: Notification,
    cover: Option<Arc<RenderImage>>,
    weak: WeakEntity<Workspace>,
) -> AnyElement {
    let id = item.id;
    let mut row = div()
        .id(format!("notification-{id}"))
        .role(gpui::Role::Button)
        .aria_label(notification_text(&item))
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_color(rgb(ACCENT)))
        .w_full()
        .flex()
        .items_start()
        .gap(px(12.))
        .p(px(12.))
        .mb(px(6.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(EDGE))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(PANEL_2)))
        .on_click(move |_, window, app| {
            let _ = weak.update(app, |this, cx| this.open_notification(id, window, cx));
        })
        .child(
            div()
                .text_size(px(18.))
                .child(notification_icon(&item.kind)),
        );
    if let Some(image) = cover {
        row = row.child(img(image).w(px(32.)).h(px(44.)));
    }
    row.child(
        div()
            .min_w_0()
            .flex_1()
            .child(div().text_color(rgb(INK)).child(notification_text(&item)))
            .when_some(item.reason.clone(), |row, reason| {
                row.child(
                    div()
                        .mt(px(3.))
                        .text_size(px(12.))
                        .text_color(rgb(DIM))
                        .child(reason),
                )
            })
            .child(
                div()
                    .mt(px(5.))
                    .text_size(px(12.))
                    .text_color(rgb(DIM))
                    .child(time_ago(item.created_at)),
            ),
    )
    .into_any()
}

fn notification_text(item: &Notification) -> String {
    let user = item.user_name.as_deref().unwrap_or("Someone");
    let media = item.media_title.as_deref().unwrap_or("A media");
    let thread = item
        .thread_title
        .as_deref()
        .map(|title| format!("“{title}”"))
        .unwrap_or_else(|| "a thread".into());
    match item.kind.to_ascii_uppercase().as_str() {
        "AIRING" => format!(
            "Episode {} of {media} aired.",
            item.episode
                .map_or("?".into(), |episode| episode.to_string())
        ),
        "FOLLOWING" => format!("{user} started following you."),
        "ACTIVITY_MESSAGE" => format!("{user} sent you a message."),
        "ACTIVITY_MENTION" => format!("{user} mentioned you in an activity."),
        "ACTIVITY_REPLY" => format!("{user} replied to your activity."),
        "ACTIVITY_REPLY_SUBSCRIBED" => {
            format!("{user} replied in an activity you're following.")
        }
        "ACTIVITY_LIKE" => format!("{user} liked your activity."),
        "ACTIVITY_REPLY_LIKE" => format!("{user} liked your reply."),
        "THREAD_COMMENT_MENTION" => format!("{user} mentioned you in {thread}."),
        "THREAD_COMMENT_REPLY" => format!("{user} replied to you in {thread}."),
        "THREAD_COMMENT_SUBSCRIBED" => format!("{user} commented in {thread}."),
        "THREAD_COMMENT_LIKE" => format!("{user} liked your comment in {thread}."),
        "THREAD_LIKE" => format!(
            "{user} liked your thread{}.",
            item.thread_title
                .as_ref()
                .map_or(String::new(), |_| format!(" {thread}"))
        ),
        "RELATED_MEDIA_ADDITION" => format!("{media} was added to AniList."),
        "MEDIA_DATA_CHANGE" => format!("{media} data was changed."),
        "MEDIA_MERGE" => format!("{media} was merged."),
        "MEDIA_DELETION" => format!(
            "{} was deleted.",
            item.deleted_media_title.as_deref().unwrap_or("A media")
        ),
        _ => item
            .context
            .clone()
            .unwrap_or_else(|| item.kind.replace('_', " ").to_ascii_lowercase()),
    }
}

fn notification_icon(kind: &str) -> &'static str {
    match kind.to_ascii_uppercase().as_str() {
        "AIRING"
        | "RELATED_MEDIA_ADDITION"
        | "MEDIA_DATA_CHANGE"
        | "MEDIA_MERGE"
        | "MEDIA_DELETION" => "📺",
        "FOLLOWING" => "👤",
        "ACTIVITY_LIKE" | "ACTIVITY_REPLY_LIKE" | "THREAD_LIKE" | "THREAD_COMMENT_LIKE" => "❤",
        "ACTIVITY_MESSAGE" => "✉",
        "ACTIVITY_MENTION"
        | "ACTIVITY_REPLY"
        | "ACTIVITY_REPLY_SUBSCRIBED"
        | "THREAD_COMMENT_MENTION"
        | "THREAD_COMMENT_REPLY"
        | "THREAD_COMMENT_SUBSCRIBED" => "💬",
        _ => "🔔",
    }
}

fn notification_url(item: &Notification) -> String {
    if let Some(id) = item.activity_id {
        return format!("https://anilist.co/activity/{id}");
    }
    if let Some(raw) = &item.comment_url
        && let Ok(url) = Url::parse(raw)
        && url.scheme() == "https"
        && url.host_str() == Some("anilist.co")
        && url.username().is_empty()
        && url.password().is_none()
    {
        return url.to_string();
    }
    if let Some(id) = item.thread_id {
        return format!("https://anilist.co/forum/thread/{id}");
    }
    if let Some(name) = &item.user_name
        && let Ok(mut url) = Url::parse("https://anilist.co/user/")
    {
        if let Ok(mut path) = url.path_segments_mut() {
            path.push(name);
        }
        return url.to_string();
    }
    "https://anilist.co/notifications".into()
}

fn time_ago(timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return String::new();
    };
    let elapsed = Utc::now().timestamp() - timestamp;
    if elapsed < 60 {
        "just now".into()
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3600)
    } else if elapsed < 604_800 {
        format!("{}d", elapsed / 86_400)
    } else {
        DateTime::from_timestamp(timestamp, 0)
            .map(|date| date.format("%b %-d").to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_request_survives_paging_before_layout() {
        let rows = ListState::new(0, ListAlignment::Top, px(0.));
        let mut state = NotificationsState {
            widget: Some(rows.clone()),
            ..Default::default()
        };
        let items = |count| {
            (0..count)
                .map(|id| Notification {
                    id,
                    ..Default::default()
                })
                .collect()
        };
        state.replace_items(items(20));
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
        rows.scroll_to_end();
        state.replace_items(items(30));
        assert_eq!(rows.logical_scroll_top().item_ix, 30);
        state.replace_items(vec![]);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    #[gpui::test]
    fn paging_preserves_measured_notification_heights(cx: &mut gpui::TestAppContext) {
        struct Rows(ListState);
        impl gpui::Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| div().h(px(150.)).into_any()).size_full()
            }
        }
        let rows = ListState::new(0, ListAlignment::Top, px(0.));
        let mut state = NotificationsState {
            widget: Some(rows.clone()),
            ..Default::default()
        };
        let items = |count| {
            (0..count)
                .map(|id| Notification {
                    id,
                    ..Default::default()
                })
                .collect()
        };
        state.replace_items(items(20));
        let (_, cx) = cx.add_window_view(|_, _| Rows(rows.clone()));
        cx.run_until_parked();
        let before = rows.max_offset_for_scrollbar().y;
        state.replace_items(items(21));
        assert_eq!(
            rows.max_offset_for_scrollbar().y,
            before + px(NOTIFICATION_ROW_HEIGHT)
        );
        let after = rows.max_offset_for_scrollbar().y;
        state.replace_items(items(21));
        assert_eq!(rows.max_offset_for_scrollbar().y, after);
    }

    #[test]
    fn refresh_and_paging_keep_the_notification_anchor_and_inset() {
        let rows = ListState::new(0, ListAlignment::Top, px(800.));
        let mut state = NotificationsState {
            widget: Some(rows.clone()),
            ..Default::default()
        };
        let items = |range: std::ops::Range<i64>| {
            range
                .map(|id| Notification {
                    id,
                    ..Default::default()
                })
                .collect()
        };
        state.replace_items(items(0..100));
        assert_eq!(rows.max_offset_for_scrollbar().y, px(8400.));
        rows.scroll_to(gpui::ListOffset {
            item_ix: 30,
            offset_in_item: px(11.),
        });
        state.replace_items(items(-3..100));
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 33,
                offset_in_item: px(11.)
            }
        );
        state.replace_items(items(-3..150));
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 33,
                offset_in_item: px(11.)
            }
        );
        state.replace_items(items(100..150));
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
        state.replace_items(vec![]);
        assert_eq!(rows.item_count(), 0);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    #[test]
    fn paging_deduplicates_without_reordering_older_rows() {
        let first = Notification {
            id: 5,
            kind: "AIRING".into(),
            ..Default::default()
        };
        let second = Notification {
            id: 4,
            kind: "FOLLOWING".into(),
            ..Default::default()
        };
        let updated = Notification {
            id: 4,
            kind: "ACTIVITY_LIKE".into(),
            ..Default::default()
        };
        let third = Notification {
            id: 3,
            kind: "AIRING".into(),
            ..Default::default()
        };
        let merged = merge_page(&[first, second], vec![updated, third], true);
        assert_eq!(
            merged.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
        assert_eq!(merged[1].kind, "ACTIVITY_LIKE");
    }

    #[test]
    fn external_notification_links_reject_other_schemes() {
        let item = Notification {
            comment_url: Some("javascript:alert(1)".into()),
            thread_id: Some(7),
            ..Default::default()
        };
        assert_eq!(notification_url(&item), "https://anilist.co/forum/thread/7");
    }
}
