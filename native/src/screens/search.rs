use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    AnyElement, Context, Entity, FontWeight, ListAlignment, ListState as GpuiListState,
    RenderImage, Subscription, WeakEntity, Window, div, img, list, prelude::*, px, rgb, svg,
};
use kurisu_core::models::{ListEntry, Media, SearchPage};

use super::covers::CoverOwner;
use super::state::status_label;
use super::{Workspace, joined, now_seconds};
use crate::components::input::{TextInput, TextInputEvent};
use crate::components::{button, tracked_mono};
use crate::contracts::Route;
use crate::theme::{ACCENT, BASE, DIM, EDGE, ERROR, INK, PANEL, PANEL_2, RUST, serif_font};

pub(super) const CARD_ROW_HEIGHT: f32 = 302.;
pub(super) const ERROR_BG: u32 = 0x160b0b;
pub(super) const ERROR_EDGE: u32 = 0x502826;

pub(super) fn discovery_heading(
    section: &str,
    title: String,
    description: String,
    actions: Option<AnyElement>,
    compact: bool,
) -> gpui::Div {
    let mut header = div()
        .relative()
        .w_full()
        .min_h(px(94.))
        .flex_shrink_0()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(20.))
        .gap_y(px(8.))
        .pr(px(if compact { 135. } else { 155. }))
        .pb(px(12.))
        .mb(px(18.))
        .border_b_1()
        .border_color(rgb(EDGE))
        .child(
            div()
                .flex_1()
                .flex_basis(px(205.))
                .min_w_0()
                .min_h(px(82.))
                .flex()
                .flex_col()
                .justify_center()
                .py(px(2.))
                .overflow_hidden()
                .child(
                    tracked_mono(section.to_uppercase(), 13., 13. * 0.14)
                        .line_height(px(19.5))
                        .text_color(rgb(DIM)),
                )
                .child(
                    div()
                        .id("discovery-heading")
                        .debug_selector(|| "discovery-heading".into())
                        .role(gpui::Role::Heading)
                        .aria_level(1)
                        .aria_label(title.clone())
                        .flex()
                        .items_center()
                        .font(serif_font())
                        .text_size(px(30.))
                        .line_height(px(34.5))
                        .my(px(5.))
                        .child(title)
                        .child(div().text_color(rgb(ACCENT)).child(".")),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(18.2))
                        .text_color(rgb(DIM))
                        .child(description),
                ),
        );
    if let Some(actions) = actions {
        header = header.child(
            div()
                .id("discovery-heading-actions")
                .debug_selector(|| "discovery-heading-actions".into())
                .pr(px(16.))
                .child(actions),
        );
    }
    header
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
                .text_color(rgb(RUST))
                .child("＋"),
        )
}

#[derive(Default)]
pub(super) struct SearchState {
    pub query: String,
    pub input: Option<Entity<TextInput>>,
    pub subscriptions: Vec<Subscription>,
    pub results: Vec<Media>,
    pub submitted: String,
    pub page: i64,
    pub has_more: bool,
    pub searched: bool,
    pub searching: bool,
    pub error: String,
    pub action_error: String,
    pub request: u64,
    pub resume: Option<(String, bool)>,
    pub rows: Option<GpuiListState>,
    pub columns: usize,
}

pub(super) fn card_columns(width: f32) -> usize {
    if width < 800. {
        1
    } else if width < 1050. {
        2
    } else {
        3
    }
}

pub(super) fn reset_card_rows(
    rows: &GpuiListState,
    item_count: usize,
    old_columns: usize,
    columns: usize,
) {
    let mut offset = rows.logical_scroll_top();
    let at_end = rows.item_count() > 0 && offset.item_ix >= rows.item_count();
    let first = offset.item_ix.saturating_mul(old_columns.max(1));
    offset.item_ix = first.min(item_count.saturating_sub(1)) / columns.max(1);
    if first >= item_count {
        offset.offset_in_item = px(0.);
    }
    let count = item_count.div_ceil(columns.max(1));
    let old_count = rows.item_count();
    if old_columns == columns && count >= old_count {
        if count > old_count {
            rows.splice(old_count..old_count, count - old_count);
            rows.clone().with_uniform_item_height(px(CARD_ROW_HEIGHT));
        } else {
            rows.remeasure_items(0..count);
        }
    } else {
        rows.reset_with_uniform_height(count, px(CARD_ROW_HEIGHT));
    }
    if at_end {
        rows.scroll_to_end();
    } else {
        rows.scroll_to(offset);
    }
}

pub(super) fn merge_search_page(
    previous: &[Media],
    found: SearchPage,
    more: bool,
) -> (Vec<Media>, i64, bool) {
    let mut merged = if more { previous.to_vec() } else { Vec::new() };
    let mut seen: HashSet<i64> = merged.iter().map(|media| media.id).collect();
    for media in found.items {
        if let Some(index) = merged.iter().position(|old| old.id == media.id) {
            merged[index] = media;
        } else if seen.insert(media.id) {
            merged.push(media);
        }
    }
    (merged, found.page, found.has_next_page)
}

impl Workspace {
    pub(super) fn enter_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.input.is_none() {
            let query = self.search.query.clone();
            let input = cx.new(|cx| TextInput::new(query, "Anime title…", window, cx));
            self.search.subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, event, window, cx| match event {
                    TextInputEvent::Changed(value) => this.search.query = value.clone(),
                    TextInputEvent::Submitted => this.run_search(false, window, cx),
                },
            ));
            self.search.input = Some(input);
        }
        if self.search.rows.is_none() {
            let rows = GpuiListState::new(0, ListAlignment::Top, px(290.))
                .with_uniform_item_height(px(CARD_ROW_HEIGHT));
            let weak = cx.weak_entity();
            rows.set_scroll_handler(move |event, _window, app| {
                let _ = weak.update(app, |this, _| {
                    this.prefetch_search_covers(event.visible_range.clone())
                });
            });
            self.search.rows = Some(rows);
            self.search.columns = card_columns(window.bounds().size.width.into());
            self.reset_search_rows();
        }
        if let Some((term, more)) = self.search.resume.take() {
            self.search.submitted = term;
            self.run_search(more, window, cx);
        }
        self.refresh_search_covers();
    }

    pub(super) fn leave_search(&mut self) {
        if self.search.searching {
            self.search.resume = Some((self.search.submitted.clone(), self.search.page > 0));
        }
        self.search.request += 1;
        self.search.searching = false;
        self.cancel_reads("search");
    }

    fn reset_search_rows(&mut self) {
        if let Some(rows) = self.search.rows.as_ref() {
            reset_card_rows(
                rows,
                self.search.results.len(),
                self.search.columns,
                self.search.columns,
            );
        }
    }

    fn run_search(&mut self, more: bool, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.search.searching {
            return;
        }
        let term = if more {
            self.search.submitted.clone()
        } else {
            self.search.query.trim().to_string()
        };
        if term.is_empty() || more && !self.search.has_more {
            return;
        }
        let page = if more { self.search.page + 1 } else { 1 };
        self.search.request += 1;
        let request = self.search.request;
        self.search.searching = true;
        self.search.submitted = term.clone();
        self.search.resume = None;
        self.search.error.clear();
        if !more {
            self.search.results.clear();
            self.search.page = 0;
            self.search.has_more = false;
            self.search.searched = false;
            self.reset_search_rows();
        }
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let searched_term = term.clone();
        let job = core.spawn(async move { worker.search_anime_page(searched_term, page).await });
        self.replace_reads("search", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.search.request != request
                    || this.search.submitted != term
                {
                    return;
                }
                this.search.searching = false;
                match result {
                    Ok(found) => {
                        let (results, page, has_more) =
                            merge_search_page(&this.search.results, found, more);
                        this.search.results = results;
                        this.search.page = page;
                        this.search.has_more = has_more;
                        this.search.searched = true;
                        this.reset_search_rows();
                        this.refresh_search_covers();
                    }
                    Err(error) => this.search.error = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn prefetch_search_covers(&mut self, range: std::ops::Range<usize>) {
        let columns = self.search.columns.max(1);
        let start = range.start.saturating_sub(2) * columns;
        let end = range.end.saturating_add(3) * columns;
        let len = self.search.results.len();
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(len)..end.min(len) {
            let media = &self.search.results[index];
            if let Some(url) = media
                .cover_large
                .as_deref()
                .or(media.cover_medium.as_deref())
            {
                let item = ((media.id, 174, 176), url.to_owned());
                if (range.start * columns..range.end * columns).contains(&index) {
                    visible.push(item);
                } else {
                    nearby.push(item);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Search), visible, nearby);
    }

    pub(super) fn refresh_search_covers(&mut self) {
        if let Some(rows) = self.search.rows.as_ref() {
            let first = rows.logical_scroll_top().item_ix;
            self.prefetch_search_covers(first..first.saturating_add(9));
        }
    }

    pub(super) fn render_search(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let columns = card_columns(window.bounds().size.width.into());
        if columns != self.search.columns {
            if let Some(rows) = self.search.rows.as_ref() {
                reset_card_rows(
                    rows,
                    self.search.results.len(),
                    self.search.columns,
                    columns,
                );
            }
            self.search.columns = columns;
            cx.spawn_in(window, async move |this, cx| {
                let _ = this.update_in(cx, |this, _, _| this.refresh_search_covers());
            })
            .detach();
        }
        let actions = self.search.input.clone().map(|input| {
            div()
                .w(px(288.))
                .max_w_full()
                .flex()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(input))
                .child(
                    button(
                        "search-submit",
                        if self.search.searching {
                            "…"
                        } else {
                            "Search"
                        },
                        self.search.searching,
                    )
                    .w(px(78.))
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(ACCENT))
                    .border_color(rgb(ACCENT))
                    .text_color(rgb(BASE))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_search(false, window, cx)),
                    ),
                )
                .into_any()
        });
        let mut page = div()
            .flex_1()
            .min_h_0()
            .px(px(if f32::from(window.viewport_size().width) <= 960. {
                20.
            } else {
                28.
            }))
            .py(px(20.))
            .flex()
            .flex_col()
            .child(discovery_heading(
                "Discover",
                "Search".into(),
                "Find your next show".into(),
                actions,
                f32::from(window.viewport_size().width) <= 960.,
            ));
        let error = [
            self.search.error.as_str(),
            if self.list.loaded {
                ""
            } else {
                self.list.error.as_str()
            },
            self.search.action_error.as_str(),
        ]
        .into_iter()
        .find(|error| !error.is_empty());
        if let Some(error) = error {
            page = page.child(
                div()
                    .border_1()
                    .border_color(rgb(ERROR_EDGE))
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(ERROR))
                    .p(px(8.))
                    .mb(px(8.))
                    .child(error.to_string()),
            );
        }
        if !self.list.loaded && !self.list.error.is_empty() {
            page = page.child(
                button("search-list-retry", "Retry list", false)
                    .on_click(cx.listener(|this, _, window, cx| this.load_list(window, cx))),
            );
        }
        if self.search.searched
            && self.search.results.is_empty()
            && !self.search.searching
            && self.search.error.is_empty()
        {
            page = page.child(div().py(px(40.)).text_color(rgb(DIM)).child(format!(
                "No anime found for “{}”. Try another title.",
                self.search.submitted
            )));
        }
        if let Some(rows) = self.search.rows.as_ref() {
            let weak = cx.weak_entity();
            let columns = self.search.columns.max(1);
            page = page.child(
                list(rows.clone(), move |row, _window, app| {
                    let result = weak.read_with(app, |this, _| {
                        this.search
                            .results
                            .iter()
                            .skip(row * columns)
                            .take(columns)
                            .map(|media| {
                                (
                                    media.clone(),
                                    this.list.entry(media.id).cloned(),
                                    this.cover_images.get(&(media.id, 174, 176)).cloned(),
                                    this.editor_adding.contains(&media.id),
                                )
                            })
                            .collect::<Vec<_>>()
                    });
                    let Ok(cards) = result else {
                        return div().into_any();
                    };
                    let count = cards.len();
                    let mut row_view = div().w_full().flex().gap(px(10.)).py(px(5.));
                    for (media, entry, cover, adding) in cards {
                        row_view = row_view.child(media_card(
                            media,
                            entry,
                            cover,
                            adding,
                            true,
                            true,
                            weak.clone(),
                        ));
                    }
                    for _ in count..columns {
                        row_view = row_view.child(div().flex_1().min_w_0());
                    }
                    row_view.into_any()
                })
                .flex_1()
                .min_h_0(),
            );
        }
        if self.search.has_more {
            page = page.child(
                div().flex().justify_center().mt(px(12.)).child(
                    button(
                        "search-more",
                        if self.search.searching {
                            "Loading…"
                        } else {
                            "Load more anime"
                        },
                        self.search.searching,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.run_search(true, window, cx))),
                ),
            );
        }
        page.into_any()
    }

    pub(super) fn change_discovery_status(
        &mut self,
        media: Media,
        status: &'static str,
        origin: Route,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.profile.is_none()
            || self.editor_adding.contains(&media.id)
            || self.barrier.is_some()
            || self.auth_busy
            || self.update_busy
        {
            return;
        }
        let id = media.id;
        self.editor_adding.insert(id);
        self.active_writes += 1;
        match origin {
            Route::Search => self.search.action_error.clear(),
            Route::Seasons => self.seasons.action_error.clear(),
            _ => {}
        }
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    worker
                        .update_entry(id, Some(status.into()), None, None, None)
                        .await
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation
                    || account != this.profile.as_ref().map(|user| user.id)
                {
                    this.check_barrier(window, cx);
                    return;
                }
                this.editor_adding.remove(&id);
                match result {
                    Ok(mut entry) => {
                        if entry.media.is_none() {
                            entry.media = Some(media);
                        }
                        let change = this.list.apply_one(entry, now_seconds());
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Err(error) => match origin {
                        Route::Search => this.search.action_error = error,
                        Route::Seasons => this.seasons.action_error = error,
                        _ => this.global_error = error,
                    },
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

pub(super) fn media_card(
    media: Media,
    entry: Option<ListEntry>,
    cover: Option<Arc<RenderImage>>,
    adding: bool,
    show_year: bool,
    show_listed_actions: bool,
    weak: WeakEntity<Workspace>,
) -> AnyElement {
    let id = media.id;
    let title = media.display_title();
    let mut metadata = Vec::new();
    if let Some(format) = media.format.as_ref() {
        metadata.push(format.replace('_', " "));
    }
    if show_year && let Some(year) = media.season_year {
        metadata.push(year.to_string());
    }
    if let Some(episodes) = media.episodes {
        metadata.push(format!("{episodes} eps"));
    }
    if let Some(score) = media.average_score.filter(|score| *score > 0) {
        metadata.push(format!("★ {score}"));
    }
    let badge = entry.as_ref().map(|entry| {
        let mut badge = status_label(&entry.status).to_string();
        if entry.progress > 0 {
            badge.push_str(&media.episodes.map_or_else(
                || format!(" · {}", entry.progress),
                |total| format!(" · {}/{total}", entry.progress),
            ));
        }
        badge
    });
    let cover_element: AnyElement = if let Some(image) = cover {
        img(image)
            .w_full()
            .h(px(176.))
            .object_fit(gpui::ObjectFit::Cover)
            .into_any()
    } else {
        div().w_full().h(px(176.)).bg(rgb(PANEL_2)).into_any()
    };
    let origin = if show_listed_actions {
        Route::Search
    } else {
        Route::Seasons
    };
    let mut card = div()
        .w_full()
        .flex_1()
        .min_w_0()
        .min_h(px(292.))
        .overflow_hidden()
        .border_1()
        .border_color(rgb(EDGE))
        .bg(rgb(PANEL))
        .flex()
        .flex_col()
        .child(
            div()
                .id(format!("media-cover-{id}"))
                .role(gpui::Role::Button)
                .aria_label(format!("Open details for {title}"))
                .focusable()
                .tab_stop(true)
                .cursor_pointer()
                .w_full()
                .h(px(176.))
                .flex_shrink_0()
                .overflow_hidden()
                .border_b_1()
                .border_color(rgb(EDGE))
                .on_key_down({
                    let weak = weak.clone();
                    move |event: &gpui::KeyDownEvent, window, app| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                            app.stop_propagation();
                            let _ = weak.update(app, |this, cx| {
                                this.navigate(Route::Anime(id), window, cx)
                            });
                        }
                    }
                })
                .on_click({
                    let weak = weak.clone();
                    move |_, window, app| {
                        let _ = weak
                            .update(app, |this, cx| this.navigate(Route::Anime(id), window, cx));
                    }
                })
                .child(cover_element),
        )
        .child(
            div()
                .id(format!("media-title-{id}"))
                .role(gpui::Role::Button)
                .aria_label(format!("Open details for {title}"))
                .focusable()
                .tab_stop(true)
                .cursor_pointer()
                .on_key_down({
                    let weak = weak.clone();
                    move |event: &gpui::KeyDownEvent, window, app| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                            app.stop_propagation();
                            let _ = weak.update(app, |this, cx| {
                                this.navigate(Route::Anime(id), window, cx)
                            });
                        }
                    }
                })
                .p(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .whitespace_normal()
                .on_click({
                    let weak = weak.clone();
                    move |_, window, app| {
                        let _ = weak
                            .update(app, |this, cx| this.navigate(Route::Anime(id), window, cx));
                    }
                })
                .child(title),
        )
        .child(
            div()
                .px(px(10.))
                .text_size(px(11.))
                .text_color(rgb(DIM))
                .child(metadata.join(" · ")),
        );
    if let Some(badge) = badge {
        card = card.child(
            div()
                .px(px(10.))
                .py(px(5.))
                .text_size(px(11.))
                .text_color(rgb(ACCENT))
                .child(if show_listed_actions {
                    badge
                } else {
                    format!("✓ {badge}")
                }),
        );
    }
    if entry.is_none() || show_listed_actions {
        let mut actions = div().flex().flex_wrap().gap(px(4.)).p(px(8.)).mt_auto();
        for (status, label) in [
            ("CURRENT", "Watching"),
            ("PLANNING", "Plan to watch"),
            ("COMPLETED", "Completed"),
        ] {
            let selected = entry.as_ref().is_some_and(|entry| entry.status == status);
            let media = media.clone();
            let weak = weak.clone();
            let origin = origin.clone();
            actions = actions.child(
                button(format!("media-{id}-{status}"), label, adding)
                    .bg(rgb(if selected { ACCENT } else { PANEL_2 }))
                    .border_color(rgb(if selected { ACCENT } else { PANEL_2 }))
                    .text_color(rgb(if selected { BASE } else { INK }))
                    .text_size(px(12.))
                    .px(px(8.))
                    .py(px(4.))
                    .rounded(px(3.))
                    .on_key_down({
                        let weak = weak.clone();
                        let media = media.clone();
                        let origin = origin.clone();
                        move |event: &gpui::KeyDownEvent, window, app| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                app.stop_propagation();
                                let _ = weak.update(app, |this, cx| {
                                    this.change_discovery_status(
                                        media.clone(),
                                        status,
                                        origin.clone(),
                                        window,
                                        cx,
                                    )
                                });
                            }
                        }
                    })
                    .on_click(move |_, window, app| {
                        let _ = weak.update(app, |this, cx| {
                            this.change_discovery_status(
                                media.clone(),
                                status,
                                origin.clone(),
                                window,
                                cx,
                            )
                        });
                    }),
            );
        }
        card = card.child(actions);
    }
    card.child(div().w_full().h(px(2.)).bg(rgb(RUST)))
        .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Render, TestAppContext};

    #[test]
    fn end_request_survives_paging_and_reflow_before_layout() {
        let rows = GpuiListState::new(0, ListAlignment::Top, px(0.));
        reset_card_rows(&rows, 30, 3, 3);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
        rows.scroll_to_end();
        reset_card_rows(&rows, 33, 3, 3);
        assert_eq!(rows.logical_scroll_top().item_ix, 11);
        reset_card_rows(&rows, 33, 3, 1);
        assert_eq!(rows.logical_scroll_top().item_ix, 33);
        reset_card_rows(&rows, 0, 1, 1);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    #[gpui::test]
    fn paging_preserves_measured_height_estimates(cx: &mut TestAppContext) {
        struct Rows(GpuiListState);
        impl Render for Rows {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                list(self.0.clone(), |_, _, _| div().h(px(500.)).into_any()).size_full()
            }
        }
        let rows = GpuiListState::new(0, ListAlignment::Top, px(0.));
        reset_card_rows(&rows, 30, 3, 3);
        let (_, cx) = cx.add_window_view(|_, _| Rows(rows.clone()));
        cx.run_until_parked();
        let before = rows.max_offset_for_scrollbar().y;
        reset_card_rows(&rows, 33, 3, 3);
        assert_eq!(
            rows.max_offset_for_scrollbar().y,
            before + px(CARD_ROW_HEIGHT)
        );
        let after = rows.max_offset_for_scrollbar().y;
        reset_card_rows(&rows, 33, 3, 3);
        assert_eq!(rows.max_offset_for_scrollbar().y, after);
    }

    #[test]
    fn paging_and_column_changes_keep_the_card_anchor_and_inset() {
        let rows = GpuiListState::new(0, ListAlignment::Top, px(290.));
        reset_card_rows(&rows, 300, 3, 3);
        assert_eq!(rows.max_offset_for_scrollbar().y, px(30_200.));
        rows.scroll_to(gpui::ListOffset {
            item_ix: 50,
            offset_in_item: px(35.),
        });
        reset_card_rows(&rows, 350, 3, 3);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 50,
                offset_in_item: px(35.)
            }
        );
        reset_card_rows(&rows, 350, 3, 2);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 75,
                offset_in_item: px(35.)
            }
        );
        reset_card_rows(&rows, 10, 2, 2);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 4,
                offset_in_item: px(0.)
            }
        );
        reset_card_rows(&rows, 0, 2, 2);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    struct CompactHeading;

    impl Render for CompactHeading {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let actions = div()
                .flex()
                .gap(px(7.))
                .child(button("test-prev", "← Prev", false).px(px(10.)))
                .child(button("test-next", "Next →", false).px(px(10.)))
                .into_any();
            div()
                .size_full()
                .child(div().w(px(544.)).child(discovery_heading(
                    "Seasons",
                    "Fall 2026".into(),
                    "Browse the season".into(),
                    Some(actions),
                    true,
                )))
        }
    }

    #[gpui::test]
    fn compact_seasons_heading_keeps_actions_beside_title(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| CompactHeading);
        cx.run_until_parked();
        let title = cx.debug_bounds("discovery-heading").unwrap();
        let actions = cx.debug_bounds("discovery-heading-actions").unwrap();
        assert!(actions.origin.x > title.origin.x + px(150.));
        assert!(actions.origin.y < title.origin.y + px(38.));
    }

    #[test]
    fn pagination_replaces_duplicate_media_without_losing_order() {
        let previous = vec![
            Media {
                id: 1,
                ..Default::default()
            },
            Media {
                id: 2,
                ..Default::default()
            },
        ];
        let found = SearchPage {
            items: vec![
                Media {
                    id: 2,
                    title_english: Some("Updated".into()),
                    ..Default::default()
                },
                Media {
                    id: 3,
                    ..Default::default()
                },
            ],
            page: 2,
            has_next_page: true,
        };
        let (merged, page, more) = merge_search_page(&previous, found, true);
        assert_eq!(
            merged.iter().map(|media| media.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(merged[1].display_title(), "Updated");
        assert_eq!((page, more), (2, true));
    }
}
