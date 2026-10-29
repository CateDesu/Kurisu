use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

use crate::{anilist, commands::AppState, db::Db, models::ListEntry};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Patch {
    pub status: Option<String>,
    pub progress: Option<i64>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
}

impl Patch {
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
    pub base: Option<ListEntry>,
    pub patch: Patch,
    pub error: Option<String>,
    pub conflict: bool,
    #[serde(default)]
    pub missing_media: bool,
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

const HISTORY_FIRST_PAGE: &str = "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
     COALESCE(m.title_english,m.title_romaji,m.title_native,'#' || h.media_id)
     FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
     WHERE h.account_id = ? ORDER BY h.id DESC LIMIT ?";

const HISTORY_BEFORE_PAGE: &str = "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
     COALESCE(m.title_english,m.title_romaji,m.title_native,'#' || h.media_id)
     FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
     WHERE h.account_id = ? AND h.id < ? ORDER BY h.id DESC LIMIT ?";

impl Db {
    pub(crate) fn pending(&self, account: i64) -> Result<Vec<Pending>> {
        let conn = self.0.lock();
        let mut stmt = conn
            .prepare("SELECT payload FROM pending_change WHERE account_id = ? ORDER BY media_id")?;
        let rows = stmt.query_map([account], |r| r.get::<_, String>(0))?;
        rows.map(|r| parse_pending(&r?)).collect()
    }

    fn pending_background_ids(&self, account: i64) -> Result<Vec<i64>> {
        let conn = self.0.lock();
        let mut stmt = conn
            .prepare("SELECT payload FROM pending_change WHERE account_id = ? ORDER BY media_id")?;
        let mut rows = stmt.query([account])?;
        let mut ids = Vec::with_capacity(10);
        while let Some(row) = rows.next()? {
            let pending = parse_pending(&row.get::<_, String>(0)?)?;
            if !pending.conflict {
                ids.push(pending.media_id);
                if ids.len() == 10 {
                    break;
                }
            }
        }
        Ok(ids)
    }

    pub(crate) fn pending_change(&self, account: i64, media_id: i64) -> Result<Option<Pending>> {
        let payload = self
            .0
            .lock()
            .query_row(
                "SELECT payload FROM pending_change WHERE account_id = ? AND media_id = ?",
                [account, media_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload.map(|payload| parse_pending(&payload)).transpose()
    }

    pub(crate) fn put_pending(&self, account: i64, pending: &Pending) -> Result<()> {
        self.durable_write(|conn| {
            conn.execute(
            "INSERT OR REPLACE INTO pending_change (account_id,media_id,payload) VALUES (?,?,?)",
            rusqlite::params![account, pending.media_id, serde_json::to_string(pending)?],
        )?;
            Ok(())
        })
    }

    pub(crate) fn remove_pending(&self, account: i64, media_id: i64) -> Result<()> {
        self.durable_write(|conn| {
            conn.execute(
                "DELETE FROM pending_change WHERE account_id = ? AND media_id = ?",
                [account, media_id],
            )?;
            Ok(())
        })
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
                title: r.get(5)?,
            })
        };
        let rows = match before {
            Some(before) => stmt.query_map(rusqlite::params![account, before, limit], map_row)?,
            None => stmt.query_map(rusqlite::params![account, limit], map_row)?,
        };
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

pub(crate) fn stage(
    state: &AppState,
    media_id: i64,
    patch: Patch,
) -> Result<Option<(i64, Pending, bool)>> {
    let Some(account) = account_id(&state.db) else {
        return Ok(None);
    };
    let existing = state.db.pending_change(account, media_id)?;
    let already_pending = existing.is_some();
    let mut pending = existing.unwrap_or(Pending {
        media_id,
        base: state.db.get_entry(media_id)?,
        patch: Patch::default(),
        error: None,
        conflict: false,
        missing_media: false,
    });
    if pending.missing_media {
        return Err(anyhow::anyhow!(
            "This anime was removed from AniList. Resolve its saved change before continuing."
        ));
    }
    pending.patch.merge(&patch);
    state.db.put_pending(account, &pending)?;
    Ok(Some((account, pending, already_pending)))
}

pub(crate) fn local_entry(state: &AppState, pending: &Pending) -> Result<ListEntry> {
    let mut entry = state
        .db
        .get_entry(pending.media_id)?
        .or_else(|| pending.base.clone())
        .unwrap_or(ListEntry {
            media_id: pending.media_id,
            status: "CURRENT".into(),
            ..Default::default()
        });
    pending.patch.apply(&mut entry);
    entry.media = state.db.get_media(pending.media_id)?;
    state.db.upsert_entry(&entry)?;
    state.refresh_matchers();
    entry_changed(pending.media_id);
    Ok(entry)
}

pub(crate) fn overlay(state: &AppState) -> Result<()> {
    if let Some(account) = account_id(&state.db) {
        let pending = state.db.pending(account)?;
        let mut matchers_changed = false;
        for pending in pending {
            let stored = state.db.get_entry(pending.media_id)?;
            if pending.missing_media {
                if stored.is_some() {
                    state.db.delete_entry(pending.media_id)?;
                    matchers_changed = true;
                }
                continue;
            }
            let mut entry = stored
                .clone()
                .or_else(|| pending.base.clone())
                .unwrap_or(ListEntry {
                    media_id: pending.media_id,
                    status: "CURRENT".into(),
                    ..Default::default()
                });
            pending.patch.apply(&mut entry);
            if stored.as_ref().is_some_and(|stored| {
                stored.status == entry.status
                    && stored.progress == entry.progress
                    && stored.score == entry.score
                    && stored.repeat == entry.repeat
            }) {
                continue;
            }
            state.db.upsert_entry(&entry)?;
            matchers_changed |= stored
                .as_ref()
                .is_none_or(|stored| stored.status != entry.status);
        }
        if matchers_changed {
            state.refresh_matchers();
        }
    }
    Ok(())
}

pub(crate) fn mark_media_missing(state: &AppState, media_id: i64) -> Result<()> {
    if let Some(account) = account_id(&state.db) {
        if let Some(mut pending) = state
            .db
            .pending(account)?
            .into_iter()
            .find(|p| p.media_id == media_id)
        {
            pending.conflict = true;
            pending.missing_media = true;
            pending.error = Some("This anime was removed or merged on AniList. Your saved change is retained here until you discard it.".into());
            state.db.put_pending(account, &pending)?;
            state.db.delete_entry(media_id)?;
            state.refresh_matchers();
            entry_changed(media_id);
        }
    }
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

pub(crate) fn changed() {
    if let Some(handle) = APP.get() {
        let _ = handle.emit("kurisu://pending-changed", ());
    }
}

fn entry_changed(media_id: i64) {
    #[cfg(test)]
    ENTRY_NOTIFICATIONS.set(ENTRY_NOTIFICATIONS.get() + 1);
    if let Some(handle) = APP.get() {
        let _ = handle.emit(
            "kurisu://pending-changed",
            serde_json::json!({ "media_ids": [media_id] }),
        );
    }
}

pub(crate) fn publish(entry: &ListEntry) {
    if let Some(handle) = APP.get() {
        let _ = handle.emit("kurisu://episode-updated", entry);
    }
}

pub(crate) fn conflict(pending: &Pending, remote: Option<&ListEntry>) -> bool {
    let (Some(base), Some(remote)) = (&pending.base, remote) else {
        return pending.base.is_some() != remote.is_some();
    };
    let p = &pending.patch;
    base.id != remote.id
        || p.status
            .as_ref()
            .is_some_and(|v| remote.status != base.status && remote.status != *v)
        || p.progress
            .is_some_and(|v| remote.progress != base.progress && remote.progress != v)
        || p.score.is_some_and(|v| {
            remote.score.unwrap_or(0.0) != base.score.unwrap_or(0.0)
                && remote.score.unwrap_or(0.0) != v
        })
        || p.repeat
            .is_some_and(|v| remote.repeat != base.repeat && remote.repeat != v)
        || (p.progress.is_some() && remote.repeat != base.repeat)
        || (p.progress.is_some()
            && remote.status != base.status
            && p.status.as_ref() != Some(&remote.status))
}

pub(crate) fn already_applied(pending: &Pending, remote: &ListEntry) -> bool {
    if pending.base.as_ref().is_some_and(|base| {
        base.id != remote.id
            || (pending.patch.progress.is_some()
                && remote.repeat != pending.patch.repeat.unwrap_or(base.repeat))
    }) {
        return false;
    }
    let p = &pending.patch;
    p.status.as_ref().is_none_or(|v| remote.status == *v)
        && p.progress.is_none_or(|v| remote.progress == v)
        && p.score.is_none_or(|v| remote.score.unwrap_or(0.0) == v)
        && p.repeat.is_none_or(|v| remote.repeat == v)
}

/// Caller holds remote_lock so queued writes and account replacement remain ordered.
pub(crate) async fn flush_unlocked(state: &AppState) -> Result<(), String> {
    let _guard = state.entry_lock.lock().await;
    let Some(account) = account_id(&state.db) else {
        return Ok(());
    };
    let al = state.anilist.lock().clone();
    if !al.has_token() {
        return Ok(());
    }
    let pending = state
        .db
        .pending(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| !p.conflict)
        .take(10)
        .collect::<Vec<_>>();
    drop(_guard);
    for pending in pending {
        flush_one_unlocked(state, &al, account, pending).await?;
    }
    Ok(())
}

async fn flush_background(state: &AppState) -> Result<(), String> {
    let (account, token, media_ids) = {
        let _remote = state.remote_lock.lock().await;
        let _guard = state.entry_lock.lock().await;
        let Some(account) = account_id(&state.db) else {
            return Ok(());
        };
        let token = state.anilist.lock().token();
        if token.is_none() {
            return Ok(());
        }
        let media_ids = state
            .db
            .pending_background_ids(account)
            .map_err(|e| e.to_string())?;
        (account, token, media_ids)
    };
    for media_id in media_ids {
        // Tokio's FIFO mutex admits waiting edits between complete remote writes.
        let _remote = state.remote_lock.lock().await;
        let _guard = state.entry_lock.lock().await;
        let al = state.anilist.lock().clone();
        if account_id(&state.db) != Some(account) || al.token() != token {
            return Ok(());
        }
        let pending = state
            .db
            .pending_change(account, media_id)
            .map_err(|e| e.to_string())?
            .filter(|pending| !pending.conflict);
        drop(_guard);
        if let Some(pending) = pending {
            flush_one_unlocked(state, &al, account, pending).await?;
        }
    }
    Ok(())
}

/// Caller holds remote_lock through preflight, mutation and local acknowledgement.
async fn flush_one_unlocked(
    state: &AppState,
    al: &anilist::AniList,
    account: i64,
    mut pending: Pending,
) -> Result<(), String> {
    let result = al.entry_by_media_id(pending.media_id).await;
    let _guard = state.entry_lock.lock().await;
    let remote = match result {
        Ok(remote) => remote.map(|e| remote_entry(pending.media_id, e)),
        Err(e) => {
            let error = e.to_string();
            let conflict = !anilist::is_retryable(&e) && !anilist::is_auth_rejection(&e);
            let mut pending_changed = false;
            // Reload under entry_lock so failed requests cannot overwrite newer intent.
            if let Some(mut current) = state
                .db
                .pending_change(account, pending.media_id)
                .map_err(|e| e.to_string())?
            {
                if current.error.as_deref() != Some(&error) || current.conflict != conflict {
                    current.error = Some(error);
                    current.conflict = conflict;
                    state
                        .db
                        .put_pending(account, &current)
                        .map_err(|e| e.to_string())?;
                    pending_changed = true;
                }
            }
            if anilist::media_not_found(&e) {
                mark_media_missing(state, pending.media_id).map_err(|e| e.to_string())?;
            }
            if pending_changed {
                entry_changed(pending.media_id);
            }
            return Err(crate::commands::write_err(state, &e));
        }
    };
    if let Some(mut entry) = remote
        .clone()
        .filter(|entry| already_applied(&pending, entry))
    {
        entry.media = state
            .db
            .get_media(entry.media_id)
            .map_err(|e| e.to_string())?;
        state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
        state
            .db
            .remove_pending(account, entry.media_id)
            .map_err(|e| e.to_string())?;
        state.refresh_matchers();
        publish(&entry);
        entry_changed(entry.media_id);
        return Ok(());
    }
    if conflict(&pending, remote.as_ref()) {
        pending.conflict = true;
        pending.error = Some(
            "AniList changed while this update was waiting. Choose which version to keep.".into(),
        );
        state
            .db
            .put_pending(account, &pending)
            .map_err(|e| e.to_string())?;
        entry_changed(pending.media_id);
        return Ok(());
    }
    drop(_guard);
    send_pending(state, account, pending, remote.and_then(|entry| entry.id)).await
}

async fn send_pending(
    state: &AppState,
    account: i64,
    mut pending: Pending,
    entry_id: Option<i64>,
) -> Result<(), String> {
    if pending.missing_media {
        return Err("AniList no longer has this anime. Its saved change can be discarded.".into());
    }
    let p = &pending.patch;
    let status = p
        .status
        .as_deref()
        .map(crate::commands::parse_status)
        .transpose()?;
    let al = state.anilist.lock().clone();
    let saved = match entry_id {
        Some(id) => {
            al.save_entry_by_id(id, status, p.progress, p.score, p.repeat)
                .await
        }
        None => {
            al.save_entry(pending.media_id, status, p.progress, p.score, p.repeat)
                .await
        }
    };
    let _guard = state.entry_lock.lock().await;
    match saved {
        Ok(saved) => {
            let mut entry = remote_entry(pending.media_id, saved);
            entry.media = state
                .db
                .get_media(entry.media_id)
                .map_err(|e| e.to_string())?;
            state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
            state
                .db
                .remove_pending(account, entry.media_id)
                .map_err(|e| e.to_string())?;
            state.refresh_matchers();
            publish(&entry);
            entry_changed(entry.media_id);
            Ok(())
        }
        Err(e) => {
            pending.error = Some(if entry_id.is_some() && anilist::media_not_found(&e) {
                "Your AniList entry changed or was removed during sync. Choose which version to keep."
                    .into()
            } else {
                e.to_string()
            });
            pending.conflict = !anilist::is_retryable(&e) && !anilist::is_auth_rejection(&e);
            state
                .db
                .put_pending(account, &pending)
                .map_err(|e| e.to_string())?;
            if entry_id.is_none() && anilist::media_not_found(&e) {
                mark_media_missing(state, pending.media_id).map_err(|e| e.to_string())?;
            }
            entry_changed(pending.media_id);
            Err(crate::commands::write_err(state, &e))
        }
    }
}

#[tauri::command]
pub fn get_pending_changes(state: State<'_, AppState>) -> Result<Vec<PendingChange>, String> {
    summaries(&state)
}

fn summaries(state: &AppState) -> Result<Vec<PendingChange>, String> {
    let Some(account) = account_id(&state.db) else {
        return Ok(vec![]);
    };
    let conn = state.db.0.lock();
    let mut stmt = conn
        .prepare(
            "SELECT p.payload,m.id,COALESCE(m.title_english,m.title_romaji,m.title_native)
             FROM pending_change p LEFT JOIN media m ON m.id = p.media_id
             WHERE p.account_id = ? ORDER BY p.media_id",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([account], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        let (payload, cached_id, cached_title) = row.map_err(|e| e.to_string())?;
        let p = parse_pending(&payload).map_err(|e| e.to_string())?;
        let title = match cached_id {
            Some(id) => cached_title.unwrap_or_else(|| format!("#{id}")),
            None => p
                .base
                .as_ref()
                .and_then(|entry| entry.media.as_ref())
                .map(|media| media.display_title())
                .unwrap_or_else(|| format!("#{}", p.media_id)),
        };
        Ok(PendingChange {
            media_id: p.media_id,
            title,
            progress: p.patch.progress,
            status: p.patch.status,
            score: p.patch.score,
            repeat: p.patch.repeat,
            error: p.error,
            conflict: p.conflict,
            missing_media: p.missing_media,
        })
    })
    .collect()
}

#[tauri::command]
pub async fn sync_pending_changes(
    state: State<'_, AppState>,
) -> Result<Vec<PendingChange>, String> {
    let token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    if state.anilist.lock().token() != token {
        return Err("Account changed".into());
    }
    flush_unlocked(&state).await?;
    summaries(&state)
}

#[tauri::command]
pub async fn resolve_pending_change(
    media_id: i64,
    keep_local: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    resolve_pending_inner(media_id, keep_local, state.inner()).await
}

async fn resolve_pending_inner(
    media_id: i64,
    keep_local: bool,
    state: &AppState,
) -> Result<(), String> {
    let token = state.anilist.lock().token();
    let _remote = state.remote_lock.lock().await;
    if token.is_none() || state.anilist.lock().token() != token {
        return Err("Account changed".into());
    }
    let _guard = state.entry_lock.lock().await;
    let account = account_id(&state.db).ok_or("Not signed in")?;
    let Some(pending) = state
        .db
        .pending(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| p.media_id == media_id)
    else {
        return Ok(());
    };
    if keep_local {
        if pending.missing_media {
            return Err(
                "AniList no longer has this anime. Its saved change can be discarded.".into(),
            );
        }
        let al = state.anilist.lock().clone();
        drop(_guard);
        let result = al.entry_by_media_id(media_id).await;
        let _guard = state.entry_lock.lock().await;
        let remote = match result {
            Ok(remote) => remote,
            Err(e) => {
                if anilist::media_not_found(&e) {
                    mark_media_missing(state, media_id).map_err(|e| e.to_string())?;
                }
                return Err(crate::commands::write_err(state, &e));
            }
        };
        drop(_guard);
        return send_pending(state, account, pending, remote.map(|entry| entry.id)).await;
    }
    if pending.missing_media {
        state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
        state
            .db
            .remove_pending(account, media_id)
            .map_err(|e| e.to_string())?;
        state.refresh_matchers();
        entry_changed(media_id);
        return Ok(());
    }
    let al = state.anilist.lock().clone();
    drop(_guard);
    let result = al.entry_by_media_id(media_id).await;
    let _guard = state.entry_lock.lock().await;
    let remote = match result {
        Ok(remote) => remote,
        Err(e) if anilist::media_not_found(&e) => None,
        Err(e) => return Err(crate::commands::write_err(state, &e)),
    };
    match remote {
        Some(remote) => {
            let mut entry = remote_entry(media_id, remote);
            entry.media = state.db.get_media(media_id).map_err(|e| e.to_string())?;
            state.db.upsert_entry(&entry).map_err(|e| e.to_string())?;
            publish(&entry);
        }
        None => {
            state.db.delete_entry(media_id).map_err(|e| e.to_string())?;
        }
    }
    state
        .db
        .remove_pending(account, media_id)
        .map_err(|e| e.to_string())?;
    state.refresh_matchers();
    entry_changed(media_id);
    Ok(())
}

static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

pub fn spawn(app: tauri::AppHandle) {
    let _ = APP.set(app.clone());
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let state = app.state::<AppState>();
            if let Err(e) = flush_background(&state).await {
                log::debug!("pending sync: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Media, User};
    use parking_lot::Mutex;
    use serde_json::json;
    use std::{path::Path, sync::Arc};

    fn state(api: anilist::AniList, db: Db) -> AppState {
        AppState {
            anilist: Mutex::new(api),
            db: Arc::new(db),
            user: Mutex::new(None),
            auth_intent: tokio::sync::watch::channel(0).0,
            entry_lock: tokio::sync::Mutex::new(()),
            remote_lock: tokio::sync::Mutex::new(()),
            matchers: Mutex::new(Arc::new(vec![])),
            library_cache: Default::default(),
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
                ..Default::default()
            })
            .unwrap();
    }

    fn remote(progress: i64) -> serde_json::Value {
        json!({"id":22,"status":"CURRENT","progress":progress,"score":0,"repeat":0})
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
                            base: None,
                            patch: Patch {
                                progress: Some(media_id),
                                ..Default::default()
                            },
                            error: None,
                            conflict: false,
                            missing_media: false,
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
            assert_eq!(statements.len(), 2, "{statements:?}");
            assert!(statements[0].contains("settings"));
            assert!(statements[1].contains("LEFT JOIN media"));
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
            println!("pending summaries {count} rows: 1 joined query + 1 account lookup");
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
            (Some(""), Some("Romaji"), Some("Native"), ""),
            (None, Some("Romaji"), Some("Native"), "Romaji"),
            (None, None, Some("Native"), "Native"),
            (None, None, None, "#5"),
            (Some("  "), Some("Romaji"), None, "  "),
            (None, Some(""), Some("Native"), ""),
            (None, None, Some(""), ""),
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
                        },
                        error: Some("Saved error".into()),
                        conflict: true,
                        missing_media: true,
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
            .chain(["Saved title", "#70", "#11", ""])
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
        assert!(flush_one_unlocked(&state, &al, 7, stale).await.is_err());
        assert_eq!(crate::db::durable_commit_count() - before, 1);
        assert_eq!(ENTRY_NOTIFICATIONS.get() - events, 1);
        let current = state.db.pending_change(7, 1).unwrap().unwrap();
        assert!(current.conflict);
        let before = crate::db::durable_commit_count();
        let events = ENTRY_NOTIFICATIONS.get();
        assert!(flush_one_unlocked(&state, &al, 7, current).await.is_err());
        assert_eq!(crate::db::durable_commit_count(), before);
        assert_eq!(ENTRY_NOTIFICATIONS.get(), events);
        for _ in 0..7 {
            let request = requests.recv().await.unwrap();
            assert_eq!(request["variables"], json!({"mediaId":1}));
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
            let error = api.entry_by_media_id(1).await.err().unwrap().to_string();
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
            let error = flush_one_unlocked(&state, &al, 7, pending)
                .await
                .unwrap_err();
            let retained = state.db.pending_change(7, 1).unwrap().unwrap();
            assert_eq!(retained.patch.progress, Some(4));
            if status == 401 {
                assert!(error.contains("expired"));
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
                    flush_one_unlocked(&worker_state, &al, 7, stale).await
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
                    base: None,
                    patch: Patch {
                        progress: Some(4),
                        ..Default::default()
                    },
                    error: None,
                    conflict: conflict(media_id),
                    missing_media: false,
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
            assert_eq!(db.pending_background_ids(7).unwrap(), expected);
            let parsed = PENDING_DESERIALIZATIONS.get() - before;
            assert_eq!(parsed, [10, 20, 110, 1000][pattern]);
            assert_eq!(trace.statements.borrow().len(), 1);
            assert_eq!(db.0.lock().total_changes(), writes);
            assert!(db.pending_background_ids(9).unwrap().is_empty());
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
                db.pending_background_ids(7).unwrap(),
                (1..=10).collect::<Vec<_>>()
            );
            assert_eq!(PENDING_DESERIALIZATIONS.get() - before, 10);
            assert!(db.pending(7).is_err());
            db.remove_pending(7, 1).unwrap();
            assert_eq!(
                db.pending_background_ids(7).unwrap_err().to_string(),
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
                let mut after = before.clone();
                after["progress"] = json!(4);
                [
                    (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
                    (200, json!({"data":{"SaveMediaListEntry":after}})),
                ]
            })
            .collect()
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
                json!({"mediaId":1})
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
                json!({"id":22,"progress":4})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"mediaId":2})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":23,"progress":5})
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
                    json!({"id":22,"progress":4})
                );
                let pending = state.db.pending_change(7, 2).unwrap();
                if resolve {
                    assert!(pending.is_none());
                    assert_eq!(
                        requests.recv().await.unwrap()["variables"],
                        json!({"mediaId":2})
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
                    json!({"id":22,"progress":4})
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
                json!({"mediaId":media_id})
            );
            assert_eq!(
                requests.recv().await.unwrap()["variables"],
                json!({"id":media_id + 21,"progress":4})
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
            replies.push((200, json!({"data":{"Media":{"mediaListEntry":baseline}}})));
        }
        replies.push((200, json!({"data":{"SaveMediaListEntry":unrelated}})));
        replies.push((200, json!({"data":{"Media":{"mediaListEntry":applied}}})));
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
                assert_eq!(lookup["variables"], json!({"mediaId":1}));
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
                json!({"id":22,"progress":4})
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
                json!({"mediaId":1})
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
                (200, json!({"data":{"Media":{"mediaListEntry":null}}})),
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
    async fn remote_entry_timestamps_survive_every_sync_outcome() {
        for action in ["retry", "acknowledge", "keep_remote", "keep_local", "save"] {
            let mut before = remote(if action == "acknowledge" { 4 } else { 3 });
            before["updatedAt"] = json!(1_700_000_000);
            let mut after = remote(4);
            after["updatedAt"] = json!(1_700_000_500);
            let writes = matches!(action, "retry" | "keep_local" | "save");
            let mut replies = vec![];
            if action != "save" {
                replies.push((200, json!({"data":{"Media":{"mediaListEntry":before}}})));
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
            (200, json!({"data":{"Media":{"mediaListEntry":remote(3)}}})),
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
        assert_eq!(mutation["variables"], json!({"id":22,"progress":4}));
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
                assert_eq!(entry.score, None);
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
            (200, json!({"data":{"Media":{"mediaListEntry":null}}})),
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
            json!({"mediaId":1,"status":"CURRENT","progress":4})
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
            (200, json!({"data":{"Media":{"mediaListEntry":{"id":33,"status":"CURRENT","progress":8,"score":9,"repeat":0}}}})),
            (200, json!({"data":{"SaveMediaListEntry":{"id":33,"status":"CURRENT","progress":4,"score":9,"repeat":0}}})),
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
            json!({"id":33,"progress":4})
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
            (200,json!({"data":{"Media":{"mediaListEntry":remote(3)}}})),
            (200,json!({"data":{"SaveMediaListEntry":{"id":22,"status":"CURRENT","progress":4,"score":9,"repeat":0}}})),
        ]).await;
        let restarted = state(api, Db::open(&path).unwrap());
        assert_eq!(restarted.db.get_entry(1).unwrap().unwrap().progress, 4);
        flush_unlocked(&restarted).await.unwrap();
        requests.recv().await.unwrap();
        assert_eq!(
            requests.recv().await.unwrap()["variables"],
            json!({"id":22,"progress":4})
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
                json!({"data":{"Media":{"mediaListEntry":remote}}}),
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
                (200, json!({"data":{"Media":{"mediaListEntry":before}}})),
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
            assert_eq!(lookup["variables"], json!({"mediaId":1}));
            let saved = requests.recv().await.unwrap();
            assert_eq!(
                saved["variables"],
                if repeating {
                    json!({"id":22,"progress":12,"status":"COMPLETED","repeat":3})
                } else {
                    json!({"id":22,"progress":4})
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
            json!({"data":{"Media":{"mediaListEntry":applied}}}),
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
                replies.push((200, json!({"data":{"Media":{"mediaListEntry":remote(3)}}})));
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
            (200, json!({"data":{"Media":{"mediaListEntry":remote(8)}}})),
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
    async fn an_interrupted_success_is_acknowledged_without_repeating_the_mutation() {
        let (api, mut requests) = anilist::mock_api(vec![(
            200,
            json!({"data":{"Media":{"mediaListEntry":remote(4)}}}),
        )])
        .await;
        let state = state(api, Db::open(Path::new(":memory:")).unwrap());
        seed(&state);
        state.db.delete_entry(1).unwrap();
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
        flush_unlocked(&state).await.unwrap();
        assert!(state.db.pending(7).unwrap().is_empty());
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().progress, 4);
        requests.recv().await.unwrap();
        assert!(requests.try_recv().is_err());
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
            snapshot.id = Some(99);
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
            assert_eq!((restored.id, restored.updated_at), (Some(99), Some(1234)));
        }
    }

    #[test]
    fn overlay_restores_absent_rows_and_only_deletes_missing_media_once() {
        let state = state(
            anilist::AniList::new(),
            Db::open(Path::new(":memory:")).unwrap(),
        );
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
                            base: None,
                            patch: Patch {
                                progress: Some(4),
                                ..Default::default()
                            },
                            error: None,
                            conflict: false,
                            missing_media: false,
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
        assert_eq!(state.db.get_entry(1).unwrap().unwrap().score, None);
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
            (200, json!({"data":{"Media":{"mediaListEntry":remote(3)}}})),
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
            json!({"id":22,"progress":4})
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
                (6,'','Romaji','Native',1);
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
            ["", "#5", "#4", "Native", "Romaji", "English"]
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
         COALESCE(m.title_english,m.title_romaji,m.title_native,'#' || h.media_id)
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
                    title: r.get(5)?,
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
            base: Some(base.clone()),
            patch: Patch {
                progress: Some(4),
                ..Default::default()
            },
            error: None,
            conflict: false,
            missing_media: false,
        };
        assert!(conflict(&pending, None));
        let mut remote = base;
        remote.repeat = 1;
        assert!(conflict(&pending, Some(&remote)));
    }
}
