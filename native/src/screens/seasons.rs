use std::sync::Arc;

use chrono::Datelike;
use gpui::{
    AnyElement, Context, Entity, FocusHandle, ListAlignment, ListState, Subscription, Window,
    deferred, div, list, prelude::*, px, rgb,
};
use kurisu_core::models::Media;

use super::search::{
    CARD_ROW_HEIGHT, ERROR_BG, ERROR_EDGE, card_columns, discovery_heading, media_card,
    reset_card_rows,
};
use super::state::{STATUSES, status_label};
use super::{Workspace, covers::CoverOwner, joined};
use crate::components::input::{TextInput, TextInputEvent};
use crate::components::{button, eyebrow, select_with_disabled};
use crate::contracts::Route;
use crate::theme::{DIM, EDGE, ERROR, PANEL, PANEL_2};

const SEASONS: [&str; 4] = ["WINTER", "SPRING", "SUMMER", "FALL"];
const FORMATS: [&str; 7] = ["TV", "TV_SHORT", "MOVIE", "SPECIAL", "OVA", "ONA", "MUSIC"];

fn arrow_choice<'a>(current: &str, options: &[&'a str], key: &str) -> Option<&'a str> {
    let index = options
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    let next = match key {
        "up" | "arrowup" => index.saturating_sub(1),
        "down" | "arrowdown" => (index + 1).min(options.len().saturating_sub(1)),
        _ => return None,
    };
    options.get(next).copied()
}

fn season_name(value: &str) -> &'static str {
    match value {
        "WINTER" => "Winter",
        "SPRING" => "Spring",
        "SUMMER" => "Summer",
        _ => "Fall",
    }
}

fn current_season() -> (String, i64) {
    let today = chrono::Local::now();
    (
        SEASONS[(today.month0() / 3) as usize].into(),
        today.year() as i64,
    )
}

pub(super) fn shifted(season: &str, year: i64, delta: i64) -> (String, i64) {
    let index = SEASONS.iter().position(|item| *item == season).unwrap_or(0) as i64;
    let months = (year * 4 + index + delta).clamp(4, 9999 * 4 + 3);
    (SEASONS[(months % 4) as usize].into(), months / 4)
}

pub(super) fn season_matches(
    media: &Media,
    needle: &str,
    format: &str,
    list_status: &str,
    status: Option<&str>,
) -> bool {
    let needle = needle.trim().to_lowercase();
    let title_match = needle.is_empty()
        || [
            media.title_english.as_deref(),
            media.title_romaji.as_deref(),
            media.title_native.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|title| title.to_lowercase().contains(&needle));
    title_match
        && (format == "ALL" || media.format.as_deref() == Some(format))
        && (list_status == "ALL"
            || if list_status == "UNLISTED" {
                status.is_none()
            } else {
                status == Some(list_status)
            })
}

pub(super) struct SeasonsState {
    pub season: String,
    pub year: i64,
    pub selected_season: String,
    pub selected_year: String,
    pub year_input: Option<Entity<TextInput>>,
    pub title_input: Option<Entity<TextInput>>,
    pub subscriptions: Vec<Subscription>,
    pub title_filter: String,
    pub format_filter: String,
    pub list_filter: String,
    pub season_open: bool,
    pub format_open: bool,
    pub list_open: bool,
    pub media: Vec<Media>,
    pub visible: Arc<Vec<usize>>,
    pub loading: bool,
    pub loaded_once: bool,
    pub error: String,
    pub action_error: String,
    pub request: u64,
    pub rows: Option<ListState>,
    pub columns: usize,
}

impl Default for SeasonsState {
    fn default() -> Self {
        let (season, year) = current_season();
        Self {
            selected_season: season.clone(),
            selected_year: year.to_string(),
            season,
            year,
            year_input: None,
            title_input: None,
            subscriptions: Vec::new(),
            title_filter: String::new(),
            format_filter: "ALL".into(),
            list_filter: "ALL".into(),
            season_open: false,
            format_open: false,
            list_open: false,
            media: Vec::new(),
            visible: Arc::new(Vec::new()),
            loading: false,
            loaded_once: false,
            error: String::new(),
            action_error: String::new(),
            request: 0,
            rows: None,
            columns: 0,
        }
    }
}

impl SeasonsState {
    fn replace_visible(&mut self, visible: Vec<usize>) {
        if let Some(rows) = &self.rows {
            if self.visible.as_ref() == &visible {
                rows.remeasure_items(0..rows.item_count());
            } else {
                let mut offset = rows.logical_scroll_top();
                let at_end = rows.item_count() > 0 && offset.item_ix >= rows.item_count();
                let columns = self.columns.max(1);
                let anchor = self.visible.get(offset.item_ix * columns);
                if let Some(position) =
                    anchor.and_then(|anchor| visible.iter().position(|id| id == anchor))
                {
                    offset.item_ix = position / columns;
                } else {
                    offset.item_ix = offset
                        .item_ix
                        .min(visible.len().saturating_sub(1) / columns);
                    offset.offset_in_item = px(0.);
                }
                rows.reset_with_uniform_height(
                    visible.len().div_ceil(columns),
                    px(CARD_ROW_HEIGHT),
                );
                if at_end {
                    rows.scroll_to_end();
                } else {
                    rows.scroll_to(offset);
                }
            }
        }
        self.visible = Arc::new(visible);
    }
}

impl Workspace {
    pub(super) fn dismiss_season_menu(&mut self) -> Option<FocusHandle> {
        let season_open = std::mem::take(&mut self.seasons.season_open);
        let format_open = std::mem::take(&mut self.seasons.format_open);
        let list_open = std::mem::take(&mut self.seasons.list_open);
        if season_open {
            Some(self.season_jump_focus.clone())
        } else if format_open {
            Some(self.season_format_focus.clone())
        } else if list_open {
            Some(self.season_status_focus.clone())
        } else {
            None
        }
    }

    pub(super) fn enter_seasons(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.seasons.year_input.is_none() {
            let year = self.seasons.selected_year.clone();
            let input = cx.new(|cx| TextInput::new(year, "Year", window, cx));
            self.seasons.subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, event, window, cx| match event {
                    TextInputEvent::Changed(value) => {
                        if this.stopping || this.writes_fenced() {
                            let current = this.seasons.selected_year.clone();
                            if let Some(input) = &this.seasons.year_input {
                                input.update(cx, |input, cx| input.set_value(current, cx));
                            }
                            return;
                        }
                        this.seasons.selected_year = value.clone();
                    }
                    TextInputEvent::Submitted => this.jump_season(window, cx),
                },
            ));
            self.seasons.year_input = Some(input);
        }
        if self.seasons.title_input.is_none() {
            let title = self.seasons.title_filter.clone();
            let input = cx.new(|cx| TextInput::new(title, "Filter titles…", window, cx));
            self.seasons
                .subscriptions
                .push(cx.subscribe(&input, |this, input, event, cx| {
                    if let TextInputEvent::Changed(value) = event {
                        if this.stopping || this.writes_fenced() {
                            let current = this.seasons.title_filter.clone();
                            input.update(cx, |input, cx| input.set_value(current, cx));
                            return;
                        }
                        this.seasons.title_filter = value.clone();
                        this.refresh_season_visible();
                        cx.notify();
                    }
                }));
            self.seasons.title_input = Some(input);
        }
        if self.seasons.rows.is_none() {
            let rows = ListState::new(0, ListAlignment::Top, px(290.))
                .with_uniform_item_height(px(CARD_ROW_HEIGHT));
            self.seasons.rows = Some(rows);
            self.seasons.columns = card_columns(window.bounds().size.width.into());
        }
        if !self.seasons.loaded_once && !self.seasons.loading {
            self.load_season(window, cx);
        } else {
            self.refresh_season_visible();
        }
    }

    pub(super) fn leave_seasons(&mut self) {
        self.seasons.season_open = false;
        self.seasons.format_open = false;
        self.seasons.list_open = false;
        if self.seasons.loading {
            self.seasons.loaded_once = false;
            self.seasons.loading = false;
        }
        self.seasons.request += 1;
        self.cancel_reads("seasons");
    }

    fn load_season(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.profile.is_none() || self.seasons.loading {
            return;
        }
        self.seasons.request += 1;
        let request = self.seasons.request;
        self.seasons.loading = true;
        self.seasons.loaded_once = false;
        self.seasons.error.clear();
        self.seasons.media.clear();
        self.refresh_season_visible();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let season = self.seasons.season.clone();
        let year = self.seasons.year;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_season(season.clone(), year).await });
        self.replace_reads("seasons", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if this.stopping
                    || this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.seasons.request != request
                {
                    return;
                }
                this.seasons.loading = false;
                match result {
                    Ok(media) => {
                        this.seasons.media = media;
                        this.seasons.loaded_once = true;
                        this.refresh_season_visible();
                    }
                    Err(error) => this.seasons.error = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn shift_season(&mut self, delta: i64, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.seasons.loading {
            return;
        }
        let (season, year) = shifted(&self.seasons.season, self.seasons.year, delta);
        self.seasons.season = season.clone();
        self.seasons.year = year;
        self.seasons.selected_season = season;
        self.seasons.selected_year = year.to_string();
        if let Some(input) = &self.seasons.year_input {
            input.update(cx, |input, cx| input.set_value(year.to_string(), cx));
        }
        self.load_season(window, cx);
    }

    pub(super) fn jump_season(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.seasons.loading {
            return;
        }
        let Ok(year) = self.seasons.selected_year.trim().parse::<i64>() else {
            self.seasons.error = "Enter a valid year.".into();
            cx.notify();
            return;
        };
        if !(1..=9999).contains(&year) {
            self.seasons.error = "Enter a valid year.".into();
            cx.notify();
            return;
        }
        self.seasons.year = year;
        self.seasons.season = self.seasons.selected_season.clone();
        self.load_season(window, cx);
    }

    pub(super) fn refresh_season_visible(&mut self) {
        let visible = self
            .seasons
            .media
            .iter()
            .enumerate()
            .filter_map(|(index, media)| {
                let status = self.list.entry(media.id).map(|entry| entry.status.as_str());
                season_matches(
                    media,
                    &self.seasons.title_filter,
                    &self.seasons.format_filter,
                    &self.seasons.list_filter,
                    status,
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        self.seasons.replace_visible(visible);
        self.refresh_seasons_covers();
    }

    fn prefetch_season_covers(&mut self, range: std::ops::Range<usize>) {
        let columns = self.seasons.columns.max(1);
        let start = range.start.saturating_sub(2) * columns;
        let end = range.end.saturating_add(3) * columns;
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for position in start..end.min(self.seasons.visible.len()) {
            let Some(media) = self
                .seasons
                .visible
                .get(position)
                .and_then(|index| self.seasons.media.get(*index))
            else {
                continue;
            };
            if let Some(url) = media
                .cover_large
                .as_deref()
                .or(media.cover_medium.as_deref())
            {
                let item = ((media.id, 174, 176), url.to_owned());
                if (range.start * columns..range.end * columns).contains(&position) {
                    visible.push(item);
                } else {
                    nearby.push(item);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Seasons), visible, nearby);
    }

    pub(super) fn refresh_seasons_covers(&mut self) {
        if let Some(rows) = self.seasons.rows.as_ref() {
            self.prefetch_season_covers(super::covers::visible_card_rows(rows));
        }
    }

    pub(super) fn render_seasons(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.modal_open() || self.edits_fenced() {
            self.seasons.season_open = false;
            self.seasons.format_open = false;
            self.seasons.list_open = false;
        } else {
            if self.seasons.season_open
                && self.season_jump_focus_subscription.is_some()
                && !self.season_jump_focus.contains_focused(window, cx)
            {
                self.seasons.season_open = false;
            }
            if self.seasons.format_open
                && self.season_format_focus_subscription.is_some()
                && !self.season_format_focus.contains_focused(window, cx)
            {
                self.seasons.format_open = false;
            }
            if self.seasons.list_open
                && self.season_status_focus_subscription.is_some()
                && !self.season_status_focus.contains_focused(window, cx)
            {
                self.seasons.list_open = false;
            }
        }
        let columns = card_columns(window.bounds().size.width.into());
        if columns != self.seasons.columns {
            if let Some(rows) = self.seasons.rows.as_ref() {
                reset_card_rows(
                    rows,
                    self.seasons.visible.len(),
                    self.seasons.columns,
                    columns,
                );
            }
            self.seasons.columns = columns;
        }
        let reads_fenced = self.stopping || self.writes_fenced();
        let load_disabled = self.seasons.loading || reads_fenced;
        let actions = div()
            .flex()
            .gap(px(7.))
            .child(
                button("season-prev", "← Prev", load_disabled)
                    .h(px(34.))
                    .px(px(10.))
                    .py(px(6.))
                    .bg(rgb(PANEL_2))
                    .border_color(rgb(PANEL_2))
                    .on_click(cx.listener(|this, _, window, cx| this.shift_season(-1, window, cx))),
            )
            .child(
                button("season-next", "Next →", load_disabled)
                    .h(px(34.))
                    .px(px(10.))
                    .py(px(6.))
                    .bg(rgb(PANEL_2))
                    .border_color(rgb(PANEL_2))
                    .on_click(cx.listener(|this, _, window, cx| this.shift_season(1, window, cx))),
            )
            .into_any();
        let mut page = self
            .reconcile_card_covers_on_layout(div(), Route::Seasons, cx)
            .id("seasons-page")
            .debug_selector(|| "seasons-page".into())
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
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.dismiss_season_menu().is_some() {
                    cx.notify();
                }
            }))
            .child(discovery_heading(
                "Seasons",
                format!(
                    "{} {}",
                    season_name(&self.seasons.season),
                    self.seasons.year
                ),
                "Browse the season".into(),
                Some(actions),
                f32::from(window.viewport_size().width) <= 960.,
            ));
        let fenced = self.edits_fenced();
        let mut season_select = select_with_disabled("season-jump", "Season", fenced)
            .relative()
            .when(!fenced, |control| {
                control.track_focus(&self.season_jump_focus)
            })
            .aria_expanded(!fenced && self.seasons.season_open)
            .debug_selector(|| "season-jump".into())
            .w_full()
            .child(format!("{} ▾", season_name(&self.seasons.selected_season)))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if this.edits_fenced() {
                    return;
                }
                if let Some(value) = arrow_choice(
                    &this.seasons.selected_season,
                    &SEASONS,
                    &event.keystroke.key,
                ) {
                    cx.stop_propagation();
                    this.seasons.selected_season = value.into();
                    this.seasons.season_open = false;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.edits_fenced() {
                    return;
                }
                if this.season_jump_focus_subscription.is_none() {
                    let focus = this.season_jump_focus.clone();
                    this.season_jump_focus_subscription =
                        Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                            if this.seasons.season_open {
                                this.seasons.season_open = false;
                                cx.notify();
                            }
                        }));
                }
                this.seasons.season_open = !this.seasons.season_open;
                this.seasons.format_open = false;
                this.seasons.list_open = false;
                cx.notify();
            }));
        if !fenced && self.seasons.season_open {
            let mut choices = div()
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .absolute()
                .top(px(36.))
                .left(px(0.))
                .w_full()
                .occlude()
                .flex()
                .flex_col()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE));
            for season in SEASONS {
                choices = choices.child(
                    button(
                        format!("season-option-{season}"),
                        season_name(season),
                        false,
                    )
                    .debug_selector(move || format!("season-choice-{season}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.seasons.selected_season = season.into();
                        this.seasons.season_open = false;
                        window.focus(&this.season_jump_focus, cx);
                        cx.notify();
                    })),
                );
            }
            season_select = season_select.child(deferred(choices).priority(1));
        }
        let season_select = div()
            .relative()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(season_select);
        let mut jump = div()
            .flex()
            .flex_wrap()
            .items_end()
            .gap(px(8.))
            .mb(px(16.))
            .child(
                div()
                    .w(px(144.))
                    .child(eyebrow("Season"))
                    .child(season_select),
            );
        if let Some(input) = self.seasons.year_input.clone() {
            jump = jump.child(
                div()
                    .w(px(96.))
                    .debug_selector(|| "season-year-field".into())
                    .child(eyebrow("Year"))
                    .child(input),
            );
        }
        jump = jump.child(
            button("season-go", "Go to season", load_disabled)
                .on_click(cx.listener(|this, _, window, cx| this.jump_season(window, cx))),
        );
        page = page.child(jump);
        let mut filters = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(12.))
            .mb(px(16.));
        if let Some(input) = self.seasons.title_input.clone() {
            filters = filters.child(
                div()
                    .min_w(px(170.))
                    .flex_1()
                    .debug_selector(|| "season-title-field".into())
                    .child(input),
            );
        }
        filters = filters
            .child(self.render_season_format(cx))
            .child(self.render_season_status(cx))
            .child(div().text_size(px(12.)).text_color(rgb(DIM)).child(format!(
                "{} of {} shows",
                self.seasons.visible.len(),
                self.seasons.media.len()
            )));
        page = page.child(filters);
        if !self.seasons.error.is_empty() {
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
                    .child(div().flex_1().min_w_0().child(self.seasons.error.clone()))
                    .child(
                        button("season-retry", "Retry", load_disabled)
                            .h(px(20.))
                            .px(px(0.))
                            .py(px(0.))
                            .bg(rgb(ERROR_BG))
                            .border_color(rgb(ERROR_BG))
                            .text_color(rgb(crate::theme::ACCENT))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !this.seasons.loading {
                                    this.load_season(window, cx);
                                }
                            })),
                    ),
            );
        }
        if !self.seasons.action_error.is_empty() {
            page = page.child(
                div()
                    .border_1()
                    .border_color(rgb(ERROR_EDGE))
                    .bg(rgb(ERROR_BG))
                    .text_color(rgb(ERROR))
                    .p(px(8.))
                    .child(self.seasons.action_error.clone()),
            );
        }
        if !self.list.loaded && !self.list.error.is_empty() {
            let list_retry_disabled = self.list.loading || self.list.syncing || reads_fenced;
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!("Could not read your list: {}", self.list.error))
                    .child(
                        button("season-list-retry", "Retry list", list_retry_disabled)
                            .debug_selector(|| "season-list-retry".into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !this.list.loading
                                    && !this.list.syncing
                                    && !this.stopping
                                    && !this.writes_fenced()
                                {
                                    this.load_list(window, cx);
                                }
                            })),
                    ),
            );
        }
        if self.seasons.loading {
            page = page.child(div().py(px(20.)).text_color(rgb(DIM)).child("Loading…"));
        } else if self.seasons.list_filter != "ALL"
            && !self.list.loaded
            && self.list.error.is_empty()
        {
            page = page.child(
                div()
                    .py(px(20.))
                    .text_color(rgb(DIM))
                    .child("Loading your list…"),
            );
        } else if self.seasons.visible.is_empty() && self.seasons.error.is_empty() {
            page = page.child(div().py(px(20.)).text_color(rgb(DIM)).child(
                if self.seasons.media.is_empty() {
                    "Nothing found for this season."
                } else {
                    "No shows match these filters."
                },
            ));
        }
        if !self.seasons.loading
            && (self.seasons.list_filter == "ALL" || self.list.loaded)
            && let Some(rows) = self.seasons.rows.as_ref()
        {
            let weak = cx.weak_entity();
            let columns = self.seasons.columns.max(1);
            let visible = self.seasons.visible.clone();
            page = page.child(
                list(rows.clone(), move |row, _window, app| {
                    let result = weak.read_with(app, |this, _| {
                        visible
                            .iter()
                            .skip(row * columns)
                            .take(columns)
                            .filter_map(|index| {
                                this.seasons.media.get(*index).map(|media| {
                                    (
                                        media.clone(),
                                        this.list.entry(media.id).cloned(),
                                        this.cover_images.get(&(media.id, 174, 176)).cloned(),
                                        this.discovery_status_blocked(media.id),
                                    )
                                })
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
                            false,
                            false,
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
        page.into_any()
    }

    fn render_season_format(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let label = if self.seasons.format_filter == "ALL" {
            "All formats".into()
        } else {
            self.seasons.format_filter.replace('_', " ")
        };
        let fenced = self.edits_fenced();
        let mut control = select_with_disabled("season-format", "Filter by format", fenced)
            .relative()
            .w(px(160.))
            .when(!fenced, |control| {
                control.track_focus(&self.season_format_focus)
            })
            .aria_expanded(!fenced && self.seasons.format_open)
            .debug_selector(|| "season-format".into())
            .w_full()
            .child(format!("{label} ▾"))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if this.edits_fenced() {
                    return;
                }
                let options = [
                    "ALL", "TV", "TV_SHORT", "MOVIE", "SPECIAL", "OVA", "ONA", "MUSIC",
                ];
                if let Some(value) =
                    arrow_choice(&this.seasons.format_filter, &options, &event.keystroke.key)
                {
                    cx.stop_propagation();
                    this.seasons.format_filter = value.into();
                    this.seasons.format_open = false;
                    this.refresh_season_visible();
                    cx.notify();
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.edits_fenced() {
                    return;
                }
                if this.season_format_focus_subscription.is_none() {
                    let focus = this.season_format_focus.clone();
                    this.season_format_focus_subscription =
                        Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                            if this.seasons.format_open {
                                this.seasons.format_open = false;
                                cx.notify();
                            }
                        }));
                }
                this.seasons.format_open = !this.seasons.format_open;
                this.seasons.season_open = false;
                this.seasons.list_open = false;
                cx.notify();
            }));
        if !fenced && self.seasons.format_open {
            let mut options = div()
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .absolute()
                .top(px(36.))
                .left(px(0.))
                .w_full()
                .occlude()
                .flex()
                .flex_col()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE));
            for value in ["ALL"].into_iter().chain(FORMATS) {
                let label = if value == "ALL" {
                    "All formats".into()
                } else {
                    value.replace('_', " ")
                };
                options = options.child(
                    button(format!("format-{value}"), label, false)
                        .debug_selector(move || format!("format-choice-{value}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.seasons.format_filter = value.into();
                            this.seasons.format_open = false;
                            window.focus(&this.season_format_focus, cx);
                            this.refresh_season_visible();
                            cx.notify();
                        })),
                );
            }
            control = control.child(deferred(options).priority(1));
        }
        div()
            .relative()
            .w(px(160.))
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(control)
            .into_any()
    }

    fn render_season_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let label = match self.seasons.list_filter.as_str() {
            "ALL" => "Any list status",
            "UNLISTED" => "Not on my list",
            status => status_label(status),
        };
        let fenced = self.edits_fenced();
        let mut control =
            select_with_disabled("season-list-status", "Filter by list status", fenced)
                .relative()
                .w(px(176.))
                .when(!fenced, |control| {
                    control.track_focus(&self.season_status_focus)
                })
                .aria_expanded(!fenced && self.seasons.list_open)
                .debug_selector(|| "season-list-status".into())
                .w_full()
                .child(format!("{label} ▾"))
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if this.edits_fenced() {
                        return;
                    }
                    let options = [
                        "ALL",
                        "UNLISTED",
                        "CURRENT",
                        "PLANNING",
                        "COMPLETED",
                        "PAUSED",
                        "DROPPED",
                        "REPEATING",
                    ];
                    if let Some(value) =
                        arrow_choice(&this.seasons.list_filter, &options, &event.keystroke.key)
                    {
                        cx.stop_propagation();
                        this.seasons.list_filter = value.into();
                        this.seasons.list_open = false;
                        this.refresh_season_visible();
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.edits_fenced() {
                        return;
                    }
                    if this.season_status_focus_subscription.is_none() {
                        let focus = this.season_status_focus.clone();
                        this.season_status_focus_subscription =
                            Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                                if this.seasons.list_open {
                                    this.seasons.list_open = false;
                                    cx.notify();
                                }
                            }));
                    }
                    this.seasons.list_open = !this.seasons.list_open;
                    this.seasons.season_open = false;
                    this.seasons.format_open = false;
                    cx.notify();
                }));
        if !fenced && self.seasons.list_open {
            let mut options = div()
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .absolute()
                .top(px(36.))
                .left(px(0.))
                .w_full()
                .occlude()
                .flex()
                .flex_col()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE));
            for value in ["ALL", "UNLISTED"].into_iter().chain(STATUSES) {
                let label = match value {
                    "ALL" => "Any list status",
                    "UNLISTED" => "Not on my list",
                    _ => status_label(value),
                };
                options = options.child(
                    button(format!("list-filter-{value}"), label, false)
                        .debug_selector(move || format!("status-choice-{value}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.seasons.list_filter = value.into();
                            this.seasons.list_open = false;
                            window.focus(&this.season_status_focus, cx);
                            this.refresh_season_visible();
                            cx.notify();
                        })),
                );
            }
            control = control.child(deferred(options).priority(1));
        }
        div()
            .relative()
            .w(px(176.))
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(control)
            .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn end_request_survives_filtering_before_layout() {
        let rows = ListState::new(0, ListAlignment::Top, px(0.));
        let mut state = SeasonsState {
            rows: Some(rows.clone()),
            columns: 3,
            ..Default::default()
        };
        state.replace_visible((0..90).collect());
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
        rows.scroll_to_end();
        state.replace_visible((0..90).step_by(2).collect());
        assert_eq!(rows.logical_scroll_top().item_ix, 15);
        state.replace_visible(vec![]);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    #[test]
    fn filters_and_reflow_keep_the_top_media_and_inset() {
        let rows = ListState::new(0, ListAlignment::Top, px(290.));
        let mut state = SeasonsState {
            rows: Some(rows.clone()),
            columns: 3,
            ..Default::default()
        };
        state.replace_visible((0..300).collect());
        assert_eq!(rows.max_offset_for_scrollbar().y, px(30_200.));
        rows.scroll_to(gpui::ListOffset {
            item_ix: 50,
            offset_in_item: px(22.),
        });
        let filtered = (0..300).step_by(2).collect::<Vec<_>>();
        state.replace_visible(filtered.clone());
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 25,
                offset_in_item: px(22.)
            }
        );
        state.replace_visible(filtered);
        assert_eq!(rows.logical_scroll_top().offset_in_item, px(22.));
        reset_card_rows(&rows, state.visible.len(), 3, 1);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 75,
                offset_in_item: px(22.)
            }
        );
        state.columns = 1;
        state.replace_visible(vec![1, 3]);
        assert_eq!(
            rows.logical_scroll_top(),
            gpui::ListOffset {
                item_ix: 1,
                offset_in_item: px(0.)
            }
        );
        state.replace_visible(vec![]);
        assert_eq!(rows.logical_scroll_top(), gpui::ListOffset::default());
    }

    #[test]
    fn season_shift_and_filters_follow_account_status() {
        assert_eq!(shifted("FALL", 2026, 1), ("WINTER".into(), 2027));
        assert_eq!(shifted("WINTER", 2026, -1), ("FALL".into(), 2025));
        assert_eq!(
            arrow_choice("SPRING", &SEASONS, "arrowdown"),
            Some("SUMMER")
        );
        assert_eq!(arrow_choice("WINTER", &SEASONS, "arrowup"), Some("WINTER"));
        let media = Media {
            id: 1,
            title_english: Some("Other".into()),
            title_native: Some("原作".into()),
            format: Some("TV_SHORT".into()),
            ..Default::default()
        };
        assert!(season_matches(
            &media,
            "原",
            "TV_SHORT",
            "CURRENT",
            Some("CURRENT")
        ));
        assert!(!season_matches(
            &media,
            "原",
            "TV",
            "CURRENT",
            Some("CURRENT")
        ));
        assert!(season_matches(&media, "", "ALL", "UNLISTED", None));
        assert!(!season_matches(
            &media,
            "",
            "ALL",
            "UNLISTED",
            Some("PLANNING")
        ));
    }

    #[gpui::test]
    fn seasons_controls_disable_and_menus_close_while_fenced(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::fixture;
        use gpui::AccessibleAction;
        use kurisu_core::models::User;

        let (core, _runtime, _dir) = fixture();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(gpui::size(px(1100.), px(900.)));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 7,
                ..Default::default()
            });
            w.route = Route::Seasons;
            w.seasons.loaded_once = true;
            w.seasons.season_open = true;
            w.seasons.format_open = true;
            w.seasons.list_open = true;
            w.update_busy = true;
            w.close_ready = true;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert!(!w.seasons.season_open);
            assert!(!w.seasons.format_open);
            assert!(!w.seasons.list_open);
        });
        let tree = cx.take_accessibility_tree_update().unwrap();
        for label in ["← Prev", "Next →", "Go to season"] {
            let (_, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing seasons button {label}"));
            assert!(
                node.is_disabled(),
                "{label} should be disabled while fenced"
            );
            assert!(!node.supports_action(AccessibleAction::Click));
        }
        for label in ["Season", "Filter by format", "Filter by list status"] {
            let (_, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing seasons combobox {label}"));
            assert!(node.is_disabled(), "{label} should disable while fenced");
            assert_eq!(node.is_expanded(), Some(false));
            assert!(!node.supports_action(AccessibleAction::Click));
            assert!(!node.supports_action(AccessibleAction::Focus));
        }

        view.update_in(cx, |w, _, cx| {
            w.close_ready = false;
            cx.notify();
        });
        cx.update(|win, app| win.draw(app).clear(app));
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        for label in ["Season", "Filter by format", "Filter by list status"] {
            let (_, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing seasons combobox {label}"));
            assert!(
                node.is_disabled(),
                "{label} should remain disabled while update_busy"
            );
            assert_eq!(node.is_expanded(), Some(false));
            assert!(!node.supports_action(AccessibleAction::Click));
        }

        view.update_in(cx, |w, _, cx| {
            w.update_busy = false;
            w.list.loaded = false;
            w.list.error = "offline".into();
            cx.notify();
        });
        cx.update(|win, app| win.draw(app).clear(app));
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        for label in ["Season", "Filter by format", "Filter by list status"] {
            let (_, node) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing seasons combobox {label}"));
            assert!(!node.is_disabled(), "{label} should re-enable after fence");
        }
        let (retry_id, retry_enabled) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Retry list"))
            .map(|(id, node)| (*id, node))
            .expect("missing seasons Retry list button");
        assert!(!retry_enabled.is_disabled());
        let retry_bounds = cx
            .debug_bounds("season-list-retry")
            .expect("season-list-retry bounds");
        let before_request = view.update_in(cx, |w, _, _| {
            w.list.loading = true;
            w.list_request
        });
        cx.simulate_click(retry_bounds.center(), gpui::Modifiers::default());
        view.read_with(cx, |w, _| {
            assert_eq!(
                w.list_request, before_request,
                "disabled Retry list must not restart load_list while already loading"
            );
        });
        view.update_in(cx, |w, _, _| {
            w.list.loading = false;
            w.barrier = Some(super::super::BarrierAction::Install);
        });
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: AccessibleAction::Click,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: retry_id,
            data: None,
        });
        view.read_with(cx, |w, _| {
            assert_eq!(
                w.list_request, before_request,
                "fenced Retry list must not start load_list during a barrier"
            );
            assert!(w.list_read.is_none());
        });

        view.update_in(cx, |w, _, cx| {
            w.barrier = None;
            w.list.syncing = true;
            cx.notify();
        });
        cx.update(|win, app| win.draw(app).clear(app));
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (_, retry_list) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Retry list"))
            .expect("missing seasons Retry list button");
        assert!(retry_list.is_disabled());
        assert!(!retry_list.supports_action(AccessibleAction::Click));
    }
}
