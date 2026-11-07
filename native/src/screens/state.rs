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
use kurisu_core::models::{EntryDetails, FuzzyDate, ListEntry, Media, MediaDetail};

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
        self.step_error.clear();
        self.next_airing_boundary = None;
        self.revision += 1;
    }

    pub fn entry(&self, id: i64) -> Option<&ListEntry> {
        self.entries.get(&id)
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
    pub fn tracked_episodes(&self) -> i64 {
        self.entries.values().map(|e| e.progress).sum()
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
                .map(|id| (*id, title_key(&entries[id])))
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
        return false;
    };
    [
        &media.title_english,
        &media.title_romaji,
        &media.title_native,
    ]
    .into_iter()
    .flatten()
    .any(|title| title.to_lowercase().contains(needle))
}

fn title_key(entry: &ListEntry) -> String {
    entry
        .media
        .as_ref()
        .map(Media::display_title)
        .unwrap_or_else(|| format!("#{}", entry.media_id))
}

fn title_cmp(a: &ListEntry, b: &ListEntry) -> Ordering {
    compare_titles(&title_key(a), &title_key(b))
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
}

impl ProgressEdit {
    pub fn new(progress: i64, total: Option<i64>) -> Self {
        Self {
            saved: progress,
            pending: progress,
            total,
            remote_progress: None,
            requires_review: false,
            due: None,
            in_flight: false,
            error: None,
            deleting: false,
        }
    }

    pub fn dirty(&self) -> bool {
        self.pending != self.saved
    }

    pub fn step(&mut self, delta: i64, now: Instant) -> bool {
        if self.in_flight || self.deleting || self.requires_review {
            return false;
        }
        let next = self
            .pending
            .saturating_add(delta)
            .clamp(0, self.total.unwrap_or(9999).max(0));
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
        self.total = total;
        if self.deleting || self.in_flight {
            return;
        }
        if self.requires_review {
            if progress != self.saved {
                self.error = Some("Progress changed on your list. Review before saving.".into());
                self.due = None;
            }
            if self.remote_progress.is_some() || progress != self.saved {
                self.remote_progress = Some(progress);
            }
            return;
        }
        if self.error.is_some() {
            if progress != self.saved {
                self.remote_progress = Some(progress);
                self.requires_review = true;
                self.error = Some("Progress changed on your list. Review before saving.".into());
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
            self.error = Some("Progress changed on your list. Review before saving.".into());
            self.due = None;
        } else if self.requires_review {
            self.remote_progress = Some(progress);
        }
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

    pub fn finish_superseded_save(&mut self, submitted: i64, progress: i64) {
        self.in_flight = false;
        self.due = None;
        if self.pending == submitted {
            self.pending = progress;
        } else if progress != submitted {
            // Core returns the current value when the expected baseline no longer matches.
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
        let mut settled_missing = Vec::new();
        for (&(owner, id), edit) in &mut self.edits {
            if owner != generation {
                continue;
            }
            let Some(entry) = list.entry(id) else {
                if edit.deleting {
                    continue;
                }
                if edit.dirty() || edit.in_flight || edit.error.is_some() || edit.requires_review {
                    edit.error = Some(
                        "This entry is no longer on your list. Review the saved progress.".into(),
                    );
                    edit.requires_review = true;
                    edit.remote_progress = None;
                    edit.due = None;
                } else {
                    settled_missing.push((owner, id));
                }
                continue;
            };
            edit.absorb(
                entry.progress,
                entry.media.as_ref().and_then(|media| media.episodes),
            );
        }
        for key in settled_missing {
            self.edits.remove(&key);
        }
    }

    pub fn reconcile_entry(&mut self, generation: u64, entry: &ListEntry) {
        if let Some(edit) = self.edits.get_mut(&(generation, entry.media_id)) {
            edit.absorb(
                entry.progress,
                entry.media.as_ref().and_then(|media| media.episodes),
            );
        }
    }

    pub fn first_review(&self, generation: u64) -> Option<(i64, Option<i64>)> {
        self.edits
            .iter()
            .filter_map(|(&(owner, id), edit)| {
                (owner == generation && edit.requires_review).then_some((id, edit.remote_progress))
            })
            .min_by_key(|(id, _)| *id)
    }

    pub fn resolve_review(
        &mut self,
        generation: u64,
        id: i64,
        observed: Option<i64>,
        keep_local: bool,
    ) -> bool {
        let key = (generation, id);
        let Some(edit) = self.edits.get_mut(&key).filter(|edit| edit.requires_review) else {
            return false;
        };
        if edit.remote_progress != observed || keep_local && observed.is_none() {
            return false;
        }
        if let Some(progress) = observed {
            edit.saved = progress;
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
        if edit.requires_review || !edit.dirty() || edit.error.is_none() {
            return false;
        }
        edit.error = None;
        true
    }

    pub fn entry(&mut self, generation: u64, entry: &ListEntry) -> &mut ProgressEdit {
        let edit = self
            .edits
            .entry((generation, entry.media_id))
            .or_insert_with(|| {
                ProgressEdit::new(
                    entry.progress,
                    entry.media.as_ref().and_then(|m| m.episodes),
                )
            });
        edit.absorb(
            entry.progress,
            entry.media.as_ref().and_then(|m| m.episodes),
        );
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
        self.edits
            .iter()
            .any(|(&(g, _), edit)| g == generation && (edit.in_flight || edit.deleting))
    }
    pub fn first_error(&self, generation: u64) -> Option<&str> {
        self.edits.iter().find_map(|(&(g, _), edit)| {
            (g == generation && (edit.dirty() || edit.requires_review))
                .then(|| {
                    edit.error.as_deref().or(edit
                        .requires_review
                        .then_some("Review saved progress before continuing."))
                })
                .flatten()
        })
    }
    pub fn begin_delete(&mut self, generation: u64, id: i64) -> bool {
        let key = (generation, id);
        if !self.removals.insert(key) {
            return false;
        }
        if let Some(edit) = self.edits.get_mut(&key) {
            edit.deleting = true;
            edit.due = None;
        }
        true
    }
    pub fn finish_delete(&mut self, generation: u64, id: i64, success: bool) {
        let key = (generation, id);
        self.removals.remove(&key);
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
    pub repeat: i64,
    status_before_completion: Option<String>,
    status_chosen: bool,
    pub details: Option<EntryDetails>,
    pub notes: String,
    pub started_at: FuzzyDate,
    pub completed_at: FuzzyDate,
    pub custom_lists: Vec<String>,
    pub busy: bool,
    pub confirming_delete: bool,
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
            repeat: entry.repeat,
            snapshot: entry,
            status_before_completion: None,
            status_chosen: false,
            details: None,
            notes: String::new(),
            started_at: FuzzyDate::default(),
            completed_at: FuzzyDate::default(),
            custom_lists: Vec::new(),
            busy: false,
            confirming_delete: false,
            error: String::new(),
        }
    }

    pub fn total(&self) -> Option<i64> {
        self.media.as_ref().and_then(|m| m.episodes)
    }

    pub fn choose_status(&mut self, status: &str) {
        if !STATUSES.contains(&status) {
            return;
        }
        self.status = status.into();
        self.status_chosen = true;
        self.status_before_completion = None;
        if status == "COMPLETED"
            && let Some(total) = self.total()
        {
            self.progress = total;
        }
    }

    pub fn set_progress(&mut self, progress: i64) {
        self.progress = progress;
        if self.status_chosen {
            return;
        }
        if self
            .total()
            .is_some_and(|total| total > 0 && progress == total)
        {
            if self.status != "COMPLETED" {
                self.status_before_completion = Some(self.status.clone());
                self.status = "COMPLETED".into();
            }
        } else if let Some(previous) = self.status_before_completion.take() {
            self.status = previous;
        }
    }

    pub fn set_score(&mut self, value: Option<f64>, format: Option<&str>) {
        self.score = value.map(|v| clamp_score(v, format));
    }

    pub fn apply_details(&mut self, details: EntryDetails) {
        self.notes = details.notes.clone();
        self.started_at = details.started_at.clone().unwrap_or_default();
        self.completed_at = details.completed_at.clone().unwrap_or_default();
        self.custom_lists = details.custom_lists.clone();
        self.details = Some(details);
    }

    pub fn basic_patch(&self, score_available: bool) -> EntryPatch {
        let progress_touched =
            self.progress != self.snapshot.progress || self.status_before_completion.is_some();
        EntryPatch {
            status: (self.status_before_completion.is_none()
                && (self.status != self.snapshot.status || self.status_chosen && progress_touched))
                .then(|| self.status.clone()),
            progress: progress_touched.then(|| {
                self.progress
                    .clamp(0, self.total().unwrap_or(i64::MAX).max(0))
            }),
            score: (score_available && self.score != self.snapshot.score)
                .then_some(self.score.unwrap_or(0.0)),
            repeat: (self.repeat != self.snapshot.repeat).then_some(self.repeat.max(0)),
        }
    }

    pub fn is_dirty(&self, score_available: bool) -> bool {
        !self.basic_patch(score_available).is_empty() || !self.details_patch().is_empty()
    }

    pub fn saved_progress(&self) -> i64 {
        self.snapshot.progress
    }

    pub fn accept_progress(&mut self, entry: &ListEntry) {
        self.snapshot.progress = entry.progress;
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

    pub fn accept_basic(&mut self, entry: ListEntry) {
        self.snapshot = entry;
        self.status_before_completion = None;
        self.status_chosen = false;
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
    let (max, step) = match format {
        Some("POINT_3") => (3.0, 1.0),
        Some("POINT_5") => (5.0, 1.0),
        Some("POINT_10") => (10.0, 1.0),
        Some("POINT_10_DECIMAL") => (10.0, 0.1),
        _ => (100.0, 1.0),
    };
    ((score.clamp(0.0, max) / step).round() * step * 10.0).round() / 10.0
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
    fn completion_dropdown_sets_known_total_immediately() {
        let mut editor = EditorState::new(entry(4, "A", "CURRENT", 3, Some(12)));
        editor.choose_status("COMPLETED");
        assert_eq!(editor.progress, 12);
        let patch = editor.basic_patch(true);
        assert_eq!(patch.status.as_deref(), Some("COMPLETED"));
        assert_eq!(patch.progress, Some(12));
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
    fn score_formats_clamp_to_production_steps() {
        assert_eq!(clamp_score(3.6, Some("POINT_3")), 3.0);
        assert_eq!(clamp_score(6.0, Some("POINT_5")), 5.0);
        assert_eq!(clamp_score(7.7, Some("POINT_10")), 8.0);
        assert_eq!(clamp_score(7.76, Some("POINT_10_DECIMAL")), 7.8);
        assert_eq!(clamp_score(101.0, Some("POINT_100")), 100.0);
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
        assert_eq!(registry.first_review(9), Some((4, Some(5))));
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
        assert_eq!(registry.first_review(9), Some((4, Some(5))));
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
    fn superseded_compare_miss_cannot_retry_the_submitted_intent() {
        for submitted in [3, 8] {
            let mut edit = ProgressEdit::new(2, Some(12));
            assert!(edit.step(submitted - 2, Instant::now()));
            assert_eq!(edit.begin_save(), Some((submitted, 2)));
            edit.finish_superseded_save(submitted, 5);
            edit.absorb(5, Some(12));
            assert_eq!((edit.saved, edit.pending), (5, 5));
            assert!(!edit.in_flight && !edit.requires_review);
            assert_eq!(edit.begin_save(), None);
        }
    }

    #[test]
    fn superseded_compare_miss_preserves_a_distinct_draft_for_review() {
        let mut edit = ProgressEdit::new(2, Some(12));
        assert!(edit.step(1, Instant::now()));
        assert_eq!(edit.begin_save(), Some((3, 2)));
        edit.pending = 8;
        edit.finish_superseded_save(3, 5);
        edit.absorb(5, Some(12));
        assert_eq!(
            (edit.saved, edit.pending, edit.remote_progress),
            (2, 8, Some(5))
        );
        assert!(edit.requires_review);
        assert_eq!(edit.begin_save(), None);
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
        registry
            .edit_mut(9, 4)
            .unwrap()
            .finish_superseded_save(8, 8);
        registry.reconcile_entries(9, &list);
        let edit = registry.edit(9, 4).unwrap();
        assert!(!edit.in_flight && !edit.dirty());
        assert!(edit.requires_review);
        assert!(registry.first_error(9).is_some());
        assert!(!registry.resolve_review(9, 4, None, true));
        assert!(registry.resolve_review(9, 4, None, false));
        assert!(registry.edit(9, 4).is_none());
    }
}
