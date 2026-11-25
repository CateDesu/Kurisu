use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, Context, Entity, InteractiveElement, ListAlignment, ListOffset, ListState,
    PathPromptOptions, RenderImage, ScrollHandle, Subscription, WeakEntity, Window, div, img, list,
    prelude::*, px, rgb,
};
use kurisu_core::{
    LibraryBindingScope as BindScope, Platform, library_binding_folder,
    models::{LibraryBinding, LibraryFile, ListEntry, UnreadableFolder, WatchHistoryItem},
};

use super::{ModalLayer, Workspace, covers::CoverOwner, joined, state::compare_titles};
use crate::{
    components::{
        button, dialog, eyebrow, heading,
        input::{TextInput, TextInputEvent},
        scrollbar::{list_scrollbar, replace_list_rows},
    },
    contracts::Route,
    theme::{ACCENT, BASE, DIM, EDGE, ERROR, INK, PANEL, PANEL_2, mono_font},
};

struct Group {
    media_id: i64,
    title: String,
    entry: Option<ListEntry>,
    files: Vec<LibraryFile>,
    bound: bool,
}

impl Group {
    fn next_file(&self) -> Option<&LibraryFile> {
        let episode = self
            .entry
            .as_ref()
            .map_or(0, |entry| entry.progress)
            .checked_add(1)?;
        self.files.iter().find(|file| file.episode == Some(episode))
    }
}

enum LibraryRow {
    Group(Box<Group>),
    File(LibraryFile, i64, i64, String),
    UnmatchedHeader(usize),
    Unmatched(LibraryFile),
}
impl LibraryRow {
    fn key(&self) -> String {
        match self {
            Self::Group(group) => format!("g{}", group.media_id),
            Self::File(file, _, _, _) => format!("f{}", file.path),
            Self::UnmatchedHeader(_) => "u".into(),
            Self::Unmatched(file) => format!("u{}", file.path),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindMode {
    Link,
    Numbering,
}
struct BindDialog {
    id: u64,
    revision: u64,
    path: String,
    media_id: Option<i64>,
    title: String,
    mode: BindMode,
    scope: BindScope,
    request: u64,
    current: Option<LibraryBinding>,
    current_title: Option<String>,
    checked: bool,
    loading: bool,
    busy: bool,
    error: String,
    relink: Option<i64>,
    restored: bool,
    offset_edited: bool,
}
pub(super) struct LibraryDraft {
    bind: BindDialog,
    query: String,
    offset: String,
}

struct BindingCandidate {
    media_id: i64,
    label: gpui::SharedString,
    text: gpui::SharedString,
}

struct BindingCandidates {
    query: String,
    total: usize,
    rows: Vec<BindingCandidate>,
}

impl BindDialog {
    fn folder_allowed(&self, roots: &[String]) -> bool {
        library_binding_folder(&self.path, roots).is_some()
    }
    fn target(&self, roots: &[String], folders_loaded: bool) -> Option<String> {
        match self.scope {
            BindScope::File => Some(self.path.clone()),
            BindScope::Folder if folders_loaded => library_binding_folder(&self.path, roots),
            BindScope::Folder => None,
        }
    }
}

const OFFSET_INPUT: usize = 0;
const LINK_QUERY_INPUT: usize = 1;

pub(super) struct LibraryState {
    folders: Vec<String>,
    folders_loaded: bool,
    folders_error: String,
    files: Vec<LibraryFile>,
    unreadable: Vec<UnreadableFolder>,
    pub(super) scan_loaded: bool,
    pub(super) scanning: bool,
    pub(super) refresh_after_scan: bool,
    scan_error: String,
    list_error: String,
    entries: Vec<ListEntry>,
    entries_loaded: bool,
    rows: Arc<Vec<LibraryRow>>,
    expanded: HashSet<i64>,
    list_widget: ListState,
    history_widget: ListState,
    history_open: bool,
    history: Vec<WatchHistoryItem>,
    history_loading: bool,
    history_refresh_pending: bool,
    history_error: String,
    history_more: bool,
    history_request: u64,
    data_request: u64,
    pub(super) list_revision: u64,
    bind: Option<BindDialog>,
    pending_draft: Option<LibraryDraft>,
    bind_request: u64,
    link_query: Option<Entity<TextInput>>,
    binding_candidates: Option<Arc<BindingCandidates>>,
    link_cover_query: String,
    offset: Option<Entity<TextInput>>,
    binding_input_values: [String; 2],
    fenced_inputs: Option<[String; 2]>,
    subscriptions: Vec<Subscription>,
    remove_folder: Option<String>,
    unlink_media: Option<i64>,
    busy: bool,
    picking: bool,
    opening_file: bool,
    action_error: String,
    scanned_count: Option<usize>,
    rescan_feedback: bool,
    feedback_seq: u64,
    last_scan: Option<Instant>,
    scroll: ScrollHandle,
}
impl Default for LibraryState {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            folders_loaded: false,
            folders_error: String::new(),
            files: Vec::new(),
            unreadable: Vec::new(),
            scan_loaded: false,
            scanning: false,
            refresh_after_scan: false,
            scan_error: String::new(),
            list_error: String::new(),
            entries: Vec::new(),
            entries_loaded: false,
            rows: Arc::new(Vec::new()),
            expanded: HashSet::new(),
            list_widget: ListState::new(0, ListAlignment::Top, px(68.)),
            history_widget: ListState::new(0, ListAlignment::Top, px(56.)),
            history_open: false,
            history: Vec::new(),
            history_loading: false,
            history_refresh_pending: false,
            history_error: String::new(),
            history_more: false,
            history_request: 0,
            data_request: 0,
            list_revision: 0,
            bind: None,
            pending_draft: None,
            bind_request: 0,
            link_query: None,
            binding_candidates: None,
            link_cover_query: String::new(),
            offset: None,
            binding_input_values: ["0".into(), String::new()],
            fenced_inputs: None,
            subscriptions: Vec::new(),
            remove_folder: None,
            unlink_media: None,
            busy: false,
            picking: false,
            opening_file: false,
            action_error: String::new(),
            scanned_count: None,
            rescan_feedback: false,
            feedback_seq: 0,
            last_scan: None,
            scroll: ScrollHandle::new(),
        }
    }
}

impl LibraryState {
    pub(super) fn capture_fenced_inputs(&mut self, cx: &gpui::App) {
        if self.fenced_inputs.is_none() {
            let values = [&self.offset, &self.link_query];
            self.fenced_inputs = Some(std::array::from_fn(|index| {
                values[index]
                    .as_ref()
                    .map(|input| input.read(cx).value().to_string())
                    .unwrap_or_else(|| self.binding_input_values[index].clone())
            }));
        }
    }

    pub(super) fn clear_fenced_inputs(&mut self) {
        if let Some(values) = self.fenced_inputs.take() {
            self.binding_input_values = values;
        }
    }

    fn set_binding_input(&mut self, index: usize, value: String, cx: &mut Context<Workspace>) {
        self.binding_input_values[index] = value.clone();
        if let Some(values) = &mut self.fenced_inputs {
            values[index] = value.clone();
        }
        let inputs = [&self.offset, &self.link_query];
        if let Some(input) = inputs[index] {
            input.update(cx, |input, cx| input.set_value(value, cx));
        }
    }

    fn replace_entries(&mut self, entries: Vec<ListEntry>) {
        self.entries = entries;
        self.binding_candidates = None;
        self.entries_loaded = true;
        self.list_error.clear();
    }

    fn prepare_binding_candidates(&mut self, query: &str) {
        if self
            .binding_candidates
            .as_ref()
            .is_some_and(|candidates| candidates.query == query)
        {
            return;
        }
        let candidates = link_candidates(&self.entries, query);
        self.binding_candidates = Some(Arc::new(BindingCandidates {
            query: query.to_owned(),
            total: candidates.len(),
            rows: candidates
                .into_iter()
                .take(100)
                .map(|entry| {
                    let title = entry
                        .media
                        .as_ref()
                        .map(|media| media.display_title())
                        .unwrap_or_else(|| format!("#{}", entry.media_id));
                    BindingCandidate {
                        media_id: entry.media_id,
                        label: format!("Link {title}").into(),
                        text: format!("{title} · {}", super::state::status_label(&entry.status))
                            .into(),
                    }
                })
                .collect(),
        }));
    }

    fn scan_due(&self, list_revision: u64, now: Instant) -> bool {
        !self.scan_loaded
            || self.list_revision != list_revision
            || self
                .last_scan
                .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(60))
    }
}

impl Workspace {
    pub(super) fn library_modal_layer(&self) -> Option<ModalLayer> {
        if self.route != Route::Library {
            None
        } else if self.library_screen.remove_folder.is_some() {
            Some(ModalLayer::LibraryRemove)
        } else if self.library_screen.unlink_media.is_some() {
            Some(ModalLayer::LibraryUnlink)
        } else if self.library_screen.bind.is_some() {
            Some(ModalLayer::LibraryBind)
        } else {
            None
        }
    }

    pub(super) fn focus_library_dialog(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let input = match self.library_screen.bind.as_ref() {
            Some(bind) if bind.mode == BindMode::Link => self.library_screen.link_query.as_ref(),
            Some(bind) if bind.checked && !bind.busy => self.library_screen.offset.as_ref(),
            _ => None,
        };
        if let Some(input) = input {
            cx.focus_view(input, window);
            return self.modal_focus.contains(window, cx);
        }
        false
    }

    pub(super) fn dismiss_library_dialog(&mut self, cx: &mut Context<Self>) {
        let layer = self.library_modal_layer();
        if self.library_dismiss_disabled() {
            return;
        }
        match layer {
            Some(ModalLayer::LibraryRemove) if !self.library_screen.busy => {
                self.library_screen.remove_folder = None;
            }
            Some(ModalLayer::LibraryUnlink) if !self.library_screen.busy => {
                self.library_screen.unlink_media = None;
            }
            Some(ModalLayer::LibraryBind)
                if self
                    .library_screen
                    .bind
                    .as_ref()
                    .is_some_and(|bind| !bind.busy) =>
            {
                self.library_screen.bind = None;
                self.cancel_reads("library-binding");
            }
            _ => return,
        }
        cx.notify();
    }

    pub(super) fn enter_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            return;
        }
        if self.library_screen.link_query.is_none() {
            let query = cx.new(|cx| TextInput::new("", "Search your list…", window, cx));
            let offset = cx.new(|cx| TextInput::new("0", "Episode offset", window, cx));
            for (index, input) in [(LINK_QUERY_INPUT, &query), (OFFSET_INPUT, &offset)] {
                self.library_screen.subscriptions.push(cx.subscribe(
                    input,
                    move |this, input, event, cx| {
                        let TextInputEvent::Changed(value) = event else {
                            return;
                        };
                        let awaiting_offset = index == OFFSET_INPUT
                            && this.library_screen.bind.as_ref().is_some_and(|bind| {
                                bind.mode == BindMode::Numbering && !bind.checked
                            });
                        if this.library_binding_inputs_disabled() || awaiting_offset {
                            let library = &this.library_screen;
                            let values = library
                                .fenced_inputs
                                .as_ref()
                                .unwrap_or(&library.binding_input_values);
                            input
                                .update(cx, |input, cx| input.set_value(values[index].clone(), cx));
                            return;
                        }
                        this.library_screen.binding_input_values[index].clone_from(value);
                        if index == LINK_QUERY_INPUT {
                            if let Some(bind) = &mut this.library_screen.bind {
                                bind.revision = bind.revision.wrapping_add(1);
                            }
                            this.prefetch_library_link_covers(cx);
                        } else if let Some(bind) = &mut this.library_screen.bind {
                            bind.offset_edited = true;
                            bind.revision = bind.revision.wrapping_add(1);
                        }
                        cx.notify();
                    },
                ));
            }
            self.library_screen.link_query = Some(query);
            self.library_screen.offset = Some(offset);
            let weak = cx.weak_entity();
            self.library_screen
                .list_widget
                .set_scroll_handler(move |event, window, app| {
                    let _ = weak.update(app, |this, _| {
                        this.prefetch_library_covers(
                            event.visible_range.clone(),
                            window.scale_factor(),
                        )
                    });
                });
        }
        if let Some(mut draft) = self.library_screen.pending_draft.take() {
            draft.bind.request = 0;
            draft.bind.current = None;
            draft.bind.checked = false;
            draft.bind.loading = false;
            draft.bind.busy = false;
            draft.bind.relink = None;
            draft.bind.error.clear();
            draft.bind.restored = true;
            self.library_screen.link_cover_query = draft.query.trim().to_lowercase();
            self.library_screen
                .set_binding_input(LINK_QUERY_INPUT, draft.query, cx);
            self.library_screen
                .set_binding_input(OFFSET_INPUT, draft.offset, cx);
            self.library_screen.bind = Some(draft.bind);
            self.library_screen.bind_request += 1;
            self.load_library_binding(window, cx);
        }
        self.ensure_library_fresh(window, cx);
        self.refresh_library_history(window, cx);
        self.refresh_library_covers();
    }

    pub(super) fn take_library_draft(&mut self, cx: &gpui::App) -> Option<LibraryDraft> {
        let Some(mut bind) = self.library_screen.bind.take() else {
            return self.library_screen.pending_draft.take();
        };
        bind.busy = false;
        let query = self
            .library_screen
            .link_query
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let offset = if bind.offset_edited {
            self.library_screen.binding_input_values[OFFSET_INPUT].clone()
        } else {
            "0".into()
        };
        Some(LibraryDraft {
            bind,
            query,
            offset,
        })
    }

    pub(super) fn restore_library_draft(&mut self, draft: LibraryDraft) {
        self.library_screen.pending_draft = Some(draft);
    }

    pub(super) fn refresh_library_covers(&mut self) {
        let offset = self.library_screen.list_widget.logical_scroll_top().item_ix;
        self.prefetch_library_covers(offset..offset.saturating_add(36), self.cover_scale);
        self.refresh_library_link_covers();
    }

    fn prefetch_library_link_covers(&mut self, cx: &gpui::App) {
        let query = self
            .library_screen
            .link_query
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        self.library_screen.link_cover_query = query.clone();
        self.prefetch_library_link_covers_for(&query);
    }

    fn refresh_library_link_covers(&mut self) {
        let query = self.library_screen.link_cover_query.clone();
        self.prefetch_library_link_covers_for(&query);
    }

    fn prefetch_library_link_covers_for(&mut self, query: &str) {
        if self
            .library_screen
            .bind
            .as_ref()
            .is_none_or(|bind| bind.mode != BindMode::Link)
        {
            return;
        }
        let urls: Vec<_> = link_candidates(&self.library_screen.entries, query)
            .into_iter()
            .take(100)
            .filter_map(|entry| {
                entry.media.as_ref().and_then(|media| {
                    media
                        .cover_medium
                        .as_ref()
                        .map(|url| (entry.media_id, url.clone()))
                })
            })
            .collect();
        self.replace_cover_region(
            CoverOwner::LibraryDialog,
            urls.into_iter()
                .map(|(id, url)| ((id, 32, 44), url))
                .collect(),
            Vec::new(),
        );
    }

    fn prefetch_library_covers(&mut self, range: std::ops::Range<usize>, scale: f32) {
        self.ensure_cover_scale(scale);
        let start = range.start.saturating_sub(6);
        let end = range
            .end
            .saturating_add(10)
            .min(self.library_screen.rows.len());
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(end)..end {
            if let LibraryRow::Group(group) = &self.library_screen.rows[index]
                && let Some(url) = group
                    .entry
                    .as_ref()
                    .and_then(|entry| entry.media.as_ref())
                    .and_then(|media| media.cover_medium.as_ref())
            {
                let cover = ((group.media_id, 46, 63), url.clone());
                if range.contains(&index) {
                    visible.push(cover);
                } else {
                    nearby.push(cover);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Library), visible, nearby);
    }

    fn rebuild_library_rows(&mut self) {
        let rows = group_library_rows(
            &self.library_screen.files,
            &self.library_screen.entries,
            &self.library_screen.expanded,
        );
        replace_list_rows(
            &self.library_screen.list_widget,
            &self.library_screen.rows,
            &rows,
            px(68.),
            LibraryRow::key,
        );
        self.library_screen.rows = Arc::new(rows);
        self.refresh_library_covers();
    }

    pub(super) fn refresh_library_entries(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        self.library_screen.scan_loaded = false;
        if self.library_screen.scanning || self.writes_fenced() {
            self.library_screen.refresh_after_scan = true;
            return;
        }
        if self.route == Route::Library {
            self.refresh_library_data(window, cx);
        }
    }

    pub(super) fn ensure_library_fresh(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.profile.is_none() || self.library_screen.scanning {
            return;
        }
        if !self
            .library_screen
            .scan_due(self.list.revision(), Instant::now())
        {
            return;
        }
        if self.writes_fenced() {
            self.library_screen.refresh_after_scan = true;
            return;
        }
        self.refresh_library_data(window, cx);
    }

    fn refresh_library_data(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping
            || self.writes_fenced()
            || self.profile.is_none()
            || self.library_screen.scanning
        {
            return;
        }
        self.library_screen.refresh_after_scan = false;
        self.library_screen.scanning = true;
        self.library_screen.scan_error.clear();
        self.library_screen.data_request += 1;
        let request = self.library_screen.data_request;
        let list_revision = self.list.revision();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            let (folders, entries, scan) = tokio::join!(
                worker.get_library_folders(),
                worker.local_entries(),
                worker.scan_library()
            );
            (folders, entries, scan)
        });
        self.replace_reads("library-data", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await.map_err(|error| error.to_string());
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.library_screen.data_request != request
                {
                    return;
                }
                this.library_screen.scanning = false;
                let again = std::mem::take(&mut this.library_screen.refresh_after_scan);
                match result {
                    Ok((folders, entries, scan)) => {
                        match folders {
                            Ok(folders) => {
                                this.library_screen.folders = folders;
                                this.library_screen.folders_loaded = true;
                                this.library_screen.folders_error.clear();
                                this.resume_library_binding(window, cx);
                            }
                            Err(error) => this.library_screen.folders_error = error,
                        }
                        match entries {
                            Ok(entries) => {
                                this.library_screen.replace_entries(entries);
                            }
                            Err(error) => this.library_screen.list_error = error,
                        }
                        match scan {
                            Ok(scan) => {
                                this.library_screen.files = scan.files.clone();
                                this.library_files.replace(scan.files);
                                this.library_screen.unreadable = scan.unreadable;
                                this.library_screen.scan_loaded = true;
                                this.library_screen.list_revision = list_revision;
                                this.library_screen.last_scan = Some(Instant::now());
                                this.library_screen.scan_error.clear();
                                if std::mem::take(&mut this.library_screen.rescan_feedback) {
                                    this.library_screen.scanned_count =
                                        Some(this.library_screen.files.len());
                                    this.library_screen.feedback_seq += 1;
                                    let seq = this.library_screen.feedback_seq;
                                    cx.spawn_in(window, async move |this, cx| {
                                        cx.background_executor()
                                            .timer(Duration::from_secs(3))
                                            .await;
                                        let _ = this.update_in(cx, |this, _, cx| {
                                            if this.generation == generation
                                                && this.profile.as_ref().map(|user| user.id)
                                                    == account
                                                && this.library_screen.feedback_seq == seq
                                            {
                                                this.library_screen.scanned_count = None;
                                                cx.notify();
                                            }
                                        });
                                    })
                                    .detach();
                                }
                            }
                            Err(error) => {
                                this.library_screen.scan_error = error;
                                this.library_screen.rescan_feedback = false;
                            }
                        }
                        this.rebuild_library_rows();
                    }
                    Err(error) => {
                        this.library_screen.scan_error = error;
                        this.library_screen.rescan_feedback = false;
                    }
                }
                if again || this.list.revision() != list_revision {
                    if this.writes_fenced() {
                        this.library_screen.scan_loaded = false;
                        this.library_screen.refresh_after_scan = true;
                    } else {
                        this.refresh_library_data(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn pick_library_folder(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.library_controls_disabled(None)
            || self.library_screen.busy
            || self.library_screen.picking
        {
            return;
        }
        self.library_screen.picking = true;
        cx.notify();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose anime folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let selected = receiver.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                {
                    return;
                }
                this.library_screen.picking = false;
                cx.notify();
                if this.library_controls_disabled(None) {
                    return;
                }
                match selected {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.add_library_folder(
                                path.to_string_lossy().into_owned(),
                                window,
                                cx,
                            );
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        this.library_screen.action_error = error.to_string();
                    }
                    Err(error) => {
                        this.library_screen.action_error = error.to_string();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn add_library_folder(
        &mut self,
        path: String,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.library_screen.busy || self.library_controls_disabled(None) {
            return;
        }
        self.library_screen.busy = true;
        self.library_screen.action_error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.add_library_folder(path).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    this.library_screen.busy = false;
                    match result {
                        Ok(folders) => {
                            this.library_screen.folders = folders;
                            this.refresh_library_entries(window, cx);
                        }
                        Err(error) => this.library_screen.action_error = error,
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_library_folder(&mut self, path: String, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.busy
            || self.library_controls_disabled(Some(ModalLayer::LibraryRemove))
            || self.library_screen.remove_folder.as_deref() != Some(path.as_str())
        {
            return;
        }
        self.library_screen.busy = true;
        self.library_screen.action_error.clear();
        self.active_writes += 1;
        cx.notify();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.remove_library_folder(path).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    this.library_screen.busy = false;
                    this.library_screen.remove_folder = None;
                    match result {
                        Ok(folders) => {
                            this.library_screen.folders = folders;
                            this.refresh_library_entries(window, cx);
                        }
                        Err(error) => this.library_screen.action_error = error,
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_library_bind(
        &mut self,
        path: String,
        media: Option<(i64, String)>,
        mode: BindMode,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.library_controls_disabled(None) || self.library_screen.busy {
            return;
        }
        self.library_bind_id = self.library_bind_id.wrapping_add(1);
        let dialog = BindDialog {
            id: self.library_bind_id,
            revision: 0,
            path,
            media_id: media.as_ref().map(|value| value.0),
            title: media.map(|value| value.1).unwrap_or_default(),
            mode,
            scope: BindScope::File,
            request: 0,
            current: None,
            current_title: None,
            checked: false,
            loading: false,
            busy: false,
            error: String::new(),
            relink: None,
            restored: false,
            offset_edited: false,
        };
        let allowed = self.library_screen.folders_loaded
            && dialog.folder_allowed(&self.library_screen.folders);
        let mut dialog = dialog;
        if mode == BindMode::Link && allowed {
            dialog.scope = BindScope::Folder;
        }
        self.library_screen.bind_request += 1;
        self.library_screen.bind = Some(dialog);
        self.library_screen
            .set_binding_input(OFFSET_INPUT, "0".into(), cx);
        self.library_screen
            .set_binding_input(LINK_QUERY_INPUT, String::new(), cx);
        self.prefetch_library_link_covers(cx);
        self.load_library_binding(window, cx);
    }

    fn change_bind_scope(&mut self, scope: BindScope, window: &Window, cx: &mut Context<Self>) {
        if self.library_controls_disabled(Some(ModalLayer::LibraryBind)) {
            return;
        }
        if let Some(bind) = self.library_screen.bind.as_mut() {
            if bind.busy
                || bind.scope == scope
                || scope == BindScope::Folder
                    && (!self.library_screen.folders_loaded
                        || !bind.folder_allowed(&self.library_screen.folders))
            {
                return;
            }
            bind.scope = scope;
            bind.revision = bind.revision.wrapping_add(1);
            bind.restored = false;
            bind.offset_edited = false;
            bind.current = None;
            bind.checked = false;
            bind.relink = None;
            self.library_screen
                .set_binding_input(OFFSET_INPUT, "0".into(), cx);
            self.load_library_binding(window, cx);
        }
    }

    pub(super) fn resume_library_binding(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.bind.as_ref().is_some_and(|bind| {
            !bind.checked
                && !bind.loading
                && !bind.busy
                && bind.error.is_empty()
                && (bind.scope == BindScope::File || self.library_screen.folders_loaded)
        }) {
            self.load_library_binding(window, cx);
        }
    }

    fn load_library_binding(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.edits_fenced()
            || self.profile.is_none()
            || self.library_modal_layer() != Some(ModalLayer::LibraryBind)
        {
            return;
        }
        let Some(bind) = self.library_screen.bind.as_mut() else {
            return;
        };
        let folders_loaded = self.library_screen.folders_loaded;
        let roots = &self.library_screen.folders;
        let target_changed =
            bind.scope == BindScope::Folder && folders_loaded && !bind.folder_allowed(roots);
        if target_changed {
            bind.scope = BindScope::File;
            bind.offset_edited = false;
            bind.restored = false;
        }
        let Some(target) = bind.target(roots, folders_loaded) else {
            return;
        };
        bind.request += 1;
        bind.loading = true;
        bind.checked = false;
        bind.current_title = None;
        bind.error.clear();
        bind.relink = None;
        let request = bind.request;
        let dialog_id = self.library_screen.bind_request;
        let mode = bind.mode;
        if target_changed {
            self.library_screen
                .set_binding_input(OFFSET_INPUT, "0".into(), cx);
        }
        if mode == BindMode::Numbering
            && let Some(input) = &self.library_screen.offset
        {
            input.update(cx, |input, cx| input.set_disabled(true, cx));
        }
        cx.notify();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            if mode == BindMode::Link {
                Ok(worker
                    .library_binding_for(target)
                    .await?
                    .map(LibraryBinding::from))
            } else {
                worker.library_binding_details(target).await
            }
        });
        self.replace_reads("library-binding", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.library_screen.bind_request != dialog_id
                {
                    return;
                }
                let fenced = this.edits_fenced();
                let Some(bind) = this.library_screen.bind.as_mut() else {
                    return;
                };
                if bind.request != request {
                    return;
                }
                bind.loading = false;
                if fenced {
                    cx.notify();
                    return;
                }
                let offset = match result {
                    Ok(current) => {
                        bind.current = current;
                        bind.checked = true;
                        (bind.mode == BindMode::Numbering && !bind.offset_edited)
                            .then(|| current.map_or(0, |item| item.episode_offset).to_string())
                    }
                    Err(error) => {
                        bind.error = error;
                        None
                    }
                };
                let current = bind.checked.then_some(bind.current).flatten();
                if let Some(offset) = offset {
                    this.library_screen
                        .set_binding_input(OFFSET_INPUT, offset, cx);
                }
                if let Some(current) = current {
                    this.load_library_binding_title(current.media_id, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_library_binding_title(
        &mut self,
        media_id: i64,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.stopping || self.edits_fenced() {
            return;
        }
        let Some(bind) = self.library_screen.bind.as_ref() else {
            return;
        };
        let request = bind.request;
        let dialog_id = self.library_screen.bind_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_media(media_id).await });
        self.replace_reads("library-binding", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.stopping
                    || !scope_matches(this, generation, account)
                    || this.library_screen.bind_request != dialog_id
                {
                    return;
                }
                let Some(bind) = this.library_screen.bind.as_mut() else {
                    return;
                };
                if bind.request != request
                    || bind
                        .current
                        .is_none_or(|current| current.media_id != media_id)
                {
                    return;
                }
                if let Ok(media) = result {
                    bind.current_title = Some(media.display_title());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn save_library_bind(&mut self, media_id: i64, window: &Window, cx: &mut Context<Self>) {
        if self.library_binding_inputs_disabled() {
            return;
        }
        let Some(bind) = self.library_screen.bind.as_mut() else {
            return;
        };
        if !bind.checked || bind.busy {
            return;
        }
        let Some(target) = bind.target(
            &self.library_screen.folders,
            self.library_screen.folders_loaded,
        ) else {
            return;
        };
        if bind.mode == BindMode::Numbering
            && bind
                .current
                .is_some_and(|current| current.media_id != media_id)
        {
            bind.error = "This location is linked to a different show. Select this file only or relink it first.".into();
            cx.notify();
            return;
        }
        if bind.mode == BindMode::Link
            && bind
                .current
                .is_some_and(|current| current.media_id != media_id)
            && bind.relink != Some(media_id)
        {
            bind.relink = Some(media_id);
            bind.revision = bind.revision.wrapping_add(1);
            cx.notify();
            return;
        }
        let raw = self
            .library_screen
            .offset
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let Ok(offset) = raw.parse::<i64>() else {
            bind.error = "Enter a whole-number offset between -9999 and 9999.".into();
            cx.notify();
            return;
        };
        if !(-9999..=9999).contains(&offset) {
            bind.error = "Enter a whole-number offset between -9999 and 9999.".into();
            cx.notify();
            return;
        }
        if bind.id == 0 {
            self.library_bind_id = self.library_bind_id.wrapping_add(1);
            bind.id = self.library_bind_id;
        }
        bind.revision = bind.revision.wrapping_add(1);
        let bind_id = bind.id;
        let save_revision = bind.revision;
        let scope = bind.scope;
        bind.busy = true;
        bind.error.clear();
        if let Some(input) = &self.library_screen.offset {
            input.update(cx, |input, cx| input.set_disabled(true, cx));
        }
        self.active_writes += 1;
        cx.notify();
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    worker
                        .bind_library_path(target, media_id, Some(offset), scope)
                        .await
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.library_screen.bind.as_ref().is_some_and(|bind| {
                    bind.id == bind_id && bind.revision == save_revision && bind.busy
                }) {
                    if let Some(bind) = this.library_screen.bind.as_mut() {
                        bind.busy = false;
                    }
                    match result {
                        Ok(()) => {
                            this.library_screen.bind = None;
                            this.cancel_reads("library-binding");
                            this.refresh_library_entries(window, cx);
                        }
                        Err(error) => {
                            if let Some(bind) = this.library_screen.bind.as_mut() {
                                bind.error = error;
                            }
                            let disabled = this.library_binding_inputs_disabled();
                            if let Some(input) = &this.library_screen.offset {
                                input.update(cx, |input, cx| input.set_disabled(disabled, cx));
                            }
                        }
                    }
                } else if result.is_ok() {
                    // Retire the saved draft only when it still matches the submitted revision.
                    if this.library_screen.bind.as_ref().is_some_and(|bind| {
                        bind.id == bind_id && bind.revision == save_revision && !bind.busy
                    }) {
                        this.library_screen.bind = None;
                        this.cancel_reads("library-binding");
                        this.refresh_library_entries(window, cx);
                    } else {
                        if this
                            .library_screen
                            .pending_draft
                            .as_ref()
                            .is_some_and(|draft| {
                                draft.bind.id == bind_id && draft.bind.revision == save_revision
                            })
                        {
                            this.library_screen.pending_draft = None;
                        }
                        this.library_drafts.retain(|_, draft| {
                            !(draft.bind.id == bind_id && draft.bind.revision == save_revision)
                        });
                        if this.profile.is_some() {
                            this.refresh_library_entries(window, cx);
                        }
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn unlink_library_media(&mut self, id: i64, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.busy
            || self.library_controls_disabled(Some(ModalLayer::LibraryUnlink))
            || self.library_screen.unlink_media != Some(id)
        {
            return;
        }
        self.library_screen.busy = true;
        self.active_writes += 1;
        cx.notify();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.unbind_library_media(id).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
                    this.library_screen.busy = false;
                    this.library_screen.unlink_media = None;
                    match result {
                        Ok(()) => {
                            this.refresh_library_entries(window, cx);
                        }
                        Err(error) => this.library_screen.action_error = error,
                    }
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn refresh_library_history(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.history_open {
            self.load_library_history(false, window, cx);
        }
    }

    fn load_library_history(&mut self, more: bool, window: &Window, cx: &mut Context<Self>) {
        const PAGE_SIZE: usize = 50;

        if self.stopping
            || self.writes_fenced()
            || self.profile.is_none()
            || self.core.generation() != self.generation
        {
            return;
        }
        if self.library_screen.history_loading {
            self.library_screen.history_refresh_pending |= !more;
            return;
        }
        if more && !self.library_screen.history_more {
            return;
        }
        let tail = self.library_screen.history.last().map(|item| item.id);
        self.library_screen.history_request += 1;
        let request = self.library_screen.history_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.library_screen.history_loading = true;
        self.library_screen.history_error.clear();
        cx.notify();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            let mut before = if more { tail } else { None };
            let oldest = tail.filter(|_| !more);
            let mut history = Vec::new();
            loop {
                if worker.generation() != generation {
                    return Err("Account changed".into());
                }
                let mut page = worker
                    .get_watch_history(Some(PAGE_SIZE as i64 + 1), before)
                    .await?;
                if !page.is_empty() && worker.saved_account_id().await != account {
                    return Err("Account changed".into());
                }
                let has_more = page.len() > PAGE_SIZE;
                page.truncate(PAGE_SIZE);
                // Re-read through the oldest loaded row, keeping at least one page.
                let keep = oldest
                    .filter(|_| !history.is_empty())
                    .map_or(page.len(), |id| page.partition_point(|item| item.id >= id));
                let passed_oldest = keep < page.len();
                page.truncate(keep);
                history.extend(page);
                before = history.last().map(|item| item.id);
                if passed_oldest || !has_more || oldest.is_none_or(|id| before <= Some(id)) {
                    return Ok((history, has_more || passed_oldest));
                }
            }
        });
        self.replace_reads("library-history", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.library_screen.history_request != request
                {
                    return;
                }
                this.library_screen.history_loading = false;
                if this.core.generation() != generation {
                    return;
                }
                let refresh_pending =
                    std::mem::take(&mut this.library_screen.history_refresh_pending);
                if refresh_pending && !more {
                    this.load_library_history(false, window, cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok((rows, has_more)) => {
                        let old = this.library_screen.history_widget.logical_scroll_top();
                        let history = &mut this.library_screen.history;
                        let at_end = !history.is_empty() && old.item_ix >= history.len();
                        let anchor = history.get(old.item_ix).or_else(|| history.last());
                        let anchor = anchor.map(|item| item.id);
                        this.library_screen.history_more = has_more;
                        if more {
                            history.extend(rows);
                        } else {
                            *history = rows;
                        }
                        let widget = &this.library_screen.history_widget;
                        widget.reset_with_uniform_height(history.len(), px(56.));
                        if at_end && !more {
                            widget.scroll_to_end();
                        } else if let Some(id) = anchor {
                            let index = history.partition_point(|item| item.id > id);
                            widget.scroll_to(ListOffset {
                                item_ix: index.min(history.len().saturating_sub(1)),
                                offset_in_item: if history
                                    .get(index)
                                    .is_some_and(|item| item.id == id)
                                {
                                    old.offset_in_item
                                } else {
                                    px(0.)
                                },
                            });
                        }
                    }
                    Err(error) => this.library_screen.history_error = error,
                }
                if refresh_pending {
                    this.load_library_history(false, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_library(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let compact = f32::from(window.viewport_size().width) <= 960.;
        let controls_disabled = self.library_controls_disabled(None);
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let mut page = div()
            .id("library-page")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.library_screen.scroll)
            .px(px(if compact { 20. } else { 28. }))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(super::watching::route_header(
                "Your collection",
                "Library",
                "Anime on your device",
                Some(
                    button(
                        "library-rescan",
                        if self.library_screen.scanning {
                            "Scanning…"
                        } else {
                            "↻ Rescan"
                        },
                        controls_disabled
                            || self.library_screen.scanning
                            || self.library_screen.folders.is_empty(),
                    )
                    .border_color(rgb(PANEL_2))
                    .rounded(px(4.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !scope_matches(this, generation, account)
                            || this.library_controls_disabled(None)
                            || this.library_screen.scanning
                            || this.library_screen.folders.is_empty()
                        {
                            return;
                        }
                        this.library_screen.rescan_feedback = true;
                        this.refresh_library_data(window, cx)
                    }))
                    .into_any(),
                ),
                compact,
                12.,
            ));
        if let Some(count) = self.library_screen.scanned_count {
            page = page.child(
                div()
                    .text_color(rgb(ACCENT))
                    .text_size(px(11.))
                    .child(format!("Scan finished · {count} files")),
            );
        }
        for error in [
            &self.library_screen.action_error,
            &self.library_screen.scan_error,
            &self.library_screen.list_error,
            &self.library_screen.folders_error,
        ] {
            if !error.is_empty() {
                page = page.child(div().text_color(rgb(ERROR)).child(error.clone()));
            }
        }
        if !self.library_screen.unreadable.is_empty() {
            let mut note = div()
                .border_1()
                .border_color(rgb(EDGE))
                .p(px(8.))
                .text_color(rgb(DIM))
                .flex()
                .flex_col()
                .child("Some paths could not be read. Files may be missing from this scan.");
            for unreadable in &self.library_screen.unreadable {
                note = note.child(format!("{}: {}", unreadable.path, unreadable.error));
            }
            page = page.child(note);
        }
        let mut folders = div().flex().flex_col().gap(px(6.)).mt(px(-4.)).child(
            div()
                .id("library-folders-heading")
                .debug_selector(|| "library-folders-heading".into())
                .flex()
                .items_center()
                .gap(px(10.))
                .child(heading("Folders"))
                .child(
                    div()
                        .id("library-folders-rule")
                        .debug_selector(|| "library-folders-rule".into())
                        .flex_1()
                        .h(px(1.))
                        .bg(rgb(EDGE))
                        .mt(px(6.)),
                ),
        );
        if self.library_screen.folders.is_empty() {
            folders = folders.child(
                div()
                    .id("library-empty-hint")
                    .debug_selector(|| "library-empty-hint".into())
                    .text_color(rgb(DIM))
                    .text_size(px(14.))
                    .child(if self.library_screen.folders_loaded {
                        "No library folders yet. Add the folders where your anime files live."
                    } else {
                        "Loading folders…"
                    }),
            );
        } else {
            for (index, folder) in self.library_screen.folders.iter().enumerate() {
                let path = folder.clone();
                folders = folders.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(EDGE))
                        .p(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .font(mono_font())
                                .whitespace_normal()
                                .child(folder.clone()),
                        )
                        .child(
                            button(
                                format!("library-remove-folder-{index}"),
                                "Remove",
                                controls_disabled || self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !scope_matches(this, generation, account)
                                        || this.library_controls_disabled(None)
                                        || this.library_screen.busy
                                    {
                                        return;
                                    }
                                    this.library_screen.remove_folder = Some(path.clone());
                                    cx.notify();
                                },
                            )),
                        ),
                );
            }
        }
        folders = folders.child(
            div().flex().child(
                button(
                    "library-add-folder",
                    "+ Add folder",
                    controls_disabled || self.library_screen.busy || self.library_screen.picking,
                )
                .debug_selector(|| "library-add-folder".into())
                .border_color(rgb(PANEL_2))
                .rounded(px(4.))
                .py(px(3.))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if scope_matches(this, generation, account) && !this.library_screen.busy {
                        this.pick_library_folder(window, cx);
                    }
                })),
            ),
        );
        page = page.child(folders);
        if !self.library_screen.folders.is_empty()
            && self.library_screen.scanning
            && !self.library_screen.scan_loaded
        {
            page = page.child(
                div()
                    .py(px(40.))
                    .text_color(rgb(DIM))
                    .flex()
                    .justify_center()
                    .child("Scanning…"),
            );
        } else if !self.library_screen.folders.is_empty()
            && self.library_screen.scan_loaded
            && self.library_screen.rows.is_empty()
        {
            page = page.child(
                div()
                    .py(px(40.))
                    .text_color(rgb(DIM))
                    .flex()
                    .justify_center()
                    .child("No video files found in those folders."),
            );
        } else if !self.library_screen.folders.is_empty() {
            let weak = cx.weak_entity();
            let scroll_weak = weak.clone();
            page = page.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(230.))
                    .flex()
                    .flex_col()
                    .child(
                        list(
                            self.library_screen.list_widget.clone(),
                            move |index, _, app| {
                                weak.read_with(app, |this, _| {
                                    let Some(row) = this.library_screen.rows.get(index) else {
                                        return div().into_any();
                                    };
                                    let cover = match row {
                                        LibraryRow::Group(group) => this
                                            .cover_images
                                            .get(&(group.media_id, 46, 63))
                                            .cloned(),
                                        _ => None,
                                    };
                                    let expanded = match row {
                                        LibraryRow::Group(group) => {
                                            this.library_screen.expanded.contains(&group.media_id)
                                        }
                                        _ => false,
                                    };
                                    render_library_row(row, cover, expanded, weak.clone(), this)
                                })
                                .unwrap_or_else(|_| div().into_any())
                            },
                        )
                        .flex_1(),
                    )
                    .child(list_scrollbar(
                        "library-files-scrollbar",
                        "Library files scroll position",
                        self.library_screen.list_widget.clone(),
                        move |first, window, app| {
                            let _ = scroll_weak.update(app, |this, cx| {
                                if scope_matches(this, generation, account) {
                                    this.prefetch_library_covers(
                                        first..first.saturating_add(24),
                                        window.scale_factor(),
                                    );
                                    cx.notify();
                                }
                            });
                        },
                    )),
            );
        }
        let history_open = self.library_screen.history_open;
        page = page.child(
            div()
                .id("library-history-section")
                .debug_selector(|| "library-history-section".into())
                .mt(px(12.))
                .border_t_1()
                .border_color(rgb(EDGE))
                .pt(px(16.))
                .flex()
                .child(
                    button(
                        "library-history-toggle",
                        if history_open {
                            "▾ Watch history"
                        } else {
                            "▸ Watch history"
                        },
                        false,
                    )
                    .debug_selector(|| "library-history-toggle".into())
                    .bg(rgb(BASE))
                    .border_color(rgb(BASE))
                    .px(px(0.))
                    .py(px(0.))
                    .text_color(rgb(DIM))
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !scope_matches(this, generation, account) {
                            return;
                        }
                        this.library_screen.history_open = !this.library_screen.history_open;
                        this.refresh_library_history(window, cx);
                        cx.notify();
                    })),
                ),
        );
        if history_open {
            page = page
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .text_size(px(11.))
                        .child("Playback recorded on this device."),
                )
                .child(
                    button(
                        "library-history-refresh",
                        "Refresh history",
                        self.library_screen.history_loading,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.load_library_history(false, window, cx);
                        }
                    })),
                );
            if !self.library_screen.history_error.is_empty() {
                page = page.child(
                    div()
                        .text_color(rgb(ERROR))
                        .child(self.library_screen.history_error.clone()),
                );
            }
            if self.library_screen.history_loading && self.library_screen.history.is_empty() {
                page = page.child(div().text_color(rgb(DIM)).child("Loading watch history…"));
            } else if self.library_screen.history.is_empty()
                && self.library_screen.history_error.is_empty()
            {
                page = page.child(
                    div()
                        .text_color(rgb(DIM))
                        .child("No watched episodes recorded yet."),
                );
            } else {
                let weak = cx.weak_entity();
                page =
                    page.child(
                        div()
                            .relative()
                            .h(px(220.))
                            .flex()
                            .flex_col()
                            .child(
                                list(
                                    self.library_screen.history_widget.clone(),
                                    move |index, _, app| {
                                        let data = weak.update(app, |this, _| {
                                            this.library_screen.history.get(index).cloned().map(
                                                |item| {
                                                    (
                                                        item,
                                                        this.generation,
                                                        this.profile.as_ref().map(|user| user.id),
                                                    )
                                                },
                                            )
                                        });
                                        let Ok(Some((item, generation, account))) = data else {
                                            return div().into_any();
                                        };
                                        let id = item.media_id;
                                        let weak = weak.clone();
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(10.))
                                            .p(px(6.))
                                            .border_b_1()
                                            .border_color(rgb(EDGE))
                                            .child(
                                                button(
                                                    format!("history-{generation}-{}", item.id),
                                                    format!(
                                                        "{} · Episode {}",
                                                        item.title, item.episode
                                                    ),
                                                    false,
                                                )
                                                .on_click(move |_, window, app| {
                                                    let _ = weak.update(app, |this, cx| {
                                                        if scope_matches(this, generation, account)
                                                            && this.core.generation() == generation
                                                            && this.route == Route::Library
                                                            && this.library_screen.history_open
                                                        {
                                                            this.navigate(
                                                                Route::Anime(id),
                                                                window,
                                                                cx,
                                                            );
                                                        }
                                                    });
                                                }),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .text_size(px(11.))
                                                    .text_color(rgb(DIM))
                                                    .child(basename(&item.path)),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(rgb(DIM))
                                                    .child(format_time(item.watched_at)),
                                            )
                                            .into_any()
                                    },
                                )
                                .flex_1(),
                            )
                            .child(list_scrollbar(
                                "library-history-scrollbar",
                                "Watch history scroll position",
                                self.library_screen.history_widget.clone(),
                                |_, _, _| {},
                            )),
                    );
            }
            if self.library_screen.history_more {
                page = page.child(
                    button(
                        "library-history-more",
                        if self.library_screen.history_loading {
                            "Loading…"
                        } else {
                            "Load older history"
                        },
                        self.library_screen.history_loading,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.load_library_history(true, window, cx);
                        }
                    })),
                );
            }
        }
        page.into_any()
    }

    pub(super) fn render_library_overlay(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("library-dialog-scroll")
            .max_h(px(480.))
            .overflow_y_scroll()
            .child(self.render_library_dialog(cx))
            .into_any()
    }

    fn library_binding_inputs_disabled(&self) -> bool {
        self.library_controls_disabled(Some(ModalLayer::LibraryBind))
            || self
                .library_screen
                .bind
                .as_ref()
                .is_some_and(|bind| bind.busy)
    }

    fn library_controls_disabled(&self, layer: Option<ModalLayer>) -> bool {
        self.edits_fenced()
            || self.route != Route::Library
            || self.profile.is_none()
            || self.active_modal() != layer
    }

    fn library_dismiss_disabled(&self) -> bool {
        self.edits_fenced() || self.active_modal() != self.library_modal_layer()
    }

    fn render_library_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let disabled = self.library_binding_inputs_disabled();
        let dismiss_disabled = self.library_dismiss_disabled();
        if let Some(offset) = &self.library_screen.offset {
            let awaiting_binding = self
                .library_screen
                .bind
                .as_ref()
                .is_some_and(|bind| bind.mode == BindMode::Numbering && !bind.checked);
            offset.update(cx, |input, cx| {
                input.set_disabled(disabled || awaiting_binding, cx)
            });
        }
        if let Some(query) = &self.library_screen.link_query {
            query.update(cx, |input, cx| input.set_disabled(disabled, cx));
        }
        if self
            .library_screen
            .bind
            .as_ref()
            .is_some_and(|bind| bind.mode == BindMode::Link)
        {
            let query = self
                .library_screen
                .link_query
                .as_ref()
                .map(|input| input.read(cx).value().trim().to_lowercase())
                .unwrap_or_default();
            self.library_screen.prepare_binding_candidates(&query);
        }
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        if let Some(path) = self.library_screen.remove_folder.clone() {
            let disabled = self.library_screen.busy
                || self.library_controls_disabled(Some(ModalLayer::LibraryRemove));
            return dialog("library-remove-confirm", "Remove library folder?")
                .child(div().child(format!(
                    "{path} will no longer be scanned. The files stay on disk."
                )))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .mt(px(12.))
                        .child(
                            button(
                                "library-remove-cancel",
                                "Cancel",
                                dismiss_disabled || self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.dismiss_library_dialog(cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            button("library-remove-confirm-button", "Remove", disabled).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.remove_library_folder(path.clone(), window, cx);
                                    }
                                }),
                            ),
                        ),
                )
                .into_any();
        }
        if let Some(id) = self.library_screen.unlink_media {
            let disabled = self.library_screen.busy
                || self.library_controls_disabled(Some(ModalLayer::LibraryUnlink));
            return dialog("library-unlink-confirm", "Remove manual links?")
                .child("The recognizer will guess these files again on the next scan.")
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .mt(px(12.))
                        .child(
                            button(
                                "library-unlink-cancel",
                                "Cancel",
                                dismiss_disabled || self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.dismiss_library_dialog(cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            button("library-unlink-confirm-button", "Remove links", disabled)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.unlink_library_media(id, window, cx);
                                    }
                                })),
                        ),
                )
                .into_any();
        }
        let Some(bind) = self.library_screen.bind.as_ref() else {
            return div().into_any();
        };
        let folder_allowed =
            self.library_screen.folders_loaded && bind.folder_allowed(&self.library_screen.folders);
        let target = bind.target(
            &self.library_screen.folders,
            self.library_screen.folders_loaded,
        );
        let target_unavailable = target.is_none();
        let mode = bind.mode;
        let scope = bind.scope;
        let busy = bind.busy;
        let mut body = dialog(
            "library-bind-dialog",
            if mode == BindMode::Link {
                "Link to a show on your list"
            } else {
                "Episode numbering"
            },
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(DIM))
                .whitespace_normal()
                .child(if mode == BindMode::Link {
                    basename(&bind.path)
                } else {
                    bind.title.clone()
                }),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(DIM))
                .whitespace_normal()
                .child(target.unwrap_or_else(|| bind.path.clone())),
        )
        .child(
            div()
                .flex()
                .gap(px(8.))
                .mt(px(8.))
                .child(
                    button(
                        "library-bind-file",
                        "This file",
                        disabled || scope == BindScope::File,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.change_bind_scope(BindScope::File, window, cx);
                        }
                    })),
                )
                .child(
                    button(
                        "library-bind-folder",
                        "Whole folder",
                        disabled || !folder_allowed || scope == BindScope::Folder,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.change_bind_scope(BindScope::Folder, window, cx);
                        }
                    })),
                ),
        );
        if bind.loading {
            body = body.child(
                div()
                    .text_color(rgb(DIM))
                    .child("Reading the current link…"),
            );
        }
        if target_unavailable && !self.library_screen.folders_loaded {
            if self.library_screen.folders_error.is_empty() {
                body = body.child(
                    div()
                        .text_color(rgb(DIM))
                        .child("Checking library folders…"),
                );
            } else {
                body = body.child(
                    button(
                        "library-bind-retry-folders",
                        "Retry loading library folders",
                        disabled || self.library_screen.scanning,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.library_binding_inputs_disabled() {
                            this.refresh_library_data(window, cx);
                        }
                    })),
                );
            }
        }
        if let Some(current) = bind.current {
            let title = bind
                .current_title
                .clone()
                .unwrap_or_else(|| format!("show #{}", current.media_id));
            body = body.child(
                div()
                    .text_color(rgb(DIM))
                    .child(format!("Currently linked to {title}")),
            );
        }
        if bind.restored && (mode == BindMode::Link || bind.offset_edited) {
            body = body.child(
                div()
                    .text_color(rgb(ACCENT))
                    .text_size(px(12.))
                    .child("Saved draft for this account. Review before saving."),
            );
        }
        if let Some(id) = bind.relink {
            body = body.child(
                div()
                    .text_color(rgb(ACCENT))
                    .child(format!("Click show #{id} again to confirm relinking.")),
            );
        }
        if !bind.error.is_empty() {
            body = body
                .child(div().text_color(rgb(ERROR)).child(bind.error.clone()))
                .child(
                    button("library-bind-retry", "Retry", bind.loading || disabled).on_click(
                        cx.listener(move |this, _, window, cx| {
                            if scope_matches(this, generation, account)
                                && !this.library_binding_inputs_disabled()
                                && this
                                    .library_screen
                                    .bind
                                    .as_ref()
                                    .is_some_and(|bind| !bind.loading && !bind.busy)
                            {
                                this.load_library_binding(window, cx);
                            }
                        }),
                    ),
                );
        }
        if let Some(offset) = &self.library_screen.offset {
            body = body.child(eyebrow("Episode offset")).child(div().w(px(125.)).border_1().border_color(rgb(EDGE)).p(px(4.)).child(offset.clone()))
                .child(div().text_size(px(11.)).text_color(rgb(DIM)).child("Added to detected file episode numbers. Use -12 to map file 13 to episode 1. Results must fit the show's episode total."));
        }
        if mode == BindMode::Link {
            if let Some(query) = &self.library_screen.link_query {
                body = body.child(
                    div()
                        .border_1()
                        .border_color(rgb(EDGE))
                        .p(px(4.))
                        .child(query.clone()),
                );
            }
            let candidates = self.library_screen.binding_candidates.as_ref().unwrap();
            if candidates.total == 0 {
                body = body.child(div().text_color(rgb(DIM)).child("No matches on your list."));
            }
            if candidates.total > 100 {
                body = body.child(
                    div()
                        .text_color(rgb(DIM))
                        .text_size(px(11.))
                        .child("Showing the first 100 matches. Search to narrow the list."),
                );
            }
            let mut list = div()
                .id("library-link-candidates")
                .max_h(px(240.))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            for candidate in &candidates.rows {
                let id = candidate.media_id;
                let image = self.cover_images.get(&(id, 32, 44)).cloned();
                let cover = if let Some(image) = image {
                    img(image)
                        .w(px(32.))
                        .h(px(44.))
                        .object_fit(gpui::ObjectFit::Cover)
                        .into_any()
                } else {
                    div().w(px(32.)).h(px(44.)).bg(rgb(PANEL_2)).into_any()
                };
                list = list.child(
                    button(
                        format!("library-link-{id}"),
                        "",
                        disabled
                            || !bind.checked
                            || target_unavailable
                            || bind.current.is_some_and(|current| current.media_id == id),
                    )
                    .aria_label(candidate.label.clone())
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(cover)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(candidate.text.clone()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if scope_matches(this, generation, account) {
                            this.save_library_bind(id, window, cx);
                        }
                    })),
                );
            }
            body = body.child(list);
        } else if let Some(id) = bind.media_id {
            body = body.child(
                button(
                    "library-numbering-save",
                    if busy { "Saving…" } else { "Save numbering" },
                    disabled
                        || bind.loading
                        || target_unavailable
                        || !bind.checked
                        || bind.current.is_some_and(|current| current.media_id != id),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if scope_matches(this, generation, account) {
                        this.save_library_bind(id, window, cx);
                    }
                })),
            );
        }
        body.child(
            button("library-bind-cancel", "Cancel", dismiss_disabled || busy).on_click(
                cx.listener(move |this, _, _, cx| {
                    if scope_matches(this, generation, account) {
                        this.dismiss_library_dialog(cx);
                    }
                }),
            ),
        )
        .into_any()
    }
}

fn link_candidates<'a>(entries: &'a [ListEntry], query: &str) -> Vec<&'a ListEntry> {
    let mut candidates: Vec<_> = entries
        .iter()
        .filter(|entry| {
            query.is_empty()
                || entry.media.as_ref().is_some_and(|media| {
                    [
                        media.title_english.as_deref(),
                        media.title_romaji.as_deref(),
                        media.title_native.as_deref(),
                    ]
                    .iter()
                    .flatten()
                    .any(|title| title.to_lowercase().contains(query))
                })
        })
        .collect();
    candidates.sort_by_cached_key(|entry| {
        (
            status_order(&entry.status),
            entry
                .media
                .as_ref()
                .map(|media| media.display_title().to_lowercase())
                .unwrap_or_default(),
        )
    });
    candidates
}

fn group_library_rows(
    files: &[LibraryFile],
    entries: &[ListEntry],
    expanded: &HashSet<i64>,
) -> Vec<LibraryRow> {
    let entries: HashMap<_, _> = entries
        .iter()
        .map(|entry| (entry.media_id, entry.clone()))
        .collect();
    let mut groups: HashMap<i64, Group> = HashMap::new();
    let mut group_order = Vec::new();
    let mut unmatched = Vec::new();
    for file in files {
        if let Some(id) = file.media_id {
            let group = groups.entry(id).or_insert_with(|| {
                group_order.push(id);
                Group {
                    media_id: id,
                    title: file.matched.clone().unwrap_or_else(|| format!("#{id}")),
                    entry: entries.get(&id).cloned(),
                    files: Vec::new(),
                    bound: false,
                }
            });
            group.bound |= file.bound;
            group.files.push(file.clone());
        } else {
            unmatched.push(file.clone());
        }
    }
    let mut groups: Vec<_> = group_order
        .into_iter()
        .filter_map(|id| groups.remove(&id))
        .collect();
    groups.sort_by(|a, b| compare_titles(&a.title, &b.title));
    let mut rows = Vec::new();
    for mut group in groups {
        group.files.sort_by(|a, b| {
            a.episode
                .unwrap_or(i64::MAX)
                .cmp(&b.episode.unwrap_or(i64::MAX))
                .then_with(|| a.path.cmp(&b.path))
        });
        let progress = group.entry.as_ref().map_or(-1, |entry| entry.progress);
        let id = group.media_id;
        let title = group
            .entry
            .as_ref()
            .and_then(|entry| entry.media.as_ref())
            .map(|media| media.display_title())
            .unwrap_or_else(|| group.title.clone());
        let files = if expanded.contains(&id) {
            group.files.clone()
        } else {
            Vec::new()
        };
        rows.push(LibraryRow::Group(Box::new(group)));
        rows.extend(
            files
                .into_iter()
                .map(|file| LibraryRow::File(file, id, progress, title.clone())),
        );
    }
    if !unmatched.is_empty() {
        rows.push(LibraryRow::UnmatchedHeader(unmatched.len()));
        rows.extend(unmatched.into_iter().map(LibraryRow::Unmatched));
    }
    rows
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}
fn status_order(status: &str) -> usize {
    [
        "CURRENT",
        "REPEATING",
        "PLANNING",
        "PAUSED",
        "DROPPED",
        "COMPLETED",
    ]
    .iter()
    .position(|item| *item == status)
    .unwrap_or(9)
}
fn format_time(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|date| {
            date.with_timezone(&chrono::Local)
                .format("%b %-d, %Y %-I:%M %p")
                .to_string()
        })
        .unwrap_or_default()
}
fn cover(cover: Option<Arc<RenderImage>>) -> AnyElement {
    if let Some(image) = cover {
        img(image)
            .w(px(46.))
            .h(px(63.))
            .object_fit(gpui::ObjectFit::Cover)
            .into_any()
    } else {
        div().w(px(46.)).h(px(63.)).bg(rgb(PANEL_2)).into_any()
    }
}
fn scope_matches(this: &Workspace, generation: u64, account: Option<i64>) -> bool {
    this.generation == generation && this.profile.as_ref().map(|user| user.id) == account
}
fn open_file(
    this: &mut Workspace,
    path: &str,
    reveal: bool,
    generation: u64,
    account: Option<i64>,
    cx: &mut Context<Workspace>,
) {
    if !scope_matches(this, generation, account)
        || this.core.generation() != generation
        || this.library_controls_disabled(None)
        || this.library_screen.opening_file
    {
        return;
    }
    this.library_screen.opening_file = true;
    this.library_screen.action_error.clear();
    let path = Path::new(path).to_path_buf();
    let job = this.core.spawn(async move {
        tokio::task::spawn_blocking(move || {
            if reveal {
                crate::platform::NativePlatform.reveal_path(&path)
            } else {
                crate::platform::NativePlatform.open_path(&path)
            }
        })
        .await
        .map_err(|error| error.to_string())?
    });
    cx.spawn(async move |this, cx| {
        let result = joined(job.await);
        let _ = this.update(cx, |this, cx| {
            if !scope_matches(this, generation, account) {
                return;
            }
            this.library_screen.opening_file = false;
            if this.core.generation() == generation
                && let Err(error) = result
            {
                this.library_screen.action_error = error;
            }
            cx.notify();
        });
    })
    .detach();
    cx.notify();
}
fn render_library_row(
    row: &LibraryRow,
    image: Option<Arc<RenderImage>>,
    expanded: bool,
    weak: WeakEntity<Workspace>,
    workspace: &Workspace,
) -> AnyElement {
    let generation = workspace.generation;
    let account = workspace.profile.as_ref().map(|user| user.id);
    let actions_disabled = workspace.library_controls_disabled(None);
    let folder_actions_disabled = actions_disabled || workspace.library_screen.busy;
    let file_actions_disabled = actions_disabled || workspace.library_screen.opening_file;
    match row {
        LibraryRow::Group(group) => {
            let id = group.media_id;
            let title = group
                .entry
                .as_ref()
                .and_then(|entry| entry.media.as_ref())
                .map(|media| media.display_title())
                .unwrap_or_else(|| group.title.clone());
            let progress = group.entry.as_ref().map(|entry| entry.progress);
            let total = group
                .entry
                .as_ref()
                .and_then(|entry| entry.media.as_ref())
                .and_then(|media| media.episodes);
            let weak_toggle = weak.clone();
            let weak_detail = weak.clone();
            let weak_unlink = weak.clone();
            let mut line = div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(9.))
                .p(px(7.))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .child(
                    button(
                        format!("library-expand-{id}"),
                        if expanded { "▾" } else { "▸" },
                        false,
                    )
                    .aria_label(format!(
                        "{} files for {title}",
                        if expanded { "Collapse" } else { "Expand" }
                    ))
                    .aria_expanded(expanded)
                    .on_click(move |_, _, app| {
                        let _ = weak_toggle.update(app, |this, cx| {
                            if !scope_matches(this, generation, account) {
                                return;
                            }
                            if !this.library_screen.expanded.insert(id) {
                                this.library_screen.expanded.remove(&id);
                            }
                            this.rebuild_library_rows();
                            cx.notify();
                        });
                    }),
                )
                .child(cover(image))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().whitespace_normal().text_color(rgb(INK)).child(title))
                        .child(div().text_size(px(11.)).text_color(rgb(DIM)).child(format!(
                                "{} · {} files",
                                progress
                                    .map(|progress| format!(
                                        "Ep {progress}{} watched",
                                        total.map(|total| format!("/{total}")).unwrap_or_default()
                                    ))
                                    .unwrap_or_else(|| "Not on your list".into()),
                                group.files.len()
                            ))),
                )
                .child(
                    button(format!("library-detail-{id}"), "Details", false).on_click(
                        move |_, window, app| {
                            let _ = weak_detail.update(app, |this, cx| {
                                if scope_matches(this, generation, account) {
                                    this.navigate(Route::Anime(id), window, cx);
                                }
                            });
                        },
                    ),
                );
            if group.bound {
                line = line.child(
                    button(
                        format!("library-unlink-{id}"),
                        "Unlink",
                        folder_actions_disabled,
                    )
                    .on_click(move |_, _, app| {
                        let _ = weak_unlink.update(app, |this, cx| {
                            if !scope_matches(this, generation, account)
                                || this.library_controls_disabled(None)
                                || this.library_screen.busy
                            {
                                return;
                            }
                            this.library_screen.unlink_media = Some(id);
                            cx.notify();
                        });
                    }),
                );
            }
            if let Some(file) = group.next_file() {
                let path = file.path.clone();
                let episode = file.episode.unwrap_or(0);
                let movie = group
                    .entry
                    .as_ref()
                    .and_then(|entry| entry.media.as_ref())
                    .and_then(|media| media.format.as_deref())
                    == Some("MOVIE");
                let weak_play = weak.clone();
                line = line.child(
                    button(
                        format!("library-next-{generation}-{id}-{episode}-{path}"),
                        if movie {
                            "▶ Play movie".into()
                        } else {
                            format!("▶ Ep {episode}")
                        },
                        file_actions_disabled,
                    )
                    .debug_selector(move || format!("library-next-{id}"))
                    .on_click(move |_, _, app| {
                        let _ = weak_play.update(app, |this, cx| {
                            let next = this.library_screen.rows.iter().find_map(|row| match row {
                                LibraryRow::Group(group) if group.media_id == id => {
                                    group.next_file()
                                }
                                _ => None,
                            });
                            if next.is_some_and(|file| {
                                file.path == path && file.episode == Some(episode)
                            }) {
                                open_file(this, &path, false, generation, account, cx);
                            }
                        });
                    }),
                );
            }
            line.into_any()
        }
        LibraryRow::File(file, id, progress, title) => {
            let id = *id;
            let progress = *progress;
            let path = file.path.clone();
            let weak_play = weak.clone();
            let weak_reveal = weak.clone();
            let weak_number = weak.clone();
            let mut line = div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(7.))
                .pl(px(48.))
                .py(px(5.))
                .border_b_1()
                .border_color(rgb(EDGE))
                .child(
                    div().w(px(50.)).text_color(rgb(DIM)).child(
                        file.episode
                            .map(|episode| format!("Ep {episode}"))
                            .unwrap_or_else(|| "—".into()),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(rgb(
                            if file.episode.is_some_and(|episode| episode <= progress) {
                                DIM
                            } else {
                                INK
                            },
                        ))
                        .whitespace_normal()
                        .child(basename(&file.path)),
                )
                .child(
                    button(
                        format!("library-file-play-{}", file.path),
                        "Play",
                        file_actions_disabled,
                    )
                    .on_click(move |_, _, app| {
                        let _ = weak_play.update(app, |this, cx| {
                            open_file(this, &path, false, generation, account, cx)
                        });
                    }),
                );
            if progress >= 0 {
                let path = file.path.clone();
                let title = title.clone();
                line = line.child(
                    button(
                        format!("library-file-number-{}", file.path),
                        "Numbering",
                        folder_actions_disabled,
                    )
                    .on_click(move |_, window, app| {
                        let _ = weak_number.update(app, |this, cx| {
                            if !scope_matches(this, generation, account) {
                                return;
                            }
                            this.open_library_bind(
                                path.clone(),
                                Some((id, title.clone())),
                                BindMode::Numbering,
                                window,
                                cx,
                            )
                        });
                    }),
                );
            }
            let path = file.path.clone();
            line.child(
                button(
                    format!("library-file-reveal-{}", file.path),
                    "Show",
                    file_actions_disabled,
                )
                .on_click(move |_, _, app| {
                    let _ = weak_reveal.update(app, |this, cx| {
                        open_file(this, &path, true, generation, account, cx)
                    });
                }),
            )
            .into_any()
        }
        LibraryRow::UnmatchedHeader(count) => div()
            .w_full()
            .p(px(8.))
            .bg(rgb(PANEL))
            .text_color(rgb(DIM))
            .child(format!("Unmatched · {count} files"))
            .into_any(),
        LibraryRow::Unmatched(file) => {
            let path = file.path.clone();
            let weak_link = weak.clone();
            let weak_play = weak.clone();
            let weak_reveal = weak.clone();
            let play_path = file.path.clone();
            let reveal_path = file.path.clone();
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(7.))
                .p(px(6.))
                .border_b_1()
                .border_color(rgb(EDGE))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_size(px(12.))
                        .text_color(rgb(DIM))
                        .child(basename(&file.path)),
                )
                .child(
                    button(
                        format!("library-link-{}", file.path),
                        "Link",
                        folder_actions_disabled,
                    )
                    .on_click(move |_, window, app| {
                        let _ = weak_link.update(app, |this, cx| {
                            if !scope_matches(this, generation, account) {
                                return;
                            }
                            this.open_library_bind(path.clone(), None, BindMode::Link, window, cx)
                        });
                    }),
                )
                .child(
                    button(
                        format!("library-unmatched-play-{}", file.path),
                        "Play",
                        file_actions_disabled,
                    )
                    .on_click(move |_, _, app| {
                        let _ = weak_play.update(app, |this, cx| {
                            open_file(this, &play_path, false, generation, account, cx)
                        });
                    }),
                )
                .child(
                    button(
                        format!("library-unmatched-reveal-{}", file.path),
                        "Show",
                        file_actions_disabled,
                    )
                    .on_click(move |_, _, app| {
                        let _ = weak_reveal.update(app, |this, cx| {
                            open_file(this, &reveal_path, true, generation, account, cx)
                        });
                    }),
                )
                .into_any()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn history_accessibility_target_survives_insertion(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = super::super::workspace_tests::fixture();
        let session = core.clone();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let record = |id, media_id, title: &str| WatchHistoryItem {
            id,
            media_id,
            title: title.into(),
            path: format!("/fixture/{id}.mkv"),
            episode: 1,
            watched_at: 0,
        };
        cx.simulate_resize(gpui::size(px(1100.), px(1000.)));
        cx.simulate_accessibility_activation();
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Library;
            let library = &mut w.library_screen;
            library.history_open = true;
            library.history = vec![record(1, 90, "Original")];
            library.history_widget.reset_with_uniform_height(1, px(56.));
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (original, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Original · Episode 1"))
            .unwrap();
        workspace.update_in(cx, |w, _, cx| {
            let library = &mut w.library_screen;
            library.history.insert(0, record(2, 91, "New playback"));
            library.history_widget.reset_with_uniform_height(2, px(56.));
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        let click = gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::Click,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *original,
            data: None,
        };
        cx.simulate_accessibility_action(click.clone());
        workspace.read_with(cx, |w, _| assert_eq!(w.route, Route::Anime(90)));

        workspace.update_in(cx, |w, _, cx| {
            w.route = Route::Library;
            cx.notify();
        });
        cx.update(|window, app| window.draw(app).clear(app));
        session.begin_login_intent();
        cx.simulate_accessibility_action(click);
        workspace.read_with(cx, |w, _| assert_eq!(w.route, Route::Library));
    }

    #[gpui::test]
    fn library_next_file_handles_max_progress(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = super::super::workspace_tests::fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(gpui::size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Library;
            w.library_screen.folders = vec!["/fixture".into()];
            w.library_screen.scan_loaded = true;
            w.library_screen.files = [i64::MIN, i64::MAX]
                .into_iter()
                .map(|episode| LibraryFile {
                    path: format!("/fixture/episode-{episode}.mkv"),
                    media_id: Some(9),
                    episode: Some(episode),
                    ..Default::default()
                })
                .collect();
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        for (progress, expected) in [(i64::MAX, false), (i64::MAX - 1, true)] {
            workspace.update_in(cx, |w, _, cx| {
                w.library_screen.entries = vec![ListEntry {
                    media_id: 9,
                    progress,
                    ..Default::default()
                }];
                w.rebuild_library_rows();
                cx.notify();
            });
            cx.update(|window, app| window.draw(app).clear(app));
            let tree = cx.take_accessibility_tree_update().unwrap();
            let label = format!("▶ Ep {}", i64::MAX);
            assert_eq!(
                tree.nodes
                    .iter()
                    .any(|(_, node)| node.label() == Some(&label)),
                expected
            );
        }
    }

    #[gpui::test]
    fn library_next_file_stays_bound_to_the_pressed_path(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = super::super::workspace_tests::fixture();
        let session = core.clone();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let file = |path: &str| LibraryFile {
            media_id: Some(9),
            episode: Some(2),
            path: path.into(),
            ..Default::default()
        };
        cx.simulate_resize(gpui::size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, _, cx| {
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Library;
            w.library_screen.folders = vec!["/fixture".into()];
            w.library_screen.scan_loaded = true;
            w.library_screen.entries = vec![ListEntry {
                media_id: 9,
                progress: 1,
                ..Default::default()
            }];
            w.library_screen.files = vec![file("/fixture/old.mkv")];
            w.rebuild_library_rows();
            cx.notify();
        });
        cx.run_until_parked();
        let play = cx.debug_bounds("library-next-9").unwrap().center();
        cx.simulate_mouse_down(play, gpui::MouseButton::Left, gpui::Modifiers::default());
        workspace.update_in(cx, |w, _, cx| {
            w.library_screen.files = vec![file("/fixture/new.mkv")];
            w.rebuild_library_rows();
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_up(play, gpui::MouseButton::Left, gpui::Modifiers::default());
        workspace.read_with(cx, |w, _| assert!(!w.library_screen.opening_file));

        session.begin_login_intent();
        cx.simulate_click(play, gpui::Modifiers::default());
        workspace.read_with(cx, |w, _| assert!(!w.library_screen.opening_file));
    }

    #[gpui::test]
    fn history_refresh_reconciles_loaded_pages_after_pagination(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::{TestPlatform, fixture};
        use std::time::{Duration, Instant};

        let (initial, runtime, dir) = fixture();
        drop(initial);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        let profile = kurisu_core::User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        db.replace_account("synthetic-token", &profile).unwrap();
        for episode in 1..=125 {
            db.record_watch(7, "/fixture/show.mkv", 9, episode).unwrap();
        }
        let core = kurisu_core::Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let finish = |cx: &mut gpui::VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(4);
            while view.read_with(cx, |w, _| w.library_screen.history_loading) {
                assert!(Instant::now() < deadline, "history read did not finish");
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
                cx.run_until_parked();
            }
        };
        view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile);
            w.route = Route::Library;
            w.library_screen.history_open = true;
            w.refresh_library_history(window, cx);
        });
        finish(cx);
        view.update_in(cx, |w, window, cx| {
            assert_eq!(w.library_screen.history.len(), 50);
            w.load_library_history(true, window, cx);
            db.record_watch(7, "/fixture/show.mkv", 9, 126).unwrap();
            w.refresh_library_history(window, cx);
        });
        finish(cx);
        view.update_in(cx, |w, window, cx| {
            let state = &w.library_screen;
            assert_eq!(state.history.len(), 101);
            assert_eq!(state.history[0].episode, 126);
            assert_eq!(state.history.last().unwrap().episode, 26);
            assert!(state.history_more);
            state.history_widget.scroll_to(ListOffset {
                item_ix: 70,
                offset_in_item: px(7.),
            });
            assert_eq!(state.history[70].episode, 56);
            db.record_watch(7, "/fixture/show.mkv", 9, 127).unwrap();
            db.0.lock()
                .execute("DELETE FROM watch_history WHERE episode = 57", [])
                .unwrap();
            db.upsert_media(&kurisu_core::models::Media {
                id: 9,
                title_english: Some("Updated title".into()),
                ..Default::default()
            })
            .unwrap();
            w.refresh_library_history(window, cx);
            w.refresh_library_history(window, cx);
        });
        finish(cx);
        view.update_in(cx, |w, window, cx| {
            let state = &w.library_screen;
            assert_eq!(state.history.len(), 101);
            assert_eq!(state.history[0].episode, 127);
            assert!(
                state
                    .history
                    .iter()
                    .all(|item| item.episode != 57 && item.title == "Updated title")
            );
            assert_eq!(state.history.last().unwrap().episode, 26);
            assert_eq!(
                state.history_widget.logical_scroll_top(),
                ListOffset {
                    item_ix: 70,
                    offset_in_item: px(7.)
                }
            );
            w.load_library_history(true, window, cx);
        });
        finish(cx);
        view.read_with(cx, |w, _| {
            let state = &w.library_screen;
            assert_eq!(state.history.len(), 126);
            assert_eq!(state.history.last().unwrap().episode, 1);
            assert!(!state.history_more);
        });

        cx.run_until_parked();
        let toggle = cx.debug_bounds("library-history-toggle").unwrap().center();
        cx.simulate_click(toggle, gpui::Modifiers::default());
        db.record_watch(7, "/fixture/show.mkv", 9, 128).unwrap();
        view.update_in(cx, |w, window, cx| {
            assert!(!w.library_screen.history_open);
            w.handle_event(
                kurisu_core::Event {
                    generation: w.generation,
                    account_id: Some(7),
                    prompt_id: None,
                    kind: kurisu_core::EventKind::WatchHistoryUpdated(128),
                },
                window,
                cx,
            );
        });
        cx.simulate_click(toggle, gpui::Modifiers::default());
        finish(cx);
        view.read_with(cx, |w, _| {
            assert!(w.library_screen.history_open);
            assert_eq!(w.library_screen.history[0].episode, 128);
            assert_eq!(w.library_screen.history.len(), 127);
        });
    }

    #[gpui::test]
    fn history_rejects_core_account_change_before_workspace_updates(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::{TestPlatform, fixture};

        let (initial, runtime, dir) = fixture();
        drop(initial);
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        let profile = kurisu_core::User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        db.replace_account("synthetic-token", &profile).unwrap();
        let core = kurisu_core::Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core.clone(), window, cx, false)
        });
        let job = view.update_in(cx, |w, window, cx| {
            w.profile = Some(profile.clone());
            w.route = Route::Library;
            w.library_screen.history_open = true;
            w.refresh_library_history(window, cx);
            w.read_aborts["library-history"][0].clone()
        });
        core.begin_login_intent();
        db.replace_account("replacement-token", &kurisu_core::User { id: 8, ..profile })
            .unwrap();
        db.record_watch(8, "/fixture/replacement.mkv", 9, 1)
            .unwrap();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(4), async {
                while !job.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert_eq!(w.profile.as_ref().unwrap().id, 7);
            assert!(w.library_screen.history.is_empty());
        });
    }

    #[gpui::test]
    fn accesskit_clicks_only_reach_the_active_dialog(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::{fixture, settle, tracking_fixture, update_fixture};
        use gpui::accesskit::{Action, ActionRequest, NodeId, TreeId, TreeUpdate};

        let (first, runtime, dir) = fixture();
        let profile = kurisu_core::User {
            id: 7,
            ..Default::default()
        };
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        db.replace_account("synthetic-token", &profile).unwrap();
        db.upsert_media(&kurisu_core::models::Media {
            id: 9,
            title_english: Some("Show".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        drop(first);
        let core = kurisu_core::Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(super::super::workspace_tests::TestPlatform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let folder = dir.0.join("shows");
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("Show - 03.mkv");
        std::fs::write(&file, b"").unwrap();
        let path = file.to_string_lossy().into_owned();
        let folder = folder.to_string_lossy().into_owned();
        let folders = runtime
            .block_on(core.add_library_folder(folder.clone()))
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|win, cx| {
            Workspace::new_with_session_restore(core.clone(), win, cx, false)
        });
        workspace.update_in(cx, |w, win, cx| {
            w.profile = Some(profile);
            w.route = Route::Library;
            w.enter_library(win, cx);
            w.library_screen.folders = folders;
            w.library_screen.folders_loaded = true;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();

        let button_id = |tree: &TreeUpdate, label: &str, hidden: bool| -> NodeId {
            tree.nodes
                .iter()
                .rev()
                .find_map(|(id, node)| {
                    (node.label() == Some(label)
                        && node.is_hidden() == hidden
                        && node.supports_action(Action::Click) != hidden)
                        .then_some(*id)
                })
                .unwrap_or_else(|| panic!("missing accessible button {label}"))
        };
        let click = |cx: &gpui::VisualTestContext, target_node| {
            cx.simulate_accessibility_action(ActionRequest {
                action: Action::Click,
                target_tree: TreeId::ROOT,
                target_node,
                data: None,
            });
        };
        for (library, label) in [
            (ModalLayer::LibraryBind, "Save numbering"),
            (ModalLayer::LibraryUnlink, "Remove links"),
            (ModalLayer::LibraryRemove, "Remove"),
        ] {
            let open = |w: &mut Workspace, win: &mut Window, cx: &mut Context<Workspace>| {
                match library {
                    ModalLayer::LibraryBind => {
                        w.open_library_bind(
                            path.clone(),
                            Some((9, "Show".into())),
                            BindMode::Numbering,
                            win,
                            cx,
                        );
                    }
                    ModalLayer::LibraryUnlink => w.library_screen.unlink_media = Some(9),
                    ModalLayer::LibraryRemove => {
                        w.library_screen.remove_folder = Some(folder.clone());
                    }
                    _ => unreachable!(),
                }
                cx.notify();
            };
            workspace.update_in(cx, open);
            let deadline = Instant::now() + Duration::from_secs(5);
            while workspace.read_with(cx, |w, _| {
                w.library_screen
                    .bind
                    .as_ref()
                    .is_some_and(|bind| !bind.checked)
            }) {
                assert!(Instant::now() < deadline, "binding lookup did not finish");
                settle(cx, &runtime);
            }
            cx.run_until_parked();
            let tree = cx.take_accessibility_tree_update().unwrap();
            let covered = [
                button_id(&tree, label, false),
                button_id(&tree, "Cancel", false),
            ];
            for (top, dismiss) in [
                (ModalLayer::Prompt, "Skip"),
                (ModalLayer::Update, "Later"),
                (ModalLayer::UpdateFailed, "Got it"),
                (ModalLayer::Logout, "Cancel"),
            ] {
                workspace.update_in(cx, |w, _, cx| {
                    match top {
                        ModalLayer::Prompt => w.prompt = Some(tracking_fixture(4)),
                        ModalLayer::Update => w.update = Some(update_fixture()),
                        ModalLayer::UpdateFailed => {
                            w.update_failed = "Update failed in test".into()
                        }
                        ModalLayer::Logout => w.logout_confirming = true,
                        _ => unreachable!(),
                    }
                    cx.notify();
                });
                cx.run_until_parked();
                let tree = cx.take_accessibility_tree_update().unwrap();
                let hidden = [
                    button_id(&tree, label, true),
                    button_id(&tree, "Cancel", true),
                ];
                for id in covered.into_iter().chain(hidden) {
                    click(cx, id);
                    workspace.read_with(cx, |w, _| {
                        assert_eq!(w.active_modal(), Some(top));
                        assert_eq!(w.library_modal_layer(), Some(library));
                        assert_eq!(w.active_writes, 0);
                    });
                }
                click(cx, button_id(&tree, dismiss, false));
                assert_eq!(
                    workspace.read_with(cx, |w, _| w.active_modal()),
                    Some(library)
                );
            }
            if library == ModalLayer::LibraryRemove {
                click(cx, covered[1]);
                assert_eq!(workspace.read_with(cx, |w, _| w.active_modal()), None);
                workspace.update_in(cx, open);
                cx.run_until_parked();
            }
            click(cx, covered[0]);
            while workspace.read_with(cx, |w, _| w.active_writes != 0) {
                assert!(Instant::now() < deadline, "Library action did not finish");
                settle(cx, &runtime);
            }
            workspace.read_with(cx, |w, _| {
                assert_eq!(
                    w.active_modal(),
                    None,
                    "{label}: {:?} {}",
                    w.library_screen.bind.as_ref().map(|bind| &bind.error),
                    w.library_screen.action_error
                );
            });
            match library {
                ModalLayer::LibraryBind | ModalLayer::LibraryUnlink => assert_eq!(
                    runtime
                        .block_on(core.library_binding_for(path.clone()))
                        .unwrap(),
                    (library == ModalLayer::LibraryBind).then_some(9),
                ),
                ModalLayer::LibraryRemove => {
                    assert!(
                        runtime
                            .block_on(core.get_library_folders())
                            .unwrap()
                            .is_empty()
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    #[gpui::test]
    fn bucket_b_close_fences_library_binding_inputs(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::{fixture, settle};
        use gpui::{AccessibleAction, EntityInputHandler};

        let (first, runtime, dir) = fixture();
        let profile = kurisu_core::User {
            id: 7,
            ..Default::default()
        };
        let db = kurisu_core::Db::open(&dir.0.join("kurisu.db")).unwrap();
        db.replace_account("synthetic-token", &profile).unwrap();
        db.upsert_media(&kurisu_core::models::Media {
            id: 9,
            title_english: Some("Show".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        drop(first);
        let core = kurisu_core::Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(super::super::workspace_tests::TestPlatform),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        let folder = dir.0.join("shows");
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("Show - 03.mkv");
        std::fs::write(&file, b"").unwrap();
        let path = file.to_string_lossy().into_owned();
        let folders = runtime
            .block_on(core.add_library_folder(folder.to_string_lossy().into_owned()))
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|win, cx| {
            Workspace::new_with_session_restore(core.clone(), win, cx, false)
        });
        workspace.update_in(cx, |w, win, cx| {
            w.profile = Some(profile);
            w.route = Route::Library;
            w.enter_library(win, cx);
            w.library_screen.folders = folders;
            w.open_library_bind(path.clone(), None, BindMode::Link, win, cx);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while workspace.read_with(cx, |w, _| !w.library_screen.bind.as_ref().unwrap().checked) {
            assert!(Instant::now() < deadline, "binding lookup did not finish");
            settle(cx, &runtime);
        }
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (id, node) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.placeholder() == Some("Episode offset"))
            .unwrap();
        let offset_id = *id;
        assert!(node.supports_action(AccessibleAction::SetValue));
        let set_offset = |cx: &mut gpui::VisualTestContext, value: &str| {
            cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
                action: AccessibleAction::SetValue,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node: offset_id,
                data: Some(gpui::accesskit::ActionData::Value(value.into())),
            });
        };
        set_offset(cx, "-1");
        workspace.update_in(cx, |w, win, cx| {
            let offset = w.library_screen.offset.as_ref().unwrap().clone();
            let query = w.library_screen.link_query.as_ref().unwrap().clone();
            assert_eq!(offset.read(cx).value(), "-1");
            query.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..0), "Show", win, cx)
            });
            w.query_error = "hold close barrier".into();
            w.request_close(super::super::CloseIntent::Window, win, cx);
            offset.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..2), "8", win, cx)
            });
            query.update(cx, |input, cx| {
                input.replace_text_in_range(Some(0..4), "Covered", win, cx)
            });
        });
        workspace.read_with(cx, |w, app| {
            assert_eq!(
                w.library_screen.offset.as_ref().unwrap().read(app).value(),
                "-1",
                "close fence let a queued edit change the offset"
            );
            assert_eq!(
                w.library_screen
                    .link_query
                    .as_ref()
                    .unwrap()
                    .read(app)
                    .value(),
                "Show"
            );
        });
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        assert!(tree.nodes.iter().all(|(_, node)| {
            node.placeholder() != Some("Episode offset")
                || !node.supports_action(AccessibleAction::SetValue)
        }));
        set_offset(cx, "9");
        workspace.update_in(cx, |w, win, cx| {
            w.query_error.clear();
            w.cancel_barrier(win, cx);
        });
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.placeholder() == Some("Episode offset")
                && node.supports_action(AccessibleAction::SetValue)
        }));
        workspace.update_in(cx, |w, win, cx| w.save_library_bind(9, win, cx));
        while workspace.read_with(cx, |w, _| w.active_writes != 0) {
            assert!(Instant::now() < deadline, "binding save did not finish");
            settle(cx, &runtime);
        }
        let binding = runtime
            .block_on(core.library_binding_details(path))
            .unwrap()
            .unwrap();
        assert_eq!(binding.episode_offset, -1);
    }

    #[gpui::test]
    fn bucket_b_library_scan_replays_removal_after_tray_restore(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::{fixture, settle};

        let (core, runtime, dir) = fixture();
        let folder = dir.0.join("shows");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("Show - 01.mkv"), b"").unwrap();
        let folder = folder.to_string_lossy().into_owned();
        runtime
            .block_on(core.add_library_folder(folder.clone()))
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|win, cx| {
            Workspace::new_with_session_restore(core.clone(), win, cx, false)
        });
        let (request, worker) = workspace.update_in(cx, |w, win, cx| {
            w.route = Route::Library;
            w.profile = Some(kurisu_core::User {
                id: 7,
                ..Default::default()
            });
            w.refresh_library_entries(win, cx);
            (
                w.library_screen.data_request,
                w.read_aborts["library-data"][0].clone(),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "old scan did not finish");
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        workspace.update_in(cx, |w, win, cx| {
            w.request_close(super::super::CloseIntent::Window, win, cx);
        });
        // Apply the removal while withholding delivery of the completed scan.
        runtime
            .block_on(core.remove_library_folder(folder))
            .unwrap();
        workspace.update_in(cx, |w, win, cx| {
            w.refresh_library_entries(win, cx);
            assert!(w.close_ready && w.library_screen.scanning);
            w.resume_after_close_visibility(true, win, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(
                w.library_screen.data_request > request,
                "old scan did not start a replacement after tray restore"
            );
        });
        while workspace.read_with(cx, |w, _| w.library_screen.scanning) {
            assert!(Instant::now() < deadline, "replacement scan did not finish");
            settle(cx, &runtime);
        }
        workspace.read_with(cx, |w, _| {
            assert!(w.library_screen.scan_loaded);
            assert!(w.library_screen.folders.is_empty());
            assert!(w.library_screen.files.is_empty());
        });
    }

    fn set_binding_draft(w: &mut Workspace) {
        w.library_screen.bind = Some(BindDialog {
            id: 0,
            revision: 0,
            path: "/fixture/Show - 03.mkv".into(),
            media_id: Some(9),
            title: "Show".into(),
            mode: BindMode::Link,
            scope: BindScope::File,
            request: 1,
            current: None,
            current_title: None,
            checked: true,
            loading: false,
            busy: false,
            error: String::new(),
            relink: None,
            restored: false,
            offset_edited: true,
        });
        w.library_screen.binding_input_values[OFFSET_INPUT] = "3".into();
    }

    #[gpui::test]
    fn unknown_library_drafts_require_verified_same_session_identity(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::workspace_tests::fixture;
        for (new_id, offline, superseded, adopts) in [
            (Some(71), false, false, true),
            (Some(71), true, false, false),
            (Some(71), false, true, false),
            (None, false, false, false),
        ] {
            let (core, _runtime, _dir) = fixture();
            let (workspace, cx) = cx.add_window_view(|win, cx| {
                Workspace::new_with_session_restore(core, win, cx, false)
            });
            workspace.update_in(cx, |w, win, cx| {
                w.set_profile(
                    Some(kurisu_core::User {
                        offline: true,
                        ..Default::default()
                    }),
                    win,
                    cx,
                );
                w.route = Route::Settings;
                set_binding_draft(w);
                if superseded {
                    w.core.cancel_login();
                }
                w.set_profile(
                    new_id.map(|id| kurisu_core::User {
                        id,
                        offline,
                        ..Default::default()
                    }),
                    win,
                    cx,
                );
                assert!(w.library_drafts.is_empty());
                assert!(!w.has_inactive_drafts());
                assert_eq!(
                    w.library_screen
                        .pending_draft
                        .as_ref()
                        .map(|draft| draft.offset.as_str()),
                    adopts.then_some("3")
                );
            });
        }
    }

    #[gpui::test]
    fn restored_library_draft_resumes_scanning_after_close_or_install(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{BarrierAction, CloseIntent, workspace_tests::fixture};
        for barrier in [
            BarrierAction::Close(CloseIntent::Quit),
            BarrierAction::Install,
        ] {
            let (core, _runtime, _dir) = fixture();
            let (workspace, cx) = cx.add_window_view(|win, cx| {
                Workspace::new_with_session_restore(core, win, cx, false)
            });
            workspace.update_in(cx, |w, win, cx| {
                let user = kurisu_core::User {
                    id: 71,
                    ..Default::default()
                };
                w.set_profile(
                    Some(kurisu_core::User {
                        id: 72,
                        ..Default::default()
                    }),
                    win,
                    cx,
                );
                set_binding_draft(w);
                w.set_profile(Some(user.clone()), win, cx);
                w.route = Route::Settings;
                set_binding_draft(w);
                w.set_profile(None, win, cx);
                w.start_barrier(barrier, win, cx);
                assert!(w.barrier_blocked_by_inactive_drafts);
                w.route = Route::MyList;
                w.set_profile(Some(user), win, cx);
                w.discard_inactive_drafts(win, cx);
                assert!(w.library_drafts.contains_key(&72));
                assert!(!w.close_ready && !w.stopping);
                assert!(w.barrier.is_none() && w.barrier_error.is_empty());
                assert!(w.library_screen.scanning);
                assert_eq!(w.library_screen.pending_draft.as_ref().unwrap().offset, "3");
            });
        }
    }

    #[gpui::test]
    fn restored_binding_draft_cancels_close_after_an_earlier_save_error(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::super::{CloseIntent, workspace_tests::fixture};
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        workspace.update_in(cx, |w, win, cx| {
            let user = kurisu_core::User {
                id: 71,
                ..Default::default()
            };
            w.set_profile(Some(user.clone()), win, cx);
            set_binding_draft(w);
            w.set_profile(None, win, cx);
            w.query_error = "Save failed".into();
            w.request_close(CloseIntent::Quit, win, cx);
            assert!(w.barrier.is_some() && !w.barrier_blocked_by_inactive_drafts);
            w.set_profile(Some(user), win, cx);
            w.query_error.clear();
            w.check_barrier(win, cx);
            assert!(w.barrier.is_none() && !w.close_ready);
            assert_eq!(w.library_screen.pending_draft.as_ref().unwrap().offset, "3");
        });
    }

    #[gpui::test]
    fn bucket_b_second_signout_keeps_unopened_library_draft(cx: &mut gpui::TestAppContext) {
        use gpui::EntityInputHandler;
        let (core, _runtime, _dir) = super::super::workspace_tests::fixture();
        let (workspace, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        workspace.update_in(cx, |w, win, cx| {
            let user = kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            };
            w.profile = Some(user.clone());
            w.route = Route::Library;
            w.enter_library(win, cx);
            w.open_library_bind("/shows/Show - 03.mkv".into(), None, BindMode::Link, win, cx);
            w.library_screen
                .offset
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..1), "-1", win, cx);
                });
        });
        workspace.update_in(cx, |w, win, cx| {
            let user = w.profile.clone().unwrap();
            w.route = Route::Settings;
            w.set_profile(None, win, cx);
            assert!(w.library_drafts.contains_key(&7));
            w.set_profile(Some(user), win, cx);
            assert!(w.library_screen.pending_draft.is_some());
            assert!(w.library_screen.bind.is_none());
            w.set_profile(None, win, cx);
            let draft = w
                .library_drafts
                .get(&7)
                .expect("second sign-out lost the unopened Library draft");
            assert_eq!(draft.offset, "-1");
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(w.barrier_blocked_by_inactive_drafts);
        });
    }

    #[test]
    fn binding_candidates_cache_order_limit_and_alias_search() {
        let mut state = LibraryState::default();
        state.replace_entries(
            (1..=101)
                .map(|id| ListEntry {
                    media_id: id,
                    status: if id == 101 { "CURRENT" } else { "PLANNING" }.into(),
                    media: Some(kurisu_core::models::Media {
                        id,
                        title_english: Some(format!("Show {id:03}")),
                        title_romaji: (id == 100).then(|| "Different alias".into()),
                        title_native: (id == 100).then(|| "別名".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect(),
        );
        state.prepare_binding_candidates("");
        let first = Arc::clone(state.binding_candidates.as_ref().unwrap());
        assert_eq!(first.total, 101);
        assert_eq!(first.rows.len(), 100);
        assert_eq!(first.rows[0].media_id, 101);
        assert_eq!(first.rows[99].media_id, 99);
        state.prepare_binding_candidates("");
        assert!(Arc::ptr_eq(
            &first,
            state.binding_candidates.as_ref().unwrap()
        ));

        for query in ["different alias", "別名", "show 100"] {
            state.prepare_binding_candidates(query);
            let candidates = state.binding_candidates.as_ref().unwrap();
            assert_eq!(candidates.total, 1);
            assert_eq!(candidates.rows[0].media_id, 100);
            assert_eq!(candidates.rows[0].label, "Link Show 100");
            assert_eq!(candidates.rows[0].text, "Show 100 · Plan to Watch");
        }
        state.replace_entries(vec![ListEntry {
            media_id: 200,
            status: "COMPLETED".into(),
            ..Default::default()
        }]);
        state.prepare_binding_candidates("show 100");
        assert_eq!(state.binding_candidates.as_ref().unwrap().total, 0);
        state.prepare_binding_candidates("");
        let candidates = state.binding_candidates.as_ref().unwrap();
        assert_eq!(candidates.total, 1);
        assert_eq!(candidates.rows[0].media_id, 200);
        assert_eq!(candidates.rows[0].label, "Link #200");
        assert_eq!(candidates.rows[0].text, "#200 · Completed");
    }

    #[test]
    fn replacing_entry_fields_invalidates_labels_without_changing_old_snapshot() {
        let mut state = LibraryState::default();
        let entry = |title: &str, status: &str| ListEntry {
            media_id: 1,
            status: status.into(),
            media: Some(kurisu_core::models::Media {
                id: 1,
                title_english: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        state.replace_entries(vec![entry("Before", "CURRENT")]);
        state.prepare_binding_candidates("");
        let snapshot = state.binding_candidates.clone().unwrap();
        state.replace_entries(vec![entry("After", "COMPLETED")]);
        state.prepare_binding_candidates("");
        assert_eq!(snapshot.rows[0].label, "Link Before");
        assert_eq!(snapshot.rows[0].text, "Before · Watching");
        let current = state.binding_candidates.as_ref().unwrap();
        assert_eq!(current.rows[0].label, "Link After");
        assert_eq!(current.rows[0].text, "After · Completed");
        assert!(!Arc::ptr_eq(&snapshot, current));
    }

    #[test]
    fn binding_candidate_sort_keeps_status_order_and_title_ties() {
        let entries: Vec<_> = [
            (1, "CURRENT", "Same"),
            (2, "COMPLETED", "A"),
            (3, "CURRENT", "same"),
            (4, "REPEATING", "A"),
            (5, "PLANNING", "A"),
            (6, "PAUSED", "A"),
            (7, "DROPPED", "A"),
        ]
        .into_iter()
        .map(|(id, status, title)| ListEntry {
            media_id: id,
            status: status.into(),
            media: Some(kurisu_core::models::Media {
                id,
                title_english: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
        assert_eq!(
            link_candidates(&entries, "")
                .iter()
                .map(|entry| entry.media_id)
                .collect::<Vec<_>>(),
            [1, 3, 4, 5, 6, 7, 2]
        );
    }

    #[gpui::test]
    fn link_dialog_refreshes_candidates_when_library_scan_fails(cx: &mut gpui::TestAppContext) {
        use gpui::EntityInputHandler;
        struct NoExternal;
        impl Platform for NoExternal {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected path")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected reveal")
            }
        }
        struct FixtureDir(std::path::PathBuf);
        impl Drop for FixtureDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = FixtureDir(std::env::temp_dir().join(format!(
            "kurisu-library-refresh-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir_all(&directory.0).unwrap();
        let db_path = directory.0.join("kurisu.db");
        let db = kurisu_core::Db::open(&db_path).unwrap();
        let user = kurisu_core::User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        db.replace_account("fixture-session", &user).unwrap();
        let episode = directory.0.join("Unrelated 01.mkv");
        std::fs::write(&episode, []).unwrap();
        db.set_setting(
            "library_folders",
            &serde_json::to_string(&[directory.0.to_string_lossy().as_ref()]).unwrap(),
        )
        .unwrap();
        let media = |id, title: &str| kurisu_core::models::Media {
            id,
            title_english: Some(title.into()),
            cover_medium: Some(format!("invalid-cover-{id}")),
            ..Default::default()
        };
        for (id, title) in [(71, "Match"), (72, "Other")] {
            db.upsert_media(&media(id, title)).unwrap();
            db.upsert_entry(&ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::from_db(
            kurisu_core::Db::open(&db_path).unwrap(),
            directory.0.clone(),
            runtime.handle().clone(),
            Arc::new(NoExternal),
            kurisu_core::UpdateChannel::Native,
        );
        let observer = core.clone();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |this, window, cx| {
            this.auth_intent += 1;
            this.restoring_session = false;
            this.profile = Some(user);
            this.route = Route::Library;
            this.enter_library(window, cx);
        });
        let settle = |cx: &mut gpui::VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
                cx.run_until_parked();
                if workspace.read_with(cx, |this, _| {
                    !this.library_screen.scanning && this.library_screen.entries_loaded
                }) {
                    break;
                }
                assert!(Instant::now() < deadline, "Library data read must finish");
            }
        };
        settle(cx);
        workspace.update_in(cx, |this, window, cx| {
            assert!(this.library_screen.scan_loaded);
            assert!(this.library_screen.scan_error.is_empty());
            assert_eq!(this.library_screen.files.len(), 1);
            let path = this.library_screen.files[0].path.clone();
            assert_eq!(path, episode.to_string_lossy());
            this.open_library_bind(path, None, BindMode::Link, window, cx);
            let input = this.library_screen.link_query.clone().unwrap();
            input.update(cx, |input, cx| {
                input.replace_text_in_range(None, "  MATCH  ", window, cx)
            });
        });
        cx.run_until_parked();
        workspace.read_with(cx, |this, _| {
            assert_eq!(this.library_screen.link_cover_query, "match");
            assert_eq!(
                this.cover_regions[&CoverOwner::LibraryDialog].audit_visible(),
                &HashSet::from([(71, 32, 44)])
            );
        });
        db.upsert_media(&media(71, "Former")).unwrap();
        db.upsert_media(&media(72, "Match")).unwrap();
        db.set_setting("library_bindings", "{").unwrap();
        let expected_error = runtime.block_on(observer.scan_library()).unwrap_err();
        assert_eq!(
            expected_error,
            "EOF while parsing an object at line 1 column 1"
        );
        workspace.update_in(cx, |this, window, cx| {
            this.refresh_library_entries(window, cx)
        });
        settle(cx);
        workspace.read_with(cx, |this, _| {
            assert_eq!(this.library_screen.scan_error, expected_error);
            assert!(this.library_screen.list_error.is_empty());
            assert_eq!(
                link_candidates(&this.library_screen.entries, "match")
                    .iter()
                    .map(|entry| entry.media_id)
                    .collect::<Vec<_>>(),
                [72]
            );
            assert_eq!(
                this.cover_regions[&CoverOwner::LibraryDialog].audit_visible(),
                &HashSet::from([(72, 32, 44)]),
                "Fresh candidates must own covers when the scan fails"
            );
        });
        db.set_setting("library_bindings", "{}").unwrap();
        db.upsert_media(&media(71, "Match")).unwrap();
        db.upsert_media(&media(72, "Other")).unwrap();
        workspace.update_in(cx, |this, window, cx| {
            this.refresh_library_entries(window, cx)
        });
        settle(cx);
        workspace.read_with(cx, |this, _| {
            assert!(this.library_screen.scan_error.is_empty());
            assert!(this.library_screen.scan_loaded);
            assert_eq!(
                this.cover_regions[&CoverOwner::LibraryDialog].audit_visible(),
                &HashSet::from([(71, 32, 44)]),
                "Successful retry must update candidates again"
            );
        });
        workspace.update_in(cx, |this, _, _| this.cancel_all_reads());
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(observer);
        drop(runtime);
        drop(db);
    }

    #[gpui::test]
    fn link_dialog_cover_owners_follow_sorted_and_filtered_candidates(
        cx: &mut gpui::TestAppContext,
    ) {
        struct NoExternal;
        impl Platform for NoExternal {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected path")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected reveal")
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::from_db(
            kurisu_core::Db::open(Path::new(":memory:")).unwrap(),
            std::env::temp_dir(),
            runtime.handle().clone(),
            Arc::new(NoExternal),
            kurisu_core::UpdateChannel::Native,
        );
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |this, window, cx| {
            this.route = Route::Library;
            this.library_screen.bind = Some(BindDialog {
                id: 0,
                revision: 0,
                path: "/controlled/episode.mkv".into(),
                media_id: Some(1),
                title: "Synthetic".into(),
                mode: BindMode::Link,
                scope: BindScope::File,
                request: 1,
                current: Some(1.into()),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
                offset_edited: false,
            });
            this.library_screen.entries = (1..=101)
                .map(|id| ListEntry {
                    media_id: id,
                    status: if id == 101 { "CURRENT" } else { "PLANNING" }.into(),
                    media: Some(kurisu_core::models::Media {
                        id,
                        title_english: Some(format!("Show {id:03}")),
                        cover_medium: Some(format!("invalid-cover-{id}")),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
                .collect();
            let input = cx.new(|cx| TextInput::new("", "Search", window, cx));
            this.library_screen.link_query = Some(input.clone());
            this.prefetch_library_link_covers(cx);
            let owner = CoverOwner::LibraryDialog;
            let keys = this.cover_regions.get(&owner).unwrap().audit_visible();
            assert_eq!(keys.len(), 100);
            assert!(
                keys.contains(&(101, 32, 44)),
                "First displayed candidate needs a cover owner"
            );
            assert!(
                !keys.contains(&(100, 32, 44)),
                "Candidate beyond display limit must retire"
            );
            input.update(cx, |input, cx| input.set_value("Show 100", cx));
            this.prefetch_library_link_covers(cx);
            assert_eq!(
                this.cover_regions
                    .get(&owner)
                    .unwrap()
                    .audit_visible()
                    .len(),
                1
            );
            assert!(
                this.cover_regions
                    .get(&owner)
                    .unwrap()
                    .audit_visible()
                    .contains(&(100, 32, 44))
            );
            this.refresh_library_covers();
            let keys = this.cover_regions.get(&owner).unwrap().audit_visible();
            assert_eq!(
                keys.len(),
                1,
                "Route refresh must preserve the active candidate query"
            );
            assert!(keys.contains(&(100, 32, 44)));
        });
        cx.update(|window, _| window.remove_window());
        drop(workspace);
        drop(runtime);
    }
    #[gpui::test]
    fn empty_library_keeps_compact_folder_and_history_spacing(cx: &mut gpui::TestAppContext) {
        struct NoExternal;
        impl Platform for NoExternal {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected path")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected reveal")
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::from_db(
            kurisu_core::Db::open(Path::new(":memory:")).unwrap(),
            std::env::temp_dir(),
            runtime.handle().clone(),
            Arc::new(NoExternal),
            kurisu_core::UpdateChannel::Native,
        );
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.profile = Some(kurisu_core::User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Library;
            workspace.library_screen.folders_loaded = true;
            cx.notify();
        });
        for width in [1100., 800.] {
            cx.simulate_resize(gpui::size(
                px(width),
                px(if width <= 800. { 600. } else { 750. }),
            ));
            cx.run_until_parked();
            cx.update(|window, app| window.draw(app).clear(app));
            let heading = cx.debug_bounds("library-folders-heading").unwrap();
            let rule = cx.debug_bounds("library-folders-rule").unwrap();
            let hint = cx.debug_bounds("library-empty-hint").unwrap();
            let add = cx.debug_bounds("library-add-folder").unwrap();
            let history = cx.debug_bounds("library-history-section").unwrap();
            assert!(rule.size.width > px(200.));
            assert!(hint.top() - heading.top() >= px(25.));
            assert!(hint.top() - heading.top() <= px(38.));
            assert!(add.top() - hint.top() >= px(20.));
            assert!(add.top() - hint.top() <= px(30.));
            assert!(add.size.height >= px(32.) && add.size.height <= px(35.));
            assert!(history.top() - add.bottom() >= px(22.));
            assert!(history.top() - add.bottom() <= px(26.));
        }
    }

    #[test]
    fn library_title_groups_keep_collation_ties_in_file_order() {
        let files: Vec<_> = ["Éclair", "Eclair", "Episode 10", "Episode 2", "Zebra"]
            .into_iter()
            .enumerate()
            .map(|(index, title)| LibraryFile {
                path: format!("/synthetic/{index}.mkv"),
                media_id: Some(index as i64 + 1),
                matched: Some(title.into()),
                episode: Some(1),
                bound: false,
            })
            .collect();
        let ids: Vec<_> = group_library_rows(&files, &[], &HashSet::new())
            .into_iter()
            .map(|row| match row {
                LibraryRow::Group(group) => group.media_id,
                _ => panic!("Expected a collapsed group"),
            })
            .collect();
        assert_eq!(ids, [1, 2, 4, 3, 5]);
    }

    #[gpui::test]
    fn invalid_numbering_offset_keeps_the_dialog_and_starts_no_write(
        cx: &mut gpui::TestAppContext,
    ) {
        struct NoExternal;
        impl Platform for NoExternal {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected path")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected reveal")
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::from_db(
            kurisu_core::Db::open(Path::new(":memory:")).unwrap(),
            std::env::temp_dir(),
            runtime.handle().clone(),
            Arc::new(NoExternal),
            kurisu_core::UpdateChannel::Native,
        );
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |this, window, cx| {
            this.profile = Some(kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            this.route = Route::Library;
            this.library_screen.bind = Some(BindDialog {
                id: 0,
                revision: 0,
                path: "/controlled/episode.mkv".into(),
                media_id: Some(1),
                title: "Synthetic".into(),
                mode: BindMode::Numbering,
                scope: BindScope::File,
                request: 1,
                current: Some(1.into()),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
                offset_edited: false,
            });
            let input = cx.new(|cx| TextInput::new("", "offset", window, cx));
            this.library_screen.offset = Some(input.clone());
            for raw in [
                "-9223372036854775808",
                "9223372036854775807",
                "-10000",
                "10000",
                "invalid",
            ] {
                input.update(cx, |input, cx| input.set_value(raw, cx));
                this.library_screen.bind.as_mut().unwrap().error.clear();
                this.save_library_bind(1, window, cx);
                let bind = this.library_screen.bind.as_ref().unwrap();
                assert!(!bind.error.is_empty());
                assert!(!bind.busy);
                assert_eq!(this.active_writes, 0);
                assert_eq!(input.read(cx).value(), raw);
            }
        });
    }

    #[gpui::test]
    fn binding_titles_use_cached_media_and_reject_old_scope(cx: &mut gpui::TestAppContext) {
        struct NoExternalActions;
        impl Platform for NoExternalActions {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected external URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected file open")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected file reveal")
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let db = kurisu_core::Db::open(Path::new(":memory:")).unwrap();
        for (id, title) in [(1, "First cached title"), (2, "Second cached title")] {
            db.upsert_media(&kurisu_core::models::Media {
                id,
                title_english: Some(title.into()),
                ..Default::default()
            })
            .unwrap();
        }
        let core = kurisu_core::Core::from_db(
            db,
            std::env::temp_dir(),
            runtime.handle().clone(),
            Arc::new(NoExternalActions),
            kurisu_core::UpdateChannel::Native,
        );
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |this, window, cx| {
            this.auth_intent += 1;
            this.profile = Some(kurisu_core::User {
                id: 7,
                name: "Synthetic".into(),
                ..Default::default()
            });
            this.library_screen.bind = Some(BindDialog {
                id: 0,
                revision: 0,
                path: "/controlled/episode.mkv".into(),
                media_id: None,
                title: String::new(),
                mode: BindMode::Link,
                scope: BindScope::File,
                request: 1,
                current: Some(1.into()),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
                offset_edited: false,
            });
            this.load_library_binding_title(1, window, cx);
        });
        cx.run_until_parked();
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        workspace.update_in(cx, |this, window, cx| {
            assert!(this.read_aborts["library-binding"][0].is_finished());
            let bind = this.library_screen.bind.as_mut().unwrap();
            bind.request += 1;
            bind.current.as_mut().unwrap().media_id = 2;
            this.load_library_binding_title(2, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |this, _| {
            assert!(
                this.library_screen
                    .bind
                    .as_ref()
                    .unwrap()
                    .current_title
                    .is_none()
            );
        });
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        cx.run_until_parked();
        workspace.read_with(cx, |this, _| {
            assert_eq!(
                this.library_screen
                    .bind
                    .as_ref()
                    .unwrap()
                    .current_title
                    .as_deref(),
                Some("Second cached title")
            );
        });
        workspace.update_in(cx, |this, window, cx| {
            this.library_screen.bind.as_mut().unwrap().current_title = None;
            this.load_library_binding_title(2, window, cx);
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !this.read_aborts["library-binding"][0].is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            });
            this.request_close(super::super::CloseIntent::Quit, window, cx);
            assert!(this.stopping);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |this, _| {
            let bind = this.library_screen.bind.as_ref().unwrap();
            assert!(bind.current_title.is_none());
        });
    }

    #[test]
    fn shared_library_scan_uses_sixty_second_age_and_list_revision() {
        let now = Instant::now();
        let mut state = LibraryState::default();
        assert!(state.scan_due(7, now));
        state.scan_loaded = true;
        state.list_revision = 7;
        state.last_scan = Some(now);
        assert!(!state.scan_due(7, now + Duration::from_secs(59)));
        assert!(state.scan_due(7, now + Duration::from_secs(60)));
        assert!(state.scan_due(8, now + Duration::from_secs(1)));
    }

    #[test]
    fn grouped_rows_keep_episode_order_and_unmatched_files() {
        let files = vec![
            LibraryFile {
                path: "/controlled/season10/ep3.mkv".into(),
                media_id: Some(10),
                matched: Some("Season 10".into()),
                episode: Some(3),
                bound: true,
            },
            LibraryFile {
                path: "/controlled/season2/ep2.mkv".into(),
                media_id: Some(2),
                matched: Some("Season 2".into()),
                episode: Some(2),
                bound: false,
            },
            LibraryFile {
                path: "/controlled/season2/ep1.mkv".into(),
                media_id: Some(2),
                matched: Some("Season 2".into()),
                episode: Some(1),
                bound: false,
            },
            LibraryFile {
                path: "/controlled/unknown.mkv".into(),
                ..Default::default()
            },
        ];
        let entries = vec![ListEntry {
            media_id: 2,
            progress: 1,
            status: "CURRENT".into(),
            ..Default::default()
        }];
        let rows = group_library_rows(&files, &entries, &HashSet::from([2]));
        assert_eq!(rows.len(), 6);
        assert!(
            matches!(&rows[0], LibraryRow::Group(group) if group.media_id == 2 && group.entry.as_ref().is_some_and(|entry| entry.progress == 1))
        );
        assert!(matches!(&rows[1], LibraryRow::File(file, 2, 1, _) if file.episode == Some(1)));
        assert!(matches!(&rows[2], LibraryRow::File(file, 2, 1, _) if file.episode == Some(2)));
        assert!(
            matches!(&rows[3], LibraryRow::Group(group) if group.media_id == 10 && group.bound)
        );
        assert!(matches!(&rows[4], LibraryRow::UnmatchedHeader(1)));
        assert!(
            matches!(&rows[5], LibraryRow::Unmatched(file) if file.path.ends_with("unknown.mkv"))
        );
    }

    #[test]
    fn binding_scope_never_uses_library_root() {
        let mut bind = BindDialog {
            id: 0,
            revision: 0,
            path: "/controlled/root/season/ep13.mkv".into(),
            media_id: Some(2),
            title: String::new(),
            mode: BindMode::Numbering,
            scope: BindScope::Folder,
            request: 0,
            current: None,
            current_title: None,
            checked: false,
            loading: false,
            busy: false,
            error: String::new(),
            relink: None,
            restored: false,
            offset_edited: false,
        };
        let roots = ["/controlled/root".into()];
        assert_eq!(bind.target(&roots, false), None);
        assert_eq!(
            bind.target(&roots, true),
            Some("/controlled/root/season".into())
        );
        bind.path = "/controlled/root/ep13.mkv".into();
        assert_eq!(bind.target(&roots, true), None);
        bind.scope = BindScope::File;
        assert_eq!(bind.target(&roots, true), Some(bind.path.clone()));
    }

    #[gpui::test]
    fn fenced_library_scan_completion_preserves_pending_reload_until_unfence(
        cx: &mut gpui::TestAppContext,
    ) {
        struct NoExternal;
        impl Platform for NoExternal {
            fn open_url(&self, _: &str) -> Result<(), String> {
                panic!("Unexpected URL")
            }
            fn open_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected path")
            }
            fn reveal_path(&self, _: &Path) -> Result<(), String> {
                panic!("Unexpected reveal")
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kurisu-library-fenced-scan-{}-{nonce}",
            std::process::id()
        ));
        let anime_dir = root.join("anime");
        std::fs::create_dir_all(&anime_dir).unwrap();
        std::fs::write(anime_dir.join("Show - 01.mkv"), b"").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = kurisu_core::Core::open(
            root.clone(),
            runtime.handle().clone(),
            Arc::new(NoExternal),
            kurisu_core::UpdateChannel::Native,
        )
        .unwrap();
        runtime
            .block_on(core.add_library_folder(anime_dir.to_string_lossy().into_owned()))
            .unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let first_scan = workspace.update_in(cx, |w, window, cx| {
            w.restoring_session = false;
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.route = Route::Library;
            w.refresh_library_entries(window, cx);
            assert!(w.library_screen.scanning);
            w.read_aborts["library-data"][0].clone()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !first_scan.is_finished() {
            assert!(Instant::now() < deadline);
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        std::fs::write(anime_dir.join("Show - 02.mkv"), b"").unwrap();
        workspace.update_in(cx, |w, window, cx| {
            w.refresh_library_entries(window, cx);
            assert!(w.library_screen.refresh_after_scan);
            w.query_error = "hold barrier".into();
            w.request_close(super::super::CloseIntent::Window, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(!w.library_screen.scanning);
            assert_eq!(w.library_screen.files.len(), 1);
            assert!(
                !w.library_screen.scan_loaded,
                "fenced library scan completion left scan_loaded == true despite pending reload"
            );
        });
        workspace.update_in(cx, |w, _, _| w.query_error.clear());
        cx.update(|win, app| win.draw(app).clear(app));
        let cancel = cx.debug_bounds("cancel-failed-barrier").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        while workspace.read_with(cx, |w, _| !w.library_screen.scan_loaded) {
            assert!(Instant::now() < deadline);
            cx.run_until_parked();
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.library_screen.files.len(), 2);
        });

        let second_scan = workspace.update_in(cx, |w, window, cx| {
            w.refresh_library_entries(window, cx);
            assert!(w.library_screen.scanning);
            w.read_aborts["library-data"][0].clone()
        });
        while !second_scan.is_finished() {
            assert!(Instant::now() < deadline);
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        std::fs::write(anime_dir.join("Show - 03.mkv"), b"").unwrap();
        workspace.update_in(cx, |w, window, cx| {
            w.list.apply_one(
                ListEntry {
                    media_id: 1,
                    status: "CURRENT".into(),
                    ..Default::default()
                },
                0,
            );
            w.request_close(super::super::CloseIntent::Window, window, cx);
            assert!(w.close_ready);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.library_screen.files.len(), 2);
            assert!(!w.library_screen.scan_loaded);
        });
        workspace.update_in(cx, |w, window, cx| {
            w.resume_after_close_visibility(true, window, cx);
        });
        while workspace.read_with(cx, |w, _| !w.library_screen.scan_loaded) {
            assert!(Instant::now() < deadline);
            cx.run_until_parked();
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.library_screen.files.len(), 3);
        });
        cx.update(|win, _| win.remove_window());
        drop(workspace);
        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bind_dialog_rejects_filesystem_root() {
        let mut dialog = BindDialog {
            id: 0,
            revision: 0,
            path: "/Show - 01.mkv".into(),
            media_id: None,
            title: String::new(),
            mode: BindMode::Link,
            scope: BindScope::Folder,
            request: 0,
            current: None,
            current_title: None,
            checked: false,
            loading: false,
            busy: false,
            error: String::new(),
            relink: None,
            restored: false,
            offset_edited: false,
        };
        assert!(!dialog.folder_allowed(&[]));
        let (_core, _runtime, dir) = super::super::workspace_tests::fixture();
        let root = dir.0.join("library root");
        let show = root.join("show");
        std::fs::create_dir_all(&show).unwrap();
        let roots = [reqwest::Url::from_directory_path(&root)
            .unwrap()
            .to_string()];
        dialog.path = root.join("Episode 01.mkv").to_str().unwrap().into();
        assert!(!dialog.folder_allowed(&roots));
        assert_eq!(dialog.target(&roots, true), None);
        dialog.path = show.join("Episode 01.mkv").to_str().unwrap().into();
        assert_eq!(dialog.target(&roots, true).as_deref(), show.to_str());
    }
}

#[cfg(test)]
mod modal_regressions {
    use super::super::workspace_tests::{TestPlatform, fixture, settle};
    use super::*;
    use gpui::accesskit::{Action, ActionRequest, TreeId};
    use kurisu_core::{Core, Db, UpdateChannel, User};

    #[gpui::test]
    fn library_cancel_button_and_escape_share_the_edit_fence(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Library;
            w.library_screen.remove_folder = Some("/fixture/folder".into());
            w.auth_busy = true;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Cancel") && !node.is_hidden() && node.is_disabled()
        }));
        view.update_in(cx, |w, win, cx| {
            w.escape_active_modal(win, cx);
            assert_eq!(w.active_modal(), Some(ModalLayer::LibraryRemove));
            w.auth_busy = false;
            w.escape_active_modal(win, cx);
            assert!(w.active_modal().is_none());
        });
    }

    #[gpui::test]
    fn library_dialog_stays_fixed_and_blocks_pending_actions(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        cx.simulate_resize(gpui::size(px(1000.), px(700.)));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.route = Route::Library;
            w.pending_error = "Sync status fixture".into();
            w.library_screen.folders = (0..32).map(|i| format!("/fixture/folder-{i:02}")).collect();
            w.library_screen.folders_loaded = true;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let target = |tree: &gpui::accesskit::TreeUpdate, label| {
            tree.nodes
                .iter()
                .find(|(_, node)| node.label() == Some(label))
                .unwrap()
                .0
        };
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|win, app| win.draw(app).clear(app));
            cx.take_accessibility_tree_update().unwrap()
        };
        let retry = target(&draw(cx), "Retry sync");
        let modal_bounds = |tree: &gpui::accesskit::TreeUpdate| {
            tree.nodes
                .iter()
                .find(|(_, node)| node.is_modal())
                .unwrap()
                .1
                .bounds()
                .unwrap()
        };
        let retry_position = cx.debug_bounds("pending-retry").unwrap().center();
        view.update_in(cx, |w, _, cx| {
            w.library_screen.remove_folder = w.library_screen.folders.get(4).cloned();
            cx.notify();
        });
        let before_scroll = modal_bounds(&draw(cx));
        view.update_in(cx, |w, _, cx| {
            w.library_screen
                .scroll
                .set_offset(gpui::point(px(0.), px(-80.)));
            cx.notify();
        });
        let after_scroll = draw(cx);
        assert!(view.read_with(cx, |w, _| w.library_screen.scroll.offset().y < px(0.)));
        assert_eq!(modal_bounds(&after_scroll), before_scroll);
        assert!(after_scroll.nodes.iter().any(|(_, node)| {
            node.label() == Some("Retry sync") && node.is_hidden() && node.is_disabled()
        }));
        cx.simulate_click(retry_position, gpui::Modifiers::default());
        cx.simulate_accessibility_action(ActionRequest {
            action: Action::Click,
            target_tree: TreeId::ROOT,
            target_node: retry,
            data: None,
        });
        view.update_in(cx, |w, win, cx| {
            w.retry_pending(win, cx);
            assert!(!w.pending_busy && w.active_writes == 0);
        });
    }

    #[gpui::test]
    fn numbering_load_replays_after_close_and_auth_with_the_existing_offset(
        cx: &mut gpui::TestAppContext,
    ) {
        #[cfg(feature = "fixture-runtime")]
        if super::super::workspace_tests::prompt_restore_tests::fixture_server("route").is_none() {
            return;
        }
        let (initial, runtime, dir) = fixture();
        drop(initial);
        let db = Db::open(&dir.0.join("kurisu.db")).unwrap();
        let user = User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        db.replace_account("synthetic-token", &user).unwrap();
        db.upsert_media(&kurisu_core::models::Media {
            id: 9,
            title_english: Some("Show".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let core = Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        let folder = dir.0.join("shows");
        let show_folder = folder.join("Show");
        std::fs::create_dir_all(&show_folder).unwrap();
        let file = show_folder.join("Show - 03.mkv");
        std::fs::write(&file, b"").unwrap();
        let path = file.to_string_lossy().into_owned();
        runtime
            .block_on(core.add_library_folder(folder.to_string_lossy().into_owned()))
            .unwrap();
        runtime
            .block_on(core.bind_library_path(path.clone(), 9, Some(5), BindScope::File))
            .unwrap();
        runtime
            .block_on(core.bind_library_path(
                show_folder.to_string_lossy().into_owned(),
                9,
                Some(7),
                BindScope::Folder,
            ))
            .unwrap();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        view.update_in(cx, |w, win, cx| {
            w.profile = Some(user);
            w.offline = true;
            w.route = Route::Library;
            w.enter_library(win, cx);
            w.open_library_bind(
                path.clone(),
                Some((9, "Show".into())),
                BindMode::Numbering,
                win,
                cx,
            );
            assert!(w.library_screen.bind.as_ref().unwrap().loading);
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(w.close_ready);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let wait_for_read = |cx: &mut gpui::VisualTestContext| {
            while view.read_with(cx, |w, _| w.library_screen.bind.as_ref().unwrap().loading) {
                assert!(Instant::now() < deadline);
                settle(cx, &runtime);
            }
        };
        let assert_offset = |cx: &mut gpui::VisualTestContext, stored, input| {
            view.read_with(cx, |w, cx| {
                let bind = w.library_screen.bind.as_ref().unwrap();
                assert!(bind.checked);
                assert_eq!(bind.current.unwrap().episode_offset, stored);
                assert_eq!(
                    w.library_screen.offset.as_ref().unwrap().read(cx).value(),
                    input
                );
            });
        };
        wait_for_read(cx);
        view.update_in(cx, |w, win, cx| {
            let bind = w.library_screen.bind.as_ref().unwrap();
            assert!(!bind.checked);
            let request = bind.request;
            w.resume_after_close_visibility(false, win, cx);
            assert_eq!(w.library_screen.bind.as_ref().unwrap().request, request + 1);
        });
        wait_for_read(cx);
        assert_offset(cx, 5, "5");
        #[cfg(feature = "fixture-runtime")]
        {
            use gpui::EntityInputHandler;

            let begin_auth = |w: &mut Workspace| {
                w.auth_busy = true;
                w.auth_abort = Some(w.core.spawn(std::future::pending::<()>()).abort_handle());
            };
            let cancel_auth = |cx: &mut gpui::VisualTestContext| {
                let generation = view.update_in(cx, |w, win, cx| {
                    w.cancel_login(win, cx);
                    w.generation
                });
                while !view.read_with(cx, |w, _| {
                    w.generation != generation
                        && w.library_screen
                            .bind
                            .as_ref()
                            .is_some_and(|bind| bind.checked)
                }) {
                    assert!(Instant::now() < deadline, "restored numbering did not load");
                    settle(cx, &runtime);
                }
            };
            view.update_in(cx, |w, win, cx| {
                w.dismiss_library_dialog(cx);
                w.open_library_bind(path, Some((9, "Show".into())), BindMode::Numbering, win, cx);
                begin_auth(w);
            });
            wait_for_read(cx);
            view.read_with(cx, |w, cx| {
                assert!(!w.library_screen.bind.as_ref().unwrap().checked);
                assert_eq!(
                    w.library_screen.offset.as_ref().unwrap().read(cx).value(),
                    "0"
                );
            });
            cancel_auth(cx);
            assert_offset(cx, 5, "5");

            view.update_in(cx, |w, win, cx| {
                w.library_screen
                    .offset
                    .as_ref()
                    .unwrap()
                    .update(cx, |input, cx| {
                        input.replace_text_in_range(Some(0..1), "-2", win, cx);
                    });
            });
            view.update_in(cx, |w, _, _| {
                assert!(w.library_screen.bind.as_ref().unwrap().offset_edited);
                begin_auth(w);
            });
            cancel_auth(cx);
            assert_offset(cx, 5, "-2");

            while !view.read_with(cx, |w, _| w.library_screen.folders_loaded) {
                assert!(Instant::now() < deadline);
                settle(cx, &runtime);
            }
            view.update_in(cx, |w, win, cx| {
                w.change_bind_scope(BindScope::Folder, win, cx)
            });
            wait_for_read(cx);
            assert_offset(cx, 7, "7");
            view.update_in(cx, |w, win, cx| {
                w.change_bind_scope(BindScope::File, win, cx)
            });
            wait_for_read(cx);
            assert_offset(cx, 5, "5");
        }
    }

    #[gpui::test]
    fn deferred_library_scan_runs_on_non_library_route_after_fence_clears(
        cx: &mut gpui::TestAppContext,
    ) {
        let (core, runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        let user = User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(5);

        view.update_in(cx, |w, win, cx| {
            w.route = Route::MyList;
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(w.close_ready);
            w.set_profile_at(Some(user.clone()), w.generation + 1, win, cx);
            assert!(w.route_entry_deferred);
            assert!(w.library_screen.refresh_after_scan);
            assert!(!w.library_screen.scanning);
            assert!(!w.library_screen.scan_loaded);
            w.resume_after_close(win, cx);
            assert!(w.library_screen.scanning);
        });
        while !view.read_with(cx, |w, _| {
            w.library_screen.scan_loaded && !w.library_screen.scanning
        }) {
            assert!(
                Instant::now() < deadline,
                "deferred scan did not complete after fence cleared"
            );
            settle(cx, &runtime);
        }

        view.update_in(cx, |w, win, cx| {
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(w.close_ready);
            w.enter_active_route(win, cx);
            assert!(w.route_entry_deferred);
            w.refresh_library_entries(win, cx);
            assert!(w.library_screen.refresh_after_scan);
            assert!(!w.library_screen.scanning);
            w.resume_after_close(win, cx);
            assert!(w.library_screen.scanning);
        });
        while !view.read_with(cx, |w, _| {
            w.library_screen.scan_loaded && !w.library_screen.scanning
        }) {
            assert!(
                Instant::now() < deadline,
                "refresh_after_scan did not run after deferred route entry"
            );
            settle(cx, &runtime);
        }
    }

    #[gpui::test]
    fn save_library_bind_retires_stashed_and_unmodified_restored_drafts_only(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::EntityInputHandler;

        let (initial, runtime, dir) = fixture();
        drop(initial);
        let db = Db::open(&dir.0.join("kurisu.db")).unwrap();
        let user = User {
            id: 7,
            offline: true,
            ..Default::default()
        };
        db.replace_account("synthetic-token", &user).unwrap();
        db.upsert_media(&kurisu_core::models::Media {
            id: 9,
            title_english: Some("Show".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let core = Core::open(
            dir.0.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
            UpdateChannel::Native,
        )
        .unwrap();
        let folder = dir.0.join("shows");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("Show - 03.mkv");
        std::fs::write(&file, b"").unwrap();
        let path = file.to_string_lossy().into_owned();
        runtime
            .block_on(core.add_library_folder(folder.to_string_lossy().into_owned()))
            .unwrap();

        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        let deadline = Instant::now() + Duration::from_secs(5);
        let wait_writes = |cx: &mut gpui::VisualTestContext| {
            while view.read_with(cx, |w, _| w.active_writes > 0) {
                assert!(Instant::now() < deadline, "library bind write timed out");
                settle(cx, &runtime);
            }
        };
        let open_ready_bind = |w: &mut Workspace,
                               win: &mut gpui::Window,
                               cx: &mut Context<Workspace>,
                               target: String| {
            w.route = Route::Library;
            w.enter_library(win, cx);
            w.open_library_bind(
                target,
                Some((9, "Show".into())),
                BindMode::Numbering,
                win,
                cx,
            );
            w.cancel_reads("library-binding");
            let bind = w.library_screen.bind.as_mut().unwrap();
            bind.loading = false;
            bind.checked = true;
            w.library_screen
                .set_binding_input(OFFSET_INPUT, "2".into(), cx);
        };

        view.update_in(cx, |w, win, cx| {
            w.set_profile_at(Some(user.clone()), w.generation + 1, win, cx);
            open_ready_bind(w, win, cx, path.clone());
            w.save_library_bind(9, win, cx);
            assert_eq!(w.active_writes, 1);
            w.set_profile_at(None, w.generation + 1, win, cx);
            assert!(w.library_drafts.contains_key(&7));
            assert!(w.has_inactive_drafts());
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(!w.close_ready);
        });
        wait_writes(cx);
        view.update_in(cx, |w, win, cx| {
            assert!(!w.library_drafts.contains_key(&7));
            assert!(!w.has_inactive_drafts());
            assert!(w.close_ready);
            w.resume_after_close(win, cx);
            w.set_profile_at(Some(user.clone()), w.generation + 1, win, cx);
            assert!(w.library_screen.pending_draft.is_none());
            assert!(w.library_screen.bind.is_none());
        });

        view.update_in(cx, |w, win, cx| {
            open_ready_bind(w, win, cx, path.clone());
            w.save_library_bind(9, win, cx);
            w.set_profile_at(Some(user.clone()), w.generation + 1, win, cx);
            assert!(w.library_screen.bind.as_ref().is_some_and(|b| b.restored));
        });
        wait_writes(cx);
        view.read_with(cx, |w, _| {
            assert!(w.library_screen.bind.is_none());
            assert!(w.library_drafts.is_empty());
        });

        view.update_in(cx, |w, win, cx| {
            open_ready_bind(w, win, cx, path.clone());
            w.save_library_bind(9, win, cx);
            w.set_profile_at(Some(user.clone()), w.generation + 1, win, cx);
            w.cancel_reads("library-binding");
            let bind = w.library_screen.bind.as_mut().unwrap();
            bind.loading = false;
            bind.checked = true;
            cx.notify();
        });
        cx.update(|win, app| win.draw(app).clear(app));
        view.update_in(cx, |w, win, cx| {
            w.library_screen
                .offset
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| {
                    input.replace_text_in_range(Some(0..input.value().len()), "4", win, cx);
                    assert_eq!(input.value(), "4");
                });
        });
        wait_writes(cx);
        view.read_with(cx, |w, cx| {
            assert!(w.library_screen.bind.is_some());
            assert_eq!(
                w.library_screen.offset.as_ref().unwrap().read(cx).value(),
                "4"
            );
        });

        view.update_in(cx, |w, win, cx| {
            w.dismiss_library_dialog(cx);
            open_ready_bind(w, win, cx, path.clone());
            w.library_screen.bind.as_mut().unwrap().mode = BindMode::Link;
            w.save_library_bind(999, win, cx);
            w.set_profile_at(None, w.generation + 1, win, cx);
            assert!(w.library_drafts.contains_key(&7));
        });
        wait_writes(cx);
        view.read_with(cx, |w, _| {
            assert!(w.library_drafts.contains_key(&7));
        });
    }
}
