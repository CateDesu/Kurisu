use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::{anilist, commands::AppState, db::Db, models::ListEntry};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Patch {
    pub status: Option<String>,
    pub progress: Option<i64>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback: Option<PlaybackProgress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_completion: Option<AutomaticCompletion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_baseline: Option<CompletionBaseline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CompletionBaseline {
    id: i64,
    updated_at: Option<i64>,
    status: String,
    repeat: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AutomaticCompletion {
    // The fields to keep when the requested episode is no longer the finale.
    status: Option<String>,
    repeat: Option<i64>,
}

impl AutomaticCompletion {
    pub(crate) fn new(repeat: Option<i64>) -> Self {
        Self {
            status: None,
            repeat,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PlaybackProgress {
    episode: i64,
    prior_status: Option<String>,
    prior_progress: Option<i64>,
    prior_repeat: Option<i64>,
    manual_status: bool,
    manual_repeat: bool,
}

impl Patch {
    fn apply_completion_total(&mut self, total: Option<i64>) {
        if let Some(completion) = &self.automatic_completion {
            if total != self.progress {
                self.status.clone_from(&completion.status);
                self.repeat = completion.repeat;
            }
        }
    }

    fn merge(&mut self, newer: &Self) {
        if newer.status.is_some() {
            self.status.clone_from(&newer.status);
        }
        if newer.progress.is_some() {
            self.progress = newer.progress;
        }
        if newer.score.is_some() {
            self.score = newer.score;
        }
        if newer.repeat.is_some() {
            self.repeat = newer.repeat;
        }
    }

    // Older queues stored decrements without the corresponding status change.
    fn reopen_completed(&mut self, total: Option<i64>, before: Option<&ListEntry>) {
        if self.status.is_none()
            && self
                .progress
                .is_some_and(|progress| crate::commands::reopens_completed(before, total, progress))
        {
            self.status = Some("CURRENT".into());
        }
    }

    pub fn apply(&self, entry: &mut ListEntry) {
        if let Some(status) = &self.status {
            entry.status.clone_from(status);
        }
        if let Some(progress) = self.progress {
            entry.progress = progress;
        }
        if let Some(score) = self.score {
            entry.score = Some(score);
        }
        if let Some(repeat) = self.repeat {
            entry.repeat = repeat;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub media_id: i64,
    #[serde(default)]
    pub deleting: bool,
    // A resolved removal target can differ from the saved edit baseline.
    #[serde(default)]
    pub deletion_id: Option<i64>,
    pub base: Option<ListEntry>,
    pub patch: Patch,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_error: Option<String>,
    pub conflict: bool,
    #[serde(default)]
    pub missing_media: bool,
    #[serde(default)]
    pub score_format: Option<String>,
    // Older rows use the patch progress and the remote baseline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub playback: bool,
    // Re-entry changes the intent's scale, never the offline baseline's scale.
    #[serde(default)]
    pub base_score_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_progress: Option<anilist::IncompatibleEntryWrite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_total: Option<i64>,
}

const PROGRESS_EXCEEDS_TOTAL: &str =
    "The saved progress exceeds AniList's episode total. Edit the progress before syncing.";
const ENTRY_REPLACED: &str =
    "AniList replaced this entry while a change was waiting. Choose which version to keep.";

impl Pending {
    pub(crate) fn needs_cleanup_only(&self) -> bool {
        !self.deleting
            && !self.missing_media
            && !self.conflict
            && self.base.as_ref().is_some_and(|base| base.id.is_some())
            && self.patch.status.is_none()
            && self.patch.progress.is_none()
            && self.patch.score.is_none()
            && self.patch.repeat.is_none()
            && !self.is_playback()
    }

    fn identity_replaced_by(&self, current: Option<&ListEntry>) -> bool {
        let target = self
            .deletion_id
            .filter(|_| self.deleting)
            .or_else(|| self.base.as_ref().and_then(|base| base.id))
            .or_else(|| self.rejected_progress.as_ref().map(|write| write.id));
        current
            .and_then(|entry| entry.id)
            .is_some_and(|id| Some(id) != target)
    }

    fn mark_entry_replaced(&mut self) {
        self.conflict = true;
        self.error = Some(ENTRY_REPLACED.into());
        self.retry_error = None;
        self.missing_media = false;
    }

    pub(crate) fn mark_media_missing(&mut self) {
        self.conflict = true;
        self.missing_media = true;
        self.error = Some("This anime was removed or merged on AniList. Your saved change is retained here until you discard it.".into());
        self.retry_error = None;
    }

    pub(crate) fn record_retry_error(&mut self, error: &str) {
        if self.conflict {
            self.retry_error = Some(error.into());
        } else {
            self.error = Some(error.into());
        }
    }

    fn record_conflict(&mut self, error: String) {
        if self.error.as_deref() == Some(ENTRY_REPLACED) {
            self.retry_error = Some(error);
        } else {
            self.error = Some(error);
            self.retry_error = None;
        }
        self.conflict = true;
    }

    pub(crate) fn saved_media(&self, total: Option<Option<i64>>) -> Option<crate::Media> {
        let mut media = self.base.as_ref()?.media.as_ref()?.clone();
        if !media.has_metadata() {
            return None;
        }
        media.episodes = total.unwrap_or_else(|| media.episodes.filter(|total| *total > 0));
        Some(media)
    }

    fn is_playback(&self) -> bool {
        self.playback || self.patch.playback.is_some()
    }

    fn playback_baseline(&self) -> Option<ListEntry> {
        if self.patch.playback.is_none() && (!self.playback || self.base.is_none()) {
            return None;
        }
        let mut base = self
            .base
            .as_ref()
            .map_or_else(ListEntry::default, |base| ListEntry {
                id: base.id,
                status: base.status.clone(),
                progress: base.progress,
                repeat: base.repeat,
                updated_at: base.updated_at,
                ..Default::default()
            });
        if let Some(playback) = &self.patch.playback {
            if let Some(status) = &playback.prior_status {
                base.status.clone_from(status);
            }
            base.progress = playback.prior_progress.unwrap_or(base.progress);
            base.repeat = playback.prior_repeat.unwrap_or(base.repeat);
        }
        Some(base)
    }

    fn apply_playback_total(&mut self, total: Option<i64>) {
        if let Some((status, repeat)) = self.playback_fields(total) {
            self.patch.status = status;
            self.patch.repeat = repeat;
        }
    }

    fn playback_fields(&self, total: Option<i64>) -> Option<(Option<String>, Option<i64>)> {
        let base = self.playback_baseline()?;
        let progress = self.patch.progress?;
        let Some(playback) = &self.patch.playback else {
            return Some(crate::commands::playback_fields(&base, total, progress));
        };
        // A clamped prompt confirms its displayed progress without completing the show.
        let (status, repeat) = if playback.episode != progress && total == Some(progress) {
            (None, None)
        } else {
            crate::commands::playback_fields(&base, total, progress)
        };
        Some((
            if playback.manual_status {
                self.patch.status.clone()
            } else {
                status.or_else(|| playback.prior_status.clone())
            },
            if playback.manual_repeat {
                self.patch.repeat
            } else {
                repeat.or(playback.prior_repeat)
            },
        ))
    }

    fn merge_entry_patch(&mut self, patch: &Patch, episode: Option<i64>, total: Option<i64>) {
        self.apply_playback_total(total);
        self.merge_automatic_completion(patch, episode, total);
        if let Some(episode) = episode {
            let playback = self.patch.playback.get_or_insert_with(|| {
                if self.playback {
                    PlaybackProgress::default()
                } else {
                    PlaybackProgress {
                        prior_status: self.patch.status.clone(),
                        prior_progress: self.patch.progress,
                        prior_repeat: self.patch.repeat,
                        ..Default::default()
                    }
                }
            });
            // A clamped prompt advances only progress. Keep the previous playback edits.
            let clamped = patch.progress.is_some_and(|progress| episode != progress);
            if clamped {
                playback.prior_progress = self.patch.progress;
            }
            if clamped || playback.manual_status {
                playback.prior_status.clone_from(&self.patch.status);
                playback.manual_status = false;
            }
            if clamped || playback.manual_repeat {
                playback.prior_repeat = self.patch.repeat;
                playback.manual_repeat = false;
            }
            playback.episode = episode;
            self.playback = false;
        } else if patch.progress.is_some() || patch.status.as_deref() == Some("COMPLETED") {
            self.patch.playback = None;
            self.playback = false;
        } else if self.is_playback() && (patch.status.is_some() || patch.repeat.is_some()) {
            let playback = self.patch.playback.get_or_insert_with(|| PlaybackProgress {
                episode: self.patch.progress.unwrap_or_default(),
                ..Default::default()
            });
            playback.manual_status |= patch.status.is_some();
            playback.manual_repeat |= patch.repeat.is_some();
            self.playback = false;
        }
        self.patch.merge(patch);
        self.apply_playback_total(total);
    }

    fn merge_automatic_completion(
        &mut self,
        patch: &Patch,
        episode: Option<i64>,
        total: Option<i64>,
    ) {
        let previous = self.patch.automatic_completion.take();
        if let Some(completion) = &patch.automatic_completion {
            let fallback = previous.unwrap_or_else(|| match &self.patch.playback {
                Some(playback) => AutomaticCompletion {
                    status: if playback.manual_status {
                        self.patch.status.clone()
                    } else {
                        playback.prior_status.clone()
                    },
                    repeat: if playback.manual_repeat {
                        self.patch.repeat
                    } else {
                        playback.prior_repeat
                    },
                },
                None if self.playback => AutomaticCompletion::default(),
                None => AutomaticCompletion {
                    status: self.patch.status.clone(),
                    repeat: self.patch.repeat,
                },
            });
            self.patch.automatic_completion = Some(AutomaticCompletion {
                status: fallback.status,
                repeat: completion.repeat.or(fallback.repeat),
            });
        } else if let Some(mut previous) = previous {
            if episode.is_none()
                && patch.status.is_none()
                && (patch.progress.is_none() || patch.progress == self.patch.progress)
            {
                previous.repeat = patch.repeat.or(previous.repeat);
                self.patch.automatic_completion = Some(previous);
            } else if patch.status.as_deref() != Some("COMPLETED") || total != self.patch.progress {
                self.patch.status = previous.status;
                self.patch.repeat = previous.repeat;
            }
        }
    }

    // The recorded episode stays fixed when AniList changes the finale.
    pub(crate) fn refresh_playback_total(
        &mut self,
        state: &AppState,
        remote: Option<i64>,
    ) -> Result<bool, String> {
        if !self.is_playback() {
            return Ok(true);
        }
        let total = match remote {
            Some(total) => Some(total),
            None => state
                .db
                .episode_total(self.media_id)
                .map_err(|e| e.to_string())?,
        };
        self.apply_playback_total(total);
        Ok(self
            .patch
            .progress
            .zip(total)
            .is_none_or(|(episode, total)| episode <= total))
    }

    fn allows_completion_adjustment(&self) -> bool {
        !self.is_playback()
            && self.patch.automatic_completion.is_none()
            && self.patch.status.as_deref() == Some("COMPLETED")
    }

    fn progress_for_total(&self, total: Option<i64>) -> Option<i64> {
        if self.allows_completion_adjustment() {
            if let Some(total) = total.filter(|total| *total > 0) {
                return Some(total);
            }
        }
        self.patch.progress
    }

    fn replay_at_total(
        &self,
        remote: Option<&ListEntry>,
        fresh_total: Option<i64>,
        total: Option<i64>,
    ) -> Self {
        let mut replay = self.clone();
        replay.patch.apply_completion_total(total);
        replay.reopen_after_lookup(remote, fresh_total, total);
        replay
    }

    fn reopen_after_lookup(
        &mut self,
        remote: Option<&ListEntry>,
        fresh_total: Option<i64>,
        total: Option<i64>,
    ) {
        if self.patch.status.is_some() {
            return;
        }
        let clamped_base = self.base.as_ref().filter(|base| {
            self.rejected_progress.as_ref().is_some_and(|rejected| {
                rejected.clamped_active_entry(base)
                    && remote.is_some_and(|entry| rejected.matches_entry(entry))
            })
        });
        if let Some(base) = clamped_base {
            if self
                .patch
                .progress
                .is_some_and(|p| crate::commands::reopens_completed(remote, fresh_total, p))
            {
                self.patch.status = Some(base.status.clone());
            }
        } else {
            self.patch
                .reopen_completed(total, remote.or(self.base.as_ref()));
        }
    }

    fn progress_rejection(
        &self,
        remote: Option<&ListEntry>,
        total: Option<i64>,
        progress: Option<i64>,
    ) -> Option<&'static str> {
        let rejected = self.rejected_progress.as_ref()?;
        let remote = remote?;
        let total_increased = total
            .is_some_and(|total| total > rejected.total.flatten().unwrap_or(rejected.progress));
        let same_tuple = self.patch.status == rejected.requested_status
            && progress == rejected.requested_progress
            && !total_increased;
        let would_complete = rejected.forced_completion()
            && matches!(self.patch.status.as_deref(), Some("CURRENT" | "REPEATING"))
            && progress.is_some_and(|progress| progress >= total.unwrap_or(rejected.progress));
        let exceeds_clamp = total.is_none()
            && self
                .base
                .as_ref()
                .is_some_and(|base| rejected.clamped_active_entry(base))
            && progress.is_some_and(|p| p > rejected.progress);
        (rejected.matches_entry(remote) && (same_tuple || would_complete || exceeds_clamp))
            .then_some(anilist::IncompatibleEntryWrite::MESSAGE)
    }

    fn progress_write_rejection(&self) -> Option<&anilist::IncompatibleEntryWrite> {
        self.rejected_progress.as_ref().filter(|rejected| {
            let requested_status_matches = rejected
                .requested_status
                .as_ref()
                .is_none_or(|status| *status == rejected.status);
            rejected.requested_progress.is_some_and(|progress| {
                rejected.forced_completion()
                    || requested_status_matches && progress != rejected.progress
            })
        })
    }

    fn has_progress_conflict(&self) -> bool {
        self.conflict
            && match self.error.as_deref() {
                Some(PROGRESS_EXCEEDS_TOTAL | crate::commands::EPISODE_EXCEEDS_TOTAL) => true,
                Some(anilist::IncompatibleEntryWrite::MESSAGE) => {
                    self.progress_write_rejection().is_some()
                }
                _ => false,
            }
    }

    fn record_progress_conflict(&mut self, error: &str, total: Option<i64>) -> bool {
        let update_error =
            !self.conflict || self.has_progress_conflict() && self.error.as_deref() != Some(error);
        if update_error {
            self.conflict = true;
            self.error = Some(error.into());
            self.retry_error = None;
        }
        let total = total.or(self.rejected_total);
        let changed = update_error || total != self.rejected_total;
        self.rejected_total = total;
        changed
    }

    fn overshoot_total(
        &self,
        progress: Option<i64>,
        remote_total: Option<i64>,
        cached_total: Option<i64>,
    ) -> Option<i64> {
        remote_total
            .or(self
                .rejected_total
                .map(|total| cached_total.unwrap_or(total)))
            .filter(|total| progress.is_some_and(|progress| progress > *total))
    }

    pub(crate) fn record_incompatible_write(
        &mut self,
        mut rejected: anilist::IncompatibleEntryWrite,
        total: Option<i64>,
    ) {
        rejected.total = rejected.total.or(total.map(Some));
        self.record_conflict(rejected.to_string());
        self.rejected_progress = (!rejected.score_or_repeat_only).then_some(rejected);
    }
}

#[cfg(test)]
thread_local! {
    static PENDING_DESERIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENTRY_NOTIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn parse_pending(payload: &str) -> Result<Pending> {
    #[cfg(test)]
    PENDING_DESERIALIZATIONS.set(PENDING_DESERIALIZATIONS.get() + 1);
    Ok(serde_json::from_str(payload)?)
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingChange {
    pub media_id: i64,
    pub title: String,
    pub progress: Option<i64>,
    pub status: Option<String>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
    pub error: Option<String>,
    pub conflict: bool,
    pub missing_media: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WatchHistoryItem {
    pub id: i64,
    pub path: String,
    pub media_id: i64,
    pub episode: i64,
    pub watched_at: i64,
    pub title: String,
}

pub(crate) fn account_id(db: &Db) -> Option<i64> {
    db.get_setting("anilist_user_id")
        .ok()
        .flatten()?
        .parse()
        .ok()
        .filter(|id| *id > 0)
}

fn row_title(row: &rusqlite::Row<'_>, start: usize, id: i64) -> rusqlite::Result<String> {
    Ok(cached_title(row, start)?.unwrap_or_else(|| format!("#{id}")))
}

fn cached_title(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Option<String>> {
    for index in start..start + 3 {
        if let Some(title) = row.get::<_, Option<String>>(index)? {
            if !title.trim().is_empty() {
                return Ok(Some(title));
            }
        }
    }
    Ok(None)
}

const HISTORY_FIRST_PAGE: &str = "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
     m.title_english,m.title_romaji,m.title_native
     FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
     WHERE h.account_id = ? ORDER BY h.id DESC LIMIT ?";

const HISTORY_BEFORE_PAGE: &str = "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
     m.title_english,m.title_romaji,m.title_native
     FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
     WHERE h.account_id = ? AND h.id < ? ORDER BY h.id DESC LIMIT ?";

impl Db {
    pub(crate) fn write_completion_baselines(tx: &rusqlite::Transaction<'_>) -> Result<()> {
        // The remote snapshot and the fields hidden by its local overlay must land together.
        let baselines = "WITH baselines AS MATERIALIZED (
                SELECT p.media_id, p.account_id,
                    COALESCE(p.payload -> '$.patch.completion_baseline', 'null') AS previous,
                    CASE WHEN e.entry_id IS NOT NULL THEN json_object(
                        'id', e.entry_id, 'updated_at', e.updated_at,
                        'status', e.status, 'repeat', e.repeat) ELSE 'null' END AS baseline
                FROM pending_change p LEFT JOIN list_entry e ON e.media_id = p.media_id
                WHERE p.account_id = (SELECT value FROM settings WHERE key = 'anilist_user_id')
                  AND CASE WHEN json_valid(p.payload) THEN
                      json_extract(p.payload, '$.playback') = 1
                      OR json_type(p.payload, '$.patch.playback') = 'object'
                      OR json_type(p.payload, '$.patch.automatic_completion') = 'object'
                  END
             ), changed AS (
                SELECT media_id, account_id, baseline FROM baselines WHERE previous IS NOT baseline
             )";
        let expected = tx.query_row(
            &format!("{baselines} SELECT count(*) FROM changed"),
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if expected != 0 {
            let written = tx.execute(
                &format!(
                    "{baselines} UPDATE pending_change AS p
                     SET payload = json_set(p.payload, '$.patch.completion_baseline', json(b.baseline))
                     FROM changed b
                     WHERE p.media_id = b.media_id AND p.account_id = b.account_id"
                ),
                [],
            )?;
            anyhow::ensure!(
                written == usize::try_from(expected)?,
                "A saved completion baseline was not updated"
            );
        }
        Ok(())
    }

    pub(crate) fn pending(&self, account: i64) -> Result<Vec<Pending>> {
        let conn = self.0.lock();
        let mut stmt = conn
            .prepare("SELECT payload FROM pending_change WHERE account_id = ? ORDER BY media_id")?;
        let rows = stmt.query_map([account], |r| r.get::<_, String>(0))?;
        rows.map(|r| parse_pending(&r?)).collect()
    }

    fn pending_background_ids(&self, account: i64, after: Option<i64>) -> Result<Vec<i64>> {
        let conn = self.0.lock();
        let mut ids = Vec::with_capacity(10);
        let queries: &[&str] = if after.is_some() {
            &[
                "SELECT payload FROM pending_change WHERE account_id = ? AND media_id > ? ORDER BY media_id",
                "SELECT payload FROM pending_change WHERE account_id = ? AND media_id <= ? ORDER BY media_id",
            ]
        } else {
            &["SELECT payload FROM pending_change WHERE account_id = ? ORDER BY media_id"]
        };
        for query in queries {
            let mut stmt = conn.prepare(query)?;
            let mut rows = stmt.query(rusqlite::params_from_iter(
                [account].into_iter().chain(after),
            ))?;
            while let Some(row) = rows.next()? {
                let pending = parse_pending(&row.get::<_, String>(0)?)?;
                if !pending.conflict {
                    ids.push(pending.media_id);
                    if ids.len() == 10 {
                        return Ok(ids);
                    }
                }
            }
        }
        Ok(ids)
    }

    pub(crate) fn pending_change(&self, account: i64, media_id: i64) -> Result<Option<Pending>> {
        Self::read_pending(&self.0.lock(), account, media_id)
    }

    fn read_pending(
        conn: &rusqlite::Connection,
        account: i64,
        media_id: i64,
    ) -> Result<Option<Pending>> {
        let payload = conn
            .query_row(
                "SELECT payload FROM pending_change WHERE account_id = ? AND media_id = ?",
                [account, media_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload.map(|payload| parse_pending(&payload)).transpose()
    }

    pub(crate) fn put_pending(&self, account: i64, pending: &Pending) -> Result<()> {
        self.put_pending_with_retry_position(account, pending, false)
    }

    pub(crate) fn put_pending_with_retry_position(
        &self,
        account: i64,
        pending: &Pending,
        advance: bool,
    ) -> Result<()> {
        self.durable_write(|tx| {
            Self::write_pending(tx, account, pending)?;
            if advance {
                Self::write_retry_position(tx, account, pending.media_id)?;
            }
            Ok(())
        })
    }

    fn write_retry_position(
        tx: &rusqlite::Transaction<'_>,
        account: i64,
        media_id: i64,
    ) -> Result<()> {
        let written = tx
            .execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [retry_cursor_key(account), media_id.to_string()],
            )
            .map_err(|e| anyhow::anyhow!("Could not save the sync retry position: {e}"))?;
        anyhow::ensure!(written == 1, "Could not save the sync retry position");
        Ok(())
    }

    fn write_pending(conn: &rusqlite::Connection, account: i64, pending: &Pending) -> Result<()> {
        let written = conn.execute(
            "INSERT OR REPLACE INTO pending_change (account_id,media_id,payload) VALUES (?,?,?)",
            rusqlite::params![account, pending.media_id, serde_json::to_string(pending)?],
        )?;
        anyhow::ensure!(written == 1, "the pending change was not saved");
        Ok(())
    }

    pub(crate) fn remove_pending(&self, account: i64, media_id: i64) -> Result<()> {
        self.durable_write(|tx| Self::delete_pending_from(tx, account, media_id))
    }

    pub(crate) fn delete_pending_from(
        conn: &rusqlite::Connection,
        account: i64,
        media_id: i64,
    ) -> Result<()> {
        let removed = conn.execute(
            "DELETE FROM pending_change WHERE account_id = ? AND media_id = ?",
            [account, media_id],
        )?;
        anyhow::ensure!(
            removed == 1 || Self::read_pending(conn, account, media_id)?.is_none(),
            "the pending change was not removed"
        );
        Ok(())
    }

    fn write_pending_acknowledgement(
        conn: &rusqlite::Connection,
        account: i64,
        entry: &ListEntry,
    ) -> Result<()> {
        if let Some(mut pending) = Self::read_pending(conn, account, entry.media_id)? {
            pending.base = Some(entry.clone());
            pending.patch = Patch::default();
            pending.deleting = false;
            pending.deletion_id = None;
            pending.playback = false;
            pending.conflict = false;
            pending.missing_media = false;
            pending.error = None;
            pending.retry_error = None;
            pending.rejected_progress = None;
            pending.rejected_total = None;
            Self::write_pending(conn, account, &pending)?;
        }
        Ok(())
    }

    pub(crate) fn mark_acknowledged_pending(&self, account: i64, entry: &ListEntry) -> Result<()> {
        self.durable_write(|tx| Self::write_pending_acknowledgement(tx, account, entry))
    }

    pub(crate) fn cache_acknowledged_entry(
        &self,
        account: Option<i64>,
        entry: &mut ListEntry,
        confirmed_total: Option<i64>,
    ) -> Result<()> {
        let media_id = entry.media_id;
        let total = confirmed_total.map(Some);
        let cache = |tx: &rusqlite::Transaction<'_>, entry: &mut ListEntry| -> Result<()> {
            if entry.queued_creation.is_none() {
                entry.queued_creation = tx
                    .query_row(
                        "SELECT queued_creation FROM list_entry WHERE media_id = ? AND entry_id = ?",
                        rusqlite::params![entry.media_id, entry.id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
            }
            Self::write_entry_with_creation(tx, entry, entry.queued_creation.as_deref())?;
            anyhow::ensure!(tx.changes() == 1, "the confirmed entry was not cached");
            Ok(())
        };
        let cached = self.durable_write_with_episode_total(media_id, total, |tx| {
            if let Some(account) = account {
                Self::write_pending_acknowledgement(tx, account, entry)?;
            }
            cache(tx, entry)
        });
        if let (Err(error), Some(account)) = (&cached, account) {
            if self
                .durable_write_with_episode_total(media_id, total, |tx| {
                    Self::delete_pending_from(tx, account, media_id)?;
                    cache(tx, entry)
                })
                .is_ok()
            {
                return Ok(());
            }
            // An entry cache failure must not make a confirmed mutation replayable.
            if let Err(settlement) = self
                .mark_acknowledged_pending(account, entry)
                .or_else(|_| self.remove_pending(account, media_id))
            {
                anyhow::bail!("{error}; could not settle the confirmed write: {settlement}");
            }
        }
        cached
    }

    pub fn record_watch(
        &self,
        account_id: i64,
        path: &str,
        media_id: i64,
        episode: i64,
    ) -> Result<()> {
        self.durable_write(|conn| {
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO watch_history (account_id,path,media_id,episode,watched_at)
             SELECT ?,?,?,?,? WHERE NOT EXISTS (
                 SELECT 1 FROM watch_history WHERE account_id = ? AND path = ? AND media_id = ?
                 AND episode = ? AND watched_at > ?)",
                rusqlite::params![
                    account_id,
                    path,
                    media_id,
                    episode,
                    now,
                    account_id,
                    path,
                    media_id,
                    episode,
                    now - 300
                ],
            )?;
            Ok(())
        })
    }

    pub fn watch_history(
        &self,
        account: i64,
        limit: i64,
        before: Option<i64>,
    ) -> Result<Vec<WatchHistoryItem>> {
        let conn = self.0.lock();
        let mut stmt = conn.prepare(if before.is_some() {
            HISTORY_BEFORE_PAGE
        } else {
            HISTORY_FIRST_PAGE
        })?;
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok(WatchHistoryItem {
                id: r.get(0)?,
                path: r.get(1)?,
                media_id: r.get(2)?,
                episode: r.get(3)?,
                watched_at: r.get(4)?,
                title: row_title(r, 5, r.get(2)?)?,
            })
        };
        let rows = match before {
            Some(before) => stmt.query_map(rusqlite::params![account, before, limit], map_row)?,
            None => stmt.query_map(rusqlite::params![account, limit], map_row)?,
        };
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

pub(crate) const SCORE_FORMAT_ERROR: &str = "The saved score format is unknown or changed. Re-enter the score in your current AniList format before syncing.";
const SCORE_BASELINE_ERROR: &str = "The saved score baseline cannot be compared in the current AniList format. Choose which version to keep.";

fn comparable_base_score(pending: &Pending, target_format: Option<&str>) -> Option<f64> {
    let score = pending.base.as_ref()?.score?;
    if score == 0.0 {
        return Some(0.0);
    }
    let from = pending.base_score_format.as_deref()?;
    let to = target_format?;
    match (from, to) {
        _ if from == to => Some(score),
        ("POINT_100", "POINT_10_DECIMAL")
            if (0.0..=100.0).contains(&score) && score.fract() == 0.0 =>
        {
            Some(score / 10.0)
        }
        ("POINT_10_DECIMAL", "POINT_100")
            if (0.0..=10.0).contains(&score) && (score * 10.0).round() / 10.0 == score =>
        {
            Some((score * 10.0).round())
        }
        // Coarse formats do not preserve the original score's precision.
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn stage(
    state: &AppState,
    media_id: i64,
    patch: Patch,
) -> Result<Option<(i64, Pending, bool)>> {
    let staged = prepare_entry(state, media_id, patch, None, None)?;
    if let Some((account, pending, _)) = &staged {
        state.db.put_pending(*account, pending)?;
    }
    Ok(staged)
}

pub(crate) fn prepare_entry(
    state: &AppState,
    media_id: i64,
    patch: Patch,
    playback_episode: Option<i64>,
    current: Option<&ListEntry>,
) -> Result<Option<(i64, Pending, bool)>> {
    let Some(account) = account_id(&state.db) else {
        return Ok(None);
    };
    let existing = state
        .db
        .pending_change(account, media_id)?
        .filter(|pending| !pending.needs_cleanup_only());
    let already_pending = existing.is_some();
    let mut pending = match existing {
        Some(pending) => pending,
        None => Pending {
            media_id,
            deleting: false,
            deletion_id: None,
            base: state.db.get_entry(media_id)?,
            patch: Patch::default(),
            error: None,
            conflict: false,
            missing_media: false,
            score_format: None,
            playback: false,
            base_score_format: state.db.cached_user()?.and_then(|user| user.score_format),
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        },
    };
    if pending.base.is_none() && pending.patch.creation_key.is_none() {
        pending.patch.creation_key = Some(state.db.0.lock().query_row(
            "SELECT lower(hex(randomblob(16)))",
            [],
            |row| row.get(0),
        )?);
    }
    if pending.deleting {
        anyhow::bail!("Removal is pending. Retry it or keep the AniList version before editing.");
    }
    if pending.missing_media {
        return Err(anyhow::anyhow!(
            "This anime was removed from AniList. Resolve its saved change before continuing."
        ));
    }
    if pending.error.as_deref() == Some(ENTRY_REPLACED) {
        anyhow::bail!(ENTRY_REPLACED);
    }
    if already_pending
        && patch.automatic_completion.is_some()
        && pending.patch.automatic_completion.is_none()
        && !pending.is_playback()
    {
        // A list refresh can advance the cached entry while an unrelated edit is queued.
        pending.patch.completion_baseline = current.and_then(|entry| {
            Some(CompletionBaseline {
                id: entry.id?,
                updated_at: entry.updated_at,
                status: entry.status.clone(),
                repeat: entry.repeat,
            })
        });
    }
    let previous_progress = pending.patch.progress;
    if patch.score.is_some() {
        pending.score_format = state.db.cached_user()?.and_then(|user| user.score_format);
        if pending.base.is_some()
            && comparable_base_score(&pending, pending.score_format.as_deref()).is_none()
        {
            pending.conflict = true;
            pending.error = Some(SCORE_BASELINE_ERROR.into());
            pending.retry_error = None;
        } else if matches!(
            pending.error.as_deref(),
            Some(SCORE_FORMAT_ERROR | SCORE_BASELINE_ERROR)
        ) {
            pending.conflict = false;
            pending.error = None;
            pending.retry_error = None;
        }
    }
    let total = state.db.episode_total(media_id)?;
    if !pending.is_playback() && pending.patch.progress.is_some() {
        pending.patch.reopen_completed(total, pending.base.as_ref());
    }
    pending.merge_entry_patch(&patch, playback_episode, total);
    let forced_completion = pending
        .rejected_progress
        .as_ref()
        .is_some_and(anilist::IncompatibleEntryWrite::forced_completion);
    if playback_episode.is_none()
        && (patch.progress.is_some() || forced_completion && patch.status.is_some())
        && pending.has_progress_conflict()
    {
        let rejected = pending.progress_write_rejection();
        let total = total
            .filter(|total| *total > 0)
            .or(pending.rejected_total)
            .or_else(|| rejected.and_then(|rejected| rejected.total.flatten()));
        let progress = pending.progress_for_total(total);
        let validate_total = total.filter(|_| {
            !forced_completion
                || !matches!(
                    pending.patch.status.as_deref(),
                    Some("CURRENT" | "REPEATING")
                )
        });
        if let Some(error) =
            progress.and_then(|progress| correction_error(progress, validate_total))
        {
            anyhow::bail!(error);
        }
        let corrected = forced_completion
            || if pending.error.as_deref() == Some(anilist::IncompatibleEntryWrite::MESSAGE) {
                rejected.is_some_and(|rejected| {
                    (pending.patch.status.as_ref(), progress)
                        != (
                            rejected.requested_status.as_ref(),
                            rejected.requested_progress,
                        )
                })
            } else {
                total.is_some() || progress < previous_progress
            };
        if corrected {
            pending.conflict = false;
            pending.error = None;
            pending.retry_error = None;
        }
    }
    Ok(Some((account, pending, already_pending)))
}

fn correction_error(progress: i64, total: Option<i64>) -> Option<&'static str> {
    if !(0..=i64::from(i32::MAX)).contains(&progress) {
        Some("Progress must be between 0 and 2147483647")
    } else if total.is_some_and(|total| total > 0 && progress > total) {
        Some(PROGRESS_EXCEEDS_TOTAL)
    } else {
        None
    }
}

pub(crate) fn validate_progress_edit(
    state: &AppState,
    media_id: i64,
    progress: Option<i64>,
    total: Option<i64>,
) -> Result<()> {
    if let Some(error) = progress.and_then(|progress| correction_error(progress, total)) {
        if let Some(account) = account_id(&state.db) {
            if state
                .db
                .pending_change(account, media_id)?
                .is_some_and(|pending| pending.has_progress_conflict())
            {
                anyhow::bail!(error);
            }
        }
    }
    Ok(())
}

pub(crate) fn can_recheck_forced_completion(state: &AppState, media_id: i64) -> Result<bool> {
    let Some(account) = account_id(&state.db) else {
        return Ok(false);
    };
    Ok(state
        .db
        .pending_change(account, media_id)?
        .is_some_and(|pending| {
            (!pending.conflict || pending.has_progress_conflict())
                && pending
                    .rejected_progress
                    .as_ref()
                    .is_some_and(anilist::IncompatibleEntryWrite::forced_completion)
        }))
}

fn apply_display_patch(
    state: &AppState,
    pending: &Pending,
    entry: &mut ListEntry,
    current_baseline: bool,
    total: Option<i64>,
) -> Result<()> {
    let mut patch = pending.patch.clone();
    let snapshot = pending.patch.completion_baseline.as_ref().filter(|base| {
        current_baseline && entry.id == Some(base.id) && entry.updated_at == base.updated_at
    });
    patch.apply_completion_total(total);
    if patch.automatic_completion.is_none() {
        patch.reopen_completed(total, pending.base.as_ref());
    }
    if patch.automatic_completion.is_some()
        && Some(entry.progress) == patch.progress
        && (snapshot.is_some()
            || pending.base.as_ref().map_or_else(
                || entry.id.is_none() && entry.queued_creation == patch.creation_key,
                |base| entry.id == base.id && entry.updated_at == base.updated_at,
            ))
    {
        if patch.status.is_none() && Some(&entry.status) == pending.patch.status.as_ref() {
            entry.status = snapshot.map(|base| base.status.clone()).unwrap_or_else(|| {
                pending
                    .base
                    .as_ref()
                    .map_or_else(|| "CURRENT".into(), |base| base.status.clone())
            });
        }
        if patch.repeat.is_none() && Some(entry.repeat) == pending.patch.repeat {
            entry.repeat = snapshot.map_or_else(
                || pending.base.as_ref().map_or(0, |base| base.repeat),
                |base| base.repeat,
            );
        }
    }
    if pending.is_playback() || patch.status.as_deref() == Some("COMPLETED") {
        if let (Some((status, repeat)), Some(base)) =
            (pending.playback_fields(total), pending.playback_baseline())
        {
            patch.status = status;
            patch.repeat = repeat;
            let playback = pending.patch.playback.as_ref();
            // Undo only the old local finale projection. A list sync can supply newer fields.
            if patch.progress.is_some_and(|progress| {
                (entry.progress == progress
                    || (base.progress < entry.progress && entry.progress < progress))
                    && playback.is_none_or(|playback| playback.episode == progress)
            }) && (snapshot.is_some()
                || entry.id == base.id && entry.updated_at == base.updated_at)
            {
                let matches_projected_status = entry.status == "COMPLETED"
                    || playback.is_some_and(|playback| {
                        playback.manual_status
                            && pending.patch.status.as_ref() == Some(&entry.status)
                    });
                if entry.status == "COMPLETED" && patch.status.is_none() {
                    entry
                        .status
                        .clone_from(snapshot.map_or(&base.status, |base| &base.status));
                }
                if matches_projected_status
                    && patch.repeat.is_none()
                    && base.status == "REPEATING"
                    && entry.repeat == base.repeat.saturating_add(1).min(1000)
                {
                    entry.repeat = snapshot.map_or(base.repeat, |base| base.repeat);
                }
            }
        } else {
            patch.progress = pending.progress_for_total(total);
        }
    }
    // A base snapshot can outlive both the account cache and its score scale.
    if !current_baseline {
        entry.score = None;
    }
    if patch.score.is_some_and(|score| score != 0.0)
        && (pending.score_format.is_none()
            || pending.score_format != state.db.cached_user()?.and_then(|user| user.score_format))
    {
        patch.score = None;
        entry.score = None;
    }
    patch.apply(entry);
    Ok(())
}

pub(crate) fn projected_entry(
    state: &AppState,
    pending: &Pending,
    current: Option<ListEntry>,
) -> Result<ListEntry> {
    let current_baseline = current.is_some();
    let mut entry = current
        .or_else(|| pending.base.clone())
        .unwrap_or(ListEntry {
            media_id: pending.media_id,
            status: "CURRENT".into(),
            ..Default::default()
        });
    let cached = state.db.get_media(pending.media_id)?;
    let total = match &cached {
        Some(media) => Some(media.episodes),
        None => state.db.cached_episode_total(pending.media_id)?,
    };
    apply_display_patch(
        state,
        pending,
        &mut entry,
        current_baseline,
        total.flatten(),
    )?;
    entry.media = cached.or_else(|| pending.saved_media(total));
    if entry.id.is_none() && pending.base.is_none() {
        entry
            .queued_creation
            .clone_from(&pending.patch.creation_key);
    }
    Ok(entry)
}

pub(crate) fn current_entry(state: &AppState, media_id: i64) -> Result<Option<ListEntry>> {
    let stored = state.db.get_entry(media_id)?;
    if let Some(account) = account_id(&state.db) {
        if let Some(pending) = state.db.pending_change(account, media_id)? {
            if overlay_allowed(state, account, &pending, stored.as_ref())?
                && (pending.is_playback() || pending.patch.automatic_completion.is_some())
                && !pending.deleting
                && !pending.missing_media
            {
                return projected_entry(state, &pending, stored).map(Some);
            }
        }
    }
    Ok(stored)
}

pub(crate) fn overlay_allowed(
    state: &AppState,
    account: i64,
    pending: &Pending,
    stored: Option<&ListEntry>,
) -> Result<bool> {
    if pending.needs_cleanup_only() {
        return Ok(true);
    }
    if pending.error.as_deref() == Some(ENTRY_REPLACED) && pending.conflict {
        return Ok(false);
    }
    if !pending.identity_replaced_by(stored) {
        return Ok(true);
    }
    let origin = state.origin();
    anyhow::ensure!(origin.account_id == Some(account), "Account changed");
    let intent = state
        .auth_intent_guard(origin.generation)
        .map_err(anyhow::Error::msg)?;
    let mut retained = pending.clone();
    retained.mark_entry_replaced();
    state.db.put_pending(account, &retained)?;
    drop(intent);
    entry_changed_from(state, origin, pending.media_id);
    Ok(false)
}

pub(crate) fn cache_entry_replacement(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    pending: &Pending,
    entry: Option<&ListEntry>,
) -> Result<bool, String> {
    let Some(entry) = entry.filter(|entry| pending.identity_replaced_by(Some(entry))) else {
        return Ok(false);
    };
    let intent = crate::commands::write_origin_guard(state, origin, token)?;
    let mut retained = pending.clone();
    retained.mark_entry_replaced();
    state
        .db
        .durable_write(|tx| {
            Db::write_pending(tx, account, &retained)?;
            Db::write_entry(tx, entry)?;
            anyhow::ensure!(tx.changes() == 1, "the replacement entry was not cached");
            Ok(())
        })
        .map_err(|err| err.to_string())?;
    drop(intent);
    state.refresh_matchers();
    entry_changed_from(state, origin, pending.media_id);
    Ok(true)
}

#[cfg(test)]
pub(crate) fn local_entry(state: &AppState, pending: &Pending) -> Result<ListEntry> {
    let origin = state.origin();
    let intent = state
        .auth_intent_guard(origin.generation)
        .map_err(anyhow::Error::msg)?;
    let entry = projected_entry(state, pending, state.db.get_entry(pending.media_id)?)?;
    state.db.upsert_entry(&entry)?;
    drop(intent);
    state.refresh_matchers();
    entry_changed_from(state, origin, pending.media_id);
    Ok(entry)
}

pub(crate) fn commit_pending_projection(
    state: &AppState,
    origin: crate::EventOrigin,
    intent: tokio::sync::watch::Ref<'_, u64>,
    account: i64,
    pending: &Pending,
    current: Option<ListEntry>,
) -> Result<ListEntry, String> {
    let entry = projected_entry(state, pending, current).map_err(|e| e.to_string())?;
    state
        .db
        .durable_write(|tx| {
            Db::write_pending(tx, account, pending)?;
            Db::write_entry(tx, &entry)
        })
        .map_err(|e| e.to_string())?;
    drop(intent);
    state.refresh_matchers();
    entry_changed_from(state, origin, pending.media_id);
    Ok(entry)
}

pub(crate) fn overlay(state: &AppState) -> Result<std::collections::HashMap<i64, crate::Media>> {
    let mut saved_media = std::collections::HashMap::new();
    if let Some(account) = account_id(&state.db) {
        let pending = state.db.pending(account)?;
        let mut matchers_changed = false;
        let result = pending.into_iter().try_for_each(|pending| -> Result<()> {
            if pending.needs_cleanup_only() {
                return Ok(());
            }
            let stored = state.db.get_entry(pending.media_id)?;
            if !overlay_allowed(state, account, &pending, stored.as_ref())? {
                return Ok(());
            }
            if pending.deleting {
                if let Some(media) =
                    pending.saved_media(state.db.cached_episode_total(pending.media_id)?)
                {
                    saved_media.insert(pending.media_id, media);
                }
                return Ok(());
            }
            if pending.missing_media {
                if stored.is_some() {
                    state.db.delete_entry(pending.media_id)?;
                    matchers_changed = true;
                }
                return Ok(());
            }
            let mut entry = projected_entry(state, &pending, stored.clone())?;
            if stored.as_ref().is_none_or(|row| row.media.is_none()) {
                if let Some(media) = entry.media.take() {
                    saved_media.insert(entry.media_id, media);
                }
            }
            if stored.as_ref().is_some_and(|stored| {
                stored.status == entry.status
                    && stored.progress == entry.progress
                    && stored.score == entry.score
                    && stored.repeat == entry.repeat
                    && stored.queued_creation == entry.queued_creation
            }) {
                return Ok(());
            }
            state.db.upsert_entry(&entry)?;
            matchers_changed |= stored
                .as_ref()
                .is_none_or(|stored| stored.status != entry.status);
            Ok(())
        });
        if matchers_changed {
            state.refresh_matchers();
        }
        result?;
    }
    Ok(saved_media)
}

#[cfg(test)]
pub(crate) fn mark_media_missing(state: &AppState, media_id: i64) -> Result<()> {
    let origin = state.origin();
    let intent = state
        .auth_intent_guard(origin.generation)
        .map_err(anyhow::Error::msg)?;
    mark_media_missing_from(state, origin, intent, media_id, false)
}

pub(crate) fn mark_media_missing_from(
    state: &AppState,
    origin: crate::EventOrigin,
    intent: tokio::sync::watch::Ref<'_, u64>,
    media_id: i64,
    remove_media: bool,
) -> Result<()> {
    let pending = match origin.account_id {
        Some(account) => state
            .db
            .pending_change(account, media_id)?
            .map(|p| (account, p)),
        None => None,
    };
    commit_missing_media(state, origin, intent, media_id, pending, remove_media)
}

pub(crate) fn commit_missing_media(
    state: &AppState,
    origin: crate::EventOrigin,
    intent: tokio::sync::watch::Ref<'_, u64>,
    media_id: i64,
    pending: Option<(i64, Pending)>,
    remove_media: bool,
) -> Result<()> {
    let pending = pending.filter(|(_, pending)| !pending.needs_cleanup_only());
    if pending.is_none() && !remove_media {
        return Ok(());
    }
    state
        .db
        .commit_media_removal(remove_media.then_some(media_id), |tx| {
            if let Some((account, mut pending)) = pending {
                pending.mark_media_missing();
                Db::write_pending(tx, account, &pending)?;
            }
            tx.execute("DELETE FROM list_entry WHERE media_id = ?", [media_id])?;
            Ok(())
        })?;
    drop(intent);
    state.clear_tracking_prompt_for_media(origin, media_id);
    state.refresh_matchers();
    entry_changed_from(state, origin, media_id);
    Ok(())
}

pub(crate) fn remote_entry(media_id: i64, remote: anilist::SavedEntry) -> ListEntry {
    ListEntry {
        id: Some(remote.id),
        media_id,
        status: remote.status,
        progress: remote.progress,
        score: remote.score,
        repeat: remote.repeat,
        updated_at: remote.updated_at,
        ..Default::default()
    }
}

fn entry_changed_from(state: &AppState, origin: crate::EventOrigin, media_id: i64) {
    #[cfg(test)]
    ENTRY_NOTIFICATIONS.set(ENTRY_NOTIFICATIONS.get() + 1);
    state.emit_from(
        origin,
        crate::EventKind::PendingChanged(Some(vec![media_id])),
    );
}

pub(crate) fn publish_from(state: &AppState, origin: crate::EventOrigin, entry: &ListEntry) {
    state.emit_from(origin, crate::EventKind::EpisodeUpdated(entry.clone()));
}

#[cfg(test)]
pub(crate) fn conflict(pending: &Pending, remote: Option<&ListEntry>) -> bool {
    conflict_in_format(pending, remote, pending.score_format.as_deref())
}

pub(crate) fn conflict_in_format(
    pending: &Pending,
    remote: Option<&ListEntry>,
    score_format: Option<&str>,
) -> bool {
    conflict_at_progress(pending, remote, pending.patch.progress, None, score_format)
}

fn conflict_at_progress(
    pending: &Pending,
    remote: Option<&ListEntry>,
    progress: Option<i64>,
    remote_total: Option<i64>,
    score_format: Option<&str>,
) -> bool {
    let rejected = pending.progress_write_rejection();
    let Some(remote) = remote else {
        return pending.base.is_some() || rejected.is_some();
    };
    if rejected.is_some_and(|rejected| !rejected.matches_entry(remote)) {
        return true;
    }
    let p = &pending.patch;
    if let Some(base) = &pending.base {
        let status_changed = (p.status.is_some() || progress.is_some())
            && remote.status != base.status
            && p.status.as_ref() != Some(&remote.status);
        let progress_changed = progress.is_some_and(|v| {
            remote.progress != base.progress
                && remote.progress != v
                && p.progress != Some(remote.progress)
        });
        let explains_status = rejected.is_some_and(|rejected| {
            rejected.requested_status.is_some()
                || (rejected.clamped_active_entry(base)
                    && remote_total.is_none_or(|total| total == rejected.progress))
        });
        if base.id != remote.id
            || (status_changed && !explains_status)
            || (progress_changed && rejected.is_none())
        {
            return true;
        }
    } else if rejected.is_none() {
        return true;
    }
    let base_repeat = pending.base.as_ref().map(|base| base.repeat);
    p.score.is_some_and(|v| {
        remote.score != Some(v)
            && comparable_base_score(pending, score_format)
                .is_none_or(|score| remote.score != Some(score))
    }) || ((p.repeat.is_some() || progress.is_some() && base_repeat.is_some())
        && base_repeat != Some(remote.repeat)
        && p.repeat != Some(remote.repeat))
}

pub(crate) fn already_applied(pending: &Pending, remote: &ListEntry) -> bool {
    already_applied_at_progress(pending, remote, pending.patch.progress)
}

fn already_applied_at_progress(
    pending: &Pending,
    remote: &ListEntry,
    progress: Option<i64>,
) -> bool {
    // A local creation key and matching fields do not identify an AniList entry.
    if pending.identity_replaced_by(Some(remote)) {
        return false;
    }
    if pending.base.as_ref().is_some_and(|base| {
        base.id != remote.id
            || (progress.is_some() && remote.repeat != pending.patch.repeat.unwrap_or(base.repeat))
    }) {
        return false;
    }
    let p = &pending.patch;
    p.status.as_ref().is_none_or(|v| remote.status == *v)
        && progress.is_none_or(|v| remote.progress == v)
        && p.score.is_none_or(|v| remote.score == Some(v))
        && p.repeat.is_none_or(|v| remote.repeat == v)
}

#[cfg(test)]
pub(crate) async fn flush_unlocked(state: &AppState) -> Result<(), String> {
    flush_from(state, state.origin())
        .await
        .map_err(String::from)
}

/// Caller holds remote_lock. A saved account ID only identifies the local queue owner.
pub(crate) async fn confirm_write_account(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
) -> Result<(), String> {
    let token = al.token();
    match verify_write_account(state, origin, al, account).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _entry = state.entry_lock.lock().await;
            crate::commands::check_write_origin(state, origin, &token)?;
            Err(crate::commands::write_err(state, origin, &token, &e))
        }
    }
}

pub(crate) async fn verify_write_account(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
) -> Result<()> {
    let token = al.token();
    crate::commands::check_write_origin(state, origin, &token).map_err(anyhow::Error::msg)?;
    if origin.account_id.is_some_and(|owner| owner != account) {
        anyhow::bail!("Account changed");
    }
    if state.anilist.lock().verified_account() == Some(account) {
        return Ok(());
    }
    if origin.account_id.is_none() {
        anyhow::bail!("Account changed");
    }
    let result = al.viewer().await;
    let _entry = state.entry_lock.lock().await;
    let _intent =
        crate::commands::write_origin_guard(state, origin, &token).map_err(anyhow::Error::msg)?;
    let user = match result {
        Err(e) if anilist::media_not_found(&e) => {
            anyhow::bail!("AniList could not verify the saved account: {e}")
        }
        result => result?,
    };
    state.anilist.lock().record_viewer(user.id);
    if user.id != account {
        anyhow::bail!("Account changed");
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum EntryFlushError {
    Entry(String),
    Batch(String),
}

impl EntryFlushError {
    pub(crate) fn from_remote(error: &anyhow::Error, message: String) -> Self {
        if anilist::is_entry_retry_error(error) {
            Self::Entry(message)
        } else {
            Self::Batch(message)
        }
    }

    fn record(self, first_error: &mut Option<Self>) -> Result<(), Self> {
        match self {
            Self::Entry(_) => {
                first_error.get_or_insert(self);
                Ok(())
            }
            Self::Batch(_) => Err(self),
        }
    }
}

impl From<String> for EntryFlushError {
    fn from(message: String) -> Self {
        Self::Batch(message)
    }
}

impl From<EntryFlushError> for String {
    fn from(error: EntryFlushError) -> Self {
        match error {
            EntryFlushError::Entry(message) | EntryFlushError::Batch(message) => message,
        }
    }
}

fn retry_cursor_key(account: i64) -> String {
    format!("pending_retry_cursor:{account}")
}

fn retry_after(db: &Db, account: i64) -> Result<Option<i64>, EntryFlushError> {
    Ok(db
        .get_setting(&retry_cursor_key(account))
        .map_err(|error| error.to_string())?
        .and_then(|value| value.parse().ok()))
}

/// Caller holds remote_lock so queued writes and account replacement remain ordered.
pub(crate) async fn flush_from(
    state: &AppState,
    origin: crate::EventOrigin,
) -> Result<(), EntryFlushError> {
    let _guard = state.entry_lock.lock().await;
    if !state.origin_is_current(origin) {
        return Err(EntryFlushError::Batch("Account changed".into()));
    }
    let Some(account) = origin.account_id else {
        return Ok(());
    };
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    if !al.has_token() {
        return Ok(());
    }
    let mut pending = state
        .db
        .pending(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| !p.conflict)
        .collect::<Vec<_>>();
    if let Some(after) = retry_after(&state.db, account)? {
        let split = pending.partition_point(|pending| pending.media_id <= after);
        pending.rotate_left(split);
    }
    pending.truncate(10);
    drop(_guard);
    if pending.iter().any(|pending| !pending.needs_cleanup_only()) {
        confirm_write_account(state, origin, &al, account).await?;
    }
    let mut first_error = None;
    for pending in pending {
        if let Err(error) = flush_one_unlocked(state, origin, &al, account, pending).await {
            error.record(&mut first_error)?;
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub(crate) async fn flush_background(state: &AppState) -> Result<(), String> {
    let origin = state.origin();
    let (account, token, media_ids) = {
        let _remote = state.remote_lock.lock().await;
        let _guard = state.entry_lock.lock().await;
        if !state.origin_is_current(origin) {
            return Ok(());
        }
        let Some(account) = account_id(&state.db) else {
            return Ok(());
        };
        let token = state.anilist.lock().token();
        if token.is_none() {
            return Ok(());
        }
        let media_ids = state
            .db
            .pending_background_ids(account, retry_after(&state.db, account)?)
            .map_err(|e| e.to_string())?;
        (account, token, media_ids)
    };
    let mut first_error = None;
    for media_id in media_ids {
        // Tokio's FIFO mutex admits waiting edits between complete remote writes.
        let _remote = state.remote_lock.lock().await;
        let _guard = state.entry_lock.lock().await;
        let al = state
            .anilist
            .lock()
            .for_intent(&state.auth_intent, origin.generation);
        if account_id(&state.db) != Some(account)
            || al.token() != token
            || !state.origin_is_current(origin)
        {
            return Ok(());
        }
        let pending = state
            .db
            .pending_change(account, media_id)
            .map_err(|e| e.to_string())?
            .filter(|pending| !pending.conflict);
        drop(_guard);
        if let Some(pending) = pending {
            if !pending.needs_cleanup_only() {
                confirm_write_account(state, origin, &al, account).await?;
            }
            if let Err(error) = flush_one_unlocked(state, origin, &al, account, pending).await {
                error.record(&mut first_error)?;
            }
        }
    }
    first_error.map_or(Ok(()), |error| Err(error.into()))
}

/// Caller holds remote_lock through preflight, mutation and local acknowledgement.
async fn flush_one_unlocked(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
    mut pending: Pending,
) -> Result<(), EntryFlushError> {
    let token = al.token();
    crate::commands::check_write_origin(state, origin, &token)?;
    if pending.needs_cleanup_only() {
        let _guard = state.entry_lock.lock().await;
        remove_acknowledged_pending(state, origin, &token, account, pending.media_id)?;
        return Ok(());
    }
    if pending.deleting {
        return crate::commands::delete_entry_unlocked(pending.media_id, false, state, origin)
            .await;
    }
    {
        let _guard = state.entry_lock.lock().await;
        let stored = state
            .db
            .get_entry(pending.media_id)
            .map_err(|e| e.to_string())?;
        if !overlay_allowed(state, account, &pending, stored.as_ref()).map_err(|e| e.to_string())? {
            return Ok(());
        }
    }
    check_pending_score_format(state, origin, al, account, &mut pending).await?;
    let query_format = state.cached_score_format()?;
    let result = al
        .entry_by_media_id_with_episodes(pending.media_id, query_format.as_deref())
        .await;
    let _guard = state.entry_lock.lock().await;
    crate::commands::check_write_origin(state, origin, &token)?;
    let (remote, remote_episodes) = match result {
        Ok((remote, episodes)) => {
            cache_reported_episodes(state, origin, &token, pending.media_id, episodes)?;
            (
                remote.map(|e| remote_entry(pending.media_id, e)),
                episodes.flatten(),
            )
        }
        Err(e) if anilist::media_not_found(&e) => {
            let intent = crate::commands::write_origin_guard(state, origin, &token)?;
            mark_media_missing_from(state, origin, intent, pending.media_id, false)
                .map_err(|e| e.to_string())?;
            return Err(e.to_string().into());
        }
        Err(e) => {
            let conflict = !anilist::is_retryable(&e) && !anilist::is_auth_rejection(&e);
            let entry_retry_error = anilist::is_entry_retry_error(&e);
            let (pending_changed, write_error) =
                crate::commands::settle_write_error(state, origin, &token, &e, |message| {
                    let Some(mut current) = state
                        .db
                        .pending_change(account, pending.media_id)
                        .map_err(|e| e.to_string())?
                    else {
                        return Ok(false);
                    };
                    let next_conflict = current.conflict || conflict;
                    if current.error.as_deref() == Some(message)
                        && current.conflict == next_conflict
                    {
                        if entry_retry_error {
                            state
                                .db
                                .durable_write(|tx| {
                                    Db::write_retry_position(tx, account, pending.media_id)
                                })
                                .map_err(|e| e.to_string())?;
                        }
                        return Ok(false);
                    }
                    current.error = Some(message.into());
                    current.conflict = next_conflict;
                    state
                        .db
                        .put_pending_with_retry_position(account, &current, entry_retry_error)
                        .map_err(|e| e.to_string())?;
                    Ok(true)
                })?;
            if pending_changed && !anilist::is_auth_rejection(&e) {
                entry_changed_from(state, origin, pending.media_id);
            }
            return Err(EntryFlushError::from_remote(&e, write_error));
        }
    };
    drop(_guard);
    // Equal numbers on different scales cannot acknowledge a saved score.
    check_pending_score_format(state, origin, al, account, &mut pending).await?;
    let _guard = state.entry_lock.lock().await;
    crate::commands::check_write_origin(state, origin, &token)?;
    if cache_entry_replacement(state, origin, &token, account, &pending, remote.as_ref())? {
        return Ok(());
    }
    let total = remote_episodes.or_else(|| state.db.episode_total(pending.media_id).ok().flatten());
    if !reconcile_queued_playback(
        state,
        origin,
        &token,
        account,
        &mut pending,
        remote_episodes,
    )? {
        return Ok(());
    }
    let baseline = pending.progress_write_rejection().and_then(|rejected| {
        remote
            .as_ref()
            .filter(|entry| rejected.matches_entry(entry))
    });
    let replay = pending.replay_at_total(baseline, remote_episodes, total);
    if pending.patch.automatic_completion.is_none() {
        pending.patch.status.clone_from(&replay.patch.status);
    }
    let progress = replay.progress_for_total(total);
    let overshoot = pending.overshoot_total(progress, remote_episodes, total);
    if let Some(entry) = remote.as_ref().filter(|entry| {
        overshoot.is_none()
            && pending
                .progress_write_rejection()
                .is_none_or(|rejected| rejected.matches_entry(entry))
            && already_applied_at_progress(&replay, entry, progress)
    }) {
        return acknowledge_remote_entry(state, origin, &token, account, entry.clone())
            .map_err(Into::into);
    }
    if conflict_at_progress(
        &replay,
        remote.as_ref(),
        progress,
        remote_episodes,
        query_format.as_deref(),
    ) {
        let intent = crate::commands::write_origin_guard(state, origin, &token)?;
        pending.conflict = true;
        pending.error = Some(
            "AniList changed while this update was waiting. Choose which version to keep.".into(),
        );
        state
            .db
            .put_pending(account, &pending)
            .map_err(|e| e.to_string())?;
        drop(intent);
        entry_changed_from(state, origin, pending.media_id);
        return Ok(());
    }
    drop(_guard);
    send_pending(
        state,
        origin,
        al,
        account,
        pending,
        remote,
        remote_episodes,
        true,
    )
    .await
    .map(drop)
}

pub(crate) fn cache_reported_episodes(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    media_id: i64,
    episodes: anilist::ReportedEpisodes,
) -> Result<(), String> {
    if let Some(total) = episodes {
        let intent = crate::commands::write_origin_guard(state, origin, token)?;
        if state
            .db
            .cache_episode_total(media_id, total)
            .map_err(|e| e.to_string())?
        {
            drop(intent);
            state.refresh_matchers();
        }
    }
    Ok(())
}

pub(crate) async fn prepare_score_confirmation(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    media_id: i64,
    score: Option<f64>,
    result: &Result<(anilist::SavedEntry, Option<i64>)>,
) -> Result<bool, String> {
    if !score.is_some_and(|score| score != 0.0) {
        return Ok(false);
    }
    let total = match result {
        Ok((_, total)) => total.map(Some),
        Err(error) => match error.downcast_ref::<anilist::IncompatibleEntryWrite>() {
            Some(rejected) if rejected.score_or_repeat_only => rejected.total,
            _ => return Ok(false),
        },
    };
    // A later Viewer failure cannot invalidate a confirmed episode total.
    if let Some(total) = total {
        let _entry = state.entry_lock.lock().await;
        match crate::commands::write_origin_guard(state, origin, token) {
            Ok(intent) => match state.db.cache_episode_total(media_id, total) {
                Ok(true) => {
                    drop(intent);
                    state.refresh_matchers();
                }
                Err(error) if result.is_err() => return Err(error.to_string()),
                // Confirmed saves retry the cache write during acknowledgement.
                _ => {}
            },
            Err(error) => {
                if result.is_ok() {
                    return Err(error);
                }
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn remove_acknowledged_pending(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    media_id: i64,
) -> Result<(), String> {
    let intent = crate::commands::write_origin_guard(state, origin, token)?;
    state
        .db
        .remove_pending(account, media_id)
        .map_err(|e| e.to_string())?;
    drop(intent);
    entry_changed_from(state, origin, media_id);
    Ok(())
}

fn acknowledge_remote_entry(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    mut entry: ListEntry,
) -> Result<(), String> {
    let intent = crate::commands::write_origin_guard(state, origin, token)?;
    state
        .db
        .cache_acknowledged_entry(Some(account), &mut entry, None)
        .map_err(|e| e.to_string())?;
    entry.media = state
        .db
        .get_media(entry.media_id)
        .map_err(|e| e.to_string())?;
    let acknowledgement = state.db.remove_pending(account, entry.media_id);
    drop(intent);
    state.refresh_matchers();
    publish_from(state, origin, &entry);
    entry_changed_from(state, origin, entry.media_id);
    acknowledgement.map_err(|e| e.to_string())
}

fn acknowledge_confirmed_entry(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    mut entry: ListEntry,
    confirmed_total: Option<i64>,
) -> Result<(), String> {
    let origin_result = crate::commands::write_origin_guard(state, origin, token);
    if let Err(origin_error) = &origin_result {
        // Settle a confirmed write before a waiting auth change takes remote_lock.
        if !crate::commands::write_session_account_matches(state, origin, token) {
            if let Err(error) = state.db.remove_pending(account, entry.media_id) {
                state
                    .db
                    .mark_acknowledged_pending(account, &entry)
                    .map_err(|e| e.to_string())?;
                return Err(error.to_string());
            }
            return Err(origin_error.clone());
        }
    }
    state
        .db
        .cache_acknowledged_entry(Some(account), &mut entry, confirmed_total)
        .map_err(|e| e.to_string())?;
    entry.media = state
        .db
        .get_media(entry.media_id)
        .map_err(|e| e.to_string())?;
    let acknowledgement = state.db.remove_pending(account, entry.media_id);
    match origin_result {
        Ok(intent) => {
            drop(intent);
            state.refresh_matchers();
            publish_from(state, origin, &entry);
            entry_changed_from(state, origin, entry.media_id);
            acknowledgement.map_err(|e| e.to_string())
        }
        Err(origin_error) => {
            state.refresh_matchers();
            acknowledgement.map_err(|e| e.to_string())?;
            Err(origin_error)
        }
    }
}

fn keep_anilist_entry(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    media_id: i64,
    mut entry: Option<ListEntry>,
    reported_total: anilist::ReportedEpisodes,
) -> Result<(), String> {
    let intent = crate::commands::write_origin_guard(state, origin, token)?;
    if let Some(entry) = &mut entry {
        entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
        if let (Some(media), Some(total)) = (&mut entry.media, reported_total) {
            media.episodes = total;
        }
    }
    state
        .db
        .restore_and_remove_pending(account, media_id, entry.as_ref(), reported_total)
        .map_err(|e| e.to_string())?;
    drop(intent);
    if entry.is_none() {
        state.clear_tracking_prompt_for_media(origin, media_id);
    }
    state.refresh_matchers();
    if let Some(entry) = &entry {
        publish_from(state, origin, entry);
    }
    entry_changed_from(state, origin, media_id);
    Ok(())
}

pub(crate) struct ScoreFormatError {
    pub message: String,
    pub format_mismatch: bool,
    pub retryable: bool,
    pub auth_rejected: bool,
    pub account_changed: bool,
}

async fn check_pending_score_format(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
    pending: &mut Pending,
) -> Result<(), String> {
    if let Err(error) = check_score_format_detailed(state, origin, al, account, pending, true).await
    {
        if error.auth_rejected || error.account_changed {
            return Err(error.message);
        }
        let _guard = state.entry_lock.lock().await;
        let intent = crate::commands::write_origin_guard(state, origin, &al.token())?;
        if error.format_mismatch || !error.retryable && !pending.conflict {
            pending.record_conflict(error.message.clone());
        } else {
            pending.record_retry_error(&error.message);
        }
        state
            .db
            .put_pending(account, pending)
            .map_err(|e| e.to_string())?;
        drop(intent);
        entry_changed_from(state, origin, pending.media_id);
        return Err(error.message);
    }
    Ok(())
}

async fn confirm_pending_score_format(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
    pending: &mut Pending,
    rejected: Option<&anilist::IncompatibleEntryWrite>,
    remote_episodes: Option<i64>,
) -> Result<(), String> {
    let Some(rejected) = rejected else {
        return check_pending_score_format(state, origin, al, account, pending).await;
    };
    let mut refused = pending.clone();
    refused.record_incompatible_write(rejected.clone(), remote_episodes);
    if let Err(error) =
        check_score_format_detailed(state, origin, al, account, &refused, true).await
    {
        if error.auth_rejected {
            return Err(error.message);
        }
        let _guard = state.entry_lock.lock().await;
        if error.account_changed {
            let intent = crate::commands::write_origin_guard(state, origin, &al.token()).ok();
            state
                .db
                .put_pending(account, &refused)
                .map_err(|e| e.to_string())?;
            if let Some(intent) = intent {
                drop(intent);
                entry_changed_from(state, origin, pending.media_id);
            }
            return Err(error.message);
        }
        let intent = match crate::commands::write_origin_guard(state, origin, &al.token()) {
            Ok(intent) => intent,
            Err(origin_error) => {
                if error.format_mismatch {
                    refused.record_conflict(error.message);
                }
                state
                    .db
                    .put_pending(account, &refused)
                    .map_err(|e| e.to_string())?;
                return Err(origin_error);
            }
        };
        if error.format_mismatch {
            refused.record_conflict(error.message.clone());
        } else {
            refused.record_retry_error(&error.message);
        }
        *pending = refused;
        state
            .db
            .put_pending(account, pending)
            .map_err(|e| e.to_string())?;
        drop(intent);
        entry_changed_from(state, origin, pending.media_id);
        return Err(error.message);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) async fn check_score_format(
    state: &AppState,
    al: &anilist::AniList,
    account: i64,
    pending: &Pending,
) -> Result<(), String> {
    check_score_format_detailed(state, state.origin(), al, account, pending, false)
        .await
        .map_err(|error| error.message)
}

pub(crate) async fn check_score_format_detailed(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
    pending: &Pending,
    persist_on_auth_rejection: bool,
) -> Result<(), ScoreFormatError> {
    let replaced = |message| ScoreFormatError {
        message,
        format_mismatch: false,
        retryable: false,
        auth_rejected: false,
        account_changed: true,
    };
    let token = al.token();
    crate::commands::check_write_origin(state, origin, &token).map_err(replaced)?;
    if pending.patch.score.is_none_or(|score| score == 0.0) {
        return Ok(());
    }
    let Some(format) = pending.score_format.as_deref() else {
        return Err(ScoreFormatError {
            message: SCORE_FORMAT_ERROR.into(),
            format_mismatch: true,
            retryable: false,
            auth_rejected: false,
            account_changed: false,
        });
    };
    if !matches!(
        format,
        "POINT_100" | "POINT_10_DECIMAL" | "POINT_10" | "POINT_5" | "POINT_3"
    ) {
        return Err(ScoreFormatError {
            message: SCORE_FORMAT_ERROR.into(),
            format_mismatch: true,
            retryable: false,
            auth_rejected: false,
            account_changed: false,
        });
    }
    let result = al.viewer().await;
    let _guard = state.entry_lock.lock().await;
    crate::commands::check_write_origin(state, origin, &token).map_err(replaced)?;
    let user = result.map_err(|e| {
        let auth_rejected = anilist::is_auth_rejection(&e);
        let message = if auth_rejected {
            match crate::commands::settle_write_error(state, origin, &token, &e, |message| {
                if persist_on_auth_rejection {
                    let mut retained = pending.clone();
                    retained.record_retry_error(message);
                    state
                        .db
                        .put_pending(account, &retained)
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }) {
                Ok(((), message)) => message,
                Err(message) => return replaced(message),
            }
        } else {
            e.to_string()
        };
        ScoreFormatError {
            retryable: anilist::is_retryable(&e),
            auth_rejected,
            message,
            format_mismatch: false,
            account_changed: false,
        }
    })?;
    let _intent = crate::commands::write_origin_guard(state, origin, &token).map_err(replaced)?;
    state.anilist.lock().record_viewer(user.id);
    if user.id != account {
        return Err(ScoreFormatError {
            message: "Account changed".into(),
            format_mismatch: false,
            retryable: false,
            auth_rejected: false,
            account_changed: true,
        });
    }
    let matches = user.score_format.as_deref() == Some(format);
    state
        .db
        .replace_account(token.as_deref().expect("validated session token"), &user)
        .map_err(|e| ScoreFormatError {
            message: e.to_string(),
            format_mismatch: false,
            retryable: false,
            auth_rejected: false,
            account_changed: false,
        })?;
    *state.user.lock() = Some(user);
    if !matches {
        return Err(ScoreFormatError {
            message: SCORE_FORMAT_ERROR.into(),
            format_mismatch: true,
            retryable: false,
            auth_rejected: false,
            account_changed: false,
        });
    }
    Ok(())
}

fn reconcile_queued_playback(
    state: &AppState,
    origin: crate::EventOrigin,
    token: &Option<String>,
    account: i64,
    pending: &mut Pending,
    remote_total: Option<i64>,
) -> Result<bool, String> {
    if !pending.is_playback() {
        return Ok(true);
    }
    let intent = crate::commands::write_origin_guard(state, origin, token)?;
    let previous = (pending.patch.status.clone(), pending.patch.repeat);
    let in_range = pending.refresh_playback_total(state, remote_total)?;
    let conflict_changed = if !in_range {
        let total = remote_total.or(state
            .db
            .episode_total(pending.media_id)
            .map_err(|e| e.to_string())?);
        pending.record_progress_conflict(crate::commands::EPISODE_EXCEEDS_TOTAL, total)
    } else {
        false
    };
    let pending_changed =
        conflict_changed || previous != (pending.patch.status.clone(), pending.patch.repeat);
    let stored = state
        .db
        .get_entry(pending.media_id)
        .map_err(|e| e.to_string())?;
    let entry = projected_entry(state, pending, stored.clone()).map_err(|e| e.to_string())?;
    let entry_changed = pending.error.as_deref() != Some(ENTRY_REPLACED)
        && !pending.identity_replaced_by(stored.as_ref())
        && stored.as_ref().is_none_or(|stored| {
            stored.status != entry.status
                || stored.progress != entry.progress
                || stored.repeat != entry.repeat
                || stored.score != entry.score
        });
    if pending_changed && entry_changed {
        state
            .db
            .durable_write(|tx| {
                Db::write_pending(tx, account, pending)?;
                Db::write_entry(tx, &entry)
            })
            .map_err(|e| e.to_string())?;
    } else if pending_changed {
        state
            .db
            .put_pending(account, pending)
            .map_err(|e| e.to_string())?;
    } else if entry_changed {
        state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
    }
    drop(intent);
    if entry_changed {
        state.refresh_matchers();
        publish_from(state, origin, &entry);
    }
    if pending_changed || entry_changed {
        entry_changed_from(state, origin, pending.media_id);
    }
    Ok(in_range)
}

#[allow(clippy::too_many_arguments)]
async fn send_pending(
    state: &AppState,
    origin: crate::EventOrigin,
    al: &anilist::AniList,
    account: i64,
    mut pending: Pending,
    remote: Option<ListEntry>,
    remote_episodes: Option<i64>,
    advance_retry_position: bool,
) -> Result<Option<String>, EntryFlushError> {
    if pending.missing_media {
        return Err(EntryFlushError::Batch(
            "AniList no longer has this anime. Its saved change can be discarded.".into(),
        ));
    }
    let total = remote_episodes.or_else(|| state.db.episode_total(pending.media_id).ok().flatten());
    let replay = pending.replay_at_total(remote.as_ref(), remote_episodes, total);
    if pending.patch.automatic_completion.is_none() {
        pending.patch.status.clone_from(&replay.patch.status);
    }
    let progress = replay.progress_for_total(total);
    let token = al.token();
    let overshoot = pending.overshoot_total(progress, remote_episodes, total);
    let rejection = overshoot
        .map(|_| PROGRESS_EXCEEDS_TOTAL)
        .or_else(|| replay.progress_rejection(remote.as_ref(), remote_episodes, progress));
    if let Some(error) = rejection {
        let _guard = state.entry_lock.lock().await;
        let intent = crate::commands::write_origin_guard(state, origin, &token)?;
        if pending.record_progress_conflict(error, overshoot) {
            state
                .db
                .put_pending(account, &pending)
                .map_err(|e| e.to_string())?;
            drop(intent);
            entry_changed_from(state, origin, pending.media_id);
        }
        return Ok(Some(error.into()));
    }
    let entry_id = remote.and_then(|entry| entry.id);
    let p = &replay.patch;
    let status = p
        .status
        .as_deref()
        .map(crate::commands::parse_status)
        .transpose()?;
    let score_format = state.cached_score_format()?;
    crate::commands::check_write_origin(state, origin, &token)?;
    let saved = al
        .save_entry(
            pending.media_id,
            entry_id,
            status,
            progress,
            p.score,
            p.repeat,
            score_format.as_deref(),
            replay.allows_completion_adjustment(),
        )
        .await;
    if prepare_score_confirmation(state, origin, &token, pending.media_id, p.score, &saved).await? {
        let rejected = saved
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<anilist::IncompatibleEntryWrite>());
        confirm_pending_score_format(
            state,
            origin,
            al,
            account,
            &mut pending,
            rejected,
            remote_episodes,
        )
        .await?;
    }
    let _guard = state.entry_lock.lock().await;
    match saved {
        Ok((saved, confirmed_total)) => {
            let mut entry = remote_entry(pending.media_id, saved);
            // Only this creation's mutation can confirm its new identity.
            if entry_id.is_none() && pending.base.is_none() {
                entry.queued_creation = pending.patch.creation_key;
            }
            acknowledge_confirmed_entry(state, origin, &token, account, entry, confirmed_total)?;
            Ok(None)
        }
        Err(e)
            if !anilist::is_retryable(&e)
                && !anilist::is_auth_rejection(&e)
                && !e.is::<anilist::UnsentRequest>() =>
        {
            // Settle this account's dispatched mutation before a waiting login takes remote_lock.
            let rejected = e.downcast_ref::<anilist::IncompatibleEntryWrite>();
            let missing = anilist::media_not_found(&e);
            if let Some(rejected) = rejected {
                pending.record_incompatible_write(rejected.clone(), remote_episodes);
            } else if missing && entry_id.is_none() {
                match crate::commands::write_origin_guard(state, origin, &token) {
                    Ok(intent) => {
                        let media_id = pending.media_id;
                        commit_missing_media(
                            state,
                            origin,
                            intent,
                            media_id,
                            Some((account, pending)),
                            false,
                        )
                        .map_err(|e| e.to_string())?;
                        return Err(e.to_string().into());
                    }
                    Err(error) => {
                        pending.mark_media_missing();
                        state
                            .db
                            .put_pending(account, &pending)
                            .map_err(|e| e.to_string())?;
                        return Err(error.into());
                    }
                }
            } else {
                pending.record_conflict(if missing {
                    "Your AniList entry changed or was removed during sync. Choose which version to keep.".into()
                } else {
                    e.to_string()
                });
            }
            state
                .db
                .put_pending(account, &pending)
                .map_err(|e| e.to_string())?;
            crate::commands::check_write_origin(state, origin, &token)?;
            if let Some(rejected) = rejected {
                cache_reported_episodes(state, origin, &token, pending.media_id, rejected.total)?;
                entry_changed_from(state, origin, pending.media_id);
                return Ok(Some(e.to_string()));
            }
            entry_changed_from(state, origin, pending.media_id);
            Err(e.to_string().into())
        }
        Err(e) => {
            if let Some(unconfirmed) = e.downcast_ref::<anilist::UnconfirmedEntryWrite>() {
                cache_reported_episodes(state, origin, &token, pending.media_id, unconfirmed.0)?;
            }
            let ((), write_error) =
                crate::commands::settle_write_error(state, origin, &token, &e, |message| {
                    pending.record_retry_error(message);
                    state
                        .db
                        .put_pending_with_retry_position(
                            account,
                            &pending,
                            advance_retry_position && anilist::is_entry_retry_error(&e),
                        )
                        .map_err(|e| e.to_string())
                })?;
            if !anilist::is_auth_rejection(&e) {
                entry_changed_from(state, origin, pending.media_id);
            }
            Err(EntryFlushError::from_remote(&e, write_error))
        }
    }
}

pub async fn get_pending_changes(state: &AppState) -> Result<Vec<PendingChange>, String> {
    Ok(read_pending_changes(state, None).await?.unwrap_or_default())
}

pub(crate) async fn read_pending_changes(
    state: &AppState,
    origin: Option<crate::EventOrigin>,
) -> Result<Option<Vec<PendingChange>>, String> {
    let _guard = state.entry_lock.lock().await;
    if origin.is_some_and(|origin| !state.origin_is_current(origin)) {
        return Ok(None);
    }
    if !state.anilist.lock().has_token() {
        return Ok(Some(vec![]));
    }
    summaries(state).map(Some)
}

fn summaries(state: &AppState) -> Result<Vec<PendingChange>, String> {
    let Some(account) = account_id(&state.db) else {
        return Ok(vec![]);
    };
    let score_format = state
        .db
        .cached_user()
        .map_err(|e| e.to_string())?
        .and_then(|user| user.score_format);
    let conn = state.db.0.lock();
    let mut stmt = conn
        .prepare(
            "SELECT p.payload,m.title_english,m.title_romaji,m.title_native,m.episodes
             FROM pending_change p LEFT JOIN media m ON m.id = p.media_id
             WHERE p.account_id = ? ORDER BY p.media_id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([account], |row| {
            Ok((
                row.get::<_, String>(0)?,
                cached_title(row, 1)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        let (payload, cached_title, episodes) = row.map_err(|e| e.to_string())?;
        let episodes = episodes.filter(|total| *total > 0);
        let mut p = parse_pending(&payload).map_err(|e| e.to_string())?;
        p.patch.apply_completion_total(episodes);
        p.patch.reopen_completed(episodes, p.base.as_ref());
        p.apply_playback_total(episodes);
        let title = cached_title.unwrap_or_else(|| {
            p
                .base
                .as_ref()
                .and_then(|entry| entry.media.as_ref())
                .map(crate::models::Media::display_title)
                .unwrap_or_else(|| format!("#{}", p.media_id))
        });
        let progress = (!p.deleting)
            .then(|| p.progress_for_total(episodes))
            .flatten();
        Ok(PendingChange {
            media_id: p.media_id,
            title,
            progress,
            status: (!p.deleting).then_some(p.patch.status).flatten(),
            score: p.patch.score.filter(|_| !p.deleting).filter(|score| *score == 0.0 || (p.score_format.is_some() && p.score_format == score_format)),
            repeat: (!p.deleting).then_some(p.patch.repeat).flatten(),
            error: p.error.or(p.retry_error).or_else(|| p.deleting.then(|| "Removal is pending. Send saved change retries removal. Keep AniList version cancels it.".into())).or_else(|| (p.patch.score.is_some_and(|score| score != 0.0)
                && (p.score_format.is_none() || p.score_format != score_format))
                .then(|| format!("Saved score {} has an unknown or changed format. Re-enter it in the current AniList format.", p.patch.score.unwrap()))),
            conflict: p.conflict,
            missing_media: p.missing_media,
        })
    })
    .collect()
}

pub async fn sync_pending_changes(state: &AppState) -> Result<Vec<PendingChange>, String> {
    let origin = state.origin();
    let token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    if state.anilist.lock().token() != token || !state.origin_is_current(origin) {
        return Err("Account changed".into());
    }
    if token.is_none() {
        return Ok(vec![]);
    }
    flush_from(state, origin).await?;
    let _guard = state.entry_lock.lock().await;
    if state.anilist.lock().token() != token || !state.origin_is_current(origin) {
        return Err("Account changed".into());
    }
    summaries(state)
}

pub async fn resolve_pending_change(
    media_id: i64,
    keep_local: bool,
    state: &AppState,
) -> Result<(), String> {
    resolve_pending_inner(media_id, keep_local, state).await
}

async fn resolve_pending_inner(
    media_id: i64,
    keep_local: bool,
    state: &AppState,
) -> Result<(), String> {
    let origin = state.origin();
    let token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    if token.is_none() || state.anilist.lock().token() != token || !state.origin_is_current(origin)
    {
        return Err("Account changed".into());
    }
    let _guard = state.entry_lock.lock().await;
    crate::commands::check_write_origin(state, origin, &token).map_err(|_| "Account changed")?;
    let account = account_id(&state.db).ok_or("Not signed in")?;
    let Some(mut pending) = state
        .db
        .pending_change(account, media_id)
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    if pending.needs_cleanup_only() {
        return remove_acknowledged_pending(state, origin, &token, account, media_id);
    }
    if keep_local {
        if pending.deleting {
            drop(_guard);
            return crate::commands::delete_entry_unlocked(media_id, true, state, origin)
                .await
                .map_err(String::from);
        }
        if pending.missing_media {
            return Err(
                "AniList no longer has this anime. Its saved change can be discarded.".into(),
            );
        }
        let al = state
            .anilist
            .lock()
            .for_intent(&state.auth_intent, origin.generation);
        drop(_guard);
        confirm_write_account(state, origin, &al, account).await?;
        check_pending_score_format(state, origin, &al, account, &mut pending).await?;
        let result = al
            .entry_by_media_id_with_episodes(media_id, state.cached_score_format()?.as_deref())
            .await;
        let _guard = state.entry_lock.lock().await;
        crate::commands::check_write_origin(state, origin, &token)?;
        let (remote, remote_episodes) = match result {
            Ok(pair) => pair,
            Err(e) => {
                if anilist::media_not_found(&e) {
                    let intent = crate::commands::write_origin_guard(state, origin, &token)?;
                    mark_media_missing_from(state, origin, intent, media_id, false)
                        .map_err(|e| e.to_string())?;
                }
                return Err(crate::commands::write_err(state, origin, &token, &e));
            }
        };
        cache_reported_episodes(state, origin, &token, media_id, remote_episodes)?;
        let remote_episodes = remote_episodes.flatten();
        if !reconcile_queued_playback(
            state,
            origin,
            &token,
            account,
            &mut pending,
            remote_episodes,
        )? {
            return Err(crate::commands::EPISODE_EXCEEDS_TOTAL.into());
        }
        let remote = remote.map(|entry| remote_entry(media_id, entry));
        if pending.rejected_progress.is_some() {
            let total = remote_episodes.or(state
                .db
                .episode_total(media_id)
                .map_err(|e| e.to_string())?);
            let replay = pending.replay_at_total(remote.as_ref(), remote_episodes, total);
            let progress = replay.progress_for_total(total);
            if let Some(entry) = remote.as_ref().filter(|entry| {
                pending
                    .overshoot_total(progress, remote_episodes, total)
                    .is_none()
                    && already_applied_at_progress(&replay, entry, progress)
            }) {
                drop(_guard);
                check_pending_score_format(state, origin, &al, account, &mut pending).await?;
                let _guard = state.entry_lock.lock().await;
                return acknowledge_remote_entry(state, origin, &token, account, entry.clone());
            }
        }
        drop(_guard);
        return send_pending(
            state,
            origin,
            &al,
            account,
            pending,
            remote,
            remote_episodes,
            false,
        )
        .await?
        .map_or(Ok(()), Err);
    }
    if pending.missing_media {
        return keep_anilist_entry(state, origin, &token, account, media_id, None, None);
    }
    let al = state
        .anilist
        .lock()
        .for_intent(&state.auth_intent, origin.generation);
    drop(_guard);
    confirm_write_account(state, origin, &al, account).await?;
    let result = al
        .entry_by_media_id_with_episodes(media_id, state.cached_score_format()?.as_deref())
        .await;
    let _guard = state.entry_lock.lock().await;
    crate::commands::check_write_origin(state, origin, &token)?;
    let (remote, total) = match result {
        Ok(pair) => pair,
        Err(e) if anilist::media_not_found(&e) => (None, None),
        Err(e) => return Err(crate::commands::write_err(state, origin, &token, &e)),
    };
    let entry = remote.map(|remote| remote_entry(media_id, remote));
    keep_anilist_entry(state, origin, &token, account, media_id, entry, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Media, User};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::{path::Path, sync::Arc};

    #[path = "entry_recovery_tests.rs"]
    mod entry_recovery_tests;

    fn state(mut api: anilist::AniList, db: Db) -> AppState {
        api.record_viewer(7);
        AppState {
            anilist: Mutex::new(api),
            db: Arc::new(db),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: Default::default(),
            remote_lock: Default::default(),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
            events: tokio::sync::broadcast::channel(128).0,
            last_auth_expired: Mutex::new(None),
            last_now_playing: Mutex::new(None),
            last_tracking_prompt: Mutex::new(None),
        }
    }

    fn seed(state: &AppState) {
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    score_format: Some("POINT_100".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .upsert_media(&Media {
                id: 1,
                episodes: Some(12),
                title_english: Some("Test show".into()),
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(22),
                media_id: 1,
                progress: 3,
                status: "CURRENT".into(),
                score: Some(0.0),
                ..Default::default()
            })
            .unwrap();
    }

    fn remote(progress: i64) -> serde_json::Value {
        json!({"id":22,"mediaId":1,"status":"CURRENT","progress":progress,"score":0,"repeat":0})
    }

    fn viewer(format: &str) -> serde_json::Value {
        json!({"data":{"Viewer":{"id":7,"name":"Tester","mediaListOptions":{"scoreFormat":format}}}})
    }

    #[tokio::test]
    async fn snapshot_preserves_fields_hidden_by_an_inferred_completion() {
        for (playback, legacy, status, repeat) in [
            (false, false, "REPEATING", 2),
            (true, false, "PAUSED", 4),
            (true, true, "REPEATING", 4),
            (false, false, "COMPLETED", 3),
        ] {
            let mut snapshot = remote(if status == "COMPLETED" { 12 } else { 11 });
            snapshot["status"] = json!(status);
            snapshot["repeat"] = json!(repeat);
            snapshot["updatedAt"] = json!(101);
            snapshot["score"] = json!(70);
            let mut replies = vec![
                (503, json!({"errors":[{"message":"Unavailable"}]})),
                (
                    500,
                    json!({"errors":[{"status":500,"message":"Entry temporarily unavailable"}]}),
                ),
                (
                    200,
                    json!({"data":{"MediaListCollection":{"user":{"id":7},
                    "hasNextChunk":false,"lists":[{"entries":[snapshot]}]}}}),
                ),
                (200, viewer("POINT_100")),
            ];
            let follow_up = !playback && status == "REPEATING";
            if follow_up {
                let mut confirmed = snapshot.clone();
                confirmed["progress"] = json!(13);
                replies.extend([
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"episodes":24,"mediaListEntry":snapshot}}}),
                    ),
                    (200, json!({"data":{"SaveMediaListEntry":confirmed}})),
                ]);
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let path = std::env::temp_dir().join(format!(
                "kurisu-completion-snapshot-{}-{}.db",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap(),
            ));
            {
                let state = Arc::new(state(api.clone(), Db::open(&path).unwrap()));
                seed(&state);
                let mut base = state.db.get_entry(1).unwrap().unwrap();
                base.status = "REPEATING".into();
                base.progress = 11;
                base.repeat = 2;
                base.updated_at = Some(100);
                state.db.upsert_entry(&base).unwrap();
                let entry = if playback {
                    crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
                        .await
                        .unwrap()
                        .unwrap()
                } else {
                    crate::commands::save_entry_inner(&state, 1, None, Some(12), None, None)
                        .await
                        .unwrap()
                };
                assert_eq!(
                    (entry.status.as_str(), entry.progress, entry.repeat),
                    ("COMPLETED", 12, 3)
                );
                if legacy {
                    let mut pending = state.db.pending_change(7, 1).unwrap().unwrap();
                    pending.patch.playback = None;
                    pending.playback = true;
                    state.db.put_pending(7, &pending).unwrap();
                }
                let entries = crate::commands::sync_my_list(&state).await.unwrap();
                let entry = &entries[0];
                assert_eq!(
                    (entry.status.as_str(), entry.progress, entry.repeat),
                    ("COMPLETED", 12, 3)
                );
                assert_eq!((entry.updated_at, entry.score), (Some(101), Some(70.0)));
                let initial_query = if playback {
                    "mediaListEntry"
                } else {
                    "mutation"
                };
                for query in [
                    initial_query,
                    "mediaListEntry",
                    "MediaListCollection",
                    "Viewer",
                ] {
                    let request = requests.try_recv().unwrap();
                    assert!(
                        request["query"].as_str().unwrap().contains(query),
                        "{request}"
                    );
                }
                state.db.cache_episode_total(1, Some(24)).unwrap();
            }

            let state = state(api, Db::open(&path).unwrap());
            let entry = current_entry(&state, 1).unwrap().unwrap();
            assert_eq!(
                (entry.status.as_str(), entry.progress, entry.repeat),
                (status, 12, repeat)
            );
            let entries = crate::commands::local_entries(&state).await.unwrap();
            let entry = &entries[0];
            assert_eq!(
                (entry.status.as_str(), entry.progress, entry.repeat),
                (status, 12, repeat)
            );
            assert_eq!((entry.updated_at, entry.score), (Some(101), Some(70.0)));
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(pending.patch.progress, Some(12));
            assert_eq!(pending.base.unwrap().updated_at, Some(100));
            if follow_up {
                let edited =
                    crate::commands::save_entry_inner(&state, 1, None, Some(13), None, None)
                        .await
                        .unwrap();
                assert_eq!(
                    (edited.status.as_str(), edited.progress, edited.repeat),
                    ("REPEATING", 13, 2)
                );
                flush_unlocked(&state).await.unwrap();
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
                assert_eq!(
                    requests.try_recv().unwrap()["variables"],
                    json!({"id":22,"progress":13,"scoreFormat":"POINT_100"})
                );
                assert!(state.db.pending_change(7, 1).unwrap().is_none());
            }
            assert!(requests.try_recv().is_err());
            drop(state);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn replacement_snapshot_preserves_remote_rewatches_and_requires_resolution() {
        for keep_local in [false, true] {
            let mut replacement = remote(3);
            replacement["id"] = json!(33);
            replacement["repeat"] = json!(1);
            let lookup = json!({"data":{"Media":{"id":1,"mediaListEntry":replacement}}});
            let snapshot = json!({"data":{"MediaListCollection":{"user":{"id":7},
                "hasNextChunk":false,"lists":[{"entries":[replacement]}]}}});
            let mut replies = vec![
                (503, json!({"errors":[{"message":"Unavailable"}]})),
                (
                    500,
                    json!({"errors":[{"status":500,"message":"Entry temporarily unavailable"}]}),
                ),
                (200, snapshot),
                (200, viewer("POINT_100")),
                (200, lookup.clone()),
            ];
            if keep_local {
                replies.push((503, json!({"errors":[{"message":"Unavailable"}]})));
                replies.push((200, lookup));
                replacement["repeat"] = json!(5);
                replies.push((200, json!({"data":{"SaveMediaListEntry":replacement}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let path = std::env::temp_dir().join(format!(
                "kurisu-replaced-entry-{}-{}.db",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap(),
            ));
            let state = Arc::new(state(api, Db::open(&path).unwrap()));
            seed(&state);
            let saved = crate::commands::save_entry_inner(&state, 1, None, None, None, Some(5))
                .await
                .unwrap();
            assert_eq!((saved.id, saved.repeat), (Some(22), 5));
            let entries = crate::commands::sync_my_list(&state).await.unwrap();
            assert_eq!((entries[0].id, entries[0].repeat), (Some(33), 1));
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict);
            assert_eq!(pending.error.as_deref(), Some(ENTRY_REPLACED));
            assert_eq!(pending.base.unwrap().id, Some(22));
            assert_eq!(pending.patch.repeat, Some(5));
            let reopened = Db::open(&path).unwrap();
            assert!(reopened.pending_change(7, 1).unwrap().unwrap().conflict);
            assert_eq!(reopened.get_entry(1).unwrap().unwrap().repeat, 1);
            drop(reopened);
            let reads = crate::commands::local_entry_updates(vec![1], 7, &state)
                .await
                .unwrap();
            assert_eq!(reads[0].as_ref().unwrap().repeat, 1);
            assert!(crate::commands::local_entry_updates(vec![1], 8, &state)
                .await
                .is_err());
            assert!(
                crate::commands::save_entry_inner(&state, 1, None, None, None, Some(6))
                    .await
                    .unwrap_err()
                    .contains("Choose which version")
            );
            flush_background(&state).await.unwrap();
            assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("MediaListCollection"));
            assert!(requests.try_recv().unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("Viewer"));
            assert!(requests.try_recv().is_err());
            if keep_local {
                assert!(resolve_pending_inner(1, true, &state)
                    .await
                    .unwrap_err()
                    .contains("Unavailable"));
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
                assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 33);
                let retained = state.db.pending_change(7, 1).unwrap().unwrap();
                assert_eq!(retained.error.as_deref(), Some(ENTRY_REPLACED));
                assert!(retained.retry_error.is_some());
                assert_eq!(
                    crate::commands::get_entry(1, &state)
                        .await
                        .unwrap()
                        .unwrap()
                        .repeat,
                    1
                );
            }
            resolve_pending_inner(1, keep_local, &state).await.unwrap();
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            if keep_local {
                assert_eq!(
                    requests.try_recv().unwrap()["variables"],
                    json!({"id":33,"repeat":5,"scoreFormat":"POINT_100"})
                );
            }
            assert!(requests.try_recv().is_err());
            let entry = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (entry.id, entry.repeat),
                (Some(33), if keep_local { 5 } else { 1 })
            );
            assert!(state.db.pending(7).unwrap().is_empty());
            drop(state);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn replacement_discovered_by_entry_lookup_replaces_only_the_cached_entry() {
        for background in [false, true] {
            let mut replacement = remote(3);
            replacement["id"] = json!(33);
            replacement["repeat"] = json!(1);
            let (api, mut requests) = anilist::mock_api(vec![(
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":replacement}}}),
            )])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            stage(
                &state,
                1,
                Patch {
                    repeat: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
            if background {
                overlay(&state).unwrap();
                flush_background(&state).await.unwrap();
            } else {
                state.db.delete_entry(1).unwrap();
                assert!(
                    crate::commands::save_entry_inner(&state, 1, None, None, None, Some(6))
                        .await
                        .unwrap_err()
                        .contains("reopen the editor")
                );
            }

            let entries = crate::commands::local_entries(&state).await.unwrap();
            assert_eq!((entries[0].id, entries[0].repeat), (Some(33), 1));
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict);
            assert_eq!(pending.error.as_deref(), Some(ENTRY_REPLACED));
            assert_eq!(pending.base.unwrap().id, Some(22));
            assert_eq!(pending.patch.repeat, Some(5));
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn correcting_an_overshoot_requeues_only_a_progress_conflict() {
        for repeat in [0, 1] {
            let mut before = remote(3);
            before["mediaId"] = json!(1);
            before["repeat"] = json!(repeat);
            let mut replies = vec![
                (503, json!({"errors":[{"message":"Unavailable"}]})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":before}}}),
                ),
                (
                    200,
                    json!({"data":{"MediaListCollection":{"user":{"id":7},"hasNextChunk":false,"lists":[{"entries":[before]}]}}}),
                ),
                (200, viewer("POINT_100")),
            ];
            if repeat == 0 {
                replies.extend([
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                    ),
                    (200, json!({"data":{"SaveMediaListEntry":remote(11)}})),
                ]);
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
            seed(&state);
            state.db.delete_media(1).unwrap();

            let edit = |progress| {
                crate::commands::save_entry_inner(
                    &state,
                    1,
                    Some("CURRENT".into()),
                    Some(progress),
                    None,
                    None,
                )
            };
            assert_eq!(edit(13).await.unwrap().progress, 13);
            assert_eq!(
                crate::commands::sync_my_list(&state).await.unwrap()[0].progress,
                13
            );
            let rejected = state.db.pending(7).unwrap().remove(0);
            assert!(rejected.conflict);
            assert_eq!(rejected.rejected_total, (repeat == 0).then_some(12));
            for operation in [
                "SaveMediaListEntry",
                "mediaListEntry",
                "Collection",
                "Viewer",
            ] {
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains(operation));
            }

            if repeat == 0 {
                assert!(edit(13).await.unwrap_err().contains("exceeds"));
                assert!(edit(-1).await.unwrap_err().contains("between"));
                state.db.delete_media(1).unwrap();
                assert!(edit(13).await.unwrap_err().contains("exceeds"));
                assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(13));
            }
            assert_eq!(edit(11).await.unwrap().progress, 11);
            let corrected = state.db.pending(7).unwrap().remove(0);
            assert_eq!(json!(corrected.base), json!(rejected.base));
            assert_eq!(corrected.base_score_format, rejected.base_score_format);
            assert_eq!(corrected.conflict, repeat != 0);
            assert_eq!(corrected.error, rejected.error.filter(|_| repeat != 0));
            assert!(requests.try_recv().is_err());

            let pending = sync_pending_changes(&state).await.unwrap();
            if repeat == 0 {
                assert!(pending.is_empty());
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
                assert_eq!(
                    requests.try_recv().unwrap()["variables"],
                    json!({"id":22,"status":"CURRENT","progress":11,"scoreFormat":"POINT_100"})
                );
            } else {
                assert!(pending[0].conflict);
                assert!(pending[0]
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("while this update"));
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn correcting_a_rejected_mutation_checks_the_fresh_entry() {
        for change in [
            None,
            Some("id"),
            Some("status"),
            Some("progress"),
            Some("repeat"),
        ] {
            let mut rejected = remote(12);
            rejected["status"] = json!("COMPLETED");
            let mut fresh = rejected.clone();
            match change {
                Some("id") => fresh["id"] = json!(23),
                Some("status") => fresh["status"] = json!("CURRENT"),
                Some("progress") => fresh["progress"] = json!(3),
                Some("repeat") => fresh["repeat"] = json!(1),
                Some(field) => unreachable!("{field}"),
                None => {}
            }
            let mut replies = vec![
                (200, json!({"data":{"SaveMediaListEntry":rejected}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":fresh}}}),
                ),
            ];
            if change.is_none() {
                replies.push((200, json!({"data":{"SaveMediaListEntry":remote(11)}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let edit = |progress| {
                crate::commands::save_entry_inner(
                    &state,
                    1,
                    Some("CURRENT".into()),
                    Some(progress),
                    None,
                    None,
                )
            };
            assert_eq!(edit(12).await.unwrap().progress, 12);
            let rejected = state.db.pending(7).unwrap().remove(0);
            assert!(rejected.conflict);
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
            assert_eq!(edit(11).await.unwrap().progress, 11);
            let corrected = state.db.pending(7).unwrap().remove(0);
            assert!(!corrected.conflict);
            assert!(corrected.error.is_none());
            assert_eq!(json!(corrected.base), json!(rejected.base));
            assert_eq!(corrected.base_score_format, rejected.base_score_format);
            assert_eq!(
                json!(corrected.rejected_progress),
                json!(rejected.rejected_progress)
            );
            assert!(requests.try_recv().is_err());

            let pending = sync_pending_changes(&state).await.unwrap();
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            if change.is_none() {
                assert!(pending.is_empty());
                assert_eq!(
                    requests.try_recv().unwrap()["variables"],
                    json!({"id":22,"status":"CURRENT","progress":11,"scoreFormat":"POINT_100"})
                );
            } else {
                assert!(pending[0].conflict);
                let error = pending[0].error.as_deref().unwrap();
                if change == Some("id") {
                    assert_eq!(error, ENTRY_REPLACED);
                    assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(23));
                } else {
                    assert!(error.contains("while this update"));
                }
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn correcting_progress_preserves_an_unrelated_status_rejection() {
        let mut reply = remote(9);
        reply["status"] = json!("PAUSED");
        let (api, mut requests) =
            anilist::mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":reply}}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let edit = |progress| {
            crate::commands::save_entry_inner(
                &state,
                1,
                Some("CURRENT".into()),
                Some(progress),
                None,
                None,
            )
        };
        edit(10).await.unwrap();
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 10);
        let rejected = state.db.pending(7).unwrap().remove(0);
        edit(11).await.unwrap();
        let pending = sync_pending_changes(&state).await.unwrap();
        assert!(pending[0].conflict);
        assert_eq!(pending[0].error, rejected.error);
        assert_eq!(pending[0].progress, Some(11));
        assert!(requests.try_recv().is_err());
    }

    async fn queue_completion_before_total_increases(state: &AppState) {
        seed(state);
        let entry =
            crate::commands::save_entry_inner(state, 1, Some("COMPLETED".into()), None, None, None)
                .await
                .unwrap();
        assert_eq!((entry.status.as_str(), entry.progress), ("COMPLETED", 12));
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(12));
    }

    #[tokio::test]
    async fn queued_completion_overlay_uses_the_updated_episode_total() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        queue_completion_before_total_increases(&state).await;
        state.db.cache_episode_total(1, Some(13)).unwrap();

        overlay(&state).unwrap();

        let entry = state.db.get_entry(1).unwrap().unwrap();
        let total = state.db.episode_total(1).unwrap();
        assert_eq!(
            (entry.status.as_str(), entry.progress, total),
            ("COMPLETED", 13, Some(13))
        );
    }

    async fn assert_replayed_completion_uses_new_total(
        before: serde_json::Value,
        mutation_progress: Option<i64>,
    ) {
        let expected = mutation_progress.unwrap_or(13);
        let mut saved = remote(expected);
        saved["status"] = json!("COMPLETED");
        let mut replies = vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":before}}}),
            ),
        ];
        if mutation_progress.is_some() {
            replies.push((200, json!({"data":{"SaveMediaListEntry":saved}})));
            if expected != 13 {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":expected,"mediaListEntry":saved}}}),
                ));
            }
        }
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        queue_completion_before_total_increases(&state).await;
        let mut events = state.events.subscribe();

        flush_unlocked(&state).await.unwrap();

        requests.try_recv().unwrap();
        requests.try_recv().unwrap();
        if mutation_progress.is_some() {
            assert_eq!(
                requests.try_recv().unwrap()["variables"],
                json!({"id":22,"progress":13,"status":"COMPLETED","scoreFormat":"POINT_100"})
            );
            if expected != 13 {
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            }
        }
        assert!(requests.try_recv().is_err());
        let entry = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (entry.status.as_str(), entry.progress),
            ("COMPLETED", expected)
        );
        assert_eq!(entry.media.unwrap().episodes, Some(expected));
        let crate::EventKind::EpisodeUpdated(published) = events.try_recv().unwrap().kind else {
            panic!("expected a replayed entry update");
        };
        assert_eq!(published.progress, expected);
        assert_eq!(published.media.unwrap().episodes, Some(expected));
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn keeping_anilist_completion_publishes_its_current_episode_total() {
        let mut saved = remote(13);
        saved["status"] = json!("COMPLETED");
        let (api, requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":saved}}}),
            ),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        queue_completion_before_total_increases(&state).await;
        let mut events = state.events.subscribe();
        resolve_pending_inner(1, false, &state).await.unwrap();
        let crate::EventKind::EpisodeUpdated(entry) = events.try_recv().unwrap().kind else {
            panic!("expected resolved entry");
        };
        assert_eq!(
            (entry.progress, entry.media.unwrap().episodes),
            (13, Some(13))
        );
        assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn legacy_decrement_is_acknowledged_only_after_reopening() {
        for status in ["COMPLETED", "CURRENT"] {
            let mut before = remote(11);
            before["status"] = json!(status);
            let mut replies = vec![(
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":before}}}),
            )];
            if status == "COMPLETED" {
                replies.push((200, json!({"data":{"SaveMediaListEntry":remote(11)}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "COMPLETED".into();
            base.progress = 12;
            state.db.upsert_entry(&base).unwrap();
            stage(
                &state,
                1,
                Patch {
                    progress: Some(11),
                    ..Default::default()
                },
            )
            .unwrap();
            let listed = crate::commands::local_entries(&state)
                .await
                .unwrap()
                .remove(0);
            assert_eq!((listed.status.as_str(), listed.progress), ("CURRENT", 11));
            assert_eq!(
                summaries(&state).unwrap()[0].status.as_deref(),
                Some("CURRENT")
            );
            flush_unlocked(&state).await.unwrap();
            requests.try_recv().unwrap();
            if status == "COMPLETED" {
                let mutation = requests.try_recv().unwrap();
                assert_eq!(mutation["variables"]["status"], "CURRENT");
                assert_eq!(mutation["variables"]["progress"], 11);
            }
            assert!(requests.try_recv().is_err());
            let entry = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!((entry.status.as_str(), entry.progress), ("CURRENT", 11));
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn queued_completion_replay_sends_the_updated_episode_total() {
        assert_replayed_completion_uses_new_total(remote(3), Some(13)).await;
    }

    #[tokio::test]
    async fn queued_completion_replay_updates_an_already_applied_total() {
        let mut before = remote(12);
        before["status"] = json!("COMPLETED");
        assert_replayed_completion_uses_new_total(before, Some(13)).await;
    }

    #[tokio::test]
    async fn queued_completion_caches_the_total_when_the_mutation_is_already_applied() {
        let mut before = remote(13);
        before["status"] = json!("COMPLETED");
        assert_replayed_completion_uses_new_total(before, None).await;
    }

    #[tokio::test]
    async fn queued_completion_caches_a_confirmed_total_that_changes_during_replay() {
        assert_replayed_completion_uses_new_total(remote(3), Some(14)).await;
    }

    fn score_reentry_baseline(
        state: &AppState,
        format: &str,
        score: f64,
        queued_score: Option<f64>,
    ) -> ListEntry {
        seed(state);
        let mut user = state.db.cached_user().unwrap().unwrap();
        user.score_format = Some(format.into());
        state.db.replace_account("test-session", &user).unwrap();
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.score = Some(score);
        state.db.upsert_entry(&base).unwrap();
        let (_, pending, _) = stage(
            state,
            1,
            Patch {
                progress: Some(4),
                score: queued_score,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(state, &pending).unwrap();
        base
    }

    fn refresh_score_reentry_cache(state: &AppState, format: &str, score: f64) {
        let mut user = state.db.cached_user().unwrap().unwrap();
        user.score_format = Some(format.into());
        state.db.replace_account("test-session", &user).unwrap();
        let mut cached = state.db.get_entry(1).unwrap().unwrap();
        cached.score = Some(score);
        state.db.upsert_entry(&cached).unwrap();
    }

    #[tokio::test]
    async fn invalidated_score_baselines_require_resolution_before_any_write() {
        for existing_progress in [false, true] {
            let mut saved = remote(4);
            saved["score"] = json!(3);
            let (api, mut requests) = anilist::mock_api(vec![
                (200, viewer("POINT_5")),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (200, viewer("POINT_5")),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut cached = state.db.get_entry(1).unwrap().unwrap();
            cached.score = Some(80.0);
            state.db.upsert_entry(&cached).unwrap();
            let mut user = state.db.cached_user().unwrap().unwrap();
            user.score_format = Some("POINT_5".into());
            state.db.replace_account("test-session", &user).unwrap();
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, None);
            if existing_progress {
                let (_, pending, _) = stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                local_entry(&state, &pending).unwrap();
            }
            let local =
                crate::commands::save_entry_inner(&state, 1, None, Some(4), Some(3.0), None)
                    .await
                    .unwrap();
            assert_eq!((local.progress, local.score), (4, Some(3.0)));
            let pending = state.db.pending(7).unwrap().remove(0);
            assert!(pending.conflict);
            assert_eq!(pending.error.as_deref(), Some(SCORE_BASELINE_ERROR));
            assert_eq!(pending.base.as_ref().unwrap().score, None);
            flush_unlocked(&state).await.unwrap();
            flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err());

            resolve_pending_inner(1, true, &state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
            requests.recv().await.unwrap();
            requests.recv().await.unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"progress":4,"score":3.0,"scoreFormat":"POINT_5"})
            );
            requests.recv().await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn legacy_null_baselines_preserve_intent_and_require_score_resolution() {
        for score_patch in [false, true] {
            let mut replies = Vec::new();
            if score_patch {
                replies.push((200, viewer("POINT_5")));
            }
            replies.push((
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
            ));
            if score_patch {
                replies.push((200, viewer("POINT_5")));
            } else {
                replies.push((200, json!({"data":{"SaveMediaListEntry":remote(4)}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut user = state.db.cached_user().unwrap().unwrap();
            user.score_format = Some("POINT_5".into());
            state.db.replace_account("test-session", &user).unwrap();
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.score = None;
            let payload = json!({
                "media_id":1,"base":base,
                "patch":{"progress":4,"score":score_patch.then_some(3.0)},
                "error":null,"conflict":false,"score_format":"POINT_5"
            });
            let pending = parse_pending(&payload.to_string()).unwrap();
            state.db.put_pending(7, &pending).unwrap();
            overlay(&state).unwrap();
            flush_unlocked(&state).await.unwrap();
            let remaining = state.db.pending(7).unwrap();
            if score_patch {
                assert_eq!(remaining.len(), 1);
                assert!(remaining[0].conflict);
                assert_eq!(remaining[0].patch.score, Some(3.0));
                assert_eq!(remaining[0].patch.progress, Some(4));
            } else {
                assert!(remaining.is_empty());
            }
            let mut mutations = 0;
            while let Ok(request) = requests.try_recv() {
                if request["query"].as_str().unwrap().contains("mutation") {
                    assert!(!score_patch);
                    assert!(request["variables"].get("score").is_none());
                    mutations += 1;
                }
            }
            assert_eq!(mutations, usize::from(!score_patch));
        }
    }

    #[tokio::test]
    async fn unknown_remote_score_cannot_acknowledge_a_legacy_clear() {
        let mut rated = remote(3);
        rated["score"] = json!(80);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":rated}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut user = state.db.cached_user().unwrap().unwrap();
        user.score_format = None;
        state.db.replace_account("test-session", &user).unwrap();
        let mut base = state.db.get_entry(1).unwrap().unwrap();
        base.score = Some(0.0);
        let pending = parse_pending(
            &json!({"media_id":1,"base":base,"patch":{"score":0},"error":null,"conflict":false})
                .to_string(),
        )
        .unwrap();
        state.db.put_pending(7, &pending).unwrap();
        flush_unlocked(&state).await.unwrap();
        let remaining = state.db.pending(7).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].conflict);
        assert_eq!(remaining[0].patch.score, Some(0.0));
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn fresh_unrated_snapshot_allows_an_offline_rating_after_format_change() {
        let mut unrated = remote(3);
        unrated["mediaId"] = json!(1);
        unrated["score"] = json!(null);
        let mut saved = remote(3);
        saved["score"] = json!(3);
        let snapshot = json!({"data":{"MediaListCollection":{"user":{"id":7},
            "hasNextChunk":false,"lists":[{"entries":[unrated]}]
        }}});
        let (api, mut requests) = anilist::mock_api(vec![
            (200, snapshot.clone()),
            (200, viewer("POINT_5")),
            (200, snapshot),
            (200, viewer("POINT_5")),
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, viewer("POINT_5")),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":unrated}}}),
            ),
            (200, viewer("POINT_5")),
            (200, json!({"data":{"SaveMediaListEntry":saved}})),
            (200, viewer("POINT_5")),
        ])
        .await;
        let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&state);
        let mut cached = state.db.get_entry(1).unwrap().unwrap();
        cached.score = Some(80.0);
        state.db.upsert_entry(&cached).unwrap();
        assert_eq!(
            crate::commands::sync_my_list(&state).await.unwrap()[0].score,
            Some(0.0)
        );
        let local = crate::commands::save_entry_inner(&state, 1, None, None, Some(3.0), None)
            .await
            .unwrap();
        assert_eq!(local.score, Some(3.0));
        let pending = state.db.pending(7).unwrap().remove(0);
        assert!(!pending.conflict);
        assert_eq!(pending.base.unwrap().score, Some(0.0));
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(3.0));
        let mut mutations = Vec::new();
        while let Ok(request) = requests.try_recv() {
            if request["query"].as_str().unwrap().contains("mutation") {
                mutations.push(request["variables"].clone());
            }
        }
        assert_eq!(
            mutations,
            vec![json!({"id":22,"score":3.0,"scoreFormat":"POINT_5"})]
        );
    }

    #[tokio::test]
    async fn score_reentry_compares_the_offline_baseline_without_false_conflicts() {
        for (old_format, old_score, new_format, remote_score, desired) in [
            ("POINT_100", 81.0, "POINT_10_DECIMAL", 8.1, 9.0),
            ("POINT_10_DECIMAL", 8.1, "POINT_100", 81.0, 90.0),
        ] {
            for queued_score in [None, Some(old_score)] {
                let mut snapshot = remote(3);
                snapshot["score"] = json!(remote_score);
                let mut saved = remote(4);
                saved["score"] = json!(desired);
                let (api, mut requests) = anilist::mock_api(vec![
                    (200, viewer(new_format)),
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":snapshot}}}),
                    ),
                    (200, viewer(new_format)),
                    (200, json!({"data":{"SaveMediaListEntry":saved}})),
                    (200, viewer(new_format)),
                ])
                .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                let base = score_reentry_baseline(&state, old_format, old_score, queued_score);
                refresh_score_reentry_cache(&state, new_format, remote_score);
                let mut pending = state.db.pending(7).unwrap().remove(0);
                pending.conflict = true;
                pending.error = Some(SCORE_FORMAT_ERROR.into());
                // Re-entry must also work after restoring a saved offline change.
                let restored =
                    serde_json::from_str(&serde_json::to_string(&pending).unwrap()).unwrap();
                state.db.put_pending(7, &restored).unwrap();

                let entry =
                    crate::commands::save_entry_inner(&state, 1, None, None, Some(desired), None)
                        .await
                        .unwrap();
                assert_eq!(entry.score, Some(desired));
                let pending = state.db.pending(7).unwrap().remove(0);
                assert!(!pending.conflict);
                assert!(pending.error.is_none());
                assert_eq!(pending.base_score_format.as_deref(), Some(old_format));
                assert_eq!(pending.score_format.as_deref(), Some(new_format));
                assert_eq!(serde_json::to_value(&pending.base).unwrap(), json!(base));

                flush_unlocked(&state).await.unwrap();
                assert!(state.db.pending(7).unwrap().is_empty());
                let cached = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!((cached.progress, cached.score), (4, Some(desired)));
                let mut mutations = Vec::new();
                while let Ok(request) = requests.try_recv() {
                    if request["query"].as_str().unwrap().contains("mutation") {
                        mutations.push(request["variables"].clone());
                    }
                }
                assert_eq!(
                    mutations,
                    vec![json!({"id":22,"progress":4,"score":desired,"scoreFormat":new_format})]
                );
            }
        }
    }

    #[tokio::test]
    async fn score_reentry_does_not_overwrite_a_remote_score_matching_the_old_number() {
        for (old_format, new_format, desired) in [
            ("POINT_100", "POINT_10_DECIMAL", 9.0),
            ("POINT_10_DECIMAL", "POINT_100", 90.0),
        ] {
            for queued_score in [None, Some(7.0)] {
                let mut snapshot = remote(3);
                snapshot["score"] = json!(8.0);
                let (api, mut requests) = anilist::mock_api(vec![
                    (200, viewer(new_format)),
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":snapshot}}}),
                    ),
                    (200, viewer(new_format)),
                ])
                .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                let base = score_reentry_baseline(&state, old_format, 8.0, queued_score);
                refresh_score_reentry_cache(&state, new_format, 8.0);
                crate::commands::save_entry_inner(&state, 1, None, None, Some(desired), None)
                    .await
                    .unwrap();
                flush_unlocked(&state).await.unwrap();
                let pending = state.db.pending(7).unwrap().remove(0);
                assert!(pending.conflict);
                assert!(pending.error.unwrap().contains("Choose which version"));
                assert_eq!(pending.patch.score, Some(desired));
                assert_eq!(pending.patch.progress, Some(4));
                assert_eq!(serde_json::to_value(&pending.base).unwrap(), json!(base));
                assert_eq!(pending.base_score_format.as_deref(), Some(old_format));
                let mut count = 0;
                while let Ok(request) = requests.try_recv() {
                    assert!(!request["query"].as_str().unwrap().contains("mutation"));
                    count += 1;
                }
                assert_eq!(count, 3);
            }
        }
    }

    #[tokio::test]
    async fn score_reentry_requires_resolution_when_the_baseline_scale_is_uncertain() {
        for (old_format, old_score, new_format, desired, legacy) in [
            ("POINT_5", 4.0, "POINT_100", 80.0, false),
            ("POINT_10", 8.0, "POINT_10_DECIMAL", 9.0, false),
            ("POINT_3", 2.0, "POINT_100", 80.0, false),
            ("POINT_100", 80.0, "POINT_100", 90.0, true),
        ] {
            let mut snapshot = remote(3);
            snapshot["score"] = json!(old_score);
            let mut saved = remote(4);
            saved["score"] = json!(desired);
            let (api, mut requests) = anilist::mock_api(vec![
                (200, viewer(new_format)),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":snapshot}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (200, viewer(new_format)),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            let base = score_reentry_baseline(&state, old_format, old_score, Some(old_score));
            if legacy {
                let mut payload = serde_json::to_value(&state.db.pending(7).unwrap()[0]).unwrap();
                payload.as_object_mut().unwrap().remove("base_score_format");
                state
                    .db
                    .put_pending(7, &serde_json::from_value(payload).unwrap())
                    .unwrap();
            }
            refresh_score_reentry_cache(&state, new_format, old_score);
            for _ in 0..2 {
                crate::commands::save_entry_inner(&state, 1, None, None, Some(desired), None)
                    .await
                    .unwrap();
                let pending = state.db.pending(7).unwrap().remove(0);
                assert!(pending.conflict);
                assert_eq!(pending.error.as_deref(), Some(SCORE_BASELINE_ERROR));
                assert_eq!(pending.score_format.as_deref(), Some(new_format));
                assert_eq!(serde_json::to_value(&pending.base).unwrap(), json!(base));
                assert!(conflict(&pending, Some(&base)));
                flush_unlocked(&state).await.unwrap();
                flush_background(&state).await.unwrap();
                assert!(requests.try_recv().is_err());
            }
            resolve_pending_inner(1, true, &state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(desired));
            requests.recv().await.unwrap();
            requests.recv().await.unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"]["score"],
                desired
            );
            requests.recv().await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn score_format_change_retains_intent_and_requires_reentry() {
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, viewer("POINT_100")),
            (200, viewer("POINT_100")),
            (200, viewer("POINT_100")),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}})),
            (200, viewer("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":{"mediaId":1,"id":22,"status":"CURRENT","progress":3,"repeat":0,"score":80}}})),
            (200, viewer("POINT_100")),
        ]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    score_format: Some("POINT_5".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let saved = crate::commands::save_entry_inner(&state, 1, None, None, Some(4.0), None)
            .await
            .unwrap();
        assert_eq!(saved.score, Some(4.0));
        let persisted = serde_json::to_string(&state.db.pending(7).unwrap()[0]).unwrap();
        let restored: Pending = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored.score_format.as_deref(), Some("POINT_5"));
        state.db.put_pending(7, &restored).unwrap();
        assert!(flush_unlocked(&state).await.is_err());
        let pending = state.db.pending(7).unwrap().remove(0);
        assert!(pending.conflict);
        assert_eq!(pending.patch.score, Some(4.0));
        overlay(&state).unwrap();
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, None);
        assert_eq!(summaries(&state).unwrap()[0].score, None);
        requests.recv().await.unwrap();
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
        assert!(resolve_pending_inner(1, true, &state).await.is_err());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.score, Some(4.0));
        // Re-entry explicitly replaces the ambiguous score in the current format.
        stage(
            &state,
            1,
            Patch {
                score: Some(80.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!state.db.pending(7).unwrap()[0].conflict);
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(80.0));
    }

    #[tokio::test]
    async fn legacy_scores_cannot_be_acknowledged_or_forced_without_their_format() {
        let mut same_number = remote(3);
        same_number["score"] = json!(4);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":same_number}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                score: Some(4.0),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        let mut payload = serde_json::to_value(pending).unwrap();
        payload.as_object_mut().unwrap().remove("score_format");
        let legacy: Pending = serde_json::from_value(payload).unwrap();
        state.db.put_pending(7, &legacy).unwrap();
        assert!(flush_unlocked(&state).await.is_err());
        assert!(resolve_pending_inner(1, true, &state).await.is_err());
        assert_eq!(state.db.pending(7).unwrap()[0].patch.score, Some(4.0));
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn unavailable_profile_keeps_scores_retryable() {
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (200, viewer("POINT_100")),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":22,"mediaId":1,"status":"CURRENT","progress":3,"repeat":0,"score":80}}}})),
            (200, viewer("POINT_100")),
        ]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                score: Some(80.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(flush_unlocked(&state).await.is_err());
        assert!(!state.db.pending(7).unwrap()[0].conflict);
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        for _ in 0..4 {
            requests.recv().await.unwrap();
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn legacy_zero_scores_keep_their_existing_clear_semantics() {
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, mut pending, _) = stage(
            &state,
            1,
            Patch {
                score: Some(0.0),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        pending.score_format = None;
        state.db.put_pending(7, &pending).unwrap();
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn unchanged_score_format_uses_the_established_score_api() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, viewer("POINT_100")),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}})),
            (200, viewer("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":{"mediaId":1,"id":22,"status":"CURRENT","progress":3,"repeat":0,"score":80}}})),
            (200, viewer("POINT_100")),
        ]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                score: Some(80.0),
                ..Default::default()
            },
        )
        .unwrap();
        flush_unlocked(&state).await.unwrap();
        for _ in 0..3 {
            requests.recv().await.unwrap();
        }
        let write = requests.recv().await.unwrap();
        assert_eq!(write["variables"]["score"], 80.0);
        assert!(requests.recv().await.unwrap()["query"]
            .as_str()
            .unwrap()
            .contains("Viewer"));
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending(7).unwrap().is_empty());
    }

    #[tokio::test]
    async fn score_format_race_must_not_acknowledge_reinterpreted_score() {
        for keep_local in [false, true] {
            let mut changed = remote(3);
            changed["score"] = json!(4);
            let mut replies = vec![
                (200, viewer("POINT_100")),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ),
            ];
            if !keep_local {
                replies.push((200, viewer("POINT_100")));
            }
            replies.extend([
                (200, json!({"data":{"SaveMediaListEntry":changed}})),
                (200, viewer("POINT_10_DECIMAL")),
            ]);
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            stage(
                &state,
                1,
                Patch {
                    score: Some(4.0),
                    ..Default::default()
                },
            )
            .unwrap();
            let result = if keep_local {
                resolve_pending_inner(1, true, &state).await
            } else {
                flush_unlocked(&state).await
            };
            assert_eq!(result.unwrap_err(), SCORE_FORMAT_ERROR);
            let mut mutation_seen = false;
            while let Ok(request) = requests.try_recv() {
                if request["query"].as_str().unwrap().contains("mutation") {
                    assert_eq!(request["variables"]["score"], 4.0);
                    mutation_seen = true;
                }
            }
            assert!(mutation_seen);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(0.0));
            let pending = state.db.pending(7).unwrap().remove(0);
            assert_eq!(pending.patch.score, Some(4.0));
            assert_eq!(pending.score_format.as_deref(), Some("POINT_100"));
            assert!(pending.conflict);
            assert_eq!(pending.error.as_deref(), Some(SCORE_FORMAT_ERROR));
            assert_eq!(
                state
                    .db
                    .cached_user()
                    .unwrap()
                    .unwrap()
                    .score_format
                    .as_deref(),
                Some("POINT_10_DECIMAL")
            );
            assert_eq!(summaries(&state).unwrap()[0].score, None);
        }
    }

    #[tokio::test]
    async fn lookup_format_race_cannot_acknowledge_an_equal_number() {
        let mut changed = remote(3);
        changed["score"] = json!(4);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, viewer("POINT_100")),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":changed}}}),
            ),
            (200, viewer("POINT_10_DECIMAL")),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                score: Some(4.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            flush_unlocked(&state).await.unwrap_err(),
            SCORE_FORMAT_ERROR
        );
        assert_eq!(state.db.pending(7).unwrap()[0].patch.score, Some(4.0));
        assert!(state.db.pending(7).unwrap()[0].conflict);
        for _ in 0..3 {
            requests.recv().await.unwrap();
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn keep_local_checks_score_scale_before_acknowledging_a_progress_conflict() {
        let mut rejected = remote(3);
        rejected["score"] = json!(4);
        let mut applied = rejected.clone();
        applied["progress"] = json!(5);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, viewer("POINT_100")),
            (200, json!({"data":{"SaveMediaListEntry":rejected}})),
            (200, viewer("POINT_100")),
            (
                200,
                json!({"data":{"Media":{"id":1,"episodes":12,"mediaListEntry":applied}}}),
            ),
            (200, viewer("POINT_10_DECIMAL")),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        crate::commands::save_entry_inner(&state, 1, None, Some(5), Some(4.0), None)
            .await
            .unwrap();
        assert!(state.db.pending(7).unwrap()[0].rejected_progress.is_some());

        assert_eq!(
            resolve_pending_change(1, true, &state).await.unwrap_err(),
            SCORE_FORMAT_ERROR
        );
        let pending = state.db.pending(7).unwrap().remove(0);
        assert!(pending.conflict);
        assert_eq!(pending.patch.score, Some(4.0));
        assert_eq!(pending.patch.progress, Some(5));
        assert_eq!(pending.error.as_deref(), Some(SCORE_FORMAT_ERROR));
        crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
            .await
            .unwrap();
        let corrected = state.db.pending(7).unwrap().remove(0);
        assert!(corrected.conflict);
        assert_eq!(corrected.error, pending.error);
        assert_eq!(corrected.score_format, pending.score_format);
        assert_eq!(corrected.base_score_format, pending.base_score_format);
        let mut mutations = 0;
        while let Ok(request) = requests.try_recv() {
            mutations += usize::from(request["query"].as_str().unwrap().starts_with("mutation"));
        }
        assert_eq!(mutations, 1);
    }

    #[tokio::test]
    async fn failed_score_postflight_keeps_intent_until_a_verified_retry() {
        for expired in [false, true] {
            let mut saved = remote(13);
            saved["status"] = json!("COMPLETED");
            saved["score"] = json!(80);
            let failure = if expired {
                (
                    401,
                    json!({"errors":[{"message":"Invalid token","status":401}]}),
                )
            } else {
                (503, json!({"errors":[{"message":"Unavailable"}]}))
            };
            let mut replies = vec![
                (200, viewer("POINT_100")),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ),
                (200, viewer("POINT_100")),
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":saved}}}),
                ),
                failure,
            ];
            if !expired {
                replies.extend([
                    (200, viewer("POINT_100")),
                    (
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":saved}}}),
                    ),
                    (200, viewer("POINT_100")),
                ]);
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            stage(
                &state,
                1,
                Patch {
                    status: Some("COMPLETED".into()),
                    score: Some(80.0),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(flush_unlocked(&state).await.is_err());
            let pending = state.db.pending(7).unwrap().remove(0);
            assert_eq!(pending.patch.score, Some(80.0));
            assert!(!pending.conflict);
            assert_eq!(state.db.episode_total(1).unwrap(), Some(13));
            if expired {
                assert!(state.anilist.lock().token().is_none());
                assert!(state.db.entries_with_media().unwrap().is_empty());
            } else {
                flush_unlocked(&state).await.unwrap();
                assert!(state.db.pending(7).unwrap().is_empty());
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(80.0));
            }
            let mut mutations = 0;
            while let Ok(request) = requests.try_recv() {
                mutations += usize::from(request["query"].as_str().unwrap().contains("mutation"));
            }
            assert_eq!(mutations, 1);
        }
    }

    struct SqlTrace<'a> {
        db: &'a Db,
        statements: Box<std::cell::RefCell<Vec<String>>>,
    }

    impl<'a> SqlTrace<'a> {
        fn new(db: &'a Db) -> Self {
            unsafe extern "C" fn record(
                _: u32,
                context: *mut std::ffi::c_void,
                statement: *mut std::ffi::c_void,
                _: *mut std::ffi::c_void,
            ) -> i32 {
                unsafe {
                    let statements = &*context.cast::<std::cell::RefCell<Vec<String>>>();
                    let sql = rusqlite::ffi::sqlite3_sql(statement.cast());
                    statements
                        .borrow_mut()
                        .push(std::ffi::CStr::from_ptr(sql).to_string_lossy().into_owned());
                }
                0
            }
            let mut trace = Self {
                db,
                statements: Box::default(),
            };
            let result = unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    db.0.lock().handle(),
                    rusqlite::ffi::SQLITE_TRACE_STMT,
                    Some(record),
                    std::ptr::from_mut(trace.statements.as_mut()).cast(),
                )
            };
            assert_eq!(result, rusqlite::ffi::SQLITE_OK);
            trace
        }
    }

    impl Drop for SqlTrace<'_> {
        fn drop(&mut self) {
            unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    self.db.0.lock().handle(),
                    0,
                    None,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    #[test]
    fn pending_summaries_use_one_joined_query_for_every_queue_size() {
        for count in [0, 1, 1000, 5000] {
            let state = state(
                anilist::AniList::new(),
                Db::open(Path::new(":memory:")).unwrap(),
            );
            seed(&state);
            state
                .db
                .durable_write(|conn| {
                    for media_id in (1..=count).rev() {
                        let pending = Pending {
                            media_id,
                            deleting: false,
                            deletion_id: None,
                            base: None,
                            patch: Patch {
                                progress: Some(media_id),
                                ..Default::default()
                            },
                            error: None,
                            conflict: false,
                            missing_media: false,
                            score_format: None,
                            playback: false,
                            base_score_format: None,
                            rejected_progress: None,
                            retry_error: None,
                            rejected_total: None,
                        };
                        conn.execute(
                            "INSERT INTO pending_change VALUES (7,?,?)",
                            rusqlite::params![media_id, serde_json::to_string(&pending)?],
                        )?;
                    }
                    conn.execute("INSERT INTO pending_change VALUES (9,99999,'invalid')", [])?;
                    Ok(())
                })
                .unwrap();
            let trace = SqlTrace::new(&state.db);
            let parses = PENDING_DESERIALIZATIONS.get();
            let changes = summaries(&state).unwrap();
            let statements = trace.statements.borrow();
            assert_eq!(statements.len(), 4, "{statements:?}");
            assert!(statements[0].contains("settings"));
            assert!(statements[1].contains("settings"));
            assert!(statements[2].contains("settings"));
            assert!(statements[3].contains("LEFT JOIN media"));
            assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, count as usize);
            assert_eq!(changes.len(), count as usize);
            for (index, change) in changes.iter().enumerate() {
                let id = index as i64 + 1;
                assert_eq!(change.media_id, id);
                assert_eq!(change.progress, Some(id));
                assert_eq!(
                    change.title,
                    if id == 1 {
                        "Test show".into()
                    } else {
                        format!("#{id}")
                    }
                );
            }
            println!(
                "pending summaries {count} rows: 1 joined query + 3 profile and account lookups"
            );
        }
    }

    #[test]
    fn pending_summaries_preserve_titles_fields_accounts_and_selected_errors() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        let titles = [
            (Some("English"), Some("Romaji"), Some("Native"), "English"),
            (Some(""), Some("Romaji"), Some("Native"), "Romaji"),
            (None, Some("Romaji"), Some("Native"), "Romaji"),
            (None, None, Some("Native"), "Native"),
            (None, None, None, "Saved title"),
            (Some("\u{2003}"), Some("Romaji"), None, "Romaji"),
            (None, Some(""), Some("Native"), "Native"),
            (None, None, Some(""), "Saved title"),
        ];
        for (index, (english, romaji, native, _)) in titles.iter().enumerate() {
            let id = index as i64 + 1;
            state
                .db
                .upsert_media(&Media {
                    id,
                    title_english: english.map(String::from),
                    title_romaji: romaji.map(String::from),
                    title_native: native.map(String::from),
                    ..Default::default()
                })
                .unwrap();
        }
        for media_id in (1..=12).rev() {
            state
                .db
                .put_pending(
                    7,
                    &Pending {
                        media_id,
                        deleting: false,
                        deletion_id: None,
                        base: (media_id != 11).then(|| ListEntry {
                            media: Some(Media {
                                id: 70,
                                title_english: match media_id {
                                    10 => None,
                                    12 => Some(String::new()),
                                    _ => Some("Saved title".into()),
                                },
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        patch: Patch {
                            progress: Some(4),
                            status: Some("PAUSED".into()),
                            score: Some(0.0),
                            repeat: Some(2),
                            ..Default::default()
                        },
                        error: Some("Saved error".into()),
                        conflict: true,
                        missing_media: true,
                        score_format: None,
                        playback: false,
                        base_score_format: None,
                        rejected_progress: None,
                        retry_error: None,
                        rejected_total: None,
                    },
                )
                .unwrap();
        }
        state
            .db
            .0
            .lock()
            .execute_batch(
                "UPDATE media SET description = X'FF',genres = X'FF',studios = X'FF';
             INSERT INTO media(id,title_english,cached_at) VALUES(99,X'FF',0);
             INSERT INTO pending_change VALUES(9,99,'invalid');",
            )
            .unwrap();
        let changes = summaries(&state).unwrap();
        let expected = titles
            .iter()
            .map(|row| row.3)
            .chain(["Saved title", "#70", "#11", "#70"])
            .collect::<Vec<_>>();
        assert_eq!(
            changes
                .iter()
                .map(|change| change.title.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        for (index, change) in changes.iter().enumerate() {
            assert_eq!(change.media_id, index as i64 + 1);
            assert_eq!(change.progress, Some(4));
            assert_eq!(change.status.as_deref(), Some("PAUSED"));
            assert_eq!(change.score, Some(0.0));
            assert_eq!(change.repeat, Some(2));
            assert_eq!(change.error.as_deref(), Some("Saved error"));
            assert!(change.conflict);
            assert!(change.missing_media);
        }
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET title_english = X'FF' WHERE id=1", [])
            .unwrap();
        assert!(summaries(&state).unwrap_err().contains("Blob"));
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET title_english = 'English' WHERE id=1", [])
            .unwrap();
        for payload in ["'invalid'", "X'FF'", "'{}'"] {
            state.db.0.lock().execute(&format!("UPDATE pending_change SET payload = {payload} WHERE account_id=7 AND media_id=1"), []).unwrap();
            assert!(summaries(&state).is_err());
        }
        for account in ["0", "invalid", "8"] {
            state.db.set_setting("anilist_user_id", account).unwrap();
            assert!(summaries(&state).unwrap().is_empty());
        }
        state.db.delete_setting("anilist_user_id").unwrap();
        assert!(summaries(&state).unwrap().is_empty());
    }

    #[test]
    fn staging_deserializes_only_its_target_and_preserves_payload_errors() {
        for count in [1, 1000, 5000] {
            let state = state(
                anilist::AniList::new(),
                Db::open(Path::new(":memory:")).unwrap(),
            );
            seed(&state);
            state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TRIGGER require_full BEFORE INSERT ON pending_change
                 WHEN (SELECT synchronous FROM pragma_synchronous) <> 2
                 BEGIN SELECT RAISE(ABORT, 'pending intent requires FULL'); END;",
                )
                .unwrap();
            let initial = crate::db::durable_commit_count();
            let (_, mut pending, already_pending) = stage(
                &state,
                1,
                Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            assert!(!already_pending);
            assert_eq!(crate::db::durable_commit_count(), initial + 1);
            state
                .db
                .durable_write(|conn| {
                    for media_id in 2..=count {
                        pending.media_id = media_id;
                        conn.execute(
                            "INSERT INTO pending_change VALUES (7,?,?)",
                            rusqlite::params![media_id, serde_json::to_string(&pending)?],
                        )?;
                    }
                    conn.execute("INSERT INTO pending_change VALUES (9,1,'corrupt')", [])?;
                    Ok(())
                })
                .unwrap();
            let before = PENDING_DESERIALIZATIONS.get();
            let (_, updated, already_pending) = stage(
                &state,
                1,
                Patch {
                    score: Some(8.5),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            assert!(already_pending);
            assert_eq!(updated.patch.progress, Some(4));
            assert_eq!(updated.patch.score, Some(8.5));
            assert_eq!(updated.base.unwrap().progress, 3);
            assert_eq!(
                PENDING_DESERIALIZATIONS.get() - before,
                1,
                "backlog {count}"
            );
            println!("stage over {count} pending rows: 1 payload deserialized");
            state
                .db
                .durable_write(|conn| {
                    conn.execute(
                        "INSERT OR REPLACE INTO pending_change VALUES (7,2,'corrupt')",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
            assert!(stage(&state, 1, Patch::default()).is_ok());
            let before = PENDING_DESERIALIZATIONS.get();
            let commits = crate::db::durable_commit_count();
            assert!(stage(&state, 2, Patch::default()).is_err());
            assert_eq!(PENDING_DESERIALIZATIONS.get() - before, 1);
            assert_eq!(crate::db::durable_commit_count(), commits);
        }
    }

    #[test]
    fn staging_reads_baseline_only_for_new_pending_changes() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        for media_id in [1, 2] {
            let base = serde_json::to_value(state.db.get_entry(media_id).unwrap()).unwrap();
            let trace = SqlTrace::new(&state.db);
            let baseline_reads = || {
                trace
                    .statements
                    .borrow()
                    .iter()
                    .filter(|sql| sql.contains("FROM list_entry"))
                    .count()
            };
            let (account, mut pending, already_pending) = stage(
                &state,
                media_id,
                Patch {
                    status: Some("PAUSED".into()),
                    progress: Some(4),
                    repeat: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(account, 7);
            assert!(!already_pending);
            assert_eq!(serde_json::to_value(&pending.base).unwrap(), base);
            assert_eq!(baseline_reads(), 1);
            pending.error = Some("Saved failure".into());
            pending.conflict = true;
            state.db.put_pending(account, &pending).unwrap();
            state
                .db
                .0
                .lock()
                .execute(
                    "UPDATE list_entry SET progress = 9 WHERE media_id = ?",
                    [media_id],
                )
                .unwrap();
            trace.statements.borrow_mut().clear();
            let commits = crate::db::durable_commit_count();
            let (account, updated, already_pending) = stage(
                &state,
                media_id,
                Patch {
                    progress: Some(5),
                    score: Some(8.5),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(account, 7);
            assert!(already_pending);
            assert_eq!(baseline_reads(), 0);
            assert_eq!(serde_json::to_value(&updated.base).unwrap(), base);
            assert_eq!(updated.patch.status.as_deref(), Some("PAUSED"));
            assert_eq!(updated.patch.progress, Some(5));
            assert_eq!(updated.patch.score, Some(8.5));
            assert_eq!(updated.patch.repeat, Some(1));
            assert_eq!(updated.error, pending.error);
            assert!(updated.conflict);
            assert!(!updated.missing_media);
            assert_eq!(crate::db::durable_commit_count(), commits + 1);
            assert_eq!(
                serde_json::to_value(state.db.pending_change(7, media_id).unwrap().unwrap())
                    .unwrap(),
                serde_json::to_value(&updated).unwrap()
            );
        }
    }

    #[test]
    fn staging_preserves_required_baseline_errors_without_blocking_existing_pending() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        state
            .db
            .0
            .lock()
            .execute(
                "UPDATE list_entry SET progress = 'broken' WHERE media_id = 1",
                [],
            )
            .unwrap();
        let baseline_error = state.db.get_entry(1).unwrap_err().to_string();
        state.db.set_setting("anilist_user_id", "9").unwrap();
        let trace = SqlTrace::new(&state.db);
        let commits = crate::db::durable_commit_count();
        let error = stage(&state, 1, Patch::default()).unwrap_err();
        assert_eq!(error.to_string(), baseline_error);
        assert_eq!(crate::db::durable_commit_count(), commits);
        assert!(state.db.pending_change(9, 1).unwrap().is_none());
        assert_eq!(
            trace
                .statements
                .borrow()
                .iter()
                .filter(|sql| sql.contains("FROM list_entry"))
                .count(),
            1
        );
        assert_eq!(
            serde_json::to_value(state.db.pending_change(7, 1).unwrap().unwrap()).unwrap(),
            serde_json::to_value(&pending).unwrap()
        );
        state.db.set_setting("anilist_user_id", "7").unwrap();
        trace.statements.borrow_mut().clear();
        let commits = crate::db::durable_commit_count();
        let (account, updated, already_pending) = stage(
            &state,
            1,
            Patch {
                score: Some(8.5),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(account, 7);
        assert!(already_pending);
        assert_eq!(
            serde_json::to_value(&updated.base).unwrap(),
            serde_json::to_value(&pending.base).unwrap()
        );
        assert_eq!(updated.patch.progress, Some(4));
        assert_eq!(updated.patch.score, Some(8.5));
        assert_eq!(crate::db::durable_commit_count(), commits + 1);
        assert!(!trace
            .statements
            .borrow()
            .iter()
            .any(|sql| sql.contains("FROM list_entry")));
    }

    #[test]
    fn staging_rejects_missing_media_without_reading_baseline() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        let pending = Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: state.db.get_entry(1).unwrap(),
            patch: Patch {
                progress: Some(4),
                ..Default::default()
            },
            error: Some("Saved failure".into()),
            conflict: true,
            missing_media: true,
            score_format: None,
            playback: false,
            base_score_format: None,
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        };
        state.db.put_pending(7, &pending).unwrap();
        state
            .db
            .0
            .lock()
            .execute(
                "UPDATE list_entry SET progress = 'broken' WHERE media_id = 1",
                [],
            )
            .unwrap();
        let trace = SqlTrace::new(&state.db);
        let commits = crate::db::durable_commit_count();
        let error = stage(
            &state,
            1,
            Patch {
                progress: Some(5),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "This anime was removed from AniList. Resolve its saved change before continuing."
        );
        assert!(!trace
            .statements
            .borrow()
            .iter()
            .any(|sql| sql.contains("FROM list_entry")));
        assert_eq!(crate::db::durable_commit_count(), commits);
        assert_eq!(
            serde_json::to_value(state.db.pending_change(7, 1).unwrap().unwrap()).unwrap(),
            serde_json::to_value(&pending).unwrap()
        );
    }

    #[tokio::test]
    async fn identical_preflight_failures_do_not_commit_but_changed_failures_do() {
        let unavailable = (503, json!({"errors":[{"message":"Unavailable"}]}));
        let invalid = (400, json!({"errors":[{"message":"Invalid score"}]}));
        let (api, mut requests) = anilist::mock_api(vec![
            unavailable.clone(),
            unavailable.clone(),
            unavailable,
            (503, json!({"errors":[{"message":"Different outage"}]})),
            invalid.clone(),
            invalid.clone(),
            invalid,
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
        let _remote = state.remote_lock.lock().await;
        for expected in [1, 0, 0, 1, 1] {
            let before = crate::db::durable_commit_count();
            let events = ENTRY_NOTIFICATIONS.get();
            assert!(flush_unlocked(&state).await.is_err());
            assert_eq!(crate::db::durable_commit_count() - before, expected);
            assert_eq!(ENTRY_NOTIFICATIONS.get() - events, expected);
        }
        let stale = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(stale.conflict);
        assert_eq!(stale.patch.progress, Some(4));
        let mut current = stale.clone();
        current.conflict = false;
        state.db.put_pending(7, &current).unwrap();
        let al = state.anilist.lock().clone();
        let before = crate::db::durable_commit_count();
        let events = ENTRY_NOTIFICATIONS.get();
        assert!(flush_one_unlocked(&state, state.origin(), &al, 7, stale)
            .await
            .is_err());
        assert_eq!(crate::db::durable_commit_count() - before, 1);
        assert_eq!(ENTRY_NOTIFICATIONS.get() - events, 1);
        let current = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(current.conflict);
        let before = crate::db::durable_commit_count();
        let events = ENTRY_NOTIFICATIONS.get();
        assert!(flush_one_unlocked(&state, state.origin(), &al, 7, current)
            .await
            .is_err());
        assert_eq!(crate::db::durable_commit_count(), before);
        assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
        for _ in 0..7 {
            let request = requests.recv().await.unwrap();
            assert_eq!(
                request["variables"],
                json!({"mediaId":1,"scoreFormat":"POINT_100"})
            );
            assert!(!request["query"].as_str().unwrap().contains("mutation"));
        }
        assert!(requests.try_recv().is_err());
        println!(
            "identical retry failures: 0 commits and events; changed error or conflict: 1 each"
        );
    }

    #[tokio::test]
    async fn unchanged_preflight_errors_still_reject_auth_and_mark_missing_media() {
        for (status, message) in [(401, "Invalid Token"), (404, "Not Found")] {
            let failure = (
                status,
                json!({"errors":[{"message":message,"status":status}]}),
            );
            let (api, mut requests) = anilist::mock_api(vec![failure.clone(), failure]).await;
            let error = api
                .entry_by_media_id(1, None)
                .await
                .err()
                .unwrap()
                .to_string();
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let (_, mut pending, _) = stage(
                &state,
                1,
                Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            pending.error = Some(error);
            pending.conflict = status == 404;
            state.db.put_pending(7, &pending).unwrap();
            let events = ENTRY_NOTIFICATIONS.get();
            let _remote = state.remote_lock.lock().await;
            let al = state.anilist.lock().clone();
            let error = flush_one_unlocked(&state, state.origin(), &al, 7, pending)
                .await
                .unwrap_err();
            let retained = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(retained.patch.progress, Some(4));
            if status == 401 {
                assert!(String::from(error).contains("expired"));
                assert!(!state.anilist.lock().has_token());
                assert_eq!(account_id(&state.db), None);
                assert!(!retained.conflict);
                assert!(!retained.missing_media);
                assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
            } else {
                assert!(retained.conflict);
                assert!(retained.missing_media);
                assert!(retained.error.unwrap().contains("removed or merged"));
                assert_eq!(ENTRY_NOTIFICATIONS.get() - events, 1);
            }
            assert!(state.db.get_entry(1).unwrap().is_none());
            for _ in 0..2 {
                let request = requests.recv().await.unwrap();
                assert!(!request["query"].as_str().unwrap().contains("mutation"));
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn failed_preflight_retains_edits_staged_while_the_request_is_in_flight() {
        for change_error in [false, true] {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let gate = Arc::new(tokio::sync::Semaphore::new(1));
                let failure = (503, json!({"errors":[{"message":"Unavailable"}]}));
                let (api, mut requests) =
                    anilist::mock_api_gated(vec![failure.clone(), failure], Some(gate.clone()))
                        .await;
                let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
                seed(&state);
                stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap();
                assert!(flush_unlocked(&state).await.is_err());
                requests.recv().await.unwrap();
                let stale = state.db.pending_change(7, 1).unwrap().unwrap();
                let original_error = stale.error.clone();
                let worker_state = state.clone();
                let worker = tokio::spawn(async move {
                    let _remote = worker_state.remote_lock.lock().await;
                    let al = worker_state.anilist.lock().clone();
                    flush_one_unlocked(&worker_state, worker_state.origin(), &al, 7, stale).await
                });
                requests.recv().await.unwrap();
                {
                    let _entry = state.entry_lock.lock().await;
                    let (_, mut current, _) = stage(
                        &state,
                        1,
                        Patch {
                            status: Some("PAUSED".into()),
                            progress: Some(5),
                            score: Some(0.0),
                            repeat: Some(2),
                            ..Default::default()
                        },
                    )
                    .unwrap()
                    .unwrap();
                    if change_error {
                        current.error = None;
                        state.db.put_pending(7, &current).unwrap();
                    }
                }
                let before = crate::db::durable_commit_count();
                let events = ENTRY_NOTIFICATIONS.get();
                gate.add_permits(1);
                assert!(worker.await.unwrap().is_err());
                assert_eq!(
                    crate::db::durable_commit_count() - before,
                    usize::from(change_error)
                );
                assert_eq!(
                    ENTRY_NOTIFICATIONS.get() - events,
                    usize::from(change_error)
                );
                let current = state.db.pending_change(7, 1).unwrap().unwrap();
                assert_eq!(current.base.unwrap().progress, 3);
                assert_eq!(current.patch.progress, Some(5));
                assert_eq!(current.patch.status.as_deref(), Some("PAUSED"));
                assert_eq!(current.patch.score, Some(0.0));
                assert_eq!(current.patch.repeat, Some(2));
                assert_eq!(current.error, original_error);
                assert!(!current.conflict);
                assert!(!current.missing_media);
                assert!(requests.try_recv().is_err());
            })
            .await
            .unwrap();
        }
    }

    fn seed_pending_batch(state: &AppState, count: i64) {
        seed(state);
        for media_id in 1..=count {
            state
                .db
                .upsert_entry(&ListEntry {
                    id: Some(media_id + 21),
                    media_id,
                    progress: 3,
                    status: "CURRENT".into(),
                    ..Default::default()
                })
                .unwrap();
            let (_, pending, _) = stage(
                state,
                media_id,
                Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            local_entry(state, &pending).unwrap();
        }
    }

    fn discovery_fixture(conflict: impl Fn(i64) -> bool) -> Db {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.durable_write(|conn| {
            let mut insert = conn.prepare("INSERT INTO pending_change VALUES (7,?,?)")?;
            for media_id in (1..=1000).rev() {
                let pending = Pending {
                    media_id,
                    deleting: false,
                    deletion_id: None,
                    base: None,
                    patch: Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                    error: None,
                    conflict: conflict(media_id),
                    missing_media: false,
                    score_format: None,
                    playback: false,
                    base_score_format: None,
                    rejected_progress: None,
                    retry_error: None,
                    rejected_total: None,
                };
                insert.execute(rusqlite::params![
                    media_id,
                    serde_json::to_string(&pending)?
                ])?;
            }
            conn.execute("INSERT INTO pending_change VALUES (8,1,'corrupt')", [])?;
            Ok(())
        })
        .unwrap();
        db
    }

    #[test]
    fn background_discovery_stops_parsing_after_ten_eligible_rows() {
        for pattern in 0..4 {
            let db = discovery_fixture(|id| match pattern {
                0 => false,
                1 => id % 2 == 1,
                2 => id <= 100,
                _ => true,
            });
            let before = PENDING_DESERIALIZATIONS.get();
            let expected = db
                .pending(7)
                .unwrap()
                .into_iter()
                .filter(|pending| !pending.conflict)
                .take(10)
                .map(|pending| pending.media_id)
                .collect::<Vec<_>>();
            assert_eq!(PENDING_DESERIALIZATIONS.get() - before, 1000);
            let before = PENDING_DESERIALIZATIONS.get();
            let writes = db.0.lock().total_changes();
            let trace = SqlTrace::new(&db);
            assert_eq!(db.pending_background_ids(7, None).unwrap(), expected);
            let parsed = PENDING_DESERIALIZATIONS.get() - before;
            assert_eq!(parsed, [10, 20, 110, 1000][pattern]);
            assert_eq!(trace.statements.borrow().len(), 1);
            assert_eq!(db.0.lock().total_changes(), writes);
            assert!(db.pending_background_ids(9, None).unwrap().is_empty());
            println!("background discovery pattern {pattern}: {parsed} parses instead of 1000");
        }
    }

    #[test]
    fn background_discovery_defers_only_unexamined_malformed_rows() {
        use rusqlite::types::Value;

        for malformed in [
            Value::Text("invalid".into()),
            Value::Text("{}".into()),
            Value::Blob(vec![255]),
        ] {
            let db = discovery_fixture(|_| false);
            db.0.lock()
                .execute(
                    "UPDATE pending_change SET payload=? WHERE account_id=7 AND media_id=11",
                    [&malformed],
                )
                .unwrap();
            let before = PENDING_DESERIALIZATIONS.get();
            assert_eq!(
                db.pending_background_ids(7, None).unwrap(),
                (1..=10).collect::<Vec<_>>()
            );
            assert_eq!(PENDING_DESERIALIZATIONS.get() - before, 10);
            assert!(db.pending(7).is_err());
            db.remove_pending(7, 1).unwrap();
            assert_eq!(
                db.pending_background_ids(7, None).unwrap_err().to_string(),
                db.pending(7).unwrap_err().to_string()
            );
            let retained: Value =
                db.0.lock()
                    .query_row(
                        "SELECT payload FROM pending_change WHERE account_id=7 AND media_id=11",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
            assert_eq!(retained, malformed);
        }
    }

    #[tokio::test]
    async fn manual_flush_still_checks_payloads_beyond_the_first_batch() {
        let db = discovery_fixture(|_| false);
        db.set_setting("anilist_user_id", "7").unwrap();
        db.0.lock()
            .execute(
                "UPDATE pending_change SET payload='invalid' WHERE account_id=7 AND media_id=11",
                [],
            )
            .unwrap();
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api, db);
        assert!(flush_unlocked(&state).await.is_err());
        assert!(requests.try_recv().is_err());
        assert_eq!(
            state
                .db
                .pending_change(7, 1)
                .unwrap()
                .unwrap()
                .patch
                .progress,
            Some(4)
        );
    }

    fn batch_replies(count: i64) -> Vec<(u16, serde_json::Value)> {
        (1..=count)
            .flat_map(|media_id| {
                let mut before = remote(3);
                before["id"] = json!(media_id + 21);
                before["mediaId"] = json!(media_id);
                let mut after = before.clone();
                after["progress"] = json!(4);
                [
                    (
                        200,
                        json!({"data":{"Media":{"id":media_id,"mediaListEntry":before}}}),
                    ),
                    (200, json!({"data":{"SaveMediaListEntry":after}})),
                ]
            })
            .collect()
    }

    #[tokio::test]
    async fn partial_save_waits_for_a_complete_read_before_acknowledging() {
        let mut partial = remote(4);
        partial["updatedAt"] = serde_json::Value::Null;
        let mut confirmed = remote(4);
        confirmed["updatedAt"] = json!(120);
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({
                "data":{"SaveMediaListEntry":partial},
                "errors":[{"message":"Could not load update timestamp","path":["SaveMediaListEntry","updatedAt"]}]
            })),
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":confirmed}}})),
        ]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);

        let saved = crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
            .await
            .unwrap();
        assert_eq!(saved.progress, 4);
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert_eq!(pending.patch.progress, Some(4));
        assert!(!pending.conflict);
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 4);

        flush_background(&state).await.unwrap();
        assert!(state.db.pending_change(7, 1).unwrap().is_none());
        assert_eq!(
            state.db.get_entry(1).unwrap().unwrap().updated_at,
            Some(120)
        );
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn omitted_entry_lookup_rotates_the_queue_after_restart() {
        let path = std::env::temp_dir().join(format!(
            "kurisu-incomplete-entry-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut replies = (1..=10)
            .map(|id| (200, json!({"data":{"Media":{"id":id}}})))
            .collect::<Vec<_>>();
        replies.extend(batch_replies(11).into_iter().skip(20));
        replies.push((503, json!({"errors":[{"message":"Unavailable"}]})));
        let (api, mut requests) = anilist::mock_api(replies).await;
        {
            let state = state(api.clone(), Db::open(&path).unwrap());
            seed_pending_batch(&state, 11);
            let error = flush_background(&state).await.unwrap_err();
            assert!(error.contains("mediaListEntry"), "{error}");
            assert_eq!(retry_after(&state.db, 7).unwrap(), Some(10));
            assert!(state
                .db
                .pending(7)
                .unwrap()
                .iter()
                .all(|item| !item.conflict));
        }
        let state = state(api, Db::open(&path).unwrap());
        assert!(flush_background(&state)
            .await
            .unwrap_err()
            .contains("Unavailable"));
        for id in 1..=11 {
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], id);
        }
        assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 32);
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
        assert!(state.db.pending_change(7, 11).unwrap().is_none());
        assert_eq!(retry_after(&state.db, 7).unwrap(), Some(10));
        drop(state);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn automatic_completion_replays_the_requested_episode_after_a_total_increase() {
        for (stepper, prior_status) in
            [(true, "CURRENT"), (true, "REPEATING"), (false, "REPEATING")]
        {
            let mut before = remote(11);
            before["status"] = json!(prior_status);
            before["repeat"] = json!(2);
            let mut confirmed = before.clone();
            confirmed["progress"] = json!(12);
            let (api, mut requests) = anilist::mock_api(vec![
                (503, json!({"errors":[{"message":"Offline"}]})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":before}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":confirmed}})),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = prior_status.into();
            base.progress = 11;
            base.repeat = 2;
            state.db.upsert_entry(&base).unwrap();

            let saved = if stepper {
                crate::commands::set_progress_inner(&state, 1, 12, Some(11)).await
            } else {
                crate::commands::save_entry_inner(&state, 1, None, Some(12), None, None).await
            }
            .unwrap();
            assert_eq!((saved.status.as_str(), saved.progress), ("COMPLETED", 12));
            assert_eq!(
                saved.repeat,
                if prior_status == "REPEATING" { 3 } else { 2 }
            );

            flush_background(&state).await.unwrap();
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert_eq!(
                requests.try_recv().unwrap()["variables"],
                json!({"id":22,"progress":12,"scoreFormat":"POINT_100"})
            );
            let saved = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (saved.status.as_str(), saved.progress, saved.repeat),
                (prior_status, 12, 2)
            );
            assert!(state.db.pending_change(7, 1).unwrap().is_none());
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn item_server_error_keeps_later_edits_moving() {
        for background in [false, true] {
            for failed_request in ["lookup", "save", "delete"] {
                let during_save = failed_request == "save";
                let deleting = failed_request == "delete";
                let mut replies = batch_replies(2);
                if !during_save {
                    replies.remove(1);
                }
                replies[usize::from(during_save)] = (
                    if during_save { 200 } else { 500 },
                    json!({"errors":[{"status":500,"message":"Entry temporarily unavailable"}]}),
                );
                let (api, mut requests) = anilist::mock_api(replies).await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed_pending_batch(&state, 2);
                if deleting {
                    let mut pending = state.db.pending_change(7, 1).unwrap().unwrap();
                    pending.deleting = true;
                    pending.deletion_id = Some(22);
                    pending.patch = Patch::default();
                    state.db.put_pending(7, &pending).unwrap();
                }
                let result = if background {
                    flush_background(&state).await
                } else {
                    sync_pending_changes(&state).await.map(drop)
                };
                assert!(result
                    .unwrap_err()
                    .contains("Entry temporarily unavailable"));
                let retained = state.db.pending(7).unwrap();
                assert_eq!(retained.len(), 1);
                assert_eq!(retained[0].media_id, 1);
                assert_eq!(retained[0].conflict, deleting);
                assert_eq!(retained[0].deletion_id, deleting.then_some(22));
                assert_eq!(retained[0].patch.progress, (!deleting).then_some(4));
                assert_eq!(state.db.get_entry(2).unwrap().unwrap().progress, 4);
                let first = requests.try_recv().unwrap();
                if deleting {
                    assert!(first["query"]
                        .as_str()
                        .unwrap()
                        .contains("DeleteMediaListEntry"));
                    assert_eq!(first["variables"]["id"], 22);
                } else {
                    assert_eq!(first["variables"]["mediaId"], 1);
                }
                if during_save {
                    assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
                }
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 2);
                assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 23);
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn batch_errors_stop_later_edits() {
        for background in [false, true] {
            for (status, message, verify_account) in [
                (500, "Viewer unavailable", true),
                (503, "Service unavailable", false),
                (429, "Rate limited", false),
                (401, "Invalid Token", false),
            ] {
                let transport_status = if status == 429 { 200 } else { status };
                let (api, mut requests) = anilist::mock_api(vec![(
                    transport_status,
                    json!({"errors":[{"message":message,"status":status}]}),
                )])
                .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed_pending_batch(&state, 2);
                if verify_account {
                    state.anilist.lock().set_token(Some("test-session".into()));
                }
                let result = if background {
                    flush_background(&state).await
                } else {
                    sync_pending_changes(&state).await.map(drop)
                };
                assert!(result.is_err());
                assert_eq!(state.db.pending(7).unwrap().len(), 2);
                let request = requests.try_recv().unwrap();
                if verify_account {
                    assert!(request["query"].as_str().unwrap().contains("Viewer"));
                } else {
                    assert_eq!(request["variables"]["mediaId"], 1);
                }
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn batch_errors_in_a_500_response_stop_later_edits() {
        for background in [false, true] {
            for body in [
                json!({"message":"Upstream service failed"}),
                json!({"errors":[{"message":"The AniList API has been temporarily disabled","status":500}]}),
                json!({"errors":[{"message":"Too many requests","status":429}]}),
                json!({"errors":[{"message":"Entry temporarily unavailable","status":500},
                    {"message":"Too many requests","status":429}]}),
            ] {
                let (api, mut requests) =
                    anilist::mock_api(vec![(500, body), (503, json!({}))]).await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed_pending_batch(&state, 2);
                let result = if background {
                    flush_background(&state).await
                } else {
                    sync_pending_changes(&state).await.map(drop)
                };
                assert!(result.is_err());
                assert_eq!(state.db.pending(7).unwrap().len(), 2);
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn a_lookup_without_a_confirmed_media_id_stops_the_batch() {
        for data in [json!({}), json!({"Media":null}), json!({"Media":{"id":2}})] {
            let (api, mut requests) = anilist::mock_api(vec![(200, json!({"data":data}))]).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed_pending_batch(&state, 2);

            assert!(flush_background(&state).await.is_err());
            assert_eq!(retry_after(&state.db, 7).unwrap(), None);
            assert_eq!(state.db.pending(7).unwrap().len(), 2);
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn later_batches_make_progress_after_restart_when_ten_entries_keep_failing() {
        for background in [false, true] {
            let path = std::env::temp_dir().join(format!(
                "kurisu-retry-cursor-{}-{}.db",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap()
            ));
            let failure = (
                500,
                json!({"errors":[{"message":"Entry temporarily unavailable","status":500}]}),
            );
            let mut replies = vec![failure.clone(); 10];
            replies.extend(batch_replies(11).into_iter().skip(20));
            replies.extend(vec![failure; 9]);
            let (api, mut requests) = anilist::mock_api(replies).await;
            for attempt in 0..2 {
                let state = state(api.clone(), Db::open(&path).unwrap());
                if attempt == 0 {
                    seed_pending_batch(&state, 11);
                }
                let result = if background {
                    flush_background(&state).await
                } else {
                    sync_pending_changes(&state).await.map(drop)
                };
                assert!(result.is_err());
            }
            for id in 1..=11 {
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], id);
            }
            assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 32);
            for id in 1..=9 {
                assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], id);
            }
            assert!(requests.try_recv().is_err());
            let db = Db::open(&path).unwrap();
            assert!(db.pending_change(7, 11).unwrap().is_none());
            assert_eq!(db.pending(7).unwrap().len(), 10);
            assert!(retry_after(&db, 9).unwrap().is_none());
            drop(db);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn a_failed_cursor_write_does_not_commit_half_of_an_entry_failure() {
        for background in [false, true] {
            for operation in ["lookup", "save", "delete"] {
                let mut replies = batch_replies(1);
                replies.truncate(usize::from(operation == "save"));
                replies.push((
                    500,
                    json!({"errors":[{"message":"Entry temporarily unavailable","status":500}]}),
                ));
                let (api, mut requests) = anilist::mock_api(replies).await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed_pending_batch(&state, 2);
                if operation == "delete" {
                    let mut pending = state.db.pending_change(7, 1).unwrap().unwrap();
                    pending.deleting = true;
                    pending.deletion_id = Some(22);
                    pending.patch = Patch::default();
                    state.db.put_pending(7, &pending).unwrap();
                }
                state
                    .db
                    .0
                    .lock()
                    .execute_batch(
                        "CREATE TRIGGER refuse_cursor BEFORE INSERT ON settings
                     WHEN NEW.key = 'pending_retry_cursor:7'
                     BEGIN SELECT RAISE(ABORT, 'fixture cursor write failed'); END;",
                    )
                    .unwrap();
                let result = if background {
                    flush_background(&state).await
                } else {
                    sync_pending_changes(&state).await.map(drop)
                };
                assert!(result.unwrap_err().contains("fixture cursor write failed"));
                let pending = state.db.pending_change(7, 1).unwrap().unwrap();
                assert!(pending.error.is_none(), "{operation}");
                assert!(pending.retry_error.is_none(), "{operation}");
                assert!(!pending.conflict, "{operation}");
                assert_eq!(retry_after(&state.db, 7).unwrap(), None);
                assert_eq!(state.db.pending(7).unwrap().len(), 2);
                for _ in 0..1 + usize::from(operation == "save") {
                    assert!(requests.try_recv().is_ok());
                }
                assert!(requests.try_recv().is_err());
            }
        }
    }

    async fn enqueue_waiter<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn background_sync_hands_lock_to_foreground_between_items() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(batch_replies(10), Some(gate.clone())).await;
            let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
            seed_pending_batch(&state, 10);
            let worker_state = state.clone();
            let worker = tokio::spawn(async move { flush_background(&worker_state).await });
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"mediaId":1,"scoreFormat":"POINT_100"})
            );
            let mut foreground = Box::pin(async {
                let _remote = state.remote_lock.lock().await;
                state.db.pending(7).unwrap().len()
            });
            enqueue_waiter(foreground.as_mut()).await;
            let start = std::time::Instant::now();
            let foreground = async {
                let remaining = foreground.await;
                eprintln!(
                    "foreground waited {:?}, pending items remaining: {remaining}",
                    start.elapsed()
                );
                remaining
            };
            let transport = async {
                for index in 0..20 {
                    if index > 0 {
                        requests.recv().await.unwrap();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    gate.add_permits(1);
                }
            };
            let (remaining, ()) = tokio::join!(foreground, transport);
            worker.await.unwrap().unwrap();
            assert_eq!(remaining, 9);
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(requests.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn background_sync_reloads_foreground_progress_before_the_next_preflight() {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let mut replies = batch_replies(2);
            replies[3].1["data"]["SaveMediaListEntry"]["progress"] = json!(5);
            let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
            let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
            seed_pending_batch(&state, 2);
            let worker_state = state.clone();
            let worker = tokio::spawn(async move { flush_background(&worker_state).await });
            requests.recv().await.unwrap();
            let mut foreground =
                Box::pin(crate::commands::set_progress_inner(&state, 2, 5, Some(4)));
            enqueue_waiter(foreground.as_mut()).await;
            gate.add_permits(4);
            let (edit, synced) = tokio::join!(foreground, worker);
            assert_eq!(edit.unwrap().progress, 5);
            synced.unwrap().unwrap();
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"mediaId":2,"scoreFormat":"POINT_100"})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":23,"progress":5,"scoreFormat":"POINT_100"})
            );
            assert!(requests.try_recv().is_err());
            assert!(state.db.pending(7).unwrap().is_empty());
            assert_eq!(state.db.get_entry(2).unwrap().unwrap().progress, 5);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn background_sync_skips_resolved_or_conflicted_pending_items() {
        for resolve in [true, false] {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let mut replies = batch_replies(2);
                replies.truncate(if resolve { 3 } else { 2 });
                let (api, mut requests) =
                    anilist::mock_api_gated(replies, Some(gate.clone())).await;
                let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
                seed_pending_batch(&state, 2);
                let worker_state = state.clone();
                let worker = tokio::spawn(async move { flush_background(&worker_state).await });
                requests.recv().await.unwrap();
                let mut foreground = Box::pin(async {
                    if resolve {
                        resolve_pending_inner(2, false, &state).await.unwrap();
                    } else {
                        let _remote = state.remote_lock.lock().await;
                        let _guard = state.entry_lock.lock().await;
                        let mut pending = state.db.pending_change(7, 2).unwrap().unwrap();
                        pending.conflict = true;
                        pending.error = Some("Synthetic conflict".into());
                        state.db.put_pending(7, &pending).unwrap();
                    }
                });
                enqueue_waiter(foreground.as_mut()).await;
                gate.add_permits(3);
                let ((), synced) = tokio::join!(foreground, worker);
                synced.unwrap().unwrap();
                assert_eq!(
                    requests.recv().await.unwrap()["variables"],
                    json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
                );
                let pending = state.db.pending_change(7, 2).unwrap();
                if resolve {
                    assert!(pending.is_none());
                    assert_eq!(
                        requests.recv().await.unwrap()["variables"],
                        json!({"mediaId":2,"scoreFormat":"POINT_100"})
                    );
                    assert_eq!(state.db.get_entry(2).unwrap().unwrap().progress, 3);
                } else {
                    assert!(pending.unwrap().conflict);
                    assert_eq!(state.db.get_entry(2).unwrap().unwrap().progress, 4);
                }
                assert!(requests.try_recv().is_err());
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn background_sync_stops_when_its_account_or_token_changes() {
        for (account, token) in [
            (9, Some("other-session")),
            (9, Some("test-session")),
            (7, Some("other-session")),
            (7, None),
        ] {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let (api, mut requests) =
                    anilist::mock_api_gated(batch_replies(1), Some(gate.clone())).await;
                let state = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
                seed_pending_batch(&state, 2);
                let worker_state = state.clone();
                let worker = tokio::spawn(async move { flush_background(&worker_state).await });
                requests.recv().await.unwrap();
                let mut foreground = Box::pin(async {
                    let _remote = state.remote_lock.lock().await;
                    let _guard = state.entry_lock.lock().await;
                    assert!(state.db.pending_change(7, 1).unwrap().is_none());
                    state
                        .db
                        .replace_account(
                            token.unwrap_or(""),
                            &User {
                                id: account,
                                name: "Synthetic account".into(),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    state.anilist.lock().set_token(token.map(str::to_string));
                    if account != 7 {
                        stage(
                            &state,
                            2,
                            Patch {
                                progress: Some(9),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    }
                });
                enqueue_waiter(foreground.as_mut()).await;
                gate.add_permits(2);
                let ((), synced) = tokio::join!(foreground, worker);
                synced.unwrap().unwrap();
                assert_eq!(
                    requests.recv().await.unwrap()["variables"],
                    json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
                );
                assert!(requests.try_recv().is_err());
                assert_eq!(
                    state
                        .db
                        .pending_change(7, 2)
                        .unwrap()
                        .unwrap()
                        .patch
                        .progress,
                    Some(4)
                );
                if account != 7 {
                    assert_eq!(
                        state
                            .db
                            .pending_change(account, 2)
                            .unwrap()
                            .unwrap()
                            .patch
                            .progress,
                        Some(9)
                    );
                    assert!(state.db.entries_with_media().unwrap().is_empty());
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn background_sync_preserves_the_ten_item_limit() {
        let (api, mut requests) = anilist::mock_api(batch_replies(10)).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed_pending_batch(&state, 11);
        flush_background(&state).await.unwrap();
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].media_id, 11);
        for media_id in 1..=10 {
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"mediaId":media_id,"scoreFormat":"POINT_100"})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":media_id + 21,"progress":4,"scoreFormat":"POINT_100"})
            );
        }
        assert!(requests.try_recv().is_err());
    }

    async fn wrong_entry_save_reply_preserves_pending(action: &str) {
        let mut baseline = remote(3);
        baseline["score"] = json!(37);
        let mut applied = baseline.clone();
        applied["progress"] = json!(4);
        let mut unrelated = remote(4);
        unrelated["id"] = json!(999);
        unrelated["score"] = json!(91);
        unrelated["repeat"] = json!(6);
        let mut replies = vec![];
        if action != "save" {
            replies.push((
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":baseline}}}),
            ));
        }
        replies.push((200, json!({"data":{"SaveMediaListEntry":unrelated}})));
        replies.push((
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":applied}}}),
        ));
        let (api, mut requests) = anilist::mock_api(replies).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.score = Some(37.0);
        state.db.upsert_entry(&entry).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            if action == "save" {
                let saved = crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
                    .await
                    .unwrap();
                assert_eq!(saved.id, Some(22));
            } else {
                let (_, pending, _) = stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                local_entry(&state, &pending).unwrap();
                let result = if action == "retry" {
                    let _remote = state.remote_lock.lock().await;
                    flush_unlocked(&state).await
                } else {
                    resolve_pending_inner(1, true, &state).await
                };
                assert!(result.unwrap_err().contains("entry 999 instead of 22"));
                let lookup = requests.recv().await.unwrap();
                assert_eq!(
                    lookup["variables"],
                    json!({"mediaId":1,"scoreFormat":"POINT_100"})
                );
            }
            let cached = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (cached.id, cached.progress, cached.repeat),
                (Some(22), 4, 0)
            );
            assert_eq!(cached.score, Some(37.0));
            let mut changes = state.db.pending(7).unwrap();
            assert_eq!(changes.len(), 1);
            let pending = changes.remove(0);
            assert_eq!(pending.base.as_ref().unwrap().id, Some(22));
            assert_eq!(pending.patch.progress, Some(4));
            assert!(!pending.conflict);
            assert!(!pending.missing_media);
            assert!(pending.error.unwrap().contains("entry 999 instead of 22"));
            assert_eq!(
                state.anilist.lock().token().as_deref(),
                Some("test-session")
            );
            assert_eq!(state.db.cached_user().unwrap().unwrap().id, 7);
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
            );
            {
                let _remote = state.remote_lock.lock().await;
                flush_unlocked(&state).await.unwrap();
            }
            assert!(state.db.pending(7).unwrap().is_empty());
            let cached = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (cached.id, cached.progress, cached.repeat),
                (Some(22), 4, 0)
            );
            assert_eq!(cached.score, Some(37.0));
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"mediaId":1,"scoreFormat":"POINT_100"})
            );
            assert!(requests.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn wrong_entry_reply_during_save_keeps_the_identity_and_pending_intent() {
        wrong_entry_save_reply_preserves_pending("save").await;
    }

    #[tokio::test]
    async fn wrong_entry_reply_during_retry_keeps_the_identity_and_pending_intent() {
        wrong_entry_save_reply_preserves_pending("retry").await;
    }

    #[tokio::test]
    async fn wrong_entry_reply_during_resolution_keeps_the_identity_and_pending_intent() {
        wrong_entry_save_reply_preserves_pending("resolve").await;
    }

    #[tokio::test]
    async fn incomplete_remote_lookup_cannot_discard_saved_progress() {
        for reply in [
            json!({"data":{"Media":{}}}),
            json!({"data":{"Media":null}}),
            json!({"data":{}}),
        ] {
            let (api, mut requests) = anilist::mock_api(vec![
                (200, reply),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
                ),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let (_, mut pending, _) = stage(
                &state,
                1,
                Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            pending.conflict = true;
            state.db.put_pending(7, &pending).unwrap();
            local_entry(&state, &pending).unwrap();

            assert!(resolve_pending_inner(1, false, &state).await.is_err());
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
            assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
            assert!(state.anilist.lock().has_token());

            resolve_pending_inner(1, false, &state).await.unwrap();
            assert!(state.db.get_entry(1).unwrap().is_none());
            assert!(state.db.pending(7).unwrap().is_empty());
            requests.recv().await.unwrap();
            requests.recv().await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn incomplete_remote_lookup_keeps_existing_changes_retryable() {
        let (api, mut requests) =
            anilist::mock_api(vec![(200, json!({"data":{"Media":{}}}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();

        assert!(flush_unlocked(&state).await.is_err());
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending[0].patch.progress, Some(4));
        assert!(!pending[0].conflict);
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn incomplete_remote_lookup_cannot_turn_a_queued_add_into_a_blind_write() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{}}})),
            (200, json!({"data":{"SaveMediaListEntry":remote(4)}})),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_entry(1).unwrap();
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                status: Some("CURRENT".into()),
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();

        assert!(flush_unlocked(&state).await.is_err());
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending[0].patch.progress, Some(4));
        assert!(!pending[0].conflict);
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn progress_saves_pin_scores_to_the_cached_format() {
        for (format, score, default_score) in [("POINT_100", 80.0, 4.0), ("POINT_5", 4.0, 80.0)] {
            for action in [
                "save",
                "add",
                "restore",
                "tracking",
                "retry",
                "acknowledge",
                "keep_remote",
                "keep_local",
            ] {
                let lookup = match action {
                    "save" => None,
                    "add" => Some(serde_json::Value::Null),
                    "acknowledge" => Some(remote(4)),
                    _ => Some(remote(3)),
                };
                let writes = !matches!(action, "acknowledge" | "keep_remote");
                let mut replies = Vec::new();
                if let Some(lookup) = &lookup {
                    replies.push((
                        200,
                        json!({"data":{"Media":{"id":1,"mediaListEntry":lookup}}}),
                    ));
                }
                if writes {
                    replies.push((200, json!({"data":{"SaveMediaListEntry":remote(4)}})));
                }
                let count = replies.len();
                let (api, mut requests) =
                    anilist::mock_api_mapped(replies, None, move |request, response| {
                        let pinned = request["query"]
                            .as_str()
                            .unwrap()
                            .contains("score(format: $scoreFormat)")
                            && request["variables"]["scoreFormat"] == format;
                        for path in ["/data/Media/mediaListEntry", "/data/SaveMediaListEntry"] {
                            if let Some(entry) =
                                response.pointer_mut(path).filter(|e| e.is_object())
                            {
                                entry["score"] = json!(if pinned { score } else { default_score });
                            }
                        }
                    })
                    .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed(&state);
                let mut user = state.db.cached_user().unwrap().unwrap();
                user.score_format = Some(format.into());
                state.db.replace_account("test-session", &user).unwrap();
                let mut entry = state.db.get_entry(1).unwrap().unwrap();
                entry.score = Some(score);
                state.db.upsert_entry(&entry).unwrap();
                if matches!(action, "add" | "restore") {
                    state.db.delete_entry(1).unwrap();
                }
                match action {
                    "save" | "add" | "restore" => {
                        let saved =
                            crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
                                .await
                                .unwrap();
                        assert_eq!(saved.score, Some(score), "{format} {action}");
                    }
                    "tracking" => {
                        let saved = crate::commands::watcher_set_progress(
                            &state,
                            1,
                            4,
                            Some("test-session"),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(saved.score, Some(score), "{format} {action}");
                    }
                    _ => {
                        let (_, pending, _) = stage(
                            &state,
                            1,
                            Patch {
                                progress: Some(4),
                                ..Default::default()
                            },
                        )
                        .unwrap()
                        .unwrap();
                        local_entry(&state, &pending).unwrap();
                        if matches!(action, "keep_remote" | "keep_local") {
                            resolve_pending_inner(1, action == "keep_local", &state)
                                .await
                                .unwrap();
                        } else {
                            flush_unlocked(&state).await.unwrap();
                        }
                    }
                }
                let saved = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!(saved.score, Some(score), "{format} {action}");
                assert_eq!(saved.progress, if action == "keep_remote" { 3 } else { 4 });
                assert!(state.db.pending(7).unwrap().is_empty());
                assert_eq!(
                    state.db.cached_user().unwrap().unwrap().score_format,
                    Some(format.into())
                );
                for _ in 0..count {
                    let request = requests.recv().await.unwrap();
                    assert_eq!(request["variables"]["scoreFormat"], format);
                    assert!(request["variables"].get("score").is_none());
                    assert!(!request["query"].as_str().unwrap().contains("Viewer"));
                }
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn progress_saves_pin_retry_scores_without_losing_offline_changes() {
        let (api, mut requests) = anilist::mock_api_mapped(
            vec![
                (503, json!({"errors":[{"message":"Unavailable"}]})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":remote(4)}})),
            ],
            None,
            |request, response| {
                let pinned = request["query"]
                    .as_str()
                    .unwrap()
                    .contains("score(format: $scoreFormat)")
                    && request["variables"]["scoreFormat"] == "POINT_100";
                for path in ["/data/Media/mediaListEntry", "/data/SaveMediaListEntry"] {
                    if let Some(entry) = response.pointer_mut(path) {
                        entry["score"] = json!(if pinned { 80 } else { 4 });
                    }
                }
            },
        )
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.score = Some(70.0);
        state.db.upsert_entry(&entry).unwrap();
        let saved = crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
            .await
            .unwrap();
        assert_eq!((saved.progress, saved.score), (4, Some(70.0)));
        let pending = state.db.pending(7).unwrap().remove(0);
        assert_eq!(pending.patch.progress, Some(4));
        assert_eq!(pending.patch.score, None);
        assert!(!pending.conflict);
        flush_unlocked(&state).await.unwrap();
        let saved = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!((saved.progress, saved.score), (4, Some(80.0)));
        assert!(state.db.pending(7).unwrap().is_empty());
        for _ in 0..3 {
            let request = requests.recv().await.unwrap();
            assert_eq!(request["variables"]["scoreFormat"], "POINT_100");
            assert!(request["variables"].get("score").is_none());
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn remote_entry_timestamps_survive_every_sync_outcome() {
        for action in ["retry", "acknowledge", "keep_remote", "keep_local", "save"] {
            let mut before = remote(if action == "acknowledge" { 4 } else { 3 });
            before["updatedAt"] = json!(1_700_000_000);
            let mut after = remote(4);
            after["updatedAt"] = json!(1_700_000_500);
            let writes = matches!(action, "retry" | "keep_local" | "save");
            let mut replies = vec![];
            if action != "save" {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                ));
            }
            if writes {
                replies.push((200, json!({"data":{"SaveMediaListEntry":after}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            if action == "save" {
                crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
                    .await
                    .unwrap();
            } else {
                let (_, pending, _) = stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                local_entry(&state, &pending).unwrap();
                if matches!(action, "keep_remote" | "keep_local") {
                    resolve_pending_inner(1, action == "keep_local", &state)
                        .await
                        .unwrap();
                } else {
                    flush_unlocked(&state).await.unwrap();
                }
            }

            let entry = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                entry.updated_at,
                Some(if writes { 1_700_000_500 } else { 1_700_000_000 }),
                "{action} must preserve the remote update time used by Updated sorting"
            );
            assert_eq!(entry.progress, if action == "keep_remote" { 3 } else { 4 });
            assert!(state.db.pending(7).unwrap().is_empty());
            let count = if writes && action != "save" { 2 } else { 1 };
            for _ in 0..count {
                let request = requests.recv().await.unwrap();
                assert!(request["query"].as_str().unwrap().contains("updatedAt"));
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn adversarial_retry_keeps_the_entry_identity_when_deleted_after_preflight() {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
            ),
            (404, json!({"errors":[{"message":"Not Found"}]})),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();

        assert!(flush_unlocked(&state).await.is_err());
        requests.recv().await.unwrap();
        let mutation = requests.recv().await.unwrap();
        assert_eq!(
            mutation["variables"],
            json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
        );
        let pending = state.db.pending(7).unwrap().remove(0);
        assert!(pending.conflict);
        assert!(!pending.missing_media);
        assert_eq!(pending.patch.progress, Some(4));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        assert!(state.db.get_media(1).unwrap().is_some());
    }

    #[tokio::test]
    async fn adversarial_invalid_edits_do_not_enter_or_replace_the_offline_queue() {
        for queued in [false, true] {
            for (score, repeat) in [(Some(101.0), None), (None, Some(1001))] {
                let (api, mut requests) =
                    anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))])
                        .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed(&state);
                if queued {
                    crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
                        .await
                        .unwrap();
                    requests.recv().await.unwrap();
                }
                let result =
                    crate::commands::save_entry_inner(&state, 1, None, None, score, repeat).await;
                assert!(
                    result.is_err(),
                    "invalid score {score:?} or repeat {repeat:?} was saved"
                );
                let entry = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!(entry.score, Some(0.0));
                assert_eq!(entry.repeat, 0);
                assert_eq!(entry.progress, if queued { 4 } else { 3 });
                let pending = state.db.pending(7).unwrap();
                assert_eq!(pending.len(), usize::from(queued));
                if queued {
                    assert_eq!(pending[0].patch.progress, Some(4));
                    assert_eq!(pending[0].patch.score, None);
                    assert_eq!(pending[0].patch.repeat, None);
                }
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn queued_adds_can_still_create_a_new_remote_entry() {
        let (api, mut requests) = anilist::mock_api(vec![
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":remote(4)}})),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_entry(1).unwrap();
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                status: Some("CURRENT".into()),
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();
        flush_unlocked(&state).await.unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"mediaId":1,"status":"CURRENT","progress":4,"scoreFormat":"POINT_100"})
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().id, Some(22));
    }

    #[tokio::test]
    async fn unknown_episode_totals_still_enforce_graphql_progress_limits() {
        let (api, mut requests) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET episodes = NULL", [])
            .unwrap();
        assert!(crate::commands::save_entry_inner(
            &state,
            1,
            None,
            Some(i32::MAX as i64 + 1),
            None,
            None,
        )
        .await
        .is_err());
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
        assert!(requests.try_recv().is_err());
        let valid =
            crate::commands::save_entry_inner(&state, 1, None, Some(i32::MAX as i64), None, None)
                .await
                .unwrap();
        assert_eq!(valid.progress, i32::MAX as i64);
        assert_eq!(
            state.db.pending(7).unwrap()[0].patch.progress,
            Some(i32::MAX as i64)
        );
    }

    #[tokio::test]
    async fn keeping_local_changes_uses_the_current_remote_entry_identity() {
        let (api, mut requests) = anilist::mock_api(vec![
            (200, json!({"data":{"Media":{"id":1,"mediaListEntry":{"id":33,"mediaId":1,"status":"CURRENT","progress":8,"score":9,"repeat":0}}}})),
            (200, json!({"data":{"SaveMediaListEntry":{"mediaId":1,"id":33,"status":"CURRENT","progress":4,"score":9,"repeat":0}}})),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (account, mut pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        pending.conflict = true;
        state.db.put_pending(account, &pending).unwrap();
        local_entry(&state, &pending).unwrap();
        resolve_pending_inner(1, true, &state).await.unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":33,"progress":4,"scoreFormat":"POINT_100"})
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        let entry = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(entry.id, Some(33));
        assert_eq!(entry.progress, 4);
        assert_eq!(entry.score, Some(9.0));
    }

    #[tokio::test]
    async fn offline_progress_survives_restart_and_syncs_without_touching_remote_score() {
        let path = std::env::temp_dir().join(format!(
            "kurisu-pending-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let first = state(api, Db::open(&path).unwrap());
        seed(&first);
        let saved = crate::commands::watcher_set_progress(&first, 1, 4, Some("test-session"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.progress, 4);
        assert_eq!(first.db.pending(7).unwrap().len(), 1);
        drop(first);
        let (api,mut requests) = anilist::mock_api(vec![
            (200,json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}})),
            (200,json!({"data":{"SaveMediaListEntry":{"mediaId":1,"id":22,"status":"CURRENT","progress":4,"score":9,"repeat":0}}})),
        ]).await;
        let restarted = state(api, Db::open(&path).unwrap());
        assert_eq!(restarted.db.get_entry(1).unwrap().unwrap().progress, 4);
        flush_unlocked(&restarted).await.unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
        );
        assert!(restarted.db.pending(7).unwrap().is_empty());
        assert_eq!(restarted.db.get_entry(1).unwrap().unwrap().score, Some(9.0));
        drop(restarted);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn automatic_progress_checks_remote_changes_before_its_first_write() {
        let mut rewatch = remote(0);
        rewatch["status"] = json!("REPEATING");
        rewatch["repeat"] = json!(1);
        let mut paused = remote(3);
        paused["status"] = json!("PAUSED");
        let mut recreated = remote(3);
        recreated["id"] = json!(23);
        let mut another_cycle = remote(4);
        another_cycle["repeat"] = json!(1);
        for remote in [
            remote(8),
            remote(2),
            rewatch,
            paused,
            recreated,
            another_cycle,
            json!(null),
        ] {
            let (api, mut requests) = anilist::mock_api(vec![(
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote}}}),
            )])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let entry = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
                .await
                .unwrap()
                .unwrap();
            let request = requests.recv().await.unwrap();
            assert!(
                !request["query"].as_str().unwrap().contains("mutation"),
                "automatic tracking sent an unchecked write: {request}"
            );
            assert!(requests.try_recv().is_err(), "remote: {remote}");
            let pending = state.db.pending(7).unwrap();
            assert_eq!(pending.len(), 1, "remote: {remote}");
            assert!(pending[0].conflict, "remote: {remote}");
            assert_eq!(pending[0].base.as_ref().unwrap().progress, 3);
            assert_eq!(pending[0].patch.progress, Some(4));
            assert_eq!(entry.progress, 4);
        }
    }

    #[tokio::test]
    async fn automatic_progress_requires_an_identified_account_before_writing() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_setting("anilist_user_id").unwrap();
        let error = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .unwrap_err();
        assert!(error.contains("Reconnect"));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn automatic_progress_auth_rejection_preserves_previously_saved_changes() {
        let (api, mut requests) = anilist::mock_api(vec![(
            401,
            json!({"errors":[{"message":"Invalid Token","status":401}]}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .upsert_media(&Media {
                id: 2,
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(23),
                media_id: 2,
                status: "CURRENT".into(),
                progress: 2,
                score: Some(8.0),
                repeat: 1,
                ..Default::default()
            })
            .unwrap();
        let (_, retained, _) = stage(
            &state,
            2,
            Patch {
                progress: Some(5),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        let error = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .unwrap_err();
        assert!(error.contains("expired"));
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.entries_with_media().unwrap().is_empty());
        let pending = state.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            serde_json::to_value(&pending[0]).unwrap(),
            serde_json::to_value(retained).unwrap()
        );
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn queued_playback_rechecks_the_detected_episode_before_completing_a_rewatch() {
        for keep_local in [false, true] {
            let mut before = remote(11);
            before["status"] = json!("REPEATING");
            before["repeat"] = json!(2);
            let mut after = before.clone();
            after["progress"] = json!(12);
            let (api, mut requests) = anilist::mock_api(vec![
                (503, json!({"errors":[{"message":"Unavailable"}]})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":before}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":after}})),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "REPEATING".into();
            base.progress = 11;
            base.repeat = 2;
            state.db.upsert_entry(&base).unwrap();
            let local = crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!((local.status.as_str(), local.repeat), ("COMPLETED", 3));
            if keep_local {
                state.db.cache_episode_total(1, Some(13)).unwrap();
                overlay(&state).unwrap();
                let local = state.db.get_entry(1).unwrap().unwrap();
                assert_eq!(
                    (local.status.as_str(), local.progress, local.repeat),
                    ("REPEATING", 12, 2)
                );
                resolve_pending_inner(1, true, &state).await.unwrap();
            } else {
                flush_unlocked(&state).await.unwrap();
            }
            requests.try_recv().unwrap();
            requests.try_recv().unwrap();
            let mutation = requests.try_recv().unwrap();
            assert_eq!(
                mutation["variables"],
                json!({"id":22, "progress":12, "scoreFormat":"POINT_100"})
            );
            assert!(requests.try_recv().is_err());
            assert!(state.db.pending(7).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn automatic_playback_uses_the_episode_total_from_its_existing_lookup() {
        for (total, episode, fail_write) in [
            (13, 12, false),
            (13, 12, true),
            (11, 11, false),
            (11, 12, false),
        ] {
            let mut before = remote(10);
            before["status"] = json!("REPEATING");
            before["repeat"] = json!(2);
            let completed = episode == total;
            let mut after = before.clone();
            after["progress"] = json!(episode);
            if completed {
                after["status"] = json!("COMPLETED");
                after["repeat"] = json!(3);
            }
            let mut replies = vec![(
                200,
                json!({"data":{"Media":{"id":1,"episodes":total,"mediaListEntry":before}}}),
            )];
            if episode <= total {
                replies.push(if fail_write {
                    (503, json!({"errors":[{"message":"Unavailable"}]}))
                } else {
                    (200, json!({"data":{"SaveMediaListEntry":after}}))
                });
            }
            let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(replies, Some(gate.clone())).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            let mut base = state.db.get_entry(1).unwrap().unwrap();
            base.status = "REPEATING".into();
            base.repeat = 2;
            base.progress = 10;
            state.db.upsert_entry(&base).unwrap();

            let playback =
                crate::commands::watcher_set_progress(&state, 1, episode, Some("test-session"));
            let observe = async {
                requests.recv().await.unwrap();
                overlay(&state).unwrap();
                gate.add_permits(2);
            };
            let (result, ()) = tokio::join!(playback, observe);
            assert_eq!(state.db.episode_total(1).unwrap(), Some(total));
            let cached = state.db.get_entry(1).unwrap().unwrap();
            if episode > total {
                assert!(result.unwrap_err().contains("offset"));
                assert_eq!(
                    (cached.progress, cached.repeat, cached.status.as_str()),
                    (10, 2, "REPEATING")
                );
            } else {
                let entry = result.unwrap().unwrap();
                let write = requests.try_recv().unwrap();
                assert_eq!(write["variables"]["progress"], episode);
                assert_eq!(
                    write["variables"]["status"].as_str(),
                    completed.then_some("COMPLETED")
                );
                assert_eq!(
                    write["variables"]["repeat"].as_i64(),
                    completed.then_some(3)
                );
                assert_eq!(entry.progress, episode);
                assert_eq!(cached.status, after["status"].as_str().unwrap());
                assert_eq!(cached.repeat, if completed { 3 } else { 2 });
            }
            assert_eq!(state.db.pending(7).unwrap().len(), usize::from(fail_write));
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn automatic_progress_preserves_remote_scores_and_completes_rewatches_once() {
        for repeating in [false, true] {
            let (progress, episode) = if repeating { (11, 12) } else { (3, 4) };
            let mut before = remote(progress);
            before["score"] = json!(9);
            if repeating {
                before["status"] = json!("REPEATING");
                before["repeat"] = json!(2);
            }
            let mut after = before.clone();
            after["progress"] = json!(episode);
            if repeating {
                after["status"] = json!("COMPLETED");
                after["repeat"] = json!(3);
            }
            let (api, mut requests) = anilist::mock_api(vec![
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":before}}}),
                ),
                (200, json!({"data":{"SaveMediaListEntry":after}})),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            if repeating {
                let mut entry = state.db.get_entry(1).unwrap().unwrap();
                entry.progress = progress;
                entry.status = "REPEATING".into();
                entry.repeat = 2;
                state.db.upsert_entry(&entry).unwrap();
            }
            let entry =
                crate::commands::watcher_set_progress(&state, 1, episode, Some("test-session"))
                    .await
                    .unwrap()
                    .unwrap();
            let lookup = requests.recv().await.unwrap();
            assert_eq!(
                lookup["variables"],
                json!({"mediaId":1,"scoreFormat":"POINT_100"})
            );
            let saved = requests.recv().await.unwrap();
            assert_eq!(
                saved["variables"],
                if repeating {
                    json!({"id":22,"progress":12,"status":"COMPLETED","repeat":3,"scoreFormat":"POINT_100"})
                } else {
                    json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
                }
            );
            assert_eq!(entry.score, Some(9.0));
            assert_eq!(entry.repeat, if repeating { 3 } else { 0 });
            let cached = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (
                    cached.status.as_str(),
                    cached.progress,
                    cached.score,
                    cached.repeat
                ),
                (
                    if repeating { "COMPLETED" } else { "CURRENT" },
                    episode,
                    Some(9.0),
                    if repeating { 3 } else { 0 }
                )
            );
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(crate::commands::watcher_set_progress(
                &state,
                1,
                episode,
                Some("test-session")
            )
            .await
            .unwrap()
            .is_none());
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn automatic_progress_acknowledges_a_remotely_completed_rewatch() {
        let mut applied = remote(12);
        applied["status"] = json!("COMPLETED");
        applied["repeat"] = json!(3);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"mediaListEntry":applied}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.progress = 11;
        entry.status = "REPEATING".into();
        entry.repeat = 2;
        state.db.upsert_entry(&entry).unwrap();
        let saved = crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, "COMPLETED");
        assert_eq!(saved.repeat, 3);
        assert_eq!(saved.progress, 12);
        let cached = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (cached.status.as_str(), cached.progress, cached.repeat),
            ("COMPLETED", 12, 3)
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        requests.recv().await.unwrap();
        assert!(
            crate::commands::watcher_set_progress(&state, 1, 12, Some("test-session"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn automatic_progress_keeps_removed_media_and_rejected_writes_distinct() {
        for missing in [false, true] {
            let mut replies = vec![];
            if !missing {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ));
            }
            replies.push(if missing {
                (
                    404,
                    json!({"errors":[{"message":"Not Found","status":404}]}),
                )
            } else {
                (
                    400,
                    json!({"errors":[{"message":"Invalid progress","status":400}]}),
                )
            });
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            assert!(
                crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
                    .await
                    .is_err()
            );
            let pending = state.db.pending(7).unwrap();
            if missing {
                assert!(pending[0].missing_media);
                assert_eq!(pending[0].patch.progress, Some(4));
                assert!(state.db.get_entry(1).unwrap().is_none());
            } else {
                assert!(pending.is_empty());
                assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
                requests.recv().await.unwrap();
            }
            requests.recv().await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn newer_remote_progress_is_never_overwritten_by_retry() {
        let (api, mut requests) = anilist::mock_api(vec![
            (503, json!({"errors":[{"message":"Unavailable"}]})),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(8)}}}),
            ),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .unwrap();
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap()[0].conflict);
        requests.recv().await.unwrap();
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_matching_remote_add_without_a_confirmed_identity_requires_resolution() {
        for legacy in [false, true] {
            let (api, mut requests) = anilist::mock_api(vec![(
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(4)}}}),
            )])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state.db.delete_entry(1).unwrap();
            let (_, mut pending, _) = stage(
                &state,
                1,
                Patch {
                    status: Some("CURRENT".into()),
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            if legacy {
                pending.patch.creation_key = None;
                state.db.put_pending(7, &pending).unwrap();
            }
            flush_unlocked(&state).await.unwrap();
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.conflict);
            assert_eq!(pending.error.as_deref(), Some(ENTRY_REPLACED));
            let entry = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(entry.progress, 4);
            assert!(entry.queued_creation.is_none());
            requests.recv().await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn replacement_identity_takes_priority_over_playback_progress_conflict() {
        let mut replacement = remote(2);
        replacement["id"] = json!(33);
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"id":1,"episodes":3,"mediaListEntry":replacement}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_entry(1).unwrap();
        let (account, pending, _) = prepare_entry(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
            Some(4),
            None,
        )
        .unwrap()
        .unwrap();
        state.db.put_pending(account, &pending).unwrap();
        local_entry(&state, &pending).unwrap();

        flush_background(&state).await.unwrap();

        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.conflict);
        assert_eq!(pending.error.as_deref(), Some(ENTRY_REPLACED));
        assert_eq!(pending.patch.progress, Some(4));
        let entry = crate::commands::get_entry(1, &state)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((entry.id, entry.progress), (Some(33), 2));
        assert!(entry.queued_creation.is_none());
        assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn confirmed_same_id_save_caches_entry_when_queue_marker_cannot_be_written() {
        for replay in [false, true] {
            let mut replies = Vec::new();
            if replay {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ));
            }
            replies.push((200, json!({"data":{"SaveMediaListEntry":remote(4)}})));
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TRIGGER refuse_save_marker BEFORE INSERT ON pending_change
                 WHEN json_extract(NEW.payload, '$.patch.progress') IS NULL
                 BEGIN SELECT RAISE(FAIL, 'fixture marker failure'); END;",
                )
                .unwrap();

            if replay {
                stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap();
                flush_background(&state).await.unwrap();
                requests.try_recv().unwrap();
            } else {
                crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None)
                    .await
                    .unwrap();
            }
            assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
            assert!(state.db.pending(7).unwrap().is_empty());
            flush_background(&state).await.unwrap();
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn confirmed_completion_keeps_old_cache_consistent_when_the_total_write_fails() {
        for scored in [false, true] {
            let mut completed = remote(13);
            completed["status"] = json!("COMPLETED");
            let mut replies = Vec::new();
            if scored {
                completed["score"] = json!(80);
                replies.push((200, viewer("POINT_100")));
            }
            replies.extend([
                (200, json!({"data":{"SaveMediaListEntry":completed}})),
                (
                    200,
                    json!({"data":{"Media":{"id":1,"episodes":13,"mediaListEntry":completed}}}),
                ),
            ]);
            if scored {
                replies.push((200, viewer("POINT_100")));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TRIGGER refuse_new_total BEFORE INSERT ON media WHEN NEW.episodes = 13
                 BEGIN SELECT RAISE(FAIL, 'fixture total failure'); END;",
                )
                .unwrap();

            let error = crate::commands::save_entry_inner(
                &state,
                1,
                Some("COMPLETED".into()),
                None,
                scored.then_some(80.0),
                None,
            )
            .await
            .unwrap_err();
            assert!(error.contains("fixture total failure"), "{error}");
            let entry = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!((entry.status.as_str(), entry.progress), ("CURRENT", 3));
            assert_eq!(state.db.episode_total(1).unwrap(), Some(12));
            assert!(
                state.db.pending(7).unwrap()[0].needs_cleanup_only(),
                "scored={scored}"
            );
            flush_background(&state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
            if scored {
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("Viewer"));
            }
            assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 12);
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            if scored {
                assert!(requests.try_recv().unwrap()["query"]
                    .as_str()
                    .unwrap()
                    .contains("Viewer"));
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn a_cache_write_failure_leaves_confirmed_updates_safe_to_clean_up() {
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"SaveMediaListEntry":remote(4)}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER ignore_confirmed_entry BEFORE INSERT ON list_entry
                 BEGIN SELECT RAISE(IGNORE); END;",
            )
            .unwrap();

        let result = crate::commands::save_entry_inner(&state, 1, None, Some(4), None, None).await;
        assert!(result
            .unwrap_err()
            .contains("confirmed entry was not cached"));
        let pending = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(pending.needs_cleanup_only());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(requests.try_recv().unwrap()["variables"]["progress"], 4);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn confirmed_creation_survives_failed_queue_acknowledgement() {
        for replay in [false, true] {
            let saved = remote(4);
            let absent = (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
            );
            let (api, mut requests) = anilist::mock_api(vec![
                absent,
                (200, json!({"data":{"SaveMediaListEntry":saved}})),
            ])
            .await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state.db.delete_entry(1).unwrap();
            fail_pending_acknowledgement(&state);

            let error = if replay {
                stage(
                    &state,
                    1,
                    Patch {
                        status: Some("CURRENT".into()),
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap();
                flush_unlocked(&state).await.unwrap_err()
            } else {
                crate::commands::save_entry_inner(
                    &state,
                    1,
                    Some("CURRENT".into()),
                    Some(4),
                    None,
                    None,
                )
                .await
                .unwrap_err()
            };
            assert!(error.contains("fixture acknowledgement failure"), "{error}");
            let entry = crate::commands::get_entry(1, &state)
                .await
                .unwrap()
                .unwrap();
            assert_eq!((entry.id, entry.progress), (Some(22), 4));
            assert!(entry.queued_creation.is_some());
            let pending = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(pending.needs_cleanup_only());
            assert_eq!(pending.base.unwrap().id, Some(22));
            state.db.replace_list_snapshot(&[]).unwrap();
            assert!(crate::commands::local_entries(&state)
                .await
                .unwrap()
                .is_empty());

            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_pending_ack")
                .unwrap();
            flush_unlocked(&state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert_eq!(requests.try_recv().unwrap()["variables"]["mediaId"], 1);
            assert!(requests.try_recv().is_err());
        }
    }

    fn seed_competing_title(state: &AppState) {
        seed(state);
        let mut media = state.db.get_media(1).unwrap().unwrap();
        media.id = 2;
        state.db.upsert_media(&media).unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                id: Some(23),
                media_id: 2,
                status: "REPEATING".into(),
                ..Default::default()
            })
            .unwrap();
        state.refresh_matchers();
        assert_title_winner(state, 1);
    }

    fn assert_title_winner(state: &AppState, expected: i64) {
        let matchers = state.matchers.lock();
        let mut ids: Vec<_> = matchers.iter().map(|m| m.media_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        let winner = crate::recognize::match_title(&matchers, "Test show - 04.mkv", "");
        assert_eq!(winner.map(|m| m.media_id), Some(expected));
    }

    #[tokio::test]
    async fn replay_refreshes_matchers_when_pending_acknowledgement_fails() {
        for resend in [false, true] {
            let mut saved = remote(4);
            saved["status"] = json!("PAUSED");
            let lookup = |entry| {
                (
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":entry}}}),
                )
            };
            let mut replies = vec![lookup(if resend { remote(3) } else { saved.clone() })];
            if resend {
                replies.push((200, json!({"data":{"SaveMediaListEntry":saved}})));
            }
            let (api, mut requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed_competing_title(&state);
            stage(
                &state,
                1,
                Patch {
                    status: Some("PAUSED".into()),
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            let mut events = state.events.subscribe();
            fail_pending_acknowledgement(&state);

            let error = sync_pending_changes(&state).await.unwrap_err();
            assert!(error.contains("fixture acknowledgement failure"), "{error}");
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "PAUSED");
            assert_title_winner(&state, 2);
            assert!(state.db.pending(7).unwrap()[0].needs_cleanup_only());
            let emitted: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
            let updated_pos = emitted
                .iter()
                .position(|event| {
                    matches!(
                        &event.kind,
                        crate::EventKind::EpisodeUpdated(entry)
                            if entry.media_id == 1 && entry.status == "PAUSED" && entry.progress == 4
                    )
                })
                .unwrap_or_else(|| panic!("missing EpisodeUpdated for resend={resend}: {emitted:?}"));
            let pending_pos = emitted
                .iter()
                .position(|event| {
                    matches!(
                        &event.kind,
                        crate::EventKind::PendingChanged(Some(ids)) if ids == &[1]
                    )
                })
                .unwrap_or_else(|| {
                    panic!("missing PendingChanged for resend={resend}: {emitted:?}")
                });
            assert!(
                updated_pos < pending_pos,
                "expected EpisodeUpdated before PendingChanged for resend={resend}: {emitted:?}"
            );

            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_pending_ack")
                .unwrap();
            assert!(sync_pending_changes(&state).await.unwrap().is_empty());
            assert_title_winner(&state, 2);
            assert_eq!(
                std::iter::from_fn(|| requests.try_recv().ok()).count(),
                1 + usize::from(resend)
            );
        }
    }

    #[tokio::test]
    async fn keeping_anilist_absence_rolls_back_when_pending_removal_fails() {
        for missing_media in [false, true] {
            let absent = (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":null}}}),
            );
            let replies = if missing_media {
                vec![]
            } else {
                vec![absent.clone(), absent]
            };
            let (api, _) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state.refresh_matchers();
            assert_eq!(state.matchers.lock()[0].media_id, 1);
            let (_, mut pending, _) = stage(&state, 1, Patch::default()).unwrap().unwrap();
            pending.deleting = !missing_media;
            pending.missing_media = missing_media;
            state.db.put_pending(7, &pending).unwrap();
            fail_pending_acknowledgement(&state);
            let mut events = state.events.subscribe();

            let error = resolve_pending_change(1, false, &state).await.unwrap_err();
            assert!(error.contains("fixture acknowledgement failure"), "{error}");
            assert!(state.db.get_entry(1).unwrap().is_some());
            assert_eq!(state.matchers.lock()[0].media_id, 1);
            assert!(events.try_recv().is_err());
            let retained = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(retained.deleting, pending.deleting);
            assert_eq!(retained.missing_media, pending.missing_media);

            state
                .db
                .0
                .lock()
                .execute_batch("DROP TRIGGER fail_pending_ack")
                .unwrap();
            resolve_pending_change(1, false, &state).await.unwrap();
            assert!(state.db.pending(7).unwrap().is_empty());
            assert!(state.db.get_entry(1).unwrap().is_none());
            assert!(state.matchers.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn foreground_save_ranks_the_acknowledged_or_restored_status() {
        use std::time::Duration;

        for (accepted, fail_ack) in [(false, false), (false, true), (true, true), (true, false)] {
            let mut responses = vec![if accepted {
                let mut saved = remote(3);
                saved["status"] = json!("PAUSED");
                (200, json!({"data":{"SaveMediaListEntry":saved}}))
            } else {
                (
                    400,
                    json!({"errors":[{"message":"Invalid status","status":400}]}),
                )
            }];
            if !accepted && fail_ack {
                responses.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ));
            }
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) = anilist::mock_api_gated(responses, Some(gate.clone())).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed_competing_title(&state);
            if fail_ack {
                fail_pending_acknowledgement(&state);
            }

            let mut events = state.events.subscribe();
            let saving =
                crate::commands::update_entry(1, Some("PAUSED".into()), None, None, None, &state);
            let cached_read = async {
                requests.recv().await.unwrap();
                let entries = crate::commands::local_entries(&state).await.unwrap();
                assert_eq!(
                    entries.iter().find(|e| e.media_id == 1).unwrap().status,
                    "PAUSED"
                );
                assert_title_winner(&state, 2);
                gate.add_permits(1);
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(saving, cached_read)
            })
            .await
            .unwrap();
            if fail_ack {
                let error = result.unwrap_err();
                assert!(error.contains("fixture acknowledgement failure"), "{error}");
                let retained = &state.db.pending(7).unwrap()[0];
                if accepted {
                    assert!(retained.needs_cleanup_only());
                } else {
                    assert_eq!(retained.patch.status.as_deref(), Some("PAUSED"));
                    assert!(retained.conflict);
                }
                let notified = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                    matches!(event.kind, crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1])
                });
                assert!(notified, "accepted={accepted}");
                if !accepted {
                    assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, "PAUSED");
                    assert_title_winner(&state, 2);
                    flush_background(&state).await.unwrap();
                    assert!(requests.try_recv().is_err());
                    state
                        .db
                        .0
                        .lock()
                        .execute_batch("DROP TRIGGER fail_pending_ack")
                        .unwrap();
                    gate.add_permits(1);
                    resolve_pending_change(1, false, &state).await.unwrap();
                    assert!(state.db.pending(7).unwrap().is_empty());
                }
            } else if accepted {
                assert_eq!(result.unwrap().status, "PAUSED");
                assert!(state.db.pending(7).unwrap().is_empty());
            } else {
                let error = result.unwrap_err();
                assert!(error.contains("Invalid status"), "{error}");
                assert!(state.db.pending(7).unwrap().is_empty());
            }
            let status = if accepted { "PAUSED" } else { "CURRENT" };
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().status, status);
            assert_title_winner(&state, if accepted { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn rejected_foreground_edit_preserves_base_and_marks_conflict_when_entry_restore_fails() {
        use std::time::Duration;

        for fail_conflict_save in [false, true] {
            let response = (
                400,
                json!({"errors":[{"message":"Invalid status","status":400}]}),
            );
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let (api, mut requests) =
                anilist::mock_api_gated(vec![response], Some(gate.clone())).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed_competing_title(&state);

            let mut events = state.events.subscribe();
            let saving =
                crate::commands::update_entry(1, Some("PAUSED".into()), None, None, None, &state);
            let cached_read = async {
                requests.recv().await.unwrap();
                let entries = crate::commands::local_entries(&state).await.unwrap();
                assert_eq!(
                    entries.iter().find(|e| e.media_id == 1).unwrap().status,
                    "PAUSED"
                );
                assert_title_winner(&state, 2);
                let mut triggers = String::from(
                    "CREATE TEMP TRIGGER fail_entry_restore BEFORE INSERT ON list_entry
                     BEGIN SELECT RAISE(ABORT, 'fixture entry restore failure'); END;",
                );
                if fail_conflict_save {
                    triggers.push_str(
                        "CREATE TEMP TRIGGER fail_conflict_mark BEFORE INSERT ON pending_change
                         BEGIN SELECT RAISE(ABORT, 'fixture conflict mark failure'); END;",
                    );
                }
                state.db.0.lock().execute_batch(&triggers).unwrap();
                gate.add_permits(1);
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(saving, cached_read)
            })
            .await
            .unwrap();
            let error = result.unwrap_err();
            assert!(
                error.contains("fixture entry restore failure"),
                "fail_conflict_save={fail_conflict_save}: {error}"
            );
            if fail_conflict_save {
                assert!(
                    error.contains("fixture conflict mark failure"),
                    "expected put_pending failure to surface: {error}"
                );
            }
            let pending = state.db.pending(7).unwrap();
            assert_eq!(
                pending.len(),
                1,
                "pending recovery record must never be deleted when list_entry restore fails"
            );
            let retained = &pending[0];
            assert_eq!(
                retained.base.as_ref().map(|entry| entry.status.as_str()),
                Some("CURRENT")
            );
            assert_eq!(retained.patch.status.as_deref(), Some("PAUSED"));
            if !fail_conflict_save {
                assert!(retained.conflict);
                assert!(retained
                    .error
                    .as_deref()
                    .is_some_and(|msg| msg.contains("Invalid status")));
            }
            let emitted: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
            assert!(
                emitted.iter().any(|event| {
                    matches!(
                        &event.kind,
                        crate::EventKind::PendingChanged(Some(ids)) if ids == &[1]
                    )
                }),
                "expected scoped PendingChanged([1]): {emitted:?}"
            );
            assert!(
                !emitted
                    .iter()
                    .any(|event| matches!(event.kind, crate::EventKind::PendingChanged(None))),
                "must not emit clean PendingChanged(None) when restore failed: {emitted:?}"
            );
            if !fail_conflict_save {
                flush_background(&state).await.unwrap();
                assert!(requests.try_recv().is_err());
            }
        }
    }

    fn assert_session_expired(
        state: &AppState,
        events: &mut tokio::sync::broadcast::Receiver<crate::Event>,
    ) {
        assert!(!state.anilist.lock().has_token());
        assert!(state.db.get_setting("anilist_token").unwrap().is_none());
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.matchers.lock().is_empty());
        let expired: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| matches!(event.kind, crate::EventKind::AuthExpired))
            .collect();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].account_id, Some(7));
        assert!(state.event_is_current(&expired[0]));
    }

    fn invalid_token() -> (u16, serde_json::Value) {
        (
            401,
            json!({"errors":[{"message":"Invalid token","status":401}]}),
        )
    }

    #[tokio::test]
    async fn foreground_rejection_expires_session_even_if_rollback_fails() {
        for fault in ["INSERT ON list_entry", "DELETE ON pending_change"] {
            let (api, _requests) = anilist::mock_api(vec![invalid_token()]).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state.refresh_matchers();
            let mut events = state.events.subscribe();
            state
                .db
                .0
                .lock()
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER fail_rollback BEFORE {fault}
                 BEGIN SELECT RAISE(ABORT, 'fixture rollback failure'); END;"
                ))
                .unwrap();

            let error =
                crate::commands::update_entry(1, Some("PAUSED".into()), None, None, None, &state)
                    .await
                    .unwrap_err();
            assert_session_expired(&state, &mut events);
            if fault == "INSERT ON list_entry" {
                assert!(error.contains("expired"), "{error}");
                assert!(state.db.pending(7).unwrap().is_empty());
            } else {
                assert!(error.contains("fixture rollback failure"), "{error}");
                let retained = &state.db.pending(7).unwrap()[0];
                assert_eq!(retained.patch.status.as_deref(), Some("PAUSED"));
                assert!(retained.conflict);
            }
        }
    }

    #[tokio::test]
    async fn replay_rejection_expires_session_even_if_pending_error_cannot_be_saved() {
        for resend in [false, true] {
            let mut replies = Vec::new();
            if resend {
                replies.push((
                    200,
                    json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
                ));
            }
            replies.push(invalid_token());
            let (api, _requests) = anilist::mock_api(replies).await;
            let state = state(api, Db::open(Path::new(":memory:")).unwrap());
            seed(&state);
            state.refresh_matchers();
            let mut events = state.events.subscribe();
            let (_, pending, _) = stage(
                &state,
                1,
                Patch {
                    progress: Some(4),
                    ..Default::default()
                },
            )
            .unwrap()
            .unwrap();
            state
                .db
                .0
                .lock()
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_pending_error BEFORE INSERT ON pending_change
                 BEGIN SELECT RAISE(ABORT, 'fixture pending error failure'); END;",
                )
                .unwrap();

            let error = sync_pending_changes(&state).await.unwrap_err();
            assert!(error.contains("fixture pending error failure"), "{error}");
            assert_session_expired(&state, &mut events);
            assert_eq!(
                serde_json::to_value(&state.db.pending(7).unwrap()[0]).unwrap(),
                serde_json::to_value(pending).unwrap()
            );
        }
    }

    fn fail_pending_acknowledgement(state: &AppState) {
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TRIGGER fail_pending_ack BEFORE DELETE ON pending_change
             BEGIN SELECT RAISE(FAIL, 'fixture acknowledgement failure'); END;",
            )
            .unwrap();
    }

    #[tokio::test]
    async fn local_list_refreshes_matchers_after_partial_overlay_failure() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        state.anilist.lock().set_token(Some("test-session".into()));
        state
            .db
            .upsert_media(&Media {
                id: 3,
                title_english: Some("Unaffected Lighthouse".into()),
                ..Default::default()
            })
            .unwrap();
        state
            .db
            .upsert_entry(&ListEntry {
                media_id: 3,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        state.refresh_matchers();
        let matcher_ids = || {
            state
                .matchers
                .lock()
                .iter()
                .map(|m| m.media_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(matcher_ids(), [1, 3]);
        let (_, mut missing, _) = stage(&state, 1, Patch::default()).unwrap().unwrap();
        missing.missing_media = true;
        state.db.put_pending(7, &missing).unwrap();
        stage(
            &state,
            2,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
        state
            .db
            .0
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER refuse_later_overlay BEFORE INSERT ON list_entry
             WHEN NEW.media_id = 2 BEGIN SELECT RAISE(ABORT, 'synthetic overlay failure'); END;",
            )
            .unwrap();

        let error = crate::commands::local_entries(&state).await.unwrap_err();
        assert!(error.contains("synthetic overlay failure"));
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert_eq!(state.db.pending(7).unwrap().len(), 2);
        assert_eq!(matcher_ids(), [3]);

        state
            .db
            .0
            .lock()
            .execute_batch("DROP TRIGGER refuse_later_overlay")
            .unwrap();
        let entries = crate::commands::local_entries(&state).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| (e.media_id, e.progress) == (2, 4)));
        assert!(entries.iter().any(|e| e.media_id == 3));
        assert_eq!(state.db.pending(7).unwrap().len(), 2);
    }

    #[test]
    fn overlay_skips_unchanged_rows_and_reapplies_each_pending_field() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                status: Some("PAUSED".into()),
                score: Some(0.0),
                repeat: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        overlay(&state).unwrap();
        let patched = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(
            (
                patched.progress,
                patched.status.as_str(),
                patched.score,
                patched.repeat
            ),
            (4, "PAUSED", Some(0.0), 2)
        );
        assert_eq!((patched.id, patched.updated_at), (Some(22), None));
        let changes = state.db.0.lock().total_changes();
        let work = crate::recognize::matcher_work_counts();
        for _ in 0..25 {
            overlay(&state).unwrap();
        }
        assert_eq!(state.db.0.lock().total_changes(), changes);
        assert_eq!(crate::recognize::matcher_work_counts(), work);

        for field in 0..4 {
            let mut snapshot = patched.clone();
            snapshot.updated_at = Some(1234);
            match field {
                0 => snapshot.status = "CURRENT".into(),
                1 => snapshot.progress = 3,
                2 => snapshot.score = None,
                _ => snapshot.repeat = 0,
            }
            state.db.replace_list_snapshot(&[snapshot]).unwrap();
            let changes = state.db.0.lock().total_changes();
            let work = crate::recognize::matcher_work_counts();
            overlay(&state).unwrap();
            assert_eq!(state.db.0.lock().total_changes(), changes + 1);
            if field > 0 {
                assert_eq!(crate::recognize::matcher_work_counts(), work);
            }
            let restored = state.db.get_entry(1).unwrap().unwrap();
            assert_eq!(
                (
                    restored.progress,
                    restored.status.as_str(),
                    restored.score,
                    restored.repeat
                ),
                (4, "PAUSED", Some(0.0), 2)
            );
            assert_eq!((restored.id, restored.updated_at), (Some(22), Some(1234)));
        }
    }

    #[test]
    fn overlay_restores_absent_rows_and_only_deletes_missing_media_once() {
        let mut api = anilist::AniList::new();
        api.set_token(Some("test-session".into()));
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, mut pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        state.db.replace_list_snapshot(&[]).unwrap();
        state.refresh_matchers();
        assert!(state.matchers.lock().is_empty());
        overlay(&state).unwrap();
        let restored = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!((restored.id, restored.progress), (Some(22), 4));
        assert_eq!(state.matchers.lock()[0].media_id, 1);
        pending.missing_media = true;
        state.db.put_pending(7, &pending).unwrap();
        let changes = state.db.0.lock().total_changes();
        overlay(&state).unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.matchers.lock().is_empty());
        assert_eq!(state.db.0.lock().total_changes(), changes + 1);
        let work = crate::recognize::matcher_work_counts();
        overlay(&state).unwrap();
        assert_eq!(state.db.0.lock().total_changes(), changes + 1);
        assert_eq!(crate::recognize::matcher_work_counts(), work);
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
        state.db.replace_list_snapshot(&[restored]).unwrap();
        state.refresh_matchers();
        overlay(&state).unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
        assert!(state.matchers.lock().is_empty());
    }

    #[test]
    fn overlay_restores_new_pending_entries_only_for_the_active_account() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            2,
            Patch {
                progress: Some(1),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert!(pending.base.is_none());
        overlay(&state).unwrap();
        let entry = state.db.get_entry(2).unwrap().unwrap();
        assert_eq!(
            (entry.id, entry.status.as_str(), entry.progress),
            (None, "CURRENT", 1)
        );
        state
            .db
            .replace_account(
                "other-session",
                &User {
                    id: 9,
                    ..Default::default()
                },
            )
            .unwrap();
        let changes = state.db.0.lock().total_changes();
        overlay(&state).unwrap();
        assert!(state.db.entries_with_media().unwrap().is_empty());
        assert_eq!(state.db.0.lock().total_changes(), changes);
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    ..Default::default()
                },
            )
            .unwrap();
        overlay(&state).unwrap();
        assert_eq!(state.db.get_entry(2).unwrap().unwrap().progress, 1);
        state.db.delete_setting("anilist_user_id").unwrap();
        let changes = state.db.0.lock().total_changes();
        overlay(&state).unwrap();
        assert_eq!(state.db.0.lock().total_changes(), changes);
    }

    #[test]
    fn overlay_large_pending_list_has_no_writes_or_matcher_reads_after_application() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
        seed(&state);
        state
            .db
            .durable_write(|conn| {
                let mut insert = conn.prepare("INSERT INTO pending_change VALUES (7,?,?)")?;
                for media_id in 1..=1000 {
                    insert.execute(rusqlite::params![
                        media_id,
                        serde_json::to_string(&Pending {
                            media_id,
                            deleting: false,
                            deletion_id: None,
                            base: None,
                            patch: Patch {
                                progress: Some(4),
                                ..Default::default()
                            },
                            error: None,
                            conflict: false,
                            missing_media: false,
                            score_format: None,
                            playback: false,
                            base_score_format: None,
                            rejected_progress: None,
                            retry_error: None,
                            rejected_total: None,
                        })?
                    ])?;
                }
                Ok(())
            })
            .unwrap();
        let initial = state.db.0.lock().total_changes();
        overlay(&state).unwrap();
        assert_eq!(state.db.0.lock().total_changes(), initial + 1000);
        let work = crate::recognize::matcher_work_counts();
        for _ in 0..10 {
            overlay(&state).unwrap();
        }
        assert_eq!(state.db.0.lock().total_changes(), initial + 1000);
        assert_eq!(crate::recognize::matcher_work_counts(), work);
        assert_eq!(state.db.entries_with_media().unwrap().len(), 1000);
        println!(
            "overlay: 1000 initial writes, 0 writes and 0 matcher rows across 10 unchanged reads"
        );
    }

    #[tokio::test]
    async fn queued_edits_coalesce_without_losing_the_remote_baseline() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"errors":[{"message":"Unavailable"}]}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .unwrap();
        crate::commands::save_entry_inner(&state, 1, Some("PAUSED".into()), Some(5), None, None)
            .await
            .unwrap();
        let pending = state.db.pending(7).unwrap().remove(0);
        assert_eq!(pending.base.unwrap().progress, 3);
        assert_eq!(pending.patch.progress, Some(5));
        assert_eq!(pending.patch.status.as_deref(), Some("PAUSED"));
        state
            .db
            .replace_account(
                "other-token",
                &User {
                    id: 9,
                    name: "Other".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        overlay(&state).unwrap();
        assert!(state.db.entries_with_media().unwrap().is_empty());
        assert!(summaries(&state).unwrap().is_empty());
        assert_eq!(state.db.pending(7).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn automatic_out_of_range_episode_never_sends_or_queues_progress() {
        let (api, mut requests) = anilist::mock_api(vec![]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let error = crate::commands::watcher_set_progress(&state, 1, 13, Some("test-session"))
            .await
            .unwrap_err();
        assert!(error.contains("offset"));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
        assert!(state.db.pending(7).unwrap().is_empty());
        assert!(requests.try_recv().is_err());
    }

    fn single_title_lookup_fixture(count: i64, missing_media: bool) -> (AppState, Pending) {
        let mut api = anilist::AniList::new();
        api.set_token(Some("test-session".into()));
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.refresh_matchers();
        let pending = Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: state.db.get_entry(1).unwrap(),
            patch: Patch {
                status: Some("PAUSED".into()),
                progress: Some(4),
                score: Some(0.0),
                repeat: Some(2),
                ..Default::default()
            },
            error: Some("Saved failure".into()),
            conflict: missing_media,
            missing_media,
            score_format: None,
            playback: false,
            base_score_format: None,
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        };
        state
            .db
            .durable_write(|conn| {
                let mut insert = conn.prepare("INSERT INTO pending_change VALUES (7,?,?)")?;
                for media_id in 1..=count {
                    let mut row = pending.clone();
                    row.media_id = media_id;
                    if media_id != 1 {
                        row.base = None;
                    }
                    insert.execute(rusqlite::params![media_id, serde_json::to_string(&row)?])?;
                }
                conn.execute("INSERT INTO pending_change VALUES (9,1,'invalid')", [])?;
                conn.execute("INSERT INTO pending_change VALUES (9,6000,'invalid')", [])?;
                Ok(())
            })
            .unwrap();
        (state, pending)
    }

    #[test]
    fn indexed_single_title_missing_parses_one_payload_and_retains_intent() {
        for count in [1, 5000] {
            let (state, mut expected) = single_title_lookup_fixture(count, false);
            if count == 5000 {
                state.db.delete_media(1).unwrap();
                assert!(state.db.get_media(1).unwrap().is_none());
            }
            let parses = PENDING_DESERIALIZATIONS.get();
            let commits = crate::db::durable_commit_count();
            let events = ENTRY_NOTIFICATIONS.get();
            mark_media_missing(&state, 1).unwrap();
            assert_eq!(
                PENDING_DESERIALIZATIONS.get() - parses,
                1,
                "queue size {count}"
            );
            assert_eq!(crate::db::durable_commit_count() - commits, 1);
            assert_eq!(ENTRY_NOTIFICATIONS.get() - events, 1);
            assert!(state.db.get_entry(1).unwrap().is_none());
            assert!(state.matchers.lock().is_empty());
            let current = state.db.pending_change(7, 1).unwrap().unwrap();
            assert!(current
                .error
                .as_ref()
                .unwrap()
                .contains("removed or merged"));
            expected.error.clone_from(&current.error);
            expected.conflict = true;
            expected.missing_media = true;
            assert_eq!(
                serde_json::to_value(current).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[test]
    fn indexed_single_title_missing_isolates_payload_errors_and_absent_targets() {
        let (state, pending) = single_title_lookup_fixture(1, false);
        state
            .db
            .0
            .lock()
            .execute("INSERT INTO pending_change VALUES (7,2,'invalid')", [])
            .unwrap();
        for account in [None, Some("7")] {
            match account {
                None => state.db.delete_setting("anilist_user_id").unwrap(),
                Some(account) => state.db.set_setting("anilist_user_id", account).unwrap(),
            }
            let writes = state.db.0.lock().total_changes();
            let parses = PENDING_DESERIALIZATIONS.get();
            let events = ENTRY_NOTIFICATIONS.get();
            mark_media_missing(&state, if account.is_none() { 1 } else { 6000 }).unwrap();
            assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, 0);
            assert_eq!(state.db.0.lock().total_changes(), writes);
            assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
            assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
            assert_eq!(state.matchers.lock()[0].media_id, 1);
        }
        state
            .db
            .0
            .lock()
            .execute(
                "UPDATE pending_change SET payload='invalid' WHERE account_id=7 AND media_id=1",
                [],
            )
            .unwrap();
        let expected_error = state.db.pending_change(7, 1).unwrap_err().to_string();
        let parses = PENDING_DESERIALIZATIONS.get();
        let writes = state.db.0.lock().total_changes();
        let events = ENTRY_NOTIFICATIONS.get();
        assert_eq!(
            mark_media_missing(&state, 1).unwrap_err().to_string(),
            expected_error
        );
        assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, 1);
        assert_eq!(state.db.0.lock().total_changes(), writes);
        assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
        assert!(state.db.get_entry(1).unwrap().is_some());
        state.db.put_pending(7, &pending).unwrap();
        let parses = PENDING_DESERIALIZATIONS.get();
        mark_media_missing(&state, 1).unwrap();
        assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, 1);
        assert!(
            state
                .db
                .pending_change(7, 1)
                .unwrap()
                .unwrap()
                .missing_media
        );
        assert!(state.db.pending_change(7, 2).is_err());
    }

    #[tokio::test]
    async fn indexed_single_title_resolution_parses_one_payload_for_both_missing_media_choices() {
        for count in [1, 5000] {
            for keep_local in [true, false] {
                let (state, pending) = single_title_lookup_fixture(count, true);
                let parses = PENDING_DESERIALIZATIONS.get();
                let commits = crate::db::durable_commit_count();
                let events = ENTRY_NOTIFICATIONS.get();
                let result = resolve_pending_inner(1, keep_local, &state).await;
                assert_eq!(
                    PENDING_DESERIALIZATIONS.get() - parses,
                    1,
                    "queue size {count}"
                );
                if keep_local {
                    assert_eq!(
                        result.unwrap_err(),
                        "AniList no longer has this anime. Its saved change can be discarded."
                    );
                    assert_eq!(crate::db::durable_commit_count(), commits);
                    assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
                    assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 3);
                    assert_eq!(state.matchers.lock()[0].media_id, 1);
                    assert_eq!(
                        serde_json::to_value(state.db.pending_change(7, 1).unwrap()).unwrap(),
                        serde_json::to_value(pending).unwrap()
                    );
                } else {
                    result.unwrap();
                    assert_eq!(crate::db::durable_commit_count() - commits, 1);
                    assert_eq!(ENTRY_NOTIFICATIONS.get() - events, 1);
                    assert!(state.db.get_entry(1).unwrap().is_none());
                    assert!(state.db.pending_change(7, 1).unwrap().is_none());
                    assert!(state.matchers.lock().is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn queued_sync_actions_reject_a_new_auth_intent_with_the_same_token() {
        for operation in ["sync", "keep_local", "keep_remote"] {
            for lock in ["remote", "entry"] {
                for cancel in [false, true] {
                    let (api, mut requests) = anilist::mock_api(batch_replies(1)).await;
                    let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                    seed(&state);
                    let (_, pending, _) = stage(
                        &state,
                        1,
                        Patch {
                            progress: Some(4),
                            ..Default::default()
                        },
                    )
                    .unwrap()
                    .unwrap();
                    local_entry(&state, &pending).unwrap();
                    let saved_pending = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
                    let saved_entry = serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap();
                    let origin = state.origin();
                    let guard = if lock == "remote" {
                        state.remote_lock.lock().await
                    } else {
                        state.entry_lock.lock().await
                    };
                    let mut action = Box::pin(async {
                        match operation {
                            "sync" => sync_pending_changes(&state).await.map(|_| ()),
                            "keep_local" => resolve_pending_change(1, true, &state).await,
                            _ => resolve_pending_change(1, false, &state).await,
                        }
                    });
                    enqueue_waiter(action.as_mut()).await;
                    if lock == "entry" {
                        assert!(state.remote_lock.try_lock().is_err());
                    }
                    if cancel {
                        state.cancel_login();
                        assert!(!state.origin_is_current(origin));
                    }
                    drop(guard);
                    let result = tokio::time::timeout(std::time::Duration::from_secs(5), action)
                        .await
                        .unwrap();
                    assert_eq!(
                        state.anilist.lock().token().as_deref(),
                        Some("test-session")
                    );
                    assert!(!requests.is_closed());
                    if cancel {
                        let sent =
                            std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
                        assert!(
                            sent.is_empty(),
                            "{operation} after {lock}: {result:?}, sent {sent:?}, pending {:?}",
                            state.db.pending(7).unwrap()
                        );
                        assert_eq!(result.unwrap_err(), "Account changed");
                        assert_eq!(
                            serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                            saved_pending
                        );
                        assert_eq!(
                            serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap(),
                            saved_entry
                        );
                    } else {
                        result.unwrap();
                        assert!(state.db.pending(7).unwrap().is_empty());
                        assert_eq!(
                            state.db.get_entry(1).unwrap().unwrap().progress,
                            if operation == "keep_remote" { 3 } else { 4 }
                        );
                        assert_eq!(
                            requests.try_recv().unwrap()["variables"],
                            json!({"mediaId":1,"scoreFormat":"POINT_100"})
                        );
                        if operation != "keep_remote" {
                            let sent = requests.try_recv().unwrap();
                            assert!(sent["query"]
                                .as_str()
                                .unwrap()
                                .contains("SaveMediaListEntry"));
                            assert_eq!(
                                sent["variables"],
                                json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
                            );
                        }
                        assert!(matches!(
                            requests.try_recv(),
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                        ));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn removal_rechecks_auth_intent_after_the_write_lock_handoff() {
        for operation in ["remove", "retry", "sync", "background"] {
            for cancel in [false, true] {
                let (api, mut requests) = anilist::mock_api(vec![(
                    200,
                    json!({"data":{"DeleteMediaListEntry":{"deleted":true}}}),
                )])
                .await;
                let state = state(api, Db::open(Path::new(":memory:")).unwrap());
                seed(&state);
                let (_, mut pending, _) = stage(
                    &state,
                    1,
                    Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                )
                .unwrap()
                .unwrap();
                pending.deleting = operation != "remove";
                pending.deletion_id = pending.deleting.then_some(22);
                pending.conflict = operation == "retry";
                state.db.put_pending(7, &pending).unwrap();
                let saved_pending = serde_json::to_value(state.db.pending(7).unwrap()).unwrap();
                let saved_entry = serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap();
                let guard = state.entry_lock.lock().await;
                let mut action = Box::pin(async {
                    match operation {
                        "remove" => crate::commands::delete_entry_cmd(1, &state).await,
                        "retry" => resolve_pending_change(1, true, &state).await,
                        "sync" => sync_pending_changes(&state).await.map(|_| ()),
                        _ => flush_background(&state).await,
                    }
                });
                enqueue_waiter(action.as_mut()).await;
                let mut reader = Box::pin(state.entry_lock.lock());
                enqueue_waiter(reader.as_mut()).await;
                drop(guard);
                enqueue_waiter(action.as_mut()).await;
                let reader = reader.await;
                if cancel {
                    state.cancel_login();
                }
                drop(reader);
                let result = tokio::time::timeout(std::time::Duration::from_secs(5), action)
                    .await
                    .unwrap();
                if cancel {
                    assert!(
                        requests.try_recv().is_err(),
                        "{operation} sent a removal after its auth intent was cancelled"
                    );
                    if operation == "background" {
                        result.unwrap();
                    } else {
                        assert_eq!(result.unwrap_err(), "Account changed");
                    }
                    assert_eq!(
                        serde_json::to_value(state.db.pending(7).unwrap()).unwrap(),
                        saved_pending
                    );
                    assert_eq!(
                        serde_json::to_value(state.db.get_entry(1).unwrap()).unwrap(),
                        saved_entry
                    );
                } else {
                    result.unwrap();
                    assert_eq!(requests.try_recv().unwrap()["variables"]["id"], 22);
                    assert!(requests.try_recv().is_err());
                    assert!(state.db.pending(7).unwrap().is_empty());
                    assert!(state.db.get_entry(1).unwrap().is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn indexed_single_title_resolution_isolates_payload_errors_and_preserves_guards() {
        for keep_local in [true, false] {
            let (state, pending) = single_title_lookup_fixture(1, true);
            state
                .db
                .0
                .lock()
                .execute("INSERT INTO pending_change VALUES (7,2,'invalid')", [])
                .unwrap();
            state.anilist.lock().set_token(None);
            let parses = PENDING_DESERIALIZATIONS.get();
            assert_eq!(
                resolve_pending_inner(1, keep_local, &state)
                    .await
                    .unwrap_err(),
                "Account changed"
            );
            assert_eq!(PENDING_DESERIALIZATIONS.get(), parses);
            state.anilist.lock().set_token(Some("test-session".into()));
            let remote_guard = state.remote_lock.lock().await;
            let mut resolution = Box::pin(resolve_pending_inner(1, keep_local, &state));
            enqueue_waiter(resolution.as_mut()).await;
            state
                .anilist
                .lock()
                .set_token(Some("replacement-session".into()));
            drop(remote_guard);
            assert_eq!(resolution.await.unwrap_err(), "Account changed");
            assert_eq!(PENDING_DESERIALIZATIONS.get(), parses);
            state.anilist.lock().set_token(Some("test-session".into()));
            state.db.delete_setting("anilist_user_id").unwrap();
            assert_eq!(
                resolve_pending_inner(1, keep_local, &state)
                    .await
                    .unwrap_err(),
                "Not signed in"
            );
            assert_eq!(PENDING_DESERIALIZATIONS.get(), parses);
            state.db.set_setting("anilist_user_id", "7").unwrap();
            let writes = state.db.0.lock().total_changes();
            let events = ENTRY_NOTIFICATIONS.get();
            resolve_pending_inner(6000, keep_local, &state)
                .await
                .unwrap();
            assert_eq!(PENDING_DESERIALIZATIONS.get(), parses);
            assert_eq!(state.db.0.lock().total_changes(), writes);
            assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
            let entry_guard = state.entry_lock.lock().await;
            let mut resolution = Box::pin(resolve_pending_inner(1, keep_local, &state));
            enqueue_waiter(resolution.as_mut()).await;
            assert_eq!(PENDING_DESERIALIZATIONS.get(), parses);
            assert!(state.remote_lock.try_lock().is_err());
            state
                .db
                .0
                .lock()
                .execute(
                    "UPDATE pending_change SET payload='invalid' WHERE account_id=7 AND media_id=1",
                    [],
                )
                .unwrap();
            let expected_error = state.db.pending_change(7, 1).unwrap_err().to_string();
            let parses = PENDING_DESERIALIZATIONS.get();
            let writes = state.db.0.lock().total_changes();
            drop(entry_guard);
            assert_eq!(resolution.await.unwrap_err(), expected_error);
            assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, 1);
            assert_eq!(state.db.0.lock().total_changes(), writes);
            assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
            assert!(state.db.get_entry(1).unwrap().is_some());
            state.db.put_pending(7, &pending).unwrap();
            let parses = PENDING_DESERIALIZATIONS.get();
            let result = resolve_pending_inner(1, keep_local, &state).await;
            assert_eq!(PENDING_DESERIALIZATIONS.get() - parses, 1);
            if keep_local {
                assert!(result.unwrap_err().contains("no longer has this anime"));
                assert_eq!(
                    serde_json::to_value(state.db.pending_change(7, 1).unwrap()).unwrap(),
                    serde_json::to_value(pending).unwrap()
                );
            } else {
                result.unwrap();
                assert!(state.db.pending_change(7, 1).unwrap().is_none());
            }
            assert!(state.db.pending_change(7, 2).is_err());
        }
    }

    #[tokio::test]
    async fn removed_media_keeps_pending_evidence_until_it_can_be_discarded() {
        let missing = json!({"errors":[{"message":"Not Found"}]});
        let (api, _) = anilist::mock_api(vec![(404, missing)]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(flush_unlocked(&state).await.is_err());
        assert!(state.db.pending(7).unwrap()[0].missing_media);
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
        overlay(&state).unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
        resolve_pending_inner(1, false, &state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        overlay(&state).unwrap();
        assert!(state.db.get_entry(1).unwrap().is_none());
    }

    #[tokio::test]
    async fn validation_errors_do_not_become_offline_saves() {
        let (api, _) =
            anilist::mock_api(vec![(400, json!({"errors":[{"message":"Invalid score"}]}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        assert!(
            crate::commands::save_entry_inner(&state, 1, None, None, Some(999.0), None)
                .await
                .is_err()
        );
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, Some(0.0));
    }

    #[tokio::test]
    async fn json_service_errors_keep_the_offline_progress_change() {
        let (api, _) =
            anilist::mock_api(vec![(503, json!({"message": "Service unavailable"}))]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let saved = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .expect("a service outage should queue progress")
            .unwrap();
        assert_eq!(saved.progress, 4);
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
    }

    fn maintenance_error() -> serde_json::Value {
        json!({"errors":[{"status":403,"message":"The AniList API has been temporarily disabled due to severe stability issues. Please check the official AniList Discord for more information."}],"data":null})
    }

    #[tokio::test]
    async fn temporary_maintenance_queues_new_progress() {
        let (api, _) = anilist::mock_api(vec![(403, maintenance_error())]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let saved = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .expect("temporary maintenance should preserve offline progress")
            .unwrap();
        assert_eq!(saved.progress, 4);
        assert_eq!(state.db.pending(7).unwrap()[0].patch.progress, Some(4));
        assert!(state.anilist.lock().has_token());
    }

    #[tokio::test]
    async fn temporary_maintenance_retries_pending_changes_after_recovery() {
        let (api, mut requests) = anilist::mock_api(vec![
            (403, maintenance_error()),
            (
                200,
                json!({"data":{"Media":{"id":1,"mediaListEntry":remote(3)}}}),
            ),
            (200, json!({"data":{"SaveMediaListEntry":remote(4)}})),
        ])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();
        assert!(flush_unlocked(&state).await.is_err());
        assert!(
            !state.db.pending(7).unwrap()[0].conflict,
            "maintenance is not an edit conflict"
        );
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        requests.recv().await.unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":4,"scoreFormat":"POINT_100"})
        );
    }

    #[tokio::test]
    async fn graphql_retry_status_inside_http_success_preserves_progress() {
        let (api, _) = anilist::mock_api(vec![(
            200,
            json!({"errors":[{"status":429,"message":"Too Many Requests."}],"data":null}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let saved = crate::commands::watcher_set_progress(&state, 1, 4, Some("test-session"))
            .await
            .expect("GraphQL rate limiting should preserve offline progress")
            .unwrap();
        assert_eq!(saved.progress, 4);
        assert!(!state.db.pending(7).unwrap()[0].conflict);
    }

    #[test]
    fn history_paging_preserves_gaps_limits_accounts_and_title_fallbacks() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.0.lock()
            .execute_batch(
                "INSERT INTO media (id,title_english,title_romaji,title_native,cached_at) VALUES
                (1,'English','Romaji','Native',1),
                (2,NULL,'Romaji','Native',1),
                (3,NULL,NULL,'Native',1),
                (4,NULL,NULL,NULL,1),
                (6,' ','Romaji','Native',1);
             INSERT INTO watch_history (id,account_id,path,media_id,episode,watched_at) VALUES
                (2,7,'one',1,1,400), (5,9,'other',2,1,400),
                (8,7,'two',2,1,300), (12,7,'three',3,1,200),
                (20,7,'four',4,1,100), (23,9,'other',1,2,400),
                (30,7,'five',5,1,200), (45,7,'six',6,1,200);",
            )
            .unwrap();
        for account in [7, 9, 99] {
            for before in [
                None,
                Some(46),
                Some(30),
                Some(29),
                Some(9),
                Some(8),
                Some(0),
                Some(-1),
                Some(i64::MIN),
            ] {
                for limit in [-1, 0, 1, 2, 50] {
                    let actual = db.watch_history(account, limit, before).unwrap();
                    let expected = measure_history_query(
                        &db.0.lock(),
                        LEGACY_HISTORY_PAGE,
                        &[&account, &before, &before, &limit],
                    )
                    .0;
                    assert_eq!(
                        serde_json::to_value(actual).unwrap(),
                        serde_json::to_value(expected).unwrap(),
                        "account {account}, before {before:?}, limit {limit}"
                    );
                }
            }
        }
        let all = db.watch_history(7, -1, None).unwrap();
        assert_eq!(
            all.iter().map(|row| row.id).collect::<Vec<_>>(),
            [45, 30, 20, 12, 8, 2]
        );
        assert_eq!(
            all.iter().map(|row| row.title.as_str()).collect::<Vec<_>>(),
            ["Romaji", "#5", "#4", "Native", "Romaji", "English"]
        );
        let mut ids = Vec::new();
        let mut before = None;
        loop {
            let page = db.watch_history(7, 2, before).unwrap();
            if page.is_empty() {
                break;
            }
            before = page.last().map(|row| row.id);
            ids.extend(page.iter().map(|row| row.id));
        }
        assert_eq!(ids, [45, 30, 20, 12, 8, 2]);
    }

    const LEGACY_HISTORY_PAGE: &str = "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
         m.title_english,m.title_romaji,m.title_native
         FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
         WHERE h.account_id = ? AND (? IS NULL OR h.id < ?) ORDER BY h.id DESC LIMIT ?";

    fn measure_history_query(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> (Vec<WatchHistoryItem>, i32, i32) {
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt
            .query_map(params, |r| {
                Ok(WatchHistoryItem {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    media_id: r.get(2)?,
                    episode: r.get(3)?,
                    watched_at: r.get(4)?,
                    title: row_title(r, 5, r.get(2)?)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (
            rows,
            stmt.get_status(rusqlite::StatementStatus::VmStep),
            stmt.get_status(rusqlite::StatementStatus::Sort),
        )
    }

    #[test]
    fn history_paging_seeks_without_sorting_fifty_thousand_rows() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.0.lock()
            .execute_batch(
                "INSERT INTO media (id,title_english,title_romaji,title_native,cached_at) VALUES
                (1,'English','Romaji','Native',1), (2,NULL,'Romaji','Native',1),
                (3,NULL,NULL,'Native',1), (4,NULL,NULL,NULL,1);
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50000)
             INSERT INTO watch_history (id,account_id,path,media_id,episode,watched_at)
             SELECT i*2,i%10,'episode-' || i || '.mkv',i%7+1,1,1000+i%100 FROM n;",
            )
            .unwrap();
        let account = 7;
        let limit = 50;
        for before in [None, Some(25000)] {
            let actual =
                serde_json::to_value(db.watch_history(account, limit, before).unwrap()).unwrap();
            let conn = db.0.lock();
            let (sql, params): (&str, Vec<&dyn rusqlite::ToSql>) = match &before {
                Some(cursor) => (HISTORY_BEFORE_PAGE, vec![&account, cursor, &limit]),
                None => (HISTORY_FIRST_PAGE, vec![&account, &limit]),
            };
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params.as_slice(), |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter().any(|line| line
                    .contains("SEARCH h USING INDEX watch_history_account_id")
                    && (before.is_none() || line.contains("id<?"))),
                "{plan:?}"
            );
            assert!(
                !plan.iter().any(|line| line.contains("TEMP B-TREE")),
                "{plan:?}"
            );
            let indexed = measure_history_query(&conn, sql, params.as_slice());
            let residual = measure_history_query(
                &conn,
                LEGACY_HISTORY_PAGE,
                &[&account, &before, &before, &limit],
            );
            assert_eq!(serde_json::to_value(&indexed.0).unwrap(), actual);
            assert_eq!(serde_json::to_value(&residual.0).unwrap(), actual);
            assert_eq!(indexed.2, 0);
            if before.is_some() {
                assert!(indexed.1 < residual.1 / 10);
            }
            conn.execute_batch("DROP INDEX watch_history_account_id")
                .unwrap();
            let legacy = measure_history_query(
                &conn,
                LEGACY_HISTORY_PAGE,
                &[&account, &before, &before, &limit],
            );
            assert_eq!(serde_json::to_value(&legacy.0).unwrap(), actual);
            assert!(legacy.2 > 0);
            assert!(indexed.1 < legacy.1 / 10);
            println!("history before {before:?} over 50000 rows: paging {} VM steps, {} sorts; residual cursor {} steps; previous index {} steps, {} sorts; {plan:?}", indexed.1, indexed.2, residual.1, legacy.1, legacy.2);
            conn.execute_batch(
                "CREATE INDEX watch_history_account_id ON watch_history (account_id,id)",
            )
            .unwrap();
        }
    }

    #[test]
    fn history_is_account_scoped_and_uses_a_stable_cursor() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.record_watch(7, "episode1.mkv", 1, 1).unwrap();
        db.record_watch(7, "episode1.mkv", 1, 1).unwrap();
        db.record_watch(9, "episode2.mkv", 1, 2).unwrap();
        db.record_watch(7, "episode2.mkv", 1, 2).unwrap();
        let newest = db.watch_history(7, 1, None).unwrap();
        assert_eq!(newest[0].episode, 2);
        let older = db.watch_history(7, 50, Some(newest[0].id)).unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].episode, 1);
        assert_eq!(db.watch_history(9, 50, None).unwrap().len(), 1);
    }

    #[test]
    fn history_duplicates_require_the_full_identity_and_expire_after_five_minutes() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        for (account, path, media, episode) in [
            (7, "episode.mkv", 1, 1),
            (7, "episode.mkv", 1, 1),
            (9, "episode.mkv", 1, 1),
            (7, "renamed.mkv", 1, 1),
            (7, "episode.mkv", 2, 1),
            (7, "episode.mkv", 1, 2),
        ] {
            db.record_watch(account, path, media, episode).unwrap();
        }
        assert_eq!(db.watch_history(7, 50, None).unwrap().len(), 4);
        assert_eq!(db.watch_history(9, 50, None).unwrap().len(), 1);
        db.0.lock().execute(
            "UPDATE watch_history SET watched_at=? WHERE account_id=7 AND path='episode.mkv' AND media_id=1 AND episode=1",
            [chrono::Utc::now().timestamp() - 300],
        ).unwrap();
        db.record_watch(7, "episode.mkv", 1, 1).unwrap();
        db.record_watch(7, "episode.mkv", 1, 1).unwrap();
        assert_eq!(db.watch_history(7, 50, None).unwrap().len(), 5);
        assert_eq!(db.watch_history(9, 50, None).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inactive_account_pending_titles_survive_cache_pruning() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_entry(1).unwrap();
        let (_, pending, _) = stage(
            &state,
            1,
            Patch {
                status: Some("CURRENT".into()),
                progress: Some(4),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        local_entry(&state, &pending).unwrap();
        state
            .db
            .replace_account(
                "other-token",
                &User {
                    id: 9,
                    name: "Other".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .0
            .lock()
            .execute("UPDATE media SET cached_at = 1", [])
            .unwrap();
        assert_eq!(state.db.prune_media_cache(30).unwrap(), 0);
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    name: "Tester".into(),
                    score_format: Some("POINT_100".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        overlay(&state).unwrap();
        let entry = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(entry.progress, 4);
        assert_eq!(entry.media.unwrap().display_title(), "Test show");
        assert_eq!(state.matchers.lock().len(), 1);
    }

    #[test]
    fn frontend_contracts_include_sync_and_history_fields() {
        crate::models::assert_ts_declares(
            "PendingChange",
            &serde_json::to_value(PendingChange {
                media_id: 1,
                title: "Test".into(),
                progress: Some(1),
                status: None,
                score: None,
                repeat: None,
                error: None,
                conflict: false,
                missing_media: false,
            })
            .unwrap(),
        );
        crate::models::assert_ts_declares(
            "WatchHistoryItem",
            &serde_json::to_value(WatchHistoryItem {
                id: 1,
                path: "episode.mkv".into(),
                media_id: 1,
                episode: 1,
                watched_at: 1,
                title: "Test".into(),
            })
            .unwrap(),
        );
        crate::models::assert_ts_declares(
            "LibraryBinding",
            &serde_json::to_value(crate::library::LibraryBinding::default()).unwrap(),
        );
    }

    #[test]
    fn rewatch_and_remote_deletion_require_resolution() {
        let base = ListEntry {
            id: Some(22),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 3,
            ..Default::default()
        };
        let pending = Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: Some(base.clone()),
            patch: Patch {
                progress: Some(4),
                ..Default::default()
            },
            error: None,
            conflict: false,
            missing_media: false,
            score_format: None,
            playback: false,
            base_score_format: None,
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        };
        assert!(conflict(&pending, None));
        let mut remote = base;
        remote.repeat = 1;
        assert!(conflict(&pending, Some(&remote)));
    }
    #[tokio::test]
    async fn restored_progress_does_not_restore_an_old_base_score() {
        let (api, _) = anilist::mock_api(vec![]).await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        let mut entry = state.db.get_entry(1).unwrap().unwrap();
        entry.score = Some(80.0);
        state.db.upsert_entry(&entry).unwrap();
        stage(
            &state,
            1,
            Patch {
                progress: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        state
            .db
            .replace_account(
                "other",
                &User {
                    id: 8,
                    ..Default::default()
                },
            )
            .unwrap();
        state
            .db
            .replace_account(
                "test-session",
                &User {
                    id: 7,
                    score_format: Some("POINT_5".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        overlay(&state).unwrap();
        let restored = state.db.get_entry(1).unwrap().unwrap();
        assert_eq!(restored.progress, 5);
        assert_eq!(restored.score, None);
    }

    #[test]
    fn score_clear_compares_baseline_in_current_query_format() {
        let base_five = ListEntry {
            id: Some(10),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 3,
            score: Some(4.0),
            ..Default::default()
        };
        let clear_five = Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: Some(base_five.clone()),
            playback: false,
            base_score_format: Some("POINT_5".into()),
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
            patch: Patch {
                score: Some(0.0),
                ..Default::default()
            },
            score_format: Some("POINT_5".into()),
            conflict: false,
            missing_media: false,
            error: None,
        };
        let remote_rerated = ListEntry {
            score: Some(4.0),
            ..base_five
        };
        assert!(conflict_in_format(
            &clear_five,
            Some(&remote_rerated),
            Some("POINT_10")
        ));

        let base_hundred = ListEntry {
            id: Some(10),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 3,
            score: Some(80.0),
            ..Default::default()
        };
        let clear_hundred = Pending {
            media_id: 1,
            deleting: false,
            deletion_id: None,
            base: Some(base_hundred.clone()),
            playback: false,
            base_score_format: Some("POINT_100".into()),
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
            patch: Patch {
                score: Some(0.0),
                ..Default::default()
            },
            score_format: Some("POINT_100".into()),
            conflict: false,
            missing_media: false,
            error: None,
        };
        let remote_decimal = ListEntry {
            score: Some(8.0),
            ..base_hundred
        };
        assert!(!conflict_in_format(
            &clear_hundred,
            Some(&remote_decimal),
            Some("POINT_10_DECIMAL")
        ));
    }

    #[tokio::test]
    async fn missing_media_and_keep_empty_remote_clear_retained_tracking_prompt_before_emit() {
        let missing = (404, json!({"errors":[{"message":"Not Found"}]}));
        let (api, _) = anilist::mock_api(vec![missing]).await;
        let app = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&app);
        app.db
            .put_pending(
                7,
                &Pending {
                    media_id: 1,
                    deleting: false,
                    deletion_id: None,
                    base: app.db.get_entry(1).unwrap(),
                    playback: false,
                    base_score_format: Some("POINT_100".into()),
                    rejected_progress: None,
                    retry_error: None,
                    rejected_total: None,
                    patch: Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                    score_format: Some("POINT_100".into()),
                    conflict: false,
                    missing_media: false,
                    error: None,
                },
            )
            .unwrap();
        app.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 5,
            title: "Test show".into(),
            raw_title: "Test show - 05".into(),
            progress: 4,
        }));
        assert!(app.retained_tracking_prompt().is_some());

        let mut events = app.events.subscribe();
        let watcher_state = app.clone();
        let watcher = tokio::spawn(async move {
            loop {
                if let Ok(event) = events.recv().await {
                    if matches!(
                        event.kind,
                        crate::EventKind::PendingChanged(Some(ref ids)) if ids == &[1]
                    ) {
                        return watcher_state.retained_tracking_prompt().is_none();
                    }
                }
            }
        });
        tokio::task::yield_now().await;

        assert!(flush_from(&app, app.origin()).await.is_err());
        let pending = app.db.pending(7).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].missing_media);
        assert!(watcher.await.unwrap());
        assert!(app.retained_tracking_prompt().is_none());

        let (api, _) = anilist::mock_api(vec![]).await;
        let keep_app = Arc::new(state(api, Db::open(Path::new(":memory:")).unwrap()));
        seed(&keep_app);
        keep_app
            .db
            .put_pending(
                7,
                &Pending {
                    media_id: 1,
                    deleting: false,
                    deletion_id: None,
                    base: keep_app.db.get_entry(1).unwrap(),
                    playback: false,
                    base_score_format: Some("POINT_100".into()),
                    rejected_progress: None,
                    retry_error: None,
                    rejected_total: None,
                    patch: Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                    score_format: Some("POINT_100".into()),
                    conflict: true,
                    missing_media: false,
                    error: None,
                },
            )
            .unwrap();
        keep_app.emit(crate::EventKind::TrackingPrompt(crate::TrackingPrompt {
            media_id: 1,
            episode: 5,
            title: "Test show".into(),
            raw_title: "Test show - 05".into(),
            progress: 4,
        }));
        assert!(keep_app.retained_tracking_prompt().is_some());

        let origin = keep_app.origin();
        let token = keep_app.anilist.lock().token();
        let _remote = keep_app.remote_lock.lock().await;
        let _entry = keep_app.entry_lock.lock().await;
        keep_anilist_entry(&keep_app, origin, &token, 7, 1, None, None).unwrap();
        assert!(keep_app.retained_tracking_prompt().is_none());
    }
}
