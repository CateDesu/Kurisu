use anyhow::Result;
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

impl Db {
    pub(crate) fn pending(&self, account: i64) -> Result<Vec<Pending>> {
        let conn = self.0.lock();
        let mut stmt = conn
            .prepare("SELECT payload FROM pending_change WHERE account_id = ? ORDER BY media_id")?;
        let rows = stmt.query_map([account], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
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
        let mut stmt = conn.prepare(
            "SELECT h.id,h.path,h.media_id,h.episode,h.watched_at,
             COALESCE(m.title_english,m.title_romaji,m.title_native,'#' || h.media_id)
             FROM watch_history h LEFT JOIN media m ON m.id = h.media_id
             WHERE h.account_id = ? AND (? IS NULL OR h.id < ?) ORDER BY h.id DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(rusqlite::params![account, before, before, limit], |r| {
            Ok(WatchHistoryItem {
                id: r.get(0)?,
                path: r.get(1)?,
                media_id: r.get(2)?,
                episode: r.get(3)?,
                watched_at: r.get(4)?,
                title: r.get(5)?,
            })
        })?;
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
    let existing = state
        .db
        .pending(account)?
        .into_iter()
        .find(|p| p.media_id == media_id);
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
    changed();
    Ok(entry)
}

pub(crate) fn overlay(state: &AppState) -> Result<()> {
    if let Some(account) = account_id(&state.db) {
        let pending = state.db.pending(account)?;
        if pending.is_empty() {
            return Ok(());
        }
        for pending in pending {
            if pending.missing_media {
                state.db.delete_entry(pending.media_id)?;
                continue;
            }
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
            state.db.upsert_entry(&entry)?;
        }
        state.refresh_matchers();
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
            changed();
        }
    }
    Ok(())
}

fn remote_entry(media_id: i64, remote: anilist::SavedEntry) -> ListEntry {
    ListEntry {
        id: Some(remote.id),
        media_id,
        status: remote.status,
        progress: remote.progress,
        score: remote.score,
        repeat: remote.repeat,
        ..Default::default()
    }
}

pub(crate) fn changed() {
    if let Some(handle) = APP.get() {
        let _ = handle.emit("kurisu://pending-changed", ());
    }
}

pub(crate) fn publish(entry: &ListEntry) {
    if let Some(handle) = APP.get() {
        let _ = handle.emit("kurisu://episode-updated", entry);
    }
}

fn conflict(pending: &Pending, remote: Option<&ListEntry>) -> bool {
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

fn already_applied(pending: &Pending, remote: &ListEntry) -> bool {
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
    for mut pending in pending {
        let result = al.entry_by_media_id(pending.media_id).await;
        let _guard = state.entry_lock.lock().await;
        let remote = match result {
            Ok(remote) => remote.map(|e| remote_entry(pending.media_id, e)),
            Err(e) => {
                pending.error = Some(e.to_string());
                pending.conflict = !anilist::is_retryable(&e) && !anilist::is_auth_rejection(&e);
                state
                    .db
                    .put_pending(account, &pending)
                    .map_err(|e| e.to_string())?;
                if anilist::media_not_found(&e) {
                    mark_media_missing(state, pending.media_id).map_err(|e| e.to_string())?;
                }
                changed();
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
            changed();
            continue;
        }
        if conflict(&pending, remote.as_ref()) {
            pending.conflict = true;
            pending.error = Some(
                "AniList changed while this update was waiting. Choose which version to keep."
                    .into(),
            );
            state
                .db
                .put_pending(account, &pending)
                .map_err(|e| e.to_string())?;
            changed();
            continue;
        }
        drop(_guard);
        send_pending(state, account, pending, remote.and_then(|entry| entry.id)).await?;
    }
    Ok(())
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
            changed();
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
            changed();
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
    state
        .db
        .pending(account)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|p| {
            let title = state
                .db
                .get_media(p.media_id)
                .map_err(|e| e.to_string())?
                .or_else(|| p.base.as_ref().and_then(|entry| entry.media.clone()))
                .map(|m| m.display_title())
                .unwrap_or_else(|| format!("#{}", p.media_id));
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
        changed();
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
    changed();
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
            let _guard = state.remote_lock.lock().await;
            if let Err(e) = flush_unlocked(&state).await {
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
