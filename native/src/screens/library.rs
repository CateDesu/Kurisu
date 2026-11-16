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
    Platform,
    models::{LibraryBinding, LibraryFile, ListEntry, UnreadableFolder, WatchHistoryItem},
};

use super::{
    ModalLayer, Workspace, covers::CoverOwner, joined, overlays::modal_container,
    state::compare_titles,
};
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum BindScope {
    File,
    Folder,
}
struct BindDialog {
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
    fn folder(&self) -> String {
        Path::new(&self.path)
            .parent()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
    fn folder_allowed(&self, roots: &[String]) -> bool {
        let folder = self.folder();
        !folder.is_empty()
            && !roots
                .iter()
                .any(|root| root.trim_end_matches(['/', '\\']) == folder)
    }
    fn target(&self, roots: &[String]) -> String {
        if self.scope == BindScope::Folder && self.folder_allowed(roots) {
            self.folder()
        } else {
            self.path.clone()
        }
    }
}

pub(super) struct LibraryState {
    folders: Vec<String>,
    folders_loaded: bool,
    folders_error: String,
    files: Vec<LibraryFile>,
    unreadable: Vec<UnreadableFolder>,
    scan_loaded: bool,
    scanning: bool,
    refresh_after_scan: bool,
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
    history_error: String,
    history_more: bool,
    history_request: u64,
    data_request: u64,
    list_revision: u64,
    bind: Option<BindDialog>,
    pending_draft: Option<LibraryDraft>,
    bind_request: u64,
    link_query: Option<Entity<TextInput>>,
    binding_candidates: Option<Arc<BindingCandidates>>,
    link_cover_query: String,
    offset: Option<Entity<TextInput>>,
    subscriptions: Vec<Subscription>,
    remove_folder: Option<String>,
    unlink_media: Option<i64>,
    busy: bool,
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
            subscriptions: Vec::new(),
            remove_folder: None,
            unlink_media: None,
            busy: false,
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
        match self.library_modal_layer() {
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
            self.library_screen
                .subscriptions
                .push(cx.subscribe(&query, |this, _, event, cx| {
                    if matches!(event, TextInputEvent::Changed(_)) {
                        this.prefetch_library_link_covers(cx);
                        cx.notify();
                    }
                }));
            self.library_screen
                .subscriptions
                .push(cx.subscribe(&offset, |_, _, event, cx| {
                    if matches!(event, TextInputEvent::Changed(_)) {
                        cx.notify();
                    }
                }));
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
            if let Some(input) = &self.library_screen.link_query {
                self.library_screen.link_cover_query = draft.query.trim().to_lowercase();
                input.update(cx, |input, cx| input.set_value(draft.query, cx));
            }
            if let Some(input) = &self.library_screen.offset {
                input.update(cx, |input, cx| input.set_value(draft.offset, cx));
            }
            self.library_screen.bind = Some(draft.bind);
            self.library_screen.bind_request += 1;
            self.load_library_binding(window, cx);
        }
        self.ensure_library_fresh(window, cx);
        if self.library_screen.history_open && !self.library_screen.history_loading {
            self.load_library_history(false, window, cx);
        }
        self.refresh_library_covers();
    }

    pub(super) fn take_library_draft(&mut self, cx: &gpui::App) -> Option<LibraryDraft> {
        let mut bind = self.library_screen.bind.take()?;
        bind.busy = false;
        let query = self
            .library_screen
            .link_query
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let offset = self
            .library_screen
            .offset
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_else(|| "0".into());
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
        self.library_screen.scan_loaded = false;
        if self.library_screen.scanning {
            self.library_screen.refresh_after_scan = true;
            return;
        }
        if self.route == Route::Library {
            self.refresh_library_data(window, cx);
        }
    }

    pub(super) fn ensure_library_fresh(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.library_screen.scanning {
            return;
        }
        if self
            .library_screen
            .scan_due(self.list.revision(), Instant::now())
        {
            self.refresh_library_data(window, cx);
        }
    }

    fn refresh_library_data(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.library_screen.scanning {
            return;
        }
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
                if this.generation != generation
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
                    this.refresh_library_data(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn pick_library_folder(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.busy || self.profile.is_none() || self.barrier.is_some() {
            return;
        }
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
                        cx.notify();
                    }
                    Err(error) => {
                        this.library_screen.action_error = error.to_string();
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn add_library_folder(&mut self, path: String, window: &Window, cx: &mut Context<Self>) {
        if self.library_screen.busy || self.barrier.is_some() {
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
            || self.barrier.is_some()
            || self.library_screen.remove_folder.as_deref() != Some(path.as_str())
        {
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
        let folder = Path::new(&path)
            .parent()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let allowed = !folder.is_empty()
            && !self
                .library_screen
                .folders
                .iter()
                .any(|root| root.trim_end_matches(['/', '\\']) == folder);
        self.library_screen.bind_request += 1;
        self.library_screen.bind = Some(BindDialog {
            path,
            media_id: media.as_ref().map(|value| value.0),
            title: media.map(|value| value.1).unwrap_or_default(),
            mode,
            scope: if mode == BindMode::Link && allowed {
                BindScope::Folder
            } else {
                BindScope::File
            },
            request: 0,
            current: None,
            current_title: None,
            checked: false,
            loading: false,
            busy: false,
            error: String::new(),
            relink: None,
            restored: false,
        });
        if let Some(input) = &self.library_screen.offset {
            input.update(cx, |input, cx| {
                input.set_value("0", cx);
                input.set_disabled(mode == BindMode::Numbering, cx);
            });
        }
        if let Some(input) = &self.library_screen.link_query {
            input.update(cx, |input, cx| input.set_value("", cx));
        }
        self.prefetch_library_link_covers(cx);
        self.load_library_binding(window, cx);
    }

    fn change_bind_scope(&mut self, scope: BindScope, window: &Window, cx: &mut Context<Self>) {
        if let Some(bind) = self.library_screen.bind.as_mut() {
            if bind.busy
                || bind.scope == scope
                || scope == BindScope::Folder && !bind.folder_allowed(&self.library_screen.folders)
            {
                return;
            }
            bind.scope = scope;
            bind.restored = false;
            bind.current = None;
            bind.checked = false;
            bind.relink = None;
            self.load_library_binding(window, cx);
        }
    }

    fn load_library_binding(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(bind) = self.library_screen.bind.as_mut() else {
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
        if mode == BindMode::Numbering
            && let Some(input) = &self.library_screen.offset
        {
            input.update(cx, |input, cx| input.set_disabled(true, cx));
        }
        let target = bind.target(&self.library_screen.folders);
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            if mode == BindMode::Link {
                Ok(worker
                    .library_binding_for(target)
                    .await?
                    .map(|media_id| LibraryBinding {
                        media_id,
                        episode_offset: 0,
                    }))
            } else {
                worker.library_binding_details(target).await
            }
        });
        self.replace_reads("library-binding", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.library_screen.bind_request != dialog_id
                {
                    return;
                }
                let Some(bind) = this.library_screen.bind.as_mut() else {
                    return;
                };
                if bind.request != request {
                    return;
                }
                bind.loading = false;
                match result {
                    Ok(current) => {
                        bind.current = current;
                        bind.checked = true;
                        if bind.mode == BindMode::Numbering
                            && !bind.restored
                            && let Some(input) = &this.library_screen.offset
                        {
                            input.update(cx, |input, cx| {
                                input.set_value(
                                    current.map_or(0, |item| item.episode_offset).to_string(),
                                    cx,
                                )
                            });
                        }
                    }
                    Err(error) => bind.error = error,
                }
                if bind.mode == BindMode::Numbering
                    && let Some(input) = &this.library_screen.offset
                {
                    input.update(cx, |input, cx| input.set_disabled(!bind.checked, cx));
                }
                if bind.checked
                    && let Some(current) = bind.current
                {
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
                if !scope_matches(this, generation, account)
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
        let Some(bind) = self.library_screen.bind.as_mut() else {
            return;
        };
        if !bind.checked
            || bind.busy
            || self.barrier.is_some()
            || self.auth_busy
            || self.update_busy
        {
            return;
        }
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
        let target = bind.target(&self.library_screen.folders);
        bind.busy = true;
        bind.error.clear();
        if let Some(input) = &self.library_screen.offset {
            input.update(cx, |input, cx| input.set_disabled(true, cx));
        }
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    worker
                        .bind_library_path(target, media_id, Some(offset))
                        .await
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.generation == generation
                    && this.profile.as_ref().map(|user| user.id) == account
                {
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
                            if let Some(input) = &this.library_screen.offset {
                                input.update(cx, |input, cx| input.set_disabled(false, cx));
                            }
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
            || self.barrier.is_some()
            || self.library_screen.unlink_media != Some(id)
        {
            return;
        }
        self.library_screen.busy = true;
        self.active_writes += 1;
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
        if self.profile.is_none() || self.library_screen.history_loading {
            return;
        }
        let before = if more {
            self.library_screen.history.last().map(|item| item.id)
        } else {
            None
        };
        self.library_screen.history_request += 1;
        let request = self.library_screen.history_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        self.library_screen.history_loading = true;
        self.library_screen.history_error.clear();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_watch_history(Some(50), before).await });
        self.replace_reads("library-history", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation != generation
                    || this.profile.as_ref().map(|user| user.id) != account
                    || this.library_screen.history_request != request
                {
                    return;
                }
                this.library_screen.history_loading = false;
                match result {
                    Ok(rows) => {
                        let old = this.library_screen.history_widget.logical_scroll_top();
                        let anchor = this
                            .library_screen
                            .history
                            .get(old.item_ix)
                            .map(|item| item.id);
                        this.library_screen.history_more = rows.len() == 50;
                        if more {
                            this.library_screen.history.extend(rows);
                        } else {
                            this.library_screen.history = rows;
                        }
                        this.library_screen
                            .history_widget
                            .reset_with_uniform_height(this.library_screen.history.len(), px(56.));
                        if let Some(id) = anchor
                            && let Some(index) = this
                                .library_screen
                                .history
                                .iter()
                                .position(|item| item.id == id)
                        {
                            this.library_screen.history_widget.scroll_to(ListOffset {
                                item_ix: index,
                                offset_in_item: old.offset_in_item,
                            });
                        }
                    }
                    Err(error) => this.library_screen.history_error = error,
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
                        self.library_screen.scanning || self.library_screen.folders.is_empty(),
                    )
                    .border_color(rgb(PANEL_2))
                    .rounded(px(4.))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !scope_matches(this, generation, account)
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
                                self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !scope_matches(this, generation, account) {
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
                    self.library_screen.busy,
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
                                    render_library_row(
                                        row,
                                        cover,
                                        expanded,
                                        weak.clone(),
                                        this.generation,
                                        this.profile.as_ref().map(|user| user.id),
                                    )
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
                        if this.library_screen.history_open
                            && this.library_screen.history.is_empty()
                        {
                            this.load_library_history(false, window, cx);
                        }
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
                                                    format!("history-{index}"),
                                                    format!(
                                                        "{} · Episode {}",
                                                        item.title, item.episode
                                                    ),
                                                    false,
                                                )
                                                .on_click(move |_, window, app| {
                                                    let _ = weak.update(app, |this, cx| {
                                                        if scope_matches(this, generation, account)
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
        if self.library_modal_layer().is_some() {
            let focus = self.modal_focus();
            let active = self.active_modal() == self.library_modal_layer();
            page = page.child(modal_container(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .bg(gpui::rgba(0x000000b0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_scroll(|_, _, cx| cx.stop_propagation())
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape"
                            && this.active_modal() == this.library_modal_layer()
                        {
                            this.dismiss_library_dialog(cx);
                            cx.stop_propagation();
                        }
                    })),
                div()
                    .id("library-dialog-scroll")
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .child(self.render_library_dialog(cx))
                    .into_any(),
                active,
                &focus,
            ));
        }
        page.into_any()
    }

    fn render_library_dialog(&mut self, cx: &mut Context<Self>) -> AnyElement {
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
                            button("library-remove-cancel", "Cancel", self.library_screen.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.dismiss_library_dialog(cx);
                                    }
                                })),
                        )
                        .child(
                            button(
                                "library-remove-confirm-button",
                                "Remove",
                                self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.remove_library_folder(path.clone(), window, cx);
                                    }
                                },
                            )),
                        ),
                )
                .into_any();
        }
        if let Some(id) = self.library_screen.unlink_media {
            return dialog("library-unlink-confirm", "Remove manual links?")
                .child("The recognizer will guess these files again on the next scan.")
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .mt(px(12.))
                        .child(
                            button("library-unlink-cancel", "Cancel", self.library_screen.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.dismiss_library_dialog(cx);
                                    }
                                })),
                        )
                        .child(
                            button(
                                "library-unlink-confirm-button",
                                "Remove links",
                                self.library_screen.busy,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if scope_matches(this, generation, account) {
                                        this.unlink_library_media(id, window, cx);
                                    }
                                },
                            )),
                        ),
                )
                .into_any();
        }
        let Some(bind) = self.library_screen.bind.as_ref() else {
            return div().into_any();
        };
        let target = bind.target(&self.library_screen.folders);
        let folder_allowed = bind.folder_allowed(&self.library_screen.folders);
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
                .child(target),
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
                        busy || scope == BindScope::File,
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
                        busy || !folder_allowed || scope == BindScope::Folder,
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
        if bind.restored {
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
                    button("library-bind-retry", "Retry", bind.loading || busy).on_click(
                        cx.listener(move |this, _, window, cx| {
                            if scope_matches(this, generation, account)
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
                        busy || !bind.checked
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
                    busy || bind.loading
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
            button("library-bind-cancel", "Cancel", busy).on_click(cx.listener(
                move |this, _, _, cx| {
                    if scope_matches(this, generation, account) {
                        this.dismiss_library_dialog(cx);
                    }
                },
            )),
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
    if !scope_matches(this, generation, account) || this.library_screen.opening_file {
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
            if let Err(error) = result {
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
    generation: u64,
    account: Option<i64>,
) -> AnyElement {
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
            let next = group
                .files
                .iter()
                .find(|file| file.episode == Some(progress.unwrap_or(0) + 1));
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
                    button(format!("library-unlink-{id}"), "Unlink", false).on_click(
                        move |_, _, app| {
                            let _ = weak_unlink.update(app, |this, cx| {
                                if !scope_matches(this, generation, account) {
                                    return;
                                }
                                this.library_screen.unlink_media = Some(id);
                                cx.notify();
                            });
                        },
                    ),
                );
            }
            if let Some(file) = next {
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
                        format!("library-next-{id}"),
                        if movie {
                            "▶ Play movie".into()
                        } else {
                            format!("▶ Ep {episode}")
                        },
                        false,
                    )
                    .on_click(move |_, _, app| {
                        let _ = weak_play.update(app, |this, cx| {
                            open_file(this, &path, false, generation, account, cx)
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
                    button(format!("library-file-play-{}", file.path), "Play", false).on_click(
                        move |_, _, app| {
                            let _ = weak_play.update(app, |this, cx| {
                                open_file(this, &path, false, generation, account, cx)
                            });
                        },
                    ),
                );
            if progress >= 0 {
                let path = file.path.clone();
                let title = title.clone();
                line = line.child(
                    button(
                        format!("library-file-number-{}", file.path),
                        "Numbering",
                        false,
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
                button(format!("library-file-reveal-{}", file.path), "Show", false).on_click(
                    move |_, _, app| {
                        let _ = weak_reveal.update(app, |this, cx| {
                            open_file(this, &path, true, generation, account, cx)
                        });
                    },
                ),
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
                    button(format!("library-link-{}", file.path), "Link", false).on_click(
                        move |_, window, app| {
                            let _ = weak_link.update(app, |this, cx| {
                                if !scope_matches(this, generation, account) {
                                    return;
                                }
                                this.open_library_bind(
                                    path.clone(),
                                    None,
                                    BindMode::Link,
                                    window,
                                    cx,
                                )
                            });
                        },
                    ),
                )
                .child(
                    button(
                        format!("library-unmatched-play-{}", file.path),
                        "Play",
                        false,
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
                        false,
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
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |this, window, cx| {
            this.auth_intent += 1;
            this.restoring_session = false;
            this.profile = Some(kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            });
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
                path: "/controlled/episode.mkv".into(),
                media_id: Some(1),
                title: "Synthetic".into(),
                mode: BindMode::Link,
                scope: BindScope::File,
                request: 1,
                current: Some(LibraryBinding {
                    media_id: 1,
                    episode_offset: 0,
                }),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
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
            this.route = Route::Library;
            this.library_screen.bind = Some(BindDialog {
                path: "/controlled/episode.mkv".into(),
                media_id: Some(1),
                title: "Synthetic".into(),
                mode: BindMode::Numbering,
                scope: BindScope::File,
                request: 1,
                current: Some(LibraryBinding {
                    media_id: 1,
                    episode_offset: 0,
                }),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
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
                path: "/controlled/episode.mkv".into(),
                media_id: None,
                title: String::new(),
                mode: BindMode::Link,
                scope: BindScope::File,
                request: 1,
                current: Some(LibraryBinding {
                    media_id: 1,
                    episode_offset: 0,
                }),
                current_title: None,
                checked: true,
                loading: false,
                busy: false,
                error: String::new(),
                relink: None,
                restored: false,
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
        };
        assert_eq!(
            bind.target(&["/controlled/root".into()]),
            "/controlled/root/season"
        );
        bind.path = "/controlled/root/ep13.mkv".into();
        assert!(!bind.folder_allowed(&["/controlled/root".into()]));
        assert_eq!(
            bind.target(&["/controlled/root".into()]),
            "/controlled/root/ep13.mkv"
        );
    }
}
