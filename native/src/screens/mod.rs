mod calendar;
#[cfg(test)]
mod cover_layout_tests;
mod covers;
mod detail;
mod editor;
mod library;
mod my_list;
mod notifications;
mod overlays;
mod search;
mod seasons;
mod settings;
pub mod state;
mod stats;
mod torrents;
mod watching;
#[cfg(test)]
mod workspace_tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, ListAlignment, ListOffset,
    ListState as GpuiListState, Render, ScrollHandle, Subscription, Window, prelude::*, px,
};
use kurisu_core::models::{
    LibraryFile, Media, NowPlaying, PendingChange, TrackingPrompt, UpdateInfo, User,
};
use kurisu_core::{Core, Event, EventKind};

use crate::components::covers::{CoverCache, CoverTicket};
use crate::components::input::{TextInput, TextInputEvent};
use crate::contracts::{CloseIntent, Route, WorkspaceEvent};
use my_list::{MyListAnchor, MyListSlot};
use state::{
    DetailState, EditorState, ListChange, ListState, ProgressEdit, ProgressRegistry, SortKey,
};

fn now_seconds() -> i64 {
    Utc::now().timestamp()
}

fn prompt_payload(event: &Event) -> Option<&TrackingPrompt> {
    match &event.kind {
        EventKind::TrackingPrompt(prompt) | EventKind::TrackingAsk(prompt) => Some(prompt),
        _ => None,
    }
}

fn same_prompt(left: &Event, right: &Event) -> bool {
    left.generation == right.generation
        && left.account_id == right.account_id
        && left.prompt_id.is_some()
        && left.prompt_id == right.prompt_id
        && matches!(
            (&left.kind, &right.kind),
            (EventKind::TrackingPrompt(_), EventKind::TrackingPrompt(_))
                | (EventKind::TrackingAsk(_), EventKind::TrackingAsk(_))
        )
        && prompt_payload(left) == prompt_payload(right)
}

const CLOCK_INTERVAL_SECONDS: i64 = 30;
const PREFERENCE_KEYS: [&str; 8] = [
    "kurisu.list.filter",
    "kurisu.list.sort",
    "kurisu.list.dir",
    "kurisu.list.synced",
    "kurisu.cal.mine",
    "kurisu.torrents.new",
    "kurisu.torrents.statuses",
    "kurisu.torrents.filters",
];

#[derive(Default)]
struct PreferenceWrite {
    pending: Option<Option<String>>,
    error: Option<String>,
    revision: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct FreshnessPlan {
    run: bool,
    notify_clock: bool,
    reconnect: bool,
    sync_list: bool,
    scan_library: bool,
}

fn freshness_plan(visible: bool, route: &Route, offline: bool, clock_due: bool) -> FreshnessPlan {
    if !visible {
        return FreshnessPlan::default();
    }
    FreshnessPlan {
        run: true,
        notify_clock: clock_due
            && matches!(route, Route::MyList | Route::Anime(_) | Route::Calendar),
        reconnect: offline,
        sync_list: !offline && *route == Route::MyList,
        scan_library: matches!(route, Route::Now | Route::Library | Route::Anime(_)),
    }
}

fn joined<T>(result: Result<Result<T, String>, tokio::task::JoinError>) -> Result<T, String> {
    result.map_err(|error| error.to_string())?
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BarrierAction {
    Close(CloseIntent),
    Logout,
    Install,
    Login,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModalLayer {
    Editor,
    Logout,
    Prompt,
    Update,
    UpdateFailed,
    Barrier,
    BarrierError,
    LibraryRemove,
    LibraryUnlink,
    LibraryBind,
}

#[derive(Clone)]
pub struct ModalFocus {
    root: FocusHandle,
    start: FocusHandle,
    end: FocusHandle,
}

impl ModalFocus {
    pub fn hold(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.root, cx);
    }

    fn contains(&self, window: &Window, cx: &App) -> bool {
        self.root.contains_focused(window, cx)
    }

    fn first(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.start, cx);
        window.focus_next(cx);
        if self.end.is_focused(window) || !self.contains(window, cx) {
            window.focus(&self.root, cx);
        }
    }

    fn last(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.end, cx);
        window.focus_prev(cx);
        if self.start.is_focused(window) || !self.contains(window, cx) {
            window.focus(&self.root, cx);
        }
    }

    pub fn advance(&self, forward: bool, window: &mut Window, cx: &mut App) {
        if !self.contains(window, cx)
            || self.root.is_focused(window)
            || (forward && self.end.is_focused(window))
            || (!forward && self.start.is_focused(window))
        {
            if forward {
                self.first(window, cx);
            } else {
                self.last(window, cx);
            }
            return;
        }
        if forward {
            window.focus_next(cx);
            if self.end.is_focused(window) || !self.contains(window, cx) {
                self.first(window, cx);
            }
        } else {
            window.focus_prev(cx);
            if self.start.is_focused(window) || !self.contains(window, cx) {
                self.last(window, cx);
            }
        }
    }
}

struct EditorInputs {
    progress: Entity<TextInput>,
    score: Entity<TextInput>,
    repeat: Entity<TextInput>,
    notes: Entity<TextInput>,
    started_year: Entity<TextInput>,
    started_month: Entity<TextInput>,
    started_day: Entity<TextInput>,
    completed_year: Entity<TextInput>,
    completed_month: Entity<TextInput>,
    completed_day: Entity<TextInput>,
}

struct EditorDraft {
    editor: EditorState,
    inputs: Option<EditorInputs>,
    subscriptions: Vec<Subscription>,
    details_open: bool,
    recommendations: Vec<Media>,
}

pub struct Workspace {
    watching: watching::WatchingState,
    library_screen: library::LibraryState,
    stats: stats::StatsState,
    settings: settings::SettingsState,
    torrents: torrents::TorrentsState,
    notifications: notifications::NotificationsState,
    search: search::SearchState,
    seasons: seasons::SeasonsState,
    calendar: calendar::CalendarState,
    core: Core,
    route: Route,
    back_stack: Vec<Route>,
    profile: Option<User>,
    restoring_session: bool,
    last_account: Option<i64>,
    offline: bool,
    generation: u64,
    auth_intent: u64,
    auth_request: u64,
    auth_busy: bool,
    auth_oauth: bool,
    reconnect_busy: bool,
    reconnect_request: u64,
    last_reconnect_attempt: Option<Instant>,
    auth_abort: Option<tokio::task::AbortHandle>,
    queued_login: Option<(bool, Option<String>)>,
    auth_error: String,
    token_open: bool,
    token: Entity<TextInput>,
    list_query: Entity<TextInput>,
    sort_open: bool,
    sort_menu_focus: FocusHandle,
    sort_menu_focus_subscription: Option<Subscription>,
    editor_status_focus: FocusHandle,
    editor_status_focus_subscription: Option<Subscription>,
    season_jump_focus: FocusHandle,
    season_jump_focus_subscription: Option<Subscription>,
    season_format_focus: FocusHandle,
    season_format_focus_subscription: Option<Subscription>,
    season_status_focus: FocusHandle,
    season_status_focus_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    editor_subscriptions: Vec<Subscription>,
    list: ListState,
    visible_ids: Arc<Vec<i64>>,
    list_widget: GpuiListState,
    list_cover_revision: u64,
    list_cover_reconciled: u64,
    list_cover_refresh_scheduled: bool,
    list_cover_window_size: gpui::Size<gpui::Pixels>,
    list_cover_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    covers: CoverCache,
    cover_scale: f32,
    cover_tickets: HashMap<(i64, u32, u32), CoverTicket>,
    cover_images: HashMap<(i64, u32, u32), Arc<gpui::RenderImage>>,
    cover_regions: HashMap<covers::CoverOwner, covers::CoverRegion>,
    list_request: u64,
    sync_request: u64,
    read_aborts: HashMap<&'static str, Vec<tokio::task::AbortHandle>>,
    read_lifetime: Arc<std::sync::Mutex<HashMap<&'static str, Vec<tokio::task::AbortHandle>>>>,
    event_stop: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    list_epoch: u64,
    list_scope: Option<HashSet<i64>>,
    list_refresh_queued: bool,
    list_refresh_ticket: u64,
    last_sync_attempt: Option<Instant>,
    list_sync_flash: Option<u64>,
    clock_at: i64,
    preferences_loaded: bool,
    preference_reset: u64,
    preference_writes: HashMap<&'static str, PreferenceWrite>,
    preference_active: Option<&'static str>,
    preference_alert_dismissed: bool,
    query_request: u64,
    query_pending: Option<Option<String>>,
    query_write_active: bool,
    query_error: String,
    scroll_memory: HashMap<Route, MyListAnchor>,
    progress: ProgressRegistry,
    progress_drafts: HashMap<i64, HashMap<i64, ProgressEdit>>,
    progress_timers: HashMap<(u64, i64), u64>,
    pending_editor: Option<(i64, Route)>,
    editor: Option<EditorState>,
    editor_drafts: HashMap<i64, EditorDraft>,
    library_drafts: HashMap<i64, library::LibraryDraft>,
    editor_status_open: bool,
    editor_inputs: Option<EditorInputs>,
    editor_recs: Vec<Media>,
    editor_recs_error: String,
    editor_adding: HashSet<i64>,
    added_recommendations: HashSet<i64>,
    editor_details_loading: bool,
    editor_details_open: bool,
    delete_waiting: bool,
    detail: DetailState,
    detail_scroll: HashMap<i64, ScrollHandle>,
    library_files: Vec<LibraryFile>,
    library_request: u64,
    pending: Vec<PendingChange>,
    pending_error: String,
    pending_busy: bool,
    pending_expanded: bool,
    pending_request: u64,
    now_playing: Option<NowPlaying>,
    prompt: Option<TrackingPrompt>,
    prompt_event: Option<Event>,
    prompt_queue: VecDeque<Event>,
    prompt_busy: bool,
    prompt_error: String,
    update: Option<UpdateInfo>,
    restart_pending: bool,
    update_failed: String,
    dismissed_update: String,
    update_busy: bool,
    active_writes: usize,
    barrier: Option<BarrierAction>,
    barrier_error: String,
    logout_confirming: bool,
    global_error: String,
    modal_focus: ModalFocus,
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop_pending_auth();
        self.cancel_all_reads();
        self.stop_event_subscription();
    }
}

impl Workspace {
    pub fn new(core: Core, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let generation = core.generation();
        let token = cx.new(|cx| {
            let mut input = TextInput::new("", "AniList access token", window, cx);
            input.set_masked(true, cx);
            input
        });
        let list_query = cx.new(|cx| {
            TextInput::new("", "Find a title on your shelf…", window, cx).without_frame()
        });
        let subscriptions = vec![
            cx.subscribe_in(&list_query, window, |this, _, event, window, cx| {
                if let TextInputEvent::Changed(value) = event {
                    this.list.set_query(value.clone(), now_seconds());
                    this.stage_query(Some(value.clone()));
                    let request = this.query_request;
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(300))
                            .await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if this.query_request == request {
                                this.save_query(window, cx);
                            }
                        });
                    })
                    .detach();
                    this.reset_list_widget();
                    cx.notify();
                }
            }),
            cx.subscribe_in(&token, window, |this, _, event, window, cx| {
                if matches!(event, TextInputEvent::Submitted) {
                    this.login_with_token(window, cx);
                }
            }),
        ];
        let list_widget = GpuiListState::new(MyListSlot::count(0), ListAlignment::Top, px(800.));
        let weak = cx.weak_entity();
        list_widget.set_scroll_handler(move |event, window, app| {
            let list_range = event.visible_range.clone();
            let _ = weak.update(app, |this, cx| {
                if this.route == Route::MyList {
                    let range = MyListSlot::media_range(list_range, this.visible_ids.len());
                    let end_handoff =
                        !this.visible_ids.is_empty() && range.start >= this.visible_ids.len();
                    if !end_handoff {
                        this.mark_list_cover_refresh();
                    }
                    this.prefetch_covers(range, window, cx);
                    if !end_handoff {
                        this.schedule_list_cover_refresh(window, cx);
                    }
                }
            });
        });
        let mut workspace = Self {
            watching: Default::default(),
            library_screen: Default::default(),
            stats: Default::default(),
            settings: Default::default(),
            torrents: Default::default(),
            notifications: Default::default(),
            search: Default::default(),
            seasons: Default::default(),
            calendar: Default::default(),
            core,
            route: Route::MyList,
            back_stack: Vec::new(),
            profile: None,
            restoring_session: true,
            last_account: None,
            offline: false,
            generation,
            auth_intent: 0,
            auth_request: 0,
            auth_busy: false,
            auth_oauth: false,
            reconnect_busy: false,
            reconnect_request: 0,
            last_reconnect_attempt: None,
            auth_abort: None,
            queued_login: None,
            auth_error: String::new(),
            token_open: false,
            token,
            list_query,
            sort_open: false,
            sort_menu_focus: cx.focus_handle().tab_stop(true),
            sort_menu_focus_subscription: None,
            editor_status_focus: cx.focus_handle().tab_stop(true),
            editor_status_focus_subscription: None,
            season_jump_focus: cx.focus_handle().tab_stop(true),
            season_jump_focus_subscription: None,
            season_format_focus: cx.focus_handle().tab_stop(true),
            season_format_focus_subscription: None,
            season_status_focus: cx.focus_handle().tab_stop(true),
            season_status_focus_subscription: None,
            _subscriptions: subscriptions,
            editor_subscriptions: Vec::new(),
            list: ListState::new(),
            visible_ids: Arc::new(Vec::new()),
            list_widget,
            list_cover_revision: 0,
            list_cover_reconciled: 0,
            list_cover_refresh_scheduled: false,
            list_cover_window_size: window.viewport_size(),
            list_cover_bounds: None,
            covers: CoverCache::new(64 * 1024 * 1024, 3),
            cover_scale: window.scale_factor(),
            cover_tickets: HashMap::new(),
            cover_images: HashMap::new(),
            cover_regions: HashMap::new(),
            list_request: 0,
            sync_request: 0,
            read_aborts: HashMap::new(),
            read_lifetime: Arc::new(std::sync::Mutex::new(HashMap::new())),
            event_stop: Arc::new(std::sync::Mutex::new(None)),
            list_epoch: 0,
            list_scope: None,
            list_refresh_queued: false,
            list_refresh_ticket: 0,
            last_sync_attempt: None,
            list_sync_flash: None,
            clock_at: now_seconds(),
            preferences_loaded: false,
            preference_reset: 0,
            preference_writes: HashMap::new(),
            preference_active: None,
            preference_alert_dismissed: false,
            query_request: 0,
            query_pending: None,
            query_write_active: false,
            query_error: String::new(),
            scroll_memory: HashMap::new(),
            progress: ProgressRegistry::default(),
            progress_drafts: HashMap::new(),
            progress_timers: HashMap::new(),
            pending_editor: None,
            editor: None,
            editor_drafts: HashMap::new(),
            library_drafts: HashMap::new(),
            editor_status_open: false,
            editor_inputs: None,
            editor_recs: Vec::new(),
            editor_recs_error: String::new(),
            editor_adding: HashSet::new(),
            added_recommendations: HashSet::new(),
            editor_details_loading: false,
            editor_details_open: false,
            delete_waiting: false,
            detail: DetailState::default(),
            detail_scroll: HashMap::new(),
            library_files: Vec::new(),
            library_request: 0,
            pending: Vec::new(),
            pending_error: String::new(),
            pending_busy: false,
            pending_expanded: false,
            pending_request: 0,
            now_playing: None,
            prompt: None,
            prompt_event: None,
            prompt_queue: VecDeque::new(),
            prompt_busy: false,
            prompt_error: String::new(),
            update: None,
            restart_pending: false,
            update_failed: String::new(),
            dismissed_update: String::new(),
            update_busy: false,
            active_writes: 0,
            barrier: None,
            barrier_error: String::new(),
            logout_confirming: false,
            global_error: String::new(),
            modal_focus: ModalFocus {
                root: cx.focus_handle(),
                start: cx.focus_handle().tab_stop(true),
                end: cx.focus_handle().tab_stop(true),
            },
        };
        workspace._subscriptions.push(cx.on_release(|this, _| {
            this.stop_pending_auth();
            this.cancel_all_reads();
            this.stop_event_subscription();
        }));
        let read_aborts = workspace.read_lifetime.clone();
        let event_stop = workspace.event_stop.clone();
        let auth_owner = cx.weak_entity();
        let window_id = window.window_handle().window_id();
        workspace
            ._subscriptions
            .push(cx.on_window_closed(move |cx, closed| {
                if closed == window_id {
                    let _ = auth_owner.update(cx, |this, _| this.stop_pending_auth());
                    for (_, handles) in read_aborts.lock().unwrap().drain() {
                        for handle in handles {
                            handle.abort();
                        }
                    }
                    if let Some(stop) = event_stop.lock().unwrap().take() {
                        let _ = stop.send(());
                    }
                }
            }));
        workspace.subscribe_events(window, cx);
        workspace.restore_session(window, cx);
        workspace.load_preferences(window, cx);
        workspace.load_update_notice(window, cx);
        workspace.poll_covers(window, cx);
        workspace
            ._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.visible_freshness(window, cx);
                }
            }));
        workspace._subscriptions.push(cx.observe_window_visibility(
            window,
            |this, visibility, window, cx| {
                if visibility.is_visible() {
                    this.visible_freshness(window, cx);
                }
            },
        ));
        workspace.poll_freshness(window, cx);
        workspace
    }

    pub fn route(&self) -> &Route {
        &self.route
    }
    pub fn profile(&self) -> Option<&User> {
        self.profile.as_ref()
    }
    pub fn is_restoring_session(&self) -> bool {
        self.restoring_session && self.profile.is_none()
    }
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    pub fn modal_open(&self) -> bool {
        self.active_modal().is_some()
    }

    pub fn active_modal(&self) -> Option<ModalLayer> {
        if self.barrier.is_some() && !self.barrier_error.is_empty() {
            Some(ModalLayer::BarrierError)
        } else if self.barrier.is_some() {
            Some(ModalLayer::Barrier)
        } else if !self.update_failed.is_empty() {
            Some(ModalLayer::UpdateFailed)
        } else if self.update.is_some() {
            Some(ModalLayer::Update)
        } else if self.prompt.is_some() {
            Some(ModalLayer::Prompt)
        } else if self.logout_confirming {
            Some(ModalLayer::Logout)
        } else if self.editor.is_some() {
            Some(ModalLayer::Editor)
        } else {
            self.library_modal_layer()
        }
    }

    pub fn modal_focus(&self) -> ModalFocus {
        self.modal_focus.clone()
    }

    pub fn focus_active_modal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_modal() == Some(ModalLayer::Editor)
            && let Some(inputs) = &self.editor_inputs
        {
            cx.focus_view(&inputs.progress, window);
            if self.modal_focus.contains(window, cx) {
                return;
            }
        }
        if matches!(self.active_modal(), Some(ModalLayer::LibraryBind))
            && self.focus_library_dialog(window, cx)
        {
            return;
        }
        self.modal_focus.first(window, cx);
    }

    pub fn navigate(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() || route == self.route {
            return;
        }
        self.remember_scroll();
        self.back_stack.push(self.route.clone());
        self.switch_route(route, window, cx);
    }

    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open() {
            return;
        }
        if let Some(route) = self.back_stack.pop() {
            self.switch_route(route, window, cx);
        }
    }

    fn switch_route(&mut self, route: Route, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .pending_editor
            .as_ref()
            .is_some_and(|(_, origin)| *origin != route)
        {
            self.pending_editor = None;
        }
        match self.route {
            Route::Search if route != Route::Search => self.leave_search(),
            Route::Seasons if route != Route::Seasons => self.leave_seasons(),
            Route::Calendar if route != Route::Calendar => self.leave_calendar(),
            _ => {}
        }
        if !matches!(route, Route::Anime(_)) {
            self.cancel_reads("detail");
            self.cancel_reads("detail-entry");
            self.cancel_reads("detail-recommendations");
        }
        self.route = route.clone();
        self.retain_cover_route();
        if matches!(route, Route::Anime(_)) {
            self.ensure_library_fresh(window, cx);
        }
        self.enter_active_route(window, cx);
        if let Route::Anime(id) = route {
            self.load_detail(id, window, cx);
        }
        if route == Route::MyList {
            self.mark_list_cover_refresh();
            self.refresh_airing_order(now_seconds());
            if let Some(anchor) = self.scroll_memory.get(&Route::MyList).copied() {
                match anchor {
                    MyListAnchor::Header(inset) => self.list_widget.scroll_to(ListOffset {
                        item_ix: 0,
                        offset_in_item: inset,
                    }),
                    MyListAnchor::Media(id, inset) => {
                        if let Some(index) = self.list.visible().iter().position(|item| *item == id)
                        {
                            self.list_widget.scroll_to(ListOffset {
                                item_ix: MyListSlot::item_ix(index),
                                offset_in_item: inset,
                            });
                        }
                    }
                    MyListAnchor::Footer(inset) => self.list_widget.scroll_to(ListOffset {
                        item_ix: MyListSlot::footer_ix(self.list.visible().len()),
                        offset_in_item: inset,
                    }),
                    MyListAnchor::End => self.list_widget.scroll_to_end(),
                }
            }
            if self.list.loaded {
                let start = self.list_widget.logical_scroll_top().item_ix;
                let range = MyListSlot::media_range(
                    start..start.saturating_add(32),
                    self.visible_ids.len(),
                );
                self.prefetch_covers(range, window, cx);
            }
            if self.profile.is_some() && !self.list.loaded {
                self.load_list(window, cx);
            }
            self.visible_freshness(window, cx);
        }
        cx.emit(WorkspaceEvent::StateChanged);
        cx.notify();
    }

    fn enter_active_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.profile.is_none() && self.route != Route::Settings {
            return;
        }
        match self.route {
            Route::Now => self.enter_watching(window, cx),
            Route::Library => self.enter_library(window, cx),
            Route::Stats => self.enter_stats(window, cx),
            Route::Settings => self.enter_settings(window, cx),
            Route::Torrents => {
                self.enter_torrents(window, cx);
                self.load_torrent_entries(window, cx);
            }
            Route::Notifications => self.enter_notifications(window, cx),
            Route::Search => self.enter_search(window, cx),
            Route::Seasons => self.enter_seasons(window, cx),
            Route::Calendar => self.enter_calendar(window, cx),
            _ => {}
        }
    }

    fn refresh_route_covers(&mut self) {
        match self.route {
            Route::Now => self.refresh_watching_covers(),
            Route::Library => self.refresh_library_covers(),
            Route::Stats => self.refresh_stats_covers(),
            Route::Settings => self.refresh_settings_covers(),
            Route::Torrents => self.refresh_torrents_covers(),
            Route::Notifications => self.refresh_notifications_covers(),
            Route::Search => self.refresh_search_covers(),
            Route::Seasons => self.refresh_seasons_covers(),
            Route::Calendar => self.refresh_calendar_covers(),
            _ => {}
        }
    }

    fn remember_scroll(&mut self) {
        if self.route != Route::MyList {
            return;
        }
        let offset = self.list_widget.logical_scroll_top();
        let anchor = if offset.item_ix >= MyListSlot::count(self.visible_ids.len()) {
            MyListAnchor::End
        } else {
            match MyListSlot::at(offset.item_ix, self.visible_ids.len()) {
                MyListSlot::Header => MyListAnchor::Header(offset.offset_in_item),
                MyListSlot::Media(index) => {
                    let Some(id) = self.visible_ids.get(index) else {
                        return;
                    };
                    MyListAnchor::Media(*id, offset.offset_in_item)
                }
                MyListSlot::Footer => MyListAnchor::Footer(offset.offset_in_item),
            }
        };
        self.scroll_memory.insert(Route::MyList, anchor);
    }

    fn reset_list_widget(&mut self) {
        let offset = self.list_widget.logical_scroll_top();
        let anchor = if offset.item_ix >= MyListSlot::count(self.visible_ids.len()) {
            MyListAnchor::End
        } else {
            match MyListSlot::at(offset.item_ix, self.visible_ids.len()) {
                MyListSlot::Header => MyListAnchor::Header(offset.offset_in_item),
                MyListSlot::Media(index) => self
                    .visible_ids
                    .get(index)
                    .map_or(MyListAnchor::Header(px(0.)), |id| {
                        MyListAnchor::Media(*id, offset.offset_in_item)
                    }),
                MyListSlot::Footer => MyListAnchor::Footer(offset.offset_in_item),
            }
        };
        self.visible_ids = Arc::new(self.list.visible().to_vec());
        self.list_widget
            .reset_with_uniform_height(MyListSlot::count(self.visible_ids.len()), px(90.));
        self.mark_list_cover_refresh();
        if self.visible_ids.is_empty() {
            if self.route == Route::MyList {
                self.replace_cover_region(covers::CoverOwner::Route(Route::MyList), vec![], vec![]);
            }
            self.list_cover_reconciled = self.list_cover_revision;
            self.list_widget.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
            return;
        }
        match anchor {
            MyListAnchor::Header(inset) => self.list_widget.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: inset,
            }),
            MyListAnchor::Media(id, inset) => {
                if let Some(index) = self.visible_ids.iter().position(|item| *item == id) {
                    self.list_widget.scroll_to(ListOffset {
                        item_ix: MyListSlot::item_ix(index),
                        offset_in_item: inset,
                    });
                }
            }
            MyListAnchor::Footer(inset) => self.list_widget.scroll_to(ListOffset {
                item_ix: MyListSlot::footer_ix(self.visible_ids.len()),
                offset_in_item: inset,
            }),
            MyListAnchor::End => self.list_widget.scroll_to_end(),
        }
    }

    pub(super) fn apply_list_change(&mut self, change: ListChange) {
        if change.order_changed || change.time_boundary_crossed {
            self.reset_list_widget();
            return;
        }
        let mut remeasured = false;
        if change.changed_ids.len() == 1 {
            if let Some(index) = self
                .visible_ids
                .iter()
                .position(|id| *id == change.changed_ids[0])
            {
                let item = MyListSlot::item_ix(index);
                self.list_widget.remeasure_items(item..item + 1);
                remeasured = true;
            }
        } else if !change.changed_ids.is_empty() {
            let changed: HashSet<_> = change.changed_ids.into_iter().collect();
            for (index, id) in self.visible_ids.iter().enumerate() {
                if changed.contains(id) {
                    let item = MyListSlot::item_ix(index);
                    self.list_widget.remeasure_items(item..item + 1);
                    remeasured = true;
                }
            }
        }
        if remeasured {
            self.mark_list_cover_refresh();
        }
    }

    fn discovery_list_changed(&mut self) {
        if !self.seasons.media.is_empty() {
            self.refresh_season_visible();
        }
        if !self.calendar.items.is_empty() {
            self.refresh_calendar_visible();
        }
    }

    fn replace_reads(&mut self, group: &'static str, handles: Vec<tokio::task::AbortHandle>) {
        self.read_lifetime
            .lock()
            .unwrap()
            .insert(group, handles.clone());
        if let Some(previous) = self.read_aborts.insert(group, handles) {
            for handle in previous {
                handle.abort();
            }
        }
    }

    fn cancel_reads(&mut self, group: &'static str) {
        self.read_lifetime.lock().unwrap().remove(group);
        if let Some(previous) = self.read_aborts.remove(group) {
            for handle in previous {
                handle.abort();
            }
        }
    }

    fn cancel_all_reads(&mut self) {
        self.read_lifetime.lock().unwrap().clear();
        for (_, handles) in self.read_aborts.drain() {
            for handle in handles {
                handle.abort();
            }
        }
    }

    fn stop_event_subscription(&mut self) {
        if let Some(stop) = self.event_stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
    }

    fn subscribe_events(&mut self, window: &Window, cx: &mut Context<Self>) {
        let mut receiver = self.core.subscribe();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        *self.event_stop.lock().unwrap() = Some(stop);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let received = tokio::select! {
                    _ = &mut stopped => break,
                    received = receiver.recv() => received,
                };
                match received {
                    Ok(event) => {
                        let worker = core.clone();
                        let check = event.clone();
                        if !core
                            .spawn(async move { worker.event_is_current(&check) })
                            .await
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        if this
                            .update_in(cx, |this, window, cx| this.handle_event(event, window, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let worker = core.clone();
                        let recovered = core
                            .spawn(async move { worker.recovery_events() })
                            .await
                            .unwrap_or_default();
                        if this
                            .update_in(cx, |this, window, cx| {
                                let session_changed = recovered
                                    .iter()
                                    .find(|event| {
                                        !matches!(
                                            &event.kind,
                                            EventKind::UpdateAvailable(_)
                                                | EventKind::UpdateFailed(_)
                                        )
                                    })
                                    .is_none_or(|event| {
                                        event.generation != this.generation
                                            || event.account_id
                                                != this.profile.as_ref().map(|user| user.id)
                                    });
                                for event in recovered {
                                    if this.core.event_is_current(&event) {
                                        this.handle_event(event, window, cx);
                                    }
                                }
                                this.queue_list_refresh(None, window, cx);
                                this.refresh_pending(window, cx);
                                if session_changed {
                                    this.restore_session(window, cx);
                                }
                                if this.route == Route::Now {
                                    this.refresh_watching_list(window, cx);
                                    this.refresh_watching_playback(window, cx);
                                }
                                if this.route == Route::Library {
                                    this.refresh_library_entries(window, cx);
                                    this.refresh_library_history(window, cx);
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .detach();
    }

    fn handle_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        let kind = match event.kind.clone() {
            EventKind::UpdateAvailable(update) => {
                self.show_update(update);
                cx.notify();
                return;
            }
            EventKind::UpdateFailed(error) => {
                self.update_failed = error;
                cx.notify();
                return;
            }
            EventKind::AuthExpired => {
                if state::expiration_matches(
                    event.generation,
                    event.account_id,
                    self.generation,
                    self.profile.as_ref().map(|user| user.id),
                    self.auth_busy,
                ) {
                    self.verify_expiration(window, cx);
                }
                return;
            }
            kind => kind,
        };
        if event.generation != self.generation
            || event.account_id != self.profile.as_ref().map(|user| user.id)
        {
            return;
        }
        match kind {
            EventKind::EpisodeUpdated(entry) => {
                self.queue_list_refresh(Some(vec![entry.media_id]), window, cx);
                if self.route == Route::Now {
                    self.refresh_watching_list(window, cx);
                }
                if self.route == Route::Library {
                    self.refresh_library_entries(window, cx);
                }
                if self.detail.media_id == Some(entry.media_id) {
                    self.detail.entry = Some(entry);
                    self.detail.entry_available = true;
                }
            }
            EventKind::PendingChanged(scope) => {
                self.queue_list_refresh(scope, window, cx);
                self.refresh_pending(window, cx);
                if self.route == Route::Now {
                    self.refresh_watching_list(window, cx);
                }
                if self.route == Route::Library {
                    self.refresh_library_entries(window, cx);
                }
            }
            EventKind::TrackingPrompt(_) => self.queue_prompt(event),
            EventKind::TrackingAsk(_) => {
                self.navigate(Route::Now, window, cx);
                self.queue_prompt(event);
            }
            EventKind::UpdateAvailable(_) | EventKind::UpdateFailed(_) | EventKind::AuthExpired => {
                unreachable!()
            }
            EventKind::NowPlaying(now) => {
                self.now_playing = now.active.then_some(now);
                if self.route == Route::Now {
                    self.refresh_watching_playback(window, cx);
                }
            }
            EventKind::WatchHistoryUpdated(_) => {
                if self.route == Route::Library {
                    self.refresh_library_history(window, cx);
                }
            }
        }
        if self
            .prompt_event
            .as_ref()
            .is_some_and(|prompt| !self.core.event_is_current(prompt))
        {
            self.prompt = None;
            self.prompt_event = None;
            self.prompt_queue.clear();
            self.prompt_busy = false;
            self.prompt_error.clear();
        }
        cx.notify();
    }

    fn verify_expiration(&mut self, window: &Window, cx: &mut Context<Self>) {
        let generation = self.generation;
        let intent = self.auth_intent;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let logged_in = core.spawn(async move { worker.is_logged_in().await }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !state::expiration_verification_matches(
                    generation,
                    intent,
                    account,
                    this.generation,
                    this.auth_intent,
                    this.profile.as_ref().map(|user| user.id),
                    this.auth_busy,
                ) {
                    return;
                }
                match logged_in {
                    Ok(false) => this.set_profile(None, window, cx),
                    Ok(true) => {}
                    Err(error) => this.auth_error = error.to_string(),
                }
            });
        })
        .detach();
    }

    fn queue_prompt(&mut self, event: Event) {
        let prompt = match &event.kind {
            EventKind::TrackingPrompt(prompt) | EventKind::TrackingAsk(prompt) => prompt.clone(),
            _ => return,
        };
        if self
            .prompt_event
            .as_ref()
            .is_some_and(|current| same_prompt(current, &event))
            || self
                .prompt_queue
                .iter()
                .any(|queued| same_prompt(queued, &event))
        {
            return;
        }
        self.prompt_queue.clear();
        self.prompt = Some(prompt);
        self.prompt_event = Some(event);
        self.prompt_busy = false;
        self.prompt_error.clear();
    }

    fn dismiss_prompt(&mut self, cx: &mut Context<Self>) {
        if self.prompt_busy {
            return;
        }
        if let Some(event) = self.prompt_event.take() {
            self.ack_prompt_event(event);
        }
        let next = self.prompt_queue.pop_front();
        self.prompt = next.as_ref().and_then(prompt_payload).cloned();
        self.prompt_event = next;
        self.prompt_error.clear();
        cx.notify();
    }

    fn ack_prompt_event(&self, event: Event) {
        let core = self.core.clone();
        let worker = core.clone();
        drop(core.spawn(async move { worker.acknowledge_tracking_prompt(event).await }));
    }

    fn restore_session(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.auth_request += 1;
        let request = self.auth_request;
        let core = self.core.clone();
        let generation = self.generation;
        let intent = self.auth_intent;
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    let previous_account = worker.saved_account_id().await;
                    worker
                        .current_user()
                        .await
                        .map(|user| (previous_account, user))
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.auth_refresh_current(generation, intent, request) {
                    return;
                }
                this.restoring_session = false;
                match result {
                    Ok((previous_account, user)) => {
                        if this.last_account.is_none() && this.profile.is_none() {
                            this.last_account = previous_account;
                        }
                        this.set_profile(user, window, cx);
                    }
                    Err(error) => {
                        this.auth_error = error;
                        cx.emit(WorkspaceEvent::StateChanged);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn auth_refresh_current(&self, generation: u64, intent: u64, request: u64) -> bool {
        self.generation == generation && self.auth_intent == intent && self.auth_request == request
    }

    fn poll_freshness(&mut self, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(30))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| this.visible_freshness(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn visible_freshness(&mut self, window: &Window, cx: &mut Context<Self>) {
        let now = now_seconds();
        let clock_due = now.saturating_sub(self.clock_at) >= CLOCK_INTERVAL_SECONDS;
        let plan = freshness_plan(window.is_visible(), &self.route, self.offline, clock_due);
        if !plan.run {
            return;
        }
        let airing_changed = self.route == Route::MyList && self.refresh_airing_order(now);
        if clock_due {
            self.clock_at = now;
        }
        if airing_changed || plan.notify_clock {
            cx.notify();
        }
        if plan.reconnect {
            self.reconnect_account(false, window, cx);
        } else if plan.sync_list && self.list.loaded && self.sync_due() {
            self.sync_list(window, cx);
        }
        if plan.scan_library {
            self.ensure_library_fresh(window, cx);
        }
    }

    fn refresh_airing_order(&mut self, now: i64) -> bool {
        if !self.list.airing_order_due(now) {
            return false;
        }
        self.list.rebuild(now);
        self.reset_list_widget();
        true
    }

    fn reconnect_account(&mut self, manual: bool, window: &Window, cx: &mut Context<Self>) {
        if !self.offline || self.reconnect_busy || self.auth_busy {
            return;
        }
        if !manual
            && self
                .last_reconnect_attempt
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return;
        }
        let Some(account) = self.profile.as_ref().map(|user| user.id) else {
            return;
        };
        self.reconnect_busy = true;
        self.last_reconnect_attempt = Some(Instant::now());
        self.reconnect_request += 1;
        self.auth_request += 1;
        let request = self.reconnect_request;
        let generation = self.generation;
        let intent = self.auth_intent;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.refresh_current_user().await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation
                    || this.auth_intent != intent
                    || this.reconnect_request != request
                    || this.profile.as_ref().map(|user| user.id) != Some(account)
                {
                    return;
                }
                this.reconnect_busy = false;
                match result {
                    Ok(Some(user)) if user.id == account => {
                        if manual && user.offline {
                            this.auth_error =
                                "AniList is still unavailable. Saved data remains accessible."
                                    .into();
                        }
                        this.set_profile(Some(user), window, cx);
                    }
                    Ok(None) => this.set_profile(None, window, cx),
                    Ok(Some(user)) => this.set_profile(Some(user), window, cx),
                    Err(error) if manual => this.auth_error = error,
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_profile(&mut self, user: Option<User>, window: &mut Window, cx: &mut Context<Self>) {
        self.restoring_session = false;
        let new_generation = self.core.generation();
        let new_id = user.as_ref().map(|user| user.id);
        let old_id = self.profile.as_ref().map(|user| user.id);
        let switched_account =
            new_id.is_some() && self.last_account.is_some() && new_id != self.last_account;
        let changed = new_generation != self.generation || new_id != old_id;
        let recovered = self.offline && user.as_ref().is_some_and(|user| !user.offline);
        if changed {
            self.auth_request += 1;
            self.reconnect_request += 1;
            self.reconnect_busy = false;
            self.last_reconnect_attempt = None;
            if let Some(account) = old_id {
                if let Some(draft) = self.take_library_draft(cx) {
                    self.library_drafts.insert(account, draft);
                }
                if self.editor.is_some()
                    && let Err(error) = self.read_editor_inputs(cx)
                    && let Some(editor) = self.editor.as_mut()
                {
                    editor.error = error;
                }
                let score_available = !self.offline
                    || self
                        .profile
                        .as_ref()
                        .and_then(|user| user.score_format.as_ref())
                        .is_some();
                if let Some(mut editor) = self.editor.take()
                    && (editor.is_dirty(score_available) || editor.busy || !editor.error.is_empty())
                {
                    editor.busy = false;
                    editor.confirming_delete = false;
                    self.set_editor_inputs_disabled(false, cx);
                    if editor.error.is_empty() {
                        editor.error = "Sign-in changed. Review this draft before saving.".into();
                    }
                    self.editor_drafts.insert(
                        account,
                        EditorDraft {
                            editor,
                            inputs: self.editor_inputs.take(),
                            subscriptions: std::mem::take(&mut self.editor_subscriptions),
                            details_open: self.editor_details_open,
                            recommendations: std::mem::take(&mut self.editor_recs),
                        },
                    );
                    if new_id.is_none() {
                        self.auth_error = "Unsaved entry changes are kept for this account. Sign in again to review them.".into();
                    }
                }
                let drafts = self.progress.take_generation_drafts(self.generation);
                if !drafts.is_empty() {
                    self.progress_drafts
                        .entry(account)
                        .or_default()
                        .extend(drafts);
                    if new_id.is_none() {
                        self.auth_error = "Unsaved episode changes are kept for this account. Sign in again to review them.".into();
                    }
                }
            }
            self.cancel_all_reads();
            self.watching = Default::default();
            self.library_screen = Default::default();
            if let Some(account) = new_id
                && let Some(draft) = self.library_drafts.remove(&account)
            {
                self.restore_library_draft(draft);
            }
            self.stats = Default::default();
            self.settings = Default::default();
            self.torrents.reset_account();
            self.notifications = Default::default();
            self.search = Default::default();
            self.seasons = Default::default();
            self.calendar.reset_account();
            self.progress.clear_generation(self.generation);
            self.progress_timers
                .retain(|(generation, _), _| *generation != self.generation);
            self.generation = new_generation;
            self.pending_editor = None;
            let synced_at = (new_id.is_some() && new_id == self.last_account)
                .then_some(self.list.synced_at)
                .flatten();
            self.list.clear_account();
            self.list.synced_at = synced_at;
            if let Some(account) = new_id
                && let Some(drafts) = self.progress_drafts.remove(&account)
            {
                self.progress.restore_generation(new_generation, drafts);
                self.list.step_error = self
                    .progress
                    .first_error(new_generation)
                    .unwrap_or_default()
                    .into();
            }
            self.list_epoch += 1;
            self.list_request += 1;
            self.sync_request += 1;
            self.list_scope = None;
            self.list_refresh_queued = false;
            self.list_refresh_ticket += 1;
            self.pending_request += 1;
            self.pending.clear();
            self.pending_busy = false;
            self.pending_error.clear();
            self.editor = None;
            self.editor_status_open = false;
            self.added_recommendations.clear();
            self.editor_recs.clear();
            self.editor_recs_error.clear();
            self.editor_adding.clear();
            self.editor_details_loading = false;
            self.editor_details_open = false;
            self.editor_inputs = None;
            self.editor_subscriptions.clear();
            self.detail = DetailState::default();
            self.detail_scroll.clear();
            self.library_files.clear();
            self.library_request += 1;
            self.prompt = None;
            self.prompt_event = None;
            self.prompt_queue.clear();
            self.prompt_busy = false;
            self.prompt_error.clear();
            self.now_playing = None;
            self.list_sync_flash = None;
            self.scroll_memory.clear();
            self.list_widget.reset(MyListSlot::count(0));
            self.visible_ids = Arc::new(Vec::new());
            self.covers.clear();
            self.cover_tickets.clear();
            self.cover_images.clear();
            self.cover_regions.clear();
        }
        self.offline = user.as_ref().is_some_and(|user| user.offline);
        if recovered {
            self.last_reconnect_attempt = None;
            self.last_sync_attempt = None;
            self.auth_error.clear();
        }
        self.profile = user;
        if changed
            && let Some(account) = new_id
            && let Some(draft) = self.editor_drafts.remove(&account)
        {
            self.editor = Some(draft.editor);
            self.editor_inputs = draft.inputs;
            self.editor_subscriptions = draft.subscriptions;
            self.editor_details_open = draft.details_open;
            self.editor_recs = draft.recommendations;
            let busy = self.editor.as_ref().is_some_and(|editor| editor.busy);
            self.set_editor_inputs_disabled(busy, cx);
        }
        if switched_account {
            self.preference_reset += 1;
            self.last_sync_attempt = None;
            self.list.filter = "CURRENT".into();
            self.list.query.clear();
            self.list.synced_at = None;
            self.list_query
                .update(cx, |input, cx| input.set_value("", cx));
            self.stage_query(None);
            self.save_query(window, cx);
            for key in ["kurisu.list.filter", "kurisu.list.synced"] {
                self.persist_preference(key, None, window, cx);
            }
        }
        if let Some(id) = new_id {
            self.last_account = Some(id);
        }
        if (changed || recovered) && self.profile.is_some() {
            self.load_list(window, cx);
            self.refresh_pending(window, cx);
            self.ensure_library_fresh(window, cx);
        }
        if changed {
            self.enter_active_route(window, cx);
        }
        cx.emit(WorkspaceEvent::StateChanged);
        cx.notify();
    }

    fn load_preferences(&mut self, window: &Window, cx: &mut Context<Self>) {
        let core = self.core.clone();
        let auth_intent = self.auth_intent;
        let reset = self.preference_reset;
        let query_request = self.query_request;
        let filter_revision = self.preference_revision("kurisu.list.filter");
        let sort_revision = self.preference_revision("kurisu.list.sort");
        let dir_revision = self.preference_revision("kurisu.list.dir");
        let synced_revision = self.preference_revision("kurisu.list.synced");
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    let mut values = Vec::new();
                    for key in [
                        "kurisu.list.filter",
                        "kurisu.list.q",
                        "kurisu.list.sort",
                        "kurisu.list.dir",
                        "kurisu.list.synced",
                    ] {
                        values.push(worker.get_preference(key.into()).await?);
                    }
                    Ok(values)
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.preferences_loaded = true;
                let Ok(values) = result else {
                    if this.preference_reset == reset
                        && this.list.loaded
                        && !this.offline
                        && this.sync_due()
                    {
                        this.sync_list(window, cx);
                    }
                    return;
                };
                if this.preference_reset == reset
                    && this.preference_revision("kurisu.list.filter") == filter_revision
                    && let Some(filter) = values[0].as_deref()
                {
                    this.list.set_filter(filter, now_seconds());
                }
                if this.preference_reset == reset
                    && this.query_request == query_request
                    && let Some(query) = values[1].as_ref()
                {
                    this.list.set_query(query.clone(), now_seconds());
                    this.list_query
                        .update(cx, |input, cx| input.set_value(query.clone(), cx));
                }
                if this.preference_revision("kurisu.list.sort") == sort_revision
                    && let Some(sort) = values[2]
                        .as_deref()
                        .and_then(|value| SortKey::try_from(value).ok())
                {
                    this.list.sort = sort;
                }
                if this.preference_revision("kurisu.list.dir") == dir_revision {
                    this.list.desc = values[3]
                        .as_deref()
                        .map_or(this.list.sort.default_desc(), |value| value == "desc");
                }
                if this.preference_reset == reset
                    && this.auth_intent == auth_intent
                    && this.preference_revision("kurisu.list.synced") == synced_revision
                {
                    this.list.synced_at = values[4].as_deref().and_then(|value| value.parse().ok());
                }
                this.list.rebuild(now_seconds());
                this.reset_list_widget();
                if this.list.loaded && !this.offline && this.sync_due() {
                    this.sync_list(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn stage_query(&mut self, value: Option<String>) {
        self.query_request += 1;
        self.query_pending = Some(value);
        self.query_error.clear();
    }

    fn save_query(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.query_write_active || !self.query_error.is_empty() {
            return;
        }
        let Some(value) = self.query_pending.take() else {
            return;
        };
        self.query_write_active = true;
        self.active_writes += 1;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let saved = value.clone();
            let result = joined(
                core.spawn(
                    async move { worker.set_preference("kurisu.list.q".into(), saved).await },
                )
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.query_write_active = false;
                this.active_writes = this.active_writes.saturating_sub(1);
                if let Err(error) = result
                    && this.query_pending.is_none()
                {
                    this.query_pending = Some(value);
                    this.query_error = error;
                }
                // Only one query write runs, so a reset or newer edit settles last.
                this.save_query(window, cx);
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn preference_revision(&self, key: &'static str) -> u64 {
        self.preference_writes
            .get(key)
            .map_or(0, |write| write.revision)
    }

    fn preference_error(&self) -> Option<(&'static str, &str)> {
        PREFERENCE_KEYS.iter().copied().find_map(|key| {
            self.preference_writes
                .get(key)
                .and_then(|write| write.error.as_deref().map(|error| (key, error)))
        })
    }

    fn preference_error_message(&self) -> Option<String> {
        self.preference_error().map(|(key, error)| {
            let label = match key {
                "kurisu.list.filter" => "list filter",
                "kurisu.list.sort" => "list sort",
                "kurisu.list.dir" => "sort direction",
                "kurisu.list.synced" => "last sync time",
                "kurisu.cal.mine" => "calendar filter",
                "kurisu.torrents.new" => "torrent new-release filter",
                "kurisu.torrents.statuses" => "torrent status sections",
                "kurisu.torrents.filters" => "torrent search filters",
                _ => unreachable!(),
            };
            format!("Could not save the {label}: {error}. Retry the save.")
        })
    }

    fn preference_has_work(&self) -> bool {
        self.preference_active.is_some()
            || self
                .preference_writes
                .values()
                .any(|write| write.pending.is_some())
    }

    fn persist_preference(
        &mut self,
        key: &'static str,
        value: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        assert!(PREFERENCE_KEYS.contains(&key));
        let write = self.preference_writes.entry(key).or_default();
        write.revision = write.revision.wrapping_add(1);
        write.pending = Some(value);
        write.error = None;
        self.preference_alert_dismissed = false;
        self.save_preferences(window, cx);
        cx.notify();
    }

    fn save_preferences(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.preference_active.is_some() {
            return;
        }
        let Some(key) = PREFERENCE_KEYS.iter().copied().find(|key| {
            self.preference_writes
                .get(key)
                .is_some_and(|write| write.pending.is_some() && write.error.is_none())
        }) else {
            return;
        };
        let value = self
            .preference_writes
            .get_mut(key)
            .unwrap()
            .pending
            .take()
            .unwrap();
        self.preference_active = Some(key);
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let saved = value.clone();
            let result = joined(
                core.spawn(async move { worker.set_preference(key.into(), saved).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                if this.preference_active != Some(key) {
                    return;
                }
                this.preference_active = None;
                let write = this.preference_writes.get_mut(key).unwrap();
                if let Err(error) = result
                    && write.pending.is_none()
                {
                    write.pending = Some(value);
                    write.error = Some(error);
                    this.preference_alert_dismissed = false;
                }
                this.save_preferences(window, cx);
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn retry_preferences(&mut self, window: &Window, cx: &mut Context<Self>) {
        for write in self.preference_writes.values_mut() {
            write.error = None;
        }
        self.preference_alert_dismissed = false;
        self.save_preferences(window, cx);
        self.check_barrier(window, cx);
        cx.notify();
    }

    fn select_filter(&mut self, status: &str, window: &Window, cx: &mut Context<Self>) {
        self.sort_open = false;
        self.list.set_filter(status, now_seconds());
        self.reset_list_widget();
        self.persist_preference("kurisu.list.filter", Some(status.into()), window, cx);
        cx.notify();
    }

    fn select_sort(&mut self, sort: SortKey, window: &Window, cx: &mut Context<Self>) {
        self.list.set_sort(sort, now_seconds());
        self.sort_open = false;
        self.reset_list_widget();
        self.persist_preference("kurisu.list.sort", Some(sort.key().into()), window, cx);
        self.persist_preference(
            "kurisu.list.dir",
            Some(if self.list.desc { "desc" } else { "asc" }.into()),
            window,
            cx,
        );
        cx.notify();
    }

    fn flip_sort(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.list.set_desc(!self.list.desc, now_seconds());
        self.reset_list_widget();
        self.persist_preference(
            "kurisu.list.dir",
            Some(if self.list.desc { "desc" } else { "asc" }.into()),
            window,
            cx,
        );
        cx.notify();
    }

    fn load_list(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            return;
        }
        self.list_request += 1;
        let request = self.list_request;
        let generation = self.generation;
        let epoch = self.list_epoch;
        self.list.loading = true;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.local_entries().await });
        self.replace_reads("list", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.generation || request != this.list_request {
                    return;
                }
                if epoch != this.list_epoch {
                    this.load_list(window, cx);
                    return;
                }
                this.list.loading = false;
                match result {
                    Ok(entries) => {
                        this.list.replace(entries, now_seconds());
                        this.progress.reconcile_entries(generation, &this.list);
                        this.reconcile_detail_entry();
                        this.list.step_error = this
                            .progress
                            .first_error(generation)
                            .unwrap_or_default()
                            .into();
                        this.reset_list_widget();
                        this.discovery_list_changed();
                        this.prefetch_covers(0..32, window, cx);
                        this.list.error.clear();
                        if let Some((id, _)) = &this.pending_editor {
                            this.open_pending_editor(*id, window, cx);
                        }
                        if this.preferences_loaded && !this.offline && this.sync_due() {
                            this.sync_list(window, cx);
                        }
                    }
                    Err(error) => this.list.error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_due(&self) -> bool {
        let Some(last_attempt) = self.last_sync_attempt else {
            return true;
        };
        let stale = self
            .list
            .synced_at
            .is_none_or(|at| now_seconds() - at >= 300);
        stale && last_attempt.elapsed() >= Duration::from_secs(60)
    }

    fn sync_result_current(&self, generation: u64, request: u64) -> bool {
        self.generation == generation && self.sync_request == request
    }

    fn mark_list_synced(&mut self, window: &Window, cx: &mut Context<Self>) {
        let at = now_seconds();
        self.list.synced_at = Some(at);
        self.persist_preference("kurisu.list.synced", Some(at.to_string()), window, cx);
        let request = self.sync_request;
        let generation = self.generation;
        self.list_sync_flash = Some(request);
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let _ = this.update_in(cx, |this, _, cx| {
                if this.generation == generation && this.list_sync_flash == Some(request) {
                    this.list_sync_flash = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn sync_list(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() || self.offline || self.list.syncing {
            return;
        }
        self.last_sync_attempt = Some(Instant::now());
        self.list.syncing = true;
        self.cancel_reads("list");
        self.list.loading = false;
        self.list.error.clear();
        self.list_request += 1;
        self.sync_request += 1;
        let request = self.sync_request;
        let generation = self.generation;
        let epoch = self.list_epoch;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(core.spawn(async move { worker.sync_my_list().await }).await);
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.sync_result_current(generation, request) {
                    return;
                }
                this.list.syncing = false;
                if epoch != this.list_epoch {
                    if result.is_ok() {
                        this.mark_list_synced(window, cx);
                    }
                    this.load_list(window, cx);
                    return;
                }
                match result {
                    Ok(entries) => {
                        this.list.replace(entries, now_seconds());
                        this.progress.reconcile_entries(generation, &this.list);
                        this.reconcile_detail_entry();
                        this.list.step_error = this
                            .progress
                            .first_error(generation)
                            .unwrap_or_default()
                            .into();
                        this.mark_list_synced(window, cx);
                        this.reset_list_widget();
                        this.discovery_list_changed();
                        this.prefetch_covers(0..32, window, cx);
                        if let Some((id, _)) = &this.pending_editor {
                            this.open_pending_editor(*id, window, cx);
                        }
                    }
                    Err(error) => this.list.error = error,
                }
                this.refresh_pending(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn queue_list_refresh(
        &mut self,
        scope: Option<Vec<i64>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.route == Route::Torrents {
            self.load_torrent_entries(window, cx);
        }
        self.list_epoch += 1;
        self.list_scope = match (self.list_scope.take(), scope) {
            (Some(mut existing), Some(ids)) if !existing.is_empty() => {
                existing.extend(ids);
                Some(existing)
            }
            (None, Some(ids)) if !self.list_refresh_queued => Some(ids.into_iter().collect()),
            _ => None,
        };
        if self.list_refresh_queued {
            return;
        }
        self.list_refresh_queued = true;
        self.list_refresh_ticket += 1;
        let ticket = self.list_refresh_ticket;
        let generation = self.generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation || this.list_refresh_ticket != ticket {
                    return;
                }
                this.list_refresh_queued = false;
                let scope = this.list_scope.take();
                if let (Some(ids), Some(account)) =
                    (scope, this.profile.as_ref().map(|user| user.id))
                    && this.list.loaded
                    && !ids.is_empty()
                {
                    this.refresh_scoped(ids.into_iter().collect(), account, window, cx);
                    return;
                }
                this.load_list(window, cx);
            });
        })
        .detach();
    }

    fn refresh_scoped(
        &mut self,
        ids: Vec<i64>,
        account: i64,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.list_request += 1;
        let request = self.list_request;
        let generation = self.generation;
        let epoch = self.list_epoch;
        let core = self.core.clone();
        let worker = core.clone();
        let read_ids = ids.clone();
        let job = core.spawn(async move { worker.local_entry_updates(read_ids, account).await });
        self.replace_reads("list", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.generation || request != this.list_request {
                    return;
                }
                if epoch != this.list_epoch {
                    this.queue_list_refresh(None, window, cx);
                    return;
                }
                match result {
                    Ok(values) => {
                        let change = this.list.apply_scoped(&ids, values, now_seconds());
                        this.progress.reconcile_entries(generation, &this.list);
                        this.reconcile_detail_entry();
                        this.list.step_error = this
                            .progress
                            .first_error(generation)
                            .unwrap_or_default()
                            .into();
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        this.list.error.clear();
                        if let Some((id, _)) = this
                            .pending_editor
                            .as_ref()
                            .filter(|(waiting, _)| ids.contains(waiting))
                        {
                            this.open_pending_editor(*id, window, cx);
                        }
                    }
                    Err(error) => this.list.error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reconcile_detail_entry(&mut self) {
        if !self.list.loaded {
            return;
        }
        if let Some(id) = self.detail.media_id {
            self.detail.entry = self.list.entry(id).cloned();
            self.detail.entry_available = true;
            self.detail.entry_error.clear();
        }
    }

    fn step_progress(&mut self, id: i64, delta: i64, window: &Window, cx: &mut Context<Self>) {
        if self.barrier.is_some() || self.update_busy || self.auth_busy {
            return;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.media_id == id)
        {
            return;
        }
        let Some(mut entry) = self
            .list
            .entry(id)
            .or_else(|| {
                self.detail
                    .entry
                    .as_ref()
                    .filter(|entry| entry.media_id == id)
            })
            .cloned()
        else {
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
        let edit = self.progress.entry(self.generation, &entry);
        if !edit.step(delta, Instant::now()) {
            return;
        }
        let key = (self.generation, id);
        let seq = self.progress_timers.entry(key).or_default();
        *seq += 1;
        let current = *seq;
        let generation = self.generation;
        let due = edit.due.unwrap();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(due.saturating_duration_since(Instant::now()))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation == generation && this.progress_timers.get(&key) == Some(&current)
                {
                    this.save_progress(id, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn save_progress(&mut self, id: i64, window: &Window, cx: &mut Context<Self>) {
        let generation = self.generation;
        let entry_revision = self.list.revision();
        let Some((value, expected)) = self
            .progress
            .edit_mut(generation, id)
            .and_then(|edit| edit.begin_save())
        else {
            return;
        };
        self.active_writes += 1;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.set_progress(id, value, Some(expected)).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation {
                    this.check_barrier(window, cx);
                    return;
                }
                let outcome = result
                    .as_ref()
                    .map(|entry| entry.progress)
                    .map_err(Clone::clone);
                let superseded = result.is_ok() && this.list.revision() != entry_revision;
                if let Some(edit) = this.progress.edit_mut(generation, id) {
                    match outcome {
                        Ok(progress) if superseded => edit.finish_superseded_save(value, progress),
                        outcome => edit.finish_save(outcome),
                    }
                }
                if let Ok(entry) = result {
                    if superseded {
                        if this.list.loaded {
                            this.progress.reconcile_entries(generation, &this.list);
                            this.reconcile_detail_entry();
                        }
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                        // An unchanged baseline may belong to an earlier read of this entry.
                        if this.list.loaded
                            && this
                                .list
                                .entry(id)
                                .is_none_or(|entry| entry.progress != expected)
                        {
                            this.open_pending_editor(id, window, cx);
                        }
                    } else {
                        let change = this.list.apply_one(entry.clone(), now_seconds());
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        if this.detail.media_id == Some(id) {
                            this.detail.entry = Some(entry);
                            this.detail.entry_available = true;
                        }
                        this.open_pending_editor(id, window, cx);
                    }
                }
                this.list.step_error = this
                    .progress
                    .first_error(generation)
                    .unwrap_or_default()
                    .into();
                if this.delete_waiting
                    && this.editor.as_ref().is_some_and(|edit| edit.media_id == id)
                {
                    this.delete_waiting = false;
                    this.perform_delete(window, cx);
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn resolve_progress_review(
        &mut self,
        id: i64,
        keep_local: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.list.loaded {
            return;
        }
        let latest = self.list.entry(id).map(|entry| entry.progress);
        let recorded = self
            .progress
            .edit(self.generation, id)
            .and_then(|edit| edit.remote_progress);
        if latest != recorded {
            self.progress.reconcile_entries(self.generation, &self.list);
            self.list.step_error = self
                .progress
                .first_error(self.generation)
                .unwrap_or_default()
                .into();
            cx.notify();
            return;
        }
        if !self
            .progress
            .resolve_review(self.generation, id, latest, keep_local)
        {
            return;
        }
        self.list.step_error = self
            .progress
            .first_error(self.generation)
            .unwrap_or_default()
            .into();
        if keep_local {
            self.save_progress(id, window, cx);
        } else {
            self.open_pending_editor(id, window, cx);
            self.check_barrier(window, cx);
        }
        cx.notify();
    }

    fn retry_progress_errors(&mut self, window: &Window, cx: &mut Context<Self>) {
        for id in self.progress.dirty_ids(self.generation) {
            if self.progress.retry_error(self.generation, id) {
                self.save_progress(id, window, cx);
            }
        }
        self.list.step_error = self
            .progress
            .first_error(self.generation)
            .unwrap_or_default()
            .into();
        cx.notify();
    }

    fn load_detail(&mut self, id: i64, window: &Window, cx: &mut Context<Self>) {
        self.cancel_reads("detail-entry");
        self.cancel_reads("detail-recommendations");
        self.covers.clear();
        self.cover_tickets.clear();
        self.cover_images.clear();
        self.cover_regions.clear();
        let request = self.detail.start(id);
        let entry_revision = self.list.revision();
        let generation = self.generation;
        let core = self.core.clone();
        let worker = core.clone();
        let detail_job = core.spawn(async move { worker.get_media_detail(id).await });
        let worker = core.clone();
        let entry_job = core.spawn(async move { worker.get_entry(id).await });
        let worker = core.clone();
        let recs_job = core.spawn(async move { worker.get_recommendations(id).await });
        self.replace_reads(
            "detail",
            vec![
                detail_job.abort_handle(),
                entry_job.abort_handle(),
                recs_job.abort_handle(),
            ],
        );
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(detail_job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.generation
                    || this.route != Route::Anime(id)
                    || !this.detail.current(id, request)
                {
                    return;
                }
                this.ensure_cover_scale(window.scale_factor());
                if let Ok(detail) = &result {
                    if let Some(url) = detail
                        .media
                        .cover_large
                        .as_deref()
                        .or(detail.media.cover_medium.as_deref())
                    {
                        this.request_cover(id, url, 128, 184);
                    }
                    if let Some(url) = detail.media.banner_image.as_deref() {
                        this.request_cover(id, url, 800, 192);
                    }
                    for relation in &detail.relations {
                        if let Some(url) = relation.media.cover_medium.as_deref() {
                            this.request_cover(relation.media.id, url, 112, 160);
                        }
                    }
                    for (index, character) in detail.characters.iter().enumerate() {
                        if let Some(url) = character.image.as_deref() {
                            this.request_cover(-1 - index as i64, url, 112, 160);
                        }
                    }
                    for (index, person) in detail.staff.iter().enumerate() {
                        if let Some(url) = person.image.as_deref() {
                            this.request_cover(-200_001 - index as i64, url, 112, 160);
                        }
                    }
                }
                this.detail.finish_detail(id, request, result);
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(entry_job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if generation != this.generation
                    || this.route != Route::Anime(id)
                    || !this.detail.current(id, request)
                    || (this.list.loaded && this.list.revision() != entry_revision)
                {
                    return;
                }
                this.detail.finish_entry(id, request, result);
                cx.notify();
            });
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(recs_job.await);
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
    }

    fn add_media(
        &mut self,
        id: i64,
        status: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.profile.is_none()
            || self.editor_adding.contains(&id)
            || self.added_recommendations.contains(&id)
            || self.barrier.is_some()
            || self.auth_busy
            || self.update_busy
        {
            return;
        }
        self.editor_adding.insert(id);
        self.active_writes += 1;
        let generation = self.generation;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move {
                    match worker.get_entry(id).await? {
                        Some(entry) => Ok(entry),
                        None => {
                            worker
                                .update_entry(id, Some(status.into()), None, None, None)
                                .await
                        }
                    }
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation {
                    this.check_barrier(window, cx);
                    return;
                }
                this.editor_adding.remove(&id);
                match result {
                    Ok(entry) => {
                        this.added_recommendations.insert(id);
                        let change = this.list.apply_one(entry.clone(), now_seconds());
                        this.apply_list_change(change);
                        this.discovery_list_changed();
                        if this.detail.media_id == Some(id) {
                            this.detail.entry = Some(entry);
                            this.detail.entry_available = true;
                        }
                        this.queue_list_refresh(Some(vec![id]), window, cx);
                    }
                    Err(error) => this.global_error = error,
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn refresh_pending(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.profile.is_none() {
            return;
        }
        self.pending_request += 1;
        let request = self.pending_request;
        let generation = self.generation;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_pending_changes().await });
        self.replace_reads("pending", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _window, cx| {
                if generation != this.generation || request != this.pending_request {
                    return;
                }
                match result {
                    Ok(items) => {
                        this.pending = items;
                        this.pending_error.clear();
                    }
                    Err(error) => this.pending_error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn retry_pending(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.pending_busy || self.barrier.is_some() {
            return;
        }
        self.pending_busy = true;
        self.active_writes += 1;
        let generation = self.generation;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.sync_pending_changes().await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation {
                    this.check_barrier(window, cx);
                    return;
                }
                this.pending_busy = false;
                match result {
                    Ok(items) => {
                        this.pending = items;
                        this.pending_error.clear();
                    }
                    Err(error) => this.pending_error = error,
                }
                this.queue_list_refresh(None, window, cx);
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn resolve_pending(
        &mut self,
        id: i64,
        keep_local: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_busy || self.barrier.is_some() {
            return;
        }
        self.pending_busy = true;
        self.active_writes += 1;
        let generation = self.generation;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.resolve_pending_change(id, keep_local).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation {
                    this.check_barrier(window, cx);
                    return;
                }
                this.pending_busy = false;
                if let Err(error) = result {
                    this.pending_error = error;
                }
                this.refresh_pending(window, cx);
                this.queue_list_refresh(Some(vec![id]), window, cx);
                this.check_barrier(window, cx);
            });
        })
        .detach();
    }

    fn confirm_prompt(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(event) = self.prompt_event.clone() else {
            return;
        };
        let Some(prompt) = prompt_payload(&event).cloned() else {
            return;
        };
        if self.prompt_busy || self.barrier.is_some() {
            return;
        }
        if !self.core.event_is_current(&event) {
            self.prompt = None;
            self.prompt_event = None;
            self.prompt_queue.clear();
            cx.notify();
            return;
        }
        self.prompt_busy = true;
        self.prompt_error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let core = self.core.clone();
        let media_id = prompt.media_id;
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let event_for_ui = event.clone();
            let result = joined(
                core.spawn(async move {
                    if !worker.event_is_current(&event) {
                        return Err("Playback prompt is no longer current.".into());
                    }
                    let fresh = worker.get_entry(prompt.media_id).await?;
                    if !worker.event_is_current(&event) {
                        return Err("Playback prompt is no longer current.".into());
                    }
                    if let Some(entry) = fresh
                        && entry.progress < prompt.episode
                    {
                        worker
                            .set_progress(prompt.media_id, prompt.episode, Some(entry.progress))
                            .await?;
                    }
                    Ok(())
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if generation != this.generation
                    || this
                        .prompt_event
                        .as_ref()
                        .is_none_or(|current| !same_prompt(current, &event_for_ui))
                {
                    if result.is_ok() {
                        this.ack_prompt_event(event_for_ui);
                    }
                    this.check_barrier(window, cx);
                    return;
                }
                this.prompt_busy = false;
                match result {
                    Ok(()) => {
                        this.dismiss_prompt(cx);
                        this.queue_list_refresh(Some(vec![media_id]), window, cx);
                    }
                    Err(error) => this.prompt_error = error,
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn load_update_notice(&mut self, window: &Window, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = core
                .spawn(async move {
                    (
                        worker.take_pending_update().await,
                        worker.take_update_failed().await,
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, _window, cx| {
                if let Ok((info, error)) = result {
                    if let Some(info) = info {
                        this.show_update(info);
                    }
                    if let Some(error) = error {
                        this.update_failed = error;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn note_restart_pending(&mut self) {
        self.restart_pending = true;
        if let Some(update) = self.update.as_mut() {
            update.restart_pending = true;
        }
        if let Some(update) = self.settings.update.as_mut() {
            update.restart_pending = true;
        }
    }

    fn show_update(&mut self, mut update: UpdateInfo) {
        if update.restart_pending {
            self.note_restart_pending();
        }
        update.restart_pending |= self.restart_pending;
        if update.available && update.tag != self.dismissed_update {
            self.update = Some(update);
        }
    }

    pub fn request_logout(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.auth_busy || self.modal_open() {
            return;
        }
        self.logout_confirming = true;
        cx.notify();
    }

    pub fn request_close(
        &mut self,
        intent: CloseIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.barrier = Some(BarrierAction::Close(intent));
        self.barrier_error.clear();
        self.check_barrier(window, cx);
    }

    fn start_barrier(&mut self, action: BarrierAction, window: &Window, cx: &mut Context<Self>) {
        if action == BarrierAction::Install && (self.restart_pending || self.update_busy) {
            return;
        }
        self.barrier = Some(action);
        self.barrier_error.clear();
        self.check_barrier(window, cx);
    }

    fn cancel_barrier(&mut self, cx: &mut Context<Self>) {
        if matches!(self.barrier.take(), Some(BarrierAction::Close(_))) {
            cx.emit(WorkspaceEvent::CloseCancelled);
        }
        self.queued_login = None;
        self.barrier_error.clear();
        cx.notify();
    }

    fn check_barrier(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(action) = self.barrier else {
            return;
        };
        if !self.query_error.is_empty() {
            self.barrier_error = format!(
                "Could not save the list search: {}. Kurisu is still open.",
                self.query_error
            );
            cx.notify();
            return;
        }
        self.save_query(window, cx);
        self.save_preferences(window, cx);
        if let Some(error) = self.preference_error_message() {
            self.barrier_error = format!("{error} Kurisu is still open.");
            cx.notify();
            return;
        }
        if matches!(action, BarrierAction::Close(_) | BarrierAction::Install)
            && self.has_inactive_drafts()
        {
            self.barrier_error = "Unsaved drafts belong to another account. Sign in to that account to review them, or discard them to continue.".into();
            cx.notify();
            return;
        }
        if let Some(error) = self.progress.first_error(self.generation) {
            self.barrier_error = format!("Could not save progress: {error}. Kurisu is still open.");
            cx.notify();
            return;
        }
        let dirty = self.progress.dirty_ids(self.generation);
        for id in dirty {
            self.save_progress(id, window, cx);
        }
        if self.active_writes > 0
            || self.preference_has_work()
            || self.progress.has_in_flight(self.generation)
        {
            cx.notify();
            return;
        }
        if self.editor.is_some()
            && let Err(error) = self.read_editor_inputs(cx)
            && let Some(editor) = self.editor.as_mut()
        {
            editor.error = error;
        }
        if self.editor.as_ref().is_some_and(|editor| editor.busy) {
            cx.notify();
            return;
        }
        if let Some(error) = self
            .editor
            .as_ref()
            .filter(|editor| !editor.error.is_empty())
            .map(|editor| editor.error.as_str())
        {
            self.barrier_error = format!("Could not save entry: {error}. Kurisu is still open.");
            cx.notify();
            return;
        }
        let score_available = !self.offline
            || self
                .profile
                .as_ref()
                .and_then(|user| user.score_format.as_ref())
                .is_some();
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.is_dirty(score_available))
        {
            self.save_editor(window, cx);
            if let Some(error) = self
                .editor
                .as_ref()
                .filter(|editor| !editor.busy && !editor.error.is_empty())
                .map(|editor| editor.error.as_str())
            {
                self.barrier_error =
                    format!("Could not save entry: {error}. Kurisu is still open.");
            }
            cx.notify();
            return;
        }
        self.barrier_error.clear();
        self.barrier = None;
        match action {
            BarrierAction::Close(intent) => cx.emit(WorkspaceEvent::ReadyToClose(intent)),
            BarrierAction::Logout => self.perform_logout(window, cx),
            BarrierAction::Install => self.perform_install(window, cx),
            BarrierAction::Login => {
                if let Some((oauth, token)) = self.queued_login.take() {
                    self.perform_login(oauth, token, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn retry_barrier(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.barrier.is_none() {
            return;
        }
        self.barrier_error.clear();
        self.query_error.clear();
        let ids = self.progress.dirty_ids(self.generation);
        for id in ids {
            self.progress.retry_error(self.generation, id);
        }
        if let Some(editor) = self.editor.as_mut() {
            editor.error.clear();
        }
        self.retry_preferences(window, cx);
    }

    fn has_inactive_drafts(&self) -> bool {
        let active = self.profile.as_ref().map(|user| user.id);
        self.progress_drafts
            .iter()
            .any(|(account, drafts)| Some(*account) != active && !drafts.is_empty())
            || self
                .editor_drafts
                .keys()
                .any(|account| Some(*account) != active)
            || self
                .library_drafts
                .keys()
                .any(|account| Some(*account) != active)
    }

    fn discard_inactive_drafts(&mut self, window: &Window, cx: &mut Context<Self>) {
        let active = self.profile.as_ref().map(|user| user.id);
        self.progress_drafts
            .retain(|account, _| Some(*account) == active);
        self.editor_drafts
            .retain(|account, _| Some(*account) == active);
        self.library_drafts
            .retain(|account, _| Some(*account) == active);
        self.barrier_error.clear();
        self.check_barrier(window, cx);
    }

    fn login_with_token(&mut self, window: &Window, cx: &mut Context<Self>) {
        let value = self.token.read(cx).value().trim().to_owned();
        if value.is_empty() {
            return;
        }
        self.login(false, Some(value), window, cx);
    }

    fn login(
        &mut self,
        oauth: bool,
        token: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.auth_busy && !self.auth_retry_available() {
            return;
        }
        if self.editor.is_some()
            && let Err(error) = self.read_editor_inputs(cx)
            && let Some(editor) = self.editor.as_mut()
        {
            editor.error = error;
        }
        let score_available = !self.offline
            || self
                .profile
                .as_ref()
                .and_then(|user| user.score_format.as_ref())
                .is_some();
        if self.query_pending.is_some()
            || !self.query_error.is_empty()
            || self.preference_has_work()
            || self.preference_error().is_some()
            || self.active_writes > 0
            || self.progress.has_in_flight(self.generation)
            || !self.progress.dirty_ids(self.generation).is_empty()
            || self.editor.as_ref().is_some_and(|editor| {
                editor.busy || !editor.error.is_empty() || editor.is_dirty(score_available)
            })
        {
            self.queued_login = Some((oauth, token));
            self.start_barrier(BarrierAction::Login, window, cx);
            return;
        }
        self.perform_login(oauth, token, window, cx);
    }

    fn perform_login(
        &mut self,
        oauth: bool,
        token: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(previous) = self.auth_abort.take() {
            self.core.cancel_login();
            previous.abort();
        }
        self.restoring_session = false;
        self.auth_busy = true;
        self.auth_oauth = oauth;
        self.auth_error.clear();
        self.auth_intent += 1;
        let intent = self.auth_intent;
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            if oauth {
                worker.login_oauth().await
            } else {
                worker.login_with_token(token.unwrap_or_default()).await
            }
        });
        self.auth_abort = Some(job.abort_handle());
        cx.emit(WorkspaceEvent::StateChanged);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.auth_intent != intent {
                    return;
                }
                this.auth_abort = None;
                this.auth_busy = false;
                this.auth_oauth = false;
                match result {
                    Ok(user) => {
                        this.token.update(cx, |input, cx| input.set_value("", cx));
                        this.set_profile(Some(user), window, cx);
                    }
                    Err(error) => {
                        this.auth_error = error;
                        this.restore_session(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn auth_retry_available(&self) -> bool {
        self.auth_busy && self.auth_oauth && self.auth_abort.is_some()
    }

    fn stop_pending_auth(&mut self) {
        if let Some(job) = self.auth_abort.take() {
            self.core.cancel_login();
            job.abort();
            self.auth_intent += 1;
        }
        self.auth_busy = false;
        self.auth_oauth = false;
        self.queued_login = None;
    }

    fn cancel_login(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(job) = self.auth_abort.take() else {
            return;
        };
        self.core.cancel_login();
        job.abort();
        self.auth_intent += 1;
        self.auth_busy = false;
        self.auth_oauth = false;
        self.auth_error.clear();
        self.restore_session(window, cx);
        cx.notify();
    }

    fn perform_logout(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.auth_busy = true;
        self.auth_intent += 1;
        let intent = self.auth_intent;
        let generation = self.generation;
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(core.spawn(async move { worker.logout().await }).await);
            let _ = this.update_in(cx, |this, window, cx| {
                if intent != this.auth_intent {
                    return;
                }
                this.auth_busy = false;
                match result {
                    Ok(()) => this.set_profile(None, window, cx),
                    Err(error) => {
                        this.auth_error = error;
                        if generation == this.generation {
                            this.restore_session(window, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn perform_install(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.update_busy || self.restart_pending {
            return;
        }
        self.update_busy = true;
        self.update_failed.clear();
        let core = self.core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let worker = core.clone();
            let result = joined(
                core.spawn(async move { worker.install_update().await })
                    .await,
            );
            let _ = this.update_in(cx, |this, _window, cx| {
                this.update_busy = false;
                match result {
                    Err(error) => this.update_failed = error,
                    Ok(outcome) if outcome == "restarting" => {
                        this.note_restart_pending();
                        cx.emit(WorkspaceEvent::UpdateInstallerStarted);
                    }
                    Ok(_) => this.note_restart_pending(),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.route == Route::MyList {
            let window_size = window.viewport_size();
            let list_bounds = self.list_widget.viewport_bounds();
            if self.list_cover_window_size != window_size
                || self
                    .list_cover_bounds
                    .is_some_and(|bounds| bounds != list_bounds && list_bounds.size.height > px(0.))
            {
                self.list_cover_window_size = window_size;
                self.mark_list_cover_refresh();
            }
        }
        self.schedule_list_cover_refresh(window, cx);
        self.render_workspace(window, cx)
    }
}

#[cfg(test)]
mod menu_focus_tests {
    use super::*;
    use gpui::{Focusable, Modifiers, MouseButton, TestAppContext, VisualTestContext};
    use kurisu_core::models::ListEntry;
    use kurisu_core::{Platform, UpdateChannel};
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
            "kurisu-native-menu-focus-{}-{nonce}",
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

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, app| window.draw(app).clear(app));
    }

    #[gpui::test]
    fn editor_status_contains_choice_and_dismisses_outside(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.apply_one(
                ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    progress: 1,
                    media: Some(Media {
                        id: 9,
                        title_english: Some("Synthetic series".into()),
                        episodes: Some(24),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                0,
            );
            workspace.open_editor(9, window, cx);
        });
        draw(cx);
        let status = cx.debug_bounds("editor-status").unwrap();
        cx.simulate_click(status.center(), Modifiers::default());
        draw(cx);
        assert!(workspace.read_with(cx, |workspace, _| workspace.editor_status_open));
        let choice = cx.debug_bounds("edit-status-choice-COMPLETED").unwrap();
        cx.simulate_mouse_down(choice.center(), MouseButton::Left, Modifiers::default());
        draw(cx);
        assert!(workspace.update_in(cx, |workspace, window, cx| {
            workspace.editor_status_focus.contains_focused(window, cx)
        }));
        assert!(!workspace.update_in(cx, |workspace, window, _| {
            workspace.editor_status_focus.is_focused(window)
        }));

        cx.simulate_mouse_up(choice.center(), MouseButton::Left, Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| {
            let editor = workspace.editor.as_ref().unwrap();
            !workspace.editor_status_open && editor.status == "COMPLETED" && editor.progress == 24
        }));

        cx.simulate_click(status.center(), Modifiers::default());
        let progress = workspace.read_with(cx, |workspace, _| {
            workspace.editor_inputs.as_ref().unwrap().progress.clone()
        });
        progress.update_in(cx, |input, window, cx| {
            window.focus(&input.focus_handle(cx), cx)
        });
        draw(cx);
        assert!(!workspace.read_with(cx, |workspace, _| workspace.editor_status_open));

        cx.simulate_click(status.center(), Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| workspace.editor_status_open));
        cx.simulate_click(
            gpui::point(status.origin.x + px(20.), status.origin.y - px(12.)),
            Modifiers::default(),
        );
        assert!(workspace.read_with(cx, |workspace, _| {
            !workspace.editor_status_open && workspace.editor.is_some()
        }));
    }

    #[gpui::test]
    fn busy_editor_status_is_inert_and_recovers(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.list.apply_one(
                ListEntry {
                    media_id: 9,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id: 9,
                        episodes: Some(24),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                0,
            );
            workspace.open_editor(9, window, cx);
        });
        draw(cx);
        let status = cx.debug_bounds("editor-status").unwrap();
        cx.simulate_click(status.center(), Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| workspace.editor_status_open));
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.editor.as_mut().unwrap().busy = true;
            workspace.set_editor_inputs_disabled(true, cx);
            cx.notify();
        });
        draw(cx);
        assert!(!workspace.read_with(cx, |workspace, _| workspace.editor_status_open));
        cx.simulate_click(status.center(), Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| {
            !workspace.editor_status_open && workspace.editor.as_ref().unwrap().status == "CURRENT"
        }));
        workspace.update_in(cx, |workspace, window, cx| {
            window.focus(&workspace.modal_focus.root, cx);
        });
        for _ in 0..24 {
            workspace.update_in(cx, |_, window, cx| window.focus_next(cx));
            assert!(!workspace.update_in(cx, |workspace, window, _| {
                workspace.editor_status_focus.is_focused(window)
            }));
        }
        workspace.update_in(cx, |workspace, _, cx| {
            workspace.editor.as_mut().unwrap().busy = false;
            workspace.set_editor_inputs_disabled(false, cx);
            cx.notify();
        });
        draw(cx);
        cx.simulate_click(status.center(), Modifiers::default());
        assert!(workspace.read_with(cx, |workspace, _| workspace.editor_status_open));
    }

    #[gpui::test]
    fn season_selectors_keep_choice_focus_and_dismiss_outside(cx: &mut TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Seasons;
            workspace.list.loaded = true;
            workspace.seasons.year_input =
                Some(cx.new(|cx| TextInput::new("2026", "Year", window, cx)));
            workspace.seasons.title_input =
                Some(cx.new(|cx| TextInput::new("", "Filter titles…", window, cx)));
            cx.notify();
        });
        draw(cx);
        for (selector, choice, kind) in [
            ("season-jump", "season-choice-WINTER", 0),
            ("season-format", "format-choice-TV", 1),
            ("season-list-status", "status-choice-UNLISTED", 2),
        ] {
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), Modifiers::default());
            draw(cx);
            assert!(workspace.read_with(cx, |workspace, _| match kind {
                0 => workspace.seasons.season_open,
                1 => workspace.seasons.format_open,
                _ => workspace.seasons.list_open,
            }));
            let option = cx.debug_bounds(choice).unwrap();
            cx.simulate_mouse_down(option.center(), MouseButton::Left, Modifiers::default());
            draw(cx);
            assert!(workspace.update_in(cx, |workspace, window, cx| match kind {
                0 => workspace.season_jump_focus.contains_focused(window, cx),
                1 => workspace.season_format_focus.contains_focused(window, cx),
                _ => workspace.season_status_focus.contains_focused(window, cx),
            }));
            cx.simulate_mouse_up(option.center(), MouseButton::Left, Modifiers::default());
            assert!(workspace.read_with(cx, |workspace, _| match kind {
                0 =>
                    !workspace.seasons.season_open && workspace.seasons.selected_season == "WINTER",
                1 => !workspace.seasons.format_open && workspace.seasons.format_filter == "TV",
                _ => !workspace.seasons.list_open && workspace.seasons.list_filter == "UNLISTED",
            }));

            cx.simulate_click(bounds.center(), Modifiers::default());
            let title = workspace.read_with(cx, |workspace, _| {
                workspace.seasons.title_input.as_ref().unwrap().clone()
            });
            title.update_in(cx, |input, window, cx| {
                window.focus(&input.focus_handle(cx), cx)
            });
            draw(cx);
            assert!(workspace.read_with(cx, |workspace, _| match kind {
                0 => !workspace.seasons.season_open,
                1 => !workspace.seasons.format_open,
                _ => !workspace.seasons.list_open,
            }));

            cx.simulate_click(bounds.center(), Modifiers::default());
            assert!(workspace.read_with(cx, |workspace, _| match kind {
                0 => workspace.seasons.season_open,
                1 => workspace.seasons.format_open,
                _ => workspace.seasons.list_open,
            }));
            let outside = cx
                .debug_bounds(if kind == 0 {
                    "season-year-field"
                } else {
                    "season-title-field"
                })
                .unwrap();
            cx.simulate_click(outside.center(), Modifiers::default());
            assert!(workspace.read_with(cx, |workspace, _| match kind {
                0 => !workspace.seasons.season_open,
                1 => !workspace.seasons.format_open,
                _ => !workspace.seasons.list_open,
            }));
        }
    }
}
