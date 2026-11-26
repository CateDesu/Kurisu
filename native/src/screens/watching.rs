use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    ops::ControlFlow,
    sync::Arc,
};

use gpui::{
    AnyElement, Bounds, Context, ListAlignment, ListOffset, ListState, Pixels, RenderImage, Size,
    WeakEntity, Window, div, img, list, prelude::*, px, rgb,
};
use kurisu_core::models::{LibraryFile, ListEntry};
use kurisu_core::{EntryIdentity, Platform};

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
    // Only independent Watching reads advance this revision.
    row_revision: u64,
    request: u64,
    #[cfg(test)]
    list_response_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    #[cfg(test)]
    playback_start_gate: Option<tokio::sync::oneshot::Receiver<()>>,
    #[cfg(test)]
    launch_hook: Option<LaunchHook>,
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
    launch_error: String,
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

#[derive(Clone, Copy)]
enum WatchingPlay {
    Playback(i64),
    Row(i64),
}

#[cfg(test)]
type LaunchHook = Box<dyn FnOnce(std::path::PathBuf) -> Result<(), String> + Send>;

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
            row_revision: 0,
            request: 0,
            #[cfg(test)]
            list_response_gate: None,
            #[cfg(test)]
            playback_start_gate: None,
            #[cfg(test)]
            launch_hook: None,
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
            launch_error: String::new(),
            update_request: 0,
        }
    }
}

impl WatchingState {
    pub(super) fn row_revision(&self) -> u64 {
        self.row_revision
    }

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

    fn reconcile_row(&mut self, id: i64, entry: Option<&ListEntry>) {
        let index = self.current.iter().position(|row| row.media_id == id);
        match (index, entry.filter(|entry| entry.status == "CURRENT")) {
            (Some(index), Some(entry)) => {
                self.current[index] = entry.clone();
                let item = WatchingSlot::row_ix(index);
                self.list_widget.remeasure_items(item..item + 1);
            }
            (Some(_), None) => self.remove_row(id),
            (None, Some(entry)) => {
                let anchor = self.anchor();
                self.current.push(entry.clone());
                self.restore_anchor(anchor);
            }
            (None, None) => {}
        }
    }

    pub(super) fn clear_scores(&mut self) {
        for entry in self
            .current
            .iter_mut()
            .chain(self.playback_entry.iter_mut())
        {
            entry.score = None;
        }
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
        if self.playback_id != id {
            self.playback_entry = None;
            self.update_error.clear();
            self.update_request = self.update_request.wrapping_add(1);
            self.updating = false;
        }
        self.playback_id = id;
        self.playback_request += 1;
        self.playback_loading = id.is_some();
        self.playback_checked = id.is_none();
        Some(self.playback_request)
    }

    fn playback(&self, id: Option<i64>) -> Option<&ListEntry> {
        let id = id?;
        if self.playback_id == Some(id) && self.playback_checked {
            return self.playback_entry.as_ref();
        }
        self.playback_entry
            .as_ref()
            .filter(|entry| entry.media_id == id)
            .or_else(|| self.current.iter().find(|entry| entry.media_id == id))
    }

    fn accept_playback_snapshot(&mut self, id: i64, entry: Option<ListEntry>) -> bool {
        if self.playback_id != Some(id) {
            return false;
        }
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
                self.row_revision = self.row_revision.wrapping_add(1);
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
    pub(super) fn reconcile_watching_entries(&mut self, scope: Option<&[i64]>) {
        if self.core.generation() != self.generation {
            return;
        }
        let includes = |id: &i64| scope.is_none_or(|ids| ids.contains(id));
        if let Some(id) = self.watching.playback_id.filter(includes) {
            let entry = self.list.entry(id).cloned();
            self.watching.accept_playback_snapshot(id, entry);
        }

        if let Some(ids) = scope {
            for &id in ids {
                self.watching.reconcile_row(id, self.list.entry(id));
            }
        } else {
            self.watching.replace_rows(
                self.list
                    .ordered_entries()
                    .filter(|entry| entry.status == "CURRENT")
                    .cloned()
                    .collect(),
            );
        }
        if self.route == Route::Now {
            self.refresh_watching_covers();
        }
    }

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
        let end = range.end.saturating_add(10);
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        let entries = self.watching.current.iter().enumerate();
        for (index, entry) in entries.take(end).skip(start) {
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
            .map_or(entry.progress, |edit| edit.pending)
    }

    fn watching_next_file(&self, entry: &ListEntry) -> Option<&LibraryFile> {
        next_episode_file(
            &self.library_files,
            entry.media_id,
            self.watching_progress(entry),
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
        self.watching.launch_error.hash(&mut key);
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
        if !self.stopping && self.profile.is_some() {
            self.load_watching(window, cx);
        }
    }

    pub(super) fn refresh_watching_playback(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.core.generation() != self.generation {
            return;
        }
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
        let list_epoch = self.list_epoch;
        let list_revision = self.list.revision();
        let row_revision = self.watching.row_revision;
        let core = self.core.clone();
        let worker = core.clone();
        #[cfg(test)]
        let gate = self.watching.playback_start_gate.take();
        let job = core.spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            worker.get_entry(id).await
        });
        self.replace_reads("watching-entry", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.watching.playback_request != request
                    || this.watching.playback_id != Some(id)
                {
                    return;
                }
                this.watching.playback_loading = false;
                if this.core.generation() != generation {
                    return;
                }
                if this.list_epoch != list_epoch
                    || this.list.revision() != list_revision
                    || this.watching.row_revision != row_revision
                {
                    this.watching.playback_checked = false;
                    if this.route == Route::Now {
                        this.refresh_watching_playback(window, cx);
                    }
                    cx.notify();
                    return;
                }
                if let Ok(entry) = result {
                    if let Some(entry) = &entry {
                        this.progress.reconcile_entry(generation, entry);
                        this.update_step_error();
                    }
                    this.watching.row_revision = this.watching.row_revision.wrapping_add(1);
                    this.watching.reconcile_row(id, entry.as_ref());
                    this.watching.accept_playback_snapshot(id, entry);
                }
                this.ensure_cover_scale(window.scale_factor());
                this.refresh_watching_covers();
                cx.notify();
            });
        })
        .detach();
    }

    fn load_watching(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.core.generation() != self.generation {
            return;
        }
        self.watching.request += 1;
        let request = self.watching.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let captured = WatchingRead {
            generation,
            account,
            request,
        };
        let list_revision = self.list.revision();
        let list_epoch = self.list_epoch;
        let row_revision = self.watching.row_revision;
        self.watching.loading = true;
        self.watching.error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.local_entries().await });
        self.replace_reads("watching-list", vec![job.abort_handle()]);
        #[cfg(test)]
        let gate = self.watching.list_response_gate.take();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping {
                    return;
                }
                let current = WatchingRead {
                    generation: this.generation,
                    account: this.profile.as_ref().map(|user| user.id),
                    request: this.watching.request,
                };
                if captured != current {
                    return;
                }
                if this.core.generation() != generation {
                    this.watching.loading = false;
                    return;
                }
                if list_revision != this.list.revision()
                    || list_epoch != this.list_epoch
                    || (row_revision != this.watching.row_revision && result.is_ok())
                {
                    this.watching.loading = false;
                    if this.route == Route::Now {
                        this.load_watching(window, cx);
                    }
                    cx.notify();
                    return;
                }
                let playback = this.watching.playback_id.and_then(|id| {
                    result.as_ref().ok().map(|entries| {
                        (
                            id,
                            entries.iter().find(|entry| entry.media_id == id).cloned(),
                        )
                    })
                });
                if !this.watching.accept_list(captured, current, result) {
                    return;
                }
                for entry in &this.watching.current {
                    this.progress.reconcile_entry(generation, entry);
                }
                if let Some((id, entry)) = playback {
                    if let Some(entry) = &entry {
                        this.progress.reconcile_entry(generation, entry);
                    }
                    this.watching.accept_playback_snapshot(id, entry);
                }
                this.update_step_error();
                this.ensure_cover_scale(window.scale_factor());
                this.refresh_watching_covers();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn step_watching(
        &mut self,
        id: i64,
        identity: &EntryIdentity,
        delta: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits_fenced()
            || self.route != Route::Now
            || self.profile.is_none()
            || self.core.generation() != self.generation
            || self.modal_open()
            || self.watching.updating
        {
            return;
        }
        let Some(entry) = self
            .watching
            .current
            .iter()
            .find(|entry| entry.media_id == id && entry.identity() == *identity)
            .cloned()
        else {
            return;
        };
        if self.list.entry(id).is_none() {
            let change = self.list.apply_one(entry.clone(), now_seconds());
            self.apply_list_change(change);
        }
        self.step_entry_progress(&entry, delta, window, cx);
    }

    fn update_detected_episode(
        &mut self,
        id: i64,
        episode: i64,
        identity: EntryIdentity,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits_fenced()
            || self.route != Route::Now
            || self.profile.is_none()
            || self.modal_open()
            || self.watching.updating
            || self.core.generation() != self.generation
            || !self.now_playing.as_ref().is_some_and(|playing| {
                playing.active
                    && playing.matched.is_some()
                    && playing.media_id == Some(id)
                    && playing.episode == Some(episode)
            })
            || self.watching.playback(Some(id)).is_none_or(|entry| {
                !identity.matches(entry) || self.watching_progress(entry) >= episode
            })
        {
            return;
        }
        if self.progress.busy(self.generation, id)
            || self
                .progress
                .edit(self.generation, id)
                .is_some_and(|edit| edit.dirty() || edit.requires_review)
        {
            self.watching.update_error = "Finish the pending episode change first.".into();
            cx.notify();
            return;
        }
        self.watching.update_request += 1;
        let request = self.watching.update_request;
        let list_revision = self.list.revision();
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
                    if worker.generation() != generation {
                        return Err("Account changed".into());
                    }
                    let Some(fresh) = worker.get_entry(id).await? else {
                        return Ok(None);
                    };
                    if worker.generation() != generation {
                        return Err("Account changed".into());
                    }
                    let replaced = "Your AniList entry changed. Review before saving.";
                    if !identity.matches(&fresh) {
                        return Err(replaced.into());
                    }
                    if fresh.progress >= episode {
                        Ok(Some(fresh))
                    } else {
                        let saved = match worker
                            .set_list_progress(
                                id,
                                episode,
                                fresh.progress,
                                account,
                                identity.clone(),
                            )
                            .await?
                        {
                            ControlFlow::Continue(saved) => saved,
                            ControlFlow::Break(entry) if identity.matches(&entry) => entry,
                            ControlFlow::Break(_) => return Err(replaced.into()),
                        };
                        if saved.progress < episode
                            && saved
                                .media
                                .as_ref()
                                .and_then(|media| media.known_episodes())
                                .is_none_or(|total| saved.progress < total)
                        {
                            return Err(
                                "Progress changed on your list. Review your list before updating."
                                    .into(),
                            );
                        }
                        Ok(Some(saved))
                    }
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation != generation
                    || this.core.generation() != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.watching.update_request != request
                {
                    this.check_barrier(window, cx);
                    return;
                }
                this.watching.updating = false;
                match result {
                    Ok(_) if this.list.revision() != list_revision => {
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                        this.refresh_watching_list(window, cx);
                    }
                    Ok(Some(entry)) => {
                        let change = this.list.apply_one(entry.clone(), now_seconds());
                        this.progress.reconcile_entry(generation, &entry);
                        this.update_step_error();
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        this.reconcile_open_entries(Some(&[id]), cx);
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Ok(None) => {
                        let change = this.list.apply_scoped(&[id], vec![None], now_seconds());
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        this.reconcile_open_entries(Some(&[id]), cx);
                        this.watching.update_error = "This show is no longer on your list.".into();
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Err(error) => {
                        this.watching.update_error = error;
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
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
            let update_target = entry.zip(playing.episode).filter(|&(_, episode)| {
                playing.matched.is_some() && progress.is_some_and(|progress| episode > progress)
            });
            let next = entry
                .and_then(|entry| self.watching_next_file(entry))
                .cloned();
            let cover = playing
                .media_id
                .and_then(|id| self.cover_images.get(&(id, 96, 140)).cloned());
            let mut controls = div().flex().flex_wrap().gap(px(7.));
            let play_disabled = self.edits_fenced() || self.modal_open();
            if let Some((id, file)) = playing.media_id.zip(next) {
                let episode = file.episode.unwrap_or(0);
                controls = controls.child(
                    button(
                        format!(
                            "watching-play-next-{generation}-{id}-{episode}-{}",
                            file.path
                        ),
                        format!("▶ Play Ep {episode}"),
                        play_disabled,
                    )
                    .debug_selector(|| "watching-play-next".into())
                    .when(!play_disabled, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            open_local(
                                this,
                                WatchingPlay::Playback(id),
                                &file,
                                generation,
                                account,
                                cx,
                            )
                        }))
                    }),
                );
            }
            if let Some((entry, episode)) = update_target {
                let id = entry.media_id;
                let identity = entry.identity();
                let entry_key = match &identity {
                    EntryIdentity::Remote(id) => format!("remote-{id}"),
                    EntryIdentity::Queued(key) => format!("queued-{key}"),
                    EntryIdentity::Unverified => "unverified".into(),
                };
                let disabled = self.watching.updating
                    || self.edits_fenced()
                    || self.modal_open()
                    || identity == EntryIdentity::Unverified;
                controls = controls.child(
                    button(
                        format!("watching-update-{generation}-{id}-{episode}-{entry_key}"),
                        if self.watching.updating {
                            "Updating…".into()
                        } else {
                            format!("Update to Ep {episode}")
                        },
                        disabled,
                    )
                    .debug_selector(|| "watching-update".into())
                    .bg(rgb(ACCENT))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.update_detected_episode(id, episode, identity.clone(), window, cx);
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
                    .id(format!("watching-playback-cover-{generation}-{id}"))
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
                    button(
                        format!("watching-details-{generation}-{id}"),
                        matched.clone(),
                        false,
                    )
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
        if !self.watching.launch_error.is_empty() {
            page = page.child(div().text_color(rgb(ERROR)).child(format!(
                "Could not open file: {}",
                self.watching.launch_error
            )));
        }
        if !self.list.step_error.is_empty() {
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(format!("Episode update failed: {}", self.list.step_error)),
            );
        }
        if !self.watching.error.is_empty() {
            let retry_disabled = self.stopping || self.writes_fenced();
            page = page.child(
                div()
                    .text_color(rgb(ERROR))
                    .child(self.watching.error.clone())
                    .child(button("watching-retry", "Retry", retry_disabled).when(
                        !retry_disabled,
                        |button| {
                            button.on_click(
                                cx.listener(|this, _, window, cx| this.load_watching(window, cx)),
                            )
                        },
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
                                    let cover =
                                        this.cover_images.get(&(entry.media_id, 40, 56)).cloned();
                                    let pending = this
                                        .progress
                                        .edit(this.generation, entry.media_id)
                                        .cloned();
                                    let next = this.watching_next_file(entry).cloned();
                                    let score_format = this
                                        .profile
                                        .as_ref()
                                        .and_then(|user| user.score_format.clone());
                                    let generation = this.generation;
                                    let account = this.profile.as_ref().map(|user| user.id);
                                    let play_disabled = this.edits_fenced() || this.modal_open();
                                    let progress_disabled = play_disabled || this.watching.updating;
                                    (
                                        entry.clone(),
                                        pending,
                                        cover,
                                        next,
                                        score_format,
                                        generation,
                                        account,
                                        play_disabled,
                                        progress_disabled,
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
                                play_disabled,
                                progress_disabled,
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
                                    play_disabled,
                                    progress_disabled,
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
    target: WatchingPlay,
    file: &LibraryFile,
    generation: u64,
    account: Option<i64>,
    cx: &mut Context<Workspace>,
) {
    if !scope_matches(this, generation, account)
        || this.core.generation() != generation
        || this.route != Route::Now
        || account.is_none()
        || this.edits_fenced()
        || this.modal_open()
    {
        return;
    }
    let entry = match target {
        WatchingPlay::Playback(id) => this
            .now_playing
            .as_ref()
            .filter(|playing| playing.active && playing.media_id == Some(id))
            .and_then(|_| this.watching.playback(Some(id))),
        WatchingPlay::Row(id) => this
            .watching
            .current
            .iter()
            .find(|entry| entry.media_id == id),
    };
    if entry
        .and_then(|entry| this.watching_next_file(entry))
        .is_none_or(|next| next.episode != file.episode || next.path != file.path)
    {
        return;
    }
    if !this.watching.launch_error.is_empty() {
        this.watching.launch_error.clear();
        cx.notify();
    }
    let path = std::path::PathBuf::from(&file.path);
    #[cfg(test)]
    let launch_hook = this.watching.launch_hook.take();
    let job = this.core.spawn(async move {
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(launch) = launch_hook {
                return launch(path);
            }
            crate::platform::NativePlatform.open_path(&path)
        })
        .await
        .map_err(|error| error.to_string())?
    });
    cx.spawn(async move |this, cx| {
        let result = joined(job.await);
        let _ = this.update(cx, |this, cx| {
            if !scope_matches(this, generation, account)
                || this.core.generation() != generation
                || this.stopping
                || this.close_ready
            {
                return;
            }
            if let Err(error) = result {
                this.watching.launch_error = error;
                cx.notify();
            }
        });
    })
    .detach();
}
#[allow(clippy::too_many_arguments)]
fn watching_row(
    entry: ListEntry,
    pending: Option<super::state::ProgressEdit>,
    cover: Option<Arc<RenderImage>>,
    next: Option<kurisu_core::models::LibraryFile>,
    score_format: Option<&str>,
    weak: WeakEntity<Workspace>,
    scope: (u64, Option<i64>),
    play_disabled: bool,
    progress_disabled: bool,
) -> AnyElement {
    let (generation, account) = scope;
    let id = entry.media_id;
    let total = entry
        .media
        .as_ref()
        .and_then(|media| media.known_episodes());
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
    let saving = pending.as_ref().is_some_and(|edit| edit.busy());
    let review = pending.as_ref().is_some_and(|edit| edit.requires_review);
    let progress_locked = saving || review || progress_disabled;
    let has_error = pending.as_ref().is_some_and(|edit| edit.error.is_some());
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
    let identity = entry.identity();
    let keys_identity = identity.clone();
    let less_identity = identity.clone();
    let stepper_key = match &identity {
        EntryIdentity::Remote(entry_id) => {
            format!("watching-stepper-{generation}-{id}-remote-{entry_id}")
        }
        EntryIdentity::Queued(key) => format!("watching-stepper-{generation}-{id}-queued-{key}"),
        EntryIdentity::Unverified => format!("watching-stepper-{generation}-{id}-unverified"),
    };
    let stepper = div()
        .id(stepper_key)
        .debug_selector(move || format!("watching-stepper-{id}"))
        .tab_stop(!progress_locked)
        .when(!progress_locked, |stepper| stepper.focusable())
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
            if progress_locked || (delta < 0 && at_min) || (delta > 0 && at_max) {
                return;
            }
            let _ = weak_keys.update(app, |this, cx| {
                if scope_matches(this, generation, account) {
                    this.step_watching(id, &keys_identity, delta, window, cx);
                }
            });
        })
        .child(
            div()
                .id(format!("watching-progress-state-{id}"))
                .w(px(if review { 42. } else { 12. }))
                .flex_shrink_0()
                .text_size(px(10.))
                .flex()
                .justify_center()
                .text_color(rgb(if review || has_error { ERROR } else { ACCENT }))
                .when(review, |indicator| {
                    indicator
                        .role(gpui::Role::Status)
                        .aria_label("Review saved progress")
                })
                .when(saving && !review, |indicator| {
                    indicator
                        .role(gpui::Role::Status)
                        .aria_label("Saving progress")
                })
                .child(if review {
                    "Review"
                } else if saving {
                    "…"
                } else if has_error {
                    "!"
                } else if pending.as_ref().is_some_and(|edit| edit.dirty()) {
                    "●"
                } else {
                    ""
                }),
        )
        .child(
            button(
                format!("watching-less-{id}"),
                "−",
                at_min || progress_locked,
            )
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
            .opacity(if at_min || progress_locked { 0.3 } else { 1. })
            .aria_label("One less episode")
            .on_click(move |_, window, app| {
                if at_min || progress_locked {
                    return;
                }
                let _ = weak_less.update(app, |this, cx| {
                    if scope_matches(this, generation, account) {
                        this.step_watching(id, &less_identity, -1, window, cx);
                    }
                });
            }),
        )
        .child(progress_value)
        .child(
            button(
                format!("watching-more-{id}"),
                "+",
                at_max || progress_locked,
            )
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
            .opacity(if at_max || progress_locked { 0.3 } else { 1. })
            .aria_label("One more episode")
            .on_click(move |_, window, app| {
                if at_max || progress_locked {
                    return;
                }
                let _ = weak_more.update(app, |this, cx| {
                    if scope_matches(this, generation, account) {
                        this.step_watching(id, &identity, 1, window, cx);
                    }
                });
            }),
        );
    row = row.child(stepper);
    if let Some(file) = next {
        let weak_play = weak.clone();
        let episode = file.episode.unwrap_or(0);
        row = row.child(
            button(
                format!("watching-play-{generation}-{id}-{episode}-{}", file.path),
                format!("▶ Ep {episode}"),
                play_disabled,
            )
            .debug_selector(move || format!("watching-play-{id}"))
            .when(!play_disabled, |button| {
                button.on_click(move |_, _, app| {
                    let _ = weak_play.update(app, |this, cx| {
                        open_local(this, WatchingPlay::Row(id), &file, generation, account, cx)
                    });
                })
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
    use kurisu_core::{Core, Event, EventKind};
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

    fn signed_in_fixture() -> (
        Core,
        tokio::runtime::Runtime,
        FixtureDir,
        kurisu_core::Db,
        User,
    ) {
        account_fixture(47)
    }

    fn account_fixture(
        id: i64,
    ) -> (
        Core,
        tokio::runtime::Runtime,
        FixtureDir,
        kurisu_core::Db,
        User,
    ) {
        let (initial, runtime, dir) = fixture();
        drop(initial);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        let profile = User {
            id,
            offline: true,
            ..Default::default()
        };
        db.replace_account("fixture-session", &profile).unwrap();
        let core = Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
        )
        .unwrap();
        (core, runtime, dir, db, profile)
    }

    fn finish_reads(runtime: &tokio::runtime::Runtime, reads: &[tokio::task::AbortHandle]) {
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(4), async {
                while reads.iter().any(|read| !read.is_finished()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("Watching reads did not finish");
        });
    }

    fn hold_watching_list(
        w: &mut Workspace,
        window: &Window,
        cx: &mut Context<Workspace>,
    ) -> (tokio::sync::oneshot::Sender<()>, tokio::task::AbortHandle) {
        let (release, gate) = tokio::sync::oneshot::channel();
        w.watching.list_response_gate = Some(gate);
        w.refresh_watching_list(window, cx);
        (release, w.read_aborts["watching-list"][0].clone())
    }

    fn playback_event(w: &Workspace, now: NowPlaying) -> Event {
        Event {
            generation: w.generation,
            account_id: w.profile.as_ref().map(|user| user.id),
            prompt_id: None,
            kind: EventKind::NowPlaying(now),
        }
    }

    #[gpui::test]
    fn watching_new_playback_read_removes_row_from_older_list_snapshot(cx: &mut TestAppContext) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let list = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            w.refresh_watching_list(window, cx);
            w.read_aborts["watching-list"][0].clone()
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();
        entry.status = "PAUSED".into();
        db.upsert_entry(&entry).unwrap();
        let playback = view.update_in(cx, |w, window, cx| {
            w.now_playing = Some(playing(7));
            w.refresh_watching_playback(window, cx);
            w.read_aborts["watching-entry"][0].clone()
        });
        finish_reads(&runtime, &[playback]);
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert_eq!(w.watching.playback(Some(7)).unwrap().status, "PAUSED");
            assert!(w.watching.current.is_empty());
        });
    }

    #[gpui::test]
    fn watching_older_list_keeps_a_later_active_entry_lookup(cx: &mut TestAppContext) {
        check_active_lookup_after_older_list(cx, false);
    }

    #[gpui::test]
    fn watching_older_shared_list_keeps_a_later_active_entry_lookup(cx: &mut TestAppContext) {
        check_active_lookup_after_older_list(cx, true);
    }

    fn check_active_lookup_after_older_list(cx: &mut TestAppContext, shared: bool) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let (release, gate) = tokio::sync::oneshot::channel();
        let (playback, list) = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            w.watching.playback_start_gate = Some(gate);
            let mut now = playing(7);
            now.matched = Some(now.title.clone());
            now.episode = Some(3);
            w.handle_event(playback_event(w, now), window, cx);
            let group = if shared {
                w.load_list(window, cx);
                "list"
            } else {
                w.refresh_watching_list(window, cx);
                "watching-list"
            };
            (
                w.read_aborts["watching-entry"][0].clone(),
                w.read_aborts[group][0].clone(),
            )
        });
        finish_reads(&runtime, &[list]);
        entry.status = "PAUSED".into();
        entry.progress = 3;
        db.upsert_entry(&entry).unwrap();
        cx.run_until_parked();
        assert!(cx.debug_bounds("watching-update").is_some());

        let _ = release.send(());
        finish_reads(&runtime, &[playback]);
        cx.run_until_parked();
        if let Some(retry) = view.read_with(cx, |w, _| {
            w.watching
                .playback_loading
                .then(|| w.read_aborts["watching-entry"][0].clone())
        }) {
            finish_reads(&runtime, &[retry]);
            cx.run_until_parked();
        }
        view.read_with(cx, |w, _| {
            assert_eq!(w.watching.playback(Some(7)).unwrap().status, "PAUSED");
            assert!(w.watching.current.is_empty());
            assert!(!w.watching.playback_loading);
        });
        assert!(cx.debug_bounds("watching-update").is_none());
        assert!(cx.debug_bounds("watching-playback-title").is_some());
    }

    #[gpui::test]
    fn watching_stop_keeps_newer_status_when_an_old_list_read_finishes(cx: &mut TestAppContext) {
        check_watching_list_after_stop(cx, false);
    }

    #[gpui::test]
    fn watching_reentry_retries_an_old_list_read_discarded_off_route(cx: &mut TestAppContext) {
        check_watching_list_after_stop(cx, true);
    }

    #[gpui::test]
    fn watching_stop_rejects_an_older_shared_list_during_close(cx: &mut TestAppContext) {
        check_shared_list_after_stop(cx, false);
    }

    #[gpui::test]
    fn watching_stop_retries_an_older_scoped_list(cx: &mut TestAppContext) {
        check_shared_list_after_stop(cx, true);
    }

    fn check_shared_list_after_stop(cx: &mut TestAppContext, scoped: bool) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let (release, gate) = tokio::sync::oneshot::channel();
        let list = view.update_in(cx, |w, window, cx| {
            let account = profile.id;
            w.profile = Some(profile);
            w.offline = true;
            w.route = Route::Now;
            w.list.replace(vec![entry.clone()], 0);
            w.watching.replace_rows(vec![entry.clone()]);
            w.list_response_gate = Some(gate);
            if scoped {
                w.refresh_scoped(vec![7], account, window, cx);
            } else {
                w.load_list(window, cx);
            }
            w.read_aborts["list"][0].clone()
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();

        entry.status = "PAUSED".into();
        db.upsert_entry(&entry).unwrap();
        let playback = view.update_in(cx, |w, window, cx| {
            w.handle_event(playback_event(w, playing(7)), window, cx);
            w.read_aborts["watching-entry"][0].clone()
        });
        finish_reads(&runtime, &[playback]);
        cx.run_until_parked();
        let request = view.update_in(cx, |w, window, cx| {
            assert!(w.watching.current.is_empty());
            let mut stopped = playing(7);
            stopped.active = false;
            w.handle_event(playback_event(w, stopped), window, cx);
            if !scoped {
                w.request_close(super::super::CloseIntent::Window, window, cx);
                assert!(w.close_ready);
            }
            w.list_request
        });

        release.send(()).unwrap();
        cx.run_until_parked();
        let reads = view.update_in(cx, |w, window, cx| {
            assert!(
                w.watching.current.is_empty(),
                "Shared list restored the paused row"
            );
            if scoped {
                assert_eq!(w.list_request, request + 1);
                vec![w.read_aborts["list"][0].clone()]
            } else {
                assert!(w.list_refresh_deferred);
                assert_eq!(w.list_request, request);
                w.resume_after_close_visibility(true, window, cx);
                vec![
                    w.read_aborts["list"][0].clone(),
                    w.read_aborts["watching-list"][0].clone(),
                ]
            }
        });
        finish_reads(&runtime, &reads);
        cx.run_until_parked();
        let retries = view.read_with(cx, |w, _| {
            [
                (w.list.loading, "list"),
                (w.watching.loading, "watching-list"),
            ]
            .into_iter()
            .filter(|(loading, _)| *loading)
            .map(|(_, group)| w.read_aborts[group][0].clone())
            .collect::<Vec<_>>()
        });
        finish_reads(&runtime, &retries);
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert_eq!(w.list.entry(7).unwrap().status, "PAUSED");
            assert!(w.watching.current.is_empty());
            assert!(!w.watching.loading && !w.list.loading && w.list_read.is_none());
        });
    }

    fn check_watching_list_after_stop(cx: &mut TestAppContext, leave_route: bool) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let (release, list) = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            w.watching.replace_rows(vec![entry.clone()]);
            hold_watching_list(w, window, cx)
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();

        entry.status = "PAUSED".into();
        db.upsert_entry(&entry).unwrap();
        let playback = view.update_in(cx, |w, window, cx| {
            w.handle_event(playback_event(w, playing(7)), window, cx);
            w.read_aborts["watching-entry"][0].clone()
        });
        finish_reads(&runtime, &[playback]);
        cx.run_until_parked();
        let request = view.update_in(cx, |w, window, cx| {
            assert!(w.watching.loading);
            assert_eq!(w.watching.playback(Some(7)).unwrap().status, "PAUSED");
            assert!(w.watching.current.is_empty());
            let mut stopped = playing(7);
            stopped.active = false;
            w.handle_event(playback_event(w, stopped), window, cx);
            assert!(w.watching.playback_id.is_none());
            if leave_route {
                w.navigate(Route::Search, window, cx);
                assert_eq!(w.route, Route::Search);
            }
            w.watching.request
        });

        release.send(()).unwrap();
        cx.run_until_parked();
        let retry = view.update_in(cx, |w, window, cx| {
            assert!(
                w.watching.current.is_empty(),
                "Old list restored the paused row"
            );
            if leave_route {
                assert!(!w.watching.loading);
                assert_eq!(w.watching.request, request);
                w.back(window, cx);
                assert_eq!(w.route, Route::Now);
            }
            assert!(w.watching.loading);
            assert_eq!(w.watching.request, request + 1);
            w.read_aborts["watching-list"][0].clone()
        });
        finish_reads(&runtime, &[retry]);
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert!(w.watching.loaded);
            assert!(!w.watching.loading);
            assert_eq!(w.watching.request, request + 1);
            assert!(w.watching.current.is_empty());
        });
    }

    #[gpui::test]
    fn watching_account_switch_ignores_a_held_list_completion(cx: &mut TestAppContext) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        db.upsert_entry(&playback_entry(7, None)).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let (release, list) = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            hold_watching_list(w, window, cx)
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();

        let profile = User {
            id: 48,
            offline: true,
            ..Default::default()
        };
        db.replace_account("fixture-session", &profile).unwrap();
        db.upsert_entry(&playback_entry(8, None)).unwrap();
        let reads = view.update_in(cx, |w, window, cx| {
            w.core.begin_login_intent();
            w.set_profile(Some(profile), window, cx);
            assert_eq!(w.watching.request, 1);
            assert!(w.watching.loading);
            [
                w.read_aborts["list"][0].clone(),
                w.read_aborts["watching-list"][0].clone(),
            ]
        });
        release.send(()).unwrap();
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert!(w.watching.loading);
            assert!(w.watching.current.is_empty());
        });
        finish_reads(&runtime, &reads);
        cx.run_until_parked();
        let retry = view.read_with(cx, |w, _| {
            w.watching
                .loading
                .then(|| w.read_aborts["watching-list"][0].clone())
        });
        if let Some(retry) = retry {
            finish_reads(&runtime, &[retry]);
            cx.run_until_parked();
        }
        view.read_with(cx, |w, _| {
            assert!(w.watching.loaded);
            assert!(!w.watching.loading);
            assert_eq!(w.watching.current.len(), 1);
            assert_eq!(w.watching.current[0].media_id, 8);
        });
    }

    #[gpui::test]
    #[cfg(feature = "fixture-runtime")]
    fn watching_scoped_refresh_preserves_unrelated_autosave(cx: &mut TestAppContext) {
        if super::super::workspace_tests::prompt_restore_tests::fixture_server("progress").is_none()
        {
            return;
        }
        let (core, runtime, _dir, db, profile) = account_fixture(7);
        let mut entry = playback_entry(9, None);
        entry.id = Some(90);
        entry.progress = 2;
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let list = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile.clone());
            w.offline = true;
            w.route = Route::Now;
            entry.progress = 1;
            w.list.replace(vec![entry], 0);
            w.refresh_watching_list(window, cx);
            w.read_aborts["watching-list"][0].clone()
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();
        let more = cx.debug_bounds("watching-more-9").unwrap().center();
        cx.simulate_click(more, Modifiers::default());
        let (scoped, due) = view.update_in(cx, |w, window, cx| {
            let edit = w.progress.edit(w.generation, 9).unwrap();
            assert_eq!(
                (edit.saved, edit.pending, edit.requires_review),
                (2, 3, false)
            );
            let due = edit.due.unwrap();
            let removed = w.progress.entry(w.generation, &playback_entry(8, None));
            assert!(removed.step(1, std::time::Instant::now()));
            w.refresh_scoped(vec![8], profile.id, window, cx);
            (w.read_aborts["list"][0].clone(), due)
        });
        finish_reads(&runtime, &[scoped]);
        cx.run_until_parked();
        view.update_in(cx, |w, window, cx| {
            let stale_review = super::super::state::ProgressReview {
                generation: w.generation,
                id: 8,
                progress: Some(0),
                identity: Some(EntryIdentity::Unverified),
            };
            w.resolve_progress_review(&stale_review, true, window, cx);
            assert_eq!(w.list.entry(9).unwrap().progress, 1);
            let edit = w.progress.edit(w.generation, 9).unwrap();
            assert_eq!(
                (edit.saved, edit.pending, edit.requires_review),
                (2, 3, false)
            );
            assert_eq!(edit.due, Some(due));
            let removed = w.progress.edit(w.generation, 8).unwrap();
            assert!(removed.requires_review);
            assert_eq!((removed.saved, removed.pending), (0, 1));
            assert!(removed.due.is_none());
        });
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(4));
        cx.run_until_parked();
        wait_for_watching_write(&view, cx, &runtime);
        assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
    }

    #[gpui::test]
    #[cfg(feature = "fixture-runtime")]
    fn watching_overlapping_save_keeps_unrelated_draft(cx: &mut TestAppContext) {
        if super::super::workspace_tests::prompt_restore_tests::fixture_server("progress").is_none()
        {
            return;
        }
        let (core, runtime, _dir, db, profile) = account_fixture(7);
        let entry = ListEntry {
            id: Some(90),
            progress: 2,
            ..playback_entry(9, None)
        };
        let stale = ListEntry {
            id: Some(100),
            progress: 1,
            ..playback_entry(10, None)
        };
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let due = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.offline = true;
            w.route = Route::Now;
            w.list.replace(
                vec![entry.clone(), stale.clone(), playback_entry(8, None)],
                0,
            );
            w.step_entry_progress(&entry, 1, window, cx);
            let current = ListEntry {
                progress: 5,
                ..stale
            };
            w.step_entry_progress(&current, 1, window, cx);
            w.save_progress(9, window, cx);
            let change = w.list.apply_scoped(&[8], vec![None], 0);
            w.apply_list_change(change);
            w.progress
                .reconcile_scoped_entries(w.generation, &w.list, &[8]);
            w.reconcile_open_entries(Some(&[8]), cx);
            w.progress.edit(w.generation, 10).unwrap().due
        });
        wait_for_watching_write(&view, cx, &runtime);
        view.read_with(cx, |w, _| {
            let saved = w.progress.edit(w.generation, 9).unwrap();
            assert_eq!((saved.saved, saved.pending), (3, 3));
            let edit = w.progress.edit(w.generation, 10).unwrap();
            assert_eq!(
                (edit.saved, edit.pending, edit.requires_review),
                (5, 6, false)
            );
            assert_eq!(edit.due, due);
        });
        assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 3);
    }

    #[gpui::test]
    fn watching_row_steps_from_latest_list_snapshot(cx: &mut TestAppContext) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        entry.progress = 4;
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let list = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            entry.progress = 1;
            w.list.replace(vec![entry], 0);
            w.refresh_watching_list(window, cx);
            w.read_aborts["watching-list"][0].clone()
        });
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();
        let more = cx.debug_bounds("watching-more-7").unwrap();
        cx.simulate_click(more.center(), Modifiers::default());
        view.read_with(cx, |w, _| {
            assert_eq!(w.progress.edit(w.generation, 7).unwrap().pending, 5);
        });
    }

    #[gpui::test]
    fn watching_stepper_press_does_not_edit_replacement(cx: &mut TestAppContext) {
        let (core, _runtime, _dir, _db, profile) = signed_in_fixture();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let mut original = playback_entry(7, None);
        original.id = Some(700);
        original.progress = 1;
        let mut replacement = original.clone();
        replacement.id = Some(701);
        replacement.progress = 4;
        cx.simulate_accessibility_activation();
        for selector in ["watching-more-7", "watching-less-7"] {
            view.update_in(cx, |w, _, cx| {
                w.profile = Some(profile.clone());
                w.route = Route::Now;
                w.watching.loaded = true;
                w.watching.replace_rows(vec![original.clone()]);
                cx.notify();
            });
            cx.run_until_parked();
            let button = cx.debug_bounds(selector).unwrap().center();
            cx.simulate_mouse_down(button, gpui::MouseButton::Left, Modifiers::default());
            let keystroke = cx.update(|_, app| {
                gpui::Keystroke::parse_with_os("space", app.platform_info().os).unwrap()
            });
            cx.simulate_event(gpui::KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            let label = if selector == "watching-more-7" {
                "One more episode"
            } else {
                "One less episode"
            };
            cx.update(|window, app| window.draw(app).clear(app));
            let tree = cx.take_accessibility_tree_update().unwrap();
            let (old, _) = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap();
            view.update_in(cx, |w, _, cx| {
                w.watching.replace_rows(vec![replacement.clone()]);
                cx.notify();
            });
            cx.run_until_parked();
            cx.simulate_mouse_up(button, gpui::MouseButton::Left, Modifiers::default());
            cx.simulate_event(gpui::KeyUpEvent { keystroke });
            cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
                action: gpui::AccessibleAction::Click,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node: *old,
                data: None,
            });
            view.read_with(cx, |w, _| {
                assert!(w.progress.edit(w.generation, 7).is_none())
            });
        }
        let stepper = cx.debug_bounds("watching-stepper-7").unwrap().center();
        cx.simulate_click(stepper, Modifiers::default());
        cx.simulate_keystrokes("right");
        view.read_with(cx, |w, _| {
            let edit = w.progress.edit(w.generation, 7).unwrap();
            assert_eq!(edit.identity, EntryIdentity::Remote(701));
            assert_eq!(edit.pending, 5);
        });
    }

    #[gpui::test]
    fn watching_controls_reject_expired_core_generation(cx: &mut TestAppContext) {
        let (core, runtime, dir, _db, profile) = signed_in_fixture();
        let session = core.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        view.update_in(cx, |w, _, cx| {
            let mut entry = playback_entry(7, None);
            entry.progress = 1;
            w.profile = Some(profile);
            w.route = Route::Now;
            w.watching.loaded = true;
            w.watching.replace_rows(vec![entry]);
            w.watching.launch_error = "Previous launch failed".into();
            w.library_files.replace(vec![LibraryFile {
                media_id: Some(7),
                episode: Some(2),
                path: dir.0.join("episode.mkv").to_string_lossy().into_owned(),
                ..Default::default()
            }]);
            cx.notify();
        });
        cx.run_until_parked();
        let play = cx.debug_bounds("watching-play-7").unwrap().center();
        cx.simulate_mouse_down(play, gpui::MouseButton::Left, Modifiers::default());
        runtime.block_on(session.logout()).unwrap();
        view.read_with(cx, |w, _| assert_ne!(session.generation(), w.generation));
        cx.simulate_mouse_up(play, gpui::MouseButton::Left, Modifiers::default());
        let more = cx.debug_bounds("watching-more-7").unwrap().center();
        cx.simulate_click(more, Modifiers::default());
        view.read_with(cx, |w, _| {
            assert_eq!(w.watching.launch_error, "Previous launch failed");
            assert!(w.progress.edit(w.generation, 7).is_none());
        });
    }

    #[gpui::test]
    fn watching_play_preserves_press_and_admitted_file(cx: &mut TestAppContext) {
        let (core, runtime, dir, _db, profile) = signed_in_fixture();
        let session = core.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let file = |name: &str| LibraryFile {
            media_id: Some(7),
            episode: Some(2),
            path: dir.0.join(name).to_string_lossy().into_owned(),
            ..Default::default()
        };
        let (launched, path) = tokio::sync::oneshot::channel();
        view.update_in(cx, |w, _, cx| {
            let mut entry = playback_entry(7, None);
            entry.progress = 1;
            w.profile = Some(profile);
            w.route = Route::Now;
            w.now_playing = Some(playing(7));
            w.watching.loaded = true;
            w.watching.replace_rows(vec![entry]);
            w.watching.launch_hook = Some(Box::new(move |path| {
                launched.send(path).unwrap();
                Ok(())
            }));
            w.library_files.replace(vec![file("old.mkv")]);
            cx.notify();
        });
        cx.run_until_parked();
        for (selector, redraw) in [("watching-play-7", true), ("watching-play-next", false)] {
            let button = cx.debug_bounds(selector).unwrap().center();
            cx.simulate_mouse_down(button, gpui::MouseButton::Left, Modifiers::default());
            if redraw {
                view.update_in(cx, |w, _, cx| {
                    w.library_files.replace(vec![file("new.mkv")]);
                    cx.notify();
                });
                cx.run_until_parked();
                cx.simulate_mouse_up(button, gpui::MouseButton::Left, Modifiers::default());
            } else {
                cx.update(|window, app| {
                    view.update(app, |w, _| {
                        w.library_files.replace(vec![file("latest.mkv")])
                    });
                    window.dispatch_event(
                        gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                            button: gpui::MouseButton::Left,
                            position: button,
                            modifiers: Modifiers::default(),
                            click_count: 1,
                        }),
                        app,
                    );
                });
            }
            view.read_with(cx, |w, _| assert!(w.watching.launch_hook.is_some()));
        }
        cx.update(|window, app| window.draw(app).clear(app));
        let play = cx.debug_bounds("watching-play-next").unwrap().center();
        cx.simulate_click(play, Modifiers::default());
        view.read_with(cx, |w, _| assert!(w.watching.launch_hook.is_none()));
        session.begin_login_intent();
        view.update_in(cx, |w, _, _| {
            w.library_files.replace(vec![file("after.mkv")])
        });
        let launched = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(4), path)
                .await
                .unwrap()
                .unwrap()
        });
        assert_eq!(launched, dir.0.join("latest.mkv"));
    }

    #[gpui::test]
    #[cfg(feature = "fixture-runtime")]
    fn watching_detected_update_keeps_displayed_entry(cx: &mut TestAppContext) {
        use super::super::fixture_server::{child_gate, serve};
        use serde_json::json;

        const CHILD: &str = "KURISU_WATCHING_UPDATE_TEST";
        if child_gate(CHILD).is_none() {
            let mut lookups = 0;
            let mut writes = 0;
            serve(
                std::thread::current().name().unwrap(),
                CHILD,
                "KURISU_FIXTURE_GRAPHQL",
                |mut http, _| {
                    let request = http.json();
                    let query = request["query"].as_str().unwrap();
                    let body = if query.contains("Viewer") {
                        json!({"data":{"Viewer":{"id":7,"name":"Fixture",
                            "mediaListOptions":{"scoreFormat":"POINT_100"}}}})
                    } else if query.contains("SaveMediaListEntry") {
                        assert_eq!(lookups, 2);
                        assert_eq!(request["variables"]["id"], 92);
                        assert_eq!(request["variables"]["progress"], 3);
                        writes += 1;
                        json!({"data":{"SaveMediaListEntry":{"id":92,"mediaId":9,
                            "status":"CURRENT","progress":3,"repeat":0,"score":0}}})
                    } else {
                        assert!(query.contains("mediaListEntry"), "{query}");
                        assert_eq!(request["variables"]["mediaId"], 9);
                        lookups += 1;
                        json!({"data":{"Media":{"id":9,"episodes":12,"mediaListEntry":{
                            "id":92,"mediaId":9,"status":"CURRENT","progress":2,"repeat":0,"score":0
                        }}}})
                    };
                    http.reply_json(200, body).unwrap();
                },
            );
            assert_eq!((lookups, writes), (2, 1));
            return;
        }

        let (core, runtime, _dir, db, profile) = account_fixture(7);
        let mut entry = playback_entry(9, None);
        entry.id = Some(91);
        entry.progress = 2;
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        view.update_in(cx, |w, _, _| {
            w.profile = Some(profile);
            w.offline = true;
            w.route = Route::Now;
            let mut now = playing(9);
            now.matched = Some(now.title.clone());
            now.episode = Some(3);
            w.now_playing = Some(now);
        });
        let refresh = |cx: &mut VisualTestContext| {
            let read = view.update_in(cx, |w, window, cx| {
                w.refresh_watching_list(window, cx);
                w.read_aborts["watching-list"][0].clone()
            });
            finish_reads(&runtime, &[read]);
            cx.run_until_parked();
            cx.debug_bounds("watching-update").unwrap().center()
        };
        let button = refresh(cx);
        cx.simulate_mouse_down(button, gpui::MouseButton::Left, Modifiers::default());
        entry.id = Some(90);
        db.upsert_entry(&entry).unwrap();
        let button = refresh(cx);
        view.read_with(cx, |w, _| assert_eq!(w.watching.current[0].id, Some(90)));
        cx.simulate_mouse_up(button, gpui::MouseButton::Left, Modifiers::default());
        view.read_with(cx, |w, _| assert_eq!(w.active_writes, 0));
        assert_eq!(db.get_entry(9).unwrap().unwrap().progress, 2);

        cx.simulate_click(button, Modifiers::default());
        wait_for_watching_write(&view, cx, &runtime);
        let replacement = db.get_entry(9).unwrap().unwrap();
        assert_eq!((replacement.id, replacement.progress), (Some(92), 2));
        view.read_with(cx, |w, _| assert!(!w.watching.update_error.is_empty()));

        let button = refresh(cx);
        cx.simulate_click(button, Modifiers::default());
        wait_for_watching_write(&view, cx, &runtime);
        let saved = db.get_entry(9).unwrap().unwrap();
        assert_eq!((saved.id, saved.progress), (Some(92), 3));
    }

    #[cfg(feature = "fixture-runtime")]
    fn wait_for_watching_write(
        view: &Entity<Workspace>,
        cx: &mut VisualTestContext,
        runtime: &tokio::runtime::Runtime,
    ) {
        use std::time::{Duration, Instant};

        assert_eq!(view.read_with(cx, |w, _| w.active_writes), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.read_with(cx, |w, _| w.active_writes != 0) {
            assert!(Instant::now() < deadline, "Watching write timed out");
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(2)).await });
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    fn watching_update_click_stays_bound_to_the_rendered_episode(cx: &mut TestAppContext) {
        let (core, _runtime, _dir, _db, profile) = signed_in_fixture();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let detected = |id| {
            let mut now = playing(id);
            now.matched = Some(now.title.clone());
            now.episode = Some(3);
            now
        };
        let update_node = |cx: &mut VisualTestContext| {
            cx.update(|window, app| window.draw(app).clear(app));
            cx.take_accessibility_tree_update()
                .unwrap()
                .nodes
                .into_iter()
                .find(|(_, node)| node.label() == Some("Update to Ep 3"))
                .unwrap()
        };
        let activate = |cx: &mut VisualTestContext, target_node| {
            cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
                action: gpui::AccessibleAction::Click,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node,
                data: None,
            });
        };
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            w.watching.replace_rows(vec![playback_entry(7, None)]);
            w.now_playing = Some(detected(7));
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        let (node_id, node) = update_node(cx);
        assert!(node.is_disabled());
        assert!(!node.supports_action(gpui::AccessibleAction::Click));
        assert!(!node.supports_action(gpui::AccessibleAction::Focus));
        let button = cx.debug_bounds("watching-update").unwrap().center();
        cx.simulate_click(button, Modifiers::default());
        activate(cx, node_id);
        view.update_in(cx, |w, _, cx| {
            assert_eq!(w.active_writes, 0);
            w.watching.replace_rows(
                [7, 8]
                    .map(|id| ListEntry {
                        id: Some(id + 100),
                        ..playback_entry(id, None)
                    })
                    .to_vec(),
            );
            cx.notify();
        });
        cx.run_until_parked();
        let (original_id, node) = update_node(cx);
        assert!(node.supports_action(gpui::AccessibleAction::Click));
        let button = cx.debug_bounds("watching-update").unwrap().center();
        cx.simulate_mouse_down(button, gpui::MouseButton::Left, Modifiers::default());
        let keystroke = cx.update(|_, app| {
            gpui::Keystroke::parse_with_os("space", app.platform_info().os).unwrap()
        });
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        view.update_in(cx, |w, _, _| {
            w.now_playing = Some(detected(8));
        });
        activate(cx, original_id);
        view.update_in(cx, |w, _, cx| {
            assert_eq!(w.active_writes, 0);
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_up(button, gpui::MouseButton::Left, Modifiers::default());
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        activate(cx, original_id);
        view.update_in(cx, |w, window, cx| {
            assert!(!w.watching.updating);
            assert_eq!(w.active_writes, 0);
            w.update_detected_episode(7, 3, EntryIdentity::Remote(107), window, cx);
            w.update_detected_episode(8, 2, EntryIdentity::Remote(108), window, cx);
            w.update_detected_episode(8, 3, EntryIdentity::Remote(107), window, cx);
            assert!(!w.watching.updating);
            assert!(w.progress.begin_delete(w.generation, 8));
            w.update_detected_episode(8, 3, EntryIdentity::Remote(108), window, cx);
            assert!(!w.watching.updating);
            w.progress.finish_delete(w.generation, 8, false);
        });
        let (current_id, node) = update_node(cx);
        assert!(node.supports_action(gpui::AccessibleAction::Click));
        activate(cx, current_id);
        view.read_with(cx, |w, _| assert!(w.watching.updating));
    }

    #[gpui::test]
    fn watching_removal_supersedes_playback_and_list_reads(cx: &mut TestAppContext) {
        let (core, runtime, _dir, db, profile) = signed_in_fixture();
        let mut entry = playback_entry(7, None);
        entry.status = "PAUSED".into();
        db.upsert_entry(&entry).unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Now;
            let mut now = playing(7);
            now.matched = Some("Synthetic title 7".into());
            now.episode = Some(3);
            w.now_playing = Some(now);
            w.list.replace(vec![entry.clone()], 0);
            w.refresh_watching_playback(window, cx);
            w.refresh_watching_list(window, cx);
        });
        let reads = view.read_with(cx, |w, _| {
            [
                w.read_aborts["watching-entry"][0].clone(),
                w.read_aborts["watching-list"][0].clone(),
            ]
        });
        finish_reads(&runtime, &reads);
        cx.run_until_parked();
        assert!(cx.debug_bounds("watching-update").is_some());
        view.update_in(cx, |w, _, cx| {
            assert!(w.watching.current.is_empty());
            assert_eq!(w.watching.playback(Some(7)).unwrap().status, "PAUSED");
            entry.status = "CURRENT".into();
            entry.progress = 2;
            db.upsert_entry(&entry).unwrap();
            let change = w.list.apply_one(entry.clone(), 0);
            w.apply_list_change(change);
            w.reconcile_open_entries(Some(&[7]), cx);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("watching-update").is_some());
        let reads = view.update_in(cx, |w, window, cx| {
            assert_eq!(w.watching.current.len(), 1);
            assert_eq!(w.watching.playback(Some(7)).unwrap().progress, 2);
            w.watching.playback_checked = false;
            w.refresh_watching_playback(window, cx);
            w.refresh_watching_list(window, cx);
            [
                w.read_aborts["watching-entry"][0].clone(),
                w.read_aborts["watching-list"][0].clone(),
            ]
        });
        finish_reads(&runtime, &reads);
        db.delete_entry(7).unwrap();
        view.update_in(cx, |w, _, cx| {
            let change = w.list.apply_scoped(&[7], vec![None], 0);
            w.apply_list_change(change);
            w.reconcile_open_entries(Some(&[7]), cx);
            assert!(w.watching.playback(Some(7)).is_none());
            assert!(w.watching.current.is_empty());
            cx.notify();
        });
        cx.run_until_parked();
        let list = view.read_with(cx, |w, _| w.read_aborts["watching-list"][0].clone());
        finish_reads(&runtime, &[list]);
        cx.run_until_parked();
        assert!(cx.debug_bounds("watching-update").is_none());
        assert!(cx.debug_bounds("watching-playback-title").is_some());
        view.read_with(cx, |w, _| {
            assert!(w.watching.playback(Some(7)).is_none());
            assert!(w.watching.current.is_empty());
        });
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
    fn watching_distinguishes_review_from_saving_in_accessibility(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Now;
            let rows = cover_rows(2);
            let edit = w.progress.entry(w.generation, &rows[0]);
            edit.requires_review = true;
            let edit = w.progress.entry(w.generation, &rows[1]);
            edit.in_flight = true;
            w.watching.replace_rows(rows);
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        for label in ["Review saved progress", "Saving progress"] {
            assert!(
                tree.nodes.iter().any(|(_, node)| {
                    node.label() == Some(label)
                        && node.role() == gpui::Role::Status
                        && !node.is_hidden()
                }),
                "{label}"
            );
        }
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
        let user = User {
            id: 47,
            ..Default::default()
        };
        db.replace_account("fixture-session", &user).unwrap();
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
            .block_on(core.bind_library_path(
                path.clone(),
                7,
                Some(-12),
                kurisu_core::LibraryBindingScope::File,
            ))
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core.clone(), window, cx, false)
        });
        workspace.update_in(cx, |workspace, _, _| {
            workspace.auth_intent += 1;
            workspace.restoring_session = false;
            workspace.profile = Some(user);
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
            .block_on(core.bind_library_path(
                path.clone(),
                8,
                Some(-12),
                kurisu_core::LibraryBindingScope::File,
            ))
            .unwrap();
        refresh(cx, &[(Some(8), Some(1))]);
        runtime
            .block_on(core.bind_library_path(
                path.clone(),
                8,
                Some(-11),
                kurisu_core::LibraryBindingScope::File,
            ))
            .unwrap();
        refresh(cx, &[(Some(8), Some(2))]);
        runtime.block_on(core.unbind_library_media(8)).unwrap();
        refresh(cx, &[(None, None)]);
        runtime
            .block_on(core.bind_library_path(
                path.clone(),
                8,
                Some(-11),
                kurisu_core::LibraryBindingScope::File,
            ))
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
    fn playback_snapshot_preserves_new_show_and_pending_lookup() {
        let mut state = WatchingState::default();
        state.begin_playback(Some(7));
        state.playback_entry = Some(playback_entry(7, None));
        state.begin_playback(Some(8));
        let mut current = playback_entry(8, None);
        current.progress = 4;
        state.playback_entry = Some(current.clone());
        let read = state.playback_request;
        assert!(!state.accept_playback_snapshot(7, Some(playback_entry(7, None))));
        assert!(!state.accept_playback_snapshot(7, None));
        assert_eq!(state.playback_request, read);
        assert_eq!(state.playback(Some(8)).unwrap().progress, 4);
        current.progress = 5;
        assert!(state.accept_playback_snapshot(8, Some(current)));
        assert_eq!(state.playback_request, read);
        assert_eq!(state.playback(Some(8)).unwrap().progress, 5);
        assert!(state.playback_loading);
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
        let layout = |cx: &VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                (
                    workspace.watching_cover_layout_key(),
                    workspace
                        .watching
                        .list_widget
                        .bounds_for_item(0)
                        .unwrap()
                        .size
                        .height,
                )
            })
        };
        let (without_error, header_height) = layout(cx);
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.watching.launch_error =
                "No video player could open this episode. ".repeat(12);
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        let (with_error, taller_header) = layout(cx);
        assert_ne!(without_error, with_error);
        assert!(taller_header > header_height);
        assert_measured_cover_owners(&workspace, cx);
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.watching.launch_error.clear();
            cx.notify();
        });
        cx.run_until_parked();
        cover_frames(cx, 5);
        assert_eq!(layout(cx), (without_error, header_height));
        assert_measured_cover_owners(&workspace, cx);
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

    #[gpui::test]
    fn watching_zero_total_stepper_and_fenced_play_and_retry_buttons(cx: &mut TestAppContext) {
        use gpui::AccessibleAction;

        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(gpui::size(px(1100.), px(900.)));
        let entry = ListEntry {
            id: Some(77),
            media_id: 9,
            status: "CURRENT".into(),
            progress: 0,
            media: Some(Media {
                id: 9,
                title_english: Some("Zero Watching Show".into()),
                episodes: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            w.route = Route::Now;
            w.watching.loaded = true;
            w.watching.error = "Could not load list".into();
            w.watching.replace_rows(vec![entry.clone()]);
            w.watching.playback_entry = Some(entry.clone());
            let mut np = playing(9);
            np.matched = Some("Zero Watching Show".into());
            w.now_playing = Some(np);
            w.library_files
                .replace(vec![kurisu_core::models::LibraryFile {
                    path: "/anime/ep01.mkv".into(),
                    media_id: Some(9),
                    matched: Some("Zero Watching Show".into()),
                    episode: Some(1),
                    bound: false,
                }]);
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();

        let find_node = |cx: &mut VisualTestContext, label: &str| {
            cx.update(|win, app| win.draw(app).clear(app));
            cx.run_until_parked();
            let tree = cx.take_accessibility_tree_update().unwrap();
            tree.nodes
                .into_iter()
                .find_map(|(_, node)| (node.label() == Some(label)).then_some(node))
                .unwrap_or_else(|| panic!("missing node {label}"))
        };

        let plus = find_node(cx, "One more episode");
        assert!(!plus.is_disabled());
        assert!(plus.supports_action(AccessibleAction::Click));

        let play_next = find_node(cx, "▶ Play Ep 1");
        let play_row = find_node(cx, "▶ Ep 1");
        let update_ep = find_node(cx, "Update to Ep 1");
        let retry = find_node(cx, "Retry");
        assert!(!play_next.is_disabled());
        assert!(!play_row.is_disabled());
        assert!(!update_ep.is_disabled());
        assert!(!retry.is_disabled());

        workspace.update_in(cx, |w, _, cx| {
            w.update_busy = true;
            cx.notify();
        });
        let play_next = find_node(cx, "▶ Play Ep 1");
        let play_row = find_node(cx, "▶ Ep 1");
        let update_ep = find_node(cx, "Update to Ep 1");
        let plus = find_node(cx, "One more episode");
        let retry = find_node(cx, "Retry");
        assert!(play_next.is_disabled());
        assert!(!play_next.supports_action(AccessibleAction::Click));
        assert!(play_row.is_disabled());
        assert!(!play_row.supports_action(AccessibleAction::Click));
        assert!(update_ep.is_disabled());
        assert!(!update_ep.supports_action(AccessibleAction::Click));
        assert!(plus.is_disabled());
        assert!(!plus.supports_action(AccessibleAction::Click));
        assert!(!retry.is_disabled());

        workspace.update_in(cx, |w, _, cx| {
            w.update_busy = false;
            w.watching.updating = true;
            cx.notify();
        });
        assert!(find_node(cx, "One more episode").is_disabled());
        assert!(find_node(cx, "Updating…").is_disabled());

        workspace.update_in(cx, |w, _, cx| {
            w.watching.updating = false;
            w.logout_confirming = true;
            cx.notify();
        });
        assert!(find_node(cx, "One more episode").is_disabled());
        assert!(find_node(cx, "Update to Ep 1").is_disabled());

        workspace.update_in(cx, |w, _, cx| {
            w.logout_confirming = false;
            w.stopping = true;
            cx.notify();
        });
        assert!(find_node(cx, "Update to Ep 1").is_disabled());
        assert!(find_node(cx, "One more episode").is_disabled());

        workspace.update_in(cx, |w, _, cx| {
            w.stopping = false;
            w.close_ready = true;
            cx.notify();
        });
        let retry = find_node(cx, "Retry");
        assert!(retry.is_disabled());
        assert!(!retry.supports_action(AccessibleAction::Click));
    }

    #[gpui::test]
    fn open_local_suppresses_late_launch_error_on_close_ready(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let (core, runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let file = kurisu_core::models::LibraryFile {
            path: "/anime/ep01.mkv".into(),
            media_id: Some(9),
            matched: Some("Show".into()),
            episode: Some(1),
            bound: false,
        };
        let entry = ListEntry {
            id: Some(77),
            media_id: 9,
            status: "CURRENT".into(),
            progress: 0,
            ..Default::default()
        };
        let done = Arc::new(AtomicBool::new(false));
        let done_hook = done.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            w.route = Route::Now;
            w.watching.replace_rows(vec![entry.clone()]);
            w.library_files.replace(vec![file.clone()]);
            w.watching.launch_hook = Some(Box::new(move |_| {
                let result = rx.recv().unwrap();
                done_hook.store(true, Ordering::SeqCst);
                result
            }));
            let generation = w.generation;
            open_local(w, WatchingPlay::Row(9), &file, generation, Some(47), cx);
            w.close_ready = true;
        });
        tx.send(Err("late tray close error".into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "launch hook did not complete");
            super::super::workspace_tests::settle(cx, &runtime);
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..8 {
            super::super::workspace_tests::settle(cx, &runtime);
            std::thread::sleep(Duration::from_millis(2));
        }
        workspace.read_with(cx, |w, _| {
            assert!(w.watching.launch_error.is_empty());
        });

        let (tx, rx) = std::sync::mpsc::channel();
        workspace.update_in(cx, |w, _, cx| {
            w.close_ready = false;
            w.watching.launch_hook = Some(Box::new(move |_| rx.recv().unwrap()));
            let generation = w.generation;
            open_local(w, WatchingPlay::Row(9), &file, generation, Some(47), cx);
        });
        tx.send(Err("visible player error".into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while workspace.read_with(cx, |w, _| w.watching.launch_error.is_empty()) {
            assert!(
                Instant::now() < deadline,
                "visible launch error did not surface"
            );
            super::super::workspace_tests::settle(cx, &runtime);
            std::thread::sleep(Duration::from_millis(2));
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.watching.launch_error, "visible player error");
        });
    }
}
