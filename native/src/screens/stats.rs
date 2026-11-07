use gpui::{
    AnyElement, Context, Div, InteractiveElement, ScrollHandle, Window, div, prelude::*, px, rgb,
};
use kurisu_core::models::UserStats;

use super::{Workspace, joined};
use crate::components::{button, eyebrow};
use crate::theme::{ACCENT, DIM, EDGE, ERROR, INK, PANEL, PANEL_2};

pub(super) struct StatsState {
    data: Option<UserStats>,
    loading: bool,
    error: String,
    request: u64,
    scroll: ScrollHandle,
}

impl Default for StatsState {
    fn default() -> Self {
        Self {
            data: None,
            loading: false,
            error: String::new(),
            request: 0,
            scroll: ScrollHandle::new(),
        }
    }
}

impl Workspace {
    pub(super) fn enter_stats(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.stats.loading {
            self.load_stats(window, cx);
        }
    }

    pub(super) fn refresh_stats_covers(&mut self) {}

    fn load_stats(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.stats.loading {
            return;
        }
        self.stats.request += 1;
        let request = self.stats.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.stats.loading = true;
        self.stats.error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_user_stats().await });
        self.replace_reads("stats", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.stats.request != request
                {
                    return;
                }
                this.stats.loading = false;
                match result {
                    Ok(data) => {
                        this.stats.data = Some(data);
                        this.stats.error.clear();
                    }
                    Err(error) => this.stats.error = error,
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_stats(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) <= 960.;
        let mut page = div()
            .id("stats-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.stats.scroll)
            .px(px(if compact { 20. } else { 28. }))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(14.));
        page = page.child(super::watching::route_header(
            "Your activity",
            "Stats",
            "Your watching history in numbers",
            Some(
                button(
                    "stats-refresh",
                    if self.stats.loading {
                        "Loading…"
                    } else {
                        "↻ Refresh"
                    },
                    self.stats.loading,
                )
                .border_color(rgb(PANEL_2))
                .rounded(px(4.))
                .on_click(cx.listener(|this, _, window, cx| {
                    if !this.stats.loading {
                        this.load_stats(window, cx);
                    }
                }))
                .into_any(),
            ),
            compact,
            14.,
        ));
        if !self.stats.error.is_empty() {
            page = page.child(
                div()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(0x57312e))
                    .bg(rgb(0x190c0b))
                    .p(px(8.))
                    .text_size(px(13.))
                    .text_color(rgb(ERROR))
                    .child(self.stats.error.clone()),
            );
        }
        let Some(stats) = self.stats.data.as_ref() else {
            return page
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .h(px(100.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(if self.stats.loading {
                            "Loading…"
                        } else {
                            "Statistics are unavailable. Use Refresh to try again."
                        }),
                )
                .into_any();
        };
        if let Some(warning) = stats.warning.as_deref() {
            let when = stats
                .cached_at
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
                .map(|date| {
                    format!(
                        " from {}",
                        date.with_timezone(&chrono::Local)
                            .format("%b %-d, %Y %-I:%M %p")
                    )
                })
                .unwrap_or_default();
            page = page.child(panel().child(format!("Showing saved statistics{when}. {warning}")));
        }
        if stats.count == 0 {
            page = page.child(
                div()
                    .text_color(rgb(DIM))
                    .child("AniList has no anime statistics for your list yet."),
            );
        }
        let summary = div()
            .flex()
            .flex_wrap()
            .gap(px(9.))
            .child(metric("Anime", stats.count.to_string()))
            .child(metric("Episodes", stats.episodes_watched.to_string()))
            .child(metric(
                "Days watched",
                format!("{:.1}", stats.minutes_watched as f64 / 1440.),
            ))
            .child(metric(
                if stats.standard_deviation > 0. {
                    "Mean score · σ"
                } else {
                    "Mean score"
                },
                if stats.mean_score > 0. {
                    if stats.standard_deviation > 0. {
                        format!("{} · {}", stats.mean_score, stats.standard_deviation)
                    } else {
                        stats.mean_score.to_string()
                    }
                } else {
                    "—".into()
                },
            ));
        page = page.child(summary);
        if !stats.statuses.is_empty() {
            let order = [
                "CURRENT",
                "REPEATING",
                "COMPLETED",
                "PAUSED",
                "DROPPED",
                "PLANNING",
            ];
            let mut rows = stats.statuses.clone();
            rows.sort_by_key(|row| {
                order
                    .iter()
                    .position(|status| *status == row.status)
                    .unwrap_or(order.len())
            });
            let max = rows.iter().map(|row| row.count).max().unwrap_or(1).max(1);
            let mut body = panel().flex().flex_col().gap(px(7.));
            for row in rows {
                let known = super::state::status_label(&row.status);
                body = body.child(bar_row(
                    if known == "Unknown" {
                        &row.status
                    } else {
                        known
                    },
                    row.count,
                    max,
                    None,
                ));
            }
            page = page.child(section("By status", body));
        }
        if !stats.scores.is_empty() {
            let mut rows = stats.scores.clone();
            rows.sort_by_key(|row| row.score);
            let max = rows.iter().map(|row| row.count).max().unwrap_or(1).max(1);
            let mut body = panel().flex().flex_col().gap(px(7.));
            for row in rows {
                body = body.child(bar_row(&row.score.to_string(), row.count, max, None));
            }
            page = page.child(section("Score distribution", body));
        }
        if !stats.genres.is_empty() {
            let max = stats
                .genres
                .iter()
                .map(|row| row.count)
                .max()
                .unwrap_or(1)
                .max(1);
            let mut body = panel().flex().flex_col().gap(px(7.));
            for row in &stats.genres {
                body = body.child(bar_row(
                    &row.genre,
                    row.count,
                    max,
                    Some(format!(
                        "{} · {}h",
                        row.count,
                        (row.minutes_watched as f64 / 60.).round() as i64
                    )),
                ));
            }
            page = page.child(section("Top genres", body));
        }
        if !stats.formats.is_empty() {
            let mut body = div().flex().flex_wrap().gap(px(6.));
            for row in &stats.formats {
                body = body.child(
                    div()
                        .bg(rgb(PANEL_2))
                        .p(px(7.))
                        .text_size(px(12.))
                        .child(format!("{} {}", row.format, row.count)),
                );
            }
            page = page.child(section("Formats", body));
        }
        if stats.release_years.len() > 1 {
            let max = stats
                .release_years
                .iter()
                .map(|row| row.count)
                .max()
                .unwrap_or(1)
                .max(1);
            let mut bars = div()
                .id("stats-years")
                .h(px(112.))
                .flex()
                .items_end()
                .gap(px(3.))
                .overflow_x_scroll();
            for row in &stats.release_years {
                bars = bars.child(
                    div()
                        .id(format!("stats-year-{}", row.year))
                        .aria_label(format!("{}: {}", row.year, row.count))
                        .min_w(px(8.))
                        .flex_1()
                        .h(px((row.count as f32 / max as f32 * 88.).max(2.)))
                        .bg(rgb(ACCENT)),
                );
            }
            let first = stats.release_years.first().unwrap().year;
            let last = stats.release_years.last().unwrap().year;
            page = page.child(section(
                "By release year",
                panel().child(bars).child(
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(11.))
                        .text_color(rgb(DIM))
                        .child(first.to_string())
                        .child(last.to_string()),
                ),
            ));
        }
        page.into_any()
    }
}

fn panel() -> Div {
    div()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(EDGE))
        .rounded(px(8.))
        .p(px(12.))
}
fn section(label: &str, body: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(7.))
        .child(eyebrow(label.to_uppercase()))
        .child(body)
}
fn metric(label: &str, value: String) -> Div {
    panel()
        .min_w(px(140.))
        .flex_1()
        .child(div().text_size(px(23.)).text_color(rgb(INK)).child(value))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(DIM))
                .child(label.to_string()),
        )
}
fn bar_row(label: &str, count: i64, max: i64, detail: Option<String>) -> Div {
    let width = (count as f32 / max as f32 * 100.).round().max(2.);
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .child(
            div()
                .w(px(120.))
                .min_w_0()
                .text_color(rgb(DIM))
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .h(px(13.))
                .bg(rgb(PANEL_2))
                .rounded(px(3.))
                .child(
                    div()
                        .w(gpui::relative(width / 100.))
                        .h_full()
                        .bg(rgb(ACCENT))
                        .rounded(px(3.)),
                ),
        )
        .child(
            div()
                .w(px(85.))
                .text_color(rgb(DIM))
                .child(detail.unwrap_or_else(|| count.to_string())),
        )
}
