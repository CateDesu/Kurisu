use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    Anchor, AnyElement, Context, Entity, FontWeight, InteractiveElement, ListAlignment, ListOffset,
    ListState, Role, Subscription, Toggled, WeakEntity, Window, anchored, deferred, div, img, list,
    point, prelude::*, px, rgb,
};
use kurisu_core::{
    Platform,
    models::{FeedFailure, ListEntry, ShowTorrents, TorrentItem},
};

use super::{Workspace, covers::CoverOwner, joined, state::status_label};
use crate::{
    components::input::{TextInput, TextInputEvent},
    components::scrollbar::{list_scrollbar, replace_list_rows},
    components::{button, page_heading},
    contracts::Route,
    platform::NativePlatform,
    theme::{ACCENT, BASE, DIM, EDGE, ERROR, INK, OCHRE, PANEL, PANEL_2, serif_font},
};

#[cfg(all(test, feature = "fixture-runtime"))]
#[path = "torrent_read_tests.rs"]
mod torrent_read_tests;

const STATUS_ORDER: [&str; 6] = [
    "CURRENT",
    "PLANNING",
    "REPEATING",
    "PAUSED",
    "COMPLETED",
    "DROPPED",
];
const CATEGORY_OPTIONS: [(&str, &str); 5] = [
    ("1_0", "All languages"),
    ("1_2", "English-translated"),
    ("1_3", "Non-English-translated"),
    ("1_4", "Raw"),
    ("1_1", "Anime music videos"),
];
const RELEASE_OPTIONS: [(&str, &str); 3] = [
    ("0", "All releases"),
    ("1", "No remakes"),
    ("2", "Trusted only"),
];

struct ShowGroup {
    entry: ListEntry,
    title: String,
    cover: Option<String>,
    items: Vec<TorrentItem>,
    has_new: bool,
    newest: i64,
}

enum TorrentDisplayRow {
    SearchHeading,
    SearchRelease(Arc<TorrentItem>),
    Category {
        status: &'static str,
        total: usize,
        releases: usize,
        new_count: usize,
    },
    Filter {
        status: &'static str,
        shown: usize,
        total: usize,
    },
    Show(Arc<ShowGroup>),
    ChoiceStatus {
        media_id: i64,
        text: String,
        retry: bool,
    },
    ChoiceSection {
        media_id: i64,
        title: String,
        count: usize,
    },
    ChoiceRelease {
        media_id: i64,
        item: Arc<TorrentItem>,
    },
    ChoiceMore {
        media_id: i64,
        title: String,
        count: usize,
        expanded: bool,
    },
    Unmatched(usize),
}

impl TorrentDisplayRow {
    fn key(&self) -> String {
        match self {
            Self::SearchHeading => "search-heading".into(),
            Self::SearchRelease(item) => format!("search-{}", release_row_key(item)),
            Self::Category { status, .. } => format!("category-{status}"),
            Self::Filter { status, .. } => format!("filter-{status}"),
            Self::Show(group) => format!("show-{}", group.entry.media_id),
            Self::ChoiceStatus { media_id, text, .. } => format!("choice-{media_id}-status-{text}"),
            Self::ChoiceSection {
                media_id, title, ..
            } => format!("choice-{media_id}-section-{title}"),
            Self::ChoiceRelease { media_id, item } => {
                format!("choice-{media_id}-{}", release_row_key(item))
            }
            Self::ChoiceMore {
                media_id, title, ..
            } => format!("choice-{media_id}-more-{title}"),
            Self::Unmatched(_) => "unmatched".into(),
        }
    }
}

struct ChoiceState {
    key: String,
    fetched_at: Option<Instant>,
    choices: Option<ShowTorrents>,
    loading: bool,
    error: String,
    request: u64,
    expanded_sections: HashSet<String>,
    abort: Option<tokio::task::AbortHandle>,
}

impl ChoiceState {
    fn new(key: String) -> Self {
        Self {
            key,
            fetched_at: None,
            choices: None,
            loading: false,
            error: String::new(),
            request: 0,
            expanded_sections: HashSet::new(),
            abort: None,
        }
    }
}

pub(super) struct TorrentsState {
    items: Vec<TorrentItem>,
    failures: Vec<FeedFailure>,
    entries: Vec<ListEntry>,
    feeds: Vec<String>,
    results: Vec<TorrentItem>,
    rows: Arc<Vec<TorrentDisplayRow>>,
    widget: Option<ListState>,
    pub(super) search_input: Option<Entity<TextInput>>,
    feed_input: Option<Entity<TextInput>>,
    show_inputs: HashMap<&'static str, Entity<TextInput>>,
    show_filters: HashMap<&'static str, String>,
    subscriptions: Vec<Subscription>,
    loaded: bool,
    pub(super) loading: bool,
    refreshed: Option<Instant>,
    entries_loaded: bool,
    pub(super) entries_loading: bool,
    pub(super) entries_reload_pending: bool,
    feeds_loaded: bool,
    pub(super) feeds_loading: bool,
    pub(super) searching: bool,
    searched: bool,
    adding_feed: bool,
    removing_feed: Option<String>,
    removing: bool,
    show_feeds: bool,
    show_tips: bool,
    category_menu: bool,
    release_menu: bool,
    category: String,
    release: String,
    new_only: bool,
    expanded_statuses: HashSet<&'static str>,
    expanded_shows: HashSet<i64>,
    choices: HashMap<i64, ChoiceState>,
    opened_identities: HashSet<String>,
    submitted_query: String,
    error: String,
    feed_error: String,
    entries_error: String,
    feeds_error: String,
    prefs_loaded: bool,
    request: u64,
    entries_request: u64,
    feeds_request: u64,
    pub(super) search_request: u64,
    prefs_request: u64,
    choice_request: u64,
}

impl Default for TorrentsState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            failures: Vec::new(),
            entries: Vec::new(),
            feeds: Vec::new(),
            results: Vec::new(),
            rows: Arc::new(Vec::new()),
            widget: None,
            search_input: None,
            feed_input: None,
            show_inputs: HashMap::new(),
            show_filters: HashMap::new(),
            subscriptions: Vec::new(),
            loaded: false,
            loading: false,
            refreshed: None,
            entries_loaded: false,
            entries_loading: false,
            entries_reload_pending: false,
            feeds_loaded: false,
            feeds_loading: false,
            searching: false,
            searched: false,
            adding_feed: false,
            removing_feed: None,
            removing: false,
            show_feeds: false,
            show_tips: false,
            category_menu: false,
            release_menu: false,
            category: "1_0".into(),
            release: "0".into(),
            new_only: false,
            expanded_statuses: HashSet::from(["CURRENT"]),
            expanded_shows: HashSet::new(),
            choices: HashMap::new(),
            opened_identities: HashSet::new(),
            submitted_query: String::new(),
            error: String::new(),
            feed_error: String::new(),
            entries_error: String::new(),
            feeds_error: String::new(),
            prefs_loaded: false,
            request: 0,
            entries_request: 0,
            feeds_request: 0,
            search_request: 0,
            prefs_request: 0,
            choice_request: 0,
        }
    }
}

impl Drop for TorrentsState {
    fn drop(&mut self) {
        for choice in self.choices.values_mut() {
            if let Some(abort) = choice.abort.take() {
                abort.abort();
            }
        }
    }
}

impl TorrentsState {
    fn apply_opened_releases(&self, items: &mut [TorrentItem]) {
        for item in items {
            if self.opened_identities.contains(release_identity(item)) {
                item.seen = true;
                item.is_new = false;
            }
        }
    }

    pub(super) fn abort_choices(&mut self) {
        for choice in self.choices.values_mut() {
            if let Some(abort) = choice.abort.take() {
                abort.abort();
            }
            choice.loading = false;
        }
    }

    pub(super) fn reset_account(&mut self) {
        let mut next = Self::default();
        std::mem::swap(&mut next.new_only, &mut self.new_only);
        std::mem::swap(&mut next.expanded_statuses, &mut self.expanded_statuses);
        std::mem::swap(&mut next.category, &mut self.category);
        std::mem::swap(&mut next.release, &mut self.release);
        std::mem::swap(&mut next.prefs_loaded, &mut self.prefs_loaded);
        *self = next;
    }
}

impl Workspace {
    pub(super) fn enter_torrents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.torrents.search_input.is_none() {
            let search =
                cx.new(|cx| TextInput::new("", "Search nyaa for any torrent…", window, cx));
            let feed = cx.new(|cx| TextInput::new("", "https://nyaa.si/?page=rss&q=…", window, cx));
            self.torrents.subscriptions.push(cx.subscribe_in(
                &search,
                window,
                |this, _, event, window, cx| {
                    if matches!(event, TextInputEvent::Submitted) {
                        this.search_torrents(None, window, cx);
                    }
                },
            ));
            self.torrents.subscriptions.push(cx.subscribe_in(
                &feed,
                window,
                |this, _, event, window, cx| {
                    if matches!(event, TextInputEvent::Submitted) {
                        this.add_torrent_feed(window, cx);
                    }
                },
            ));
            self.torrents.search_input = Some(search);
            self.torrents.feed_input = Some(feed);
            for status in STATUS_ORDER {
                let input = cx.new(|cx| TextInput::new("", "Filter shows…", window, cx));
                self.torrents.subscriptions.push(cx.subscribe(
                    &input,
                    move |this, input, event, cx| {
                        if let TextInputEvent::Changed(value) = event {
                            if this.writes_fenced() {
                                let current = this
                                    .torrents
                                    .show_filters
                                    .get(status)
                                    .cloned()
                                    .unwrap_or_default();
                                input.update(cx, |input, cx| input.set_value(current, cx));
                                return;
                            }
                            this.torrents.show_filters.insert(status, value.clone());
                            this.rebuild_torrent_rows();
                            cx.notify();
                        }
                    },
                ));
                self.torrents.show_inputs.insert(status, input);
            }
            let widget = ListState::new(0, ListAlignment::Top, px(800.));
            let weak = cx.weak_entity();
            widget.set_scroll_handler(move |event, _, app| {
                let _ = weak.update(app, |this, cx| {
                    this.prefetch_torrent_covers(event.visible_range.clone());
                    cx.notify();
                });
            });
            self.torrents.widget = Some(widget);
        }
        if !self.torrents.prefs_loaded {
            self.load_torrent_preferences(window, cx);
        }
        if (!self.torrents.entries_loaded || self.torrents.entries_reload_pending)
            && !self.torrents.entries_loading
        {
            self.load_torrent_entries(window, cx);
        }
        if !self.torrents.feeds_loaded && !self.torrents.feeds_loading {
            self.load_torrent_feeds(window, cx);
        }
        self.ensure_torrent_feed_loaded(window, cx);
        self.reload_expanded_torrent_choices(window, cx);
        self.refresh_torrents_covers();
    }

    pub(super) fn refresh_torrents_covers(&mut self) {
        let start = self
            .torrents
            .widget
            .as_ref()
            .map_or(0, |widget| widget.logical_scroll_top().item_ix);
        self.prefetch_torrent_covers(start..start.saturating_add(24));
    }

    fn prefetch_torrent_covers(&mut self, range: std::ops::Range<usize>) {
        let start = range.start.saturating_sub(4);
        let end = range.end.saturating_add(8).min(self.torrents.rows.len());
        let mut visible = Vec::new();
        let mut nearby = Vec::new();
        for index in start.min(end)..end {
            if let TorrentDisplayRow::Show(group) = &self.torrents.rows[index]
                && let Some(url) = group.cover.as_ref()
            {
                let cover = ((group.entry.media_id, 40, 56), url.clone());
                if range.contains(&index) {
                    visible.push(cover);
                } else {
                    nearby.push(cover);
                }
            }
        }
        self.replace_cover_region(CoverOwner::Route(Route::Torrents), visible, nearby);
    }

    fn load_torrent_preferences(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() {
            return;
        }
        let new_revision = self.preference_revision("kurisu.torrents.new");
        let statuses_revision = self.preference_revision("kurisu.torrents.statuses");
        let filters_revision = self.preference_revision("kurisu.torrents.filters");
        self.torrents.prefs_request = self.torrents.prefs_request.wrapping_add(1);
        let request = self.torrents.prefs_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            let new_only = worker.get_preference("kurisu.torrents.new".into()).await?;
            let statuses = worker
                .get_preference("kurisu.torrents.statuses".into())
                .await?;
            let filters = worker
                .get_preference("kurisu.torrents.filters".into())
                .await?;
            Ok::<_, String>((new_only, statuses, filters))
        });
        self.replace_reads("torrents-preferences", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.torrent_scope_matches(generation, account)
                    || this.torrents.prefs_request != request
                {
                    return;
                }
                if this.writes_fenced() {
                    this.route_entry_deferred |= this.route == Route::Torrents;
                    return;
                }
                match result {
                    Ok((new_only, statuses, filters)) => {
                        if this.preference_revision("kurisu.torrents.new") == new_revision {
                            this.torrents.new_only = new_only.as_deref() == Some("1");
                        }
                        if this.preference_revision("kurisu.torrents.statuses") == statuses_revision
                            && let Some(statuses) = statuses
                                .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
                        {
                            this.torrents.expanded_statuses = STATUS_ORDER
                                .iter()
                                .copied()
                                .filter(|status| statuses.iter().any(|item| item == status))
                                .collect();
                        }
                        let old_category = this.torrents.category.clone();
                        let old_release = this.torrents.release.clone();
                        if this.preference_revision("kurisu.torrents.filters") == filters_revision
                            && let Some(filters) = filters.and_then(|value| {
                                serde_json::from_str::<serde_json::Value>(&value).ok()
                            })
                        {
                            if let Some(category) =
                                filters.get("category").and_then(|value| value.as_str())
                                && CATEGORY_OPTIONS.iter().any(|(value, _)| *value == category)
                            {
                                this.torrents.category = category.into();
                            }
                            if let Some(release) =
                                filters.get("release").and_then(|value| value.as_str())
                                && RELEASE_OPTIONS.iter().any(|(value, _)| *value == release)
                            {
                                this.torrents.release = release.into();
                            }
                        }
                        this.torrents.prefs_loaded = true;
                        if this.torrents.category != old_category
                            || this.torrents.release != old_release
                        {
                            this.reload_torrent_filter_results(window, cx);
                        } else {
                            this.rebuild_torrent_rows();
                        }
                    }
                    Err(error) => {
                        this.torrents.error = format!("Could not read torrent preferences: {error}")
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn torrent_scope_matches(&self, generation: u64, account: Option<i64>) -> bool {
        !self.stopping
            && self.generation == generation
            && self.profile.as_ref().map(|user| user.id) == account
    }

    pub(super) fn load_torrent_entries(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        if self.writes_fenced() {
            self.torrents.entries_reload_pending = true;
            if self.close_ready {
                self.list_refresh_deferred = true;
            }
            return;
        }
        if self.torrents.entries_loading {
            self.torrents.entries_reload_pending = true;
            return;
        }
        self.torrents.entries_reload_pending = false;
        self.torrents.entries_loading = true;
        self.torrents.entries_error.clear();
        cx.notify();
        self.torrents.entries_request = self.torrents.entries_request.wrapping_add(1);
        let request = self.torrents.entries_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.local_entries().await });
        self.replace_reads("torrents-entries", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.stopping
                    || !this.torrent_scope_matches(generation, account)
                    || this.torrents.entries_request != request
                {
                    return;
                }
                this.torrents.entries_loading = false;
                match result {
                    Ok(entries) => {
                        this.torrents.entries = entries;
                        this.torrents.entries_loaded = true;
                        this.rebuild_torrent_rows();
                        this.reload_expanded_torrent_choices(window, cx);
                    }
                    Err(error) => this.torrents.entries_error = error,
                }
                if this.torrents.entries_reload_pending {
                    if this.writes_fenced() {
                        if this.close_ready {
                            this.list_refresh_deferred = true;
                        }
                    } else {
                        this.torrents.entries_reload_pending = false;
                        this.load_torrent_entries(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_torrent_feeds(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.torrents.feeds_loading {
            return;
        }
        self.torrents.feeds_loading = true;
        self.torrents.feeds_error.clear();
        cx.notify();
        self.torrents.feeds_request = self.torrents.feeds_request.wrapping_add(1);
        let request = self.torrents.feeds_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.get_rss_feeds().await });
        self.replace_reads("torrents-feeds", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if !this.torrent_scope_matches(generation, account)
                    || this.torrents.feeds_request != request
                {
                    return;
                }
                this.torrents.feeds_loading = false;
                match result {
                    Ok(feeds) => {
                        this.torrents.feeds = feeds;
                        this.torrents.feeds_loaded = true;
                    }
                    Err(error) => this.torrents.feeds_error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_torrent_feed_loaded(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.torrents.loaded {
            self.refresh_torrent_feed(false, window, cx);
        }
    }

    fn refresh_torrent_feed(&mut self, flash: bool, window: &Window, cx: &mut Context<Self>) {
        if self.stopping || self.writes_fenced() || self.torrents.loading {
            return;
        }
        self.torrents.loading = true;
        self.torrents.feed_error.clear();
        self.torrents.refreshed = None;
        cx.notify();
        self.torrents.request = self.torrents.request.wrapping_add(1);
        let request = self.torrents.request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move { worker.fetch_torrents().await });
        self.replace_reads("torrents-feed-items", vec![job.abort_handle()]);
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.torrent_scope_matches(generation, account)
                    || this.torrents.request != request
                {
                    return;
                }
                this.torrents.loading = false;
                match result {
                    Ok(mut result) => {
                        this.torrents.apply_opened_releases(&mut result.items);
                        this.torrents.items = result.items;
                        this.torrents.failures = result.failures;
                        this.torrents.loaded = true;
                        for choice in this.torrents.choices.values_mut() {
                            if let Some(abort) = choice.abort.take() {
                                abort.abort();
                            }
                        }
                        this.torrents.choices.clear();
                        if flash {
                            this.torrents.refreshed = Some(Instant::now());
                        }
                        this.rebuild_torrent_rows();
                        this.reload_expanded_torrent_choices(window, cx);
                    }
                    Err(error) => this.torrents.feed_error = error,
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn release_identity(item: &TorrentItem) -> &str {
    item.seen_guid.as_deref().unwrap_or(&item.guid)
}

fn release_row_key(item: &TorrentItem) -> String {
    let id = release_identity(item);
    if id == item.guid {
        item.guid.clone()
    } else {
        format!("{}\u{1}{id}", item.guid)
    }
}

#[derive(Default)]
struct FeedReleases<'a> {
    ids: HashSet<(Option<i64>, &'a str)>,
    links: HashSet<(Option<i64>, &'a str)>,
}

impl<'a> FeedReleases<'a> {
    fn insert(&mut self, group: Option<i64>, item: &'a TorrentItem) -> bool {
        let id = (group, release_identity(item));
        let link = (group, item.link.as_str());
        // A download URL can be reused by torrents with different hashes.
        if self.ids.contains(&id) || (item.magnet.is_none() && self.links.contains(&link)) {
            return false;
        }
        self.ids.insert(id);
        if !item.link.is_empty() {
            self.links.insert(link);
        }
        true
    }
}

fn matches_filters(item: &TorrentItem, category: &str, release: &str) -> bool {
    (category == "1_0" || item.category_id.as_deref() == Some(category))
        && (release == "0"
            || (release == "1" && item.remake == Some(false))
            || (release == "2" && item.trusted == Some(true)))
}

fn is_new_for(item: &TorrentItem, entry: &ListEntry) -> bool {
    !item.seen
        && item.episode.is_some_and(|episode| {
            episode > entry.progress
                && entry
                    .media
                    .as_ref()
                    .and_then(|media| media.known_episodes())
                    .is_none_or(|total| episode <= total)
        })
}

fn display_groups(
    entries: &[ListEntry],
    items: &[TorrentItem],
    category: &str,
    release: &str,
    new_only: bool,
) -> Vec<ShowGroup> {
    let mut by_id: HashMap<i64, ShowGroup> = HashMap::new();
    let mut releases = FeedReleases::default();
    for entry in entries {
        by_id.insert(
            entry.media_id,
            ShowGroup {
                entry: entry.clone(),
                title: entry.media.as_ref().map_or_else(
                    || format!("#{}", entry.media_id),
                    |media| media.display_title(),
                ),
                cover: entry
                    .media
                    .as_ref()
                    .and_then(|media| media.cover_medium.clone()),
                items: Vec::new(),
                has_new: false,
                newest: 0,
            },
        );
    }
    for item in items {
        if !matches_filters(item, category, release) {
            continue;
        }
        let Some(group) = item.media_id.and_then(|id| by_id.get_mut(&id)) else {
            continue;
        };
        let is_new = is_new_for(item, &group.entry);
        if (new_only && !is_new) || !releases.insert(Some(group.entry.media_id), item) {
            continue;
        }
        let mut item = item.clone();
        item.is_new = is_new;
        group.has_new |= is_new;
        group.newest = group.newest.max(item.published.unwrap_or(0));
        group.items.push(item);
    }
    let mut groups: Vec<_> = by_id.into_values().collect();
    groups.sort_by(|a, b| {
        b.has_new
            .cmp(&a.has_new)
            .then_with(|| b.newest.cmp(&a.newest))
            .then_with(|| a.title.cmp(&b.title))
    });
    groups
}

fn choice_key(entry: &ListEntry, category: &str, release: &str) -> String {
    serde_json::to_string(&(
        entry.media_id,
        entry.progress,
        &entry.status,
        &entry.media,
        category,
        release,
    ))
    .unwrap_or_else(|_| format!("{}:{}:{category}:{release}", entry.media_id, entry.progress))
}

impl Workspace {
    fn rebuild_torrent_rows(&mut self) {
        let groups = display_groups(
            &self.torrents.entries,
            &self.torrents.items,
            &self.torrents.category,
            &self.torrents.release,
            self.torrents.new_only,
        )
        .into_iter()
        .map(Arc::new)
        .collect::<Vec<_>>();
        let mut rows = Vec::new();
        if self.torrents.searching || self.torrents.searched {
            rows.push(TorrentDisplayRow::SearchHeading);
            if self.torrents.searched {
                for item in &self.torrents.results {
                    if matches_filters(item, &self.torrents.category, &self.torrents.release) {
                        rows.push(TorrentDisplayRow::SearchRelease(Arc::new(item.clone())));
                    }
                }
            }
        }
        if self.torrents.entries_loaded {
            for status in STATUS_ORDER {
                let all: Vec<_> = groups
                    .iter()
                    .filter(|group| group.entry.status == status)
                    .collect();
                let total = all.len();
                rows.push(TorrentDisplayRow::Category {
                    status,
                    total,
                    releases: all.iter().map(|group| group.items.len()).sum(),
                    new_count: all
                        .iter()
                        .map(|group| group.items.iter().filter(|item| item.is_new).count())
                        .sum(),
                });
                if !self.torrents.expanded_statuses.contains(status) {
                    continue;
                }
                let needle = self
                    .torrents
                    .show_filters
                    .get(status)
                    .map_or("", String::as_str)
                    .trim()
                    .to_lowercase();
                let shown: Vec<_> = all
                    .into_iter()
                    .filter(|group| {
                        needle.is_empty()
                            || group.title.to_lowercase().contains(&needle)
                            || group.entry.media.as_ref().is_some_and(|media| {
                                [
                                    &media.title_romaji,
                                    &media.title_native,
                                    &media.title_english,
                                ]
                                .into_iter()
                                .flatten()
                                .any(|title| title.to_lowercase().contains(&needle))
                            })
                            || group
                                .items
                                .iter()
                                .any(|item| item.title.to_lowercase().contains(&needle))
                    })
                    .collect();
                rows.push(TorrentDisplayRow::Filter {
                    status,
                    shown: shown.len(),
                    total,
                });
                for group in shown {
                    rows.push(TorrentDisplayRow::Show(Arc::clone(group)));
                    if self.torrents.expanded_shows.contains(&group.entry.media_id) {
                        append_choice_rows(
                            &mut rows,
                            group,
                            self.torrents.choices.get(&group.entry.media_id),
                            &self.torrents.opened_identities,
                        );
                    }
                }
            }
            let ids: HashSet<_> = self
                .torrents
                .entries
                .iter()
                .map(|entry| entry.media_id)
                .collect();
            let mut releases = FeedReleases::default();
            let unmatched = self
                .torrents
                .items
                .iter()
                .filter(|item| item.media_id.is_none_or(|id| !ids.contains(&id)))
                .filter(|item| releases.insert(None, item))
                .count();
            if unmatched > 0 {
                rows.push(TorrentDisplayRow::Unmatched(unmatched));
            }
        }
        if let Some(widget) = &self.torrents.widget {
            replace_list_rows(
                widget,
                &self.torrents.rows,
                &rows,
                px(78.),
                TorrentDisplayRow::key,
            );
        }
        self.torrents.rows = Arc::new(rows);
        self.refresh_torrents_covers();
    }

    fn search_torrents(&mut self, query: Option<String>, window: &Window, cx: &mut Context<Self>) {
        let query = query
            .unwrap_or_else(|| {
                self.torrents
                    .search_input
                    .as_ref()
                    .map_or(String::new(), |input| input.read(cx).value().to_owned())
            })
            .trim()
            .chars()
            .take(200)
            .collect::<String>();
        if self.stopping
            || self.writes_fenced()
            || query.is_empty()
            || self.profile.is_none()
            || self.torrents.searching
        {
            return;
        }
        self.torrents.submitted_query = query.clone();
        self.torrents.searching = true;
        self.torrents.searched = false;
        self.torrents.results.clear();
        self.torrents.error.clear();
        self.torrents.search_request = self.torrents.search_request.wrapping_add(1);
        let request = self.torrents.search_request;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let category = self.torrents.category.clone();
        let release = self.torrents.release.clone();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            worker
                .search_torrents(query, Some(category), Some(release))
                .await
        });
        self.replace_reads("torrents-search", vec![job.abort_handle()]);
        self.rebuild_torrent_rows();
        if let Some(widget) = &self.torrents.widget {
            widget.scroll_to(ListOffset::default());
        }
        self.refresh_torrents_covers();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if !this.torrent_scope_matches(generation, account)
                    || this.torrents.search_request != request
                {
                    return;
                }
                this.torrents.searching = false;
                match result {
                    Ok(mut results) => {
                        this.torrents.apply_opened_releases(&mut results);
                        this.torrents.results = results;
                        this.torrents.searched = true;
                    }
                    Err(error) => this.torrents.error = error,
                }
                this.rebuild_torrent_rows();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn change_torrent_filters(
        &mut self,
        category: Option<String>,
        release: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits_fenced() {
            return;
        }
        if let Some(category) = category {
            self.torrents.category = category;
        }
        if let Some(release) = release {
            self.torrents.release = release;
        }
        self.torrents.category_menu = false;
        self.torrents.release_menu = false;
        let value =
            serde_json::json!({"category":self.torrents.category,"release":self.torrents.release})
                .to_string();
        self.persist_preference("kurisu.torrents.filters", Some(value), window, cx);
        self.reload_torrent_filter_results(window, cx);
        cx.notify();
    }

    fn reload_torrent_filter_results(&mut self, window: &Window, cx: &mut Context<Self>) {
        for choice in self.torrents.choices.values_mut() {
            if let Some(abort) = choice.abort.take() {
                abort.abort();
            }
        }
        self.torrents.choices.clear();
        if !self.torrents.submitted_query.is_empty() {
            self.torrents.searching = false;
            self.search_torrents(Some(self.torrents.submitted_query.clone()), window, cx);
        } else {
            self.rebuild_torrent_rows();
        }
        self.reload_expanded_torrent_choices(window, cx);
    }

    pub(super) fn reload_expanded_torrent_choices(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.stopping || self.writes_fenced() {
            return;
        }
        let expanded: Vec<_> = self.torrents.expanded_shows.iter().copied().collect();
        for id in expanded {
            self.load_show_choices(id, false, window, cx);
        }
    }

    fn load_show_choices(
        &mut self,
        media_id: i64,
        force: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.stopping || self.writes_fenced() {
            return;
        }
        let Some(entry) = self
            .torrents
            .entries
            .iter()
            .find(|entry| entry.media_id == media_id)
            .cloned()
        else {
            return;
        };
        let key = choice_key(&entry, &self.torrents.category, &self.torrents.release);
        self.torrents.choice_request = self.torrents.choice_request.wrapping_add(1);
        let request = self.torrents.choice_request;
        let state = self
            .torrents
            .choices
            .entry(media_id)
            .or_insert_with(|| ChoiceState::new(key.clone()));
        if state.key != key {
            if let Some(abort) = state.abort.take() {
                abort.abort();
            }
            *state = ChoiceState::new(key.clone());
        }
        if !force
            && state
                .fetched_at
                .is_some_and(|time| time.elapsed() < Duration::from_secs(300))
        {
            return;
        }
        if state.loading {
            return;
        }
        state.request = request;
        state.loading = true;
        state.error.clear();
        state.choices = None;
        state.fetched_at = None;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let category = self.torrents.category.clone();
        let release = self.torrents.release.clone();
        let core = self.core.clone();
        let worker = core.clone();
        let job = core.spawn(async move {
            worker
                .find_show_torrents(media_id, Some(category), Some(release))
                .await
        });
        state.abort = Some(job.abort_handle());
        self.rebuild_torrent_rows();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(job.await);
            let _ = this.update_in(cx, |this, _, cx| {
                if !this.torrent_scope_matches(generation, account) {
                    return;
                }
                let Some(state) = this.torrents.choices.get_mut(&media_id) else {
                    return;
                };
                if state.key != key || state.request != request {
                    return;
                }
                state.loading = false;
                state.abort = None;
                match result {
                    Ok(choices) => {
                        state.choices = Some(choices);
                        state.fetched_at = Some(Instant::now());
                    }
                    Err(error) => state.error = error,
                }
                this.prune_show_choices();
                this.rebuild_torrent_rows();
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_show_choices(&mut self, media_id: i64, window: &Window, cx: &mut Context<Self>) {
        if !self.torrents.expanded_shows.insert(media_id) {
            self.torrents.expanded_shows.remove(&media_id);
            self.prune_show_choices();
        } else {
            self.load_show_choices(media_id, false, window, cx);
        }
        self.rebuild_torrent_rows();
        cx.notify();
    }

    fn prune_show_choices(&mut self) {
        let expanded = &self.torrents.expanded_shows;
        self.torrents.choices.retain(|id, state| {
            expanded.contains(id) || state.loading || state.fetched_at.is_some()
        });
        while self.torrents.choices.len() > 50 {
            let oldest = self
                .torrents
                .choices
                .iter()
                .filter(|(id, state)| !expanded.contains(id) && !state.loading)
                .filter_map(|(id, state)| state.fetched_at.map(|at| (*id, at)))
                .min_by_key(|(_, at)| *at)
                .map(|(id, _)| id);
            let Some(oldest) = oldest else {
                break;
            };
            self.torrents.choices.remove(&oldest);
        }
    }
}

fn append_choice_rows(
    rows: &mut Vec<TorrentDisplayRow>,
    group: &ShowGroup,
    state: Option<&ChoiceState>,
    opened_ids: &HashSet<String>,
) {
    let id = group.entry.media_id;
    let Some(state) = state else {
        rows.push(TorrentDisplayRow::ChoiceStatus {
            media_id: id,
            text: "Finding batches and episode options…".into(),
            retry: false,
        });
        return;
    };
    if state.loading {
        rows.push(TorrentDisplayRow::ChoiceStatus {
            media_id: id,
            text: "Finding batches and episode options…".into(),
            retry: false,
        });
        return;
    }
    if !state.error.is_empty() {
        rows.push(TorrentDisplayRow::ChoiceStatus {
            media_id: id,
            text: format!("Could not find download options: {}", state.error),
            retry: true,
        });
    }
    let mut found_links = HashSet::new();
    let mut found_ids = HashSet::new();
    if let Some(choices) = &state.choices {
        let count = choices.batches.len() + choices.episodes.len() + choices.other.len();
        rows.push(TorrentDisplayRow::ChoiceStatus {
            media_id: id,
            text: format!(
                "{count} download option{} · Choose a release group below",
                if count == 1 { "" } else { "s" }
            ),
            retry: true,
        });
        for warning in &choices.warnings {
            rows.push(TorrentDisplayRow::ChoiceStatus {
                media_id: id,
                text: warning.clone(),
                retry: false,
            });
        }
        let episode_title = choices.next_episode.map_or_else(
            || "Episodes".to_owned(),
            |episode| format!("Next episode · {episode}"),
        );
        for (title, items) in [
            ("Batches".to_owned(), &choices.batches),
            (episode_title, &choices.episodes),
            ("Other releases".to_owned(), &choices.other),
        ] {
            if items.is_empty() {
                continue;
            }
            rows.push(TorrentDisplayRow::ChoiceSection {
                media_id: id,
                title: title.clone(),
                count: items.len(),
            });
            let expanded = state.expanded_sections.contains(&title);
            for item in items.iter().take(if expanded { usize::MAX } else { 6 }) {
                let mut item = item.clone();
                item.seen |= opened_ids.contains(release_identity(&item));
                rows.push(TorrentDisplayRow::ChoiceRelease {
                    media_id: id,
                    item: Arc::new(item),
                });
            }
            for item in items {
                found_links.insert(item.link.clone());
                found_ids.insert(release_identity(item));
            }
            if items.len() > 6 {
                rows.push(TorrentDisplayRow::ChoiceMore {
                    media_id: id,
                    title,
                    count: items.len(),
                    expanded,
                });
            }
        }
        if choices.batches.is_empty() && choices.episodes.is_empty() {
            rows.push(TorrentDisplayRow::ChoiceStatus {
                media_id: id,
                text: choices.next_episode.map_or_else(
                    || "No batches found with these filters. Your progress is already at the known episode total.".into(),
                    |episode| format!("No batches or episode {episode} releases found with these filters.")
                ),
                retry: false,
            });
        }
    }
    let remaining: Vec<_> = group
        .items
        .iter()
        .filter(|item| {
            (item.magnet.is_some() || !found_links.contains(&item.link))
                && !found_ids.contains(release_identity(item))
        })
        .collect();
    if !remaining.is_empty() {
        rows.push(TorrentDisplayRow::ChoiceSection {
            media_id: id,
            title: "Recent feed releases".into(),
            count: remaining.len(),
        });
        for item in remaining {
            rows.push(TorrentDisplayRow::ChoiceRelease {
                media_id: id,
                item: Arc::new(item.clone()),
            });
        }
    }
}

impl Workspace {
    fn accept_written_torrent_feeds(&mut self, feeds: Vec<String>) {
        self.torrents.feeds_request = self.torrents.feeds_request.wrapping_add(1);
        self.torrents.feeds_loading = false;
        self.cancel_reads("torrents-feeds");
        let enabled: HashSet<_> = feeds.iter().map(String::as_str).collect();
        let previous_count = self.torrents.items.len();
        self.torrents.items.retain(|item| {
            item.guid
                .split_once('\u{1}')
                .is_some_and(|(feed, _)| enabled.contains(feed))
        });
        self.torrents
            .failures
            .retain(|failure| enabled.contains(failure.url.as_str()));
        self.torrents.feeds = feeds;
        self.torrents.feeds_loaded = true;
        self.torrents.feeds_error.clear();
        if self.torrents.items.len() != previous_count {
            self.rebuild_torrent_rows();
        }
    }

    fn restart_torrent_feed(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.torrents.loaded = false;
        if self.torrents.loading {
            self.torrents.request = self.torrents.request.wrapping_add(1);
            self.cancel_reads("torrents-feed-items");
            self.torrents.loading = false;
        }
        self.refresh_torrent_feed(false, window, cx);
    }

    fn add_torrent_feed(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.torrent_feed_controls_disabled() {
            return;
        }
        let Some(input) = &self.torrents.feed_input else {
            return;
        };
        let url = input.read(cx).value().trim().to_owned();
        if url.is_empty() {
            return;
        }
        self.torrents.adding_feed = true;
        self.torrents.error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move { worker.add_rss_feed(url).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                let mut refreshed = false;
                if this.torrent_scope_matches(generation, account) {
                    this.torrents.adding_feed = false;
                    match result {
                        Ok(feeds) => {
                            this.accept_written_torrent_feeds(feeds);
                            if let Some(input) = &this.torrents.feed_input {
                                input.update(cx, |input, cx| input.set_value("", cx));
                            }
                            refreshed = true;
                        }
                        Err(error) => this.torrents.error = error,
                    }
                }
                this.check_barrier(window, cx);
                if refreshed {
                    this.restart_torrent_feed(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn torrent_remove_open(&self) -> bool {
        self.route == Route::Torrents && self.torrents.removing_feed.is_some()
    }

    fn torrent_feed_controls_disabled(&self) -> bool {
        self.edits_fenced()
            || self.modal_open()
            || self.profile.is_none()
            || self.torrents.adding_feed
            || self.torrents.removing
    }

    fn ask_remove_torrent_feed(&mut self, target: &str, window: &Window, cx: &mut Context<Self>) {
        if self.torrent_feed_controls_disabled()
            || self.route != Route::Torrents
            || !self.torrents.feeds.iter().any(|feed| feed == target)
        {
            return;
        }
        self.torrents.removing_feed = Some(target.into());
        cx.notify();
        cx.defer_in(window, |this, window, cx| {
            if this.active_modal() == Some(super::ModalLayer::TorrentRemove)
                && !this.modal_focus.contains(window, cx)
            {
                this.modal_focus.hold(window, cx);
            }
        });
    }

    fn can_remove_torrent_feed(&self) -> bool {
        !self.edits_fenced()
            && self.profile.is_some()
            && self.active_modal() == Some(super::ModalLayer::TorrentRemove)
            && !self.torrents.adding_feed
            && !self.torrents.removing
    }

    fn remove_torrent_feed(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.can_remove_torrent_feed() {
            return;
        }
        let Some(url) = self.torrents.removing_feed.clone() else {
            return;
        };
        self.torrents.removing = true;
        self.torrents.error.clear();
        self.active_writes += 1;
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move { worker.remove_rss_feed(url).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                let mut refreshed = false;
                if this.torrent_scope_matches(generation, account) {
                    this.torrents.removing = false;
                    this.torrents.removing_feed = None;
                    match result {
                        Ok(feeds) => {
                            this.accept_written_torrent_feeds(feeds);
                            refreshed = true;
                        }
                        Err(error) => this.torrents.error = error,
                    }
                }
                this.check_barrier(window, cx);
                if refreshed {
                    this.restart_torrent_feed(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_torrent_url(
        &mut self,
        item: Option<TorrentItem>,
        url: String,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.stopping || self.edits_fenced() || self.modal_open() {
            return;
        }
        let counted = item.is_some();
        if counted {
            self.active_writes += 1;
        }
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        #[cfg(test)]
        let controlled = self.torrent_open_result.take();
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move {
                    #[cfg(test)]
                    if let Some(controlled) = controlled {
                        return controlled.await.map_err(|error| error.to_string())?;
                    }
                    tokio::task::spawn_blocking(move || NativePlatform.open_url(&url))
                        .await
                        .map_err(|error| error.to_string())?
                })
                .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                if counted {
                    this.active_writes = this.active_writes.saturating_sub(1);
                }
                if this.stopping {
                    return;
                }
                if !this.torrent_scope_matches(generation, account) {
                    this.check_barrier(window, cx);
                    return;
                }
                match result {
                    Ok(()) => {
                        if let Some(item) = item {
                            this.mark_torrent_item_seen(item, window, cx);
                        }
                    }
                    Err(error) if !this.close_ready => {
                        this.torrents.error = format!("Could not open link: {error}");
                    }
                    Err(_) => {}
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn mark_torrent_item_seen(
        &mut self,
        item: TorrentItem,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let id = release_identity(&item).to_owned();
        self.torrents.opened_identities.insert(id.clone());
        let choices = self
            .torrents
            .choices
            .values_mut()
            .filter_map(|choice| choice.choices.as_mut())
            .flat_map(|found| {
                found
                    .batches
                    .iter_mut()
                    .chain(found.episodes.iter_mut())
                    .chain(found.other.iter_mut())
            });
        for other in self
            .torrents
            .items
            .iter_mut()
            .chain(self.torrents.results.iter_mut())
            .chain(choices)
        {
            if release_identity(other) == id {
                other.seen = true;
                other.is_new = false;
            }
        }
        self.rebuild_torrent_rows();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        let core = self.core.clone();
        let worker = core.clone();
        self.active_writes += 1;
        cx.spawn_in(window, async move |this, cx| {
            let result = joined(
                core.spawn(async move { worker.mark_torrents_seen(vec![id]).await })
                    .await,
            );
            let _ = this.update_in(cx, |this, window, cx| {
                this.active_writes = this.active_writes.saturating_sub(1);
                if this.torrent_scope_matches(generation, account)
                    && let Err(error) = result
                {
                    this.torrents.error = format!("Could not save seen releases: {error}");
                }
                this.check_barrier(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn toggle_torrent_status(
        &mut self,
        status: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits_fenced() || !self.torrents.prefs_loaded {
            return;
        }
        if !self.torrents.expanded_statuses.insert(status) {
            self.torrents.expanded_statuses.remove(status);
        }
        let value = serde_json::to_string(
            &STATUS_ORDER
                .into_iter()
                .filter(|status| self.torrents.expanded_statuses.contains(status))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        self.persist_preference("kurisu.torrents.statuses", Some(value), window, cx);
        self.rebuild_torrent_rows();
        cx.notify();
    }

    fn toggle_torrent_section(&mut self, media_id: i64, title: String, cx: &mut Context<Self>) {
        if let Some(state) = self.torrents.choices.get_mut(&media_id) {
            if !state.expanded_sections.insert(title.clone()) {
                state.expanded_sections.remove(&title);
            }
            self.rebuild_torrent_rows();
            cx.notify();
        }
    }
}

impl TorrentsState {
    pub(super) fn dismiss_menu(&mut self) -> bool {
        let open = self.category_menu || self.release_menu;
        self.category_menu = false;
        self.release_menu = false;
        open
    }
}

impl Workspace {
    pub(super) fn render_torrents(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let feed_controls_disabled = self.torrent_feed_controls_disabled();
        let reads_fenced = self.stopping || self.writes_fenced();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        if self.modal_open() || self.edits_fenced() {
            self.torrents.dismiss_menu();
        }
        let state = &self.torrents;
        let visible_new = state
            .rows
            .iter()
            .filter_map(|row| match row {
                TorrentDisplayRow::Category {
                    status, new_count, ..
                } if state.expanded_statuses.contains(status) => Some(*new_count),
                _ => None,
            })
            .sum::<usize>();
        let mut page = div()
            .id("torrents-page")
            .debug_selector(|| "torrents-page".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .p(px(28.))
            .on_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.torrents.dismiss_menu() {
                    cx.notify();
                }
            }))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                if this.torrents.dismiss_menu() {
                    cx.notify();
                }
            }));
        page = page.child(
            div()
                .min_h(px(92.))
                .mb(px(18.))
                .pb(px(12.))
                .border_b_1()
                .border_color(rgb(EDGE))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(ACCENT))
                        .child("RELEASES"),
                )
                .child(page_heading("Torrents"))
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .child("Downloads for your list and new releases"),
                ),
        );
        let mut toolbar = div()
            .mb(px(12.))
            .pt(px(16.))
            .pb(px(12.))
            .border_b_1()
            .border_color(rgb(EDGE))
            .flex()
            .flex_col()
            .gap(px(16.));
        let search = state.search_input.clone();
        let feed = state.feed_input.clone();
        let mut forms = div().flex().flex_wrap().gap(px(14.));
        if let Some(search) = search {
            forms = forms.child(
                div()
                    .min_w(px(250.))
                    .flex_1()
                    .child(
                        div()
                            .id("torrent-search-heading")
                            .role(Role::Heading)
                            .aria_level(2)
                            .aria_label("Search")
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .mb(px(8.))
                            .child(div().font(serif_font()).text_size(px(20.)).child("Search"))
                            .child(div().flex_1().h(px(1.)).bg(rgb(EDGE))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .border_1()
                                    .border_color(rgb(EDGE))
                                    .bg(rgb(PANEL))
                                    .p(px(5.))
                                    .child(search),
                            )
                            .child(
                                button(
                                    "torrent-search",
                                    if state.searching {
                                        "Searching…"
                                    } else {
                                        "Search"
                                    },
                                    state.searching || reads_fenced,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.search_torrents(None, window, cx),
                                )),
                            ),
                    ),
            );
        }
        if let Some(feed) = feed {
            forms = forms.child(
                div()
                    .min_w(px(250.))
                    .flex_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .mb(px(8.))
                            .child(
                                div()
                                    .id("torrent-feeds-heading")
                                    .role(Role::Heading)
                                    .aria_level(2)
                                    .aria_label("Feeds")
                                    .child(
                                        div()
                                            .id("torrent-feed-toggle")
                                            .role(Role::Button)
                                            .aria_label("Feeds")
                                            .aria_expanded(state.show_feeds)
                                            .tab_stop(true)
                                            .focusable()
                                            .focus_visible(|style| {
                                                style.border_1().border_color(rgb(ACCENT))
                                            })
                                            .cursor_pointer()
                                            .font(serif_font())
                                            .text_size(px(20.))
                                            .child(if state.show_feeds {
                                                "⌄ Feeds"
                                            } else {
                                                "› Feeds"
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.torrents.show_feeds =
                                                    !this.torrents.show_feeds;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(div().flex_1().h(px(1.)).bg(rgb(EDGE))),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .border_1()
                                    .border_color(rgb(EDGE))
                                    .bg(rgb(PANEL))
                                    .p(px(5.))
                                    .child(feed),
                            )
                            .child(
                                button(
                                    "torrent-feed-add",
                                    if state.adding_feed {
                                        "Adding…"
                                    } else {
                                        "+ Add feed"
                                    },
                                    feed_controls_disabled,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.add_torrent_feed(window, cx),
                                )),
                            ),
                    )
                    .when(
                        state.feeds_loaded
                            && state.feeds.is_empty()
                            && state.feeds_error.is_empty(),
                        |form| {
                            form.child(
                                div()
                                    .mt(px(8.))
                                    .text_color(rgb(DIM))
                                    .child("No feeds configured. Add an RSS feed URL above."),
                            )
                        },
                    ),
            );
        }
        toolbar = toolbar.child(forms);
        if state.show_feeds {
            let mut feeds = div()
                .id("torrent-feeds")
                .role(Role::Region)
                .aria_label("Saved feeds")
                .max_h(px(128.))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(4.));
            if !state.feeds_error.is_empty() {
                feeds = feeds.child(
                    div()
                        .text_color(rgb(ERROR))
                        .child(format!("Could not read saved feeds: {}", state.feeds_error)),
                );
                feeds = feeds.child(
                    button(
                        "torrent-retry-feeds",
                        "Retry feeds",
                        state.feeds_loading || reads_fenced,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.torrents.feeds_loading {
                            this.load_torrent_feeds(window, cx);
                        }
                    })),
                );
            }
            for (index, url) in state.feeds.iter().enumerate() {
                let target = url.clone();
                feeds = feeds.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .bg(rgb(PANEL))
                        .child(div().flex_1().min_w_0().truncate().child(url.clone()))
                        .child(
                            button(
                                format!("torrent-remove-feed-{index}"),
                                "Remove",
                                feed_controls_disabled,
                            )
                            .aria_label(format!("Remove feed {url}"))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.torrent_scope_matches(generation, account) {
                                        this.ask_remove_torrent_feed(&target, window, cx);
                                    }
                                },
                            )),
                        ),
                );
            }
            toolbar = toolbar.child(feeds);
        }
        let category_label = CATEGORY_OPTIONS
            .iter()
            .find(|(value, _)| *value == state.category)
            .map_or("All languages", |(_, label)| *label);
        let release_label = RELEASE_OPTIONS
            .iter()
            .find(|(value, _)| *value == state.release)
            .map_or("All releases", |(_, label)| *label);
        let filters_disabled = !state.prefs_loaded || self.edits_fenced();
        let mut category = div()
            .relative()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .min_w(px(210.))
            .max_w(px(320.))
            .flex_1()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(DIM))
                    .child("Language / category"),
            )
            .child(
                button("torrent-category", category_label, filters_disabled)
                    .w_full()
                    .text_left()
                    .role(Role::ComboBox)
                    .aria_expanded(state.category_menu && !filters_disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.torrents.prefs_loaded && !this.edits_fenced() {
                            this.torrents.category_menu = !this.torrents.category_menu;
                            this.torrents.release_menu = false;
                            cx.notify();
                        }
                    })),
            );
        if state.category_menu && !filters_disabled {
            let mut menu = div()
                .id("torrent-category-menu")
                .role(Role::Menu)
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .w(px(260.))
                .max_h(px(320.))
                .overflow_y_scroll()
                .occlude()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .flex()
                .flex_col()
                .gap(px(4.));
            for (value, label) in CATEGORY_OPTIONS {
                menu = menu.child(
                    button(format!("torrent-category-{value}"), label, false)
                        .role(Role::MenuItem)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.change_torrent_filters(Some(value.into()), None, window, cx)
                        })),
                );
            }
            category = category.child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(56.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(menu),
                )
                .priority(1),
            );
        }
        let mut release = div()
            .relative()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .min_w(px(150.))
            .max_w(px(320.))
            .flex_1()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(DIM))
                    .child("Release filter"),
            )
            .child(
                button("torrent-release", release_label, filters_disabled)
                    .w_full()
                    .text_left()
                    .role(Role::ComboBox)
                    .aria_expanded(state.release_menu && !filters_disabled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.torrents.prefs_loaded && !this.edits_fenced() {
                            this.torrents.release_menu = !this.torrents.release_menu;
                            this.torrents.category_menu = false;
                            cx.notify();
                        }
                    })),
            );
        if state.release_menu && !filters_disabled {
            let mut menu = div()
                .id("torrent-release-menu")
                .role(Role::Menu)
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .w(px(200.))
                .max_h(px(320.))
                .overflow_y_scroll()
                .occlude()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .flex()
                .flex_col()
                .gap(px(4.));
            for (value, label) in RELEASE_OPTIONS {
                menu = menu.child(
                    button(format!("torrent-release-{value}"), label, false)
                        .debug_selector(move || format!("torrent-release-{value}"))
                        .role(Role::MenuItem)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.change_torrent_filters(None, Some(value.into()), window, cx)
                        })),
                );
            }
            release = release.child(
                deferred(
                    anchored()
                        .anchor(Anchor::TopLeft)
                        .offset(point(px(0.), px(56.)))
                        .snap_to_window_with_margin(px(8.))
                        .child(menu),
                )
                .priority(1),
            );
        }
        let mut actions = div().flex().flex_wrap().items_center().gap(px(8.));
        if visible_new > 0 {
            actions = actions.child(
                div()
                    .text_color(rgb(ACCENT))
                    .child(format!("{visible_new} new")),
            );
        }
        let mut new_only = div()
            .id("torrent-new-only")
            .role(Role::CheckBox)
            .aria_label("New feed releases only")
            .aria_toggled(Toggled::from(state.new_only))
            .tab_stop(!filters_disabled)
            .flex()
            .items_center()
            .gap(px(6.))
            .text_color(rgb(DIM))
            .child(
                div()
                    .w(px(13.))
                    .h(px(13.))
                    .flex_shrink_0()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(rgb(if state.new_only { ACCENT } else { EDGE }))
                    .bg(rgb(if state.new_only { ACCENT } else { PANEL_2 }))
                    .when(state.new_only, |box_| {
                        box_.child(div().text_size(px(11.)).text_color(rgb(BASE)).child("✓"))
                    }),
            )
            .child("New feed releases only")
            .on_click(cx.listener(|this, _, window, cx| {
                if this.edits_fenced() || !this.torrents.prefs_loaded {
                    return;
                }
                this.torrents.new_only = !this.torrents.new_only;
                this.persist_preference(
                    "kurisu.torrents.new",
                    Some(if this.torrents.new_only { "1" } else { "0" }.into()),
                    window,
                    cx,
                );
                this.rebuild_torrent_rows();
                cx.notify();
            }));
        if filters_disabled {
            new_only = new_only
                .aria_description("Unavailable")
                .a11y_synthetic_children(|builder| {
                    crate::components::disable_control_node(builder.parent_node())
                })
                .capture_any_mouse_down(|_, _, cx| cx.stop_propagation());
        } else {
            new_only = new_only
                .focusable()
                .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
                .cursor_pointer();
        }
        actions = actions.child(new_only).child(
            button(
                "torrent-refresh",
                if state.loading {
                    "Refreshing…"
                } else {
                    "↻ Refresh"
                },
                state.loading || reads_fenced,
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if !this.torrents.loading && !this.stopping && !this.writes_fenced() {
                    this.load_torrent_feeds(window, cx);
                    this.load_torrent_entries(window, cx);
                    this.refresh_torrent_feed(true, window, cx);
                }
            })),
        );
        if state
            .refreshed
            .is_some_and(|time| time.elapsed() < Duration::from_secs(3))
        {
            actions = actions.child(div().text_color(rgb(ACCENT)).child("Refreshed ✓"));
        }
        toolbar = toolbar.child(
            div()
                .flex()
                .flex_wrap()
                .items_end()
                .gap(px(10.))
                .child(category)
                .child(release)
                .child(actions),
        );
        page = page.child(toolbar);
        let mut tips = div()
            .mb(px(12.))
            .text_size(px(12.))
            .text_color(rgb(DIM))
            .child(
                div()
                    .id("torrent-tips")
                    .role(Role::Button)
                    .aria_label("Search tips and torrent labels")
                    .aria_expanded(state.show_tips)
                    .tab_stop(true)
                    .focusable()
                    .focus_visible(|style| style.border_1().border_color(rgb(ACCENT)))
                    .cursor_pointer()
                    .self_start()
                    .child(if state.show_tips {
                        "⌄ Search tips and torrent labels"
                    } else {
                        "› Search tips and torrent labels"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.torrents.show_tips = !this.torrents.show_tips;
                        cx.notify();
                    })),
            );
        if state.show_tips {
            tips = tips.child(div().mt(px(8.)).child(
                "Filters apply to feed releases and Nyaa searches. Trusted marks trusted uploaders.",
            ));
        }
        page = page.child(tips);
        for (id, error) in [
            ("torrent-error", &state.error),
            ("torrent-error-feed", &state.feed_error),
            ("torrent-error-entries", &state.entries_error),
            ("torrent-error-feeds", &state.feeds_error),
        ] {
            if !error.is_empty() {
                page = page.child(
                    div()
                        .id(id)
                        .role(Role::Alert)
                        .mb(px(8.))
                        .p(px(8.))
                        .border_1()
                        .border_color(rgb(ERROR))
                        .text_color(rgb(ERROR))
                        .child(error.clone()),
                );
            }
        }
        if !state.entries_error.is_empty() {
            page = page.child(
                button(
                    "torrent-retry-list",
                    "Retry list",
                    state.entries_loading || reads_fenced,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    if !this.torrents.entries_loading && !this.stopping && !this.writes_fenced() {
                        this.load_torrent_entries(window, cx);
                    }
                })),
            );
        }
        if state.feed_error.is_empty() && !state.failures.is_empty() {
            let mut failures = div()
                .mb(px(8.))
                .p(px(8.))
                .border_1()
                .border_color(rgb(OCHRE))
                .text_color(rgb(OCHRE))
                .child(format!(
                    "{} feed{} failed",
                    state.failures.len(),
                    if state.failures.len() == 1 { "" } else { "s" }
                ));
            for failure in &state.failures {
                failures = failures.child(
                    div()
                        .text_size(px(12.))
                        .child(format!("{}: {}", failure.url, failure.error)),
                );
            }
            page = page.child(failures);
        }
        if !state.entries_loaded && state.entries_error.is_empty() {
            page = page.child(
                div()
                    .py(px(12.))
                    .text_color(rgb(DIM))
                    .child("Loading your list…"),
            );
        }
        if state.loading && !state.loaded {
            page = page.child(
                div()
                    .text_color(rgb(DIM))
                    .child("Checking feeds… You can browse downloads for your list below."),
            );
        }
        if let Some(widget) = &state.widget {
            let rows = state.rows.clone();
            let weak = cx.weak_entity();
            let scroll_weak = weak.clone();
            let generation = self.generation;
            let account = self.profile.as_ref().map(|user| user.id);
            page = page.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        list(widget.clone(), move |index, _, app| {
                            let Some(row) = rows.get(index) else {
                                return div().into_any();
                            };
                            let cover = if let TorrentDisplayRow::Show(group) = row {
                                weak.read_with(app, |this, _| {
                                    this.cover_images
                                        .get(&(group.entry.media_id, 40, 56))
                                        .cloned()
                                })
                                .ok()
                                .flatten()
                            } else {
                                None
                            };
                            render_torrent_row(row, cover, weak.clone(), app)
                        })
                        .flex_1(),
                    )
                    .child(list_scrollbar(
                        "torrents-scrollbar",
                        "Torrents scroll position",
                        widget.clone(),
                        move |first, _, app| {
                            let _ = scroll_weak.update(app, |this, cx| {
                                if this.generation == generation
                                    && this.profile.as_ref().map(|user| user.id) == account
                                {
                                    this.prefetch_torrent_covers(first..first.saturating_add(24));
                                    cx.notify();
                                }
                            });
                        },
                    )),
            );
        }
        page.into_any()
    }

    pub(super) fn dismiss_torrent_remove(&mut self, cx: &mut Context<Self>) {
        if self.active_modal() == Some(super::ModalLayer::TorrentRemove) && !self.torrents.removing
        {
            self.torrents.removing_feed = None;
            cx.notify();
        }
    }

    pub(super) fn render_torrent_remove(&self, cx: &mut Context<Self>) -> AnyElement {
        let url = self.torrents.removing_feed.as_deref().unwrap_or_default();
        let generation = self.generation;
        let account = self.profile.as_ref().map(|user| user.id);
        crate::components::dialog("torrent-remove-confirm", "Remove feed?")
            .w(px(440.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(crate::components::heading("Remove feed?"))
            .child(
                div()
                    .id("torrent-remove-url")
                    .min_w_0()
                    .min_h_0()
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .whitespace_normal()
                    .child(url.to_owned()),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap(px(8.))
                    .child(
                        button(
                            "torrent-remove-confirm-yes",
                            if self.torrents.removing {
                                "Removing…"
                            } else {
                                "Remove"
                            },
                            !self.can_remove_torrent_feed(),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.torrent_scope_matches(generation, account) {
                                    this.remove_torrent_feed(window, cx);
                                }
                            },
                        )),
                    )
                    .child(
                        button(
                            "torrent-remove-confirm-no",
                            "Cancel",
                            self.torrents.removing,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.dismiss_torrent_remove(cx))),
                    ),
            )
            .into_any()
    }
}

fn render_torrent_row(
    row: &TorrentDisplayRow,
    cover: Option<Arc<gpui::RenderImage>>,
    weak: WeakEntity<Workspace>,
    app: &mut gpui::App,
) -> AnyElement {
    let (reads_fenced, release_disabled) = weak
        .read_with(app, |this, _| {
            (
                this.stopping || this.writes_fenced(),
                this.stopping || this.edits_fenced() || this.modal_open(),
            )
        })
        .unwrap_or((true, true));
    match row {
        TorrentDisplayRow::SearchHeading => div()
            .w_full()
            .py(px(8.))
            .border_b_1()
            .border_color(rgb(EDGE))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgb(DIM))
                    .child("SEARCH RESULTS"),
            )
            .child(
                div().text_size(px(12.)).text_color(rgb(DIM)).child(
                    weak.read_with(app, |this, _| this.torrents.submitted_query.clone())
                        .unwrap_or_default(),
                ),
            )
            .child(
                div().text_size(px(12.)).text_color(rgb(DIM)).child(
                    weak.read_with(app, |this, _| {
                        if this.torrents.searching {
                            "Searching Nyaa…".to_owned()
                        } else if this.torrents.results.iter().all(|item| {
                            !matches_filters(item, &this.torrents.category, &this.torrents.release)
                        }) {
                            "No results match this search and the selected filters.".to_owned()
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default(),
                ),
            )
            .into_any(),
        TorrentDisplayRow::SearchRelease(item) => {
            render_release_row(item, "search".into(), release_disabled, weak)
        }
        TorrentDisplayRow::Category {
            status,
            total,
            releases,
            new_count,
        } => {
            let status = *status;
            let (expanded, status_disabled) = weak
                .read_with(app, |this, _| {
                    (
                        this.torrents.expanded_statuses.contains(status),
                        !this.torrents.prefs_loaded || this.edits_fenced(),
                    )
                })
                .unwrap_or((false, true));
            let label = status_label(status);
            let weak_action = weak.clone();
            let mut row = div()
                .w_full()
                .id(format!("torrent-status-{status}"))
                .role(Role::Button)
                .aria_label(format!(
                    "{}: {} shows, {} feed releases",
                    label, total, releases
                ))
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(EDGE))
                .h(px(56.))
                .px(px(14.))
                .flex()
                .items_center()
                .gap(px(10.))
                .tab_stop(!status_disabled)
                .aria_expanded(expanded)
                .child(
                    div()
                        .text_color(rgb(DIM))
                        .child(if expanded { "⌄" } else { "›" }),
                )
                .child(
                    div()
                        .font(serif_font())
                        .font_weight(FontWeight::BOLD)
                        .text_size(px(20.))
                        .child(label),
                )
                .child(div().flex_1())
                .child(div().text_size(px(12.)).text_color(rgb(DIM)).child(format!(
                    "{total} show{} · {releases} feed release{}",
                    if *total == 1 { "" } else { "s" },
                    if *releases == 1 { "" } else { "s" },
                )))
                .when(*new_count > 0, |row| {
                    row.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(ACCENT))
                            .child(format!("{new_count} new")),
                    )
                });
            if status_disabled {
                row = row
                    .opacity(0.45)
                    .aria_description("Unavailable")
                    .a11y_synthetic_children(|builder| {
                        crate::components::disable_control_node(builder.parent_node())
                    })
                    .capture_any_mouse_down(|_, _, cx| cx.stop_propagation());
            } else {
                row = row
                    .focusable()
                    .focus_visible(|style| style.border_color(rgb(ACCENT)))
                    .cursor_pointer()
                    .on_click(move |_, window, app| {
                        let _ = weak_action.update(app, |this, cx| {
                            this.toggle_torrent_status(status, window, cx)
                        });
                    });
            }
            div().w_full().py(px(4.)).child(row).into_any()
        }
        TorrentDisplayRow::Filter {
            status,
            shown,
            total,
        } => {
            let input = weak
                .read_with(app, |this, _| {
                    this.torrents.show_inputs.get(status).cloned()
                })
                .ok()
                .flatten();
            let mut row = div()
                .w_full()
                .pl(px(12.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.));
            if let Some(input) = input {
                row = row.child(
                    div()
                        .w(px(240.))
                        .border_1()
                        .border_color(rgb(EDGE))
                        .bg(rgb(PANEL))
                        .p(px(5.))
                        .child(input),
                );
            }
            row.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(DIM))
                    .child(if *shown == 0 {
                        "No shows or releases match your filter.".to_owned()
                    } else {
                        format!("{shown} of {total} shows")
                    }),
            )
            .into_any()
        }
        TorrentDisplayRow::Show(group) => {
            let id = group.entry.media_id;
            let title = group.title.clone();
            let progress = group.entry.progress.max(0);
            let total = group
                .entry
                .media
                .as_ref()
                .and_then(|media| media.episodes)
                .filter(|total| *total > 0);
            let finished = group
                .entry
                .media
                .as_ref()
                .and_then(|media| media.status.as_deref())
                == Some("FINISHED");
            let next = progress
                .checked_add(1)
                .filter(|next| total.is_none_or(|total| *next <= total))
                .map_or_else(String::new, |episode| format!("Next episode {episode} · "));
            let caption = format!(
                "Find batches · {next}{} feed releases{}",
                group.items.len(),
                if finished { " · Finished airing" } else { "" }
            );
            let weak_toggle = weak.clone();
            let weak_details = weak.clone();
            let expanded = weak
                .read_with(app, |this, _| this.torrents.expanded_shows.contains(&id))
                .unwrap_or(false);
            let mut row = div()
                .w_full()
                .id(format!("torrent-show-{id}"))
                .role(Role::Button)
                .aria_label(format!("Download options for {title}"))
                .aria_description(caption.clone())
                .aria_expanded(expanded)
                .tab_stop(true)
                .focusable()
                .focus_visible(|style| style.border_color(rgb(ACCENT)))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(PANEL_2)))
                .flex()
                .items_center()
                .gap(px(10.))
                .p(px(8.))
                .pl(px(16.))
                .mb(px(3.))
                .border_1()
                .border_color(rgb(EDGE))
                .bg(rgb(PANEL))
                .on_click(move |_, window, app| {
                    let _ = weak_toggle
                        .update(app, |this, cx| this.toggle_show_choices(id, window, cx));
                });
            if let Some(image) = cover {
                row = row.child(img(image).w(px(40.)).h(px(56.)));
            } else {
                row = row.child(div().w(px(40.)).h(px(56.)).bg(rgb(PANEL_2)));
            }
            row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        button(format!("torrent-show-detail-{id}"), title, false).on_click(
                            move |_, window, app| {
                                app.stop_propagation();
                                let _ = weak_details.update(app, |this, cx| {
                                    this.navigate(Route::Anime(id), window, cx)
                                });
                            },
                        ),
                    )
                    .child(div().text_size(px(12.)).text_color(rgb(DIM)).child(caption))
                    .when(group.has_new, |row| {
                        row.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(ACCENT))
                                .child("new"),
                        )
                    }),
            )
            .into_any()
        }
        TorrentDisplayRow::ChoiceStatus {
            media_id,
            text,
            retry,
        } => {
            let mut row = div()
                .w_full()
                .pl(px(24.))
                .py(px(7.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(rgb(DIM))
                .child(div().flex_1().child(text.clone()));
            if *retry {
                let media_id = *media_id;
                row = row.child(
                    button(
                        format!("torrent-choice-retry-{media_id}"),
                        "Search again",
                        reads_fenced,
                    )
                    .on_click(move |_, window, app| {
                        let _ = weak.update(app, |this, cx| {
                            this.load_show_choices(media_id, true, window, cx)
                        });
                    }),
                );
            }
            row.into_any()
        }
        TorrentDisplayRow::ChoiceSection {
            media_id,
            title,
            count,
        } => div()
            .w_full()
            .pl(px(32.))
            .py(px(6.))
            .border_y_1()
            .border_color(rgb(EDGE))
            .bg(rgb(PANEL_2))
            .text_size(px(12.))
            .text_color(rgb(DIM))
            .child(format!("{title} · {count}"))
            .id(format!("torrent-choice-section-{media_id}-{title}"))
            .into_any(),
        TorrentDisplayRow::ChoiceRelease { media_id, item } => {
            render_release_row(item, format!("show-{media_id}"), release_disabled, weak)
        }
        TorrentDisplayRow::ChoiceMore {
            media_id,
            title,
            count,
            expanded,
        } => {
            let media_id = *media_id;
            let title = title.clone();
            let label = if *expanded {
                "Show fewer".to_owned()
            } else {
                format!("Show all {count}")
            };
            div()
                .w_full()
                .pl(px(24.))
                .py(px(5.))
                .child(
                    button(
                        format!("torrent-more-{media_id}-{title}"),
                        format!("{label} {}", title.to_lowercase()),
                        false,
                    )
                    .aria_expanded(*expanded)
                    .on_click(move |_, _, app| {
                        let _ = weak.update(app, |this, cx| {
                            this.toggle_torrent_section(media_id, title.clone(), cx)
                        });
                    }),
                )
                .into_any()
        }
        TorrentDisplayRow::Unmatched(count) => div()
            .w_full()
            .py(px(8.))
            .text_size(px(12.))
            .text_color(rgb(DIM))
            .child(format!(
                "{count} feed item{} didn't match your list and {} hidden.",
                if *count == 1 { "" } else { "s" },
                if *count == 1 { "is" } else { "are" }
            ))
            .into_any(),
    }
}

fn render_release_row(
    item: &Arc<TorrentItem>,
    scope: String,
    disabled: bool,
    weak: WeakEntity<Workspace>,
) -> AnyElement {
    let guid = release_row_key(item);
    let title = item.title.clone();
    let mut meta = Vec::new();
    if item.is_new {
        meta.push("NEW".to_owned());
    }
    if let Some(episode) = item.episode {
        meta.push(format!("Ep {episode}"));
    }
    let category = match item.category_id.as_deref() {
        Some("1_1") => "Anime music video",
        Some("1_2") => "English-translated",
        Some("1_3") => "Non-English-translated",
        Some("1_4") => "Raw",
        _ => item.category.as_deref().unwrap_or("Category unknown"),
    };
    meta.push(category.to_owned());
    if item.trusted == Some(true) {
        meta.push("Trusted".into());
    }
    if item.remake == Some(true) {
        meta.push("Remake".into());
    }
    if let Some(size) = &item.size {
        meta.push(size.clone());
    }
    if let Some(seeders) = item.seeders {
        meta.push(format!("↑{seeders}"));
    }
    if let Some(leechers) = item.leechers {
        meta.push(format!("↓{leechers}"));
    }
    if let Some(published) = item.published {
        meta.push(torrent_time_ago(published));
    }
    let border = if item.remake == Some(true) {
        ERROR
    } else if item.trusted == Some(true) {
        ACCENT
    } else {
        EDGE
    };
    let mut row = div()
        .w_full()
        .id(format!("torrent-release-{scope}-{guid}"))
        .role(Role::Group)
        .pl(px(30.))
        .py(px(8.))
        .pr(px(8.))
        .mb(px(2.))
        .border_l_2()
        .border_color(rgb(border))
        .bg(rgb(PANEL))
        .flex()
        .items_start()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .text_color(rgb(if item.seen { DIM } else { INK }))
                .child(title)
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(DIM))
                        .child(meta.join(" · ")),
                ),
        );
    if let Some(url) = item.details_url.clone() {
        let weak_details = weak.clone();
        row = row.child(
            button(
                format!("torrent-details-{scope}-{guid}"),
                "Details",
                disabled,
            )
            .aria_label("Open release page")
            .on_click(move |_, window, app| {
                let _ = weak_details.update(app, |this, cx| {
                    this.open_torrent_url(None, url.clone(), window, cx)
                });
            }),
        );
    }
    if let Some(url) = item.magnet.clone() {
        let weak_magnet = weak.clone();
        let selected = Arc::clone(item);
        row = row.child(
            button(format!("torrent-magnet-{scope}-{guid}"), "Magnet", disabled)
                .aria_label("Open magnet in torrent client")
                .on_click(move |_, window, app| {
                    let _ = weak_magnet.update(app, |this, cx| {
                        this.open_torrent_url(Some((*selected).clone()), url.clone(), window, cx)
                    });
                }),
        );
    }
    let link = item.link.clone();
    let item = Arc::clone(item);
    row.child(
        button(
            format!("torrent-download-{scope}-{guid}"),
            "Download",
            disabled,
        )
        .aria_label("Download torrent file")
        .on_click(move |_, window, app| {
            let _ = weak.update(app, |this, cx| {
                this.open_torrent_url(Some((*item).clone()), link.clone(), window, cx)
            });
        }),
    )
    .into_any()
}

fn torrent_time_ago(timestamp: i64) -> String {
    let elapsed = chrono::Utc::now().timestamp().saturating_sub(timestamp);
    if elapsed < 60 {
        "just now".into()
    } else if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3600)
    } else if elapsed < 604_800 {
        format!("{}d", elapsed / 86_400)
    } else {
        chrono::DateTime::from_timestamp(timestamp, 0)
            .map_or(String::new(), |date| date.format("%b %-d").to_string())
    }
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn test_open_torrent(
        &mut self,
        item: Option<TorrentItem>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> tokio::sync::oneshot::Sender<Result<(), String>> {
        let (release, result) = tokio::sync::oneshot::channel();
        assert!(self.torrent_open_result.replace(result).is_none());
        self.open_torrent_url(item, "https://example.invalid/release".into(), window, cx);
        self.torrent_open_result.take();
        release
    }

    pub(crate) fn test_torrent_owner(&mut self, generation: u64, account: i64) {
        self.generation = generation;
        self.profile = Some(kurisu_core::User {
            id: account,
            offline: true,
            ..Default::default()
        });
    }

    pub(crate) fn test_feed_input(&self) -> Entity<TextInput> {
        self.torrents.feed_input.clone().unwrap()
    }

    pub(crate) fn test_torrent_filter_menus(&self) -> Option<(bool, bool)> {
        self.torrents
            .prefs_loaded
            .then_some((self.torrents.category_menu, self.torrents.release_menu))
    }

    pub(crate) fn test_torrent_error(&self) -> &str {
        &self.torrents.error
    }

    pub(crate) fn test_cancel_torrent_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_barrier(window, cx);
    }

    pub(crate) fn test_remove_torrent_feed(&mut self, window: &Window, cx: &mut Context<Self>) {
        let previous = self
            .torrents
            .removing_feed
            .replace("https://example.invalid/feed".into());
        self.remove_torrent_feed(window, cx);
        if !self.torrents.removing {
            self.torrents.removing_feed = previous;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use kurisu_core::{Core, Db, models::Media};
    use std::path::Path;

    #[gpui::test]
    fn torrent_next_episode_handles_progress_boundaries(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(gpui::size(px(1100.), px(1000.)));
        workspace.update_in(cx, |w, window, cx| {
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.torrents.prefs_loaded = true;
            w.torrents.loaded = true;
            w.torrents.entries_loaded = true;
            w.torrents.feeds_loaded = true;
            w.navigate(Route::Torrents, window, cx);
        });
        cx.simulate_accessibility_activation();
        for (progress, total, next) in [
            (i64::MAX, None, None),
            (i64::MAX - 1, None, Some(i64::MAX)),
            (12, Some(12), None),
            (-1, Some(0), Some(1)),
        ] {
            workspace.update_in(cx, |w, _, cx| {
                w.torrents.entries = vec![ListEntry {
                    media_id: 9,
                    progress,
                    status: "CURRENT".into(),
                    media: Some(Media {
                        id: 9,
                        episodes: total,
                        ..Default::default()
                    }),
                    ..Default::default()
                }];
                w.rebuild_torrent_rows();
                cx.notify();
            });
            cx.update(|window, app| window.draw(app).clear(app));
            let tree = cx.take_accessibility_tree_update().unwrap();
            let text = tree
                .nodes
                .iter()
                .filter_map(|(_, node)| node.description())
                .find(|text| text.starts_with("Find batches"))
                .unwrap();
            assert_eq!(
                text,
                next.map_or_else(
                    || "Find batches · 0 feed releases".into(),
                    |episode| format!("Find batches · Next episode {episode} · 0 feed releases"),
                )
            );
        }
    }

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

    fn fixture(entries: &[ListEntry], feeds: Option<&str>) -> (Core, tokio::runtime::Runtime) {
        let db = Db::open(Path::new(":memory:")).unwrap();
        if let Some(feeds) = feeds {
            db.set_setting("rss_feeds", feeds).unwrap();
        }
        if !entries.is_empty() {
            db.replace_account(
                "test-token",
                &kurisu_core::User {
                    id: 991_001,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        for entry in entries {
            db.upsert_entry(entry).unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::from_db(
            db,
            std::env::temp_dir(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
        );
        (core, runtime)
    }

    fn settle(runtime: &tokio::runtime::Runtime, cx: &mut TestAppContext) {
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(2)).await });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn direct_read_starts_notify_before_completion(tc: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (view, cx) = tc.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        view.update(cx, |w, _| {
            w.profile = Some(kurisu_core::User {
                id: 991_001,
                ..Default::default()
            });
            w.list.error = "Previous list read failed".into();
        });
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let notified = observed.clone();
        let _subscription = cx.update(|_, app| app.observe(&view, move |_, _| notified.set(true)));
        let starts = [
            Workspace::load_list as fn(&mut Workspace, &Window, &mut Context<Workspace>),
            |w, window, cx| w.load_detail(9, window, cx),
            Workspace::load_torrent_entries,
            Workspace::load_torrent_feeds,
            |w, window, cx| w.refresh_torrent_feed(true, window, cx),
        ];
        for start in starts {
            view.update_in(cx, |w, window, cx| start(w, window, cx));
            assert!(observed.replace(false));
        }
        view.read_with(cx, |w, _| {
            assert!(w.list.loading && w.detail.loading);
            assert!(w.torrents.entries_loading && w.torrents.feeds_loading && w.torrents.loading);
            assert!(w.list.error.is_empty());
        });
    }

    #[gpui::test]
    fn feeds_heading_contains_an_operable_disclosure(tc: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = tc.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |w, window, cx| {
            w.profile = Some(kurisu_core::User {
                id: 991_001,
                ..Default::default()
            });
            w.restoring_session = false;
            w.route = Route::Torrents;
            w.enter_torrents(window, cx);
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (_, heading) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Heading && node.label() == Some("Feeds"))
            .expect("Feeds heading");
        assert_eq!(heading.level(), Some(2));
        let (id, button) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Button && node.label() == Some("Feeds"))
            .expect("Feeds disclosure");
        assert!(heading.children().contains(id));
        assert!(button.supports_action(gpui::AccessibleAction::Click));
        assert_eq!(button.is_expanded(), Some(false));

        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::Click,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *id,
            data: None,
        });
        cx.run_until_parked();
        let expanded = cx.take_accessibility_tree_update().unwrap();
        assert!(
            expanded
                .nodes
                .iter()
                .any(|(node_id, node)| { node_id == id && node.is_expanded() == Some(true) })
        );
        assert!(expanded.nodes.iter().any(|(_, node)| {
            node.role() == Role::Region && node.label() == Some("Saved feeds")
        }));

        cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
            action: gpui::AccessibleAction::Focus,
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: *id,
            data: None,
        });
        cx.run_until_parked();
        let keystroke = gpui::Keystroke::parse("space").unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        assert!(!workspace.read_with(cx, |w, _| w.torrents.show_feeds));
    }

    #[gpui::test]
    fn removing_last_feed_during_close_refreshes_releases_on_restore(tc: &mut TestAppContext) {
        let url = "http://127.0.0.1:9/feed";
        let (core, runtime) = fixture(&[], Some(&format!(r#"["{url}"]"#)));
        let (view, cx) = tc.add_window_view(|win, cx| {
            Workspace::new_with_session_restore(core.clone(), win, cx, false)
        });
        view.update_in(cx, |w, win, cx| {
            w.profile = Some(kurisu_core::User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.route = Route::Torrents;
            w.torrents.loaded = true;
            w.torrents.feeds = vec![url.into()];
            w.torrents.items = vec![TorrentItem {
                title: "Removed feed release".into(),
                ..Default::default()
            }];
            w.torrents.removing_feed = Some(url.into());
            w.remove_torrent_feed(win, cx);
            assert_eq!(w.active_writes, 1);
            w.request_close(super::super::CloseIntent::Window, win, cx);
        });
        cx.run_until_parked();
        settle(&runtime, cx);
        assert!(runtime.block_on(core.get_rss_feeds()).unwrap().is_empty());
        view.update_in(cx, |w, win, cx| {
            assert!(w.close_ready && w.torrents.feeds.is_empty());
            w.resume_after_close(win, cx);
        });
        settle(&runtime, cx);
        view.read_with(cx, |w, _| {
            assert!(w.torrents.loaded && w.torrents.items.is_empty())
        });
    }

    #[gpui::test]
    fn bucket_b_expanded_choices_restart_after_tray_restore(cx: &mut TestAppContext) {
        let entry = ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            ..Default::default()
        };
        let (core, runtime) = fixture(std::slice::from_ref(&entry), Some("[]"));
        let (workspace, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        workspace.update_in(cx, |w, win, cx| {
            w.profile = Some(kurisu_core::User {
                id: 991_001,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Torrents;
            w.torrents.entries = vec![entry];
            w.toggle_show_choices(9, win, cx);
            assert!(w.torrents.choices[&9].loading);
            w.refresh_torrent_feed(false, win, cx);
            w.request_close(super::super::CloseIntent::Window, win, cx);
            assert!(w.close_ready);
        });
        settle(&runtime, cx);
        workspace.update_in(cx, |w, win, cx| {
            assert!(w.torrents.loaded);
            assert!(w.torrents.expanded_shows.contains(&9));
            assert!(w.torrents.choices.is_empty());
            w.resume_after_close_visibility(true, win, cx);
            assert!(
                w.torrents
                    .choices
                    .get(&9)
                    .is_some_and(|choice| choice.loading),
                "expanded show choices did not restart after tray restore"
            );
        });
        settle(&runtime, cx);
        workspace.read_with(cx, |w, _| {
            let choice = &w.torrents.choices[&9];
            assert!(!choice.loading);
            assert!(choice.error.contains("details are not cached"));
        });
    }

    #[gpui::test]
    fn account_reset_preserves_global_torrent_choices(cx: &mut TestAppContext) {
        let (core, runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
        for _ in 0..8 {
            settle(&runtime, cx);
        }
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.set_profile(
                Some(kurisu_core::User {
                    id: 47,
                    offline: true,
                    ..Default::default()
                }),
                window,
                cx,
            );
            workspace.navigate(Route::Torrents, window, cx);
        });
        for _ in 0..8 {
            settle(&runtime, cx);
        }
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.torrents.prefs_loaded);
            workspace.torrents.new_only = true;
            workspace.persist_preference("kurisu.torrents.new", Some("1".into()), window, cx);
            workspace.torrents.expanded_statuses.clear();
            workspace.toggle_torrent_status("PLANNING", window, cx);
            workspace.change_torrent_filters(Some("1_2".into()), Some("2".into()), window, cx);
            workspace.torrents.entries.push(ListEntry {
                media_id: 9,
                ..Default::default()
            });
            workspace.torrents.expanded_shows.insert(9);
            workspace.set_profile(
                Some(kurisu_core::User {
                    id: 48,
                    offline: true,
                    ..Default::default()
                }),
                window,
                cx,
            );
            assert!(workspace.torrents.entries.is_empty());
            assert!(workspace.torrents.expanded_shows.is_empty());
            assert!(workspace.torrents.prefs_loaded);
            assert!(workspace.preference_has_work());
        });
        for _ in 0..16 {
            settle(&runtime, cx);
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.torrents.new_only);
            assert_eq!(
                workspace.torrents.expanded_statuses,
                HashSet::from(["PLANNING"])
            );
            assert_eq!(workspace.torrents.category, "1_2");
            assert_eq!(workspace.torrents.release, "2");
            assert!(!workspace.preference_has_work());
        });
        assert_eq!(
            runtime
                .block_on(core.get_preference("kurisu.torrents.new".into()))
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }

    #[gpui::test]
    fn preference_retry_does_not_click_a_covered_torrent_section(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        cx.simulate_resize(gpui::size(px(1100.), px(750.)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.profile = Some(kurisu_core::User {
                id: 47,
                ..Default::default()
            });
            workspace.route = Route::Torrents;
            workspace.torrents.prefs_loaded = true;
            workspace.torrents.entries_loaded = true;
            workspace.torrents.feeds_loaded = true;
            workspace.torrents.loaded = true;
            workspace.torrents.expanded_statuses.clear();
            workspace.enter_torrents(window, cx);
            workspace.rebuild_torrent_rows();
            let write = workspace
                .preference_writes
                .entry("kurisu.torrents.new")
                .or_default();
            write.pending = Some(crate::screens::PreferenceEdit::Setting(
                crate::screens::QueuedPreference {
                    value: Some("1".into()),
                    account: Some(47),
                },
            ));
            write.error = Some("storage failure".into());
            cx.notify();
        });
        cx.run_until_parked();
        let retry = cx.debug_bounds("retry-preferences").unwrap();
        cx.simulate_click(retry.center(), gpui::Modifiers::default());
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.torrents.expanded_statuses.is_empty());
        });
    }

    #[gpui::test]
    fn late_preference_read_keeps_new_torrent_selections(cx: &mut TestAppContext) {
        let (core, runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::new(core.clone(), window, cx));
        for _ in 0..8 {
            settle(&runtime, cx);
        }
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.load_torrent_preferences(window, cx);
        });
        runtime.block_on(async { tokio::task::yield_now().await });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.torrents.prefs_loaded = true;
            workspace.torrents.new_only = true;
            workspace.persist_preference("kurisu.torrents.new", Some("1".into()), window, cx);
            workspace.persist_preference("kurisu.torrents.new", Some("0".into()), window, cx);
            workspace.persist_preference("kurisu.torrents.new", Some("1".into()), window, cx);
            workspace.torrents.expanded_statuses.clear();
            workspace.toggle_torrent_status("PLANNING", window, cx);
            workspace.change_torrent_filters(Some("1_2".into()), Some("2".into()), window, cx);
            workspace.request_close(super::super::CloseIntent::Quit, window, cx);
        });
        for _ in 0..16 {
            settle(&runtime, cx);
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.torrents.new_only);
            assert_eq!(
                workspace.torrents.expanded_statuses,
                HashSet::from(["PLANNING"])
            );
            assert_eq!(workspace.torrents.category, "1_2");
            assert_eq!(workspace.torrents.release, "2");
            assert!(workspace.barrier.is_none());
        });
        for (key, value) in [
            ("kurisu.torrents.new", "1"),
            ("kurisu.torrents.statuses", "[\"PLANNING\"]"),
            (
                "kurisu.torrents.filters",
                "{\"category\":\"1_2\",\"release\":\"2\"}",
            ),
        ] {
            assert_eq!(
                runtime
                    .block_on(core.get_preference(key.into()))
                    .unwrap()
                    .as_deref(),
                Some(value)
            );
        }
    }

    #[gpui::test]
    fn retained_row_snapshot_is_unchanged_by_progress_and_seen_updates(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, _, _| {
            workspace.torrents.entries_loaded = true;
            workspace.torrents.entries = vec![ListEntry {
                media_id: 1,
                status: "CURRENT".into(),
                progress: 0,
                ..Default::default()
            }];
            workspace.torrents.items = vec![TorrentItem {
                guid: "snapshot-release".into(),
                media_id: Some(1),
                episode: Some(1),
                seen: false,
                ..Default::default()
            }];
            workspace.rebuild_torrent_rows();
            let snapshot = workspace.torrents.rows.clone();
            workspace.torrents.entries[0].progress = 1;
            workspace.torrents.items[0].seen = true;
            workspace.rebuild_torrent_rows();
            let group = |rows: &Vec<TorrentDisplayRow>| {
                rows.iter()
                    .find_map(|row| {
                        if let TorrentDisplayRow::Show(group) = row {
                            Some(group.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap()
            };
            let old = group(&snapshot);
            let current = group(&workspace.torrents.rows);
            assert_eq!(old.entry.progress, 0);
            assert!(!old.items[0].seen);
            assert!(old.has_new);
            assert_eq!(current.entry.progress, 1);
            assert!(current.items[0].seen);
            assert!(!current.has_new);
            assert!(!Arc::ptr_eq(&old, &current));
        });
    }

    #[gpui::test]
    fn new_search_reveals_results_and_other_rebuilds_keep_the_anchor(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.auth_intent += 1;
            workspace.profile = Some(kurisu_core::User {
                id: 991_001,
                ..Default::default()
            });
            workspace.route = Route::Torrents;
            workspace.torrents.entries_loaded = true;
            workspace.torrents.widget = Some(ListState::new(0, ListAlignment::Top, px(0.)));
            workspace.rebuild_torrent_rows();
            let widget = workspace.torrents.widget.clone().unwrap();
            let previous_key = workspace.torrents.rows[0].key();
            assert_eq!(previous_key, "category-CURRENT");

            workspace.search_torrents(Some("garden".into()), window, cx);
            assert!(workspace.torrents.searching);
            assert!(matches!(
                workspace.torrents.rows[0],
                TorrentDisplayRow::SearchHeading
            ));
            assert_eq!(widget.logical_scroll_top(), ListOffset::default());

            workspace.torrents.searching = false;
            workspace.torrents.searched = true;
            workspace.torrents.results = vec![TorrentItem {
                guid: "synthetic-result".into(),
                category_id: Some("1_2".into()),
                ..Default::default()
            }];
            workspace.rebuild_torrent_rows();
            assert!(matches!(
                workspace.torrents.rows[1],
                TorrentDisplayRow::SearchRelease(_)
            ));
            assert_eq!(widget.logical_scroll_top(), ListOffset::default());

            let index = workspace
                .torrents
                .rows
                .iter()
                .position(|row| row.key() == previous_key)
                .unwrap();
            let offset = ListOffset {
                item_ix: index,
                offset_in_item: px(13.),
            };
            widget.scroll_to(offset);
            workspace.rebuild_torrent_rows();
            assert_eq!(widget.logical_scroll_top(), offset);

            workspace.search_torrents(Some("orbit".into()), window, cx);
            assert_eq!(widget.logical_scroll_top(), ListOffset::default());
        });
    }

    #[gpui::test]
    fn offline_anilist_still_dispatches_empty_rss_feed(cx: &mut TestAppContext) {
        let (core, runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.auth_intent += 1;
            workspace.profile = Some(kurisu_core::User {
                id: 991_001,
                ..Default::default()
            });
            workspace.offline = true;
            workspace.route = Route::Torrents;
            workspace.enter_torrents(window, cx);
            assert!(workspace.torrents.loading);
            assert_eq!(workspace.torrents.request, 1);
        });
        settle(&runtime, cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.torrents.loaded);
            assert!(!workspace.torrents.loading);
            assert!(workspace.torrents.items.is_empty());
            assert!(workspace.torrents.error.is_empty());
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.refresh_torrent_feed(true, window, cx);
            assert!(workspace.torrents.loading);
            assert_eq!(workspace.torrents.request, 2);
        });
        settle(&runtime, cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(workspace.torrents.loaded);
            assert!(!workspace.torrents.loading);
            assert!(workspace.torrents.refreshed.is_some());
        });
    }

    #[gpui::test]
    fn failed_collapsed_choices_stay_within_cache_limit(cx: &mut TestAppContext) {
        let entries: Vec<_> = (1..=64)
            .map(|id| ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .collect();
        let (core, runtime) = fixture(&entries, Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        workspace.update_in(cx, |workspace, _, _| {
            workspace.auth_intent += 1;
            workspace.profile = Some(kurisu_core::User {
                id: 991_001,
                ..Default::default()
            });
            workspace.route = Route::Torrents;
            workspace.torrents.entries = entries;
            workspace.torrents.entries_loaded = true;
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_show_choices(1, window, cx);
            assert!(workspace.torrents.choices[&1].loading);
            workspace.toggle_show_choices(1, window, cx);
            assert!(workspace.torrents.choices.contains_key(&1));
        });
        settle(&runtime, cx);
        workspace.read_with(cx, |workspace, _| {
            assert!(!workspace.torrents.choices.contains_key(&1));
        });

        for id in 2..=64 {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.toggle_show_choices(id, window, cx);
            });
            settle(&runtime, cx);
            workspace.update_in(cx, |workspace, window, cx| {
                let choice = &workspace.torrents.choices[&id];
                assert!(!choice.loading);
                assert!(choice.error.contains("details are not cached"));
                assert!(choice.fetched_at.is_none());
                if id == 2 {
                    let old_request = choice.request;
                    workspace.load_show_choices(id, true, window, cx);
                    let retried = &workspace.torrents.choices[&id];
                    assert!(retried.loading);
                    assert_ne!(retried.request, old_request);
                    assert!(retried.fetched_at.is_none());
                } else {
                    workspace.toggle_show_choices(id, window, cx);
                    assert!(!workspace.torrents.choices.contains_key(&id));
                }
            });
            if id == 2 {
                settle(&runtime, cx);
                workspace.update_in(cx, |workspace, window, cx| {
                    assert!(!workspace.torrents.choices[&id].loading);
                    let old_key = workspace.torrents.choices[&id].key.clone();
                    let old_request = workspace.torrents.choices[&id].request;
                    workspace.toggle_show_choices(id, window, cx);
                    assert!(!workspace.torrents.choices.contains_key(&id));
                    workspace.toggle_show_choices(id, window, cx);
                    assert_eq!(workspace.torrents.choices[&id].key, old_key);
                    assert_ne!(workspace.torrents.choices[&id].request, old_request);
                });
                settle(&runtime, cx);
                workspace.update_in(cx, |workspace, window, cx| {
                    assert!(
                        workspace.torrents.choices[&id]
                            .error
                            .contains("details are not cached")
                    );
                    workspace.toggle_show_choices(id, window, cx);
                });
            }
        }
        workspace.read_with(cx, |workspace, _| {
            assert!(
                workspace.torrents.choices.len() <= 50,
                "Collapsed failed entries escape the intended 50-entry cache limit"
            );
            assert!(workspace.torrents.choices.is_empty());
            assert!(workspace.torrents.expanded_shows.is_empty());
        });
    }

    #[gpui::test]
    fn inactive_successes_evict_before_expanded_or_loading_choices(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            for id in 1..=51 {
                let mut state = ChoiceState::new(format!("{id}"));
                state.choices = Some(ShowTorrents::default());
                state.fetched_at = Some(Instant::now() - Duration::from_secs(52 - id as u64));
                workspace.torrents.choices.insert(id, state);
            }
            let mut expanded_error = ChoiceState::new("52".into());
            expanded_error.error = "synthetic RSS failure".into();
            workspace.torrents.choices.insert(52, expanded_error);
            let mut loading = ChoiceState::new("53".into());
            loading.loading = true;
            workspace.torrents.choices.insert(53, loading);
            workspace.torrents.expanded_shows.extend([51, 52]);

            workspace.toggle_show_choices(51, window, cx);
            assert_eq!(workspace.torrents.choices.len(), 50);
            assert!((1..=3).all(|id| !workspace.torrents.choices.contains_key(&id)));
            assert!(workspace.torrents.choices.contains_key(&51));
            assert!(workspace.torrents.choices.contains_key(&52));
            assert!(workspace.torrents.choices.contains_key(&53));

            workspace.torrents.choices.get_mut(&53).unwrap().loading = false;
            workspace.prune_show_choices();
            assert!(!workspace.torrents.choices.contains_key(&53));
            workspace.torrents.expanded_shows.remove(&52);
            workspace.prune_show_choices();
            assert!(!workspace.torrents.choices.contains_key(&52));
        });
    }

    #[test]
    fn feed_new_is_recomputed_from_progress_and_seen() {
        let entry = ListEntry {
            media_id: 42,
            progress: 4,
            ..Default::default()
        };
        let mut item = TorrentItem {
            media_id: Some(42),
            episode: Some(5),
            category_id: Some("1_2".into()),
            remake: Some(false),
            ..Default::default()
        };
        assert!(is_new_for(&item, &entry));
        item.seen = true;
        assert!(!is_new_for(&item, &entry));
        item.seen = false;
        item.episode = Some(4);
        assert!(!is_new_for(&item, &entry));
    }

    #[test]
    fn visible_groups_keep_list_shows_without_feed_releases() {
        let entries = vec![ListEntry {
            media_id: 42,
            status: "CURRENT".into(),
            ..Default::default()
        }];
        let items = vec![TorrentItem {
            media_id: Some(42),
            category_id: Some("1_4".into()),
            ..Default::default()
        }];
        let groups = display_groups(&entries, &items, "1_2", "0", true);
        assert_eq!(groups.len(), 1);
        assert!(groups[0].items.is_empty());
    }

    #[test]
    fn release_filter_requires_explicit_trust_or_non_remake() {
        let item = TorrentItem::default();
        assert!(!matches_filters(&item, "1_0", "1"));
        assert!(!matches_filters(&item, "1_0", "2"));
        assert!(matches_filters(&item, "1_0", "0"));
    }

    #[test]
    fn choices_limit_initial_section_and_keep_unmatched_feed_release() {
        let batches: Vec<_> = (0..7)
            .map(|index| TorrentItem {
                guid: format!("batch-{index}"),
                link: format!("https://nyaa.si/download/{index}.torrent"),
                ..Default::default()
            })
            .collect();
        let recent = TorrentItem {
            guid: "recent".into(),
            link: "https://nyaa.si/download/recent.torrent".into(),
            ..Default::default()
        };
        let group = ShowGroup {
            entry: ListEntry {
                media_id: 42,
                ..Default::default()
            },
            title: "Program".into(),
            cover: None,
            items: vec![batches[0].clone(), recent],
            has_new: false,
            newest: 0,
        };
        let mut state = ChoiceState::new("42".into());
        state.choices = Some(ShowTorrents {
            batches,
            ..Default::default()
        });
        let mut rows = Vec::new();
        append_choice_rows(&mut rows, &group, Some(&state), &HashSet::new());
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, TorrentDisplayRow::ChoiceRelease { .. }))
                .count(),
            7
        );
        assert!(rows.iter().any(|row| matches!(
            row,
            TorrentDisplayRow::ChoiceMore {
                count: 7,
                expanded: false,
                ..
            }
        )));
        assert_eq!(rows.iter().filter(|row| matches!(row, TorrentDisplayRow::ChoiceSection { title, .. } if title == "Recent feed releases")).count(), 1);
    }

    #[gpui::test]
    fn torrent_calendar_and_list_preferences_respect_all_edit_fences(cx: &mut TestAppContext) {
        let (core, _runtime) = fixture(&[], Some("[]"));
        let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::new(core, window, cx));
        workspace.update_in(cx, |w, window, cx| {
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.torrents.prefs_loaded = true;
            w.calendar.preference_loaded = true;
            let item = TorrentItem {
                guid: "controlled-guid".into(),
                link: "https://example.invalid/item.torrent".into(),
                ..Default::default()
            };
            w.auth_busy = true;
            let auth_opener = w.test_open_torrent(Some(item.clone()), window, cx);
            assert!(
                auth_opener.is_closed() && w.active_writes == 0,
                "open_torrent_url admitted write during auth_busy"
            );
            w.auth_busy = false;
            w.update_busy = true;
            let update_opener = w.test_open_torrent(Some(item), window, cx);
            assert!(
                update_opener.is_closed() && w.active_writes == 0,
                "open_torrent_url admitted write during update_busy"
            );

            w.select_filter("COMPLETED", window, cx);
            assert_eq!(w.list.filter, "CURRENT");
            w.select_sort(crate::screens::state::SortKey::Score, window, cx);
            assert_eq!(w.list.sort, crate::screens::state::SortKey::Title);
            w.flip_sort(window, cx);
            assert!(!w.list.desc);
            w.list_query.update(cx, |input, cx| {
                input.set_value("mutated", cx);
                cx.emit(TextInputEvent::Changed("mutated".into()));
            });
            w.stage_query(Some("mutated".into()));
            w.persist_preference("kurisu.list.filter", Some("COMPLETED".into()), window, cx);
            w.set_calendar_filter(false, window, cx);
            assert!(w.calendar.watching_only);
            w.change_torrent_filters(Some("1_2".into()), Some("2".into()), window, cx);
            assert_eq!(w.torrents.category, "1_0");
            assert_eq!(w.torrents.release, "0");
            w.toggle_torrent_status("PLANNING", window, cx);
            assert!(!w.torrents.expanded_statuses.contains("PLANNING"));
            assert!(w.query_pending.is_none());
            assert!(!w.preference_has_work());
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, app| {
            assert_eq!(w.list.query, "");
            assert_eq!(w.list_query.read(app).value(), "");
        });
        workspace.update_in(cx, |w, _, cx| {
            w.update_busy = false;
            cx.notify();
        });
    }

    #[gpui::test]
    fn fenced_torrent_entries_completion_preserves_pending_reload_until_unfence(
        cx: &mut TestAppContext,
    ) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kurisu-torrents-fenced-entries-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = Db::open(&root.join("kurisu.db")).unwrap();
        db.set_setting("rss_feeds", "[]").unwrap();
        db.replace_account(
            "test-token",
            &kurisu_core::User {
                id: 47,
                ..Default::default()
            },
        )
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let core = Core::open(
            root.clone(),
            runtime.handle().clone(),
            Arc::new(TestPlatform),
        )
        .unwrap();
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        let first_read = workspace.update_in(cx, |w, window, cx| {
            w.restoring_session = false;
            w.profile = Some(kurisu_core::User {
                id: 47,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.route = Route::Torrents;
            w.load_torrent_entries(window, cx);
            assert!(w.torrents.entries_loading);
            w.read_aborts["torrents-entries"][0].clone()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !first_read.is_finished() {
            assert!(Instant::now() < deadline);
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        db.upsert_entry(&ListEntry {
            media_id: 2,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        workspace.update_in(cx, |w, window, cx| {
            w.load_torrent_entries(window, cx);
            assert!(w.torrents.entries_reload_pending);
            w.request_close(super::super::CloseIntent::Window, window, cx);
            assert!(w.close_ready);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(!w.torrents.entries_loading);
            assert_eq!(w.torrents.entries.len(), 1);
            assert!(
                w.torrents.entries_reload_pending,
                "fenced torrent completion dropped entries_reload_pending"
            );
        });
        workspace.update_in(cx, |w, window, cx| {
            w.resume_after_close_visibility(true, window, cx);
        });
        while workspace.read_with(cx, |w, _| w.torrents.entries_loading) {
            assert!(Instant::now() < deadline);
            settle(&runtime, cx);
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.torrents.entries.len(), 2);
            assert!(!w.torrents.entries_reload_pending);
        });

        let second_read = workspace.update_in(cx, |w, window, cx| {
            w.load_torrent_entries(window, cx);
            assert!(w.torrents.entries_loading);
            w.read_aborts["torrents-entries"][0].clone()
        });
        while !second_read.is_finished() {
            assert!(Instant::now() < deadline);
            runtime.block_on(async { tokio::time::sleep(Duration::from_millis(1)).await });
        }
        db.upsert_entry(&ListEntry {
            media_id: 3,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        workspace.update_in(cx, |w, window, cx| {
            w.load_torrent_entries(window, cx);
            assert!(w.torrents.entries_reload_pending);
            w.query_error = "hold barrier".into();
            w.request_close(super::super::CloseIntent::Window, window, cx);
        });
        cx.run_until_parked();
        workspace.read_with(cx, |w, _| {
            assert!(
                !w.torrents.entries_loading,
                "fenced torrent completion started a new read during barrier"
            );
            assert_eq!(w.torrents.entries.len(), 2);
            assert!(w.torrents.entries_reload_pending);
        });
        workspace.update_in(cx, |w, _, _| w.query_error.clear());
        cx.update(|win, app| win.draw(app).clear(app));
        let cancel = cx.debug_bounds("cancel-failed-barrier").unwrap();
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        while workspace.read_with(cx, |w, _| w.torrents.entries_loading) {
            assert!(Instant::now() < deadline);
            settle(&runtime, cx);
        }
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.torrents.entries.len(), 3);
            assert!(!w.torrents.entries_reload_pending);
        });
        cx.update(|win, _| win.remove_window());
        drop(workspace);
        drop(db);
        drop(runtime);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod modal_regressions {
    use super::super::workspace_tests::fixture;
    use super::*;
    use gpui::accesskit::{Action, ActionRequest, TreeId};
    use kurisu_core::User;

    #[gpui::test]
    fn torrent_remove_escape_works_from_pending_banner_focus(cx: &mut gpui::TestAppContext) {
        let (core, _runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            w.offline = true;
            w.route = Route::Torrents;
            w.pending_error = "Sync status fixture".into();
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Retry sync"))
            .unwrap();
        cx.simulate_accessibility_action(ActionRequest {
            action: Action::Focus,
            target_tree: TreeId::ROOT,
            target_node: *id,
            data: None,
        });
        view.update_in(cx, |w, win, cx| {
            let feed = format!("https://example.invalid/{}", "long-feed".repeat(27));
            w.torrents.feeds = vec![feed.clone()];
            w.ask_remove_torrent_feed(&feed, win, cx);
            w.auth_busy = true;
            w.remove_torrent_feed(win, cx);
            assert_eq!(w.active_writes, 0);
            cx.notify();
        });
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| node.is_modal()));
        let dialog = tree
            .nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == gpui::Role::Dialog && node.label() == Some("Remove feed?"))
                    .then(|| node.bounds().unwrap())
            })
            .unwrap();
        let scale = cx.update(|win, _| f64::from(win.scale_factor()));
        assert!(
            dialog.width() <= 440. * scale && dialog.height() > 150. * scale,
            "{dialog:?}"
        );
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.label() == Some("Remove") && !node.is_hidden() && node.is_disabled()
        }));
        view.update_in(cx, |w, win, cx| {
            w.torrents.removing = true;
            w.escape_active_modal(win, cx);
            assert!(w.torrents.removing_feed.is_some());
            w.torrents.removing = false;
        });
        cx.simulate_keystrokes("escape");
        view.update_in(cx, |w, win, cx| {
            assert!(w.torrents.removing_feed.is_none());
            w.auth_busy = false;
            w.profile = None;
            w.torrents.removing_feed = Some("https://example.invalid/feed".into());
            w.remove_torrent_feed(win, cx);
            assert_eq!(w.active_writes, 0);
        });
    }

    #[gpui::test]
    fn outside_click_dismisses_filter_menus_and_unloaded_checkbox_is_disabled(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AccessibleAction;

        let (core, _runtime, _dir) = fixture();
        let (view, cx) =
            cx.add_window_view(|win, cx| Workspace::new_with_session_restore(core, win, cx, false));
        cx.simulate_resize(gpui::size(px(1100.), px(1000.)));
        view.update_in(cx, |w, _, cx| {
            w.profile = Some(User {
                id: 7,
                offline: true,
                ..Default::default()
            });
            w.route = Route::Torrents;
            w.torrents.prefs_loaded = false;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let tree = cx.take_accessibility_tree_update().unwrap();
        let (_, checkbox) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("New feed releases only"))
            .expect("new-only checkbox node");
        assert!(
            checkbox.is_disabled(),
            "unloaded checkbox should be disabled"
        );
        assert!(!checkbox.supports_action(AccessibleAction::Click));
        assert!(!checkbox.supports_action(AccessibleAction::Focus));

        view.update_in(cx, |w, _, cx| {
            w.torrents.prefs_loaded = true;
            w.torrents.category_menu = true;
            cx.notify();
        });
        cx.run_until_parked();
        let page_bounds = cx
            .debug_bounds("torrents-page")
            .expect("torrents page bounds");
        let outside = point(
            page_bounds.origin.x + px(12.),
            page_bounds.origin.y + px(12.),
        );
        cx.simulate_mouse_down(outside, gpui::MouseButton::Left, gpui::Modifiers::default());
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.category_menu,
                "clicking outside should dismiss the torrent category menu"
            );
        });

        view.update_in(cx, |w, _, cx| {
            w.torrents.release_menu = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(outside, gpui::MouseButton::Left, gpui::Modifiers::default());
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.release_menu,
                "clicking outside should dismiss the torrent release menu"
            );
        });

        view.update_in(cx, |w, _, cx| {
            w.torrents.release_menu = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|win, app| win.draw(app).clear(app));
        let trusted_bounds = cx
            .debug_bounds("torrent-release-2")
            .expect("torrent release option bounds");
        cx.simulate_click(trusted_bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        view.read_with(cx, |w, _| {
            assert_eq!(w.torrents.release, "2");
            assert!(!w.torrents.release_menu);
        });

        view.update_in(cx, |w, _, cx| {
            w.torrents.category_menu = true;
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|win, app| win.draw(app).clear(app));
        cx.simulate_mouse_down(
            point(px(-20.), px(-20.)),
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.category_menu,
                "clicking outside torrents-page should dismiss filter menus"
            );
        });

        view.update_in(cx, |w, win, cx| {
            w.torrents.category_menu = true;
            w.switch_route(Route::Settings, win, cx);
        });
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.category_menu,
                "route switch must dismiss torrent filter menus"
            );
        });

        view.update_in(cx, |w, win, cx| {
            w.torrents.entries_loaded = true;
            w.torrents.feeds_loaded = true;
            w.torrents.prefs_loaded = true;
            w.switch_route(Route::Torrents, win, cx);
            w.cancel_reads("torrent-prefs");
            w.cancel_reads("torrent-entries");
            w.cancel_reads("torrent-feeds");
            w.cancel_reads("torrent-feed");
            w.torrents.entries = vec![ListEntry {
                media_id: 9,
                status: "CURRENT".into(),
                progress: 1,
                media: Some(kurisu_core::models::Media {
                    id: 9,
                    title_english: Some("Show".into()),
                    episodes: Some(12),
                    ..Default::default()
                }),
                ..Default::default()
            }];
            w.rebuild_torrent_rows();
            w.torrents.prefs_loaded = false;
            cx.notify();
        });
        let find_status = |cx: &mut gpui::VisualTestContext| {
            cx.update(|win, app| win.draw(app).clear(app));
            cx.run_until_parked();
            let tree = cx.take_accessibility_tree_update().unwrap();
            tree.nodes
                .into_iter()
                .find_map(|(_, node)| {
                    node.label()
                        .is_some_and(|label| label.starts_with("Watching: "))
                        .then_some(node)
                })
                .expect("torrent-status-CURRENT node")
        };
        let unloaded_status = find_status(cx);
        assert!(unloaded_status.is_disabled());
        assert!(!unloaded_status.supports_action(AccessibleAction::Click));
        assert!(!unloaded_status.supports_action(AccessibleAction::Focus));

        view.update_in(cx, |w, _, cx| {
            w.torrents.prefs_loaded = true;
            w.torrents.category_menu = true;
            w.update_busy = true;
            cx.notify();
        });
        let fenced_status = find_status(cx);
        assert!(fenced_status.is_disabled());
        assert!(!fenced_status.supports_action(AccessibleAction::Click));
        assert!(!fenced_status.supports_action(AccessibleAction::Focus));
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.category_menu,
                "edits_fenced() must dismiss open torrent filter menus"
            );
        });

        view.update_in(cx, |w, _, cx| {
            w.update_busy = false;
            cx.notify();
        });
        let enabled_status = find_status(cx);
        assert!(!enabled_status.is_disabled());
        assert!(enabled_status.supports_action(AccessibleAction::Click));
    }

    #[test]
    fn is_new_for_treats_non_positive_episode_total_as_unknown() {
        let item = TorrentItem {
            episode: Some(4),
            seen: false,
            ..Default::default()
        };
        let mut entry = ListEntry {
            media_id: 9,
            status: "CURRENT".into(),
            progress: 2,
            media: Some(kurisu_core::models::Media {
                id: 9,
                episodes: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(is_new_for(&item, &entry));
        entry.media.as_mut().unwrap().episodes = Some(-2);
        assert!(is_new_for(&item, &entry));
        entry.media.as_mut().unwrap().episodes = Some(3);
        assert!(!is_new_for(&item, &entry));
    }

    #[gpui::test]
    fn torrents_read_and_release_buttons_respect_fences(cx: &mut gpui::TestAppContext) {
        use super::super::workspace_tests::fixture;
        use gpui::AccessibleAction;

        let (core, _runtime, _dir) = fixture();
        let (view, cx) = cx.add_window_view(|window, cx| {
            Workspace::new_with_session_restore(core, window, cx, false)
        });
        cx.simulate_resize(gpui::size(px(1100.), px(900.)));
        view.update_in(cx, |w, window, cx| {
            w.profile = Some(kurisu_core::User {
                id: 7,
                ..Default::default()
            });
            w.route = Route::Torrents;
            w.enter_torrents(window, cx);
            w.cancel_all_reads();
            w.torrents.loading = false;
            w.torrents.entries_loading = false;
            w.torrents.feeds_loading = false;
            w.torrents.entries_error = "db".into();
            w.torrents.results = vec![TorrentItem {
                guid: "rel-1".into(),
                title: "Show - 01".into(),
                link: "https://nyaa.si/download/1.torrent".into(),
                details_url: Some("https://nyaa.si/view/1".into()),
                magnet: Some("magnet:?xt=urn:btih:1".into()),
                ..Default::default()
            }];
            w.torrents.searched = true;
            w.rebuild_torrent_rows();
            w.update_busy = true;
            cx.notify();
        });
        cx.simulate_accessibility_activation();
        cx.run_until_parked();
        let button_entry = |cx: &mut gpui::VisualTestContext, label: &str| {
            cx.update(|win, app| win.draw(app).clear(app));
            cx.run_until_parked();
            let tree = cx.take_accessibility_tree_update().unwrap();
            tree.nodes
                .into_iter()
                .find(|(_, node)| node.role() == Role::Button && node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing torrents button {label}"))
        };
        let button_node = |cx: &mut gpui::VisualTestContext, label: &str| button_entry(cx, label).1;
        for label in [
            "Open release page",
            "Open magnet in torrent client",
            "Download torrent file",
        ] {
            let node = button_node(cx, label);
            assert!(
                node.is_disabled(),
                "{label} should disable while update_busy"
            );
            assert!(!node.supports_action(AccessibleAction::Click));
        }
        let (refresh_id, refresh_node) = button_entry(cx, "↻ Refresh");
        assert!(!refresh_node.is_disabled());
        let (retry_list_id, retry_list_node) = button_entry(cx, "Retry list");
        assert!(!retry_list_node.is_disabled());
        view.update_in(cx, |w, _, _| {
            w.update_busy = false;
            w.barrier = Some(super::super::BarrierAction::Install);
        });
        for target_node in [refresh_id, retry_list_id] {
            cx.simulate_accessibility_action(gpui::accesskit::ActionRequest {
                action: AccessibleAction::Click,
                target_tree: gpui::accesskit::TreeId::ROOT,
                target_node,
                data: None,
            });
        }
        view.read_with(cx, |w, _| {
            assert!(
                !w.torrents.entries_reload_pending && !w.list_refresh_deferred,
                "disabled torrent read buttons must not mutate reload deferral while fenced"
            );
        });

        view.update_in(cx, |w, _, cx| {
            w.barrier = None;
            w.close_ready = true;
            cx.notify();
        });
        for label in ["Search", "↻ Refresh", "Retry list"] {
            let node = button_node(cx, label);
            assert!(
                node.is_disabled(),
                "{label} should disable while close_ready"
            );
            assert!(!node.supports_action(AccessibleAction::Click));
        }
    }
}
