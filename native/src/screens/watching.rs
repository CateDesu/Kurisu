use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
};

use gpui::{
    AnyElement, Bounds, Context, ListAlignment, ListOffset, ListState, Pixels, RenderImage, Size,
    WeakEntity, Window, div, img, list, prelude::*, px, rgb,
};
use kurisu_core::Platform;
use kurisu_core::models::{LibraryFile, ListEntry};

use super::{
    Workspace,
    covers::{CoverKey, CoverOwner},
    joined, now_seconds,
    state::LibraryFiles,
};
use crate::components::{button, heading, scrollbar::list_scrollbar};
use crate::contracts::Route;
use crate::theme::{ACCENT, DIM, EDGE, ERROR, INK, PANEL, PANEL_2};

pub(super) fn route_header(
    section: &str,
    title: &str,
    description: &str,
    action: Option<AnyElement>,
    compact: bool,
    page_gap: f32,
) -> gpui::Div {
    super::search::discovery_heading(
        section,
        title.to_owned(),
        description.to_owned(),
        action,
        compact,
    )
    .mb(px(18. - page_gap))
}

pub(super) struct WatchingState {
    current: Vec<ListEntry>,
    loaded: bool,
    loading: bool,
    error: String,
    playback_entry: Option<ListEntry>,
    playback_id: Option<i64>,
    playback_loading: bool,
    playback_checked: bool,
    playback_request: u64,
    request: u64,
    list_widget: ListState,
    scroll_bound: bool,
    cover_revision: u64,
    cover_reconciled: u64,
    cover_scheduled: Option<(u64, i64)>,
    cover_window_size: Option<Size<Pixels>>,
    cover_bounds: Option<Bounds<Pixels>>,
    cover_layout_key: Option<u64>,
    cover_visible: HashSet<CoverKey>,
    updating: bool,
    update_error: String,
    update_request: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchingSlot {
    Header,
    Row(usize),
    Footer,
}

impl WatchingSlot {
    fn count(rows: usize) -> usize {
        rows + 2
    }

    fn at(item_ix: usize, rows: usize) -> Self {
        if item_ix == 0 {
            Self::Header
        } else if item_ix <= rows {
            Self::Row(item_ix - 1)
        } else {
            Self::Footer
        }
    }

    fn row_ix(index: usize) -> usize {
        index + 1
    }

    fn footer_ix(rows: usize) -> usize {
        rows + 1
    }

    fn row_range(range: std::ops::Range<usize>, rows: usize) -> std::ops::Range<usize> {
        range.start.saturating_sub(1).min(rows)..range.end.saturating_sub(1).min(rows)
    }
}

#[derive(Clone, Copy)]
enum WatchingAnchor {
    Header(Pixels),
    Row(i64, usize, Pixels),
    Footer(Pixels),
    End,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct WatchingRead {
    generation: u64,
    account: Option<i64>,
    request: u64,
}

impl Default for WatchingState {
    fn default() -> Self {
        Self {
            current: Vec::new(),
            loaded: false,
            loading: false,
            error: String::new(),
            playback_entry: None,
            playback_id: None,
            playback_loading: false,
            playback_checked: false,
            playback_request: 0,
            request: 0,
            list_widget: ListState::new(WatchingSlot::count(0), ListAlignment::Top, px(86.)),
            scroll_bound: false,
            cover_revision: 0,
            cover_reconciled: 0,
            cover_scheduled: None,
            cover_window_size: None,
            cover_bounds: None,
            cover_layout_key: None,
            cover_visible: HashSet::new(),
            updating: false,
            update_error: String::new(),
            update_request: 0,
        }
    }
}

impl WatchingState {
    fn anchor(&self) -> WatchingAnchor {
        let offset = self.list_widget.logical_scroll_top();
        if offset.item_ix >= WatchingSlot::count(self.current.len()) {
            return WatchingAnchor::End;
        }
        match WatchingSlot::at(offset.item_ix, self.current.len()) {
            WatchingSlot::Header => WatchingAnchor::Header(offset.offset_in_item),
            WatchingSlot::Row(index) => {
                WatchingAnchor::Row(self.current[index].media_id, index, offset.offset_in_item)
            }
            WatchingSlot::Footer => WatchingAnchor::Footer(offset.offset_in_item),
        }
    }

    fn restore_anchor(&mut self, anchor: WatchingAnchor) {
        self.list_widget
            .reset(WatchingSlot::count(self.current.len()));
        match anchor {
            WatchingAnchor::Header(inset) => self.list_widget.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: inset,
            }),
            WatchingAnchor::Row(id, old_index, inset) => {
                let index = self
                    .current
                    .iter()
                    .position(|entry| entry.media_id == id)
                    .or_else(|| {
                        (!self.current.is_empty()).then(|| old_index.min(self.current.len() - 1))
                    });
                if let Some(index) = index {
                    self.list_widget.scroll_to(ListOffset {
                        item_ix: WatchingSlot::row_ix(index),
                        offset_in_item: inset,
                    });
                } else {
                    self.list_widget.scroll_to_end();
                }
            }
            WatchingAnchor::Footer(inset) => self.list_widget.scroll_to(ListOffset {
                item_ix: WatchingSlot::footer_ix(self.current.len()),
                offset_in_item: inset,
            }),
            WatchingAnchor::End => self.list_widget.scroll_to_end(),
        }
    }

    fn replace_rows(&mut self, rows: Vec<ListEntry>) {
        let anchor = self.anchor();
        self.current = rows;
        self.restore_anchor(anchor);
        self.cover_revision = self.cover_revision.wrapping_add(1);
    }

    fn remove_row(&mut self, id: i64) {
        let anchor = self.anchor();
        self.current.retain(|entry| entry.media_id != id);
        self.restore_anchor(anchor);
        self.cover_revision = self.cover_revision.wrapping_add(1);
    }

    fn retry_playback_on_enter(&mut self) {
        if self.playback_entry.is_none() && !self.playback_loading {
            self.playback_checked = false;
        }
    }

    fn begin_playback(&mut self, id: Option<i64>) -> Option<u64> {
        if self.playback_id == id && (self.playback_loading || self.playback_checked) {
            return None;
        }
        self.playback_id = id;
        self.playback_entry = None;
        self.playback_request += 1;
        self.playback_loading = id.is_some();
        self.playback_checked = id.is_none();
        Some(self.playback_request)
    }

    fn playback(&self, id: Option<i64>) -> Option<&ListEntry> {
        let id = id?;
        self.playback_entry
            .as_ref()
            .filter(|entry| entry.media_id == id)
            .or_else(|| self.current.iter().find(|entry| entry.media_id == id))
    }

    fn accept_playback_update(&mut self, id: i64, entry: Option<ListEntry>) -> bool {
        if self.playback_id != Some(id) {
            return false;
        }
        // A read started before the write must not restore old progress.
        self.playback_request += 1;
        self.playback_loading = false;
        self.playback_checked = true;
        self.playback_entry = entry;
        true
    }

    fn accept_list(
        &mut self,
        captured: WatchingRead,
        current: WatchingRead,
        result: Result<Vec<ListEntry>, String>,
    ) -> bool {
        if captured != current || captured.request != self.request {
            return false;
        }
        self.loading = false;
        match result {
            Ok(entries) => {
                self.replace_rows(
                    entries
                        .into_iter()
                        .filter(|entry| entry.status == "CURRENT")
                        .collect(),
                );
                self.loaded = true;
                self.error.clear();
            }
            Err(error) => {
                self.loaded = false;
                self.error = error;
            }
        }
        true
    }
}

impl Workspace {
    pub(super) fn enter_watching(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            return;
        }
        if !self.watching.scroll_bound {
            let weak = cx.weak_entity();
            self.watching
                .list_widget
                .set_scroll_handler(move |event, window, app| {
                    let _ = weak.update(app, |this, cx| {
                        if this.route != Route::Now || this.profile.is_none() {
                            return;
                        }
                        this.prefetch_watching_covers(
                            WatchingSlot::row_range(
                                event.visible_range.clone(),
                                this.watching.current.len(),
                            ),
                            window.scale_factor(),
                        );
                        this.mark_watching_cover_refresh();
                        this.schedule_watching_cover_refresh(window, cx);
                    });
                });
            self.watching.scroll_bound = true;
        }
        if !self.watching.loading {
            self.load_watching(window, cx);
        }
        self.watching.retry_playback_on_enter();
        self.refresh_watching_playback(window, cx);
        self.ensure_library_fresh(window, cx);
    }

    pub(super) fn refresh_watching_covers(&mut self) {
        let offset = self.watching.list_widget.logical_scroll_top().item_ix;
        self.prefetch_watching_covers(
            WatchingSlot::row_range(
                offset..offset.saturating_add(36),
                self.watching.current.len(),
            ),
            self.cover_scale,
        );
        self.refresh_playback_cover();
        self.mark_watching_cover_refresh();
    }

    fn mark_watching_cover_refresh(&mut self) {
        self.watching.cover_revision = self.watching.cover_revision.wrapping_add(1);
    }

    fn schedule_watching_cover_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.profile.as_ref().map(|user| user.id) else {
            return;
        };
        let scope = (self.generation, account);
        if self.watching.cover_scheduled == Some(scope)
            || self.watching.cover_revision == self.watching.cover_reconciled
            || self.route != Route::Now
        {
            return;
        }
        self.watching.cover_scheduled = Some(scope);
        let generation = self.generation;
        cx.on_next_frame(window, move |_, window, cx| {
            cx.on_next_frame(window, move |this, window, cx| {
                this.finish_watching_cover_refresh(generation, account, true, window, cx);
            });
        });
    }

    fn finish_watching_cover_refresh(
        &mut self,
        generation: u64,
        account: i64,
        retry: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.watching.cover_scheduled != Some((generation, account)) {
            return;
        }
        self.watching.cover_scheduled = None;
        if self.route != Route::Now
            || self.generation != generation
            || self.profile.as_ref().map(|user| user.id) != Some(account)
        {
            self.schedule_watching_cover_refresh(window, cx);
            return;
        }
        let before_visible = self.watching.cover_visible.clone();
        let before_images: HashMap<_, _> = before_visible
            .iter()
            .filter_map(|key| {
                self.cover_images
                    .get(key)
                    .map(|image| (*key, image.clone()))
            })
            .collect();
        if let Some(visible) = self.prefetch_visible_watching_covers(window.scale_factor()) {
            self.watching.cover_bounds = Some(self.watching.list_widget.viewport_bounds());
            self.watching.cover_reconciled = self.watching.cover_revision;
            let paint_changed = visible != before_visible
                || visible.iter().any(|key| {
                    match (before_images.get(key), self.cover_images.get(key)) {
                        (Some(before), Some(after)) => !Arc::ptr_eq(before, after),
                        (None, None) => false,
                        _ => true,
                    }
                });
            self.watching.cover_visible = visible;
            if paint_changed {
                cx.notify();
            }
        } else if retry {
            self.watching.cover_scheduled = Some((generation, account));
            cx.on_next_frame(window, move |this, window, cx| {
                this.finish_watching_cover_refresh(generation, account, false, window, cx);
            });
        }
    }

    fn prefetch_visible_watching_covers(&mut self, scale: f32) -> Option<HashSet<CoverKey>> {
        let viewport = self.watching.list_widget.viewport_bounds();
        if viewport.size.height <= px(0.) {
            return None;
        }
        let count = self.watching.current.len();
        let mut start = self
            .watching
            .list_widget
            .logical_scroll_top()
            .item_ix
            .max(1);
        if start > count {
            self.replace_cover_region(CoverOwner::Route(Route::Now), vec![], vec![]);
            return Some(HashSet::new());
        }
        while start <= count {
            let Some(bounds) = self.watching.list_widget.bounds_for_item(start) else {
                if start == 1
                    && self
                        .watching
                        .list_widget
                        .bounds_for_item(0)
                        .is_some_and(|header| header.bottom() >= viewport.bottom())
                {
                    self.replace_cover_region(CoverOwner::Route(Route::Now), vec![], vec![]);
                    return Some(HashSet::new());
                }
                return None;
            };
            if bounds.bottom() > viewport.top() {
                break;
            }
            start += 1;
        }
        if start > count {
            self.replace_cover_region(CoverOwner::Route(Route::Now), vec![], vec![]);
            return Some(HashSet::new());
        }
        let mut end = start;
        while end <= count {
            let Some(bounds) = self.watching.list_widget.bounds_for_item(end) else {
                if end > start
                    && self
                        .watching
                        .list_widget
                        .bounds_for_item(end - 1)
                        .is_some_and(|prior| prior.bottom() >= viewport.bottom())
                {
                    break;
                }
                return None;
            };
            if bounds.top() >= viewport.bottom() {
                break;
            }
            end += 1;
        }
        let visible = self.watching.current[start - 1..end - 1]
            .iter()
            .filter(|entry| {
                entry
                    .media
                    .as_ref()
                    .and_then(|media| media.cover_medium.as_ref())
                    .is_some()
            })
            .map(|entry| (entry.media_id, 40, 56))
            .collect();
        if start < end {
            self.prefetch_watching_covers(start - 1..end - 1, scale);
        } else {
            self.replace_cover_region(CoverOwner::Route(Route::Now), vec![], vec![]);
        }
        Some(visible)
    }

    pub(super) fn refresh_playback_cover(&mut self) {
        let playback = self
            .now_playing
            .as_ref()
            .filter(|playing| playing.active)
            .and_then(|playing| playing.media_id)
            .and_then(|id| self.watching.playback(Some(id)));
        if let Some(entry) = playback
            && let Some(url) = entry
                .media
                .as_ref()
                .and_then(|media| media.cover_large.as_ref().or(media.cover_medium.as_ref()))
                .cloned()
        {
            self.replace_cover_region(
                CoverOwner::Playback,
                vec![((entry.media_id, 96, 140), url)],
                vec![],
            );
        } else if self.cover_regions.contains_key(&CoverOwner::Playback) {
            self.cover_regions.remove(&CoverOwner::Playback);
            self.retain_cover_route();
        }
    }

    fn prefetch_watching_covers(&mut self, range: std::ops::Range<usize>, scale: f32) {
        self.ensure_cover_scale(scale);
        let start = range.start.saturating_sub(6);
        let end = range
            .end
            .saturating_add(10)
            .min(self.watching.current.len());
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(end)..end {
            let entry = &self.watching.current[index];
            if let Some(url) = entry
                .media
                .as_ref()
                .and_then(|media| media.cover_medium.as_ref())
            {
                let cover = ((entry.media_id, 40, 56), url.clone());
                if range.contains(&index) {
                    visible.push(cover);
                } else {
                    nearby.push(cover);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Now), visible, nearby);
    }

    fn watching_progress(&self, entry: &ListEntry) -> i64 {
        self.progress
            .edit(self.generation, entry.media_id)
            .map_or_else(
                || {
                    self.list
                        .entry(entry.media_id)
                        .map_or(entry.progress, |newer| newer.progress)
                },
                |edit| edit.pending,
            )
    }

    fn watching_cover_layout_key(&self) -> u64 {
        let mut key = DefaultHasher::new();
        self.list.revision().hash(&mut key);
        self.watching.current.len().hash(&mut key);
        self.watching.loaded.hash(&mut key);
        self.watching.loading.hash(&mut key);
        self.watching.error.hash(&mut key);
        self.watching.update_error.hash(&mut key);
        self.watching.updating.hash(&mut key);
        self.list.step_error.hash(&mut key);
        self.cover_scale.to_bits().hash(&mut key);
        if let Some(playing) = self.now_playing.as_ref() {
            playing.active.hash(&mut key);
            playing.media_id.hash(&mut key);
            playing.episode.hash(&mut key);
            playing.matched.hash(&mut key);
            playing.player.hash(&mut key);
            (playing.length_us > 0).hash(&mut key);
            if let Some(entry) = self.watching.playback(playing.media_id) {
                let progress = self.watching_progress(entry);
                progress.hash(&mut key);
                next_episode_file(&self.library_files, entry.media_id, progress)
                    .and_then(|file| file.episode)
                    .hash(&mut key);
            }
        }
        self.watching
            .playback_entry
            .as_ref()
            .map(|entry| (entry.media_id, entry.progress))
            .hash(&mut key);
        let first = self
            .watching
            .list_widget
            .logical_scroll_top()
            .item_ix
            .saturating_sub(1);
        let viewport = self.watching.list_widget.viewport_bounds();
        for (offset, entry) in self.watching.current.iter().skip(first).enumerate() {
            if offset >= 36
                && self
                    .watching
                    .list_widget
                    .bounds_for_item(WatchingSlot::row_ix(first + offset))
                    .is_none_or(|bounds| bounds.top() >= viewport.bottom())
            {
                break;
            }
            entry.media_id.hash(&mut key);
            let progress = self.watching_progress(entry);
            progress.hash(&mut key);
            next_episode_file(&self.library_files, entry.media_id, progress)
                .and_then(|file| file.episode)
                .hash(&mut key);
            if let Some(media) = entry.media.as_ref() {
                media.display_title().hash(&mut key);
            }
            self.progress
                .edit(self.generation, entry.media_id)
                .and_then(|edit| edit.error.as_ref())
                .hash(&mut key);
        }
        key.finish()
    }

    pub(super) fn refresh_watching_list(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_some() {
            self.load_watching(window, cx);
        }
    }

    pub(super) fn refresh_watching_playback(&mut self, window: &Window, cx: &mut Context<Self>) {
        let id = self
            .now_playing
            .as_ref()
            .and_then(|playing| playing.media_id);
        let request = self.watching.begin_playback(id);
        self.refresh_playback_cover();
        let Some(request) = request else {
            return;
        };
        let Some(id) = id else {
            self.cancel_reads("watching-entry");
            cx.notify();
            return;
        };
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_entry(id).await });
        self.replace_reads("watching-entry", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.watching.playback_request != request
                    || this.watching.playback_id != Some(id)
                {
                    return;
                }
                this.watching.playback_loading = false;
                this.watching.playback_checked = true;
                this.watching.playback_entry = result.ok().flatten();
                this.ensure_cover_scale(window.scale_factor());
                this.refresh_watching_covers();
                cx.notify();
            });
        })
        .detach();
    }

    fn load_watching(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.watching.request += 1;
        let request = self.watching.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let captured = WatchingRead {
            generation,
            account,
            request,
        };
        self.watching.loading = true;
        self.watching.error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.local_entries().await });
        self.replace_reads("watching-list", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                let current = WatchingRead {
                    generation: this.generation,
                    account: this.profile.as_ref().map(|user| user.id),
                    request: this.watching.request,
                };
                if !this.watching.accept_list(captured, current, result) {
                    return;
                }
                this.ensure_cover_scale(window.scale_factor());
                this.refresh_watching_covers();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn step_watching(&mut self, id: i64, delta: i64, window: &Window, cx: &mut Context<Self>) {
        if self.list.entry(id).is_none()
            && let Some(entry) = self
                .watching
                .current
                .iter()
                .find(|entry| entry.media_id == id)
                .cloned()
        {
            let change = self.list.apply_one(entry, now_seconds());
            self.apply_list_change(change);
        }
        self.step_progress(id, delta, window, cx);
    }

    fn update_detected_episode(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(playing) = self.now_playing.as_ref() else {
            return;
        };
        let (Some(id), Some(episode)) = (playing.media_id, playing.episode) else {
            return;
        };
        if self.watching.updating
            || self.profile.is_none()
            || self.barrier.is_some()
            || self.auth_busy
            || self.update_busy
        {
            return;
        }
        if self
            .progress
            .edit(self.generation, id)
            .is_some_and(|edit| edit.dirty() || edit.in_flight)
        {
            self.watching.update_error = "Finish the pending episode change first.".into();
            cx.notify();
            return;
        }
        self.watching.update_request += 1;
        let request = self.watching.update_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.watching.updating = true;
        self.watching.update_error.clear();
        self.active_writes += 1;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    let Some(fresh) = worker.get_entry(id).await? else {
                        return Ok(None);
                    };
                    if fresh.progress >= episode {
                        Ok(Some(fresh))
                    } else {
                        worker
                            .set_progress(id, episode, Some(fresh.progress))
                            .await
                            .map(Some)
                    }
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.watching.update_request != request
                {
                    this.check_barrier(window, cx);
                    return;
                }
                this.watching.updating = false;
                match result {
                    Ok(Some(entry)) => {
                        if this
                            .watching
                            .accept_playback_update(id, Some(entry.clone()))
                        {
                            this.cancel_reads("watching-entry");
                        }
                        let change = this.list.apply_one(entry.clone(), now_seconds());
                        this.apply_list_change(change);
                        if let Some(current) = this
                            .watching
                            .current
                            .iter_mut()
                            .find(|current| current.media_id == id)
                        {
                            *current = entry;
                        }
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Ok(None) => {
                        if this.watching.accept_playback_update(id, None) {
                            this.cancel_reads("watching-entry");
                        }
                        this.watching.remove_row(id);
                        this.watching.update_error = "This show is no longer on your list.".into();
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Err(error) => this.watching.update_error = error,
                }
                this.refresh_watching_covers();
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_watching_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) <= 960.;
        let mut page = div()
            .id("watching-header")
            .debug_selector(|| "watching-header".into())
            .w_full()
            .px(px(if compact { 20. } else { 28. }))
            .pt(px(20.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(route_header(
                "Now playing",
                "Currently Watching",
                "Playback and your current shows",
                None,
                compact,
                8.,
            ));
        let playing = self.now_playing.clone();
        if let Some(playing) = playing.filter(|playing| playing.active) {
            let generation = self.generation;
            let account = self.profile.as_ref().map(|user| user.id);
            let entry = self.watching.playback(playing.media_id);
            let progress = entry.map(|entry| self.watching_progress(entry));
            let can_update = playing
                .episode
                .zip(progress)
                .is_some_and(|(episode, progress)| episode > progress);
            let next = playing
                .media_id
                .zip(progress)
                .and_then(|(id, progress)| next_episode_file(&self.library_files, id, progress))
                .cloned();
            let cover = playing
                .media_id
                .and_then(|id| self.cover_images.get(&(id, 96, 140)).cloned());
            let mut controls = div().flex().flex_wrap().gap(px(7.));
            if let Some(file) = next {
                let path = file.path.clone();
                controls = controls.child(
                    button(
                        "watching-play-next",
                        format!("▶ Play Ep {}", file.episode.unwrap_or(0)),
                        false,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        open_local(this, &path, generation, account, cx)
                    })),
                );
            }
            if can_update && playing.matched.is_some() {
                let disabled = self.watching.updating
                    || self.barrier.is_some()
                    || self.auth_busy
                    || self.update_busy;
                controls = controls.child(
                    button(
                        "watching-update",
                        if self.watching.updating {
                            "Updating…".into()
                        } else {
                            format!("Update to Ep {}", playing.episode.unwrap_or(0))
                        },
                        disabled,
                    )
                    .bg(rgb(ACCENT))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.update_detected_episode(window, cx);
                        }
                    })),
                );
            } else if let (Some(episode), Some(_)) = (playing.episode, progress) {
                controls = controls.child(
                    div()
                        .text_color(rgb(DIM))
                        .child(format!("Already watched Ep {episode}")),
                );
            }
            let id = playing.media_id;
            let cover = if let Some(id) = id {
                div()
                    .id("watching-playback-cover")
                    .debug_selector(|| "watching-playback-cover".into())
                    .role(gpui::Role::Button)
                    .aria_label("Open details")
                    .focusable()
                    .tab_stop(true)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.navigate(Route::Anime(id), window, cx);
                        }
                    }))
                    .child(cover_or_empty(cover, 96., 140.))
                    .into_any()
            } else {
                cover_or_empty(cover, 96., 140.)
            };
            let mut card = div()
                .id("watching-active-card")
                .debug_selector(|| "watching-active-card".into())
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .rounded(px(2.))
                .p(px(14.))
                .flex()
                .items_start()
                .gap(px(14.))
                .child(cover);
            let mut details = div().flex_1().min_w_0().flex().flex_col().gap(px(7.));
            if let (Some(id), Some(matched)) = (id, playing.matched.as_ref()) {
                details = details.child(
                    button("watching-details", matched.clone(), false)
                        .debug_selector(|| "watching-playback-title".into())
                        .bg(rgb(PANEL))
                        .border_color(rgb(PANEL))
                        .px(px(0.))
                        .py(px(0.))
                        .text_size(px(18.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if scope_matches(this, generation, account) {
                                this.navigate(Route::Anime(id), window, cx);
                            }
                        })),
                );
            } else {
                details = details.child(heading("Not on your list"));
            }
            if let Some(progress) = progress {
                details = details.child(
                    div()
                        .text_color(rgb(DIM))
                        .text_size(px(12.))
                        .child(format!("Ep {progress} on your list")),
                );
            }
            if let Some(episode) = playing.episode {
                details = details.child(div().child(format!("Detected Episode {episode}")));
            }
            if playing.length_us > 0 {
                let pct = (playing.position_us as f32 / playing.length_us as f32).clamp(0., 1.);
                details = details.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .h(px(6.))
                                .bg(rgb(PANEL_2))
                                .child(div().w(gpui::relative(pct)).h_full().bg(rgb(ACCENT))),
                        )
                        .child(div().text_color(rgb(DIM)).text_size(px(11.)).child(format!(
                            "{} / {}",
                            time(playing.position_us),
                            time(playing.length_us)
                        ))),
                );
            }
            details = details
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .text_size(px(11.))
                        .child(format!("Playing in {}", playing.player)),
                )
                .child(controls);
            if !self.watching.update_error.is_empty() {
                details = details.child(
                    div()
                        .text_color(rgb(ERROR))
                        .child(format!("Update failed: {}", self.watching.update_error)),
                );
            }
            card = card.child(details);
            page = page.child(card);
        } else {
            page = page.child(
                div()
                    .id("watching-idle-card")
                    .debug_selector(|| "watching-idle-card".into())
                    .h(px(111.))
                    .mb(px(-4.))
                    .border_1()
                    .border_dashed()
                    .border_color(rgb(EDGE))
                    .rounded(px(4.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(7.))
                    .child(
                        div()
                            .text_color(rgb(DIM))
                            .child("Nothing playing right now."),
                    )
                    .child(
                        div()
                            .text_color(rgb(DIM))
                            .text_size(px(13.))
                            .child("Play an anime in MPV, VLC, or Haruna and it'll show up here."),
                    ),
            );
        }
        if !self.list.step_error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!("Episode update failed: {}", self.list.step_error)),
            );
        }
        if !self.watching.error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(self.watching.error.clone())
                    .child(button("watching-retry", "Retry", false).on_click(
                        cx.listener(|this, _, window, cx| this.load_watching(window, cx)),
                    )),
            );
        }
        page = page.child(
            div()
                .id("watching-continue")
                .debug_selector(|| "watching-continue".into())
                .flex()
                .items_center()
                .gap(px(12.))
                .child(heading("Continue watching"))
                .child(div().flex_1().h(px(1.)).bg(rgb(EDGE)).mt(px(6.))),
        );
        if self.watching.loading && !self.watching.loaded {
            page = page.child(div().text_color(rgb(DIM)).child("Loading…"));
        } else if self.watching.current.is_empty() && self.watching.loaded {
            page = page.child(
                div()
                    .text_color(rgb(DIM))
                    .child("No shows currently in progress."),
            );
        }
        if !self.watching.current.is_empty() {
            page = page.pb(px(8.));
        }
        page.into_any()
    }

    pub(super) fn render_watching(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) <= 960.;
        let rows = self.watching.current.len();
        self.watching.list_widget.remeasure_items(0..1);
        let window_size = window.viewport_size();
        let bounds = self.watching.list_widget.viewport_bounds();
        let layout_key = self.watching_cover_layout_key();
        if self.watching.cover_window_size != Some(window_size)
            || self.watching.cover_layout_key != Some(layout_key)
            || self
                .watching
                .cover_bounds
                .is_some_and(|settled| bounds.size.height > px(0.) && settled != bounds)
        {
            self.watching.cover_window_size = Some(window_size);
            self.watching.cover_layout_key = Some(layout_key);
            self.mark_watching_cover_refresh();
        }
        self.schedule_watching_cover_refresh(window, cx);
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
                    self.watching.list_widget.clone(),
                    move |item_ix, window, app| match WatchingSlot::at(item_ix, rows) {
                        WatchingSlot::Header => weak
                            .update(app, |this, cx| this.render_watching_header(window, cx))
                            .unwrap_or_else(|_| div().into_any()),
                        WatchingSlot::Footer => div()
                            .id("watching-footer")
                            .debug_selector(|| "watching-footer".into())
                            .w_full()
                            .h(px(20.))
                            .into_any(),
                        WatchingSlot::Row(index) => {
                            let data = weak.update(app, |this, _| {
                                this.watching.current.get(index).map(|entry| {
                                    let current = this
                                        .list
                                        .entry(entry.media_id)
                                        .cloned()
                                        .unwrap_or_else(|| entry.clone());
                                    let cover =
                                        this.cover_images.get(&(entry.media_id, 40, 56)).cloned();
                                    let pending = this
                                        .progress
                                        .edit(this.generation, entry.media_id)
                                        .cloned();
                                    let progress = this.watching_progress(&current);
                                    let next = next_episode_file(
                                        &this.library_files,
                                        entry.media_id,
                                        progress,
                                    )
                                    .cloned();
                                    let score_format = this
                                        .profile
                                        .as_ref()
                                        .and_then(|user| user.score_format.clone());
                                    let generation = this.generation;
                                    let account = this.profile.as_ref().map(|user| user.id);
                                    (
                                        current,
                                        pending,
                                        cover,
                                        next,
                                        score_format,
                                        generation,
                                        account,
                                    )
                                })
                            });
                            let Ok(Some((
                                entry,
                                pending,
                                cover,
                                next,
                                score_format,
                                generation,
                                account,
                            ))) = data
                            else {
                                return div().into_any();
                            };
                            div()
                                .w_full()
                                .px(px(if compact { 20. } else { 28. }))
                                .pb(px(if index + 1 < rows { 8. } else { 0. }))
                                .child(watching_row(
                                    entry,
                                    pending,
                                    cover,
                                    next,
                                    score_format.as_deref(),
                                    weak.clone(),
                                    (generation, account),
                                ))
                                .into_any()
                        }
                    },
                )
                .size_full(),
            )
            .child(list_scrollbar(
                "watching-scrollbar",
                "Currently Watching scroll position",
                self.watching.list_widget.clone(),
                move |first, window, app| {
                    let _ = scroll_weak.update(app, |this, cx| {
                        if scope_matches(this, generation, account) {
                            this.prefetch_watching_covers(
                                WatchingSlot::row_range(
                                    first..first.saturating_add(36),
                                    this.watching.current.len(),
                                ),
                                window.scale_factor(),
                            );
                            this.mark_watching_cover_refresh();
                            this.schedule_watching_cover_refresh(window, cx);
                            cx.notify();
                        }
                    });
                },
            ))
            .into_any()
    }
}

fn title(entry: &ListEntry) -> String {
    entry
        .media
        .as_ref()
        .map(|media| media.display_title())
        .unwrap_or_else(|| format!("#{}", entry.media_id))
}
fn next_episode_file(files: &LibraryFiles, media_id: i64, progress: i64) -> Option<&LibraryFile> {
    files.episode(media_id, progress.checked_add(1)?)
}
fn time(us: i64) -> String {
    let seconds = (us / 1_000_000).max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn cover_or_empty(cover: Option<Arc<RenderImage>>, width: f32, height: f32) -> AnyElement {
    if let Some(image) = cover {
        img(image)
            .w(px(width))
            .h(px(height))
            .object_fit(gpui::ObjectFit::Cover)
            .into_any()
    } else {
        let mut empty = div()
            .w(px(width))
            .h(px(height))
            .bg(rgb(PANEL_2))
            .flex()
            .items_center()
            .justify_center();
        if width > 40. {
            empty = empty
                .text_color(rgb(DIM))
                .text_size(px(12.))
                .child("No cover");
        }
        empty.into_any()
    }
}
fn scope_matches(this: &Workspace, generation: u64, account: Option<i64>) -> bool {
    this.generation == generation && this.profile.as_ref().map(|user| user.id) == account
}
fn open_local(
    this: &mut Workspace,
    path: &str,
    generation: u64,
    account: Option<i64>,
    cx: &mut Context<Workspace>,
) {
    if !scope_matches(this, generation, account) {
        return;
    }
    if let Err(error) = crate::platform::NativePlatform.open_path(std::path::Path::new(path)) {
        this.watching.update_error = error;
        cx.notify();
    }
}
fn watching_row(
    entry: ListEntry,
    pending: Option<super::state::ProgressEdit>,
    cover: Option<Arc<RenderImage>>,
    next: Option<kurisu_core::models::LibraryFile>,
    score_format: Option<&str>,
    weak: WeakEntity<Workspace>,
    scope: (u64, Option<i64>),
) -> AnyElement {
    let (generation, account) = scope;
    let id = entry.media_id;
    let total = entry.media.as_ref().and_then(|media| media.episodes);
    let title = title(&entry);
    let progress = pending.as_ref().map_or(entry.progress, |edit| edit.pending);
    let score = entry
        .score
        .filter(|score| *score > 0.)
        .map(|score| match score_format {
            Some("POINT_3") => ["", "😞", "😐", "😊"]
                .get(score.round() as usize)
                .map(|label| (*label).to_string())
                .unwrap_or_else(|| score.to_string()),
            Some("POINT_5") => "★".repeat(score.round().clamp(0., 5.) as usize),
            _ => format!("★ {score}"),
        });
    let weak_cover = weak.clone();
    let weak_title = weak.clone();
    let cover = div()
        .id(format!("watching-cover-{id}"))
        .debug_selector(move || format!("watching-cover-{id}"))
        .role(gpui::Role::Button)
        .aria_label(format!("Open details for {title}"))
        .focusable()
        .tab_stop(true)
        .cursor_pointer()
        .flex_shrink_0()
        .on_click(move |_, window, app| {
            let _ = weak_cover.update(app, |this, cx| {
                if scope_matches(this, generation, account) {
                    this.navigate(Route::Anime(id), window, cx);
                }
            });
        })
        .child(cover_or_empty(cover, 40., 56.));
    let mut info = div()
        .id(format!("watching-title-{id}"))
        .debug_selector(move || format!("watching-title-{id}"))
        .role(gpui::Role::Button)
        .aria_label(format!("Open details for {title}"))
        .focusable()
        .tab_stop(true)
        .cursor_pointer()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(3.))
        .on_click(move |_, window, app| {
            let _ = weak_title.update(app, |this, cx| {
                if scope_matches(this, generation, account) {
                    this.navigate(Route::Anime(id), window, cx);
                }
            });
        })
        .child(div().text_color(rgb(INK)).whitespace_normal().child(title));
    if let Some(score) = score {
        info = info.child(div().text_color(rgb(DIM)).text_size(px(12.)).child(score));
    }
    if let Some(error) = pending.as_ref().and_then(|edit| edit.error.as_ref()) {
        info = info.child(
            div()
                .text_color(rgb(ERROR))
                .text_size(px(11.))
                .child(error.clone()),
        );
    }
    let mut row = div()
        .id(format!("watching-row-{id}"))
        .debug_selector(move || format!("watching-row-{id}"))
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.))
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(EDGE))
        .rounded(px(2.))
        .p(px(10.))
        .child(cover)
        .child(info);
    let saving = pending
        .as_ref()
        .is_some_and(|edit| edit.in_flight || edit.deleting || edit.requires_review);
    let at_min = progress <= 0;
    let at_max = progress >= total.unwrap_or(9999);
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
    let weak_keys = weak.clone();
    let weak_less = weak.clone();
    let weak_more = weak.clone();
    let stepper = div()
        .id(format!("watching-stepper-{id}"))
        .debug_selector(move || format!("watching-stepper-{id}"))
        .focusable()
        .tab_stop(true)
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(4.))
        .on_key_down(move |event: &gpui::KeyDownEvent, window, app| {
            let delta = match event.keystroke.key.as_str() {
                "left" | "down" | "arrowleft" | "arrowdown" => -1,
                "right" | "up" | "arrowright" | "arrowup" => 1,
                _ => return,
            };
            app.stop_propagation();
            if saving || (delta < 0 && at_min) || (delta > 0 && at_max) {
                return;
            }
            let _ = weak_keys.update(app, |this, cx| {
                if scope_matches(this, generation, account) {
                    this.step_watching(id, delta, window, cx);
                }
            });
        })
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
            button(format!("watching-less-{id}"), "−", at_min || saving)
                .debug_selector(move || format!("watching-less-{id}"))
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
                .opacity(if at_min || saving { 0.3 } else { 1. })
                .aria_label("One less episode")
                .on_click(move |_, window, app| {
                    if at_min || saving {
                        return;
                    }
                    let _ = weak_less.update(app, |this, cx| {
                        if scope_matches(this, generation, account) {
                            this.step_watching(id, -1, window, cx);
                        }
                    });
                }),
        )
        .child(progress_value)
        .child(
            button(format!("watching-more-{id}"), "+", at_max || saving)
                .debug_selector(move || format!("watching-more-{id}"))
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
                .opacity(if at_max || saving { 0.3 } else { 1. })
                .aria_label("One more episode")
                .on_click(move |_, window, app| {
                    if at_max || saving {
                        return;
                    }
                    let _ = weak_more.update(app, |this, cx| {
                        if scope_matches(this, generation, account) {
                            this.step_watching(id, 1, window, cx);
                        }
                    });
                }),
        );
    row = row.child(stepper);
    if let Some(file) = next {
        let weak_play = weak.clone();
        let path = file.path;
        row = row.child(
            button(
                format!("watching-play-{id}"),
                format!("▶ Ep {}", file.episode.unwrap_or(0)),
                false,
            )
            .on_click(move |_, _, app| {
                let _ = weak_play.update(app, |this, cx| {
                    open_local(this, &path, generation, account, cx)
                });
            }),
        );
    }
    row.into_any()
}

#[cfg(test)]
mod tests {
    use super::super::state::EditorState;
    use super::*;
    use gpui::{Entity, Modifiers, Subscription, TestAppContext, VisualTestContext};
    use kurisu_core::models::{Media, NowPlaying, User};
    use kurisu_core::{Core, UpdateChannel};
    use std::path::{Path, PathBuf};
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
            "kurisu-native-watching-{}-{nonce}",
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

    fn playing(id: i64) -> NowPlaying {
        NowPlaying {
            active: true,
            player: "fixture".into(),
            title: format!("Synthetic title {id}"),
            matched: None,
            media_id: Some(id),
            episode: Some(1),
            length_us: 0,
            position_us: 0,
        }
    }

    fn playback_entry(id: i64, cover: Option<&str>) -> ListEntry {
        ListEntry {
            media_id: id,
            status: "CURRENT".into(),
            media: Some(Media {
                id,
                cover_large: cover.map(str::to_owned),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn watching_slots_and_refresh_keep_media_anchors() {
        assert_eq!(WatchingSlot::count(0), 2);
        assert_eq!(WatchingSlot::at(0, 3), WatchingSlot::Header);
        assert_eq!(WatchingSlot::at(1, 3), WatchingSlot::Row(0));
        assert_eq!(WatchingSlot::at(3, 3), WatchingSlot::Row(2));
        assert_eq!(WatchingSlot::at(4, 3), WatchingSlot::Footer);
        assert_eq!(WatchingSlot::row_range(0..5, 3), 0..3);
        assert_eq!(WatchingSlot::row_range(4..5, 3), 3..3);

        let mut state = WatchingState::default();
        let row = |id| ListEntry {
            media_id: id,
            status: "CURRENT".into(),
            ..Default::default()
        };
        state.replace_rows(vec![row(1), row(2), row(3)]);
        assert_eq!(state.list_widget.item_count(), 5);
        state.list_widget.scroll_to(ListOffset {
            item_ix: WatchingSlot::row_ix(1),
            offset_in_item: px(5.),
        });
        state.replace_rows(vec![row(3), row(1), row(2)]);
        assert_eq!(state.list_widget.logical_scroll_top().item_ix, 3);
        assert_eq!(
            state.list_widget.logical_scroll_top().offset_in_item,
            px(5.)
        );
        state.remove_row(1);
        assert_eq!(state.list_widget.item_count(), 4);
        assert_eq!(state.list_widget.logical_scroll_top().item_ix, 2);
    }

    #[gpui::test]
    fn watching_header_and_card_scroll_with_rows_and_keep_endpoint_inset(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.current = (1..=80)
                .map(|id| ListEntry {
                    media_id: id,
                    status: "CURRENT".into(),
                    progress: 1,
                    media: Some(Media {
                        id,
                        title_english: Some(format!("Synthetic show {id:03}")),
                        episodes: Some(24),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect();
            workspace.watching.loaded = true;
            workspace
                .watching
                .list_widget
                .reset(WatchingSlot::count(80));
            cx.notify();
        });
        for width in [800., 1100.] {
            for active in [false, true] {
                workspace.update_in(cx, |workspace, _, cx| {
                    workspace.watching.list_widget.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: px(0.),
                    });
                    workspace.now_playing = active.then(|| {
                        let mut current = playing(1);
                        current.matched = Some("Synthetic show 001".into());
                        current
                    });
                    workspace.watching.playback_entry = active.then(|| playback_entry(1, None));
                    cx.notify();
                });
                cx.simulate_resize(gpui::size(
                    px(width),
                    px(if width <= 800. { 600. } else { 750. }),
                ));
                cx.run_until_parked();
                assert!(cx.debug_bounds("discovery-heading").is_some());
                assert!(cx.debug_bounds("watching-continue").is_some());
                let card = if active {
                    "watching-active-card"
                } else {
                    "watching-idle-card"
                };
                assert!(cx.debug_bounds(card).is_some());
                workspace.read_with(cx, |workspace, _| {
                    assert_eq!(
                        workspace.watching.list_widget.item_count(),
                        WatchingSlot::count(80)
                    );
                    let header = workspace.watching.list_widget.bounds_for_item(0).unwrap();
                    let first = workspace
                        .watching
                        .list_widget
                        .bounds_for_item(WatchingSlot::row_ix(0))
                        .unwrap();
                    assert_eq!(header.bottom(), first.top());
                });
                if width == 800. && !active {
                    let before = workspace.read_with(cx, |workspace, _| {
                        workspace
                            .watching
                            .list_widget
                            .bounds_for_item(0)
                            .unwrap()
                            .size
                            .height
                    });
                    workspace.update_in(cx, |workspace, _, cx| {
                        workspace.list.step_error = "Synthetic episode failure".into();
                        cx.notify();
                    });
                    cx.run_until_parked();
                    workspace.read_with(cx, |workspace, _| {
                        let header = workspace.watching.list_widget.bounds_for_item(0).unwrap();
                        let first = workspace
                            .watching
                            .list_widget
                            .bounds_for_item(WatchingSlot::row_ix(0))
                            .unwrap();
                        assert!(header.size.height > before);
                        assert_eq!(header.bottom(), first.top());
                    });
                    workspace.update_in(cx, |workspace, _, cx| {
                        workspace.list.step_error.clear();
                        cx.notify();
                    });
                    cx.run_until_parked();
                }

                if width == 800. && !active {
                    for _ in 0..8 {
                        cx.simulate_event(gpui::ScrollEvent {
                            position: gpui::point(px(500.), px(500.)),
                            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-120.))),
                            ..Default::default()
                        });
                    }
                } else {
                    workspace.update_in(cx, |workspace, _, cx| {
                        workspace.watching.list_widget.scroll_by(px(650.));
                        cx.notify();
                    });
                }
                cx.run_until_parked();
                workspace.read_with(cx, |workspace, _| {
                    assert!(workspace.watching.list_widget.logical_scroll_top().item_ix > 0);
                    assert!(workspace.watching.list_widget.bounds_for_item(0).is_none());
                });
                assert!(cx.debug_bounds("discovery-heading").is_none());
                assert!(cx.debug_bounds(card).is_none());
                assert!(cx.debug_bounds("watching-continue").is_none());

                workspace.update_in(cx, |workspace, _, cx| {
                    workspace.watching.list_widget.scroll_to_end();
                    cx.notify();
                });
                cx.run_until_parked();
                let last = cx.debug_bounds("watching-row-80").unwrap();
                workspace.read_with(cx, |workspace, _| {
                    let footer = workspace
                        .watching
                        .list_widget
                        .bounds_for_item(WatchingSlot::footer_ix(80))
                        .unwrap();
                    let viewport = workspace.watching.list_widget.viewport_bounds();
                    assert_eq!(footer.size.height, px(20.));
                    assert_eq!(last.bottom(), footer.top());
                    assert_eq!(viewport.bottom() - last.bottom(), px(20.));
                });
            }
        }
    }

    #[gpui::test]
    fn watching_visible_row_remeasures_after_title_change(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.current = vec![ListEntry {
                media_id: 7,
                status: "CURRENT".into(),
                media: Some(Media {
                    id: 7,
                    title_english: Some("Short title".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }];
            workspace.watching.loaded = true;
            workspace.watching.list_widget.reset(WatchingSlot::count(1));
            cx.notify();
        });
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
        let before = workspace.read_with(cx, |workspace, _| {
            workspace
                .watching
                .list_widget
                .bounds_for_item(WatchingSlot::row_ix(0))
                .unwrap()
                .size
                .height
        });
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.watching.current[0]
                .media
                .as_mut()
                .unwrap()
                .title_english = Some("A much longer synthetic title ".repeat(30));
            cx.notify();
        });
        cx.run_until_parked();
        let after = workspace.read_with(cx, |workspace, _| {
            workspace
                .watching
                .list_widget
                .bounds_for_item(WatchingSlot::row_ix(0))
                .unwrap()
                .size
                .height
        });
        assert!(
            after > before,
            "row height stayed {before:?} after title change"
        );
    }

    #[test]
    fn watching_preserves_core_entry_order() {
        let mut state = WatchingState {
            request: 1,
            ..Default::default()
        };
        let current = WatchingRead {
            generation: 1,
            account: Some(47),
            request: 1,
        };
        let entries = [(7, "Zebra"), (8, "Apple")]
            .map(|(id, title)| ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                media: Some(Media {
                    id,
                    title_english: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .to_vec();
        assert!(state.accept_list(current, current, Ok(entries)));
        assert_eq!(
            state
                .current
                .iter()
                .map(|entry| entry.media_id)
                .collect::<Vec<_>>(),
            [7, 8]
        );
    }

    #[gpui::test]
    fn watching_row_keeps_compact_controls_and_detail_targets(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.current = vec![ListEntry {
                media_id: 7,
                status: "CURRENT".into(),
                progress: 1,
                score: Some(3.),
                media: Some(Media {
                    id: 7,
                    title_english: Some("A synthetic long title for the compact row".into()),
                    episodes: Some(24),
                    ..Default::default()
                }),
                ..Default::default()
            }];
            let mut second = workspace.watching.current[0].clone();
            second.media_id = 8;
            workspace.watching.current.push(second);
            workspace.watching.loaded = true;
            workspace.watching.list_widget.reset(WatchingSlot::count(2));
            cx.notify();
        });
        for width in [1100., 800.] {
            cx.simulate_resize(gpui::size(
                px(width),
                px(if width <= 800. { 600. } else { 750. }),
            ));
            cx.run_until_parked();
            cx.update(|window, app| window.draw(app).clear(app));
            let row = cx.debug_bounds("watching-row-7").unwrap();
            let cover = cx.debug_bounds("watching-cover-7").unwrap();
            let title = cx.debug_bounds("watching-title-7").unwrap();
            let less = cx.debug_bounds("watching-less-7").unwrap();
            let more = cx.debug_bounds("watching-more-7").unwrap();
            assert_eq!(cover.size.width, px(40.));
            assert_eq!(cover.size.height, px(56.));
            assert_eq!(less.size.width, px(24.));
            assert_eq!(more.size.width, px(24.));
            assert!(row.size.height >= px(76.) && row.size.height <= px(90.));
            assert!(title.right() < less.left());
            let next_row = cx.debug_bounds("watching-row-8").unwrap();
            assert_eq!(next_row.top() - row.bottom(), px(8.));
        }
        let more = cx.debug_bounds("watching-more-7").unwrap();
        cx.simulate_click(more.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| {
                workspace
                    .progress
                    .edit(workspace.generation, 7)
                    .unwrap()
                    .pending
            }),
            2
        );
        let title = cx.debug_bounds("watching-title-7").unwrap();
        cx.simulate_click(title.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.route.clone()),
            Route::Anime(7)
        );
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.route = Route::Now;
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        let cover = cx.debug_bounds("watching-cover-7").unwrap();
        cx.simulate_click(cover.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.route.clone()),
            Route::Anime(7)
        );
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.route = Route::Now;
            let mut current = playing(7);
            current.matched = Some("A synthetic title".into());
            workspace.now_playing = Some(current);
            workspace.watching.playback_entry = Some(playback_entry(7, None));
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        let playback_title = cx.debug_bounds("watching-playback-title").unwrap();
        let playback_cover = cx.debug_bounds("watching-playback-cover").unwrap();
        assert_eq!(playback_cover.size.width, px(96.));
        assert_eq!(playback_cover.size.height, px(140.));
        cx.simulate_click(playback_title.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.route.clone()),
            Route::Anime(7)
        );
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.route = Route::Now;
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        let playback_cover = cx.debug_bounds("watching-playback-cover").unwrap();
        cx.simulate_click(playback_cover.center(), Modifiers::default());
        assert_eq!(
            workspace.read_with(cx, |workspace, _| workspace.route.clone()),
            Route::Anime(7)
        );
    }

    #[gpui::test]
    fn playback_covers_keep_one_owner_and_retire_on_idle(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            let mut prior_image = None;
            for id in 1..=400 {
                workspace.now_playing = Some(playing(id));
                workspace.watching.playback_entry = Some(playback_entry(id, Some("invalid-url")));
                workspace.refresh_watching_covers();
                let key = (id, 96, 140);
                assert!(workspace.cover_regions.contains_key(&CoverOwner::Playback));
                assert_eq!(workspace.cover_tickets.len(), 1);
                assert!(workspace.cover_tickets.contains_key(&key));
                assert!(
                    prior_image
                        .take()
                        .is_none_or(|image: std::sync::Weak<RenderImage>| image
                            .upgrade()
                            .is_none())
                );
                let image = Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::new(1, 1),
                )]));
                prior_image = Some(Arc::downgrade(&image));
                workspace.cover_images.insert(key, image);
                assert_eq!(workspace.cover_images.len(), 1);
            }

            workspace.now_playing = Some(playing(401));
            workspace.watching.playback_entry = Some(playback_entry(401, None));
            workspace.refresh_playback_cover();
            assert!(!workspace.cover_regions.contains_key(&CoverOwner::Playback));
            assert!(workspace.cover_tickets.is_empty());
            assert!(workspace.cover_images.is_empty());
            assert!(prior_image.unwrap().upgrade().is_none());

            workspace.now_playing = Some(playing(402));
            workspace.watching.playback_entry = Some(playback_entry(402, Some("invalid-url")));
            workspace.refresh_playback_cover();
            assert!(workspace.cover_regions.contains_key(&CoverOwner::Playback));
            workspace.now_playing = None;
            workspace.refresh_watching_playback(window, cx);
            assert!(!workspace.cover_regions.contains_key(&CoverOwner::Playback));
            assert!(workspace.cover_tickets.is_empty());
        });
    }

    #[gpui::test]
    fn scale_invalidation_requeues_open_editor_covers(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, _| {
            workspace.editor = Some(EditorState::new(ListEntry {
                media_id: 9,
                media: Some(Media {
                    id: 9,
                    cover_medium: Some("invalid-url".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }));
            workspace.editor_recs = vec![Media {
                id: 10,
                cover_medium: Some("invalid-url".into()),
                ..Default::default()
            }];
            let next_scale = workspace.cover_scale + 0.5;
            workspace.ensure_cover_scale(next_scale);
            let editor_key = (9, 48, 64);
            let rec_key = (10, 96, 128);
            assert!(workspace.cover_regions.contains_key(&CoverOwner::Editor));
            assert_eq!(workspace.cover_tickets.len(), 2);
            assert!(workspace.cover_tickets.contains_key(&editor_key));
            assert!(workspace.cover_tickets.contains_key(&rec_key));
            let first_ticket = workspace.cover_tickets[&editor_key].clone();
            workspace.ensure_cover_scale(next_scale);
            assert_eq!(workspace.cover_tickets[&editor_key], first_ticket);
            workspace.ensure_cover_scale(next_scale + 0.5);
            assert_eq!(workspace.cover_tickets.len(), 2);
            assert_ne!(workspace.cover_tickets[&editor_key], first_ticket);
        });
    }

    #[test]
    fn late_list_response_cannot_replace_current_account_rows() {
        let mut state = WatchingState {
            request: 2,
            loading: true,
            ..Default::default()
        };
        let current = WatchingRead {
            generation: 9,
            account: Some(42),
            request: 2,
        };
        let old = WatchingRead {
            generation: 8,
            account: Some(17),
            request: 1,
        };
        let entries = vec![
            ListEntry {
                media_id: 2,
                status: "CURRENT".into(),
                ..Default::default()
            },
            ListEntry {
                media_id: 3,
                status: "COMPLETED".into(),
                ..Default::default()
            },
            ListEntry {
                media_id: 1,
                status: "CURRENT".into(),
                ..Default::default()
            },
        ];
        assert!(!state.accept_list(old, current, Ok(entries.clone())));
        assert!(state.loading && state.current.is_empty());
        assert!(state.accept_list(current, current, Ok(entries)));
        assert_eq!(
            state
                .current
                .iter()
                .map(|entry| entry.media_id)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(state.loaded && !state.loading);
        let superseded = WatchingRead {
            request: 3,
            ..current
        };
        state.request = 3;
        assert!(!state.accept_list(current, superseded, Err("late error".into())));
        assert!(state.error.is_empty());
    }

    #[test]
    fn next_playback_uses_list_progress_and_exact_media() {
        let mut files = LibraryFiles::default();
        files.replace(vec![
            LibraryFile {
                path: "/controlled/other2.mkv".into(),
                media_id: Some(7),
                episode: Some(2),
                ..Default::default()
            },
            LibraryFile {
                path: "/controlled/own1.mkv".into(),
                media_id: Some(8),
                episode: Some(1),
                ..Default::default()
            },
            LibraryFile {
                path: "/controlled/own2.mkv".into(),
                media_id: Some(8),
                episode: Some(2),
                ..Default::default()
            },
        ]);
        assert_eq!(
            next_episode_file(&files, 8, 1).map(|file| file.path.as_str()),
            Some("/controlled/own2.mkv")
        );
        assert!(next_episode_file(&files, 8, 2).is_none());
        assert_eq!(time(125_000_000), "2:05");
    }

    #[test]
    fn next_playback_rejects_progress_overflow() {
        let mut files = LibraryFiles::default();
        files.replace(vec![LibraryFile {
            media_id: Some(8),
            episode: Some(i64::MIN),
            ..Default::default()
        }]);
        assert!(next_episode_file(&files, 8, i64::MAX).is_none());
    }

    #[gpui::test]
    fn watching_scale_change_keeps_the_playback_cover(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, _, _| {
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            w.route = Route::Now;
            w.now_playing = Some(playing(7));
            w.watching.playback_entry = Some(playback_entry(7, Some("invalid-playback")));
            w.watching.replace_rows(cover_rows(20));
            w.refresh_watching_covers();
            let old = w.cover_tickets[&(7, 96, 140)].clone();
            w.prefetch_watching_covers(0..3, w.cover_scale * 2.);
            assert!(w.cover_regions.contains_key(&CoverOwner::Playback));
            assert_ne!(w.cover_tickets[&(7, 96, 140)], old);
        });
    }

    #[gpui::test]
    fn watching_episode_index_follows_library_scans_and_account_changes(cx: &mut TestAppContext) {
        let (core, runtime, dir) = fixture();
        drop(core);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        for id in [7, 8] {
            db.upsert_media(&Media {
                id,
                title_english: Some(format!("Synthetic Show {id}")),
                episodes: Some(24),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let core = Core::from_db(
            db,
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        );
        let folder = dir.0.join("episodes");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("ep13.mkv");
        std::fs::write(&path, []).unwrap();
        let folder = folder.to_str().unwrap().to_owned();
        let path = path.to_str().unwrap().to_owned();
        runtime
            .block_on(core.add_library_folder(folder.clone()))
            .unwrap();
        runtime
            .block_on(core.bind_library_path(path.clone(), 7, Some(-12)))
            .unwrap();
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
        workspace.update_in(cx, |workspace, _, _| {
            workspace.auth_intent += 1;
            workspace.restoring_session = false;
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
        });
        let refresh = |cx: &mut VisualTestContext, expected: &[(Option<i64>, Option<i64>)]| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.refresh_library_entries(window, cx);
                workspace.ensure_library_fresh(window, cx);
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                runtime.block_on(async {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await
                });
                cx.run_until_parked();
                if workspace.read_with(cx, |workspace, _| {
                    workspace
                        .library_files
                        .iter()
                        .map(|file| (file.media_id, file.episode))
                        .collect::<Vec<_>>()
                        == expected
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "Library scan must update Watching"
                );
            }
            workspace.read_with(cx, |workspace, _| {
                for id in [7, 8] {
                    for progress in 0..=13 {
                        let expected = workspace.library_files.iter().find(|file| {
                            file.media_id == Some(id) && file.episode == Some(progress + 1)
                        });
                        assert_eq!(
                            next_episode_file(&workspace.library_files, id, progress)
                                .map(|file| &file.path),
                            expected.map(|file| &file.path)
                        );
                    }
                }
            });
        };
        refresh(cx, &[(Some(7), Some(1))]);
        runtime
            .block_on(core.bind_library_path(path.clone(), 8, Some(-12)))
            .unwrap();
        refresh(cx, &[(Some(8), Some(1))]);
        runtime
            .block_on(core.bind_library_path(path.clone(), 8, Some(-11)))
            .unwrap();
        refresh(cx, &[(Some(8), Some(2))]);
        runtime.block_on(core.unbind_library_media(8)).unwrap();
        refresh(cx, &[(None, None)]);
        runtime
            .block_on(core.bind_library_path(path.clone(), 8, Some(-11)))
            .unwrap();
        refresh(cx, &[(Some(8), Some(2))]);
        std::fs::remove_file(&path).unwrap();
        refresh(cx, &[]);
        std::fs::write(&path, []).unwrap();
        refresh(cx, &[(Some(8), Some(2))]);
        runtime
            .block_on(core.remove_library_folder(folder.clone()))
            .unwrap();
        refresh(cx, &[]);
        runtime.block_on(core.add_library_folder(folder)).unwrap();
        refresh(cx, &[(Some(8), Some(2))]);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_profile(
                Some(User {
                    id: 48,
                    ..Default::default()
                }),
                window,
                cx,
            );
            assert_eq!(workspace.library_files.iter().len(), 0);
            assert!(next_episode_file(&workspace.library_files, 8, 1).is_none());
        });
    }

    #[test]
    fn playback_entry_retries_on_reentry_after_missing_result() {
        let mut state = WatchingState::default();
        assert_eq!(state.begin_playback(Some(8)), Some(1));
        assert_eq!(state.begin_playback(Some(8)), None);
        state.playback_loading = false;
        state.playback_checked = true;
        assert_eq!(state.begin_playback(Some(8)), None);
        state.retry_playback_on_enter();
        assert_eq!(state.begin_playback(Some(8)), Some(2));
        assert_eq!(state.begin_playback(None), Some(3));
        assert_eq!(state.begin_playback(None), None);
    }

    #[test]
    fn playback_update_preserves_new_show_and_supersedes_older_reads() {
        let mut state = WatchingState::default();
        state.begin_playback(Some(7));
        state.playback_entry = Some(playback_entry(7, None));
        state.begin_playback(Some(8));
        let mut current = playback_entry(8, None);
        current.progress = 4;
        state.playback_entry = Some(current.clone());
        let read = state.playback_request;
        assert!(!state.accept_playback_update(7, Some(playback_entry(7, None))));
        assert!(!state.accept_playback_update(7, None));
        assert_eq!(state.playback_request, read);
        assert_eq!(state.playback(Some(8)).unwrap().progress, 4);
        current.progress = 5;
        assert!(state.accept_playback_update(8, Some(current)));
        assert_ne!(state.playback_request, read);
        assert_eq!(state.playback(Some(8)).unwrap().progress, 5);
        assert!(!state.playback_loading);
        assert!(state.playback_checked);
    }

    #[test]
    fn playback_lookup_ignores_another_shows_entry() {
        let mut state = WatchingState::default();
        state.replace_rows(vec![playback_entry(8, None)]);
        state.playback_entry = Some(playback_entry(7, None));
        assert_eq!(state.playback(Some(8)).unwrap().media_id, 8);
        assert!(state.playback(Some(9)).is_none());
        assert!(state.playback(None).is_none());
    }

    fn cover_rows(count: i64) -> Vec<ListEntry> {
        (1..=count)
            .map(|id| ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                progress: 1,
                media: Some(Media {
                    id,
                    title_english: Some(format!("Synthetic show {id:03}")),
                    cover_medium: Some(format!("invalid-{id}")),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect()
    }

    fn cover_frames(cx: &mut VisualTestContext, count: usize) -> usize {
        let mut callbacks = 0;
        for _ in 0..count {
            callbacks += cx.update(|window, app| {
                window.draw(app).clear(app);
                window.simulate_next_frame(app)
            });
            cx.run_until_parked();
        }
        callbacks
    }

    fn assert_measured_cover_owners(workspace: &Entity<Workspace>, cx: &VisualTestContext) {
        workspace.read_with(cx, |workspace, _| {
            let viewport = workspace.watching.list_widget.viewport_bounds();
            let expected: HashSet<_> = workspace
                .watching
                .current
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    workspace
                        .watching
                        .list_widget
                        .bounds_for_item(WatchingSlot::row_ix(index))
                        .filter(|bounds| {
                            bounds.top() < viewport.bottom() && bounds.bottom() > viewport.top()
                        })
                        .map(|_| (entry.media_id, 40, 56))
                })
                .collect();
            let region = &workspace.cover_regions[&CoverOwner::Route(Route::Now)];
            assert_eq!(region.audit_visible(), &expected);
            assert!(region.audit_nearby().len() <= 16);
            assert!(region.audit_visible().is_disjoint(region.audit_nearby()));
            assert_eq!(workspace.watching.cover_bounds, Some(viewport));
            assert_eq!(
                workspace.watching.cover_revision,
                workspace.watching.cover_reconciled
            );
            assert!(workspace.watching.cover_scheduled.is_none());
        });
    }

    struct WatchingHost {
        workspace: Entity<Workspace>,
        _observe: Subscription,
    }

    impl Render for WatchingHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.workspace.clone())
        }
    }

    #[gpui::test]
    fn watching_covers_follow_library_buttons_and_pending_progress(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        cx.simulate_resize(gpui::size(px(800.), px(1000.)));
        let files: Vec<_> = (1..=80)
            .map(|id| LibraryFile {
                media_id: Some(id),
                episode: Some(2),
                path: format!("/synthetic/{id}.mkv"),
                ..Default::default()
            })
            .collect();
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.auth_intent += 1;
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            let mut rows = cover_rows(80);
            for row in &mut rows {
                row.media.as_mut().unwrap().title_english =
                    Some("Watching narrow title ".repeat(5));
            }
            workspace.watching.replace_rows(rows);
            workspace.watching.loaded = true;
            workspace.library_files.replace(files.clone());
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        let tall = cx.debug_bounds("watching-row-1").unwrap().size.height;
        let unrelated: Vec<_> = files
            .iter()
            .cloned()
            .map(|mut file| {
                file.media_id = file.media_id.map(|id| id + 1000);
                file
            })
            .collect();
        for (files, has_buttons) in [
            (Vec::new(), false),
            (files.clone(), true),
            (unrelated, false),
            (files, true),
        ] {
            workspace.update_in(cx, |workspace, _, cx| {
                workspace.library_files.replace(files);
                cx.notify();
            });
            cx.run_until_parked();
            assert!(cover_frames(cx, 5) <= 4);
            assert_measured_cover_owners(&workspace, cx);
            let height = cx.debug_bounds("watching-row-1").unwrap().size.height;
            if has_buttons {
                assert_eq!(height, tall);
            } else {
                assert!(height < tall);
            }
            assert_eq!(cover_frames(cx, 20), 0);
        }
        workspace.update_in(cx, |workspace, _, cx| {
            for entry in &workspace.watching.current {
                workspace
                    .progress
                    .entry(workspace.generation, entry)
                    .step(1, std::time::Instant::now());
            }
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        assert!(cx.debug_bounds("watching-row-1").unwrap().size.height < tall);
        assert_eq!(cover_frames(cx, 20), 0);
    }

    #[gpui::test]
    fn watching_covers_follow_library_changes_beyond_initial_sample(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        cx.simulate_resize(gpui::size(px(800.), px(5000.)));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.auth_intent += 1;
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            let mut rows = cover_rows(80);
            for row in &mut rows {
                row.media.as_mut().unwrap().title_english =
                    Some("Watching narrow title ".repeat(5));
            }
            workspace.watching.replace_rows(rows);
            workspace.watching.loaded = true;
            workspace.library_files.replace(
                (1..=80)
                    .map(|id| LibraryFile {
                        media_id: Some(id),
                        episode: Some(2),
                        path: format!("/synthetic/{id}.mkv"),
                        ..Default::default()
                    })
                    .collect(),
            );
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace
                    .watching
                    .cover_visible
                    .iter()
                    .any(|key| key.0 > 36)
            );
        });
        workspace.update_in(cx, |workspace, _, cx| {
            let files = workspace
                .library_files
                .iter()
                .filter(|file| file.media_id.unwrap() <= 36)
                .cloned()
                .collect();
            workspace.library_files.replace(files);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        assert_eq!(cover_frames(cx, 20), 0);
    }

    #[gpui::test]
    fn watching_playback_controls_invalidate_cover_layout(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.auth_intent += 1;
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.replace_rows(cover_rows(80));
            workspace.watching.loaded = true;
            let mut entry = cover_rows(1).pop().unwrap();
            entry.media_id = 900;
            workspace.watching.playback_entry = Some(entry);
            workspace.now_playing = Some(playing(900));
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        let old_key = workspace.read_with(cx, |workspace, _| workspace.watching_cover_layout_key());
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.library_files.replace(vec![LibraryFile {
                media_id: Some(900),
                episode: Some(2),
                path: "/synthetic/900.mkv".into(),
                ..Default::default()
            }]);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        let with_button =
            workspace.read_with(cx, |workspace, _| workspace.watching_cover_layout_key());
        assert_ne!(old_key, with_button);
        workspace.update_in(cx, |workspace, _, cx| {
            let entry = workspace.watching.playback_entry.as_ref().unwrap();
            workspace
                .progress
                .entry(workspace.generation, entry)
                .step(1, std::time::Instant::now());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        assert_ne!(
            with_button,
            workspace.read_with(cx, |workspace, _| workspace.watching_cover_layout_key())
        );
        assert_eq!(cover_frames(cx, 20), 0);
    }

    #[gpui::test]
    fn measured_watching_owners_follow_resize_scale_and_height(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.enter_watching(window, cx);
            workspace.watching.replace_rows(cover_rows(250));
            workspace.watching.loaded = true;
            workspace.watching.loading = false;
            workspace.refresh_watching_covers();
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        cx.simulate_resize(gpui::size(px(1100.), px(1400.)));
        cx.run_until_parked();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        cx.simulate_scale_factor_change(1.25);
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.list.step_error = "Synthetic failed progress update".into();
            workspace.watching.current[0]
                .media
                .as_mut()
                .unwrap()
                .title_english = Some("Long synthetic title ".repeat(30));
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        cx.simulate_resize(gpui::size(px(800.), px(40.)));
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        workspace.read_with(cx, |workspace, _| {
            let region = &workspace.cover_regions[&CoverOwner::Route(Route::Now)];
            assert!(region.audit_visible().is_empty());
            assert!(region.audit_nearby().is_empty());
        });
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.watching.list_widget.scroll_to(ListOffset {
                item_ix: WatchingSlot::footer_ix(250),
                offset_in_item: px(5.),
            });
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.watching.replace_rows(Vec::new());
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_measured_cover_owners(&workspace, cx);
    }

    #[gpui::test]
    fn watching_nested_resize_repaints_once_and_stays_idle(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let notifications = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let observed = notifications.clone();
        let (host, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::new(core, window, cx));
            let subscription = cx.observe_in(&workspace, window, move |_, _, _, _| {
                observed.set(observed.get() + 1);
            });
            WatchingHost {
                workspace,
                _observe: subscription,
            }
        });
        let workspace = host.read_with(cx, |host, _| host.workspace.clone());
        cx.simulate_resize(gpui::size(px(800.), px(600.)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.enter_watching(window, cx);
            workspace.watching.replace_rows(cover_rows(250));
            workspace.watching.loaded = true;
            workspace.watching.loading = false;
            workspace.refresh_watching_covers();
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        cx.simulate_resize(gpui::size(px(1100.), px(1400.)));
        cx.run_until_parked();
        let before = notifications.get();
        assert!(cover_frames(cx, 5) <= 4);
        assert_measured_cover_owners(&workspace, cx);
        assert!(notifications.get() > before);
        let settled = notifications.get();
        assert_eq!(cover_frames(cx, 20), 0);
        assert_eq!(notifications.get(), settled);
        let owners = workspace.read_with(cx, |workspace, _| {
            workspace.cover_regions[&CoverOwner::Route(Route::Now)]
                .audit_visible()
                .clone()
        });
        let key = *owners.iter().min().unwrap();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.cover_images.insert(
                key,
                Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::new(1, 1),
                )])),
            );
            workspace
                .watching
                .current
                .iter_mut()
                .find(|entry| entry.media_id == key.0)
                .unwrap()
                .media
                .as_mut()
                .unwrap()
                .cover_medium = Some("invalid-replaced-cover".into());
            workspace.mark_watching_cover_refresh();
            workspace.schedule_watching_cover_refresh(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
        let before_image_change = notifications.get();
        assert!(cover_frames(cx, 5) <= 3);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.cover_regions[&CoverOwner::Route(Route::Now)].audit_visible(),
                &owners
            );
            assert!(!workspace.cover_images.contains_key(&key));
        });
        assert_eq!(notifications.get() - before_image_change, 1);
        assert_eq!(cover_frames(cx, 20), 0);
    }

    #[gpui::test]
    fn scheduled_watching_cover_refresh_rejects_replaced_account(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.replace_rows(cover_rows(20));
            workspace.mark_watching_cover_refresh();
            workspace.schedule_watching_cover_refresh(window, cx);
            workspace.generation += 1;
            workspace.profile = Some(User {
                id: 48,
                ..Default::default()
            });
            workspace.route = Route::MyList;
            workspace.watching = Default::default();
            workspace
                .cover_regions
                .remove(&CoverOwner::Route(Route::Now));
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        workspace.read_with(cx, |workspace, _| {
            assert!(
                !workspace
                    .cover_regions
                    .contains_key(&CoverOwner::Route(Route::Now))
            );
            assert!(workspace.watching.cover_scheduled.is_none());
            assert!(workspace.watching.cover_visible.is_empty());
        });
    }

    #[gpui::test]
    fn newer_account_refresh_survives_older_scheduled_callback(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Now;
            workspace.watching.replace_rows(cover_rows(20));
            workspace.schedule_watching_cover_refresh(window, cx);
            workspace.generation += 1;
            workspace.profile = Some(User {
                id: 48,
                ..Default::default()
            });
            let mut new_rows = cover_rows(1);
            new_rows[0].media_id = 900;
            new_rows[0].media.as_mut().unwrap().id = 900;
            workspace.watching.replace_rows(new_rows);
            workspace.schedule_watching_cover_refresh(window, cx);
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        workspace.read_with(cx, |workspace, _| {
            let region = &workspace.cover_regions[&CoverOwner::Route(Route::Now)];
            assert_eq!(region.audit_visible(), &HashSet::from([(900, 40, 56)]));
            assert!(region.audit_nearby().is_empty());
            assert!(workspace.watching.cover_scheduled.is_none());
        });
    }
}
