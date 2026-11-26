use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use icu_collator::{
    Collator, CollatorBorrowed, CollatorPreferences,
    options::{CollatorOptions, Strength},
    preferences::CollationNumericOrdering,
};
use icu_locale_core::Locale;
use kurisu_core::models::{EntryDetails, FuzzyDate, LibraryFile, ListEntry, Media, MediaDetail};

#[derive(Default)]
pub(super) struct LibraryFiles {
    files: Vec<LibraryFile>,
    episodes: HashMap<(i64, i64), usize>,
}

impl LibraryFiles {
    pub(super) fn replace(&mut self, files: Vec<LibraryFile>) {
        self.episodes.clear();
        for (index, file) in files.iter().enumerate() {
            if let Some(key) = file.media_id.zip(file.episode) {
                self.episodes.entry(key).or_insert(index);
            }
        }
        self.files = files;
    }

    pub(super) fn clear(&mut self) {
        self.files.clear();
        self.episodes.clear();
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, LibraryFile> {
        self.files.iter()
    }

    pub(super) fn episode(&self, media_id: i64, episode: i64) -> Option<&LibraryFile> {
        self.episodes
            .get(&(media_id, episode))
            .map(|&index| &self.files[index])
    }
}

pub const STATUSES: [&str; 6] = [
    "CURRENT",
    "PLANNING",
    "COMPLETED",
    "PAUSED",
    "DROPPED",
    "REPEATING",
];

pub fn expiration_matches(
    event_generation: u64,
    event_account: Option<i64>,
    current_generation: u64,
    current_account: Option<i64>,
    signing_in: bool,
) -> bool {
    !signing_in && event_generation == current_generation && event_account == current_account
}

pub fn expiration_verification_matches(
    expected_generation: u64,
    expected_intent: u64,
    expected_account: Option<i64>,
    current_generation: u64,
    current_intent: u64,
    current_account: Option<i64>,
    signing_in: bool,
) -> bool {
    !signing_in
        && expected_generation == current_generation
        && expected_intent == current_intent
        && expected_account == current_account
}

pub fn status_label(status: &str) -> &'static str {
    match status {
        "CURRENT" => "Watching",
        "PLANNING" => "Plan to Watch",
        "COMPLETED" => "Completed",
        "PAUSED" => "Paused",
        "DROPPED" => "Dropped",
        "REPEATING" => "Rewatching",
        _ => "Unknown",
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortKey {
    #[default]
    Title,
    Score,
    Progress,
    Updated,
    Airing,
}

impl SortKey {
    pub const ALL: [Self; 5] = [
        Self::Title,
        Self::Score,
        Self::Progress,
        Self::Updated,
        Self::Airing,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Score => "score",
            Self::Progress => "progress",
            Self::Updated => "updated",
            Self::Airing => "airing",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Score => "Score",
            Self::Progress => "Progress",
            Self::Updated => "Last updated",
            Self::Airing => "Next airing",
        }
    }

    pub fn default_desc(self) -> bool {
        !matches!(self, Self::Title | Self::Airing)
    }
}

impl TryFrom<&str> for SortKey {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|key| key.key() == value)
            .ok_or(())
    }
}

#[derive(Default)]
pub struct ListState {
    entries: HashMap<i64, ListEntry>,
    order: Vec<i64>,
    visible: Vec<i64>,
    pub filter: String,
    pub query: String,
    pub sort: SortKey,
    pub desc: bool,
    pub synced_at: Option<i64>,
    pub loaded: bool,
    pub loading: bool,
    pub syncing: bool,
    pub error: String,
    pub sync_error: String,
    pub step_error: String,
    next_airing_boundary: Option<i64>,
    revision: u64,
}

#[derive(Debug, Default)]
pub struct ListChange {
    pub order_changed: bool,
    pub time_boundary_crossed: bool,
    pub changed_ids: Vec<i64>,
}

impl ListState {
    pub fn new() -> Self {
        Self {
            filter: "CURRENT".into(),
            ..Self::default()
        }
    }

    pub fn clear_account(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.visible.clear();
        self.synced_at = None;
        self.loaded = false;
        self.loading = false;
        self.syncing = false;
        self.error.clear();
        self.sync_error.clear();
        self.step_error.clear();
        self.next_airing_boundary = None;
        self.revision += 1;
    }

    pub fn clear_scores(&mut self, now: i64) -> ListChange {
        let changed_ids = self
            .entries
            .values_mut()
            .filter_map(|entry| entry.score.take().map(|_| entry.media_id))
            .collect();
        let order_changed = if self.sort == SortKey::Score {
            let previous = self.visible.clone();
            self.rebuild(now);
            previous != self.visible
        } else {
            false
        };
        self.revision += 1;
        ListChange {
            order_changed,
            changed_ids,
            ..ListChange::default()
        }
    }

    pub fn entry(&self, id: i64) -> Option<&ListEntry> {
        self.entries.get(&id)
    }
    pub fn ordered_entries(&self) -> impl Iterator<Item = &ListEntry> {
        self.order.iter().filter_map(|id| self.entries.get(id))
    }
    pub fn visible(&self) -> &[i64] {
        &self.visible
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn tracked_episodes(&self) -> i128 {
        self.entries.values().map(|e| i128::from(e.progress)).sum()
    }
    pub fn count(&self, status: &str) -> usize {
        self.entries.values().filter(|e| e.status == status).count()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn airing_order_due(&self, now: i64) -> bool {
        self.sort == SortKey::Airing
            && self
                .next_airing_boundary
                .is_some_and(|boundary| boundary <= now)
    }

    pub fn replace(&mut self, incoming: Vec<ListEntry>, now: i64) {
        let mut next = HashMap::with_capacity(incoming.len());
        let mut order = Vec::with_capacity(incoming.len());
        for mut entry in incoming {
            if entry.media.is_none() {
                entry.media = self
                    .entries
                    .get(&entry.media_id)
                    .and_then(|old| old.media.clone());
            }
            order.push(entry.media_id);
            next.insert(entry.media_id, entry);
        }
        self.entries = next;
        self.order = order;
        self.loaded = true;
        self.revision += 1;
        self.rebuild(now);
    }

    pub fn apply_scoped(
        &mut self,
        ids: &[i64],
        values: Vec<Option<ListEntry>>,
        now: i64,
    ) -> ListChange {
        let needle = self.query.trim().to_lowercase();
        let time_boundary_crossed = self.airing_order_due(now);
        let mut order_may_change = time_boundary_crossed;
        let mut changed_ids = Vec::with_capacity(ids.len());
        for (id, value) in ids.iter().copied().zip(values) {
            match value {
                Some(mut entry) => {
                    if entry.media.is_none() {
                        entry.media = self.entries.get(&id).and_then(|old| old.media.clone());
                    }
                    let old = self.entries.get(&id);
                    let old_visible = old.is_some_and(|old| {
                        old.status == self.filter && matches_query(old, &needle)
                    });
                    let new_visible = entry.status == self.filter && matches_query(&entry, &needle);
                    order_may_change |= old_visible != new_visible
                        || old_visible
                            && old.is_some_and(|old| {
                                compare_entries(old, &entry, self.sort, self.desc, now)
                                    != Ordering::Equal
                            });
                    if !self.entries.contains_key(&id) {
                        self.order.push(id);
                    }
                    self.entries.insert(id, entry);
                    changed_ids.push(id);
                }
                None => {
                    if let Some(old) = self.entries.remove(&id) {
                        order_may_change |=
                            old.status == self.filter && matches_query(&old, &needle);
                        self.order.retain(|item| *item != id);
                        changed_ids.push(id);
                    }
                }
            }
        }
        self.revision += 1;
        let order_changed = if order_may_change {
            let previous = self.visible.clone();
            self.rebuild(now);
            previous != self.visible
        } else {
            false
        };
        ListChange {
            order_changed,
            time_boundary_crossed,
            changed_ids,
        }
    }

    pub fn apply_one(&mut self, entry: ListEntry, now: i64) -> ListChange {
        let id = entry.media_id;
        self.apply_scoped(&[id], vec![Some(entry)], now)
    }

    pub fn set_filter(&mut self, status: &str, now: i64) {
        if STATUSES.contains(&status) {
            self.filter = status.into();
            self.rebuild(now);
        }
    }

    pub fn set_query(&mut self, query: String, now: i64) {
        self.query = query;
        self.rebuild(now);
    }

    pub fn set_sort(&mut self, sort: SortKey, now: i64) {
        self.sort = sort;
        self.desc = sort.default_desc();
        self.rebuild(now);
    }

    pub fn set_desc(&mut self, desc: bool, now: i64) {
        self.desc = desc;
        self.rebuild(now);
    }

    pub fn rebuild(&mut self, now: i64) {
        let needle = self.query.trim().to_lowercase();
        self.visible = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                self.entries.get(id).is_some_and(|entry| {
                    entry.status == self.filter && matches_query(entry, &needle)
                })
            })
            .collect();
        self.next_airing_boundary = (self.sort == SortKey::Airing)
            .then(|| {
                self.visible
                    .iter()
                    .filter_map(|id| {
                        self.entries
                            .get(id)
                            .and_then(|entry| entry.media.as_ref())
                            .and_then(|media| media.next_airing_at)
                            .filter(|at| *at > now)
                    })
                    .min()
            })
            .flatten();
        let sort = self.sort;
        let desc = self.desc;
        let entries = &self.entries;
        if sort == SortKey::Title {
            let titles: HashMap<_, _> = self
                .visible
                .iter()
                .map(|id| (*id, list_title(&entries[id])))
                .collect();
            self.visible.sort_by(|a, b| {
                let order = compare_titles(&titles[a], &titles[b]);
                if desc { order.reverse() } else { order }
            });
        } else {
            self.visible
                .sort_by(|a, b| compare_entries(&entries[a], &entries[b], sort, desc, now));
        }
    }
}

fn matches_query(entry: &ListEntry, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Some(media) = entry.media.as_ref() else {
        return list_title(entry).contains(needle);
    };
    let mut titles = [
        &media.title_english,
        &media.title_romaji,
        &media.title_native,
    ]
    .into_iter()
    .flatten()
    .filter(|title| !title.trim().is_empty())
    .peekable();
    if titles.peek().is_none() {
        return list_title(entry).contains(needle);
    }
    titles.any(|title| title.to_lowercase().contains(needle))
}

pub(super) fn list_title(entry: &ListEntry) -> String {
    entry
        .media
        .as_ref()
        .map(Media::display_title)
        .unwrap_or_else(|| format!("#{}", entry.media_id))
}

fn title_cmp(a: &ListEntry, b: &ListEntry) -> Ordering {
    compare_titles(&list_title(a), &list_title(b))
}

pub(super) fn compare_titles(a: &str, b: &str) -> Ordering {
    static COLLATOR: LazyLock<CollatorBorrowed<'static>> =
        LazyLock::new(|| title_collator(sys_locale::get_locale().as_deref()));
    COLLATOR.compare(a, b)
}

fn title_collator(locale: Option<&str>) -> CollatorBorrowed<'static> {
    let mut preferences = locale
        .and_then(|locale| locale.parse::<Locale>().ok())
        .map(CollatorPreferences::from)
        .unwrap_or_default();
    preferences.numeric_ordering = Some(CollationNumericOrdering::True);
    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Primary);
    Collator::try_new(preferences, options).unwrap_or_else(|_| {
        let mut fallback = CollatorPreferences::default();
        fallback.numeric_ordering = Some(CollationNumericOrdering::True);
        Collator::try_new(fallback, options).expect("compiled root collation data")
    })
}

fn compare_entries(a: &ListEntry, b: &ListEntry, sort: SortKey, desc: bool, now: i64) -> Ordering {
    let numeric = |entry: &ListEntry| -> Option<f64> {
        match sort {
            SortKey::Title => None,
            SortKey::Score => entry.score.filter(|score| *score > 0.0),
            SortKey::Progress => Some(entry.progress as f64),
            SortKey::Updated => entry.updated_at.map(|at| at as f64),
            SortKey::Airing => entry
                .media
                .as_ref()
                .and_then(|m| m.next_airing_at)
                .filter(|at| *at > now)
                .map(|at| at as f64),
        }
    };
    if sort == SortKey::Title {
        let order = title_cmp(a, b);
        return if desc { order.reverse() } else { order };
    }
    match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => {
            let order = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            let order = if desc { order.reverse() } else { order };
            order.then_with(|| title_cmp(a, b))
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => title_cmp(a, b),
    }
}

#[derive(Clone, Debug)]
pub struct ProgressEdit {
    pub saved: i64,
    pub pending: i64,
    pub total: Option<i64>,
    pub remote_progress: Option<i64>,
    pub requires_review: bool,
    pub due: Option<Instant>,
    pub in_flight: bool,
    pub error: Option<String>,
    pub deleting: bool,
    pub identity: kurisu_core::EntryIdentity,
    pub remote_identity: Option<kurisu_core::EntryIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressReview {
    pub generation: u64,
    pub id: i64,
    pub progress: Option<i64>,
    pub identity: Option<kurisu_core::EntryIdentity>,
}

impl ProgressEdit {
    const CHANGED_PROGRESS: &str = "Progress changed on your list. Review before saving.";
    const MISSING_ENTRY: &str = "This entry is no longer on your list. Review the saved progress.";
    const REPLACED_ENTRY: &str = "AniList replaced this entry. Review before saving.";

    fn can_retry(&self) -> bool {
        !self.busy() && !self.requires_review && self.dirty() && self.error.is_some()
    }

    pub fn new(progress: i64, total: Option<i64>) -> Self {
        Self {
            saved: progress,
            pending: progress,
            total: total.filter(|total| *total > 0),
            remote_progress: None,
            requires_review: false,
            due: None,
            in_flight: false,
            error: None,
            deleting: false,
            identity: kurisu_core::EntryIdentity::Unverified,
            remote_identity: None,
        }
    }

    pub fn dirty(&self) -> bool {
        self.pending != self.saved
    }

    pub fn busy(&self) -> bool {
        self.in_flight || self.deleting
    }

    pub fn reopens_completed(&self, entry: &ListEntry) -> bool {
        entry.status == "COMPLETED"
            && self.dirty()
            && self
                .total
                .is_some_and(|total| total > 0 && self.pending < total)
    }

    pub fn step(&mut self, delta: i64, now: Instant) -> bool {
        if self.in_flight || self.deleting || self.requires_review {
            return false;
        }
        let limit = self.total.filter(|total| *total > 0).unwrap_or(9999);
        if delta > 0 && self.pending >= limit {
            return false;
        }
        let next = self.pending.saturating_add(delta).clamp(0, limit);
        if next == self.pending {
            return false;
        }
        self.pending = next;
        self.error = None;
        self.due = Some(
            if self.total.is_some_and(|total| total > 0 && next == total) {
                now
            } else {
                now + Duration::from_secs(3)
            },
        );
        true
    }

    pub fn absorb(&mut self, progress: i64, total: Option<i64>) {
        self.total = total.filter(|total| *total > 0);
        if self.deleting || self.in_flight {
            return;
        }
        if self.requires_review {
            if self.error.as_deref() == Some(Self::MISSING_ENTRY) {
                self.error = Some("This entry is back on your list. Review before saving.".into());
                self.due = None;
            } else if progress != self.saved && self.remote_progress != Some(progress) {
                self.error = Some(Self::CHANGED_PROGRESS.into());
                self.due = None;
            }
            self.remote_progress = Some(progress);
            return;
        }
        if self.error.is_some() {
            if progress != self.saved {
                self.remote_progress = Some(progress);
                self.requires_review = true;
                self.error = Some(Self::CHANGED_PROGRESS.into());
                self.due = None;
            }
            return;
        }
        if !self.dirty() {
            self.pending = progress;
            self.saved = progress;
            self.remote_progress = None;
            self.requires_review = false;
        } else if progress == self.pending {
            self.saved = progress;
            self.remote_progress = None;
            self.requires_review = false;
            self.error = None;
            self.due = None;
        } else if progress != self.saved {
            self.remote_progress = Some(progress);
            self.requires_review = true;
            self.error = Some(Self::CHANGED_PROGRESS.into());
            self.due = None;
        }
    }

    fn absorb_entry(&mut self, entry: &ListEntry) {
        let total = entry
            .media
            .as_ref()
            .and_then(|media| media.known_episodes());
        let identity = entry.identity();
        if self.busy() {
            self.total = total;
            return;
        }
        if identity != self.identity
            && !self.identity.matches(entry)
            && (self.dirty() || self.error.is_some() || self.requires_review)
        {
            self.total = total;
            self.remote_progress = Some(entry.progress);
            self.remote_identity = Some(identity);
            self.requires_review = true;
            self.error = Some(Self::REPLACED_ENTRY.into());
            self.due = None;
            return;
        }
        self.identity = identity.clone();
        self.absorb(entry.progress, total);
        self.remote_identity = self.requires_review.then_some(identity);
    }

    pub fn begin_save(&mut self) -> Option<(i64, i64)> {
        if !self.dirty()
            || self.in_flight
            || self.deleting
            || self.requires_review
            || self.error.is_some()
        {
            return None;
        }
        self.in_flight = true;
        self.due = None;
        Some((self.pending, self.saved))
    }

    pub fn finish_save(&mut self, result: Result<i64, String>) {
        self.in_flight = false;
        match result {
            Ok(progress) => {
                self.saved = progress;
                self.pending = progress;
                self.remote_progress = None;
                self.requires_review = false;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(error);
            }
        }
    }

    pub fn finish_conflicted_save(&mut self, entry: &ListEntry) {
        self.in_flight = false;
        self.due = None;
        self.requires_review = true;
        self.error = Some(Self::CHANGED_PROGRESS.into());
        self.absorb_entry(entry);
    }

    pub fn finish_superseded_save(&mut self, submitted: i64, progress: i64) {
        self.in_flight = false;
        self.due = None;
        if self.pending == submitted {
            self.pending = progress;
        } else if progress != submitted {
            self.absorb(progress, self.total);
            return;
        }
        self.saved = progress;
        if !self.requires_review {
            self.error = None;
            self.remote_progress = None;
        }
    }
}

#[derive(Default)]
pub struct ProgressRegistry {
    edits: HashMap<(u64, i64), ProgressEdit>,
    removals: HashSet<(u64, i64)>,
}

impl ProgressRegistry {
    pub fn take_generation_drafts(&mut self, generation: u64) -> HashMap<i64, ProgressEdit> {
        let mut drafts = HashMap::new();
        self.edits.retain(|(owner, id), edit| {
            if *owner != generation {
                return true;
            }
            if edit.dirty() || edit.error.is_some() || edit.in_flight {
                let mut saved = edit.clone();
                if saved.in_flight {
                    saved.error =
                        Some("Previous save outcome is unknown. Review and retry.".into());
                } else if saved.error.is_none() {
                    saved.error = Some(
                        "Progress was not saved before sign-in changed. Review and retry.".into(),
                    );
                }
                saved.in_flight = false;
                saved.deleting = false;
                saved.due = None;
                saved.requires_review = true;
                saved.remote_progress = None;
                saved.remote_identity = None;
                drafts.insert(*id, saved);
            }
            false
        });
        self.removals.retain(|(owner, _)| *owner != generation);
        drafts
    }

    pub fn restore_generation(&mut self, generation: u64, drafts: HashMap<i64, ProgressEdit>) {
        for (id, draft) in drafts {
            self.edits.insert((generation, id), draft);
        }
    }

    pub fn reconcile_entries(&mut self, generation: u64, list: &ListState) {
        self.reconcile_list(generation, list, None);
    }

    pub fn reconcile_scoped_entries(&mut self, generation: u64, list: &ListState, ids: &[i64]) {
        self.reconcile_list(generation, list, Some(ids));
    }

    fn reconcile_list(&mut self, generation: u64, list: &ListState, ids: Option<&[i64]>) {
        let mut settled_missing = Vec::new();
        for (&(owner, id), edit) in &mut self.edits {
            if owner != generation || ids.is_some_and(|ids| !ids.contains(&id)) {
                continue;
            }
            let Some(entry) = list.entry(id) else {
                if edit.deleting {
                    continue;
                }
                if edit.dirty() || edit.in_flight || edit.error.is_some() || edit.requires_review {
                    edit.error = Some(ProgressEdit::MISSING_ENTRY.into());
                    edit.requires_review = true;
                    edit.remote_progress = None;
                    edit.remote_identity = None;
                    edit.due = None;
                } else {
                    settled_missing.push((owner, id));
                }
                continue;
            };
            edit.absorb_entry(entry);
        }
        for key in settled_missing {
            self.edits.remove(&key);
        }
    }

    pub fn reconcile_entry(&mut self, generation: u64, entry: &ListEntry) {
        if let Some(edit) = self.edits.get_mut(&(generation, entry.media_id)) {
            edit.absorb_entry(entry);
        }
    }

    pub fn review(&self, generation: u64, id: i64) -> Option<ProgressReview> {
        let edit = self
            .edit(generation, id)
            .filter(|edit| edit.requires_review)?;
        Some(ProgressReview {
            generation,
            id,
            progress: edit.remote_progress,
            identity: edit.remote_identity.clone(),
        })
    }

    pub fn first_review(&self, generation: u64) -> Option<ProgressReview> {
        self.edits
            .iter()
            .filter_map(|(&(owner, id), edit)| {
                (owner == generation && edit.requires_review).then_some(id)
            })
            .min()
            .and_then(|id| self.review(generation, id))
    }

    pub fn resolve_review(
        &mut self,
        generation: u64,
        id: i64,
        observed: Option<i64>,
        keep_local: bool,
    ) -> bool {
        let key = (generation, id);
        let Some(edit) = self
            .edits
            .get_mut(&key)
            .filter(|edit| edit.requires_review && !edit.busy())
        else {
            return false;
        };
        if edit.remote_progress != observed || keep_local && observed.is_none() {
            return false;
        }
        if let Some(progress) = observed {
            edit.saved = progress;
            if let Some(identity) = edit.remote_identity.take() {
                edit.identity = identity;
            }
            if !keep_local {
                edit.pending = progress;
            }
            edit.requires_review = false;
            edit.remote_progress = None;
            edit.error = None;
            edit.due = None;
        } else {
            self.edits.remove(&key);
        }
        true
    }

    pub fn retry_error(&mut self, generation: u64, id: i64) -> bool {
        let Some(edit) = self.edits.get_mut(&(generation, id)) else {
            return false;
        };
        if !edit.can_retry() {
            return false;
        }
        edit.error = None;
        true
    }

    pub fn has_retryable_error(&self, generation: u64) -> bool {
        self.edits
            .iter()
            .any(|(&(owner, _), edit)| owner == generation && edit.can_retry())
    }

    pub fn entry(&mut self, generation: u64, entry: &ListEntry) -> &mut ProgressEdit {
        let edit = self
            .edits
            .entry((generation, entry.media_id))
            .or_insert_with(|| {
                let mut edit = ProgressEdit::new(
                    entry.progress,
                    entry.media.as_ref().and_then(|m| m.known_episodes()),
                );
                edit.identity = entry.identity();
                edit
            });
        edit.absorb_entry(entry);
        edit
    }

    pub fn edit(&self, generation: u64, id: i64) -> Option<&ProgressEdit> {
        self.edits.get(&(generation, id))
    }
    pub fn edit_mut(&mut self, generation: u64, id: i64) -> Option<&mut ProgressEdit> {
        self.edits.get_mut(&(generation, id))
    }
    pub fn due(&self, generation: u64, now: Instant) -> Vec<i64> {
        self.edits
            .iter()
            .filter_map(|(&(g, id), edit)| {
                (g == generation && edit.due.is_some_and(|due| due <= now) && !edit.in_flight)
                    .then_some(id)
            })
            .collect()
    }
    pub fn dirty_ids(&self, generation: u64) -> Vec<i64> {
        self.edits
            .iter()
            .filter_map(|(&(g, id), edit)| {
                (g == generation && edit.dirty() && !edit.deleting).then_some(id)
            })
            .collect()
    }
    pub fn has_in_flight(&self, generation: u64) -> bool {
        self.removals.iter().any(|&(g, _)| g == generation)
            || self
                .edits
                .iter()
                .any(|(&(g, _), edit)| g == generation && edit.busy())
    }

    pub fn busy(&self, generation: u64, id: i64) -> bool {
        self.deleting(generation, id)
            || self.edit(generation, id).is_some_and(|edit| edit.in_flight)
    }

    pub fn deleting(&self, generation: u64, id: i64) -> bool {
        self.removals.contains(&(generation, id))
            || self.edit(generation, id).is_some_and(|edit| edit.deleting)
    }

    pub fn blocks_add(&self, generation: u64, id: i64) -> bool {
        self.busy(generation, id)
            || self
                .edit(generation, id)
                .is_some_and(|edit| edit.requires_review || edit.dirty() || edit.error.is_some())
    }
    pub fn first_error(&self, generation: u64) -> Option<&str> {
        self.errors(generation).next().map(|(_, error)| error)
    }

    pub fn first_settled_error(&self, generation: u64) -> Option<&str> {
        self.errors(generation)
            .find_map(|(edit, error)| (!edit.busy()).then_some(error))
    }

    fn errors(&self, generation: u64) -> impl Iterator<Item = (&ProgressEdit, &str)> {
        self.edits.iter().filter_map(move |(&(owner, _), edit)| {
            if owner != generation || !edit.dirty() && !edit.requires_review {
                return None;
            }
            let error = edit.error.as_deref().or(edit
                .requires_review
                .then_some("Review saved progress before continuing."))?;
            Some((edit, error))
        })
    }
    pub fn begin_delete(&mut self, generation: u64, id: i64) -> bool {
        let key = (generation, id);
        if self.busy(generation, id) {
            return false;
        }
        self.removals.insert(key);
        if let Some(edit) = self.edits.get_mut(&key) {
            edit.deleting = true;
            edit.due = None;
        }
        true
    }
    pub fn finish_delete(&mut self, generation: u64, id: i64, success: bool) {
        let key = (generation, id);
        if !self.removals.remove(&key) {
            return;
        }
        if success {
            self.edits.remove(&key);
        } else if let Some(edit) = self.edits.get_mut(&key) {
            edit.deleting = false;
        }
    }
    pub fn clear_generation(&mut self, generation: u64) {
        self.edits.retain(|(g, _), _| *g != generation);
        self.removals.retain(|(g, _)| *g != generation);
    }
    pub fn discard_generation(&mut self, generation: u64) {
        self.edits
            .retain(|&(owner, _), edit| owner != generation || edit.busy());
    }
}

#[derive(Clone, Debug, Default)]
pub struct EntryPatch {
    pub status: Option<String>,
    pub progress: Option<i64>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
}

impl EntryPatch {
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.progress.is_none()
            && self.score.is_none()
            && self.repeat.is_none()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DetailsPatch {
    pub notes: Option<String>,
    pub started_at: Option<FuzzyDate>,
    pub completed_at: Option<FuzzyDate>,
    pub custom_lists: Option<Vec<String>>,
}

impl DetailsPatch {
    pub fn is_empty(&self) -> bool {
        self.notes.is_none()
            && self.started_at.is_none()
            && self.completed_at.is_none()
            && self.custom_lists.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct EditorState {
    pub media_id: i64,
    pub media: Option<Media>,
    snapshot: ListEntry,
    pub status: String,
    pub progress: i64,
    pub score: Option<f64>,
    pub displayed_format: Option<String>,
    score_format_revision: u64,
    pub score_cleared: bool,
    pub repeat: i64,
    status_before_completion: Option<String>,
    status_chosen: bool,
    completion_pending: bool,
    progress_edited: bool,
    pub details: Option<EntryDetails>,
    pub notes: String,
    pub started_at: FuzzyDate,
    pub completed_at: FuzzyDate,
    pub custom_lists: Vec<String>,
    pub busy: bool,
    pub confirming_delete: bool,
    pub review_progress: Option<i64>,
    pub details_error: String,
    pub error: String,
}

impl EditorState {
    pub fn new(entry: ListEntry) -> Self {
        Self {
            media_id: entry.media_id,
            media: entry.media.clone(),
            status: entry.status.clone(),
            progress: entry.progress,
            score: entry.score,
            displayed_format: None,
            score_format_revision: 0,
            score_cleared: false,
            repeat: entry.repeat,
            snapshot: entry,
            status_before_completion: None,
            status_chosen: false,
            completion_pending: false,
            progress_edited: false,
            details: None,
            notes: String::new(),
            started_at: FuzzyDate::default(),
            completed_at: FuzzyDate::default(),
            custom_lists: Vec::new(),
            busy: false,
            confirming_delete: false,
            review_progress: None,
            details_error: String::new(),
            error: String::new(),
        }
    }

    pub fn total(&self) -> Option<i64> {
        self.media
            .as_ref()
            .and_then(|m| m.episodes)
            .filter(|total| *total > 0)
    }

    pub fn refresh_entry_progress(
        &mut self,
        entry: &ListEntry,
        progress_input_current: bool,
    ) -> bool {
        self.adopt_queued_entry(entry);
        if !self.matches_entry(entry) {
            return false;
        }
        let changed_total = entry.media.as_ref().and_then(|media| {
            let total = media
                .episodes
                .filter(|total| *total > 0 && Some(*total) != self.total())?;
            self.media.get_or_insert_with(|| media.clone()).episodes = Some(total);
            Some(total)
        });
        if let Some(total) = changed_total
            && self.status_chosen
            && self.status == "COMPLETED"
        {
            self.progress = total;
            self.completion_pending = true;
            return true;
        }
        if !progress_input_current {
            return false;
        }
        let previous = self.progress;
        if self.progress_edited || self.status_before_completion.is_some() {
            if changed_total.is_some() {
                self.update_progress_status();
            }
        } else if !self.status_chosen
            && !self.completion_pending
            && !self.busy
            && self.review_progress.is_none()
            && self.progress == self.snapshot.progress
            && self.status == self.snapshot.status
        {
            self.progress = entry.progress;
            self.snapshot.progress = entry.progress;
            self.status.clone_from(&entry.status);
            self.snapshot.status.clone_from(&entry.status);
        }
        self.progress != previous
    }

    pub fn choose_status(&mut self, status: &str) {
        if !STATUSES.contains(&status) {
            return;
        }
        self.status = status.into();
        self.status_chosen = true;
        self.status_before_completion = None;
        self.completion_pending = false;
        if status == "COMPLETED"
            && let Some(total) = self.total()
        {
            self.progress = total;
            self.completion_pending = true;
        }
    }

    pub fn edit_progress(&mut self, progress: i64) {
        self.progress_edited = true;
        self.set_progress(progress);
    }

    pub fn set_progress(&mut self, progress: i64) {
        if self.progress == progress {
            return;
        }
        // Overshooting can undo completion inferred from a progress edit.
        let progress = if self.status == "COMPLETED"
            && self.status_before_completion.is_none()
            && let Some(total) = self.total().filter(|total| *total > 0)
        {
            progress.min(total)
        } else {
            progress
        };
        if self.progress == progress {
            return;
        }
        self.progress = progress;
        self.update_progress_status();
    }

    fn update_progress_status(&mut self) {
        if self.status == "COMPLETED"
            && self
                .total()
                .is_some_and(|total| total > 0 && self.progress < total)
        {
            self.status = self
                .status_before_completion
                .take()
                .unwrap_or_else(|| "CURRENT".into());
            self.status_chosen = false;
            self.completion_pending = false;
            return;
        }
        if self.status_chosen {
            return;
        }
        if self
            .total()
            .is_some_and(|total| total > 0 && self.progress == total)
        {
            if self.status != "COMPLETED" {
                if self.snapshot.status != "COMPLETED" {
                    self.status_before_completion = Some(self.status.clone());
                }
                self.status = "COMPLETED".into();
            }
        } else if let Some(previous) = self.status_before_completion.take() {
            self.status = previous;
        }
    }

    pub fn set_score(&mut self, value: Option<f64>, format: Option<&str>) {
        self.score = value.map(|v| clamp_score(v, format));
        if self.score.is_some() {
            self.score_cleared = false;
        }
    }

    pub fn apply_details(&mut self, details: EntryDetails) {
        self.notes = details.notes.clone();
        self.started_at = details.started_at.clone().unwrap_or_default();
        self.completed_at = details.completed_at.clone().unwrap_or_default();
        self.custom_lists = details.custom_lists.clone();
        self.details = Some(details);
        self.details_error.clear();
    }

    pub fn accept_details(&mut self, submitted: &Self, details: EntryDetails) {
        if self.notes == submitted.notes {
            self.notes = details.notes.clone();
        }
        accept_date(
            &mut self.started_at,
            &submitted.started_at,
            details.started_at.as_ref(),
        );
        accept_date(
            &mut self.completed_at,
            &submitted.completed_at,
            details.completed_at.as_ref(),
        );
        if self.custom_lists == submitted.custom_lists {
            self.custom_lists = details.custom_lists.clone();
        }
        self.details = Some(details);
        self.details_error.clear();
    }

    pub fn score_available(&self) -> bool {
        matches!(
            self.displayed_format.as_deref(),
            Some("POINT_100" | "POINT_10_DECIMAL" | "POINT_10" | "POINT_5" | "POINT_3")
        )
    }

    pub fn review_progress(&mut self) {
        if let Some(current) = self.review_progress.take() {
            if self.progress == self.snapshot.progress
                && !self.progress_edited
                && !self.completion_pending
            {
                self.progress = current;
            }
            self.snapshot.progress = current;
            self.error = "Progress refreshed. Review your draft before saving.".into();
        }
    }

    fn score_patch(&self, score_available: bool) -> Option<f64> {
        if self.score_cleared {
            return Some(0.0);
        }
        let score = self.score.or(self.snapshot.score.map(|_| 0.0))?;
        (score_available && self.snapshot.score != Some(score)).then_some(score)
    }

    pub fn basic_patch(&self, score_available: bool) -> EntryPatch {
        let progress_touched = self.progress != self.snapshot.progress
            || self.status_before_completion.is_some()
            || self.completion_pending;
        EntryPatch {
            status: (self.status_before_completion.is_none()
                && (self.status != self.snapshot.status || self.status_chosen && progress_touched))
                .then(|| self.status.clone()),
            // Core distinguishes exact completion from overshoot before clamping.
            progress: progress_touched.then_some(self.progress),
            score: self.score_patch(score_available),
            repeat: (self.repeat != self.snapshot.repeat).then_some(self.repeat.max(0)),
        }
    }

    pub(super) fn chosen_status(&self) -> Option<&str> {
        self.status_chosen.then_some(self.status.as_str())
    }

    pub fn is_dirty(&self, score_available: bool) -> bool {
        !self.basic_patch(score_available).is_empty() || !self.details_patch().is_empty()
    }

    pub fn saved_progress(&self) -> i64 {
        self.snapshot.progress
    }

    pub fn saved_identity(&self) -> kurisu_core::EntryIdentity {
        self.snapshot.identity()
    }

    pub fn adopt_queued_entry(&mut self, entry: &ListEntry) {
        if self.media_id == entry.media_id
            && self.snapshot.id.is_none()
            && entry.id.is_some()
            && self.snapshot.queued_creation.is_some()
            && self.snapshot.queued_creation == entry.queued_creation
        {
            self.snapshot.id = entry.id;
        }
    }

    pub fn matches_entry(&self, entry: &ListEntry) -> bool {
        self.media_id == entry.media_id && self.snapshot.identity() == entry.identity()
    }

    pub fn accept_progress(&mut self, entry: &ListEntry) {
        self.snapshot.progress = entry.progress;
        self.completion_pending = false;
        self.progress_edited = false;
    }

    pub fn details_patch(&self) -> DetailsPatch {
        let Some(old) = self.details.as_ref() else {
            return DetailsPatch::default();
        };
        let mut old_lists = old.custom_lists.clone();
        let mut new_lists = self.custom_lists.clone();
        old_lists.sort();
        new_lists.sort();
        DetailsPatch {
            notes: (self.notes != old.notes).then(|| self.notes.clone()),
            started_at: (!same_date(&self.started_at, old.started_at.as_ref()))
                .then(|| self.started_at.clone()),
            completed_at: (!same_date(&self.completed_at, old.completed_at.as_ref()))
                .then(|| self.completed_at.clone()),
            custom_lists: (new_lists != old_lists).then(|| self.custom_lists.clone()),
        }
    }

    pub fn refresh_score_format(&mut self, format: Option<String>, input: Option<&str>) -> bool {
        if self.displayed_format == format {
            return false;
        }
        if self.score_available() {
            self.score_cleared = match input.map(str::trim) {
                Some("") => {
                    self.score_cleared || self.snapshot.score.is_some_and(|score| score > 0.0)
                }
                Some(input) => input.parse::<f64>() == Ok(0.0) && self.snapshot.score != Some(0.0),
                None => self.score_patch(true) == Some(0.0),
            };
        }
        self.displayed_format = format;
        self.score_format_revision = self.score_format_revision.wrapping_add(1);
        self.score = None;
        self.snapshot.score = None;
        if !self.score_cleared {
            self.error = "Score format changed. Re-enter the score before saving.".into();
        }
        true
    }

    pub fn score_format_matches(&self, submitted: &Self) -> bool {
        self.score_format_revision == submitted.score_format_revision
            && self.displayed_format == submitted.displayed_format
    }

    pub fn accept_basic(&mut self, submitted: &Self, mut entry: ListEntry) {
        if entry.id.is_none()
            && entry.queued_creation.is_some()
            && entry.queued_creation == self.snapshot.queued_creation
        {
            entry.id = self.snapshot.id;
        }
        if self.status == submitted.status
            && self.status_chosen == submitted.status_chosen
            && self.status_before_completion == submitted.status_before_completion
        {
            self.status = entry.status.clone();
            self.status_before_completion = None;
            self.status_chosen = false;
        }
        if self.progress == submitted.progress
            && self.completion_pending == submitted.completion_pending
            && self.progress_edited == submitted.progress_edited
        {
            self.progress = entry.progress;
            self.completion_pending = false;
            self.progress_edited = false;
        }
        if !self.score_format_matches(submitted) {
            // A format refresh invalidates both the draft score and its baseline.
            if self.score_cleared
                && entry.score == Some(0.0)
                && submitted.score_patch(true) == Some(0.0)
            {
                self.score_cleared = false;
            }
            entry.score = self.snapshot.score;
        } else if self.score == submitted.score && self.score_cleared == submitted.score_cleared {
            self.score = entry.score;
            self.score_cleared = false;
        }
        if self.repeat == submitted.repeat {
            self.repeat = entry.repeat;
        }
        self.snapshot = entry;
    }
}

fn accept_date(current: &mut FuzzyDate, submitted: &FuzzyDate, accepted: Option<&FuzzyDate>) {
    let accepted = accepted.cloned().unwrap_or_default();
    if current.year == submitted.year {
        current.year = accepted.year;
    }
    if current.month == submitted.month {
        current.month = accepted.month;
    }
    if current.day == submitted.day {
        current.day = accepted.day;
    }
}

fn same_date(left: &FuzzyDate, right: Option<&FuzzyDate>) -> bool {
    let empty = FuzzyDate::default();
    left == right.unwrap_or(&empty)
}

pub fn clamp_score(score: f64, format: Option<&str>) -> f64 {
    if !score.is_finite() {
        return 0.0;
    }
    let max = match format {
        Some("POINT_3") => 3.0,
        Some("POINT_5") => 5.0,
        Some("POINT_10") => 10.0,
        Some("POINT_10_DECIMAL") => return (score.clamp(0.0, 10.0) * 10.0).round() / 10.0,
        _ => 100.0,
    };
    score.clamp(0.0, max).round()
}

#[derive(Default)]
pub struct DetailState {
    pub media_id: Option<i64>,
    pub detail: Option<MediaDetail>,
    pub entry: Option<ListEntry>,
    pub entry_available: bool,
    pub recommendations: Vec<Media>,
    pub detail_error: String,
    pub entry_error: String,
    pub recommendation_error: String,
    pub recommendations_loading: bool,
    pub description_expanded: bool,
    pub loading: bool,
    request: u64,
    pub entry_revision: u64,
}

impl DetailState {
    pub fn start(&mut self, media_id: i64) -> u64 {
        self.request += 1;
        self.media_id = Some(media_id);
        self.detail = None;
        self.entry = None;
        self.entry_available = false;
        self.recommendations.clear();
        self.detail_error.clear();
        self.entry_error.clear();
        self.recommendation_error.clear();
        self.recommendations_loading = true;
        self.description_expanded = false;
        self.loading = true;
        self.request
    }

    pub fn request(&self) -> u64 {
        self.request
    }
    pub fn current(&self, media_id: i64, request: u64) -> bool {
        self.media_id == Some(media_id) && self.request == request
    }

    pub fn reconcile_entry(&mut self, entry: Option<ListEntry>) {
        self.entry_revision += 1;
        self.entry = entry;
        self.entry_available = true;
        self.entry_error.clear();
    }

    pub fn finish_detail(
        &mut self,
        media_id: i64,
        request: u64,
        result: Result<MediaDetail, String>,
    ) {
        if !self.current(media_id, request) {
            return;
        }
        match result {
            Ok(detail) => self.detail = Some(detail),
            Err(error) => self.detail_error = error,
        }
        self.loading = false;
    }

    pub fn finish_entry(
        &mut self,
        media_id: i64,
        request: u64,
        result: Result<Option<ListEntry>, String>,
    ) {
        if !self.current(media_id, request) {
            return;
        }
        match result {
            Ok(entry) => {
                self.entry = entry;
                self.entry_available = true;
                self.entry_error.clear();
            }
            Err(error) => self.entry_error = error,
        }
    }

    pub fn finish_recommendations(
        &mut self,
        media_id: i64,
        request: u64,
        result: Result<Vec<Media>, String>,
    ) {
        if !self.current(media_id, request) {
            return;
        }
        self.recommendations_loading = false;
        self.recommendation_error.clear();
        match result {
            Ok(items) => self.recommendations = items,
            Err(error) => self.recommendation_error = error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_episode_index_matches_first_file_in_scan_order() {
        let files: Vec<_> = (0..25_000)
            .map(|index| LibraryFile {
                path: format!("/synthetic/{index}.mkv"),
                media_id: (index % 7 != 0).then_some(index % 101 - 1),
                episode: (index % 11 != 0).then_some(index % 23 - 1),
                ..Default::default()
            })
            .collect();
        let mut library = LibraryFiles::default();
        library.replace(files.clone());
        for media_id in -1..=101 {
            for episode in -1..=23 {
                let expected = files
                    .iter()
                    .find(|file| file.media_id == Some(media_id) && file.episode == Some(episode));
                assert_eq!(
                    library.episode(media_id, episode).map(|file| &file.path),
                    expected.map(|file| &file.path),
                    "media {media_id}, episode {episode}"
                );
            }
        }
        assert_eq!(
            library.iter().map(|file| &file.path).collect::<Vec<_>>(),
            files.iter().map(|file| &file.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn library_episode_index_tracks_replacements_and_clear() {
        let file = |path: &str, media_id, episode| LibraryFile {
            path: path.into(),
            media_id,
            episode,
            ..Default::default()
        };
        let mut library = LibraryFiles::default();
        assert!(library.episode(7, 2).is_none());
        let first = file("first.mkv", Some(7), Some(2));
        let second = file("second.mkv", Some(7), Some(2));
        library.replace(vec![first.clone(), second.clone()]);
        assert_eq!(library.episode(7, 2).unwrap().path, "first.mkv");
        library.replace(vec![second.clone(), first]);
        assert_eq!(library.episode(7, 2).unwrap().path, "second.mkv");
        library.replace(vec![second]);
        assert_eq!(library.episode(7, 2).unwrap().path, "second.mkv");
        library.replace(vec![file("renamed.mkv", Some(7), Some(2))]);
        assert_eq!(library.episode(7, 2).unwrap().path, "renamed.mkv");
        library.replace(vec![file("renamed.mkv", Some(8), Some(2))]);
        assert!(library.episode(7, 2).is_none());
        assert!(library.episode(8, 2).is_some());
        library.replace(vec![file("renamed.mkv", Some(8), Some(1))]);
        assert!(library.episode(8, 2).is_none());
        assert!(library.episode(8, 1).is_some());
        library.replace(vec![file("renamed.mkv", None, Some(1))]);
        assert!(library.episode(8, 1).is_none());
        library.replace(vec![file("renamed.mkv", Some(8), None)]);
        assert!(library.episode(8, 1).is_none());
        library.replace(Vec::new());
        assert_eq!(library.iter().len(), 0);
        assert!(library.episode(8, 1).is_none());
        library.replace(vec![file("fresh.mkv", Some(8), Some(1))]);
        library.clear();
        assert_eq!(library.iter().len(), 0);
        assert!(library.episode(8, 1).is_none());
    }

    fn assert_full_rebuild_equivalent(list: &ListState, now: i64) {
        let mut reference = ListState::new();
        reference.entries = list.entries.clone();
        reference.order = list.order.clone();
        reference.filter = list.filter.clone();
        reference.query = list.query.clone();
        reference.sort = list.sort;
        reference.desc = list.desc;
        reference.rebuild(now);
        assert_eq!(list.visible, reference.visible);
        assert_eq!(list.next_airing_boundary, reference.next_airing_boundary);
    }

    fn apply_and_compare(list: &mut ListState, id: i64, value: Option<ListEntry>, now: i64) {
        let old_visible = list.visible.clone();
        let revision = list.revision();
        let change = list.apply_scoped(&[id], vec![value], now);
        assert_eq!(list.revision(), revision + 1);
        assert_eq!(change.order_changed, old_visible != list.visible);
        assert_full_rebuild_equivalent(list, now);
    }

    fn entry(id: i64, title: &str, status: &str, progress: i64, total: Option<i64>) -> ListEntry {
        ListEntry {
            media_id: id,
            status: status.into(),
            progress,
            media: Some(Media {
                id,
                title_english: Some(title.into()),
                episodes: total,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn list_titles_and_sorting_fall_back_past_blank_translations() {
        let mut translated = entry(1, " \t\u{3000}", "CURRENT", 0, None);
        translated.media.as_mut().unwrap().title_romaji = Some("Zebra".into());
        let restored: ListEntry =
            serde_json::from_str(&serde_json::to_string(&translated).unwrap()).unwrap();
        assert_eq!(list_title(&restored), "Zebra");
        let mut list = ListState::new();
        list.replace(vec![restored, entry(2, "Alpha", "CURRENT", 0, None)], 0);
        assert_eq!(list.visible(), &[2, 1]);
        translated.media.as_mut().unwrap().title_romaji = Some("\n".into());
        translated.media.as_mut().unwrap().title_native = Some("原題".into());
        assert_eq!(list_title(&translated), "原題");
        translated.media.as_mut().unwrap().title_native = None;
        assert_eq!(list_title(&translated), "#1");
    }

    #[test]
    fn list_search_matches_displayed_media_id_fallback() {
        let mut titled = entry(43, "English", "CURRENT", 0, None);
        titled.media.as_mut().unwrap().title_romaji = Some("Romaji".into());
        let mut missing = entry(44, "", "CURRENT", 0, None);
        missing.media = None;
        let mut list = ListState::new();
        list.replace(
            vec![entry(42, " \t", "CURRENT", 0, None), titled, missing],
            0,
        );
        for (query, expected) in [
            ("#42", &[42][..]),
            ("#44", &[44]),
            ("#43", &[]),
            ("romaji", &[43]),
        ] {
            list.set_query(query.into(), 0);
            assert_eq!(list.visible(), expected);
        }
    }

    #[test]
    fn title_collation_uses_locale_base_letters_and_decimal_numbers() {
        let english = title_collator(Some("en-US"));
        for (a, b) in [
            ("Éclair", "eclair"),
            ("E\u{301}clair", "Eclair"),
            ("Show 02", "Show ٢"),
        ] {
            assert_eq!(english.compare(a, b), Ordering::Equal);
        }
        assert_eq!(english.compare("Episode ٢", "Episode 10"), Ordering::Less);
        assert_eq!(english.compare("Öland", "Zebra"), Ordering::Less);
        let swedish = title_collator(Some("sv-SE"));
        assert_eq!(swedish.compare("Öland", "Zebra"), Ordering::Greater);
        let turkish = title_collator(Some("tr-TR"));
        assert_eq!(turkish.compare("I", "ı"), Ordering::Equal);
        assert_eq!(turkish.compare("İ", "i"), Ordering::Equal);
        assert_ne!(turkish.compare("I", "i"), Ordering::Equal);
        assert_eq!(
            title_collator(Some("C.UTF-8")).compare("Show 2", "Show 10"),
            Ordering::Less
        );
    }

    #[test]
    fn collection_title_collation_keeps_equal_titles_in_input_order() {
        let mut list = ListState::new();
        list.replace(
            ["Éclair", "Eclair", "Episode 10", "Episode 2", "Zebra"]
                .into_iter()
                .enumerate()
                .map(|(index, title)| entry(index as i64 + 1, title, "CURRENT", 0, None))
                .collect(),
            0,
        );
        assert_eq!(list.visible(), &[1, 2, 4, 3, 5]);
        list.set_desc(true, 0);
        assert_eq!(list.visible(), &[5, 3, 4, 1, 2]);
        list.set_sort(SortKey::Score, 0);
        assert_eq!(list.visible(), &[1, 2, 4, 3, 5]);
        let mut changed = list.entry(2).unwrap().clone();
        changed.media.as_mut().unwrap().title_english = Some("e\u{301}clair".into());
        apply_and_compare(&mut list, 2, Some(changed), 0);
        assert_eq!(list.visible(), &[1, 2, 4, 3, 5]);
    }

    #[test]
    fn collection_filters_all_titles_and_keeps_null_airing_last() {
        let mut list = ListState::new();
        let mut first = entry(1, "Show 10", "CURRENT", 2, None);
        first.media.as_mut().unwrap().title_native = Some("原作".into());
        first.media.as_mut().unwrap().next_airing_at = Some(10);
        let mut second = entry(2, "Show 2", "CURRENT", 0, None);
        second.media.as_mut().unwrap().next_airing_at = Some(200);
        list.replace(vec![first, second], 100);
        assert_eq!(list.visible(), &[2, 1]);
        list.set_sort(SortKey::Airing, 100);
        assert_eq!(list.visible(), &[2, 1]);
        list.set_desc(true, 100);
        assert_eq!(list.visible(), &[2, 1]);
        list.set_query("原作".into(), 100);
        assert_eq!(list.visible(), &[1]);
    }

    #[test]
    fn incremental_scoped_edits_match_full_rebuild_for_every_sort_and_direction() {
        for sort in SortKey::ALL {
            for desc in [false, true] {
                let mut list = ListState::new();
                let mut first = entry(1, "Show 2", "CURRENT", 2, Some(24));
                first.score = Some(70.);
                first.updated_at = Some(40);
                first.media.as_mut().unwrap().next_airing_at = Some(140);
                let mut second = entry(2, "Show 10", "CURRENT", 4, Some(24));
                second.updated_at = None;
                let mut third = entry(3, "夜空 2", "CURRENT", 0, Some(24));
                third.score = Some(90.);
                third.media.as_mut().unwrap().next_airing_at = Some(250);
                let hidden = entry(4, "Hidden", "PLANNING", 0, Some(24));
                list.replace(vec![first, second, third, hidden], 100);
                list.set_sort(sort, 100);
                list.set_desc(desc, 100);

                let mut changed = list.entry(1).unwrap().clone();
                changed.progress = 9;
                let change = list.apply_scoped(&[1], vec![Some(changed)], 100);
                if sort == SortKey::Title {
                    assert!(!change.order_changed);
                }
                assert_eq!(change.changed_ids, [1]);
                assert_full_rebuild_equivalent(&list, 100);

                let mut changed = list.entry(2).unwrap().clone();
                changed.score = Some(55.);
                apply_and_compare(&mut list, 2, Some(changed), 100);
                let mut changed = list.entry(3).unwrap().clone();
                changed.media.as_mut().unwrap().title_english = Some("夜空 10".into());
                apply_and_compare(&mut list, 3, Some(changed), 100);
                let mut changed = list.entry(2).unwrap().clone();
                changed.updated_at = Some(80);
                apply_and_compare(&mut list, 2, Some(changed), 100);
                let mut changed = list.entry(1).unwrap().clone();
                changed.media.as_mut().unwrap().next_airing_at = Some(90);
                apply_and_compare(&mut list, 1, Some(changed), 100);
                let mut changed = list.entry(4).unwrap().clone();
                changed.status = "CURRENT".into();
                apply_and_compare(&mut list, 4, Some(changed), 100);
                apply_and_compare(
                    &mut list,
                    5,
                    Some(entry(5, "Show 02", "CURRENT", 1, None)),
                    100,
                );
                apply_and_compare(&mut list, 3, None, 100);
                let mut changed = list.entry(2).unwrap().clone();
                changed.media = None;
                apply_and_compare(&mut list, 2, Some(changed), 100);
                assert_eq!(
                    list.entry(2)
                        .unwrap()
                        .media
                        .as_ref()
                        .unwrap()
                        .display_title(),
                    "Show 10"
                );
            }
        }
    }

    #[test]
    fn incremental_query_membership_uses_all_title_variants() {
        let mut list = ListState::new();
        let mut item = entry(1, "English", "CURRENT", 1, None);
        item.media.as_mut().unwrap().title_native = Some("夜空の庭".into());
        list.replace(vec![item.clone()], 100);
        list.set_query("夜空".into(), 100);
        assert_eq!(list.visible(), &[1]);
        item.media.as_mut().unwrap().title_native = Some("別の庭".into());
        apply_and_compare(&mut list, 1, Some(item), 100);
        assert!(list.visible().is_empty());
    }

    #[test]
    fn scoped_updates_preserve_other_media_and_identity() {
        let mut list = ListState::new();
        list.replace(
            vec![
                entry(1, "A", "CURRENT", 0, None),
                entry(2, "B", "CURRENT", 0, None),
            ],
            0,
        );
        list.apply_scoped(&[2], vec![Some(entry(2, "B", "CURRENT", 3, None))], 0);
        assert_eq!(list.entry(1).unwrap().progress, 0);
        assert_eq!(list.entry(2).unwrap().progress, 3);
        list.apply_one(
            ListEntry {
                media_id: 2,
                status: "CURRENT".into(),
                progress: 4,
                ..Default::default()
            },
            0,
        );
        assert_eq!(
            list.entry(2)
                .unwrap()
                .media
                .as_ref()
                .unwrap()
                .display_title(),
            "B"
        );
        list.apply_scoped(&[1], vec![None], 0);
        assert_eq!(list.visible(), &[2]);
    }

    #[test]
    fn clearing_scores_preserves_filters_and_restores_title_order() {
        let scored = |id, title, status, score| ListEntry {
            score: Some(score),
            ..entry(id, title, status, 0, None)
        };
        for desc in [false, true] {
            let mut list = ListState::new();
            list.set_filter("PAUSED", 0);
            list.set_query("show".into(), 0);
            list.set_sort(SortKey::Score, 0);
            list.set_desc(desc, 0);
            list.replace(
                vec![
                    scored(1, "Alpha Show", "PAUSED", if desc { 3.0 } else { 7.0 }),
                    scored(2, "Beta Show", "PAUSED", 5.0),
                    scored(3, "Other", "PAUSED", 6.0),
                    scored(4, "Other Show", "CURRENT", 9.0),
                ],
                0,
            );
            assert_eq!(list.visible(), &[2, 1]);
            let revision = list.revision();
            let change = list.clear_scores(0);
            assert!(change.order_changed);
            assert_eq!(change.changed_ids.len(), 4);
            assert_eq!(list.visible(), &[1, 2]);
            assert!(list.ordered_entries().all(|entry| entry.score.is_none()));
            assert_eq!(list.revision(), revision + 1);
        }
    }

    #[test]
    fn entry_refresh_absorbs_only_unedited_progress_and_status() {
        let before = ListEntry {
            id: Some(1),
            ..entry(9, "Show", "CURRENT", 11, Some(12))
        };
        let after = ListEntry {
            status: "COMPLETED".into(),
            progress: 12,
            repeat: 4,
            score: Some(80.0),
            ..before.clone()
        };
        let mut editor = EditorState::new(before.clone());
        editor.set_score(Some(80.0), Some("POINT_100"));
        editor.apply_details(EntryDetails::default());
        editor.notes = "Draft note".into();
        editor.refresh_entry_progress(&after, true);
        assert_eq!((editor.status.as_str(), editor.progress), ("COMPLETED", 12));
        assert_eq!(editor.saved_progress(), 12);
        let patch = editor.basic_patch(true);
        assert_eq!(patch.status, None);
        assert_eq!(patch.progress, None);
        assert_eq!(patch.repeat, None);
        assert_eq!(editor.repeat, before.repeat);
        assert_eq!(patch.score, Some(80.0));
        assert_eq!(editor.details_patch().notes.as_deref(), Some("Draft note"));

        for case in [
            "input",
            "progress",
            "status",
            "busy",
            "review",
            "replacement",
        ] {
            let mut editor = EditorState::new(before.clone());
            let mut after = after.clone();
            match case {
                "progress" => editor.edit_progress(11),
                "status" => editor.choose_status("CURRENT"),
                "busy" => editor.busy = true,
                "review" => editor.review_progress = Some(10),
                "replacement" => after.id = Some(2),
                _ => {}
            }
            editor.refresh_entry_progress(&after, case != "input");
            assert_eq!(
                (editor.status.as_str(), editor.progress),
                ("CURRENT", 11),
                "{case}"
            );
            assert_eq!(editor.saved_progress(), 11, "{case}");
        }
    }

    #[test]
    fn new_episode_total_preserves_explicit_progress_and_reconsiders_inferred_completion() {
        let before = entry(9, "Show", "CURRENT", 3, Some(12));
        let after = entry(9, "Show", "CURRENT", 3, Some(13));
        for progress in [8, 12, 13] {
            let mut editor = EditorState::new(before.clone());
            editor.edit_progress(progress);
            editor.refresh_entry_progress(&after, true);
            assert_eq!(editor.progress, progress);
            assert_eq!(
                editor.status,
                if progress == 13 {
                    "COMPLETED"
                } else {
                    "CURRENT"
                }
            );
            assert_eq!(editor.basic_patch(false).progress, Some(progress));
        }
        let mut editor = EditorState::new(entry(9, "Show", "COMPLETED", 12, Some(12)));
        editor.edit_progress(11);
        editor.edit_progress(12);
        editor.refresh_entry_progress(&after, true);
        assert_eq!((editor.status.as_str(), editor.progress), ("CURRENT", 12));
    }

    #[test]
    fn reentering_existing_progress_preserves_completed_entries_above_total() {
        for (status, progress) in [("COMPLETED", 13), ("COMPLETED", 12), ("CURRENT", 13)] {
            let mut editor = EditorState::new(entry(9, "Show", status, progress, Some(12)));
            editor.edit_progress(progress);
            assert_eq!(editor.progress, progress);
            assert!(editor.basic_patch(true).is_empty());
        }
    }

    #[test]
    fn completion_dropdown_sets_known_total_immediately() {
        for (status, progress) in [("CURRENT", 3), ("COMPLETED", 12)] {
            let mut editor = EditorState::new(entry(4, "A", status, progress, Some(12)));
            editor.choose_status("COMPLETED");
            assert_eq!(editor.progress, 12);
            let patch = editor.basic_patch(true);
            assert_eq!(patch.status.as_deref(), Some("COMPLETED"));
            assert_eq!(patch.progress, Some(12));
        }
    }

    #[test]
    fn editor_sends_only_dirty_fields_and_keeps_basic_after_partial_save() {
        let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
        editor.apply_details(EntryDetails {
            notes: "old".into(),
            ..Default::default()
        });
        editor.notes = "new".into();
        editor.progress = 5;
        assert_eq!(editor.details_patch().notes.as_deref(), Some("new"));
        assert_eq!(editor.basic_patch(true).progress, Some(5));
        editor.apply_details(EntryDetails {
            notes: "new".into(),
            ..Default::default()
        });
        assert!(editor.details_patch().is_empty());
        assert_eq!(editor.basic_patch(true).progress, Some(5));
    }

    #[test]
    fn editor_progress_accepts_partial_save_without_replaying_it() {
        let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
        editor.choose_status("COMPLETED");
        assert_eq!(editor.basic_patch(true).progress, Some(12));
        editor.accept_progress(&entry(4, "A", "CURRENT", 12, Some(12)));
        let remaining = editor.basic_patch(true);
        assert_eq!(remaining.progress, None);
        assert_eq!(remaining.status.as_deref(), Some("COMPLETED"));
    }

    #[test]
    fn progress_review_keeps_explicit_progress_completion_and_saved_details() {
        for (status, progress) in [("CURRENT", 3), ("COMPLETED", 12)] {
            let mut editor = EditorState::new(entry(4, "A", status, progress, Some(12)));
            editor.apply_details(EntryDetails {
                notes: "saved note".into(),
                ..Default::default()
            });
            editor.choose_status("COMPLETED");
            editor.score = Some(80.0);
            for current in [6, 12, 1] {
                editor.review_progress = Some(current);
                editor.review_progress();
                assert_eq!(editor.saved_progress(), current);
                assert_eq!(editor.progress, 12);
                let patch = editor.basic_patch(true);
                assert_eq!(patch.status.as_deref(), Some("COMPLETED"));
                assert_eq!(patch.progress, Some(12));
                assert_eq!(patch.score, Some(80.0));
                assert!(editor.details_patch().is_empty());
            }
        }
    }

    #[test]
    fn progress_review_retains_explicit_progress_until_saved() {
        for accept_all in [false, true] {
            let original = entry(4, "A", "CURRENT", 5, Some(12));
            let mut editor = EditorState::new(original.clone());
            editor.edit_progress(8);
            editor.edit_progress(5);
            for current in [6, 5, 7] {
                editor.review_progress = Some(current);
                editor.review_progress();
                assert_eq!(editor.saved_progress(), current);
                assert_eq!(editor.progress, 5);
                assert_eq!(
                    editor.basic_patch(true).progress,
                    (current != 5).then_some(5)
                );
            }
            if accept_all {
                editor.accept_basic(&editor.clone(), original);
            } else {
                editor.accept_progress(&original);
            }
            editor.review_progress = Some(9);
            editor.review_progress();
            assert_eq!(editor.progress, 9);
            assert!(editor.basic_patch(true).is_empty());
        }
    }

    #[test]
    fn score_formats_clamp_to_production_steps() {
        for (format, max) in [
            ("POINT_3", 3),
            ("POINT_5", 5),
            ("POINT_10", 10),
            ("POINT_100", 100),
        ] {
            let format = Some(format);
            assert_eq!(clamp_score(-1.0, format), 0.0);
            assert_eq!(clamp_score(f64::from(max + 1), format), f64::from(max));
            for whole in 0..max {
                assert_eq!(
                    clamp_score(f64::from(whole) + 0.5, format),
                    f64::from(whole + 1)
                );
            }
        }
        let format = Some("POINT_10_DECIMAL");
        assert_eq!(clamp_score(-1.0, format), 0.0);
        assert_eq!(clamp_score(11.0, format), 10.0);
        assert_eq!(clamp_score(4.349, format), 4.3);
        assert_eq!(clamp_score(4.351, format), 4.4);
        for tenth in 0..100 {
            let score = format!("{}.{}5", tenth / 10, tenth % 10).parse().unwrap();
            assert_eq!(clamp_score(score, format), f64::from(tenth + 1) / 10.0);
        }
    }

    #[test]
    fn failed_stepper_save_remains_dirty_across_removal_and_concurrent_updates() {
        let now = Instant::now();
        let mut registry = ProgressRegistry::default();
        let item = entry(4, "A", "CURRENT", 2, Some(12));
        let edit = registry.entry(9, &item);
        assert!(edit.step(1, now));
        assert_eq!(edit.begin_save(), Some((3, 2)));
        edit.finish_save(Err("offline".into()));
        registry.entry(9, &entry(4, "A", "CURRENT", 5, Some(12)));
        assert_eq!(registry.edit(9, 4).unwrap().saved, 2);
        assert_eq!(registry.edit(9, 4).unwrap().pending, 3);
        assert_eq!(
            registry
                .first_review(9)
                .map(|review| (review.id, review.progress)),
            Some((4, Some(5)))
        );
        assert_eq!(registry.edit_mut(9, 4).unwrap().begin_save(), None);
        assert!(registry.begin_delete(9, 4));
        registry.finish_delete(9, 4, false);
        assert_eq!(registry.dirty_ids(9), vec![4]);
        assert!(registry.begin_delete(9, 4));
        registry.finish_delete(9, 4, true);
        assert!(registry.dirty_ids(9).is_empty());
    }

    #[test]
    fn clean_stepper_snapshot_must_follow_later_saved_progress() {
        let mut list = ListState::new();
        let mut row = entry(135, "A", "CURRENT", 3, Some(24));
        list.replace(vec![row.clone()], 100);
        let mut registry = ProgressRegistry::default();
        for progress in [4, 5] {
            let edit = registry.entry(1, &row);
            assert!(edit.step(1, Instant::now()));
            assert!(edit.begin_save().is_some());
            edit.finish_save(Ok(progress));
            row.progress = progress;
            list.apply_one(row.clone(), 100);
        }
        assert!(!registry.edit(1, 135).unwrap().dirty());
        row.progress = 14;
        list.apply_one(row, 100);
        registry.reconcile_entries(1, &list);
        let visible = registry
            .edit(1, 135)
            .map_or(list.entry(135).unwrap().progress, |edit| edit.pending);
        assert_eq!(
            visible, 14,
            "clean stepper snapshot overrides a later editor or sync save"
        );
    }

    #[test]
    fn editor_result_reconciles_clean_snapshot_before_service_event() {
        let mut registry = ProgressRegistry::default();
        let original = entry(135, "A", "CURRENT", 5, Some(24));
        registry.entry(7, &original);
        let saved = entry(135, "A", "CURRENT", 14, Some(24));
        registry.reconcile_entry(7, &saved);
        assert_eq!(registry.edit(7, 135).unwrap().pending, 14);
        assert_eq!(registry.edit(7, 135).unwrap().saved, 14);
    }

    #[test]
    fn reconciliation_keeps_dirty_in_flight_failed_and_review_drafts() {
        let mut registry = ProgressRegistry::default();
        let now = Instant::now();
        for id in [1, 2, 3, 4] {
            assert!(
                registry
                    .entry(7, &entry(id, "A", "CURRENT", 3, Some(24)))
                    .step(1, now)
            );
        }
        assert_eq!(registry.edit_mut(7, 2).unwrap().begin_save(), Some((4, 3)));
        assert_eq!(registry.edit_mut(7, 3).unwrap().begin_save(), Some((4, 3)));
        registry
            .edit_mut(7, 3)
            .unwrap()
            .finish_save(Err("offline".into()));
        registry.edit_mut(7, 4).unwrap().absorb(8, Some(24));
        assert!(registry.edit(7, 4).unwrap().requires_review);

        let mut list = ListState::new();
        list.replace(
            (1..=4)
                .map(|id| entry(id, "A", "CURRENT", if id == 4 { 4 } else { 3 }, Some(24)))
                .collect(),
            0,
        );
        registry.reconcile_entries(7, &list);
        for id in [1, 2, 3, 4] {
            let edit = registry.edit(7, id).unwrap();
            assert_eq!((edit.saved, edit.pending), (3, 4));
        }
        assert!(registry.edit(7, 2).unwrap().in_flight);
        assert_eq!(
            registry.edit(7, 3).unwrap().error.as_deref(),
            Some("offline")
        );
        assert!(registry.edit(7, 4).unwrap().requires_review);

        list.apply_one(entry(4, "A", "CURRENT", 4, Some(24)), 0);
        registry.reconcile_entries(7, &list);
        assert!(registry.edit(7, 4).unwrap().requires_review);
        assert_eq!(registry.edit(7, 4).unwrap().remote_progress, Some(4));
    }

    #[test]
    fn removed_entry_retires_clean_snapshot_and_preserves_delete_and_other_account() {
        let mut registry = ProgressRegistry::default();
        let clean = entry(1, "A", "CURRENT", 3, Some(24));
        registry.entry(7, &clean);
        registry.entry(8, &clean);
        assert!(registry.begin_delete(7, 1));
        let mut list = ListState::new();
        list.replace(vec![], 0);
        registry.reconcile_entries(7, &list);
        assert!(registry.edit(7, 1).unwrap().deleting);
        assert_eq!(registry.edit(8, 1).unwrap().pending, 3);
        registry.finish_delete(7, 1, true);
        assert!(registry.edit(7, 1).is_none());

        registry.entry(7, &clean);
        registry.reconcile_entries(7, &list);
        assert!(registry.edit(7, 1).is_none());
        assert_eq!(registry.edit(8, 1).unwrap().pending, 3);
    }

    #[test]
    fn detail_and_entry_results_are_independent_and_stale_results_drop() {
        let mut state = DetailState::default();
        let old = state.start(4);
        state.finish_detail(4, old, Err("network".into()));
        state.finish_entry(4, old, Ok(None));
        assert!(state.entry_available);
        assert_eq!(state.detail_error, "network");
        let next = state.start(5);
        state.finish_detail(
            4,
            old,
            Ok(MediaDetail {
                media: Media {
                    id: 4,
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
        assert!(state.detail.is_none());
        state.finish_detail(
            5,
            next,
            Ok(MediaDetail {
                media: Media {
                    id: 5,
                    ..Default::default()
                },
                relations: vec![],
                characters: vec![],
                staff: vec![],
                cached_at: Some(1),
                warning: Some("offline".into()),
                unavailable_sections: vec!["cast".into()],
            }),
        );
        assert_eq!(state.detail.as_ref().unwrap().media.id, 5);
        assert_eq!(
            state.detail.as_ref().unwrap().unavailable_sections,
            ["cast"]
        );
    }

    #[test]
    fn expired_old_account_cannot_clear_replacement_login() {
        assert!(!expiration_matches(7, Some(11), 8, Some(12), false));
        assert!(!expiration_matches(8, Some(11), 8, Some(12), false));
        assert!(!expiration_matches(8, None, 8, Some(12), true));
        assert!(!expiration_matches(8, None, 8, Some(12), false));
        assert!(expiration_matches(8, Some(12), 8, Some(12), false));
        assert!(!expiration_verification_matches(
            8,
            3,
            Some(12),
            8,
            4,
            Some(12),
            false
        ));
        assert!(!expiration_verification_matches(
            8,
            3,
            Some(12),
            9,
            3,
            Some(13),
            false
        ));
        assert!(expiration_verification_matches(
            8,
            3,
            Some(12),
            8,
            3,
            Some(12),
            false
        ));
    }

    #[test]
    fn expired_progress_stays_with_original_account_and_requires_review() {
        let mut registry = ProgressRegistry::default();
        let item = entry(7, "A", "CURRENT", 2, Some(12));
        assert!(registry.entry(4, &item).step(1, Instant::now()));
        let drafts = registry.take_generation_drafts(4);
        assert!(registry.dirty_ids(4).is_empty());
        assert_eq!(drafts.get(&7).unwrap().pending, 3);
        assert_eq!(drafts.get(&7).unwrap().saved, 2);
        assert!(drafts.get(&7).unwrap().error.is_some());
        registry.restore_generation(8, drafts);
        assert!(registry.dirty_ids(9).is_empty());
        let mut changed = ListState::new();
        changed.replace(vec![entry(7, "A", "CURRENT", 5, Some(12))], 0);
        registry.reconcile_entries(8, &changed);
        let edit = registry.edit(8, 7).unwrap();
        assert_eq!((edit.saved, edit.pending), (2, 3));
        assert!(
            edit.error
                .as_ref()
                .unwrap()
                .contains("changed on your list")
        );
        assert_eq!(registry.edit_mut(8, 7).unwrap().begin_save(), None);
        assert!(registry.resolve_review(8, 7, Some(5), true));
        assert_eq!(registry.edit_mut(8, 7).unwrap().begin_save(), Some((3, 5)));
    }

    #[test]
    fn ordinary_refresh_cannot_authorize_older_progress_timer() {
        let mut registry = ProgressRegistry::default();
        let original = entry(4, "A", "CURRENT", 2, Some(12));
        assert!(registry.entry(9, &original).step(1, Instant::now()));
        let mut current = ListState::new();
        current.replace(vec![entry(4, "A", "CURRENT", 5, Some(12))], 0);
        registry.reconcile_entries(9, &current);
        assert_eq!(
            registry.entry(9, current.entry(4).unwrap()).begin_save(),
            None
        );
        assert_eq!(
            registry
                .first_review(9)
                .map(|review| (review.id, review.progress)),
            Some((4, Some(5)))
        );
        assert!(registry.resolve_review(9, 4, Some(5), false));
        assert!(registry.dirty_ids(9).is_empty());
    }

    #[test]
    fn airing_sort_keeps_expiry_pending_until_rebuilt_after_route_return() {
        let mut list = ListState::new();
        let mut first = entry(4, "A", "CURRENT", 0, Some(12));
        first.media.as_mut().unwrap().next_airing_at = Some(120);
        let mut second = entry(5, "B", "CURRENT", 0, Some(12));
        second.media.as_mut().unwrap().next_airing_at = Some(600);
        list.replace(vec![first, second], 100);
        list.set_sort(SortKey::Airing, 100);
        assert_eq!(list.visible(), &[4, 5]);
        assert!(!list.airing_order_due(119));
        assert!(list.airing_order_due(120));
        assert!(list.airing_order_due(160));
        list.rebuild(160);
        assert_eq!(list.visible(), &[5, 4]);
        assert!(!list.airing_order_due(160));
        assert!(list.airing_order_due(600));
    }

    #[test]
    fn review_required_even_without_numeric_delta_blocks_barrier() {
        let mut registry = ProgressRegistry::default();
        let mut edit = ProgressEdit::new(3, Some(12));
        edit.requires_review = true;
        edit.error = Some("Previous save outcome is unknown.".into());
        registry.restore_generation(9, [(4, edit)].into());
        assert!(registry.dirty_ids(9).is_empty());
        assert_eq!(
            registry.first_error(9),
            Some("Previous save outcome is unknown.")
        );
        assert_eq!(registry.edit_mut(9, 4).unwrap().begin_save(), None);
    }

    #[test]
    fn superseded_save_preserves_a_newer_draft_until_review() {
        let mut edit = ProgressEdit::new(2, Some(12));
        assert!(edit.step(6, Instant::now()));
        assert_eq!(edit.begin_save(), Some((8, 2)));
        edit.pending = 10;
        edit.finish_superseded_save(8, 8);
        edit.absorb(9, Some(12));
        assert!(!edit.in_flight);
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (8, 10, Some(9))
        );
        assert!(edit.requires_review);
        assert_eq!(edit.begin_save(), None);
    }

    #[test]
    fn progress_conflict_requires_review_even_if_current_progress_matches_intent() {
        let before = entry(4, "A", "CURRENT", 5, Some(12));
        let mut observed = before.clone();
        observed.progress = 7;
        let mut registry = ProgressRegistry::default();
        let edit = registry.entry(9, &before);
        assert!(edit.step(2, Instant::now()));
        assert_eq!(edit.begin_save(), Some((7, 5)));
        edit.finish_conflicted_save(&observed);
        assert!(!edit.in_flight && edit.requires_review);
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (5, 7, Some(7))
        );
        assert_eq!(edit.begin_save(), None);
        assert!(!registry.retry_error(9, 4));
        assert!(registry.resolve_review(9, 4, Some(7), true));
        assert!(registry.dirty_ids(9).is_empty());
    }

    #[test]
    fn superseded_save_keeps_a_missing_entry_review_until_discard() {
        let mut registry = ProgressRegistry::default();
        let original = entry(4, "A", "CURRENT", 2, Some(12));
        let edit = registry.entry(9, &original);
        assert!(edit.step(6, Instant::now()));
        assert_eq!(edit.begin_save(), Some((8, 2)));
        let mut list = ListState::new();
        list.replace(vec![], 0);
        registry.reconcile_entries(9, &list);
        assert!(registry.has_in_flight(9));
        assert!(!registry.resolve_review(9, 4, None, false));
        assert!(!registry.begin_delete(9, 4));
        registry.discard_generation(9);
        assert!(registry.edit(9, 4).unwrap().requires_review);
        assert!(registry.has_in_flight(9));
        registry
            .edit_mut(9, 4)
            .unwrap()
            .finish_superseded_save(8, 8);
        registry.reconcile_entries(9, &list);
        let edit = registry.edit(9, 4).unwrap();
        assert!(!edit.in_flight && !edit.dirty());
        assert!(edit.requires_review);
        assert!(registry.first_error(9).is_some());
        assert!(registry.begin_delete(9, 4));
        assert!(!registry.resolve_review(9, 4, None, false));
        registry.discard_generation(9);
        assert!(registry.edit(9, 4).unwrap().requires_review);
        registry.finish_delete(9, 4, false);
        assert!(!registry.resolve_review(9, 4, None, true));
        assert!(registry.resolve_review(9, 4, None, false));
        assert!(registry.edit(9, 4).is_none());
    }
    #[test]
    fn readded_entry_replaces_the_missing_review_message_without_losing_the_draft() {
        let mut registry = ProgressRegistry::default();
        let original = entry(4, "A", "CURRENT", 2, Some(12));
        assert!(registry.entry(9, &original).step(3, Instant::now()));
        let mut list = ListState::new();
        list.replace(vec![], 0);
        registry.reconcile_entries(9, &list);
        assert_eq!(registry.first_error(9), Some(ProgressEdit::MISSING_ENTRY));

        list.apply_one(original, 0);
        registry.reconcile_entries(9, &list);
        let edit = registry.edit(9, 4).unwrap();
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (2, 5, Some(2))
        );
        assert!(edit.requires_review);
        assert_eq!(
            edit.error.as_deref(),
            Some("This entry is back on your list. Review before saving.")
        );
        assert!(registry.resolve_review(9, 4, Some(2), true));
        assert_eq!(registry.edit_mut(9, 4).unwrap().begin_save(), Some((5, 2)));
    }

    #[test]
    fn format_change_ack_preserves_invalidated_score() {
        for score in [None, Some(80.0)] {
            for round_trip in [false, true] {
                let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
                editor.displayed_format = Some("POINT_100".into());
                editor.score = score;
                let submitted = editor.clone();
                editor.refresh_score_format(Some("POINT_5".into()), None);
                if round_trip {
                    editor.refresh_score_format(Some("POINT_100".into()), None);
                }
                editor.repeat = 7;
                let accepted = ListEntry {
                    score: Some(80.0),
                    repeat: 3,
                    progress: 5,
                    ..submitted.snapshot.clone()
                };
                editor.accept_basic(&submitted, accepted);
                assert_eq!(editor.score, None);
                assert_eq!(editor.basic_patch(true).score, None);
                assert_eq!(editor.progress, 5);
                assert_eq!(editor.basic_patch(true).progress, None);
                assert_eq!(editor.basic_patch(true).repeat, Some(7));
                editor.score_cleared = true;
                assert_eq!(editor.basic_patch(true).score, Some(0.0));
            }
        }
    }

    #[test]
    fn format_change_ack_preserves_newer_score_baseline() {
        let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
        editor.displayed_format = Some("POINT_100".into());
        editor.score = Some(80.0);
        let old = editor.clone();
        editor.refresh_score_format(Some("POINT_5".into()), None);
        editor.score = Some(4.0);
        let new = editor.clone();
        editor.accept_basic(
            &new,
            ListEntry {
                score: Some(4.0),
                ..new.snapshot.clone()
            },
        );
        editor.accept_basic(
            &old,
            ListEntry {
                score: Some(80.0),
                ..old.snapshot.clone()
            },
        );
        assert_eq!(editor.score, Some(4.0));
        assert_eq!(editor.basic_patch(true).score, None);
    }

    #[test]
    fn format_refresh_and_explicit_clear_preserve_other_editor_intent() {
        let mut editor = EditorState::new(ListEntry {
            score: Some(80.0),
            ..Default::default()
        });
        editor.displayed_format = Some("POINT_100".into());
        editor.progress = 5;
        assert!(editor.refresh_score_format(Some("POINT_5".into()), None));
        assert_eq!(editor.score, None);
        assert_eq!(editor.basic_patch(true).score, None);
        editor.score_cleared = true;
        assert_eq!(editor.basic_patch(true).score, Some(0.0));
        assert_eq!(editor.basic_patch(true).progress, Some(5));
    }

    #[test]
    fn score_clear_acknowledgement_survives_a_format_switch() {
        let mut editor = EditorState::new(ListEntry {
            score: Some(8.4),
            ..Default::default()
        });
        editor.displayed_format = Some("POINT_10_DECIMAL".into());
        editor.set_score(None, Some("POINT_10_DECIMAL"));
        let submitted = editor.clone();
        editor.refresh_score_format(None, Some(""));
        assert_eq!(editor.basic_patch(false).score, Some(0.0));
        editor.accept_basic(
            &submitted,
            ListEntry {
                score: Some(0.0),
                ..submitted.snapshot.clone()
            },
        );
        assert_eq!(editor.basic_patch(false).score, None);
        assert_eq!(editor.basic_patch(true).score, None);
    }

    #[test]
    fn completed_entry_reopens_when_progress_drops_below_total() {
        let mut editor = EditorState::new(entry(4, "A", "COMPLETED", 12, Some(12)));
        editor.edit_progress(5);
        assert_eq!(editor.status, "CURRENT");
        let patch = editor.basic_patch(true);
        assert_eq!(patch.status.as_deref(), Some("CURRENT"));
        assert_eq!(patch.progress, Some(5));

        editor.edit_progress(12);
        assert_eq!(editor.status, "COMPLETED");
        assert!(editor.basic_patch(true).is_empty());
    }

    #[test]
    fn completed_dropdown_reopens_when_progress_edited_below_total() {
        let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
        editor.choose_status("COMPLETED");
        assert_eq!(editor.status, "COMPLETED");
        assert_eq!(editor.progress, 12);

        editor.edit_progress(5);
        assert_eq!(editor.status, "CURRENT");
        let patch = editor.basic_patch(true);
        assert_eq!(patch.status, None);
        assert_eq!(patch.progress, Some(5));
    }

    #[test]
    fn progress_review_after_expiry_unblocks_when_remote_progress_is_unchanged() {
        let mut registry = ProgressRegistry::default();
        let item = entry(7, "Show", "CURRENT", 3, Some(12));
        let edit = registry.entry(1, &item);
        assert!(edit.step(1, Instant::now()));
        let drafts = registry.take_generation_drafts(1);
        registry.restore_generation(2, drafts);
        let mut list = ListState::default();
        list.replace(vec![item], 0);
        registry.reconcile_entries(2, &list);
        let restored = registry.edit(2, 7).unwrap();
        assert!(restored.requires_review);
        assert_eq!(restored.remote_progress, Some(3));
        assert!(registry.resolve_review(2, 7, Some(3), false));
        let kept = registry.edit(2, 7).unwrap();
        assert!(!kept.requires_review);
        assert!(!kept.dirty());
    }

    #[test]
    fn zero_episode_total_is_treated_as_unknown_by_editor_and_stepper() {
        let item = entry(7, "Show", "CURRENT", 5, Some(0));
        let mut editor = EditorState::new(item.clone());
        assert_eq!(editor.total(), None);
        editor.choose_status("COMPLETED");
        assert_eq!(editor.progress, 5);

        let mut direct = ProgressEdit::new(2, Some(0));
        assert_eq!(direct.total, None);
        direct.absorb(2, Some(12));
        assert_eq!(direct.total, Some(12));
        direct.absorb(2, Some(0));
        assert_eq!(direct.total, None);
        direct.absorb(2, Some(-4));
        assert_eq!(direct.total, None);

        let mut registry = ProgressRegistry::default();
        let edit = registry.entry(1, &item);
        assert_eq!(edit.total, None);
        assert!(edit.step(1, Instant::now()));
        assert_eq!(edit.pending, 6);
        let updated = entry(7, "Show", "CURRENT", 5, Some(12));
        registry.entry(1, &updated);
        assert_eq!(registry.edit(1, 7).unwrap().total, Some(12));
        registry.entry(1, &item);
        assert_eq!(registry.edit(1, 7).unwrap().total, None);
    }
}
