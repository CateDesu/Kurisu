use chrono::Local;
use gpui::{
    AnyElement, HighlightStyle, InteractiveElement, RenderImage, Role, StyledText, div, img,
    linear_color_stop, linear_gradient, prelude::*, px, rgb, rgba, svg,
};
use kurisu_core::Platform;
use kurisu_core::models::Media;
use std::sync::Arc;

use super::Workspace;
use super::search::discovery_heading;
use super::state::status_label;
use crate::components::{button, eyebrow};
use crate::contracts::Route;
use crate::theme::{ACCENT, BASE, DIM, EDGE, ERROR, PANEL, PANEL_2, serif_font};

impl Workspace {
    fn detail_entry_retry_current(
        &self,
        id: i64,
        request: u64,
        generation: u64,
        entry_revision: u64,
    ) -> bool {
        generation == self.generation
            && self.route == Route::Anime(id)
            && self.detail.current(id, request)
            && (!self.list.loaded || self.list.revision() == entry_revision)
    }

    pub(super) fn render_detail(
        &mut self,
        id: i64,
        window: &gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        let scroll = self.detail_scroll.entry(id).or_default().clone();
        let detail = self.detail.detail.clone();
        let entry = self.detail.entry.clone();
        let compact = f32::from(window.viewport_size().width) <= 960.;
        let mut root = div()
            .id("detail-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .flex()
            .flex_col();
        let mut page = div()
            .id("detail-content")
            .relative()
            .px(px(if compact { 20. } else { 28. }))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(12.));
        if detail.is_none() {
            page = page.child(discovery_heading(
                "Your collection",
                "Anime details".into(),
                String::new(),
                None,
                compact,
            ));
            if self.detail.loading {
                page = page.child(div().text_color(rgb(DIM)).child("Loading…"));
            } else {
                page = page
                    .child(div().text_color(rgb(ERROR)).child(
                        if self.detail.detail_error.is_empty() {
                            "This anime could not be loaded.".into()
                        } else {
                            self.detail.detail_error.clone()
                        },
                    ))
                    .child(button("retry-detail", "Retry details", false).on_click(
                        cx.listener(move |this, _, window, cx| this.load_detail(id, window, cx)),
                    ));
            }
            return root.child(page).into_any();
        }
        let detail = detail.unwrap();
        let media = &detail.media;
        if media.banner_image.is_some() {
            let height = if compact { 160. } else { 192. };
            let mut banner = div()
                .id("detail-banner")
                .debug_selector(|| "detail-banner".into())
                .relative()
                .w_full()
                .h(px(height))
                .flex_shrink_0()
                .overflow_hidden()
                .bg(rgb(PANEL_2));
            if let Some(image) = self.cover_images.get(&(id, 800, 192)).cloned() {
                banner = banner.child(
                    img(image)
                        .absolute()
                        .size_full()
                        .object_fit(gpui::ObjectFit::Cover)
                        .opacity(0.5),
                );
            }
            root = root.child(banner.child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(rgba(0x00000000), 0.),
                linear_color_stop(rgb(BASE), 1.),
            ))));
            page = page.mt(px(-80.));
        }
        let title = media.display_title();
        let cover = self.cover_images.get(&(id, 128, 184)).cloned();
        let mut intro = div().flex().gap(px(20.)).items_start();
        intro = intro.child(cover_or_placeholder(cover, 128., 184.));
        let mut heading_area = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .relative()
                    .min_h(px(62.))
                    .border_b_1()
                    .border_color(rgb(EDGE))
                    .child(eyebrow("ANIME DETAILS"))
                    .child(detail_title(&title))
                    .child(
                        svg()
                            .path("icons/gearwork.svg")
                            .absolute()
                            .right(px(0.))
                            .top(px(0.))
                            .w(px(120.))
                            .h(px(70.))
                            .opacity(0.5)
                            .text_color(rgb(ACCENT)),
                    ),
            );
        if let Some(romaji) = media
            .title_romaji
            .as_deref()
            .filter(|romaji| *romaji != title)
        {
            heading_area = heading_area.child(div().text_color(rgb(DIM)).child(romaji.to_string()));
        }
        let meta = media_metadata(media);
        if !meta.is_empty() {
            heading_area = heading_area.child(
                div()
                    .text_color(rgb(DIM))
                    .text_size(px(13.))
                    .whitespace_normal()
                    .child(meta),
            );
        }
        if let Some(airing) = airing_label(media, super::now_seconds()) {
            heading_area = heading_area.child(
                div()
                    .text_color(rgb(ACCENT))
                    .text_size(px(14.))
                    .child(airing),
            );
        }
        heading_area = heading_area.child(
            button("open-anilist", "Open on AniList ↗", false)
                .bg(rgb(PANEL))
                .border_color(rgb(PANEL))
                .text_color(rgb(ACCENT))
                .px(px(0.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if id <= 0 {
                        return;
                    }
                    if let Err(error) = crate::platform::NativePlatform
                        .open_url(&format!("https://anilist.co/anime/{id}"))
                    {
                        this.global_error = error;
                        cx.notify();
                    }
                })),
        );
        intro = intro.child(heading_area);
        page = page.child(intro);
        if !self.detail.detail_error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(self.detail.detail_error.clone()),
            );
        }
        if detail.warning.is_some() || !detail.unavailable_sections.is_empty() {
            let warning = detail.warning.as_deref().unwrap_or("");
            page = page.child(
                div()
                    .border_1()
                    .border_color(rgb(EDGE))
                    .bg(rgb(PANEL))
                    .p(px(10.))
                    .child(format!(
                        "Showing saved details. {warning} {}",
                        if detail.unavailable_sections.is_empty() {
                            "".into()
                        } else {
                            format!(
                                "Current {} data is unavailable.",
                                detail.unavailable_sections.join(", ")
                            )
                        }
                    ))
                    .child(
                        button("retry-partial-detail", "Retry details", self.detail.loading)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if this.detail.loading {
                                    return;
                                }
                                this.load_detail(id, window, cx)
                            })),
                    ),
            );
        }
        if !self.detail.entry_error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!(
                        "Your list entry is unavailable. {}",
                        self.detail.entry_error
                    ))
                    .child(
                        button("retry-entry", "Retry list entry", false).on_click(cx.listener(
                            move |this, _, window, cx| this.reload_detail_entry(id, window, cx),
                        )),
                    ),
            );
        }
        if let Some(genres) = media.genres.as_ref() {
            let mut pills = div().flex().flex_wrap().gap(px(5.));
            for genre in genres {
                pills = pills.child(
                    div()
                        .bg(rgb(PANEL_2))
                        .px(px(8.))
                        .py(px(3.))
                        .text_size(px(11.))
                        .child(genre.clone()),
                );
            }
            page = page.child(pills);
        }
        let mut list_card = div()
            .border_1()
            .border_color(rgb(EDGE))
            .bg(rgb(PANEL))
            .p(px(12.))
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.));
        if let Some(entry) = entry {
            let progress = self
                .progress
                .edit(self.generation, id)
                .map_or(entry.progress, |edit| edit.pending);
            list_card =
                list_card
                    .child(
                        div()
                            .text_color(rgb(ACCENT))
                            .child(format!("✓ {}", status_label(&entry.status))),
                    )
                    .child(
                        button("detail-less", "−", progress <= 0)
                            .aria_label("One less episode")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.step_progress(id, -1, window, cx)
                            })),
                    )
                    .child(div().child(if let Some(total) = media.episodes {
                        format!("{progress}/{total}")
                    } else {
                        progress.to_string()
                    }))
                    .child(
                        button(
                            "detail-more",
                            "+",
                            media.episodes.is_some_and(|total| progress >= total),
                        )
                        .aria_label("One more episode")
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.step_progress(id, 1, window, cx),
                        )),
                    );
            if let Some(score) = score_label(
                entry.score,
                self.profile
                    .as_ref()
                    .and_then(|user| user.score_format.as_deref()),
            ) {
                list_card =
                    list_card.child(div().text_size(px(14.)).text_color(rgb(DIM)).child(score));
            }
            if let Some(file) = self
                .library_files
                .iter()
                .find(|file| file.media_id == Some(id) && file.episode == Some(entry.progress + 1))
            {
                let path = file.path.clone();
                let episode = file.episode.unwrap_or(entry.progress + 1);
                list_card = list_card.child(
                    button("play-next-local", format!("Play Ep {episode}"), false).on_click(
                        cx.listener(move |this, _, _, cx| {
                            if let Err(error) = crate::platform::NativePlatform
                                .open_path(std::path::Path::new(&path))
                            {
                                this.global_error = error;
                                cx.notify();
                            }
                        }),
                    ),
                );
            }
            list_card = list_card.child(button("detail-edit", "Edit entry", false).on_click(
                cx.listener(move |this, _, window, cx| this.open_editor(id, window, cx)),
            ));
        } else if self.detail.entry_available {
            for (status, label) in [
                ("CURRENT", "Watching"),
                ("PLANNING", "Plan to watch"),
                ("COMPLETED", "Completed"),
            ] {
                list_card = list_card.child(
                    button(
                        format!("add-{status}"),
                        label,
                        self.editor_adding.contains(&id),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_media(id, status, window, cx)
                    })),
                );
            }
        } else {
            list_card = list_card.child(div().text_color(rgb(DIM)).child("List entry unavailable"));
        }
        page = page.child(list_card);
        let description = plain_description(media.description.as_deref().unwrap_or(""));
        if !description.is_empty() {
            let mut synopsis = div()
                .text_color(rgb(DIM))
                .text_size(px(14.))
                .whitespace_normal()
                .line_height(px(21.));
            if !self.detail.description_expanded {
                synopsis = synopsis.line_clamp(6);
            }
            page = page
                .child(eyebrow("SYNOPSIS"))
                .child(synopsis.child(description.clone()));
            if description.len() > 420 {
                page = page.child(
                    button(
                        "detail-description-toggle",
                        if self.detail.description_expanded {
                            "Show less"
                        } else {
                            "Show more"
                        },
                        false,
                    )
                    .bg(rgb(PANEL))
                    .border_color(rgb(PANEL))
                    .text_color(rgb(ACCENT))
                    .px(px(0.))
                    .py(px(0.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.detail.description_expanded = !this.detail.description_expanded;
                        cx.notify();
                    })),
                );
            }
        }
        if !detail.relations.is_empty() {
            page = page.child(eyebrow("RELATED"));
            let mut relations = div()
                .id("detail-relations")
                .flex()
                .gap(px(10.))
                .overflow_x_scroll()
                .pb(px(4.));
            for relation in &detail.relations {
                let related_id = relation.media.id;
                let cover = self.cover_images.get(&(related_id, 112, 160)).cloned();
                relations = relations.child(
                    div()
                        .id(format!("relation-{related_id}"))
                        .role(gpui::Role::Button)
                        .aria_label(format!("Open {}", relation.media.display_title()))
                        .focusable()
                        .tab_stop(true)
                        .w(px(112.))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .cursor_pointer()
                        .on_key_down(cx.listener(
                            move |this, event: &gpui::KeyDownEvent, window, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space" | " ") {
                                    cx.stop_propagation();
                                    this.navigate(Route::Anime(related_id), window, cx);
                                }
                            },
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate(Route::Anime(related_id), window, cx)
                        }))
                        .child(cover_or_placeholder(cover, 112., 160.))
                        .child(
                            div()
                                .text_color(rgb(ACCENT))
                                .text_size(px(11.))
                                .child(relation.relation.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .whitespace_normal()
                                .line_height(px(16.))
                                .line_clamp(2)
                                .text_ellipsis()
                                .child(relation.media.display_title()),
                        ),
                );
            }
            page = page.child(relations);
        }
        if !detail.characters.is_empty() {
            page = page.child(eyebrow("CHARACTERS"));
            let mut cast = div()
                .id("detail-characters")
                .flex()
                .gap(px(10.))
                .overflow_x_scroll()
                .pb(px(4.));
            for (index, character) in detail.characters.iter().enumerate().take(40) {
                let portrait = self
                    .cover_images
                    .get(&(-1 - index as i64, 112, 160))
                    .cloned();
                let mut person = div()
                    .id(format!("detail-character-{index}"))
                    .role(gpui::Role::Group)
                    .aria_label(character.name.clone())
                    .w(px(112.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(cover_or_placeholder(portrait, 112., 160.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .whitespace_normal()
                            .line_height(px(16.))
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(character.name.clone()),
                    );
                if let Some(voice) = character.va_name.as_ref() {
                    person = person.child(
                        div()
                            .text_color(rgb(DIM))
                            .text_size(px(12.))
                            .truncate()
                            .child(voice.clone()),
                    );
                }
                cast = cast.child(person);
            }
            page = page.child(cast);
        }
        if !detail.staff.is_empty() {
            page = page.child(eyebrow("STAFF"));
            let mut staff = div()
                .id("detail-staff")
                .flex()
                .gap(px(10.))
                .overflow_x_scroll()
                .pb(px(4.));
            for (index, person) in detail.staff.iter().enumerate().take(40) {
                let portrait = self
                    .cover_images
                    .get(&(-200_001 - index as i64, 112, 160))
                    .cloned();
                let mut card = div()
                    .id(format!("detail-staff-{index}"))
                    .role(gpui::Role::Group)
                    .aria_label(person.name.clone())
                    .w(px(112.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(cover_or_placeholder(portrait, 112., 160.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .whitespace_normal()
                            .line_height(px(16.))
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(person.name.clone()),
                    );
                if let Some(role) = person.role.as_ref() {
                    card = card.child(
                        div()
                            .text_color(rgb(DIM))
                            .text_size(px(12.))
                            .truncate()
                            .child(role.clone()),
                    );
                }
                staff = staff.child(card);
            }
            page = page.child(staff);
        }
        if !self.detail.recommendations.is_empty() {
            page = page.child(eyebrow("YOU MIGHT ALSO LIKE"));
        }
        if !self.detail.recommendation_error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(DIM))
                    .child(format!(
                        "Recommendations are unavailable. {}",
                        self.detail.recommendation_error
                    ))
                    .child(
                        button(
                            "retry-recommendations",
                            "Retry recommendations",
                            self.detail.recommendations_loading,
                        )
                        .bg(rgb(PANEL))
                        .border_color(rgb(PANEL))
                        .text_color(rgb(ACCENT))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.reload_detail_recommendations(id, window, cx)
                            },
                        )),
                    ),
            );
        } else if self.detail.recommendations_loading {
            page = page.child(div().text_color(rgb(DIM)).child("Loading recommendations…"));
        } else if self.detail.recommendations.is_empty() {
            page = page.child(div().text_color(rgb(DIM)).child("No recommendations yet."));
        }
        let mut recommendations = div()
            .id("detail-recommendations")
            .flex()
            .gap(px(10.))
            .overflow_x_scroll()
            .pb(px(4.));
        for recommendation in &self.detail.recommendations {
            let rec_id = recommendation.id;
            let cover = self.cover_images.get(&(rec_id, 112, 160)).cloned();
            recommendations = recommendations.child(
                div()
                    .w(px(112.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id(format!("recommendation-{rec_id}"))
                            .role(gpui::Role::Button)
                            .aria_label(format!("Open {}", recommendation.display_title()))
                            .focusable()
                            .tab_stop(true)
                            .cursor_pointer()
                            .on_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, window, cx| {
                                    if matches!(
                                        event.keystroke.key.as_str(),
                                        "enter" | "space" | " "
                                    ) {
                                        cx.stop_propagation();
                                        this.navigate(Route::Anime(rec_id), window, cx);
                                    }
                                },
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate(Route::Anime(rec_id), window, cx)
                            }))
                            .child(cover_or_placeholder(cover, 112., 160.)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .whitespace_normal()
                            .line_height(px(16.))
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(recommendation.display_title()),
                    )
                    .child(
                        button(
                            format!("add-rec-{rec_id}"),
                            if self.added_recommendations.contains(&rec_id) {
                                "✓ Added"
                            } else {
                                "Add to planning"
                            },
                            self.editor_adding.contains(&rec_id)
                                || self.added_recommendations.contains(&rec_id),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.add_media(rec_id, "PLANNING", window, cx)
                            },
                        )),
                    ),
            );
        }
        root.child(page.child(recommendations)).into_any()
    }

    fn reload_detail_entry(
        &mut self,
        id: i64,
        window: &gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.route != Route::Anime(id) || self.detail.media_id != Some(id) {
            return;
        }
        let request = self.detail.request();
        let generation = self.generation;
        let entry_revision = self.list.revision();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_entry(id).await });
        self.replace_reads("detail-entry", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = super::joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if !this.detail_entry_retry_current(id, request, generation, entry_revision) {
                    return;
                }
                this.detail.finish_entry(id, request, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_detail_recommendations(
        &mut self,
        id: i64,
        window: &gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.detail.recommendations_loading
            || self.route != Route::Anime(id)
            || self.detail.media_id != Some(id)
        {
            return;
        }
        let request = self.detail.request();
        let generation = self.generation;
        self.detail.recommendations_loading = true;
        self.detail.recommendation_error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_recommendations(id).await });
        self.replace_reads("detail-recommendations", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = super::joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.generation
                    || this.route != Route::Anime(id)
                    || !this.detail.current(id, request)
                {
                    return;
                }
                this.ensure_cover_scale(window.scale_factor());
                if let Ok(items) = &result {
                    for media in items {
                        if let Some(url) = media.cover_medium.as_deref() {
                            this.request_cover(media.id, url, 112, 160);
                        }
                    }
                }
                this.detail.finish_recommendations(id, request, result);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn detail_title(title: &str) -> AnyElement {
    let text = format!("{title}.");
    let dot = title.len()..text.len();
    div()
        .id("detail-title")
        .debug_selector(|| "detail-title".into())
        .role(Role::Heading)
        .aria_level(1)
        .aria_label(title.to_string())
        .w_full()
        .min_w_0()
        .font(serif_font())
        .text_size(px(28.))
        .line_height(px(32.2))
        .whitespace_normal()
        .child(StyledText::new(text).with_highlights([(
            dot,
            HighlightStyle {
                color: Some(rgb(ACCENT).into()),
                ..HighlightStyle::default()
            },
        )]))
        .into_any()
}

fn cover_or_placeholder(cover: Option<Arc<RenderImage>>, width: f32, height: f32) -> AnyElement {
    if let Some(image) = cover {
        img(image)
            .w(px(width))
            .h(px(height))
            .rounded(px(4.))
            .object_fit(gpui::ObjectFit::Cover)
            .into_any()
    } else {
        div()
            .w(px(width))
            .h(px(height))
            .bg(rgb(PANEL_2))
            .rounded(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(0x89a79a))
            .text_size(px(24.))
            .child("ク")
            .into_any()
    }
}

fn score_label(score: Option<f64>, format: Option<&str>) -> Option<String> {
    let score = score.filter(|score| *score > 0.)?;
    Some(match format {
        Some("POINT_3") => ["", "😞", "😐", "😊"]
            .get(score.round() as usize)
            .map_or_else(|| score.to_string(), |label| (*label).to_string()),
        Some("POINT_5") => "★".repeat(score.round().clamp(0., 5.) as usize),
        _ => format!("★ {score}"),
    })
}

fn airing_label(media: &Media, now: i64) -> Option<String> {
    let episode = media.next_airing_episode.filter(|episode| *episode > 0)?;
    let airing_at = media.next_airing_at?;
    let seconds = airing_at.saturating_sub(now);
    if seconds <= 0 {
        return None;
    }
    let when = if seconds < 3600 {
        format!("in {}m", ((seconds + 30) / 60).max(1))
    } else if seconds < 86400 {
        format!("in {}h", (seconds + 3599) / 3600)
    } else if seconds < 604800 {
        format!("in {}d", (seconds + 86399) / 86400)
    } else {
        let date = chrono::DateTime::from_timestamp(airing_at, 0)?;
        date.with_timezone(&Local).format("%b %-d").to_string()
    };
    Some(format!("Ep {episode} airs {when}"))
}

fn media_metadata(media: &Media) -> String {
    let mut parts = Vec::new();
    if let Some(format) = &media.format {
        parts.push(format.replace('_', " "));
    }
    if let Some(episodes) = media.episodes {
        parts.push(format!("{episodes} eps"));
    }
    if let Some(duration) = media.duration {
        parts.push(format!("{duration} min"));
    }
    if let (Some(season), Some(year)) = (&media.season, media.season_year) {
        parts.push(format!("{season} {year}"));
    } else if let Some(year) = media.season_year {
        parts.push(year.to_string());
    }
    if let Some(status) = &media.status {
        parts.push(status.replace('_', " "));
    }
    if let Some(score) = media.average_score {
        parts.push(format!("★ {score}"));
    }
    if let Some(source) = &media.source {
        parts.push(format!("Source: {}", source.replace('_', " ")));
    }
    if let Some(studios) = &media.studios {
        parts.push(studios.join(", "));
    }
    parts.join(" · ")
}

fn plain_description(html: &str) -> String {
    let mut text = String::new();
    let mut tag = String::new();
    let mut inside = false;
    for ch in html.chars() {
        if ch == '<' {
            inside = true;
            tag.clear();
            continue;
        }
        if ch == '>' && inside {
            inside = false;
            let tag = tag.trim().to_ascii_lowercase();
            if tag == "/p"
                || tag == "/div"
                || tag.strip_prefix("br").is_some_and(|rest| {
                    rest.chars().all(|ch| ch.is_ascii_whitespace() || ch == '/')
                })
            {
                text.push('\n');
            }
            continue;
        }
        if inside {
            tag.push(ch);
        } else {
            text.push(ch);
        }
    }
    let decoded = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&nbsp;", " ");
    let mut output = String::with_capacity(decoded.len());
    let mut breaks = 0;
    for ch in decoded.chars() {
        if ch == '\n' {
            breaks += 1;
            if breaks <= 2 {
                output.push(ch);
            }
        } else {
            breaks = 0;
            output.push(ch);
        }
    }
    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{Route, Workspace, airing_label, plain_description, score_label};
    use gpui::{TestAppContext, px, size};
    use kurisu_core::models::{ListEntry, Media, MediaDetail, User};
    use kurisu_core::{Core, Platform, UpdateChannel};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

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
            "kurisu-native-detail-heading-{}-{nonce}",
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

    #[gpui::test]
    fn long_detail_titles_wrap_inside_normal_and_compact_content(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Anime(9);
            workspace.detail.start(9);
            workspace.detail.detail = Some(MediaDetail {
                media: Media {
                    id: 9,
                    ..Default::default()
                },
                relations: vec![],
                characters: vec![],
                staff: vec![],
                cached_at: None,
                warning: None,
                unavailable_sections: vec![],
            });
            cx.notify();
        });
        let titles = [
            "Echo and Garden: A Very Long Journey Beyond the Familiar Horizon, Volume 135",
            "星の彼方で始まる物語と夜明けの約束を何度でも思い出すための長い旅路 第百三十五話 星の彼方で始まる物語と夜明けの約束を何度でも思い出すための長い旅路",
        ];
        for width in [1100., 800.] {
            cx.simulate_resize(size(px(width), px(if width <= 800. { 600. } else { 750. })));
            for title in titles {
                workspace.update_in(cx, |workspace, _, cx| {
                    workspace
                        .detail
                        .detail
                        .as_mut()
                        .unwrap()
                        .media
                        .title_english = Some(title.to_string());
                    cx.notify();
                });
                cx.run_until_parked();
                let bounds = cx.debug_bounds("detail-title").expect("rendered title");
                assert!(
                    bounds.size.height >= px(64.),
                    "{title} at {width}px: {bounds:?}"
                );
                assert!(
                    bounds.origin.x + bounds.size.width <= px(width),
                    "{title} at {width}px"
                );
            }
        }
    }

    #[gpui::test]
    fn detail_entry_retry_cannot_replace_canonical_deletion(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, _| {
            workspace.route = Route::Anime(9);
            let request = workspace.detail.start(9);
            workspace.list.replace(
                vec![ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    ..Default::default()
                }],
                0,
            );
            let revision = workspace.list.revision();
            let generation = workspace.generation;
            assert!(workspace.detail_entry_retry_current(9, request, generation, revision));

            workspace.list.apply_scoped(&[9], vec![None], 0);
            workspace.reconcile_detail_entry();
            assert!(workspace.detail.entry.is_none());
            assert!(!workspace.detail_entry_retry_current(9, request, generation, revision));
            assert!(!workspace.detail_entry_retry_current(
                9,
                request + 1,
                generation,
                workspace.list.revision()
            ));
            assert!(!workspace.detail_entry_retry_current(
                9,
                request,
                generation + 1,
                workspace.list.revision()
            ));
            workspace.route = Route::MyList;
            assert!(!workspace.detail_entry_retry_current(
                9,
                request,
                generation,
                workspace.list.revision()
            ));
        });
    }

    #[test]
    fn descriptions_are_plain_text_with_breaks() {
        assert_eq!(plain_description("<p>A &amp; B</p><br>C"), "A & B\n\nC");
    }

    #[test]
    fn spaced_breaks_apostrophes_and_extra_blank_lines_match_production() {
        assert_eq!(
            plain_description("  A<br />B&#039;s<br>\n<br/><br /> C  "),
            "A\nB's\n\n C"
        );
    }

    #[test]
    fn detail_score_labels_follow_account_format() {
        assert_eq!(score_label(Some(2.), Some("POINT_3")), Some("😐".into()));
        assert_eq!(score_label(Some(4.), Some("POINT_5")), Some("★★★★".into()));
        assert_eq!(
            score_label(Some(8.5), Some("POINT_10_DECIMAL")),
            Some("★ 8.5".into())
        );
        assert_eq!(score_label(Some(0.), None), None);
    }

    #[test]
    fn detail_airing_label_expires_and_changes_units() {
        let mut media = Media {
            next_airing_episode: Some(3),
            ..Media::default()
        };
        media.next_airing_at = Some(1_000);
        assert_eq!(airing_label(&media, 1_000), None);
        assert_eq!(airing_label(&media, 971), Some("Ep 3 airs in 1m".into()));
        media.next_airing_at = Some(4_600);
        assert_eq!(airing_label(&media, 1_000), Some("Ep 3 airs in 1h".into()));
        media.next_airing_at = Some(87_400);
        assert_eq!(airing_label(&media, 1_000), Some("Ep 3 airs in 1d".into()));
    }
}
