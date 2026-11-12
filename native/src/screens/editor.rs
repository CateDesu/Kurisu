use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, Window, deferred, div, img, prelude::*, px,
    rgb,
};
use kurisu_core::Platform;
use kurisu_core::models::{EntryDetails, FuzzyDate, Media};

use super::covers::CoverOwner;
use super::state::{EditorState, STATUSES, status_label};
use super::{EditorInputs, ModalLayer, Workspace, joined, now_seconds};
use crate::components::input::{TextInput, TextInputEvent};
use crate::components::{button, dialog, eyebrow, select_with_disabled};
use crate::contracts::Route;
use crate::theme::{ACCENT, DIM, EDGE, ERROR, INK, PANEL, PANEL_2, serif_font};

fn input(
    value: impl Into<String>,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TextInput> {
    let value = value.into();
    cx.new(|cx| TextInput::new(value, placeholder, window, cx))
}

fn optional_number(value: Option<i64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

fn parse_part(value: &str, min: i64, max: i64) -> Result<Option<i64>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let number = value
        .trim()
        .parse::<i64>()
        .map_err(|_| "Enter a valid date number.".to_string())?;
    if !(min..=max).contains(&number) {
        return Err(format!("Date values must be between {min} and {max}."));
    }
    Ok(Some(number))
}

impl Workspace {
    pub(super) fn open_pending_editor(
        &mut self,
        completed_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, route)) = self.pending_editor.clone() else {
            return;
        };
        if id != completed_id {
            return;
        }
        if route != self.route || self.list.entry(id).is_none() {
            self.pending_editor = None;
            return;
        }
        if self.progress.edit(self.generation, id).is_some_and(|edit| {
            edit.dirty()
                || edit.in_flight
                || edit.deleting
                || edit.requires_review
                || edit.error.is_some()
        }) {
            return;
        }
        self.pending_editor = None;
        self.open_editor(id, window, cx);
    }

    pub(super) fn set_editor_inputs_disabled(&self, disabled: bool, cx: &mut Context<Self>) {
        let Some(inputs) = self.editor_inputs.as_ref() else {
            return;
        };
        for input in [
            &inputs.progress,
            &inputs.score,
            &inputs.repeat,
            &inputs.notes,
            &inputs.started_year,
            &inputs.started_month,
            &inputs.started_day,
            &inputs.completed_year,
            &inputs.completed_month,
            &inputs.completed_day,
        ] {
            input.update(cx, |input, cx| input.set_disabled(disabled, cx));
        }
    }

    pub(super) fn open_editor(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_some_and(|editor| editor.busy) {
            return;
        }
        if self
            .pending_editor
            .as_ref()
            .is_some_and(|(waiting, route)| *waiting == id && *route == self.route)
        {
            return;
        }
        self.pending_editor = None;
        if let Some(edit) = self.progress.edit(self.generation, id)
            && (edit.dirty() || edit.in_flight)
        {
            let in_flight = edit.in_flight;
            self.pending_editor = Some((id, self.route.clone()));
            if !in_flight {
                self.save_progress(id, window, cx);
            }
            cx.notify();
            return;
        }
        self.cancel_reads("editor-recs");
        self.cancel_reads("editor-details");
        let entry = self
            .list
            .entry(id)
            .or_else(|| {
                self.detail
                    .entry
                    .as_ref()
                    .filter(|entry| entry.media_id == id)
            })
            .cloned();
        let Some(mut entry) = entry else {
            return;
        };
        if entry.media.is_none() {
            entry.media = self
                .detail
                .detail
                .as_ref()
                .filter(|detail| detail.media.id == id)
                .map(|detail| detail.media.clone());
        }
        let cover_url = entry
            .media
            .as_ref()
            .and_then(|media| media.cover_medium.as_deref())
            .map(str::to_owned);
        self.editor = Some(EditorState::new(entry.clone()));
        self.editor.as_mut().unwrap().displayed_format =
            self.profile.as_ref().and_then(|u| u.score_format.clone());
        if let Some(url) = cover_url {
            self.request_cover_for(CoverOwner::Editor, (id, 48, 64), &url);
        }
        self.editor_status_open = false;
        self.editor_recs.clear();
        self.editor_recs_error.clear();
        self.editor_adding.clear();
        self.added_recommendations.clear();
        self.editor_details_open = false;
        self.editor_details_loading = false;
        let inputs = EditorInputs {
            progress: input(entry.progress.to_string(), "Episodes", window, cx),
            score: input(optional_score(entry.score), "Score", window, cx),
            repeat: input(entry.repeat.to_string(), "Rewatches", window, cx),
            notes: input("", "Notes", window, cx),
            started_year: input("", "Year", window, cx),
            started_month: input("", "Month", window, cx),
            started_day: input("", "Day", window, cx),
            completed_year: input("", "Year", window, cx),
            completed_month: input("", "Month", window, cx),
            completed_day: input("", "Day", window, cx),
        };
        for (field, label) in [
            (&inputs.started_year, "Started year"),
            (&inputs.started_month, "Started month"),
            (&inputs.started_day, "Started day"),
            (&inputs.completed_year, "Completed year"),
            (&inputs.completed_month, "Completed month"),
            (&inputs.completed_day, "Completed day"),
        ] {
            field.update(cx, |input, cx| input.set_accessible_label(label, cx));
        }
        inputs
            .notes
            .update(cx, |input, cx| input.set_multiline(true, cx));
        self.editor_subscriptions.clear();
        self.editor_subscriptions.push(cx.subscribe_in(
            &inputs.progress,
            window,
            |this, _, event, window, cx| match event {
                TextInputEvent::Changed(value) => {
                    if let (Some(editor), Ok(number)) =
                        (this.editor.as_mut(), value.trim().parse::<i64>())
                    {
                        editor.set_progress(number);
                        cx.notify();
                    }
                }
                TextInputEvent::Submitted => this.save_editor(window, cx),
            },
        ));
        for field in [
            &inputs.score,
            &inputs.repeat,
            &inputs.started_year,
            &inputs.started_month,
            &inputs.started_day,
            &inputs.completed_year,
            &inputs.completed_month,
            &inputs.completed_day,
        ] {
            self.editor_subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, event, window, cx| {
                    if matches!(event, TextInputEvent::Submitted) {
                        this.save_editor(window, cx);
                    }
                },
            ));
        }
        self.editor_inputs = Some(inputs);
        let editor_input = self
            .editor_inputs
            .as_ref()
            .map(|inputs| inputs.progress.entity_id());
        let generation = self.generation;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_recommendations(id).await });
        self.replace_reads("editor-recs", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation
                    || this
                        .editor_inputs
                        .as_ref()
                        .map(|inputs| inputs.progress.entity_id())
                        != editor_input
                    || this
                        .editor
                        .as_ref()
                        .is_none_or(|editor| editor.media_id != id)
                {
                    return;
                }
                match result {
                    Ok(recs) => {
                        this.ensure_cover_scale(window.scale_factor());
                        for media in recs.iter().take(12) {
                            if let Some(url) = media.cover_medium.as_deref() {
                                this.request_cover_for(
                                    CoverOwner::Editor,
                                    (media.id, 96, 128),
                                    url,
                                );
                            }
                        }
                        this.editor_recs = recs;
                        this.editor_recs_error.clear();
                    }
                    Err(_) => this.editor_recs_error.clear(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn close_editor(&mut self, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_some_and(|editor| editor.busy) {
            return;
        }
        self.editor = None;
        self.retain_cover_route();
        self.cancel_reads("editor-recs");
        self.cancel_reads("editor-details");
        self.editor_status_open = false;
        self.editor_inputs = None;
        self.editor_subscriptions.clear();
        self.editor_recs.clear();
        self.editor_adding.clear();
        self.delete_waiting = false;
        cx.notify();
    }

    pub(super) fn requeue_editor_covers(&mut self) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let mut visible = Vec::with_capacity(13);
        if let Some(url) = editor
            .media
            .as_ref()
            .and_then(|media| media.cover_medium.as_deref())
        {
            visible.push(((editor.media_id, 48, 64), url.to_string()));
        }
        for media in self.editor_recs.iter().take(12) {
            if let Some(url) = media.cover_medium.as_deref() {
                visible.push(((media.id, 96, 128), url.to_string()));
            }
        }
        self.replace_cover_region(CoverOwner::Editor, visible, Vec::new());
    }

    fn load_editor_details(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        if self.editor_details_loading || editor.details.is_some() {
            return;
        }
        let id = editor.media_id;
        let editor_input = self
            .editor_inputs
            .as_ref()
            .map(|inputs| inputs.progress.entity_id());
        let generation = self.generation;
        self.editor_details_loading = true;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_entry_details(id).await });
        self.replace_reads("editor-details", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if generation != this.generation
                    || this
                        .editor_inputs
                        .as_ref()
                        .map(|inputs| inputs.progress.entity_id())
                        != editor_input
                    || this
                        .editor
                        .as_ref()
                        .is_none_or(|editor| editor.media_id != id)
                {
                    return;
                }
                this.editor_details_loading = false;
                match result {
                    Ok(details) => {
                        if let Some(inputs) = this.editor_inputs.as_ref() {
                            set_detail_inputs(inputs, &details, cx);
                        }
                        this.editor.as_mut().unwrap().apply_details(details);
                    }
                    Err(error) => {
                        this.editor.as_mut().unwrap().error =
                            format!("Entry details are unavailable. {error}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn clear_editor_score(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut().filter(|editor| !editor.busy) {
            editor.score = None;
            editor.score_cleared = true;
            if let Some(inputs) = self.editor_inputs.as_ref() {
                inputs.score.update(cx, |input, cx| input.set_value("", cx));
            }
        }
    }

    pub(super) fn read_editor_inputs(&mut self, cx: &gpui::App) -> Result<(), String> {
        let Some(inputs) = self.editor_inputs.as_ref() else {
            return Err("Editor is closed.".into());
        };
        let Some(editor) = self.editor.as_mut() else {
            return Err("Editor is closed.".into());
        };
        let progress = inputs
            .progress
            .read(cx)
            .value()
            .trim()
            .parse()
            .map_err(|_| "Enter a valid episode progress.".to_string())?;
        editor.set_progress(progress);
        editor.repeat = inputs
            .repeat
            .read(cx)
            .value()
            .trim()
            .parse()
            .map_err(|_| "Enter a valid rewatch count.".to_string())?;
        if !self.offline
            || self
                .profile
                .as_ref()
                .and_then(|user| user.score_format.as_ref())
                .is_some()
        {
            let score = inputs.score.read(cx).value().trim();
            let score = if score.is_empty() {
                None
            } else {
                Some(
                    score
                        .parse()
                        .map_err(|_| "Enter a valid score.".to_string())?,
                )
            };
            let format = editor.displayed_format.clone();
            editor.set_score(score, format.as_deref());
        }
        if editor.details.is_some() {
            editor.notes = inputs.notes.read(cx).value().to_string();
            if editor.notes.chars().count() > 6000 {
                return Err("Notes must be 6000 characters or less.".into());
            }
            editor.started_at = FuzzyDate {
                year: parse_part(inputs.started_year.read(cx).value(), 1, 9999)?,
                month: parse_part(inputs.started_month.read(cx).value(), 1, 12)?,
                day: parse_part(inputs.started_day.read(cx).value(), 1, 31)?,
            };
            editor.completed_at = FuzzyDate {
                year: parse_part(inputs.completed_year.read(cx).value(), 1, 9999)?,
                month: parse_part(inputs.completed_month.read(cx).value(), 1, 12)?,
                day: parse_part(inputs.completed_day.read(cx).value(), 1, 31)?,
            };
        }
        Ok(())
    }

    pub(super) fn save_editor(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_none_or(|editor| editor.busy) {
            return;
        }
        if let Err(error) = self.read_editor_inputs(cx) {
            self.editor.as_mut().unwrap().error = error;
            cx.notify();
            return;
        }
        let editor = self.editor.as_mut().unwrap();
        let details_patch = editor.details_patch();
        let basic_patch = editor.basic_patch(
            !self.offline
                || self
                    .profile
                    .as_ref()
                    .and_then(|user| user.score_format.as_ref())
                    .is_some(),
        );
        if details_patch.is_empty() && basic_patch.is_empty() {
            self.close_editor(cx);
            return;
        }
        if self.offline && !details_patch.is_empty() {
            editor.error =
                "Reconnect to AniList before saving notes, dates or custom lists.".into();
            cx.notify();
            return;
        }
        editor.busy = true;
        self.editor_status_open = false;
        editor.error.clear();
        let id = editor.media_id;
        let expected_progress = editor.saved_progress();
        let displayed_format = editor.displayed_format.clone();
        let generation = self.generation;
        let entry_revision = self.list.revision();
        self.set_editor_inputs_disabled(true, cx);
        self.active_writes += 1;
        cx.notify();
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = core
                .spawn(async move {
                    let mut saved_details = None;
                    if !details_patch.is_empty() {
                        match worker
                            .update_entry_details(
                                id,
                                details_patch.notes,
                                details_patch.started_at,
                                details_patch.completed_at,
                                details_patch.custom_lists,
                            )
                            .await
                        {
                            Ok(value) => saved_details = Some(value),
                            Err(error) => return (saved_details, Err(error)),
                        }
                    }
                    let basic = if basic_patch.is_empty() {
                        Ok(None)
                    } else {
                        worker
                            .update_entry_in_format(
                                id,
                                basic_patch.status,
                                basic_patch.progress,
                                basic_patch.score,
                                basic_patch.repeat,
                                Some(expected_progress),
                                displayed_format,
                            )
                            .await
                            .map(Some)
                    };
                    (saved_details, basic)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation
                    || this
                        .editor
                        .as_ref()
                        .is_none_or(|editor| editor.media_id != id)
                {
                    this.check_barrier(window, cx);
                    return;
                }
                this.editor.as_mut().unwrap().busy = false;
                match result {
                    Ok((saved_details, basic)) => {
                        if let Some(details) = saved_details {
                            this.editor.as_mut().unwrap().apply_details(details);
                        }
                        match basic {
                            Ok(entry) => {
                                if let Some(entry) = entry {
                                    this.editor.as_mut().unwrap().accept_basic(entry.clone());
                                    if this.list.revision() == entry_revision {
                                        let change =
                                            this.list.apply_one(entry.clone(), now_seconds());
                                        this.progress.reconcile_entry(generation, &entry);
                                        this.apply_list_change(change);
                                        this.discovery_list_changed();
                                        if this.detail.media_id == Some(id) {
                                            this.detail.entry = Some(entry);
                                            this.detail.entry_available = true;
                                        }
                                    } else {
                                        // A later snapshot owns the visible entry, including deletion.
                                        this.queue_list_refresh(Some(vec![id]), window, cx);
                                    }
                                }
                                this.close_editor(cx);
                            }
                            Err(error) => this.editor.as_mut().unwrap().error = error,
                        }
                    }
                    Err(error) => this.editor.as_mut().unwrap().error = error.to_string(),
                }
                if this.editor.is_some() {
                    this.set_editor_inputs_disabled(false, cx);
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_editor(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.offline {
            if let Some(editor) = self.editor.as_mut() {
                editor.error = "Reconnect to AniList before removing this entry.".into();
            }
            cx.notify();
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.busy {
            return;
        }
        if !editor.confirming_delete {
            editor.confirming_delete = true;
            self.editor_delete_confirmation = self.editor_delete_confirmation.wrapping_add(1);
            let confirmation = self.editor_delete_confirmation;
            let id = editor.media_id;
            let generation = self.generation;
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let _ = this.update_in(cx, |this, _window, cx| {
                    if this.generation == generation
                        && this.editor_delete_confirmation == confirmation
                        && this
                            .editor
                            .as_ref()
                            .is_some_and(|editor| editor.media_id == id)
                    {
                        this.editor.as_mut().unwrap().confirming_delete = false;
                        cx.notify();
                    }
                });
            })
            .detach();
            cx.notify();
            return;
        }
        editor.confirming_delete = false;
        let id = editor.media_id;
        if !self.progress.begin_delete(self.generation, id) {
            return;
        }
        editor.busy = true;
        self.editor_status_open = false;
        self.set_editor_inputs_disabled(true, cx);
        cx.notify();
        if self
            .progress
            .edit(self.generation, id)
            .is_some_and(|edit| edit.in_flight)
        {
            self.delete_waiting = true;
        } else {
            self.perform_delete(window, cx);
        }
    }

    pub(super) fn perform_delete(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let id = editor.media_id;
        let generation = self.generation;
        self.active_writes += 1;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.delete_entry_cmd(id).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                this.progress.finish_delete(generation, id, result.is_ok());
                if generation != this.generation {
                    this.check_barrier(window, cx);
                    return;
                }
                if let Some(editor) = this.editor.as_mut().filter(|editor| editor.media_id == id) {
                    editor.busy = false;
                    match result {
                        Ok(()) => {
                            let change = this.list.apply_scoped(&[id], vec![None], now_seconds());
                            this.apply_list_change(change);
                            this.discovery_list_changed();
                            if this.detail.media_id == Some(id) {
                                this.detail.entry = None;
                                this.detail.entry_available = true;
                            }
                            this.close_editor(cx);
                        }
                        Err(error) => editor.error = error,
                    }
                }
                if this.editor.is_some() {
                    this.set_editor_inputs_disabled(false, cx);
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_editor_detail(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.as_ref().is_none_or(|editor| editor.busy) {
            return;
        }
        self.close_editor(cx);
        self.navigate(Route::Anime(id), window, cx);
    }

    fn choose_editor_status(&mut self, status: &'static str, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut().filter(|editor| !editor.busy) else {
            return;
        };
        editor.choose_status(status);
        if let Some(inputs) = self.editor_inputs.as_ref() {
            inputs.progress.update(cx, |input, cx| {
                input.set_value(editor.progress.to_string(), cx)
            });
        }
        self.editor_status_open = false;
        cx.notify();
    }

    pub(super) fn render_editor(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any();
        };
        let Some(inputs) = self.editor_inputs.as_ref() else {
            return div().into_any();
        };
        if self.editor_status_open
            && (editor.busy
                || (self.editor_status_focus_subscription.is_some()
                    && !self.editor_status_focus.contains_focused(window, cx)))
        {
            self.editor_status_open = false;
        }
        let id = editor.media_id;
        let title = editor
            .media
            .as_ref()
            .map(Media::display_title)
            .unwrap_or_else(|| format!("#{id}"));
        let max_height = (f32::from(window.viewport_size().height) - 64.).max(300.);
        let mut form = dialog("entry-editor", "Edit entry")
            .debug_selector(|| "entry-editor".into())
            .w(px(510.))
            .max_w_full()
            .max_h(px(max_height))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.editor_status_open {
                    this.editor_status_open = false;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && this.editor_status_open {
                    this.editor_status_open = false;
                    window.focus(&this.editor_status_focus, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }));
        let mut header = div().w_full().flex().items_start().gap(px(12.)).mb(px(4.));
        if let Some(cover) = self.cover_images.get(&(id, 48, 64)).cloned() {
            header = header.child(
                gpui::img(cover)
                    .w(px(48.))
                    .h(px(64.))
                    .flex_shrink_0()
                    .rounded(px(4.))
                    .object_fit(gpui::ObjectFit::Cover),
            );
        }
        let watched = editor.total().map_or_else(
            || format!("{} eps watched", editor.saved_progress()),
            |total| format!("{}/{total} eps watched", editor.saved_progress()),
        );
        header = header.child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .id("editor-title")
                        .role(gpui::Role::Button)
                        .aria_label(format!("Open details for {title}"))
                        .focusable()
                        .tab_stop(true)
                        .cursor_pointer()
                        .font(serif_font())
                        .text_size(px(21.))
                        .line_height(px(26.))
                        .w_full()
                        .truncate()
                        .child(title)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_editor_detail(id, window, cx)
                        })),
                )
                .child(div().text_size(px(12.)).text_color(rgb(DIM)).child(watched)),
        );
        if let Some(file) = self
            .library_files
            .iter()
            .find(|file| file.media_id == Some(id) && file.episode == Some(editor.progress + 1))
        {
            let path = file.path.clone();
            let episode = file.episode.unwrap_or(editor.progress + 1);
            header = header.child(
                button("editor-play-next", format!("Play Ep {episode}"), false).on_click(
                    cx.listener(move |this, _, _, cx| {
                        if let Err(error) =
                            crate::platform::NativePlatform.open_path(std::path::Path::new(&path))
                        {
                            this.global_error = error;
                            cx.notify();
                        }
                    }),
                ),
            );
        }
        form = form.child(header);
        if !editor.error.is_empty() {
            form = form.child(
                div()
                    .id("editor-save-error")
                    .role(gpui::Role::Alert)
                    .aria_label(editor.error.clone())
                    .text_color(rgb(ERROR))
                    .child(editor.error.clone()),
            );
        }
        form = form.child(div().text_size(px(14.)).child("Status"));
        let mut status = select_with_disabled("editor-status", "Status", editor.busy)
            .when(!editor.busy, |control| {
                control.track_focus(&self.editor_status_focus)
            })
            .aria_expanded(self.editor_status_open && !editor.busy)
            .debug_selector(|| "editor-status".into())
            .relative()
            .w_full()
            .h(px(42.))
            .flex()
            .items_center()
            .justify_between()
            .child(status_label(&editor.status))
            .child("▾")
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if this.editor.as_ref().is_none_or(|editor| editor.busy) {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "up" | "arrowup" | "down" | "arrowdown" => {
                        cx.stop_propagation();
                        let current = this.editor.as_ref().map(|editor| editor.status.as_str());
                        let index = STATUSES
                            .iter()
                            .position(|status| Some(*status) == current)
                            .unwrap_or(0);
                        let next = if matches!(event.keystroke.key.as_str(), "up" | "arrowup") {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(STATUSES.len() - 1)
                        };
                        this.choose_editor_status(STATUSES[next], cx);
                    }
                    "escape" if this.editor_status_open => {
                        this.editor_status_open = false;
                        window.focus(&this.editor_status_focus, cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.editor.as_ref().is_none_or(|editor| editor.busy) {
                    return;
                }
                if this.editor_status_focus_subscription.is_none() {
                    let focus = this.editor_status_focus.clone();
                    this.editor_status_focus_subscription =
                        Some(cx.on_focus_out(&focus, window, |this, _, _, cx| {
                            if this.editor_status_open {
                                this.editor_status_open = false;
                                cx.notify();
                            }
                        }));
                }
                this.editor_status_open = !this.editor_status_open;
                cx.notify();
            }));
        if self.editor_status_open && self.active_modal() == Some(ModalLayer::Editor) {
            let mut choices = div()
                .id("editor-status-choices")
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .absolute()
                .top(px(42.))
                .left(px(0.))
                .w_full()
                .occlude()
                .bg(rgb(PANEL_2))
                .border_1()
                .border_color(rgb(EDGE))
                .flex()
                .flex_col();
            for value in STATUSES {
                choices = choices.child(
                    button(
                        format!("edit-status-{value}"),
                        status_label(value),
                        editor.busy,
                    )
                    .debug_selector(move || format!("edit-status-choice-{value}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.choose_editor_status(value, cx);
                        window.focus(&this.editor_status_focus, cx);
                    })),
                );
            }
            status = status.child(deferred(choices).priority(1));
        }
        form = form.child(
            div()
                .relative()
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .child(status),
        );
        let score_format = editor.displayed_format.as_deref().unwrap_or("POINT_100");
        let mut fields = div().flex().gap(px(10.));
        fields = fields.child(input_field(
            "Progress",
            inputs.progress.clone(),
            editor.total().map(|total| format!("/ {total}")),
        ));
        fields = fields.child(if matches!(score_format, "POINT_3" | "POINT_5") {
            div()
                .flex_1()
                .child(format!("Score {}", score_unit(score_format)))
                .into_any()
        } else {
            input_field(
                &format!("Score {}", score_unit(score_format)),
                inputs.score.clone(),
                None,
            )
        });
        fields = fields.child(div().w(px(96.)).flex_shrink_0().child(input_field(
            "Rewatches",
            inputs.repeat.clone(),
            None,
        )));
        form = form.child(fields);
        if matches!(score_format, "POINT_3" | "POINT_5") {
            let mut ratings = div().flex().gap(px(4.));
            ratings = ratings.child(button("score-clear", "Clear score", editor.busy).on_click(
                cx.listener(|this, _, _, cx| {
                    this.clear_editor_score(cx);
                    cx.notify();
                }),
            ));
            let limit = if score_format == "POINT_3" { 3 } else { 5 };
            for rating in 1..=limit {
                let label = if score_format == "POINT_3" {
                    ["😞", "😐", "😊"][(rating - 1) as usize]
                } else {
                    "★"
                };
                ratings = ratings.child(
                    button(
                        format!("score-choice-{rating}"),
                        format!("{label} {rating}"),
                        editor.busy,
                    )
                    .bg(rgb(if editor.score == Some(rating as f64) {
                        ACCENT
                    } else {
                        PANEL_2
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.editor.as_ref().is_none_or(|editor| editor.busy) {
                            return;
                        }
                        if let Some(editor) = this.editor.as_mut() {
                            editor.score = if editor.score == Some(rating as f64) {
                                None
                            } else {
                                Some(rating as f64)
                            };
                            if let Some(inputs) = this.editor_inputs.as_ref() {
                                inputs.score.update(cx, |input, cx| {
                                    input.set_value(optional_score(editor.score), cx)
                                });
                            }
                        }
                        cx.notify();
                    })),
                );
            }
            form = form.child(ratings);
        }
        form = form.child(
            div()
                .border_t_1()
                .border_color(rgb(EDGE))
                .pt(px(12.))
                .flex()
                .child(
                    button(
                        "edit-extra",
                        if self.editor_details_open {
                            "Notes, viewing dates and custom lists ▴"
                        } else {
                            "Notes, viewing dates and custom lists ▾"
                        },
                        editor.busy,
                    )
                    .bg(rgb(PANEL))
                    .border_color(rgb(PANEL))
                    .text_color(rgb(ACCENT))
                    .px(px(0.))
                    .py(px(0.))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.editor.as_ref().is_none_or(|editor| editor.busy) {
                            return;
                        }
                        this.editor_details_open = !this.editor_details_open;
                        if this.editor_details_open {
                            this.load_editor_details(window, cx);
                        }
                        cx.notify();
                    })),
                ),
        );
        if self.editor_details_open {
            if self.editor_details_loading {
                form = form.child(div().text_color(rgb(DIM)).child("Loading entry details…"));
            }
            if let Some(details) = editor.details.as_ref() {
                if let Some(warning) = details.warning.as_ref() {
                    form = form.child(div().text_color(rgb(DIM)).child(format!(
                        "Showing saved entry details. Reconnect before saving changes. {warning}"
                    )));
                }
                form = form
                    .child(eyebrow("Notes"))
                    .child(inputs.notes.clone())
                    .child(
                        div()
                            .text_color(rgb(DIM))
                            .text_size(px(11.))
                            .child("Notes follow your AniList list privacy settings."),
                    );
                form = form
                    .child(eyebrow("Started viewing"))
                    .child(date_inputs(
                        "started",
                        &inputs.started_year,
                        &inputs.started_month,
                        &inputs.started_day,
                    ))
                    .child(eyebrow("Completed viewing"))
                    .child(date_inputs(
                        "completed",
                        &inputs.completed_year,
                        &inputs.completed_month,
                        &inputs.completed_day,
                    ))
                    .child(
                        div()
                            .text_color(rgb(DIM))
                            .text_size(px(11.))
                            .child("Leave unknown date parts blank."),
                    );
                if details.available_custom_lists.is_empty() {
                    form = form.child(div().text_color(rgb(DIM)).text_size(px(12.)).child("Create custom list names in your AniList list settings to assign them here."));
                } else {
                    form = form.child(eyebrow("Custom lists"));
                    let mut lists = div().flex().flex_wrap().gap(px(6.));
                    for name in &details.available_custom_lists {
                        let selected = editor.custom_lists.contains(name);
                        let label = name.clone();
                        lists = lists.child(
                            button(
                                format!("custom-{name}"),
                                format!("{} {name}", if selected { "☑" } else { "☐" }),
                                editor.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if this.editor.as_ref().is_none_or(|editor| editor.busy) {
                                        return;
                                    }
                                    if let Some(editor) = this.editor.as_mut() {
                                        if let Some(index) = editor
                                            .custom_lists
                                            .iter()
                                            .position(|item| item == &label)
                                        {
                                            editor.custom_lists.remove(index);
                                        } else {
                                            editor.custom_lists.push(label.clone());
                                        }
                                    }
                                    cx.notify();
                                },
                            )),
                        );
                    }
                    form = form.child(lists);
                }
            }
        }
        form = form.child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .mt(px(8.))
                .child(
                    button(
                        "delete-entry",
                        if editor.confirming_delete {
                            "Confirm remove"
                        } else {
                            "Remove from list"
                        },
                        editor.busy || self.offline,
                    )
                    .text_color(rgb(ERROR))
                    .bg(rgb(PANEL))
                    .border_color(rgb(PANEL))
                    .on_click(cx.listener(|this, _, window, cx| this.delete_editor(window, cx))),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            button("cancel-editor", "Cancel", editor.busy)
                                .bg(rgb(PANEL_2))
                                .border_color(rgb(PANEL_2))
                                .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))),
                        )
                        .child(
                            button(
                                "save-editor",
                                if editor.busy { "Saving…" } else { "Save" },
                                editor.busy,
                            )
                            .bg(rgb(ACCENT))
                            .border_color(rgb(ACCENT))
                            .text_color(rgb(INK))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_editor(window, cx)),
                            ),
                        ),
                ),
        );
        if !self.editor_recs.is_empty() {
            form = form.child(eyebrow("You might also like"));
            let mut recs = div()
                .id("editor-recommendations")
                .w_full()
                .min_w_0()
                .flex()
                .gap(px(8.))
                .overflow_x_scroll()
                .pb(px(4.));
            for media in self.editor_recs.iter().take(12) {
                let rec_id = media.id;
                let added = self.added_recommendations.contains(&rec_id);
                let busy = self.editor_adding.contains(&rec_id) || added;
                let title = media.display_title();
                let cover = self.cover_images.get(&(rec_id, 96, 128)).cloned();
                let cover = if let Some(image) = cover {
                    img(image)
                        .w(px(96.))
                        .h(px(128.))
                        .object_fit(gpui::ObjectFit::Cover)
                        .into_any()
                } else {
                    div().w(px(96.)).h(px(128.)).bg(rgb(PANEL_2)).into_any()
                };
                recs = recs.child(
                    div()
                        .id(format!("editor-rec-{rec_id}"))
                        .role(gpui::Role::Button)
                        .aria_label(format!("{title} — add to Plan to Watch"))
                        .focusable()
                        .tab_stop(!busy)
                        .w(px(96.))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .items_start()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if !busy {
                                this.add_media(rec_id, "PLANNING", window, cx);
                            }
                        }))
                        .child(
                            div()
                                .w(px(96.))
                                .h(px(128.))
                                .overflow_hidden()
                                .rounded(px(4.))
                                .child(cover),
                        )
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .mt(px(4.))
                                .text_size(px(12.))
                                .text_color(rgb(DIM))
                                .whitespace_normal()
                                .line_clamp(2)
                                .child(if added { "✓ Added".into() } else { title }),
                        ),
                );
            }
            form = form.child(recs);
        }
        if !self.editor_recs_error.is_empty() {
            form = form.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(self.editor_recs_error.clone()),
            );
        }
        form.into_any()
    }
}

fn set_detail_inputs(inputs: &EditorInputs, details: &EntryDetails, cx: &mut Context<Workspace>) {
    inputs
        .notes
        .update(cx, |input, cx| input.set_value(details.notes.clone(), cx));
    let started = details.started_at.clone().unwrap_or_default();
    let completed = details.completed_at.clone().unwrap_or_default();
    for (field, value) in [
        (&inputs.started_year, started.year),
        (&inputs.started_month, started.month),
        (&inputs.started_day, started.day),
        (&inputs.completed_year, completed.year),
        (&inputs.completed_month, completed.month),
        (&inputs.completed_day, completed.day),
    ] {
        field.update(cx, |input, cx| input.set_value(optional_number(value), cx));
    }
}

fn input_field(label: &str, input: Entity<TextInput>, hint: Option<String>) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(14.))
                .child(format!("{label} {}", hint.unwrap_or_default())),
        )
        .child(input)
        .into_any()
}

fn date_inputs(
    id: &str,
    year: &Entity<TextInput>,
    month: &Entity<TextInput>,
    day: &Entity<TextInput>,
) -> AnyElement {
    div()
        .id(format!("{id}-date"))
        .flex()
        .gap(px(7.))
        .child(input_field("Year", year.clone(), None))
        .child(input_field("Month", month.clone(), None))
        .child(input_field("Day", day.clone(), None))
        .into_any()
}

fn score_unit(format: &str) -> &'static str {
    match format {
        "POINT_3" => "(smileys)",
        "POINT_5" => "(1–5)",
        "POINT_10" | "POINT_10_DECIMAL" => "(0–10)",
        _ => "(0–100)",
    }
}

fn optional_score(value: Option<f64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::EditorState;
    use kurisu_core::models::{ListEntry, Media};

    #[test]
    fn explicit_rewatch_at_total_keeps_one_basic_patch() {
        let entry = ListEntry {
            media_id: 1,
            status: "REPEATING".into(),
            progress: 11,
            repeat: 2,
            media: Some(Media {
                id: 1,
                episodes: Some(12),
                ..Media::default()
            }),
            ..ListEntry::default()
        };
        let mut editor = EditorState::new(entry);
        editor.choose_status("REPEATING");
        editor.set_progress(12);
        let patch = editor.basic_patch(true);
        assert_eq!(patch.status.as_deref(), Some("REPEATING"));
        assert_eq!(patch.progress, Some(12));
        assert_eq!(patch.repeat, None);
        assert_eq!(editor.saved_progress(), 11);
    }
    use super::Workspace;
    use std::{path::Path, sync::Arc};

    struct Platform;
    impl kurisu_core::Platform for Platform {
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
    struct FixtureDir(std::path::PathBuf);
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn editor_fixture() -> (kurisu_core::Core, tokio::runtime::Runtime, FixtureDir) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kurisu-editor-reopen-{}-{nonce}",
            std::process::id()
        ));
        let _dir = FixtureDir(path.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::open(
            path.clone(),
            runtime.handle().clone(),
            Arc::new(Platform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        (core, runtime, _dir)
    }

    fn total_entry(status: &str, progress: i64) -> ListEntry {
        ListEntry {
            id: Some(22),
            media_id: 71,
            progress,
            status: status.into(),
            repeat: 2,
            score: Some(70.0),
            media: Some(Media {
                id: 71,
                episodes: Some(12),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[gpui::test]
    fn untouched_total_progress_preserves_status_and_score_only_patch(
        cx: &mut gpui::TestAppContext,
    ) {
        let (core, runtime, _dir) = editor_fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        for status in ["PAUSED", "REPEATING", "CURRENT"] {
            workspace.update_in(cx, |w, window, cx| {
                w.list.replace(vec![total_entry(status, 12)], 0);
                w.open_editor(71, window, cx);
                assert!(w.editor.as_ref().unwrap().basic_patch(true).is_empty());
                w.read_editor_inputs(cx).unwrap();
                let editor = w.editor.as_ref().unwrap();
                assert_eq!(editor.status, status);
                assert!(editor.basic_patch(true).is_empty());
                w.editor_inputs
                    .as_ref()
                    .unwrap()
                    .score
                    .update(cx, |input, cx| {
                        input.set_value("80", cx);
                    });
                w.read_editor_inputs(cx).unwrap();
                let editor = w.editor.as_ref().unwrap();
                let patch = editor.basic_patch(true);
                assert_eq!(editor.status, status);
                assert_eq!(patch.score, Some(80.0));
                assert_eq!(patch.progress, None);
                assert_eq!(patch.status, None);
                assert_eq!(patch.repeat, None);
                w.close_editor(cx);
            });
        }
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }

    #[gpui::test]
    fn untouched_total_save_and_close_do_not_start_writes(cx: &mut gpui::TestAppContext) {
        let (core, runtime, _dir) = editor_fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        for _ in 0..16 {
            cx.run_until_parked();
            runtime.block_on(async { tokio::task::yield_now().await });
        }
        workspace.update_in(cx, |w, window, cx| {
            w.list.replace(vec![total_entry("PAUSED", 12)], 0);
            w.open_editor(71, window, cx);
            w.save_editor(window, cx);
            assert!(w.editor.is_none());
            assert_eq!(w.active_writes, 0);
            w.open_editor(71, window, cx);
            w.request_close(super::super::CloseIntent::Quit, window, cx);
            assert!(w.barrier.is_none());
            assert!(w.barrier_error.is_empty());
            assert_eq!(w.active_writes, 0);
            assert_eq!(w.editor.as_ref().unwrap().status, "PAUSED");
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }

    #[gpui::test]
    fn rewatch_overshoot_keeps_raw_progress_after_live_and_save_reads(
        cx: &mut gpui::TestAppContext,
    ) {
        let (core, runtime, _dir) = editor_fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            w.list.replace(vec![total_entry("REPEATING", 11)], 0);
            w.open_editor(71, window, cx);
        });
        for (value, status) in [
            (" 13 ", "REPEATING"),
            (" 12 ", "COMPLETED"),
            ("13", "REPEATING"),
        ] {
            workspace.update_in(cx, |w, _, cx| {
                w.editor_inputs
                    .as_ref()
                    .unwrap()
                    .progress
                    .update(cx, |input, cx| {
                        input.set_value(value, cx);
                        cx.emit(super::TextInputEvent::Changed(value.into()));
                    });
            });
            cx.run_until_parked();
            workspace.update_in(cx, |w, _, cx| {
                assert_eq!(w.editor.as_ref().unwrap().status, status);
                w.read_editor_inputs(cx).unwrap();
                let editor = w.editor.as_ref().unwrap();
                assert_eq!(editor.status, status);
                assert_eq!(editor.repeat, 2);
                assert_eq!(
                    editor.basic_patch(true).progress,
                    Some(value.trim().parse().unwrap())
                );
            });
        }
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }

    #[gpui::test]
    fn whitespace_progress_updates_completion_before_save(cx: &mut gpui::TestAppContext) {
        let (core, runtime, _dir) = editor_fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            w.list.replace(
                vec![ListEntry {
                    media_id: 71,
                    progress: 3,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id: 71,
                        episodes: Some(12),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                0,
            );
            w.open_editor(71, window, cx);
            let input = w.editor_inputs.as_ref().unwrap().progress.clone();
            input.update(cx, |input, cx| {
                input.set_value(" 12 ", cx);
                cx.emit(super::TextInputEvent::Changed(" 12 ".into()));
            });
        });
        cx.run_until_parked();
        workspace.update_in(cx, |w, _, cx| {
            let editor = w.editor.as_ref().unwrap();
            assert_eq!(editor.progress, 12);
            assert_eq!(editor.status, "COMPLETED");
            w.read_editor_inputs(cx).unwrap();
            assert_eq!(
                w.editor.as_ref().unwrap().basic_patch(true).progress,
                Some(12)
            );
            w.editor_inputs
                .as_ref()
                .unwrap()
                .progress
                .update(cx, |input, cx| {
                    input.set_value(" 4 ", cx);
                });
            w.read_editor_inputs(cx).unwrap();
            let editor = w.editor.as_ref().unwrap();
            assert_eq!(editor.progress, 4);
            assert_eq!(editor.status, "CURRENT");
            assert_eq!(editor.basic_patch(true).progress, Some(4));
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }

    #[gpui::test]
    fn reopened_editor_rejects_old_details_completion(cx: &mut gpui::TestAppContext) {
        let (core, runtime, _dir) = editor_fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        for _ in 0..16 {
            cx.run_until_parked();
            runtime.block_on(async { tokio::task::yield_now().await });
        }
        workspace.update_in(cx, |w, window, cx| {
            w.list.replace(
                vec![ListEntry {
                    media_id: 71,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                }],
                0,
            );
            w.open_editor(71, window, cx);
            w.load_editor_details(window, cx);
            assert!(w.editor_details_loading);
        });
        // Finish the old service read without dispatching its UI callback.
        runtime.block_on(async { tokio::task::yield_now().await });
        workspace.update_in(cx, |w, window, cx| {
            w.close_editor(cx);
            w.open_editor(71, window, cx);
            w.load_editor_details(window, cx);
            assert!(w.editor_details_loading);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(
                w.editor_details_loading,
                "Old completion cleared the new editor's loading state"
            );
            assert!(w.editor.as_ref().unwrap().error.is_empty());
        });
        for _ in 0..16 {
            runtime.block_on(async { tokio::task::yield_now().await });
            cx.run_until_parked();
        }
        workspace.read_with(cx, |w, _| {
            assert!(!w.editor_details_loading);
            assert!(
                w.editor
                    .as_ref()
                    .unwrap()
                    .error
                    .starts_with("Entry details are unavailable.")
            );
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }
    #[gpui::test]
    fn reopened_editor_keeps_its_removal_confirmation(cx: &mut gpui::TestAppContext) {
        check_removal_confirmation_timeout(cx, false);
    }

    #[gpui::test]
    fn failed_removal_retry_keeps_its_confirmation(cx: &mut gpui::TestAppContext) {
        check_removal_confirmation_timeout(cx, true);
    }

    fn check_removal_confirmation_timeout(cx: &mut gpui::TestAppContext, retry: bool) {
        use std::time::{Duration, Instant};
        let (core, runtime, _dir) = editor_fixture();
        if retry {
            let db = kurisu_core::Db::open(&_dir.0.join("kurisu.db")).unwrap();
            db.upsert_entry(&ListEntry {
                media_id: 71,
                progress: 3,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        for _ in 0..16 {
            cx.run_until_parked();
            runtime.block_on(async { tokio::task::yield_now().await });
        }
        workspace.update_in(cx, |w, window, cx| {
            w.list.replace(
                vec![ListEntry {
                    media_id: 71,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                }],
                0,
            );
            w.open_editor(71, window, cx);
            w.delete_editor(window, cx);
            assert!(w.editor.as_ref().unwrap().confirming_delete);
        });
        cx.run_until_parked();
        if retry {
            workspace.update_in(cx, |w, window, cx| {
                w.delete_editor(window, cx);
                assert!(w.editor.as_ref().unwrap().busy);
            });
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                runtime.block_on(async { tokio::task::yield_now().await });
                cx.run_until_parked();
                if workspace.read_with(cx, |w, _| !w.editor.as_ref().unwrap().busy) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Removal must finish before retrying"
                );
            }
            workspace.read_with(cx, |w, _| {
                assert_eq!(w.editor.as_ref().unwrap().error, "not authenticated");
                assert!(!w.editor.as_ref().unwrap().confirming_delete);
                assert_eq!(w.active_writes, 0);
                assert_eq!(w.list.entry(71).unwrap().progress, 3);
            });
        }
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        workspace.update_in(cx, |w, window, cx| {
            if !retry {
                w.close_editor(cx);
                w.open_editor(71, window, cx);
            }
            w.delete_editor(window, cx);
            assert!(w.editor.as_ref().unwrap().confirming_delete);
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(1100));
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(
                w.editor.as_ref().unwrap().confirming_delete,
                "An old timeout must not expire the current confirmation after 1.1 seconds"
            );
        });
        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(
                !w.editor.as_ref().unwrap().confirming_delete,
                "The current confirmation must expire after its own four-second window"
            );
            assert!(!w.editor.as_ref().unwrap().busy);
            assert_eq!(w.list.entry(71).unwrap().progress, 3);
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }
    #[cfg(feature = "fixture-runtime")]
    #[gpui::test]
    fn reopened_editor_rejects_old_recommendations(cx: &mut gpui::TestAppContext) {
        use std::time::{Duration, Instant};

        const CHILD: &str = "KURISU_EDITOR_RECOMMENDATION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            run_recommendation_fixture(CHILD);
            return;
        }
        assert!(
            std::env::var_os("KURISU_FIXTURE_GRAPHQL").is_some(),
            "The child requires the loopback fixture endpoint"
        );
        let (core, runtime, dir) = editor_fixture();
        drop(core);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        db.replace_account(
            "synthetic-editor-token",
            &kurisu_core::User {
                id: 47,
                name: "Fixture".into(),
                ..Default::default()
            },
        )
        .unwrap();
        drop(db);
        let core = kurisu_core::Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(Platform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            cx.run_until_parked();
            if workspace.read_with(cx, |w, _| w.profile.is_some() && w.list.loaded) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Account initialization must finish"
            );
        }
        let old_input = workspace.update_in(cx, |w, window, cx| {
            w.list.replace(
                vec![ListEntry {
                    media_id: 71,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                }],
                0,
            );
            w.open_editor(71, window, cx);
            w.editor_inputs.as_ref().unwrap().progress.entity_id()
        });
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            // Finish the Core read while its UI callback remains queued.
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            if workspace.read_with(cx, |w, _| {
                w.read_aborts["editor-recs"]
                    .iter()
                    .all(|job| job.is_finished())
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Old recommendation read must finish"
            );
        }
        workspace.update_in(cx, |w, window, cx| {
            w.close_editor(cx);
            w.open_editor(71, window, cx);
            assert_ne!(
                w.editor_inputs.as_ref().unwrap().progress.entity_id(),
                old_input
            );
            w.editor_recs = vec![Media {
                id: 799,
                ..Default::default()
            }];
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert_eq!(
                w.editor_recs
                    .iter()
                    .map(|media| media.id)
                    .collect::<Vec<_>>(),
                [799],
                "Old successful recommendations must not replace the reopened editor"
            );
        });
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
            cx.run_until_parked();
            if workspace.read_with(cx, |w, _| {
                w.editor_recs.first().is_some_and(|media| media.id == 702)
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Current recommendations must arrive"
            );
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.editor_recs.len(), 1);
            assert_eq!(
                w.editor_recs[0].title_english.as_deref(),
                Some("Recommendation 702")
            );
            assert!(w.editor_recs_error.is_empty());
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }

    #[cfg(feature = "fixture-runtime")]
    fn run_recommendation_fixture(child_flag: &str) {
        use serde_json::json;
        let mut recommendations = 0;
        run_graphql_fixture(
            child_flag,
            "screens::editor::tests::reopened_editor_rejects_old_recommendations",
            |request| {
                let query = request["query"].as_str().unwrap();
                if query.contains("recommendations(mediaId:") {
                    assert_eq!(request["variables"]["mediaId"], 71);
                    recommendations += 1;
                    let id = 700 + recommendations;
                    json!({"Page":{"recommendations":[{"mediaRecommendation":{
                        "id":id, "idMal":null,
                        "title":{"romaji":null,"english":format!("Recommendation {id}"),"native":null},
                        "coverImage":null, "episodes":24, "format":"TV", "status":"FINISHED",
                        "averageScore":null,"season":null,"seasonYear":null,
                        "description":null,"nextAiringEpisode":null
                    }}]}})
                } else if query.contains("MediaListCollection") {
                    json!({"MediaListCollection":{"lists":[],"hasNextChunk":false}})
                } else if query.contains("Viewer") {
                    json!({"Viewer":{"id":47,"name":"Fixture"}})
                } else {
                    panic!("Unexpected fixture query: {query}");
                }
            },
        );
        assert_eq!(recommendations, 2, "Both HTTP reads must succeed");
    }

    #[cfg(feature = "fixture-runtime")]
    #[test]
    fn editor_patch_preserves_core_completion_contract() {
        use serde_json::json;
        const CHILD: &str = "KURISU_EDITOR_COMPLETION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let mut mutations = Vec::new();
            let mut statuses = ["REPEATING"; 7].into_iter().chain(["PAUSED"]);
            run_graphql_fixture(
                CHILD,
                "screens::editor::tests::editor_patch_preserves_core_completion_contract",
                |request| {
                    let query = request["query"].as_str().unwrap();
                    if query.contains("Viewer") {
                        return json!({"Viewer":{"id":47,"name":"Fixture","mediaListOptions":{"scoreFormat":"POINT_100"}}});
                    }
                    assert!(query.contains("SaveMediaListEntry"));
                    let before_status = statuses.next().unwrap();
                    let variables = &request["variables"];
                    mutations.push(variables.clone());
                    json!({"SaveMediaListEntry":{
                        "id":22, "status":variables.get("status").cloned().unwrap_or(json!(before_status)),
                        "progress":variables.get("progress").cloned().unwrap_or(json!(12)),
                        "repeat":variables.get("repeat").cloned().unwrap_or(json!(2)),
                        "score":variables.get("score").cloned().unwrap_or(json!(70.0)),
                        "updatedAt":100
                    }})
                },
            );
            assert_eq!(
                mutations,
                vec![
                    json!({"id":22,"progress":12,"scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"status":"COMPLETED","repeat":3,"scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"status":"REPEATING","scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"status":"PAUSED","scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"status":"COMPLETED","repeat":5,"scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"scoreFormat":"POINT_100"}),
                    json!({"id":22,"progress":12,"status":"COMPLETED","repeat":3,"scoreFormat":"POINT_100"}),
                    json!({"id":22,"score":80.0,"scoreFormat":"POINT_100"}),
                ]
            );
            return;
        }
        let (core, runtime, dir) = editor_fixture();
        drop(core);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        db.replace_account(
            "synthetic-token",
            &kurisu_core::models::User {
                id: 47,
                name: "Fixture".into(),
                score_format: Some("POINT_100".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let core = kurisu_core::Core::from_db(
            db,
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(Platform),
            kurisu_core::UpdateChannel::Native,
        );
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        for (index, (requested, chosen, repeat, expected_status, expected_repeat)) in [
            (13, None, None, "REPEATING", 2),
            (12, None, None, "COMPLETED", 3),
            (12, Some("REPEATING"), None, "REPEATING", 2),
            (12, Some("PAUSED"), None, "PAUSED", 2),
            (12, None, Some(5), "COMPLETED", 5),
            (13, None, None, "REPEATING", 2),
            (13, Some("COMPLETED"), None, "COMPLETED", 3),
            (12, None, None, "PAUSED", 2),
        ]
        .into_iter()
        .enumerate()
        {
            let score_only = index == 7;
            let entry = total_entry(
                if score_only { "PAUSED" } else { "REPEATING" },
                if score_only { 12 } else { 11 },
            );
            db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
            db.upsert_entry(&entry).unwrap();
            let mut editor = EditorState::new(entry);
            if index == 5 {
                editor.set_progress(12);
            }
            editor.set_progress(requested);
            if let Some(status) = chosen {
                editor.choose_status(status);
            }
            if let Some(repeat) = repeat {
                editor.repeat = repeat;
            }
            if score_only {
                editor.set_score(Some(80.0), Some("POINT_100"));
            }
            let patch = editor.basic_patch(true);
            let saved = runtime
                .block_on(core.update_entry_in_format(
                    71,
                    patch.status,
                    patch.progress,
                    patch.score,
                    patch.repeat,
                    Some(editor.saved_progress()),
                    Some("POINT_100".into()),
                ))
                .unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                (expected_status, 12, expected_repeat)
            );
            let persisted = db.get_entry(71).unwrap().unwrap();
            assert_eq!(
                (
                    persisted.status.as_str(),
                    persisted.progress,
                    persisted.repeat
                ),
                (expected_status, 12, expected_repeat)
            );
            assert_eq!(persisted.score, Some(if score_only { 80.0 } else { 70.0 }));
        }
    }

    #[cfg(feature = "fixture-runtime")]
    fn run_graphql_fixture(
        child_flag: &str,
        test_name: &str,
        mut data: impl FnMut(serde_json::Value) -> serde_json::Value,
    ) {
        use serde_json::json;
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::TcpListener,
            process::{Command, Stdio},
            time::{Duration, Instant},
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        // Configure the existing loopback seam in a child, without changing test-process globals.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
            .env(child_flag, "1")
            .env(
                "KURISU_FIXTURE_GRAPHQL",
                format!("http://{}", listener.local_addr().unwrap()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                assert!(Instant::now() < deadline, "Editor child timed out");
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("Fixture accept failed: {error}"),
                };
                // Windows inherits the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut length = None;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.unwrap()];
                reader.read_exact(&mut body).unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let response = json!({"data":data(request)}).to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            }
        }));
        if served.is_err() {
            let _ = child.kill();
        }
        let output = child.wait_with_output().unwrap();
        if let Err(error) = served {
            std::panic::resume_unwind(error);
        }
        assert!(
            output.status.success(),
            "Editor child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
