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

pub(super) const DETAIL_PERSON_LIMIT: usize = 40;

impl Workspace {
    fn open_detail_external_with(
        &mut self,
        open: impl std::future::Future<Output = Result<(), String>> + Send + 'static,
        window: &gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = super::joined(core.spawn(open).await);
            let _ = this.update_in(cx, |this, _, cx| {
                if !this.stopping
                    && !this.close_ready
                    && this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                    && let Err(error) = result
                {
                    this.global_error = error;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn detail_entry_current(
        &self,
        id: i64,
        request: u64,
        generation: u64,
        entry_revision: u64,
    ) -> bool {
        !self.stopping
            && generation == self.generation
            && self.route == Route::Anime(id)
            && self.detail.current(id, request)
            && self.detail.entry_revision == entry_revision
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
        let reads_fenced = self.stopping || self.writes_fenced();
        let edits_fenced = self.edits_fenced() || self.modal_open();
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
                    .child(
                        button("retry-detail", "Retry details", reads_fenced).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.load_detail(id, window, cx)
                            }),
                        ),
                    );
            }
            return root
                .child(page.children(self.render_detail_progress_review(id, cx)))
                .into_any();
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
            button("open-anilist", "Open on AniList ↗", id <= 0 || edits_fenced)
                .bg(rgb(PANEL))
                .border_color(rgb(PANEL))
                .text_color(rgb(ACCENT))
                .px(px(0.))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if id <= 0 || this.edits_fenced() || this.modal_open() {
                        return;
                    }
                    let url = format!("https://anilist.co/anime/{id}");
                    this.open_detail_external_with(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                crate::platform::NativePlatform.open_url(&url)
                            })
                            .await
                            .map_err(|error| error.to_string())?
                        },
                        window,
                        cx,
                    );
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
                        button(
                            "retry-partial-detail",
                            "Retry details",
                            reads_fenced || self.detail.loading,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.detail.loading {
                                    return;
                                }
                                this.load_detail(id, window, cx)
                            },
                        )),
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
                        button("retry-entry", "Retry list entry", reads_fenced).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.reload_detail_entry(id, window, cx)
                            }),
                        ),
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
        let pending_edit = self.progress.edit(self.generation, id);
        if let Some(entry) = entry {
            let progress = pending_edit.map_or(entry.progress, |edit| edit.pending);
            let review = pending_edit.is_some_and(|edit| edit.requires_review);
            let stepper_locked = edits_fenced || self.progress.busy(self.generation, id) || review;
            let edit_locked = edits_fenced || self.progress.deleting(self.generation, id) || review;
            let edit_label = if self.pending_editor_error(id).is_some() {
                "Retry edit"
            } else {
                "Edit entry"
            };
            let total = media.episodes.filter(|total| *total > 0);
            list_card =
                list_card
                    .child(
                        div()
                            .text_color(rgb(ACCENT))
                            .child(format!("✓ {}", status_label(&entry.status))),
                    )
                    .child(
                        button("detail-less", "−", stepper_locked || progress <= 0)
                            .aria_label("One less episode")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.step_progress(id, -1, window, cx)
                            })),
                    )
                    .child(div().child(if let Some(total) = total {
                        format!("{progress}/{total}")
                    } else {
                        progress.to_string()
                    }))
                    .child(
                        button(
                            "detail-more",
                            "+",
                            stepper_locked || progress >= total.unwrap_or(9999),
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
            if let Some(episode) = entry.progress.checked_add(1)
                && let Some(file) = self.library_files.episode(id, episode)
            {
                let path = file.path.clone();
                list_card = list_card.child(
                    button(
                        "play-next-local",
                        format!("Play Ep {episode}"),
                        edits_fenced,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.edits_fenced() || this.modal_open() {
                            return;
                        }
                        let path = std::path::PathBuf::from(&path);
                        this.open_detail_external_with(
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    crate::platform::NativePlatform.open_path(&path)
                                })
                                .await
                                .map_err(|error| error.to_string())?
                            },
                            window,
                            cx,
                        );
                    })),
                );
            }
            list_card = list_card.child(button("detail-edit", edit_label, edit_locked).on_click(
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
                        self.add_media_blocked(id, None),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_media(id, status, None, window, cx)
                    })),
                );
            }
        } else {
            list_card = list_card.child(div().text_color(rgb(DIM)).child(
                if self.detail.entry_error.is_empty() {
                    "Loading list entry…"
                } else {
                    "List entry unavailable"
                },
            ));
        }
        page = page
            .child(list_card)
            .children(self.render_detail_progress_review(id, cx));
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
                        .capture_key_down(cx.listener(
                            move |this, event: &gpui::KeyDownEvent, window, cx| {
                                let stroke = &event.keystroke;
                                if !stroke.modifiers.modified()
                                    && matches!(stroke.key.as_str(), "enter" | "space" | " ")
                                {
                                    cx.stop_propagation();
                                    if !event.is_held {
                                        this.navigate(Route::Anime(related_id), window, cx);
                                    }
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
            for (index, character) in detail
                .characters
                .iter()
                .enumerate()
                .take(DETAIL_PERSON_LIMIT)
            {
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
            for (index, person) in detail.staff.iter().enumerate().take(DETAIL_PERSON_LIMIT) {
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
                            reads_fenced || self.detail.recommendations_loading,
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
                            .capture_key_down(cx.listener(
                                move |this, event: &gpui::KeyDownEvent, window, cx| {
                                    let stroke = &event.keystroke;
                                    if !stroke.modifiers.modified()
                                        && matches!(stroke.key.as_str(), "enter" | "space" | " ")
                                    {
                                        cx.stop_propagation();
                                        if !event.is_held {
                                            this.navigate(Route::Anime(rec_id), window, cx);
                                        }
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
                            self.add_media_blocked(rec_id, None),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.add_media(rec_id, "PLANNING", None, window, cx)
                            },
                        )),
                    ),
            );
        }
        root.child(page.child(recommendations)).into_any()
    }

    fn render_detail_progress_review(
        &self,
        id: i64,
        cx: &mut gpui::Context<Self>,
    ) -> Option<AnyElement> {
        let edit = self.progress.edit(self.generation, id)?;
        if edit.error.is_none() && !edit.requires_review {
            return None;
        }
        let disabled = self.edits_fenced()
            || self.modal_open()
            || self.progress.busy(self.generation, id)
            || edit.requires_review && !self.list.loaded;
        let mut banner =
            div()
                .border_1()
                .border_color(rgb(ERROR))
                .bg(rgb(PANEL))
                .p(px(10.))
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .text_color(rgb(ERROR))
                .child(edit.error.clone().unwrap_or_else(|| {
                    "Progress changed on your list. Review before saving.".into()
                }));
        if let Some(review) = self.progress.review(self.generation, id) {
            if review.progress.is_some() {
                let review = review.clone();
                banner = banner.child(button("detail-use-edit", "Use my edit", disabled).on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.resolve_progress_review(&review, true, window, cx)
                    }),
                ));
            }
            let label = if review.progress.is_some() {
                "Keep latest"
            } else {
                "Discard edit"
            };
            banner = banner.child(button("detail-keep-latest", label, disabled).on_click(
                cx.listener(move |this, _, window, cx| {
                    this.resolve_progress_review(&review, false, window, cx)
                }),
            ));
        } else {
            banner = banner.child(
                button(
                    "detail-retry-progress",
                    "Retry",
                    disabled || !self.can_retry_progress_errors(),
                )
                .debug_selector(|| "detail-retry-progress".into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    if !this.progress.busy(this.generation, id) {
                        this.retry_progress_errors(window, cx);
                    }
                })),
            );
        }
        Some(banner.into_any())
    }

    fn reload_detail_entry(
        &mut self,
        id: i64,
        window: &gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.stopping
            || self.writes_fenced()
            || self.route != Route::Anime(id)
            || self.detail.media_id != Some(id)
            || self.detail.entry_error.is_empty()
        {
            return;
        }
        self.detail.entry_error.clear();
        cx.notify();
        let request = self.detail.request();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.detail.entry_revision += 1;
        let entry_revision = self.detail.entry_revision;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_entry(id).await });
        self.replace_reads("detail-entry", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = super::joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if this.profile.as_ref().map(|user| user.id) != account
                    || !this.detail_entry_current(id, request, generation, entry_revision)
                {
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
        if self.stopping
            || self.writes_fenced()
            || self.detail.recommendations_loading
            || self.route != Route::Anime(id)
            || self.detail.media_id != Some(id)
        {
            return;
        }
        let request = self.detail.request();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.detail.recommendations_loading = true;
        self.detail.recommendation_error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_recommendations(id).await });
        self.replace_reads("detail-recommendations", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = super::joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || generation != this.generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.route != Route::Anime(id)
                    || !this.detail.current(id, request)
                {
                    return;
                }
                this.detail.finish_recommendations(id, request, result);
                this.refresh_detail_covers(window.scale_factor());
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
    if let Some(episodes) = media.known_episodes() {
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
    use super::super::{BarrierAction, CloseIntent};
    use super::{Route, Workspace, airing_label, plain_description, score_label};
    use gpui::accesskit::{ActionRequest, NodeId, TreeId};
    use gpui::{
        AccessibleAction, KeyDownEvent, KeyUpEvent, Keystroke, TestAppContext, VisualTestContext,
        px, size,
    };
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

    fn accessible_control(cx: &mut VisualTestContext, label: &str) -> NodeId {
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        cx.take_accessibility_tree_update()
            .unwrap()
            .nodes
            .into_iter()
            .find_map(|(id, node)| (node.label() == Some(label)).then_some(id))
            .unwrap_or_else(|| panic!("missing {label}"))
    }

    fn accessible_action(cx: &VisualTestContext, target_node: NodeId, action: AccessibleAction) {
        cx.simulate_accessibility_action(ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node,
            data: None,
        });
    }

    fn press_activation_key(cx: &mut VisualTestContext, key: &str, is_held: bool) {
        let os = cx.update(|_, app| app.platform_info().os);
        let keystroke = Keystroke::parse_with_os(key, os).unwrap();
        cx.simulate_event(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held,
            prefer_character_input: false,
        });
        cx.simulate_event(KeyUpEvent { keystroke });
    }

    #[gpui::test]
    fn recommendation_opens_with_plain_enter_or_accessibility_click(tc: &mut TestAppContext) {
        for use_keyboard in [true, false] {
            let (core, _runtime, _dir) = fixture();
            let (workspace, cx) = tc.add_window_view(|window, cx| {
                Workspace::new_with_session_restore(core, window, cx, false)
            });
            cx.simulate_resize(size(px(1100.), px(1200.)));
            workspace.update_in(cx, |w, _, cx| {
                w.profile = Some(User {
                    id: 47,
                    ..Default::default()
                });
                w.route = Route::Anime(9);
                let request = w.detail.start(9);
                w.detail.finish_detail(
                    9,
                    request,
                    Ok(MediaDetail {
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
                    }),
                );
                w.detail.finish_recommendations(
                    9,
                    request,
                    Ok(vec![Media {
                        id: 10,
                        title_english: Some("Recommendation fixture".into()),
                        ..Default::default()
                    }]),
                );
                cx.notify();
            });
            let id = accessible_control(cx, "Open Recommendation fixture");
            accessible_action(cx, id, AccessibleAction::Focus);
            for key in ["ctrl-enter", "shift-enter", "alt-space"] {
                press_activation_key(cx, key, false);
            }
            press_activation_key(cx, "enter", true);
            assert_eq!(
                workspace.read_with(cx, |w, _| w.route.clone()),
                Route::Anime(9)
            );
            if use_keyboard {
                press_activation_key(cx, "enter", false);
            } else {
                accessible_action(cx, id, AccessibleAction::Click);
            }
            assert_eq!(
                workspace.read_with(cx, |w, _| w.route.clone()),
                Route::Anime(10)
            );
        }
    }

    #[gpui::test]
    fn search_completed_submits_with_plain_enter_or_accessibility_click(tc: &mut TestAppContext) {
        for use_keyboard in [true, false] {
            let (core, _runtime, _dir) = fixture();
            let (workspace, cx) = tc.add_window_view(|window, cx| {
                Workspace::new_with_session_restore(core, window, cx, false)
            });
            cx.simulate_resize(size(px(1100.), px(1200.)));
            workspace.update_in(cx, |w, window, cx| {
                w.profile = Some(User {
                    id: 47,
                    ..Default::default()
                });
                w.route = Route::Search;
                w.search.results.push(Media {
                    id: 10,
                    title_english: Some("Search fixture".into()),
                    ..Default::default()
                });
                w.enter_search(window, cx);
                cx.notify();
            });
            let id = accessible_control(cx, "Completed");
            accessible_action(cx, id, AccessibleAction::Focus);
            for key in ["ctrl-enter", "shift-enter", "alt-space"] {
                press_activation_key(cx, key, false);
            }
            press_activation_key(cx, "enter", true);
            assert_eq!(workspace.read_with(cx, |w, _| w.active_writes), 0);
            if use_keyboard {
                press_activation_key(cx, "enter", false);
            } else {
                accessible_action(cx, id, AccessibleAction::Click);
            }
            assert_eq!(workspace.read_with(cx, |w, _| w.active_writes), 1);
        }
    }

    #[cfg(feature = "fixture-runtime")]
    #[gpui::test]
    fn large_only_detail_thumbnails_load_on_initial_read_retry_and_scale(tc: &mut TestAppContext) {
        use super::super::fixture_server::{child_gate, serve};
        use super::super::workspace_tests::prompt_restore_tests::signed_in_fixture;
        use serde_json::json;
        use std::io::{Cursor, Write};
        use std::time::Duration;

        const CHILD: &str = "KURISU_DETAIL_THUMBNAILS_CHILD";
        if child_gate(CHILD).is_none() {
            let mut png = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                336,
                480,
                image::Rgba([1, 2, 3, 255]),
            ))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
            let png = png.into_inner();
            let mut failed = false;
            let thread = std::thread::current();
            serve(
                thread.name().unwrap(),
                CHILD,
                "KURISU_FIXTURE_GRAPHQL",
                |mut http, _| {
                    if http.target == "/cover.png" {
                        write!(
                            http.stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            png.len()
                        )
                        .unwrap();
                        http.stream.write_all(&png).unwrap();
                        return;
                    }
                    let request = http.json();
                    let query = request["query"].as_str().unwrap();
                    let url = format!("http://{}/cover.png", http.stream.local_addr().unwrap());
                    let media = |id| {
                        json!({
                            "id": id, "idMal": null, "title": {"romaji": "Fixture", "english": null, "native": null},
                            "coverImage": {"medium": null, "large": url}, "episodes": 12, "format": null,
                            "status": null, "averageScore": null, "season": null, "seasonYear": null,
                            "description": null, "nextAiringEpisode": null
                        })
                    };
                    let data = if query.contains("recommendations(") {
                        let id = request["variables"]["mediaId"].as_i64().unwrap();
                        let recommendations = if id == 19 && !std::mem::replace(&mut failed, true) {
                            serde_json::Value::Null
                        } else {
                            json!([{"mediaRecommendation": media(id + 1)}])
                        };
                        json!({"Page": {"recommendations": recommendations}})
                    } else {
                        assert!(query.contains("relations"), "unexpected fixture query");
                        let id = request["variables"]["id"].as_i64().unwrap();
                        let mut detail = media(id);
                        detail["bannerImage"] = serde_json::Value::Null;
                        detail["genres"] = serde_json::Value::Null;
                        detail["duration"] = serde_json::Value::Null;
                        detail["source"] = serde_json::Value::Null;
                        detail["studios"] = serde_json::Value::Null;
                        let mut relation = media(id + 2);
                        relation["type"] = json!("ANIME");
                        detail["relations"] =
                            json!({"edges": [{"relationType": "SEQUEL", "node": relation}]});
                        detail["characters"] = json!({"edges": []});
                        detail["staff"] = json!({"edges": []});
                        json!({"Media": detail})
                    };
                    http.reply_json(200, json!({"data": data})).unwrap();
                },
            );
            return;
        }
        let (core, runtime, user) = signed_in_fixture(None);
        let (workspace, cx) = tc.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1200.)));
        let await_jobs = |jobs: Vec<tokio::task::AbortHandle>| {
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(4), async {
                    while jobs.iter().any(|job| !job.is_finished()) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            });
        };
        for (id, retry) in [(9, false), (19, true)] {
            cx.simulate_scale_factor_change(1.);
            let jobs = workspace.update_in(cx, |w, window, cx| {
                w.profile = Some(user.clone());
                w.route = Route::Anime(id);
                w.retain_cover_route();
                w.load_detail(id, window, cx);
                w.read_aborts["detail"].clone()
            });
            await_jobs(jobs);
            cx.run_until_parked();
            workspace.read_with(cx, |w, _| {
                assert!(
                    w.detail.detail_error.is_empty(),
                    "{}",
                    w.detail.detail_error
                );
                assert_eq!(
                    !w.detail.recommendation_error.is_empty(),
                    retry,
                    "{}",
                    w.detail.recommendation_error
                );
            });
            if retry {
                let control = accessible_control(cx, "Retry recommendations");
                accessible_action(cx, control, AccessibleAction::Click);
                await_jobs(
                    workspace.read_with(cx, |w, _| w.read_aborts["detail-recommendations"].clone()),
                );
                cx.run_until_parked();
            }
            for scale in [1, 2] {
                cx.simulate_scale_factor_change(scale as f32);
                cx.executor().advance_clock(Duration::from_millis(120));
                cx.run_until_parked();
                let keys = [(id + 1, 112, 160), (id + 2, 112, 160)];
                workspace.read_with(cx, |w, _| {
                    for key in keys {
                        assert!(
                            w.cover_tickets.contains_key(&key),
                            "missing {key:?} at {scale}x"
                        );
                    }
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(4);
                while !workspace.read_with(cx, |w, _| {
                    keys.iter().all(|key| w.cover_images.contains_key(key))
                }) {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "thumbnails did not load at {scale}x"
                    );
                    runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
                    cx.executor().advance_clock(Duration::from_millis(120));
                    cx.run_until_parked();
                }
                workspace.read_with(cx, |w, _| {
                    for key in keys {
                        let pixels = w.cover_images[&key].size(0);
                        assert_eq!(
                            (i32::from(pixels.width), i32::from(pixels.height)),
                            (112 * scale, 160 * scale)
                        );
                    }
                });
            }
        }
    }

    #[cfg(feature = "fixture-runtime")]
    #[gpui::test]
    fn failed_recommendation_retry_reloads_detail_cover_at_new_scale(tc: &mut TestAppContext) {
        use std::io::Cursor;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (core, runtime, _dir) = fixture();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}/cover.png", listener.local_addr().unwrap());
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            320,
            460,
            image::Rgba([1, 2, 3, 255]),
        ))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
        let png = png.into_inner();
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            png.len()
        );
        let (old_started, old_request) = tokio::sync::oneshot::channel();
        let (release_old, release) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let mut hold_old = Some((old_started, release));
            for index in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                while socket.read_line(&mut line).await.unwrap() != 0 && line != "\r\n" {
                    line.clear();
                }
                let gate = (index == 1).then(|| hold_old.take().unwrap());
                let header = header.clone();
                let png = png.clone();
                tokio::spawn(async move {
                    if let Some((started, release)) = gate {
                        started.send(()).unwrap();
                        if release.await.is_err() {
                            return;
                        }
                    }
                    socket.get_mut().write_all(header.as_bytes()).await.unwrap();
                    socket.get_mut().write_all(&png).await.unwrap();
                });
            }
        });

        let (workspace, cx) = tc.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1200.)));
        cx.simulate_scale_factor_change(1.);
        let completed = workspace.update_in(cx, |w, window, cx| {
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            let request = w.detail.start(9);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        cover_large: Some(url),
                        ..Default::default()
                    },
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.detail
                .finish_recommendations(9, request, Err("Initial failure".into()));
            w.ensure_cover_scale(window.scale_factor());
            w.requeue_detail_covers();
            cx.notify();
            w.covers.completion_signal()
        });
        let wait_for_cover = || {
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), completed.notified())
                    .await
                    .expect("cover worker must finish");
            });
        };
        let key = (9, 128, 184);
        let cover_size = |w: &Workspace| {
            let size = w.cover_images[&key].size(0);
            (i32::from(size.width), i32::from(size.height))
        };
        wait_for_cover();
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| cover_size(w)), (128, 184));

        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (retry, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Retry recommendations"))
            .expect("recommendation retry button");
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::Click,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *retry,
            data: None,
        });
        workspace.update_in(cx, |w, _, _| {
            assert!(w.detail.recommendations_loading);
            w.cancel_reads("detail-recommendations");
        });
        cx.simulate_scale_factor_change(2.);
        runtime.block_on(async { tokio::task::yield_now().await });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.cover_scale, 2.);
            assert!(!w.detail.recommendations_loading);
            assert!(!w.detail.recommendation_error.is_empty());
            assert!(
                w.cover_tickets.contains_key(&key),
                "2x detail cover was never queued"
            );
        });
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), old_request)
                .await
                .expect("2x cover request must reach the server")
                .unwrap();
        });

        let next_scale = 241. / 120.;
        cx.simulate_scale_factor_change(next_scale);
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| w.cover_scale), next_scale);
        wait_for_cover();
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| cover_size(w)), (258, 371));

        release_old.send(()).unwrap();
        wait_for_cover();
        cx.executor().advance_clock(Duration::from_millis(120));
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| cover_size(w)), (258, 371));
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
    fn progress_review_requires_the_displayed_entry_even_at_the_same_progress(
        cx: &mut TestAppContext,
    ) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |w, window, cx| {
            let entry = |id| ListEntry {
                id: Some(id),
                media_id: 9,
                progress: 2,
                status: "CURRENT".into(),
                ..Default::default()
            };
            let edit = w.progress.entry(w.generation, &entry(10));
            assert!(edit.step(1, std::time::Instant::now()));
            w.list.replace(vec![entry(11)], 0);
            w.progress.reconcile_entries(w.generation, &w.list);
            let displayed = w.progress.review(w.generation, 9).unwrap();
            w.list.replace(vec![entry(12)], 0);
            w.progress.reconcile_entries(w.generation, &w.list);
            w.resolve_progress_review(&displayed, true, window, cx);
            assert_eq!(w.active_writes, 0);
            let replacement = w.progress.review(w.generation, 9).unwrap();
            assert_eq!(displayed.progress, replacement.progress);
            assert_ne!(displayed.identity, replacement.identity);
            w.resolve_progress_review(&replacement, false, window, cx);
            let edit = w.progress.edit(w.generation, 9).unwrap();
            assert!(!edit.dirty());
            assert_eq!(edit.identity, kurisu_core::EntryIdentity::Remote(12));
        });
    }

    #[gpui::test]
    fn deleted_entry_shows_review_and_locks_add_until_the_save_settles(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, window, cx| {
            w.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            let request = w.detail.start(9);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        episodes: Some(12),
                        ..Default::default()
                    },
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            let entry = ListEntry {
                media_id: 9,
                progress: 2,
                status: "CURRENT".into(),
                ..Default::default()
            };
            let edit = w.progress.entry(w.generation, &entry);
            assert!(edit.step(1, std::time::Instant::now()));
            assert_eq!(edit.begin_save(), Some((3, 2)));
            w.list.replace(vec![], 0);
            w.progress.reconcile_entries(w.generation, &w.list);
            w.reconcile_open_entries(None, cx);
            let review = w.progress.review(w.generation, 9).unwrap();
            w.resolve_progress_review(&review, false, window, cx);
            w.add_media(9, "CURRENT", None, window, cx);
            assert!(!w.editor_adding.contains(&9));
            assert!(w.progress.edit(w.generation, 9).unwrap().in_flight);
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        for label in ["Discard edit", "Watching", "Plan to watch", "Completed"] {
            let node = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing {label}"));
            assert!(node.1.is_disabled());
        }
        workspace.update_in(cx, |w, _, cx| {
            w.progress
                .edit_mut(w.generation, 9)
                .unwrap()
                .finish_save(Err("Save failed".into()));
            cx.notify();
        });
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (id, node) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Discard edit"))
            .expect("missing recovery action");
        assert!(!node.is_disabled());
        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::Click,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *id,
            data: None,
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(w.progress.edit(w.generation, 9).is_none());
            assert!(!w.progress.blocks_add(w.generation, 9));
        });
    }

    #[gpui::test]
    fn detail_entry_retry_cannot_replace_canonical_deletion(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.route = Route::Anime(9);
            let request = workspace.detail.start(9);
            workspace
                .detail
                .finish_entry(9, request, Err("Unavailable".into()));
            workspace.reload_detail_entry(9, window, cx);
            assert!(workspace.detail.entry_error.is_empty());
            let retry = workspace.detail.entry_revision;
            workspace.reload_detail_entry(9, window, cx);
            assert_eq!(workspace.detail.entry_revision, retry);
            workspace.list.replace(
                vec![ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    ..Default::default()
                }],
                0,
            );
            let revision = workspace.detail.entry_revision;
            let generation = workspace.generation;
            assert!(workspace.detail_entry_current(9, request, generation, revision));

            workspace.list.apply_scoped(&[10], vec![None], 0);
            workspace.reconcile_open_entries(Some(&[10]), cx);
            assert!(workspace.detail_entry_current(9, request, generation, revision));
            assert!(!workspace.detail.entry_available);

            workspace.list.apply_scoped(&[9], vec![None], 0);
            workspace.reconcile_open_entries(Some(&[9]), cx);
            assert!(workspace.detail.entry_available && workspace.detail.entry.is_none());
            assert!(!workspace.detail_entry_current(9, request, generation, revision));
            let revision = workspace.detail.entry_revision;
            assert!(workspace.detail_entry_current(9, request, generation, revision));
            assert!(!workspace.detail_entry_current(9, request + 1, generation, revision));
            assert!(!workspace.detail_entry_current(9, request, generation + 1, revision));
            workspace.route = Route::MyList;
            assert!(!workspace.detail_entry_current(9, request, generation, revision));
        });
    }

    #[gpui::test]
    fn detail_retry_controls_and_reads_obey_write_fence(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1000.)));
        cx.simulate_accessibility_activation();

        let assert_buttons_disabled =
            |cx: &mut gpui::VisualTestContext, labels: &[&str], expected: bool| {
                cx.run_until_parked();
                let tree = cx.take_accessibility_tree_update().unwrap();
                for &label in labels {
                    let node = tree
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(label))
                        .unwrap_or_else(|| panic!("missing {label}"));
                    assert_eq!(
                        node.1.is_disabled(),
                        expected,
                        "{label} disabled state mismatch"
                    );
                }
            };

        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            let request = w.detail.start(9);
            w.detail
                .finish_detail(9, request, Err("Detail unavailable".into()));
            cx.notify();
        });
        assert_buttons_disabled(cx, &["Retry details"], false);

        for close_ready in [false, true] {
            workspace.update_in(cx, |w, win, cx| {
                w.barrier = (!close_ready).then_some(BarrierAction::Close(CloseIntent::Window));
                w.close_ready = close_ready;
                assert!(w.writes_fenced());
                let request = w.detail.request();
                w.load_detail(9, win, cx);
                assert_eq!(w.detail.request(), request);
                assert!(!w.detail.loading);
                assert!(!w.read_aborts.contains_key("detail"));
                cx.notify();
            });
            assert_buttons_disabled(cx, &["Retry details"], true);
        }

        workspace.update_in(cx, |w, _, cx| {
            w.barrier = None;
            w.close_ready = false;
            assert!(!w.writes_fenced());
            cx.notify();
        });
        assert_buttons_disabled(cx, &["Retry details"], false);
        workspace.update_in(cx, |w, win, cx| {
            let request = w.detail.request();
            w.load_detail(9, win, cx);
            assert_eq!(w.detail.request(), request + 1);
            assert!(w.detail.loading);
            assert_eq!(w.read_aborts.get("detail").map(Vec::len), Some(3));
            w.cancel_reads("detail");
        });

        workspace.update_in(cx, |w, _, cx| {
            let request = w.detail.start(9);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        title_english: Some("Partial detail".into()),
                        ..Default::default()
                    },
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: Some("Cached copy.".into()),
                    unavailable_sections: vec!["staff".into()],
                }),
            );
            w.detail
                .finish_entry(9, request, Err("Entry unavailable".into()));
            w.detail
                .finish_recommendations(9, request, Err("Recs unavailable".into()));
            cx.notify();
        });
        assert_buttons_disabled(
            cx,
            &["Retry details", "Retry list entry", "Retry recommendations"],
            false,
        );

        for close_ready in [false, true] {
            workspace.update_in(cx, |w, win, cx| {
                w.barrier = (!close_ready).then_some(BarrierAction::Close(CloseIntent::Window));
                w.close_ready = close_ready;
                assert!(w.writes_fenced());
                let request = w.detail.request();
                let entry_revision = w.detail.entry_revision;
                w.load_detail(9, win, cx);
                w.reload_detail_entry(9, win, cx);
                w.reload_detail_recommendations(9, win, cx);
                assert_eq!(w.detail.request(), request);
                assert_eq!(w.detail.entry_revision, entry_revision);
                assert!(!w.detail.loading);
                assert!(!w.detail.recommendations_loading);
                assert_eq!(w.detail.entry_error, "Entry unavailable");
                assert_eq!(w.detail.recommendation_error, "Recs unavailable");
                for group in ["detail", "detail-entry", "detail-recommendations"] {
                    assert!(!w.read_aborts.contains_key(group), "unexpected {group}");
                }
                cx.notify();
            });
            assert_buttons_disabled(
                cx,
                &["Retry details", "Retry list entry", "Retry recommendations"],
                true,
            );
        }

        workspace.update_in(cx, |w, _, cx| {
            w.barrier = None;
            w.close_ready = false;
            cx.notify();
        });
        assert_buttons_disabled(
            cx,
            &["Retry details", "Retry list entry", "Retry recommendations"],
            false,
        );
        workspace.update_in(cx, |w, win, cx| {
            let entry_revision = w.detail.entry_revision;
            w.reload_detail_entry(9, win, cx);
            assert_eq!(w.detail.entry_revision, entry_revision + 1);
            assert!(w.detail.entry_error.is_empty());
            assert!(w.read_aborts.contains_key("detail-entry"));

            w.reload_detail_recommendations(9, win, cx);
            assert!(w.detail.recommendations_loading);
            assert!(w.detail.recommendation_error.is_empty());
            assert!(w.read_aborts.contains_key("detail-recommendations"));

            let request = w.detail.request();
            w.load_detail(9, win, cx);
            assert_eq!(w.detail.request(), request + 1);
            assert!(w.detail.loading);
            assert!(w.read_aborts.contains_key("detail"));
            w.cancel_reads_for_quit();
        });
    }

    #[gpui::test]
    fn detail_omits_next_episode_when_progress_cannot_advance(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            let request = w.detail.start(9);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        title_english: Some("Unknown episode count".into()),
                        ..Default::default()
                    },
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.library_files.replace(
                [i64::MIN, i64::MAX]
                    .into_iter()
                    .map(|episode| kurisu_core::models::LibraryFile {
                        path: format!("/fixture/episode-{episode}.mkv"),
                        media_id: Some(9),
                        episode: Some(episode),
                        ..Default::default()
                    })
                    .collect(),
            );
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        for (progress, play_next) in [(i64::MAX, false), (i64::MAX - 1, true)] {
            workspace.update_in(cx, |w, _, cx| {
                w.detail.reconcile_entry(Some(ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    progress,
                    ..Default::default()
                }));
                cx.notify();
            });
            cx.update(|window, app| window.draw(app).clear(app));
            let tree = cx.take_accessibility_tree_update().unwrap();
            if progress == i64::MAX {
                assert!(tree.nodes.iter().any(|(_, node)| node.label()
                    == Some("One more episode")
                    && node.is_disabled()));
                workspace.update_in(cx, |w, window, cx| {
                    w.step_progress(9, 1, window, cx);
                    assert!(
                        w.progress
                            .edit(w.generation, 9)
                            .is_none_or(|edit| !edit.dirty())
                    );
                });
            }
            let labels: Vec<_> = tree
                .nodes
                .iter()
                .filter_map(|(_, node)| node.label())
                .filter(|label| label.starts_with("Play Ep "))
                .collect();
            if play_next {
                assert_eq!(labels, [format!("Play Ep {}", i64::MAX)]);
            } else {
                assert!(labels.is_empty());
            }
        }
    }

    #[gpui::test]
    fn detail_action_controls_obey_edit_fence(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1000.)));
        cx.simulate_accessibility_activation();

        let labels = [
            "Open on AniList ↗",
            "One less episode",
            "One more episode",
            "Play Ep 4",
            "Edit entry",
        ];
        let assert_controls = |cx: &mut gpui::VisualTestContext, expected: bool, mode: &str| {
            cx.run_until_parked();
            let tree = cx.take_accessibility_tree_update().unwrap();
            for label in labels {
                let (_, node) = tree
                    .nodes
                    .iter()
                    .find(|(_, node)| node.label() == Some(label))
                    .unwrap_or_else(|| panic!("missing {label} in {mode}"));
                assert_eq!(
                    node.is_disabled(),
                    expected,
                    "{label} disabled state mismatch in {mode}"
                );
            }
        };

        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            w.back_stack.push(Route::MyList);
            let request = w.detail.start(9);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: Media {
                        id: 9,
                        title_english: Some("Detail actions".into()),
                        episodes: Some(12),
                        ..Default::default()
                    },
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.detail.finish_entry(
                9,
                request,
                Ok(Some(ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    progress: 3,
                    ..Default::default()
                })),
            );
            w.library_files
                .replace(vec![kurisu_core::models::LibraryFile {
                    path: "/controlled/ep4.mkv".into(),
                    media_id: Some(9),
                    episode: Some(4),
                    ..Default::default()
                }]);
            cx.notify();
        });
        assert_controls(cx, false, "unfenced");

        for mode in [
            "barrier",
            "close_ready",
            "auth_busy",
            "update_busy",
            "modal",
            "stopping",
        ] {
            workspace.update_in(cx, |w, win, cx| {
                w.barrier =
                    (mode == "barrier").then_some(BarrierAction::Close(CloseIntent::Window));
                w.close_ready = mode == "close_ready";
                w.auth_busy = mode == "auth_busy";
                w.update_busy = mode == "update_busy";
                w.logout_confirming = mode == "modal";
                w.stopping = mode == "stopping";
                if mode == "stopping" {
                    w.navigate(Route::MyList, win, cx);
                    w.back(win, cx);
                    assert_eq!(w.route, Route::Anime(9));
                }
                cx.notify();
            });
            assert_controls(cx, true, mode);
        }

        workspace.update_in(cx, |w, _, cx| {
            w.barrier = None;
            w.close_ready = false;
            w.auth_busy = false;
            w.update_busy = false;
            w.logout_confirming = false;
            w.stopping = false;
            cx.notify();
        });
        assert_controls(cx, false, "cleared");
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

    #[gpui::test]
    fn disabled_detail_retry_progress_blocks_retry_while_entry_is_busy(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::workspace_tests::fixture;
        use kurisu_core::models::{ListEntry, Media, MediaDetail, User};
        use std::time::Instant;

        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Anime(9);
            let request = w.detail.start(9);
            let entry9 = ListEntry {
                id: Some(109),
                media_id: 9,
                status: "CURRENT".into(),
                progress: 3,
                media: Some(Media {
                    id: 9,
                    title_english: Some("Show Nine".into()),
                    episodes: Some(12),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let entry10 = ListEntry {
                id: Some(110),
                media_id: 10,
                status: "CURRENT".into(),
                progress: 2,
                media: Some(Media {
                    id: 10,
                    title_english: Some("Show Ten".into()),
                    episodes: Some(12),
                    ..Default::default()
                }),
                ..Default::default()
            };
            w.list.replace(vec![entry9.clone(), entry10.clone()], 0);
            w.detail.finish_detail(
                9,
                request,
                Ok(MediaDetail {
                    media: entry9.media.clone().unwrap(),
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                }),
            );
            w.detail.finish_entry(9, request, Ok(Some(entry9.clone())));
            let edit9 = w.progress.entry(w.generation, &entry9);
            assert!(edit9.step(1, Instant::now()));
            assert!(edit9.begin_save().is_some());
            edit9.finish_save(Err("offline".into()));
            let edit10 = w.progress.entry(w.generation, &entry10);
            assert!(edit10.step(1, Instant::now()));
            assert!(edit10.begin_save().is_some());
            edit10.finish_save(Err("offline".into()));
            cx.notify();
        });
        cx.run_until_parked();
        let retry_bounds = cx
            .debug_bounds("detail-retry-progress")
            .expect("detail retry button bounds");
        workspace.update_in(cx, |w, _, _| {
            assert!(w.progress.begin_delete(w.generation, 9));
            assert!(w.progress.busy(w.generation, 9));
        });
        cx.simulate_click(retry_bounds.center(), gpui::Modifiers::default());
        workspace.read_with(cx, |w, _| {
            assert_eq!(
                w.active_writes, 0,
                "disabled detail retry button must not retry progress while entry is busy"
            );
            assert!(!w.progress.busy(w.generation, 10));
        });
    }

    #[test]
    fn media_metadata_omits_non_positive_episode_totals() {
        let mut media = Media {
            id: 1,
            format: Some("TV".into()),
            episodes: Some(0),
            ..Default::default()
        };
        assert_eq!(super::media_metadata(&media), "TV");
        media.episodes = Some(-3);
        assert_eq!(super::media_metadata(&media), "TV");
        media.episodes = Some(12);
        assert_eq!(super::media_metadata(&media), "TV · 12 eps");
    }

    #[gpui::test]
    fn detail_external_launch_failure_is_ignored_after_expiry_or_close(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::workspace_tests::{fixture, settle};
        use kurisu_core::models::User;

        let (core, runtime, _dir) = fixture();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        view.update_in(cx, |w, window, cx| {
            w.profile = Some(User {
                id: 7,
                ..Default::default()
            });
            let generation = w.generation;
            w.open_detail_external_with(async move { rx.await.unwrap() }, window, cx);
            w.set_profile_at(None, generation, window, cx);
            assert_eq!(w.generation, generation);
        });
        tx.send(Err("open failed".into())).unwrap();
        settle(cx, &runtime);
        view.read_with(cx, |w, _| assert!(w.global_error.is_empty()));

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        view.update_in(cx, |w, window, cx| {
            w.profile = Some(User {
                id: 7,
                ..Default::default()
            });
            w.open_detail_external_with(async move { rx.await.unwrap() }, window, cx);
            w.close_ready = true;
        });
        tx.send(Err("open failed".into())).unwrap();
        settle(cx, &runtime);
        view.read_with(cx, |w, _| assert!(w.global_error.is_empty()));

        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        view.update_in(cx, |w, window, cx| {
            w.close_ready = false;
            w.open_detail_external_with(async move { rx.await.unwrap() }, window, cx);
        });
        tx.send(Err("open failed".into())).unwrap();
        settle(cx, &runtime);
        view.read_with(cx, |w, _| assert_eq!(w.global_error, "open failed"));
    }
}
