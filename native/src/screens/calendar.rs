use std::sync::Arc;

use chrono::{Duration, Local, NaiveDate, TimeZone};
use gpui::{
    AnyElement, Context, ListAlignment, ListOffset, ListState as GpuiListState, Window, div, img,
    list, prelude::*, px, rgb,
};
use kurisu_core::models::AiringItem;

use super::covers::CoverOwner;
use super::search::{ERROR_BG, ERROR_EDGE, discovery_heading};
use super::state::status_label;
use super::{Workspace, joined, now_seconds};
use crate::components::button;
use crate::contracts::Route;
use crate::theme::{ACCENT, BASE, DIM, EDGE, ERROR, OCHRE, PANEL, PANEL_2};

const CALENDAR_ROW_HEIGHT: f32 = 64.;

fn today() -> NaiveDate {
    Local::now().date_naive()
}

pub(super) fn week_dates(anchor: NaiveDate, offset: i64) -> (NaiveDate, NaiveDate) {
    let start = anchor
        .checked_add_signed(Duration::days(offset.clamp(-5200, 5200) * 7))
        .unwrap_or(anchor);
    (
        start,
        start.checked_add_signed(Duration::days(7)).unwrap_or(start),
    )
}

fn midnight(date: NaiveDate) -> i64 {
    let local = date.and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&local)
        .earliest()
        .unwrap_or_else(|| Local.from_utc_datetime(&local))
        .timestamp()
}

fn local_date(timestamp: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| value.with_timezone(&Local).date_naive())
        .unwrap_or_else(today)
}

fn day_label(date: NaiveDate) -> String {
    let label = date.format("%A, %b %-d").to_string();
    let days = date.signed_duration_since(today()).num_days();
    match days {
        0 => format!("Today · {label}"),
        1 => format!("Tomorrow · {label}"),
        _ => label,
    }
}

fn time_label(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| {
            let local = value.with_timezone(&Local);
            local.format("%I:%M %p").to_string()
        })
        .unwrap_or_default()
}

pub(super) fn calendar_matches(
    item: &AiringItem,
    watching_only: bool,
    status: Option<&str>,
) -> bool {
    !watching_only || status == Some("CURRENT") && item.media.id > 0
}

pub(super) struct CalendarState {
    pub anchor: NaiveDate,
    pub offset: i64,
    pub watching_only: bool,
    pub preference_loaded: bool,
    pub preference_loading: bool,
    pub preference_request: u64,
    pub items: Vec<AiringItem>,
    pub visible: Arc<Vec<usize>>,
    pub loading: bool,
    pub loaded_once: bool,
    pub error: String,
    pub request: u64,
    pub rows: Option<GpuiListState>,
}

impl Default for CalendarState {
    fn default() -> Self {
        Self {
            anchor: today(),
            offset: 0,
            watching_only: true,
            preference_loaded: false,
            preference_loading: false,
            preference_request: 0,
            items: Vec::new(),
            visible: Arc::new(Vec::new()),
            loading: false,
            loaded_once: false,
            error: String::new(),
            request: 0,
            rows: None,
        }
    }
}

impl CalendarState {
    fn replace_visible(&mut self, visible: Vec<usize>) {
        if let Some(rows) = &self.rows {
            if self.visible.as_ref() == &visible {
                rows.remeasure_items(0..rows.item_count());
            } else {
                let mut offset = rows.logical_scroll_top();
                let at_end = rows.item_count() > 0 && offset.item_ix >= rows.item_count();
                let anchor = self.visible.get(offset.item_ix);
                if let Some(position) =
                    anchor.and_then(|anchor| visible.iter().position(|id| id == anchor))
                {
                    offset.item_ix = position;
                } else {
                    offset.item_ix = offset.item_ix.min(visible.len().saturating_sub(1));
                    offset.offset_in_item = px(0.);
                }
                rows.reset_with_uniform_height(visible.len(), px(CALENDAR_ROW_HEIGHT));
                if at_end {
                    rows.scroll_to_end();
                } else {
                    rows.scroll_to(offset);
                }
            }
        }
        self.visible = Arc::new(visible);
    }

    fn scroll_anchor(&self) -> Option<(AiringItem, ListOffset)> {
        let offset = self.rows.as_ref()?.logical_scroll_top();
        let item = self.items.get(*self.visible.get(offset.item_ix)?)?;
        Some((item.clone(), offset))
    }

    fn restore_anchor(&self, anchor: Option<(AiringItem, ListOffset)>) {
        if let Some((item, mut offset)) = anchor
            && let Some(rows) = &self.rows
            && let Some(position) = self.visible.iter().position(|index| {
                let candidate = &self.items[*index];
                (candidate.media.id, candidate.episode, candidate.airing_at)
                    == (item.media.id, item.episode, item.airing_at)
            })
        {
            offset.item_ix = position;
            rows.scroll_to(offset);
        }
    }

    pub(super) fn reset_account(&mut self) {
        *self = Self {
            watching_only: self.watching_only,
            preference_loaded: self.preference_loaded,
            ..Self::default()
        };
    }
}

impl Workspace {
    pub(super) fn enter_calendar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.calendar.rows.is_none() {
            let rows = GpuiListState::new(0, ListAlignment::Top, px(84.))
                .with_uniform_item_height(px(CALENDAR_ROW_HEIGHT));
            let weak = cx.weak_entity();
            rows.set_scroll_handler(move |event, _window, app| {
                let _ = weak.update(app, |this, _| {
                    this.prefetch_calendar_covers(event.visible_range.clone())
                });
            });
            self.calendar.rows = Some(rows);
        }
        if !self.calendar.preference_loaded && !self.calendar.preference_loading {
            self.calendar.preference_loading = true;
            self.calendar.preference_request += 1;
            let request = self.calendar.preference_request;
            let generation = self.generation;
            let account = self.profile.as_ref().map(|user| user.id);
            let core = self.core.clone();
            let worker = core.clone();
            let job =
                core.spawn(async move { worker.get_preference("kurisu.cal.mine".into()).await });
            self.replace_reads("calendar-preference", vec![job.abort_handle()]);
            cx.spawn_in(window, async move |this, cx| {
                let result = joined(job.await);
                let _ = this.update_in(cx, |this, _window, cx| {
                    if this.generation != generation
                        || this.profile.as_ref().map(|user| user.id) != account
                        || this.calendar.preference_request != request
                    {
                        return;
                    }
                    this.calendar.preference_loading = false;
                    this.calendar.preference_loaded = true;
                    if let Ok(Some(value)) = result {
                        this.calendar.watching_only = value != "0";
                        this.refresh_calendar_visible();
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        if !self.calendar.loaded_once && !self.calendar.loading {
            self.load_calendar(window, cx);
        } else {
            self.refresh_calendar_visible();
        }
    }

    pub(super) fn leave_calendar(&mut self) {
        if self.calendar.preference_loading {
            self.calendar.preference_loading = false;
            self.calendar.preference_request += 1;
            self.cancel_reads("calendar-preference");
        }
        if self.calendar.loading {
            self.calendar.loaded_once = false;
            self.calendar.loading = false;
        }
        self.calendar.request += 1;
        self.cancel_reads("calendar");
    }

    fn load_calendar(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.calendar.loading {
            return;
        }
        self.calendar.request += 1;
        let request = self.calendar.request;
        self.calendar.loading = true;
        self.calendar.error.clear();
        let anchor = self.calendar.scroll_anchor();
        self.calendar.items.clear();
        self.refresh_calendar_visible();
        let (start, end) = week_dates(self.calendar.anchor, self.calendar.offset);
        let (start, end) = (midnight(start), midnight(end));
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_airing_schedule(start, end).await });
        self.replace_reads("calendar", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.calendar.request != request
                {
                    return;
                }
                this.calendar.loading = false;
                this.calendar.loaded_once = true;
                match result {
                    Ok(items) => {
                        this.calendar.items = items;
                        this.refresh_calendar_visible();
                        this.calendar.restore_anchor(anchor);
                        this.refresh_calendar_covers();
                    }
                    Err(error) => this.calendar.error = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn shift_calendar(&mut self, delta: i64, window: &Window, cx: &mut Context<Self>) {
        if self.calendar.loading {
            return;
        }
        self.calendar.offset = (self.calendar.offset + delta).clamp(-5200, 5200);
        self.load_calendar(window, cx);
    }

    fn reset_calendar_today(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.calendar.loading {
            return;
        }
        self.calendar.anchor = today();
        self.calendar.offset = 0;
        self.load_calendar(window, cx);
    }

    pub(super) fn set_calendar_filter(
        &mut self,
        watching_only: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.calendar.watching_only == watching_only
            && self.calendar.preference_loaded
            && !self.calendar.preference_loading
        {
            return;
        }
        self.calendar.watching_only = watching_only;
        self.calendar.preference_request += 1;
        self.calendar.preference_loading = false;
        self.calendar.preference_loaded = true;
        self.cancel_reads("calendar-preference");
        self.refresh_calendar_visible();
        self.persist_preference(
            "kurisu.cal.mine",
            Some(if watching_only { "1" } else { "0" }.into()),
            window,
            cx,
        );
        cx.notify();
    }

    pub(super) fn refresh_calendar_visible(&mut self) {
        let mut visible = self
            .calendar
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let status = self
                    .list
                    .entry(item.media.id)
                    .map(|entry| entry.status.as_str());
                calendar_matches(item, self.calendar.watching_only, status).then_some(index)
            })
            .collect::<Vec<_>>();
        visible.sort_by_key(|index| self.calendar.items[*index].airing_at);
        self.calendar.replace_visible(visible);
        self.refresh_calendar_covers();
    }

    fn prefetch_calendar_covers(&mut self, range: std::ops::Range<usize>) {
        let start = range.start.saturating_sub(8);
        let end = range.end.saturating_add(12);
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for position in start..end.min(self.calendar.visible.len()) {
            let Some(item) = self
                .calendar
                .visible
                .get(position)
                .and_then(|index| self.calendar.items.get(*index))
            else {
                continue;
            };
            if let Some(url) = item.media.cover_medium.as_deref() {
                let cover = ((item.media.id, 32, 44), url.to_owned());
                if range.contains(&position) {
                    visible.push(cover);
                } else {
                    nearby.push(cover);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Calendar), visible, nearby);
    }

    pub(super) fn refresh_calendar_covers(&mut self) {
        if let Some(rows) = self.calendar.rows.as_ref() {
            let first = rows.logical_scroll_top().item_ix;
            self.prefetch_calendar_covers(first..first.saturating_add(24));
        }
    }

    pub(super) fn render_calendar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (start, end) = week_dates(self.calendar.anchor, self.calendar.offset);
        let last = end.pred_opt().unwrap_or(end);
        let range = format!("{} – {}", start.format("%b %-d"), last.format("%b %-d"));
        let actions = div()
            .flex()
            .flex_wrap()
            .gap(px(7.))
            .child(
                div()
                    .flex()
                    .border_1()
                    .border_color(rgb(EDGE))
                    .child(
                        button("calendar-mine", "Watching", false)
                            .h(px(33.))
                            .px(px(10.))
                            .py(px(5.))
                            .border_color(rgb(BASE))
                            .bg(rgb(if self.calendar.watching_only {
                                PANEL_2
                            } else {
                                BASE
                            }))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_calendar_filter(true, window, cx)
                            })),
                    )
                    .child(
                        button("calendar-all", "All", false)
                            .h(px(33.))
                            .px(px(10.))
                            .py(px(5.))
                            .border_color(rgb(BASE))
                            .bg(rgb(if self.calendar.watching_only {
                                BASE
                            } else {
                                PANEL_2
                            }))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.set_calendar_filter(false, window, cx)
                            })),
                    ),
            )
            .child(
                button("calendar-prev", "←", self.calendar.loading)
                    .h(px(35.))
                    .px(px(8.))
                    .py(px(6.))
                    .bg(rgb(PANEL_2))
                    .border_color(rgb(PANEL_2))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.shift_calendar(-1, window, cx)),
                    ),
            )
            .child(
                button(
                    "calendar-today",
                    "Today",
                    self.calendar.loading || start == today(),
                )
                .h(px(35.))
                .px(px(8.))
                .py(px(6.))
                .bg(rgb(PANEL_2))
                .border_color(rgb(PANEL_2))
                .on_click(cx.listener(|this, _, window, cx| {
                    if !this.calendar.loading
                        && week_dates(this.calendar.anchor, this.calendar.offset).0 != today()
                    {
                        this.reset_calendar_today(window, cx);
                    }
                })),
            )
            .child(
                button("calendar-refresh", "Refresh", self.calendar.loading)
                    .h(px(35.))
                    .px(px(8.))
                    .py(px(6.))
                    .bg(rgb(PANEL_2))
                    .border_color(rgb(PANEL_2))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.calendar.loading {
                            this.load_calendar(window, cx);
                        }
                    })),
            )
            .child(
                button("calendar-next", "→", self.calendar.loading)
                    .h(px(35.))
                    .px(px(8.))
                    .py(px(6.))
                    .bg(rgb(PANEL_2))
                    .border_color(rgb(PANEL_2))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.shift_calendar(1, window, cx)),
                    ),
            )
            .into_any();
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
                "On the air",
                "Calendar".into(),
                range,
                Some(actions),
                f32::from(window.viewport_size().width) <= 960.,
            ));
        if !self.calendar.error.is_empty() {
            page = page.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .border_1()
                    .border_color(rgb(ERROR_EDGE))
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(ERROR))
                    .p(px(8.))
                    .child(
                        div()
                            .id("calendar-error")
                            .role(gpui::Role::Alert)
                            .aria_label(self.calendar.error.clone())
                            .flex_1()
                            .min_w_0()
                            .child(self.calendar.error.clone()),
                    )
                    .child(
                        button("calendar-retry", "Retry", self.calendar.loading)
                            .h(px(20.))
                            .px(px(0.))
                            .py(px(0.))
                            .bg(rgb(ERROR_BG))
                            .border_color(rgb(ERROR_BG))
                            .text_color(rgb(ACCENT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !this.calendar.loading {
                                    this.load_calendar(window, cx);
                                }
                            })),
                    ),
            );
        }
        if !self.list.loaded && !self.list.error.is_empty() {
            page = page.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .border_1()
                    .border_color(rgb(ERROR_EDGE))
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(ERROR))
                    .p(px(8.))
                    .child(
                        div()
                            .id("calendar-list-error")
                            .role(gpui::Role::Alert)
                            .aria_label(format!("Could not read your list: {}", self.list.error))
                            .flex_1()
                            .min_w_0()
                            .child(format!("Could not read your list: {}", self.list.error)),
                    )
                    .child(
                        button("calendar-list-retry", "Retry list", self.list.loading)
                            .h(px(20.))
                            .px(px(0.))
                            .py(px(0.))
                            .bg(rgb(ERROR_BG))
                            .border_color(rgb(ERROR_BG))
                            .text_color(rgb(ACCENT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !this.list.loading {
                                    this.load_list(window, cx);
                                }
                            })),
                    ),
            );
        }
        if self.calendar.watching_only && !self.list.loaded && self.list.error.is_empty() {
            page = page.child(
                div()
                    .py(px(20.))
                    .text_color(rgb(DIM))
                    .child("Loading your list…"),
            );
        } else if self.calendar.loading && self.calendar.items.is_empty() {
            page = page.child(div().py(px(20.)).text_color(rgb(DIM)).child("Loading…"));
        } else if self.calendar.visible.is_empty() && self.calendar.error.is_empty() {
            page = page.child(div().py(px(20.)).text_color(rgb(DIM)).child(
                if self.calendar.watching_only {
                    "None of your Watching shows air this week."
                } else {
                    "Nothing airing this week."
                },
            ));
        }
        if (!self.calendar.watching_only || self.list.loaded)
            && let Some(rows) = self.calendar.rows.as_ref()
        {
            let weak = cx.weak_entity();
            let visible = self.calendar.visible.clone();
            page = page.child(
                list(rows.clone(), move |row, _window, app| {
                    let result = weak.read_with(app, |this, _| {
                        let index = *visible.get(row)?;
                        let item = this.calendar.items.get(index)?.clone();
                        let prior_day = row
                            .checked_sub(1)
                            .and_then(|previous| visible.get(previous))
                            .and_then(|previous| this.calendar.items.get(*previous))
                            .map(|previous| local_date(previous.airing_at));
                        let entry = this.list.entry(item.media.id).cloned();
                        let cover = this.cover_images.get(&(item.media.id, 32, 44)).cloned();
                        Some((item, prior_day, entry, cover))
                    });
                    let Ok(Some((item, prior_day, entry, cover))) = result else {
                        return div().into_any();
                    };
                    let date = local_date(item.airing_at);
                    let id = item.media.id;
                    let title = item.media.display_title();
                    let aired = item.airing_at < now_seconds();
                    let mut outer = div().w_full().flex().flex_col();
                    if prior_day != Some(date) {
                        outer = outer.child(
                            div()
                                .py(px(8.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(DIM))
                                .child(day_label(date)),
                        );
                    }
                    let cover: AnyElement = if let Some(image) = cover {
                        img(image)
                            .w(px(32.))
                            .h(px(44.))
                            .object_fit(gpui::ObjectFit::Cover)
                            .into_any()
                    } else {
                        div().w(px(32.)).h(px(44.)).bg(rgb(PANEL_2)).into_any()
                    };
                    let mut meta = div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .text_color(rgb(DIM))
                        .child(format!(
                            "Ep {}{}",
                            item.episode,
                            item.media
                                .episodes
                                .map_or_else(String::new, |total| format!("/{total}"))
                        ));
                    if let Some(entry) = entry {
                        meta = meta
                            .child(
                                div()
                                    .bg(rgb(PANEL_2))
                                    .text_color(rgb(if entry.progress < item.episode - 1 {
                                        OCHRE
                                    } else {
                                        DIM
                                    }))
                                    .child(format!("you: {}/{}", entry.progress, item.episode)),
                            )
                            .child(
                                div()
                                    .bg(rgb(PANEL_2))
                                    .text_color(rgb(ACCENT))
                                    .child(status_label(&entry.status)),
                            );
                    }
                    outer = outer.child(
                        div()
                            .id(format!("airing-{id}-{}", item.episode))
                            .role(gpui::Role::Button)
                            .aria_label(format!("Open details for {title}"))
                            .focusable()
                            .tab_stop(true)
                            .cursor_pointer()
                            .on_key_down({
                                let weak = weak.clone();
                                move |event: &gpui::KeyDownEvent, window, app| {
                                    if matches!(
                                        event.keystroke.key.as_str(),
                                        "enter" | "space" | " "
                                    ) {
                                        app.stop_propagation();
                                        let _ = weak.update(app, |this, cx| {
                                            this.navigate(Route::Anime(id), window, cx)
                                        });
                                    }
                                }
                            })
                            .w_full()
                            .min_h(px(64.))
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .px(px(12.))
                            .py(px(7.))
                            .border_1()
                            .border_color(rgb(EDGE))
                            .bg(rgb(if aired { BASE } else { PANEL }))
                            .on_click({
                                let weak = weak.clone();
                                move |_, window, app| {
                                    let _ = weak.update(app, |this, cx| {
                                        this.navigate(Route::Anime(id), window, cx)
                                    });
                                }
                            })
                            .child(
                                div()
                                    .w(px(52.))
                                    .flex_shrink_0()
                                    .text_size(px(12.))
                                    .text_color(rgb(DIM))
                                    .child(time_label(item.airing_at)),
                            )
                            .child(cover)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.))
                                    .child(div().whitespace_normal().child(title))
                                    .child(meta),
                            ),
                    );
                    outer.into_any()
                })
                .flex_1()
                .min_h_0(),
            );
        }
        page.into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurisu_core::models::Media;
    #[test]
    fn refresh_restores_the_airing_identity_after_reordering() {
        let item = |id, episode, airing_at| AiringItem {
            media: Media {
                id,
                ..Default::default()
            },
            episode,
            airing_at,
        };
        let rows = GpuiListState::new(0, ListAlignment::Top, px(84.));
        let mut state = CalendarState {
            rows: Some(rows.clone()),
            items: vec![item(1, 1, 100), item(1, 2, 200)],
            ..Default::default()
        };
        state.replace_visible(vec![0, 1]);
        rows.scroll_to(ListOffset {
            item_ix: 1,
            offset_in_item: px(13.),
        });
        let anchor = state.scroll_anchor();
        state.items.clear();
        state.replace_visible(vec![]);
        state.items = vec![item(2, 1, 50), item(1, 1, 100), item(1, 2, 200)];
        state.replace_visible(vec![0, 1, 2]);
        state.restore_anchor(anchor);
        assert_eq!(
            rows.logical_scroll_top(),
            ListOffset {
                item_ix: 2,
                offset_in_item: px(13.)
            }
        );
        let anchor = state.scroll_anchor();
        state.items = vec![item(1, 2, 300)];
        state.replace_visible(vec![0]);
        state.restore_anchor(anchor);
        assert_eq!(rows.logical_scroll_top(), ListOffset::default());
    }

    #[test]
    fn end_request_survives_filtering_before_layout() {
        let rows = GpuiListState::new(0, ListAlignment::Top, px(0.));
        let mut state = CalendarState {
            rows: Some(rows.clone()),
            ..Default::default()
        };
        state.replace_visible((0..100).collect());
        assert_eq!(rows.logical_scroll_top(), ListOffset::default());
        rows.scroll_to_end();
        state.replace_visible((0..100).step_by(2).collect());
        assert_eq!(rows.logical_scroll_top().item_ix, 50);
        state.replace_visible(vec![]);
        assert_eq!(rows.logical_scroll_top(), ListOffset::default());
    }

    #[test]
    fn filters_keep_the_airing_anchor_and_inset() {
        let rows = GpuiListState::new(0, ListAlignment::Top, px(84.));
        let mut state = CalendarState {
            rows: Some(rows.clone()),
            ..Default::default()
        };
        state.replace_visible((0..100).collect());
        assert_eq!(rows.max_offset_for_scrollbar().y, px(6400.));
        rows.scroll_to(gpui::ListOffset {
            item_ix: 50,
            offset_in_item: px(17.),
        });
        let filtered = (0..100).step_by(2).collect::<Vec<_>>();
        state.replace_visible(filtered.clone());
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 25,
                offset_in_item: px(17.)
            }
        );
        state.replace_visible(filtered);
        assert_eq!(rows.logical_scroll_top().offset_in_item, px(17.));
        state.replace_visible(vec![1, 3, 5]);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 2,
                offset_in_item: px(0.)
            }
        );
        state.replace_visible(vec![]);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
        assert_eq!(rows.max_offset_for_scrollbar().y, px(0.));
    }

    #[test]
    fn week_navigation_uses_calendar_days_and_filter_uses_current_status() {
        let anchor = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        assert_eq!(
            week_dates(anchor, 1).0,
            NaiveDate::from_ymd_opt(2026, 3, 14).unwrap()
        );
        let item = AiringItem {
            airing_at: 1,
            episode: 2,
            media: Media {
                id: 1,
                ..Default::default()
            },
        };
        assert!(calendar_matches(&item, false, None));
        assert!(calendar_matches(&item, true, Some("CURRENT")));
        assert!(!calendar_matches(&item, true, Some("PLANNING")));
    }
}
