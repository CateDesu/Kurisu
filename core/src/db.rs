use std::{
    collections::HashSet,
    sync::{Arc, Weak},
};

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

use crate::models::{ListEntry, Media};

pub(crate) const SCHEMA_VERSION: i64 = 8;
pub(crate) const LIST_OWNER_KEY: &str = "native_preference:kurisu.list.owner";
pub(crate) const TOKEN_KEY: &str = "anilist_token";
pub(crate) const TOKEN_REJECTED_KEY: &str = "anilist_token_rejected";
const CATALOG_METADATA: u8 = 1;
const CATALOG_DETAIL: u8 = 2;
pub(crate) const SCOPED_LIST_PREFERENCE_KEYS: &[&str] = &[
    "native_preference:kurisu.list.q",
    "native_preference:kurisu.list.filter",
    "native_preference:kurisu.list.synced",
];

fn parse_valid_account_id(value: Option<&str>) -> Option<i64> {
    value
        .and_then(|value| value.parse().ok())
        .filter(|id| crate::anilist::valid_account_id(*id))
}

pub struct Db(pub Mutex<Connection>, Mutex<Vec<Weak<CatalogRead>>>);

#[derive(Clone, Copy)]
pub(crate) enum CatalogUpdate {
    Episodes,
    Metadata,
    Detail,
    Section(&'static str),
}

#[derive(Default)]
pub(crate) struct CatalogRead {
    newer: Mutex<CatalogChanges>,
    removed: Mutex<HashSet<i64>>,
}

#[derive(Default)]
struct CatalogChanges {
    episodes: HashSet<i64>,
    metadata: HashSet<i64>,
    detail: HashSet<i64>,
    sections: HashSet<(i64, &'static str)>,
}

impl CatalogRead {
    // Keep the entry lock through the cache write.
    pub(crate) fn preserve_newer<'a>(
        &self,
        db: &Db,
        media: impl IntoIterator<Item = &'a mut Media>,
    ) {
        let newer = self.newer.lock();
        if newer.metadata.is_empty() && newer.detail.is_empty() && newer.episodes.is_empty() {
            return;
        }
        let conn = db.0.lock();
        for item in media {
            let keep_detail = newer.detail.contains(&item.id);
            if keep_detail || newer.metadata.contains(&item.id) {
                if let Ok(Some((cached, _))) = read_media_row(&conn, item.id) {
                    *item = if keep_detail {
                        cached
                    } else {
                        cached.with_detail_fields_from(std::mem::take(item))
                    };
                }
            } else if newer.episodes.contains(&item.id) {
                if let Ok(Some(total)) = cached_episode_total(&conn, item.id) {
                    item.episodes = total;
                }
            }
        }
    }

    pub(crate) fn has_newer_media(&self, id: i64) -> bool {
        let newer = self.newer.lock();
        newer.metadata.contains(&id) || newer.detail.contains(&id)
    }

    pub(crate) fn has_newer_section(&self, id: i64, section: &'static str) -> bool {
        self.newer.lock().sections.contains(&(id, section))
    }

    pub(crate) fn removed_media(&self) -> HashSet<i64> {
        std::mem::take(&mut *self.removed.lock())
    }

    // Call after the cache write commits, while holding the entry lock.
    pub(crate) fn record_write(
        &self,
        db: &Db,
        update: CatalogUpdate,
        media_ids: impl Iterator<Item = i64> + Clone,
    ) {
        mark_catalog_reads(&mut db.1.lock(), Some(self), update, media_ids);
    }
}

#[cfg(test)]
thread_local! {
    static DURABLE_COMMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn durable_commit_count() -> usize {
    DURABLE_COMMITS.get()
}

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // Writes default to NORMAL. Queued list changes and watch history use durable_write.
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )?;
        Self::migrate(&conn)?;
        // The database and WAL can contain the plaintext AniList token.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut wal = path.as_os_str().to_os_string();
            wal.push("-wal");
            let mut shm = path.as_os_str().to_os_string();
            shm.push("-shm");
            for p in [path.to_path_buf(), wal.into(), shm.into()] {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(Db(Mutex::new(conn), Mutex::default()))
    }

    pub(crate) fn durable_write(
        &self,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<()> {
        let mut conn = self.0.lock();
        conn.pragma_update(None, "synchronous", "FULL")?;
        let result: Result<()> = (|| {
            let tx = conn.transaction()?;
            write(&tx)?;
            tx.commit()?;
            #[cfg(test)]
            DURABLE_COMMITS.set(DURABLE_COMMITS.get() + 1);
            Ok(())
        })();
        let reset = conn.pragma_update(None, "synchronous", "NORMAL");
        result?;
        Ok(reset?)
    }

    /// BEGIN IMMEDIATE prevents concurrent migrations.
    fn migrate(conn: &Connection) -> Result<()> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match Self::migrate_locked(conn) {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn migrate_locked(conn: &Connection) -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS media (
                id              INTEGER PRIMARY KEY,
                id_mal          INTEGER,
                title_romaji    TEXT,
                title_english   TEXT,
                title_native    TEXT,
                cover_medium    TEXT,
                cover_large     TEXT,
                episodes        INTEGER,
                format          TEXT,
                status          TEXT,
                average_score   INTEGER,
                season          TEXT,
                season_year     INTEGER,
                description     TEXT,
                cached_at       INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS list_entry (
                media_id    INTEGER PRIMARY KEY,
                entry_id    INTEGER,
                status      TEXT NOT NULL,
                progress    INTEGER NOT NULL DEFAULT 0,
                score       REAL,
                repeat      INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER
            );
            CREATE TABLE IF NOT EXISTS watched_file (
                path        TEXT PRIMARY KEY,
                media_id    INTEGER NOT NULL,
                episode     INTEGER NOT NULL,
                watched_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
            )?;
        }
        if version < 2 {
            Self::ensure_column(conn, "media", "next_airing_episode", "INTEGER")?;
            Self::ensure_column(conn, "media", "next_airing_at", "INTEGER")?;
        }
        if version < 3 {
            Self::ensure_column(conn, "media", "banner_image", "TEXT")?;
            Self::ensure_column(conn, "media", "genres", "TEXT")?;
            Self::ensure_column(conn, "media", "duration", "INTEGER")?;
            Self::ensure_column(conn, "media", "source", "TEXT")?;
            Self::ensure_column(conn, "media", "studios", "TEXT")?;
        }
        if version < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS rss_seen (
                guid    TEXT PRIMARY KEY,
                seen_at INTEGER NOT NULL
            );",
            )?;
        }
        if version < 5 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_change (
                    account_id INTEGER NOT NULL, media_id INTEGER NOT NULL,
                    payload TEXT NOT NULL, PRIMARY KEY(account_id, media_id)
                );
                CREATE TABLE IF NOT EXISTS watch_history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id INTEGER NOT NULL, path TEXT NOT NULL,
                    media_id INTEGER NOT NULL, episode INTEGER NOT NULL,
                    watched_at INTEGER NOT NULL
                );",
            )?;
        }
        if version < 6 {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS watch_history_duplicate
                 ON watch_history (account_id,path,media_id,episode,watched_at);",
            )?;
        }
        if version < 7 {
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS watch_history_account_id
                 ON watch_history (account_id,id);",
            )?;
        }
        if version < 8 {
            Self::ensure_column(conn, "list_entry", "queued_creation", "TEXT")?;
            conn.execute(
                "UPDATE pending_change
                 SET payload = json_set(payload, '$.patch.creation_key', lower(hex(randomblob(16))))
                 WHERE json_valid(payload)
                   AND json_type(payload, '$.base') = 'null'
                   AND json_extract(payload, '$.patch.creation_key') IS NULL",
                [],
            )?;
            let active_account: Option<String> = conn
                .query_row(
                    "SELECT value FROM settings WHERE key = 'anilist_user_id'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(account_id) = parse_valid_account_id(active_account.as_deref()) {
                conn.execute(
                    "UPDATE list_entry AS e
                     SET queued_creation = json_extract(p.payload, '$.patch.creation_key')
                     FROM pending_change AS p
                     WHERE p.account_id = ?1
                       AND p.media_id = e.media_id
                       AND e.entry_id IS NULL
                       AND e.queued_creation IS NULL
                       AND json_valid(p.payload)
                       AND json_type(p.payload, '$.base') = 'null'
                       AND COALESCE(json_extract(p.payload, '$.deleting'), 0) = 0
                       AND COALESCE(json_extract(p.payload, '$.missing_media'), 0) = 0
                       AND json_type(p.payload, '$.patch.creation_key') = 'text'",
                    [account_id],
                )?;
            }
        }
        if version <= 8 {
            // Selected nulls have different semantics from episode-only rows.
            Self::ensure_column(
                conn,
                "media",
                "catalog_fields",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        Ok(())
    }

    fn ensure_column(conn: &Connection, table: &str, col: &str, ty: &str) -> Result<()> {
        let present: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .filter_map(Result::ok)
            .collect();
        if !present.iter().any(|c| c == col) {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"), [])?;
        }
        Ok(())
    }

    pub fn upsert_media(&self, m: &Media) -> Result<()> {
        let c = self.0.lock();
        upsert_media_row(&c, m)
    }

    pub(crate) fn catalog_read(&self) -> Arc<CatalogRead> {
        let read = Arc::new(CatalogRead::default());
        let mut reads = self.1.lock();
        reads.retain(|read| read.strong_count() != 0);
        reads.push(Arc::downgrade(&read));
        read
    }

    pub(crate) fn restore_cached_metadata(&self, media: &mut Media) {
        if let Ok(Some((cached, fields))) = read_media_row(&self.0.lock(), media.id) {
            if fields == CATALOG_DETAIL {
                *media = cached;
            } else if fields == CATALOG_METADATA || cached.has_metadata() {
                *media = cached.with_detail_fields_from(std::mem::take(media));
            } else {
                media.episodes = cached.episodes;
            }
        }
    }

    pub(crate) fn restore_cached_relations(
        &self,
        relations: &mut Vec<crate::models::MediaRelation>,
    ) {
        let conn = self.0.lock();
        relations.retain_mut(|relation| {
            let id = relation.media.id;
            match read_media_row(&conn, id) {
                Ok(Some((cached, fields))) => {
                    if fields >= CATALOG_METADATA || cached.has_metadata() {
                        relation.media = cached;
                    } else {
                        relation.media.episodes = cached.episodes;
                    }
                    true
                }
                Ok(None) => !conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM settings WHERE key = ? AND value = 'null')",
                        [format!("media_detail:{id}")],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(false),
                Err(_) => true,
            }
        });
    }

    pub(crate) fn cache_episode_total(&self, media_id: i64, episodes: Option<i64>) -> Result<bool> {
        let mut reads = self.1.lock();
        let changed = write_episode_total(&self.0.lock(), media_id, episodes)?;
        mark_catalog_reads(
            &mut reads,
            None,
            CatalogUpdate::Episodes,
            std::iter::once(media_id),
        );
        Ok(changed)
    }

    pub(crate) fn durable_write_with_episode_total(
        &self,
        media_id: i64,
        total: crate::anilist::ReportedEpisodes,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some(total) = total else {
            return self.durable_write(write);
        };
        let mut reads = self.1.lock();
        self.durable_write(|tx| {
            write(tx)?;
            write_episode_total(tx, media_id, total)?;
            Ok(())
        })?;
        mark_catalog_reads(
            &mut reads,
            None,
            CatalogUpdate::Episodes,
            std::iter::once(media_id),
        );
        Ok(())
    }

    pub fn upsert_media_batch(&self, media: &[Media]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        for m in media {
            upsert_media_row(&tx, m)?;
        }
        Ok(tx.commit()?)
    }

    /// Detail fetches can clear fields that lean queries must preserve.
    pub fn upsert_media_detail(&self, m: &Media) -> Result<()> {
        let c = self.0.lock();
        let written = c.execute(
            "INSERT INTO media
             (id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
              episodes,format,status,average_score,season,season_year,description,
              next_airing_episode,next_airing_at,banner_image,genres,duration,source,studios,
              cached_at,catalog_fields)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET
              id_mal=excluded.id_mal, title_romaji=excluded.title_romaji,
              title_english=excluded.title_english, title_native=excluded.title_native,
              cover_medium=excluded.cover_medium, cover_large=excluded.cover_large,
              episodes=excluded.episodes, format=excluded.format, status=excluded.status,
              average_score=excluded.average_score, season=excluded.season,
              season_year=excluded.season_year, description=excluded.description,
              next_airing_episode=excluded.next_airing_episode,
              next_airing_at=excluded.next_airing_at,
              banner_image=excluded.banner_image,
              genres=excluded.genres,
              duration=excluded.duration,
              source=excluded.source,
              studios=excluded.studios,
              cached_at=excluded.cached_at,
              catalog_fields=excluded.catalog_fields",
            rusqlite::params![
                m.id,
                m.id_mal,
                m.title_romaji,
                m.title_english,
                m.title_native,
                m.cover_medium,
                m.cover_large,
                m.episodes.filter(|total| *total > 0),
                m.format,
                m.status,
                m.average_score,
                m.season,
                m.season_year,
                m.description,
                m.next_airing_episode,
                m.next_airing_at,
                m.banner_image,
                m.genres
                    .as_ref()
                    .and_then(|g| serde_json::to_string(g).ok()),
                m.duration,
                m.source,
                m.studios
                    .as_ref()
                    .and_then(|s| serde_json::to_string(s).ok()),
                chrono::Utc::now().timestamp(),
                CATALOG_DETAIL,
            ],
        )?;
        anyhow::ensure!(written == 1, "the media detail was not cached");
        Ok(())
    }

    pub(crate) fn cache_media_detail_sections(
        &self,
        key: &str,
        detail: &crate::models::MediaDetail,
    ) -> Result<()> {
        let value = serde_json::to_string(detail)?;
        let written = self.0.lock().execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
            [key, &value],
        )?;
        anyhow::ensure!(written == 1, "the media detail sections were not cached");
        Ok(())
    }

    pub fn get_media(&self, id: i64) -> Result<Option<Media>> {
        Ok(read_media_row(&self.0.lock(), id)?
            .map(|(media, _)| media)
            .filter(Media::has_metadata))
    }

    pub(crate) fn cached_catalog_media(&self, id: i64) -> Result<Option<Media>> {
        Ok(read_media_row(&self.0.lock(), id)?
            .filter(|(media, fields)| *fields >= CATALOG_METADATA || media.has_metadata())
            .map(|(media, _)| media))
    }

    pub fn episode_total(&self, id: i64) -> Result<Option<i64>> {
        Ok(self.cached_episode_total(id)?.flatten())
    }

    pub(crate) fn cached_episode_total(&self, id: i64) -> Result<Option<Option<i64>>> {
        cached_episode_total(&self.0.lock(), id)
    }

    pub fn upsert_entry(&self, e: &ListEntry) -> Result<()> {
        let c = self.0.lock();
        Self::write_entry(&c, e)
    }

    pub(crate) fn write_entry(c: &Connection, e: &ListEntry) -> Result<()> {
        Self::write_entry_with_creation(
            c,
            e,
            e.queued_creation.as_deref().filter(|_| e.id.is_none()),
        )
    }

    pub(crate) fn write_entry_with_creation(
        c: &Connection,
        e: &ListEntry,
        queued_creation: Option<&str>,
    ) -> Result<()> {
        c.prepare_cached(
            "INSERT OR REPLACE INTO list_entry
             (media_id,entry_id,status,progress,score,repeat,updated_at,queued_creation)
             VALUES (?,?,?,?,?,?,?,COALESCE(?, (
                 SELECT queued_creation FROM list_entry WHERE media_id = ?1 AND entry_id = ?2
             )))",
        )?
        .execute(rusqlite::params![
            e.media_id,
            e.id,
            e.status,
            e.progress,
            e.score,
            e.repeat,
            e.updated_at,
            queued_creation
        ])?;
        Ok(())
    }

    pub fn delete_entry(&self, media_id: i64) -> Result<()> {
        Self::delete_entry_from(&self.0.lock(), media_id)
    }

    pub(crate) fn delete_entry_from(conn: &Connection, media_id: i64) -> Result<()> {
        if conn.execute("DELETE FROM list_entry WHERE media_id = ?", [media_id])? == 0 {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM list_entry WHERE media_id = ?)",
                [media_id],
                |row| row.get(0),
            )?;
            anyhow::ensure!(!exists, "the list entry was not removed");
        }
        Ok(())
    }

    pub(crate) fn restore_and_remove_pending(
        &self,
        account: i64,
        media_id: i64,
        base: Option<&ListEntry>,
        reported_total: crate::anilist::ReportedEpisodes,
    ) -> Result<()> {
        self.durable_write_with_episode_total(media_id, reported_total, |tx| {
            match base {
                Some(entry) => {
                    Self::write_entry(tx, entry)?;
                    anyhow::ensure!(tx.changes() == 1, "the remote entry was not cached");
                }
                None => Self::delete_entry_from(tx, media_id)?,
            }
            Self::delete_pending_from(tx, account, media_id)
        })
    }

    pub fn delete_media(&self, media_id: i64) -> Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM media WHERE id = ?", [media_id])?;
        Ok(())
    }

    pub(crate) fn commit_media_removal(
        &self,
        remove_public_id: Option<i64>,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some(id) = remove_public_id else {
            return self.durable_write(write);
        };
        let mut reads = self.1.lock();
        self.durable_write(|tx| {
            write(tx)?;
            tx.execute("DELETE FROM media WHERE id = ?", [id])?;
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?, 'null')",
                [format!("media_detail:{id}")],
            )?;
            Ok(())
        })?;
        reads.retain(|read| {
            let Some(read) = read.upgrade() else {
                return false;
            };
            read.removed.lock().insert(id);
            true
        });
        Ok(())
    }

    pub fn replace_list_snapshot(&self, entries: &[ListEntry]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        Self::write_list_snapshot(&tx, entries)?;
        Ok(tx.commit()?)
    }

    fn write_list_snapshot(tx: &rusqlite::Transaction<'_>, entries: &[ListEntry]) -> Result<()> {
        {
            let mut statement = tx.prepare_cached(
                "INSERT INTO list_entry
                 (media_id,entry_id,status,progress,score,repeat,updated_at)
                 VALUES (?,?,?,?,?,?,?)
                 ON CONFLICT(media_id) DO UPDATE SET
                  entry_id=excluded.entry_id, status=excluded.status,
                  progress=excluded.progress, score=excluded.score,
                  repeat=excluded.repeat, updated_at=excluded.updated_at,
                  queued_creation=CASE WHEN list_entry.entry_id = excluded.entry_id
                                       THEN list_entry.queued_creation END
                 WHERE (list_entry.entry_id,list_entry.status,list_entry.progress,
                        list_entry.score,list_entry.repeat,list_entry.updated_at)
                   IS NOT (excluded.entry_id,excluded.status,excluded.progress,
                           excluded.score,excluded.repeat,excluded.updated_at)",
            )?;
            for e in entries {
                if let Some(m) = &e.media {
                    upsert_media_row(tx, m)?;
                }
                statement.execute(rusqlite::params![
                    e.media_id,
                    e.id,
                    e.status,
                    e.progress,
                    e.score,
                    e.repeat,
                    e.updated_at
                ])?;
            }
        }
        // A JSON array avoids SQLite's bound parameter limit.
        let keep: Vec<i64> = entries.iter().map(|e| e.media_id).collect();
        tx.execute(
            "DELETE FROM list_entry WHERE media_id NOT IN (SELECT value FROM json_each(?))",
            [serde_json::to_string(&keep)?],
        )?;
        Self::write_completion_baselines(tx)?;
        Ok(())
    }

    pub fn entries_with_media(&self) -> Result<Vec<ListEntry>> {
        self.entries_with_saved_media(Default::default())
    }

    pub(crate) fn entries_with_saved_media(
        &self,
        mut saved_media: std::collections::HashMap<i64, Media>,
    ) -> Result<Vec<ListEntry>> {
        let c = self.0.lock();
        let mut stmt = c.prepare(
            // NULLs preserve row_to_media_offset's layout without loading details.
            "SELECT e.media_id,e.entry_id,e.status,e.progress,e.score,e.repeat,e.updated_at,
                    m.id,m.id_mal,m.title_romaji,m.title_english,m.title_native,m.cover_medium,
                    m.cover_large,m.episodes,m.format,m.status,m.average_score,m.season,
                    m.season_year,NULL,m.next_airing_episode,m.next_airing_at,
                    NULL,NULL,NULL,NULL,NULL,e.queued_creation
             FROM list_entry e LEFT JOIN media m ON m.id = e.media_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let media_id = r.get(0)?;
            Ok(ListEntry {
                id: r.get::<_, Option<i64>>(1)?,
                media_id,
                status: r.get(2)?,
                progress: r.get(3)?,
                score: r.get(4)?,
                repeat: r.get(5)?,
                updated_at: r.get(6)?,
                queued_creation: r.get(28)?,
                media: row_to_media_offset(r, 7)
                    .ok()
                    .filter(Media::has_metadata)
                    .or_else(|| saved_media.remove(&media_id)),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn entry_media_ids(&self) -> Result<Vec<i64>> {
        let c = self.0.lock();
        let mut stmt = c.prepare("SELECT media_id FROM list_entry")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(ids)
    }

    pub fn get_entry(&self, media_id: i64) -> Result<Option<ListEntry>> {
        let c = self.0.lock();
        // Only an absent row may return None. Write paths treat it as a new entry.
        let row = c
            .query_row(
                "SELECT e.media_id,e.entry_id,e.status,e.progress,e.score,e.repeat,e.updated_at,
                        m.id,m.id_mal,m.title_romaji,m.title_english,m.title_native,m.cover_medium,
                        m.cover_large,m.episodes,m.format,m.status,m.average_score,m.season,
                        m.season_year,NULL,m.next_airing_episode,m.next_airing_at,
                        NULL,NULL,NULL,NULL,NULL,e.queued_creation
                 FROM list_entry e LEFT JOIN media m ON m.id = e.media_id
                 WHERE e.media_id = ?",
                [media_id],
                |r| {
                    Ok(ListEntry {
                        id: r.get::<_, Option<i64>>(1)?,
                        media_id: r.get(0)?,
                        status: r.get(2)?,
                        progress: r.get(3)?,
                        score: r.get(4)?,
                        repeat: r.get(5)?,
                        updated_at: r.get(6)?,
                        queued_creation: r.get(28)?,
                        media: row_to_media_offset(r, 7).ok().filter(Media::has_metadata),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub(crate) fn unqueued_progress(
        &self,
        media_id: i64,
        account_id: Option<i64>,
    ) -> Result<Option<i64>> {
        // Cache reads can overlay a write before its server response arrives.
        let c = self.0.lock();
        Ok(c.query_row(
            "SELECT progress FROM list_entry WHERE media_id = ?1
             AND NOT EXISTS (SELECT 1 FROM pending_change
                             WHERE media_id = ?1 AND account_id = ?2)",
            rusqlite::params![media_id, account_id],
            |row| row.get(0),
        )
        .optional()?)
    }

    pub fn replace_account(&self, token: &str, user: &crate::models::User) -> Result<bool> {
        self.replace_account_with_last_account(token, user, None)
    }

    pub(crate) fn replace_account_with_last_account(
        &self,
        token: &str,
        user: &crate::models::User,
        last_account: Option<i64>,
    ) -> Result<bool> {
        self.replace_account_and_list(token, user, last_account, None)
    }

    pub(crate) fn replace_account_and_list(
        &self,
        token: &str,
        user: &crate::models::User,
        last_account: Option<i64>,
        entries: Option<&[ListEntry]>,
    ) -> Result<bool> {
        let profile = serde_json::to_string(user)?;
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'anilist_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let id = user.id.to_string();
        let changed = previous.as_deref() != Some(id.as_str());
        let previous_profile: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'anilist_profile'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let previous_format = previous_profile
            .as_deref()
            .and_then(|profile| serde_json::from_str::<crate::models::User>(profile).ok())
            .and_then(|user| user.score_format);
        if changed {
            tx.execute("DELETE FROM list_entry", [])?;
        } else if previous_format != user.score_format {
            tx.execute("UPDATE list_entry SET score = NULL WHERE score != 0", [])?;
        }
        let persisted_owner: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [LIST_OWNER_KEY],
                |r| r.get(0),
            )
            .optional()?;
        // Persisted ownership survives sign-out and restart without an open workspace.
        let verified_owner = parse_valid_account_id(persisted_owner.as_deref());
        let list_owner = verified_owner
            .or_else(|| parse_valid_account_id(previous.as_deref()))
            .or(last_account.filter(|id| crate::anilist::valid_account_id(*id)));
        if (persisted_owner.is_some() && verified_owner.is_none())
            || list_owner.is_some_and(|owner| owner != user.id)
        {
            tx.execute(
                "DELETE FROM settings WHERE key IN ('native_preference:kurisu.list.q',
                 'native_preference:kurisu.list.filter', 'native_preference:kurisu.list.synced')",
                [],
            )?;
        }
        if crate::anilist::valid_account_id(user.id) {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [LIST_OWNER_KEY, id.as_str()],
            )?;
        }
        for (key, value) in [
            (TOKEN_KEY, token),
            ("anilist_username", user.name.as_str()),
            ("anilist_user_id", id.as_str()),
            ("anilist_profile", profile.as_str()),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [key, value],
            )?;
        }
        Self::delete_setting_from(&tx, TOKEN_REJECTED_KEY)?;
        if let Some(entries) = entries {
            Self::write_list_snapshot(&tx, entries)?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn saved_list_owner(&self) -> Option<i64> {
        let settings = self
            .get_settings_batch(&[LIST_OWNER_KEY, "anilist_user_id"])
            .ok()?;
        parse_valid_account_id(settings.get(LIST_OWNER_KEY).map(String::as_str))
            .or_else(|| parse_valid_account_id(settings.get("anilist_user_id").map(String::as_str)))
    }

    pub(crate) fn get_scoped_preference(
        &self,
        db_key: &str,
        has_token: bool,
    ) -> Result<Option<String>> {
        let c = self.0.lock();
        let raw_owner: Option<String> = c
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [LIST_OWNER_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let owner = parse_valid_account_id(raw_owner.as_deref());
        if raw_owner.is_some() && owner.is_none() {
            return Ok(None);
        }
        let raw_saved: Option<String> = c
            .query_row(
                "SELECT value FROM settings WHERE key = 'anilist_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let saved = parse_valid_account_id(raw_saved.as_deref());
        if owner.is_some() && (raw_saved.is_some() || has_token) && owner != saved {
            return Ok(None);
        }
        Ok(
            c.query_row("SELECT value FROM settings WHERE key = ?", [db_key], |r| {
                r.get(0)
            })
            .optional()?,
        )
    }

    pub(crate) fn set_scoped_preference(
        &self,
        db_key: &str,
        value: Option<&str>,
        account: Option<i64>,
        has_token: bool,
    ) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        let saved: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'anilist_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let raw_owner: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [LIST_OWNER_KEY],
                |r| r.get(0),
            )
            .optional()?;
        let saved = parse_valid_account_id(saved.as_deref());
        let owner = parse_valid_account_id(raw_owner.as_deref());
        let verified = account.filter(|id| crate::anilist::valid_account_id(*id));
        let allowed = match verified {
            Some(id) => has_token && saved == Some(id),
            None => saved.is_none() && raw_owner.is_none(),
        };
        if !allowed {
            return Ok(());
        }
        // Wipe all scoped keys before claiming ownership over a foreign or corrupt owner record.
        if verified.is_some_and(|id| raw_owner.is_some() && owner != Some(id)) {
            tx.execute(
                "DELETE FROM settings WHERE key IN ('native_preference:kurisu.list.q',
                 'native_preference:kurisu.list.filter', 'native_preference:kurisu.list.synced')",
                [],
            )?;
        }
        match value {
            Some(value) => {
                tx.execute(
                    "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                    [db_key, value],
                )?;
            }
            None => {
                tx.execute("DELETE FROM settings WHERE key = ?", [db_key])?;
            }
        }
        if let Some(id) = verified {
            let id_str = id.to_string();
            if raw_owner.as_deref() != Some(id_str.as_str()) {
                tx.execute(
                    "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                    [LIST_OWNER_KEY, id_str.as_str()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn preserve_list_owner(&self) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [LIST_OWNER_KEY],
                |r| r.get(0),
            )
            .optional()?;
        if parse_valid_account_id(existing.as_deref()).is_some() {
            return Ok(());
        }
        let saved: Option<String> = tx
            .query_row(
                "SELECT value FROM settings WHERE key = 'anilist_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if existing.is_none() {
            if let Some(id) = parse_valid_account_id(saved.as_deref()) {
                tx.execute(
                    "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                    [LIST_OWNER_KEY, id.to_string().as_str()],
                )?;
                tx.commit()?;
                return Ok(());
            }
        }
        // Corrupt or unowned scoped preferences must not carry over to the next session.
        tx.execute(
            "DELETE FROM settings WHERE key IN ('native_preference:kurisu.list.q',
             'native_preference:kurisu.list.filter', 'native_preference:kurisu.list.synced', ?)",
            [LIST_OWNER_KEY],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn saved_token(&self) -> Result<Option<String>> {
        let token = self
            .0
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ? AND value != ''
                 AND NOT EXISTS (SELECT 1 FROM settings WHERE key = ?)",
                [TOKEN_KEY, TOKEN_REJECTED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(token)
    }

    pub fn cached_user(&self) -> Result<Option<crate::models::User>> {
        let settings = self.get_settings_batch(&["anilist_user_id", "anilist_profile"])?;
        let user = settings
            .get("anilist_profile")
            .and_then(|value| serde_json::from_str::<crate::models::User>(value).ok())
            .filter(|user| {
                crate::anilist::valid_account_id(user.id)
                    && settings.get("anilist_user_id") == Some(&user.id.to_string())
            });
        Ok(user)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
            [key, value],
        )?;
        Ok(())
    }
    pub fn set_settings(&self, kvs: &[(&str, &str)]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        for (k, v) in kvs {
            let written = tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [k, v],
            )?;
            anyhow::ensure!(written == 1, "setting {k} was not written");
        }
        tx.commit()?;
        Ok(())
    }
    /// Scrub freed pages and the WAL as well as the live table.
    pub fn scrub_setting(&self, key: &str) -> Result<()> {
        self.durable_write(|tx| Self::delete_setting_from(tx, key))?;
        let c = self.0.lock();
        // VACUUM must run outside a transaction.
        c.execute_batch("VACUUM")?;
        // A busy checkpoint leaves token pages in the WAL. Report the failed scrub.
        let busy: i64 = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(anyhow::anyhow!(
                "the WAL checkpoint came back busy, the sidecar was not truncated"
            ));
        }
        Ok(())
    }
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }
    pub(crate) fn get_list_sort_preferences(&self) -> [Result<Option<String>>; 2] {
        self.0.lock().query_row(
            "SELECT (SELECT value FROM settings WHERE key = 'native_preference:kurisu.list.sort'),
                    (SELECT value FROM settings WHERE key = 'native_preference:kurisu.list.dir')",
            [],
            |row| Ok([row.get(0).map_err(Into::into), row.get(1).map_err(Into::into)]),
        ).unwrap_or_else(|error| [Err(anyhow::anyhow!(error.to_string())), Err(error.into())])
    }
    /// Read under one lock so a settings update cannot split the snapshot.
    pub fn get_settings_batch(
        &self,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<String, String>> {
        let c = self.0.lock();
        let mut out = std::collections::HashMap::new();
        for k in keys {
            if let Some(v) = c
                .query_row("SELECT value FROM settings WHERE key = ?", [*k], |r| {
                    r.get::<_, String>(0)
                })
                .optional()?
            {
                out.insert((*k).to_string(), v);
            }
        }
        Ok(out)
    }
    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM settings WHERE key = ?", [key])?;
        Ok(())
    }

    pub(crate) fn delete_setting_from(conn: &Connection, key: &str) -> Result<()> {
        conn.execute("DELETE FROM settings WHERE key = ?", [key])?;
        let remains: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key = ?)",
            [key],
            |row| row.get(0),
        )?;
        anyhow::ensure!(!remains, "the saved setting was not deleted");
        Ok(())
    }

    pub fn clear_entries(&self) -> Result<()> {
        self.0.lock().execute("DELETE FROM list_entry", [])?;
        Ok(())
    }

    pub fn rss_seen_set(&self) -> Result<std::collections::HashSet<String>> {
        let c = self.0.lock();
        let mut stmt = c.prepare("SELECT guid FROM rss_seen")?;
        let set = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(Result::ok)
            .collect();
        Ok(set)
    }

    pub fn mark_rss_seen(&self, guids: &[String]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        let now = chrono::Utc::now().timestamp();
        for g in guids {
            tx.execute(
                "INSERT OR REPLACE INTO rss_seen (guid, seen_at) VALUES (?,?)",
                rusqlite::params![g, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune_rss_seen_keeping(&self, days: i64, keep: &[String]) -> Result<()> {
        let cutoff = chrono::Utc::now().timestamp() - days * 86_400;
        let keep: Vec<_> = keep
            .iter()
            .flat_map(|guid| crate::show_torrents::seen_guid_aliases(guid))
            .collect();
        self.0.lock().execute(
            "DELETE FROM rss_seen WHERE seen_at < ? AND guid NOT IN (SELECT value FROM json_each(?))",
            rusqlite::params![cutoff, serde_json::to_string(&keep)?],
        )?;
        Ok(())
    }

    pub fn prune_media_cache(&self, days: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - days * 86_400;
        let n = self.0.lock().execute(
            "DELETE FROM media WHERE cached_at < ?
             AND id NOT IN (SELECT media_id FROM list_entry)
             AND id NOT IN (SELECT media_id FROM pending_change)
             AND id NOT IN (SELECT media_id FROM watch_history)",
            [cutoff],
        )?;
        Ok(n)
    }
}

fn mark_catalog_reads(
    reads: &mut Vec<Weak<CatalogRead>>,
    source: Option<&CatalogRead>,
    update: CatalogUpdate,
    media_ids: impl Iterator<Item = i64> + Clone,
) {
    // Reads register in request order. Confirmed lookup totals have no catalog source.
    let mut older = true;
    reads.retain(|read| {
        let Some(read) = read.upgrade() else {
            return false;
        };
        if source.is_some_and(|source| std::ptr::eq(read.as_ref(), source)) {
            older = false;
        }
        if older {
            let mut newer = read.newer.lock();
            let ids = media_ids.clone();
            match update {
                CatalogUpdate::Episodes => newer.episodes.extend(ids),
                CatalogUpdate::Metadata => newer.metadata.extend(ids),
                CatalogUpdate::Detail => newer.detail.extend(ids),
                CatalogUpdate::Section(section) => {
                    newer.sections.extend(ids.map(|id| (id, section)))
                }
            }
        }
        true
    });
}

fn write_episode_total(conn: &Connection, media_id: i64, episodes: Option<i64>) -> Result<bool> {
    let episodes = episodes.filter(|total| *total > 0);
    let changed = cached_episode_total(conn, media_id)? != Some(episodes);
    if changed {
        let written = conn.execute(
            "INSERT INTO media (id,episodes,cached_at) VALUES (?,?,?)
                 ON CONFLICT(id) DO UPDATE SET episodes=excluded.episodes",
            rusqlite::params![media_id, episodes, chrono::Utc::now().timestamp()],
        )?;
        anyhow::ensure!(written == 1, "the confirmed episode total was not cached");
    }
    Ok(changed)
}

fn cached_episode_total(conn: &Connection, id: i64) -> Result<Option<Option<i64>>> {
    Ok(conn
        .query_row("SELECT episodes FROM media WHERE id = ?", [id], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .map(|episodes| episodes.filter(|total| *total > 0)))
}

fn read_media_row(conn: &Connection, id: i64) -> Result<Option<(Media, u8)>> {
    Ok(conn
        .prepare_cached(
            "SELECT id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
                    episodes,format,status,average_score,season,season_year,description,
                    next_airing_episode,next_airing_at,banner_image,genres,duration,source,studios,
                    catalog_fields
             FROM media WHERE id = ?",
        )?
        .query_row([id], |row| Ok((row_to_media(row)?, row.get(21)?)))
        .optional()?)
}

/// Preserve cached detail fields that lean queries never fetch.
fn upsert_media_row(c: &Connection, m: &Media) -> Result<()> {
    let written = c
        .prepare_cached(
            "INSERT INTO media
         (id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
          episodes,format,status,average_score,season,season_year,description,
          next_airing_episode,next_airing_at,banner_image,genres,duration,source,studios,
          cached_at,catalog_fields)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT(id) DO UPDATE SET
          id_mal=excluded.id_mal, title_romaji=excluded.title_romaji,
          title_english=excluded.title_english, title_native=excluded.title_native,
          cover_medium=excluded.cover_medium, cover_large=excluded.cover_large,
          episodes=excluded.episodes, format=excluded.format, status=excluded.status,
          average_score=excluded.average_score, season=excluded.season,
          season_year=excluded.season_year, description=excluded.description,
          next_airing_episode=excluded.next_airing_episode,
          next_airing_at=excluded.next_airing_at,
          banner_image=COALESCE(excluded.banner_image, banner_image),
          genres=COALESCE(excluded.genres, genres),
          duration=COALESCE(excluded.duration, duration),
          source=COALESCE(excluded.source, source),
          studios=COALESCE(excluded.studios, studios),
          cached_at=excluded.cached_at,
          catalog_fields=MAX(catalog_fields, excluded.catalog_fields)",
        )?
        .execute(rusqlite::params![
            m.id,
            m.id_mal,
            m.title_romaji,
            m.title_english,
            m.title_native,
            m.cover_medium,
            m.cover_large,
            m.episodes.filter(|total| *total > 0),
            m.format,
            m.status,
            m.average_score,
            m.season,
            m.season_year,
            m.description,
            m.next_airing_episode,
            m.next_airing_at,
            m.banner_image,
            m.genres
                .as_ref()
                .and_then(|g| serde_json::to_string(g).ok()),
            m.duration,
            m.source,
            m.studios
                .as_ref()
                .and_then(|s| serde_json::to_string(s).ok()),
            chrono::Utc::now().timestamp(),
            CATALOG_METADATA,
        ])?;
    anyhow::ensure!(written == 1, "the media metadata was not cached");
    Ok(())
}

fn row_to_media(r: &rusqlite::Row) -> rusqlite::Result<Media> {
    row_to_media_offset(r, 0)
}

fn row_to_media_offset(r: &rusqlite::Row, o: usize) -> rusqlite::Result<Media> {
    let json_vec = |v: Option<String>| -> Option<Vec<String>> {
        v.and_then(|s| serde_json::from_str(&s).ok())
    };
    Ok(Media {
        id: r.get(o)?,
        id_mal: r.get(o + 1)?,
        title_romaji: r.get(o + 2)?,
        title_english: r.get(o + 3)?,
        title_native: r.get(o + 4)?,
        cover_medium: r.get(o + 5)?,
        cover_large: r.get(o + 6)?,
        episodes: r.get::<_, Option<i64>>(o + 7)?.filter(|total| *total > 0),
        format: r.get(o + 8)?,
        status: r.get(o + 9)?,
        average_score: r.get(o + 10)?,
        season: r.get(o + 11)?,
        season_year: r.get(o + 12)?,
        description: r.get(o + 13)?,
        next_airing_episode: r.get(o + 14)?,
        next_airing_at: r.get(o + 15)?,
        banner_image: r.get(o + 16)?,
        genres: json_vec(r.get(o + 17)?),
        duration: r.get(o + 18)?,
        source: r.get(o + 19)?,
        studios: json_vec(r.get(o + 20)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_format_changes_invalidate_cached_scores_without_changing_progress() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut user = crate::models::User {
            id: 7,
            name: "Tester".into(),
            score_format: Some("POINT_5".into()),
            ..Default::default()
        };
        db.replace_account("session", &user).unwrap();
        db.upsert_media(&crate::models::Media {
            id: 1,
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&crate::models::ListEntry {
            id: Some(22),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 3,
            score: Some(4.0),
            ..Default::default()
        })
        .unwrap();
        db.replace_account("session", &user).unwrap();
        assert_eq!(db.get_entry(1).unwrap().unwrap().score, Some(4.0));
        user.score_format = Some("POINT_100".into());
        assert!(!db.replace_account("session", &user).unwrap());
        let entry = db.get_entry(1).unwrap().unwrap();
        assert_eq!(entry.score, None);
        assert_eq!(
            (entry.id, entry.progress, entry.status.as_str()),
            (Some(22), 3, "CURRENT")
        );
    }

    #[test]
    fn profile_format_changes_preserve_known_unrated_and_unknown_scores() {
        for score in [None, Some(0.0)] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            let mut user = crate::models::User {
                id: 7,
                score_format: Some("POINT_100".into()),
                ..Default::default()
            };
            db.replace_account("session", &user).unwrap();
            db.upsert_media(&crate::models::Media {
                id: 1,
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: 1,
                status: "CURRENT".into(),
                score,
                ..Default::default()
            })
            .unwrap();
            user.score_format = Some("POINT_5".into());
            db.replace_account("session", &user).unwrap();
            assert_eq!(db.get_entry(1).unwrap().unwrap().score, score);
        }
    }

    #[test]
    fn account_change_rolls_back_identity_and_rows_on_any_write_failure() {
        for trigger in [
            "CREATE TRIGGER fail BEFORE DELETE ON list_entry BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            "CREATE TRIGGER fail BEFORE INSERT ON settings WHEN NEW.key = 'anilist_username' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            "CREATE TRIGGER fail BEFORE INSERT ON settings WHEN NEW.key = 'anilist_profile' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        ] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            let a = crate::models::User { id: 1, name: "A".into(), ..Default::default() };
            let b = crate::models::User { id: 2, name: "B".into(), ..Default::default() };
            db.replace_account("old", &a).unwrap();
            db.upsert_entry(&ListEntry { media_id: 1, progress: 8, ..Default::default() }).unwrap();
            db.0.lock().execute_batch(trigger).unwrap();
            assert!(db.replace_account("new", &b).is_err());
            assert_eq!(db.get_setting("anilist_token").unwrap().as_deref(), Some("old"));
            assert_eq!(db.get_setting("anilist_username").unwrap().as_deref(), Some("A"));
            assert_eq!(db.get_setting("anilist_user_id").unwrap().as_deref(), Some("1"));
            assert_eq!(db.cached_user().unwrap().unwrap().id, 1);
            assert_eq!(db.get_entry(1).unwrap().unwrap().progress, 8);
            db.0.lock().execute_batch("DROP TRIGGER fail").unwrap();
            assert!(db.replace_account("new", &b).unwrap());
            assert!(db.entries_with_media().unwrap().is_empty());
        }
    }

    #[test]
    fn native_list_preferences_only_adopt_keys_when_owner_is_absent() {
        for owner in [None, Some("corrupt"), Some("0")] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            if let Some(owner) = owner {
                db.set_setting(LIST_OWNER_KEY, owner).unwrap();
            }
            for key in SCOPED_LIST_PREFERENCE_KEYS {
                db.set_setting(key, "old preference").unwrap();
            }
            assert!(db.get_setting("anilist_user_id").unwrap().is_none());
            db.replace_account_with_last_account(
                "new-token",
                &crate::User {
                    id: 7,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            for key in SCOPED_LIST_PREFERENCE_KEYS {
                assert_eq!(
                    db.get_setting(key).unwrap().as_deref(),
                    owner.is_none().then_some("old preference"),
                    "owner={owner:?}, key={key}"
                );
            }
            assert_eq!(db.saved_list_owner(), Some(7));
        }
    }

    #[test]
    fn native_list_preferences_follow_only_a_verified_replacement() {
        let keys = [
            "native_preference:kurisu.list.q",
            "native_preference:kurisu.list.filter",
            "native_preference:kurisu.list.synced",
        ];
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let user = crate::User {
            id: 7,
            ..Default::default()
        };
        let other = crate::User {
            id: 8,
            ..Default::default()
        };
        for key in keys {
            db.set_setting(key, "original").unwrap();
        }
        db.set_setting("native_preference:kurisu.list.sort", "title")
            .unwrap();
        let assert_preferences = |value| {
            for key in keys {
                assert_eq!(db.get_setting(key).unwrap().as_deref(), value);
            }
        };
        db.replace_account_with_last_account("first", &user, None)
            .unwrap();
        assert_preferences(Some("original"));
        db.delete_setting("anilist_user_id").unwrap();
        db.replace_account_with_last_account("same", &user, Some(user.id))
            .unwrap();
        assert_preferences(Some("original"));

        for trigger in [
            "CREATE TRIGGER fail_reset BEFORE DELETE ON settings
             WHEN OLD.key='native_preference:kurisu.list.filter'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            "CREATE TRIGGER fail_reset BEFORE INSERT ON settings
             WHEN NEW.key='anilist_profile'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        ] {
            db.0.lock().execute_batch(trigger).unwrap();
            let error = db
                .replace_account_with_last_account("other", &other, None)
                .unwrap_err();
            assert!(error.to_string().contains("injected failure"));
            assert_eq!(
                db.get_setting("anilist_token").unwrap().as_deref(),
                Some("same")
            );
            assert_eq!(db.cached_user().unwrap().unwrap().id, user.id);
            assert_preferences(Some("original"));
            db.0.lock()
                .execute_batch("DROP TRIGGER fail_reset")
                .unwrap();
        }
        db.delete_setting("anilist_user_id").unwrap();
        db.replace_account_with_last_account("same-after-restart", &user, None)
            .unwrap();
        assert_preferences(Some("original"));
        db.delete_setting("anilist_user_id").unwrap();
        db.replace_account_with_last_account("other-after-restart", &other, None)
            .unwrap();
        assert_preferences(None);
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.sort")
                .unwrap()
                .as_deref(),
            Some("title")
        );
    }

    #[test]
    fn cached_user_rejects_invalid_account_ids_even_with_matching_profile_json() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for invalid in [0, -1, i64::from(i32::MAX) + 1] {
            let user = crate::User {
                id: invalid,
                name: "Tester".into(),
                score_format: Some("POINT_10".into()),
                ..Default::default()
            };
            db.set_settings(&[
                ("anilist_user_id", &invalid.to_string()),
                ("anilist_profile", &serde_json::to_string(&user).unwrap()),
            ])
            .unwrap();
            assert!(db.cached_user().unwrap().is_none(), "invalid={invalid}");
        }
    }

    #[test]
    fn account_ownership_uses_ids_and_clears_unknown_owners() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut user = crate::models::User {
            id: 1,
            name: "A".into(),
            ..Default::default()
        };
        db.upsert_entry(&ListEntry {
            media_id: 1,
            progress: 8,
            ..Default::default()
        })
        .unwrap();
        db.set_setting("anilist_username", "A").unwrap();
        assert!(db.replace_account("first", &user).unwrap());
        assert!(db.get_entry(1).unwrap().is_none());
        db.upsert_entry(&ListEntry {
            media_id: 1,
            progress: 8,
            ..Default::default()
        })
        .unwrap();
        user.name = "Renamed".into();
        assert!(!db.replace_account("second", &user).unwrap());
        assert_eq!(db.get_entry(1).unwrap().unwrap().progress, 8);
        user.id = 2;
        assert!(db.replace_account("third", &user).unwrap());
        assert!(db.get_entry(1).unwrap().is_none());
        assert_eq!(db.cached_user().unwrap().unwrap().id, 2);
        db.set_setting("anilist_user_id", "1").unwrap();
        assert!(db.cached_user().unwrap().is_none());
    }

    #[test]
    fn replace_list_snapshot_lands_and_reconciles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_entry(&ListEntry {
            id: Some(1),
            media_id: 1,
            status: "CURRENT".into(),
            progress: 3,
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            id: Some(2),
            media_id: 2,
            status: "PLANNING".into(),
            ..Default::default()
        })
        .unwrap();
        let snapshot = vec![
            ListEntry {
                id: Some(11),
                media_id: 1,
                status: "CURRENT".into(),
                progress: 7,
                score: Some(9.0),
                repeat: 0,
                updated_at: Some(5),
                media: Some(Media {
                    id: 1,
                    episodes: Some(12),
                    title_english: Some("Fixture Title".into()),
                    ..Default::default()
                }),
                queued_creation: None,
            },
            ListEntry {
                id: Some(12),
                media_id: 3,
                status: "COMPLETED".into(),
                progress: 24,
                ..Default::default()
            },
        ];
        db.replace_list_snapshot(&snapshot).unwrap();
        let rows = db.entries_with_media().unwrap();
        assert_eq!(rows.len(), 2, "row 2 was reconciled away");
        let one = db.get_entry(1).unwrap().unwrap();
        assert_eq!((one.id, one.progress, one.score), (Some(11), 7, Some(9.0)));
        assert_eq!(one.media.as_ref().and_then(|m| m.episodes), Some(12));
        assert!(db.get_entry(2).unwrap().is_none());
        assert!(db.get_entry(3).unwrap().is_some());
        db.replace_list_snapshot(&[]).unwrap();
        assert!(db.entries_with_media().unwrap().is_empty());
    }

    #[test]
    fn replace_list_snapshot_skips_unchanged_entries() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut entries: Vec<_> = (0..1000)
            .map(|media_id| ListEntry {
                media_id,
                id: Some(media_id + 1000),
                status: "CURRENT".into(),
                progress: 3,
                score: (media_id % 2 == 0).then_some(8.5),
                updated_at: Some(123),
                ..Default::default()
            })
            .collect();
        db.replace_list_snapshot(&entries).unwrap();
        let changes = || db.0.lock().total_changes();
        let before = changes();
        db.replace_list_snapshot(&entries).unwrap();
        assert_eq!(changes() - before, 0, "unchanged entries need no writes");

        entries[0].id = None;
        entries[1].status = "PLANNING".into();
        entries[2].progress = 4;
        entries[3].score = Some(7.5);
        entries[4].repeat = 1;
        entries[5].updated_at = None;
        let before = changes();
        db.replace_list_snapshot(&entries).unwrap();
        assert_eq!(changes() - before, 6);
        for e in &entries[..6] {
            assert_eq!(
                serde_json::to_value(db.get_entry(e.media_id).unwrap().unwrap()).unwrap(),
                serde_json::to_value(e).unwrap()
            );
        }

        entries[0].id = Some(2000);
        entries[3].score = None;
        entries[5].updated_at = Some(456);
        entries.pop();
        entries.push(ListEntry {
            media_id: 1000,
            status: "COMPLETED".into(),
            progress: 12,
            ..Default::default()
        });
        let before = changes();
        db.replace_list_snapshot(&entries).unwrap();
        assert_eq!(
            changes() - before,
            5,
            "three updates, one insert, one deletion"
        );
        assert!(db.get_entry(999).unwrap().is_none());
        assert_eq!(
            serde_json::to_value(db.entries_with_media().unwrap()).unwrap(),
            serde_json::to_value(&entries).unwrap()
        );

        let mut duplicate = entries[0].clone();
        duplicate.progress = 8;
        entries.push(duplicate.clone());
        db.replace_list_snapshot(&entries).unwrap();
        assert_eq!(db.get_entry(0).unwrap().unwrap().progress, 8);
        assert_eq!(db.entries_with_media().unwrap().len(), 1000);
    }

    #[test]
    fn replace_list_snapshot_refreshes_media_without_rewriting_entries() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let mut entries: Vec<_> = (0..1000)
            .map(|id| ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                media: Some(Media {
                    id,
                    id_mal: Some(id + 1000),
                    title_romaji: Some(format!("Show {id}")),
                    description: Some("Cached description".into()),
                    next_airing_episode: Some(4),
                    next_airing_at: Some(1234),
                    banner_image: Some("banner.jpg".into()),
                    genres: Some(vec!["Action".into()]),
                    duration: Some(24),
                    source: Some("MANGA".into()),
                    studios: Some(vec!["Studio".into()]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .collect();
        db.replace_list_snapshot(&entries).unwrap();
        let mut expected = entries[0].media.clone().unwrap();
        expected.id_mal = None;
        expected.description = None;
        expected.next_airing_episode = None;
        expected.next_airing_at = None;
        for e in &mut entries {
            e.media = Some(Media {
                id: e.media_id,
                title_romaji: Some(format!("Show {}", e.media_id)),
                ..Default::default()
            });
        }
        db.0.lock()
            .execute("UPDATE media SET cached_at = 1", [])
            .unwrap();
        let before = db.0.lock().total_changes();
        let started = chrono::Utc::now().timestamp();
        db.replace_list_snapshot(&entries).unwrap();
        assert_eq!(db.0.lock().total_changes() - before, 1000);
        assert_eq!(
            serde_json::to_value(db.get_media(0).unwrap().unwrap()).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        let (oldest, newest): (i64, i64) =
            db.0.lock()
                .query_row(
                    "SELECT min(cached_at), max(cached_at) FROM media",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
        assert!(oldest >= started);
        assert!(newest <= chrono::Utc::now().timestamp());
        db.replace_list_snapshot(&[]).unwrap();
        assert_eq!(db.prune_media_cache(30).unwrap(), 0);
        assert!(db.get_media(999).unwrap().is_some());
    }

    #[test]
    fn replace_list_snapshot_rolls_back_updates_and_deletions_together() {
        for trigger in [
            "CREATE TRIGGER fail BEFORE UPDATE ON media WHEN NEW.id = 2 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            "CREATE TRIGGER fail BEFORE INSERT ON list_entry WHEN NEW.media_id = 2 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            "CREATE TRIGGER fail BEFORE DELETE ON list_entry WHEN OLD.media_id = 3 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        ] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            let mut entries: Vec<_> = (1..=3).map(|id| ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                media: Some(Media {
                    id,
                    title_romaji: Some(format!("Show {id}")),
                    ..Default::default()
                }),
                ..Default::default()
            }).collect();
            db.replace_list_snapshot(&entries).unwrap();
            db.0.lock().execute("UPDATE media SET cached_at = 1", []).unwrap();
            let before = serde_json::to_value(db.entries_with_media().unwrap()).unwrap();
            entries.pop();
            for e in &mut entries {
                e.progress = 4;
                e.media.as_mut().unwrap().title_romaji = Some("Changed".into());
            }
            db.0.lock().execute_batch(trigger).unwrap();
            assert!(db.replace_list_snapshot(&entries).is_err());
            assert_eq!(serde_json::to_value(db.entries_with_media().unwrap()).unwrap(), before);
            let refreshed: i64 = db.0.lock().query_row(
                "SELECT count(*) FROM media WHERE cached_at != 1", [], |r| r.get(0),
            ).unwrap();
            assert_eq!(refreshed, 0);
            db.0.lock().execute_batch("DROP TRIGGER fail").unwrap();
            db.replace_list_snapshot(&entries).unwrap();
            assert_eq!(db.get_entry(1).unwrap().unwrap().progress, 4);
            assert!(db.get_entry(3).unwrap().is_none());
        }
    }

    /// Keep this historical schema independent of the current migrations.
    const V1_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS media (
        id              INTEGER PRIMARY KEY,
        id_mal          INTEGER,
        title_romaji    TEXT,
        title_english   TEXT,
        title_native    TEXT,
        cover_medium    TEXT,
        cover_large     TEXT,
        episodes        INTEGER,
        format          TEXT,
        status          TEXT,
        average_score   INTEGER,
        season          TEXT,
        season_year     INTEGER,
        description     TEXT,
        cached_at       INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS list_entry (
        media_id    INTEGER PRIMARY KEY,
        entry_id    INTEGER,
        status      TEXT NOT NULL,
        progress    INTEGER NOT NULL DEFAULT 0,
        score       REAL,
        repeat      INTEGER NOT NULL DEFAULT 0,
        updated_at  INTEGER
    );
    CREATE TABLE IF NOT EXISTS watched_file (
        path        TEXT PRIMARY KEY,
        media_id    INTEGER NOT NULL,
        episode     INTEGER NOT NULL,
        watched_at  INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );";

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kurisu-mig-{}-{}.db", tag, std::process::id()))
    }

    fn build_db_at_version(path: &std::path::Path, version: i64) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        if version >= 2 {
            conn.execute_batch(
                "ALTER TABLE media ADD COLUMN next_airing_episode INTEGER;
                 ALTER TABLE media ADD COLUMN next_airing_at INTEGER;",
            )
            .unwrap();
        }
        if version >= 3 {
            conn.execute_batch(
                "ALTER TABLE media ADD COLUMN banner_image TEXT;
                 ALTER TABLE media ADD COLUMN genres TEXT;
                 ALTER TABLE media ADD COLUMN duration INTEGER;
                 ALTER TABLE media ADD COLUMN source TEXT;
                 ALTER TABLE media ADD COLUMN studios TEXT;",
            )
            .unwrap();
        }
        if version >= 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS rss_seen (
                guid    TEXT PRIMARY KEY,
                seen_at INTEGER NOT NULL
            );",
            )
            .unwrap();
        }
        if version >= 5 {
            conn.execute_batch(
                "CREATE TABLE pending_change (
                    account_id INTEGER NOT NULL, media_id INTEGER NOT NULL,
                    payload TEXT NOT NULL, PRIMARY KEY(account_id, media_id)
                );
                CREATE TABLE watch_history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    account_id INTEGER NOT NULL, path TEXT NOT NULL,
                    media_id INTEGER NOT NULL, episode INTEGER NOT NULL,
                    watched_at INTEGER NOT NULL
                );",
            )
            .unwrap();
        }
        if version >= 6 {
            conn.execute_batch(
                "CREATE INDEX watch_history_duplicate
                 ON watch_history (account_id,path,media_id,episode,watched_at);",
            )
            .unwrap();
        }
        if version >= 7 {
            conn.execute_batch(
                "CREATE INDEX watch_history_account_id ON watch_history (account_id,id);",
            )
            .unwrap();
        }
        if version >= 8 {
            conn.execute_batch("ALTER TABLE list_entry ADD COLUMN queued_creation TEXT;")
                .unwrap();
        }
        conn.execute(
            "INSERT INTO media (id,title_romaji,episodes,cached_at) VALUES (7,'Old Show',12,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO list_entry (media_id,entry_id,status,progress,score,repeat,updated_at)
             VALUES (7,77,'CURRENT',3,7.5,0,111)",
            [],
        )
        .unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {version}"))
            .unwrap();
    }

    #[test]
    fn migrations_upgrade_every_historical_version() {
        for from in 1..SCHEMA_VERSION {
            let path = temp_db_path(&format!("v{from}"));
            let _ = std::fs::remove_file(&path);
            build_db_at_version(&path, from);
            let db = Db::open(&path).unwrap();
            let version: i64 =
                db.0.lock()
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
            assert_eq!(version, SCHEMA_VERSION, "upgrade from v{from} stalled");
            let m = db.get_media(7).unwrap().expect("media row survived");
            assert_eq!(m.title_romaji.as_deref(), Some("Old Show"));
            assert_eq!(m.episodes, Some(12));
            let e = db.get_entry(7).unwrap().expect("entry row survived");
            assert_eq!(
                (e.status.as_str(), e.progress, e.score),
                ("CURRENT", 3, Some(7.5))
            );
            assert_eq!(e.id, Some(77));
            assert_eq!(e.updated_at, Some(111));
            db.rss_seen_set().unwrap();
            db.put_pending(
                7,
                &crate::sync_queue::Pending {
                    media_id: 7,
                    deleting: false,
                    deletion_id: None,
                    base: Some(e),
                    patch: crate::sync_queue::Patch {
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
                },
            )
            .unwrap();
            db.record_watch(7, "Old Show - 04.mkv", 7, 4).unwrap();
            drop(db);
            let db = Db::open(&path).unwrap();
            let pending = db.pending(7).unwrap();
            assert_eq!(
                pending.len(),
                1,
                "upgrade from v{from} lost pending changes"
            );
            assert_eq!(pending[0].media_id, 7);
            assert_eq!(pending[0].patch.progress, Some(4));
            assert_eq!(pending[0].base.as_ref().unwrap().progress, 3);
            let history = db.watch_history(7, 50, None).unwrap();
            assert_eq!(history.len(), 1, "upgrade from v{from} lost watch history");
            assert_eq!(history[0].path, "Old Show - 04.mkv");
            assert_eq!(history[0].media_id, 7);
            assert_eq!(history[0].episode, 4);
            assert_eq!(history[0].title, "Old Show");
            drop(db);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn reopened_catalog_distinguishes_explicit_nulls_from_episode_only_rows() {
        let path = temp_db_path("catalog-null-metadata");
        let _ = std::fs::remove_file(&path);
        build_db_at_version(&path, 8);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.get_media(7).unwrap().unwrap().display_title(),
            "Old Show"
        );
        db.upsert_media(&Media {
            id: 10,
            ..Default::default()
        })
        .unwrap();
        for id in [10, 11] {
            db.cache_episode_total(id, Some(12)).unwrap();
        }
        drop(db);

        let db = Db::open(&path).unwrap();
        for (id, title) in [(10, "#10"), (11, "Saved title")] {
            let saved = Media {
                id,
                title_english: Some("Saved title".into()),
                ..Default::default()
            };
            let mut detail = saved.clone();
            db.restore_cached_metadata(&mut detail);
            assert_eq!(detail.display_title(), title);
            assert_eq!(detail.episodes, Some(12));
            let mut relations = vec![crate::models::MediaRelation {
                relation: "PREQUEL".into(),
                media: saved,
            }];
            db.restore_cached_relations(&mut relations);
            assert_eq!(relations[0].media.display_title(), title);
            assert_eq!(relations[0].media.episodes, Some(12));
        }
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn history_index_migration_preserves_existing_rows_and_pending_intent() {
        for from in [5, 6] {
            check_history_index_migration(from);
        }
    }

    fn check_history_index_migration(from: i64) {
        let path = temp_db_path(&format!("history-index-v{from}"));
        let _ = std::fs::remove_file(&path);
        build_db_at_version(&path, from);
        let payload = serde_json::to_string(&crate::sync_queue::Pending {
            media_id: 7,
            deleting: false,
            deletion_id: None,
            base: None,
            patch: crate::sync_queue::Patch {
                progress: Some(4),
                ..Default::default()
            },
            error: Some("synthetic outage".into()),
            conflict: false,
            missing_media: false,
            score_format: None,
            playback: false,
            base_score_format: None,
            rejected_progress: None,
            retry_error: None,
            rejected_total: None,
        })
        .unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "INSERT INTO watch_history (id,account_id,path,media_id,episode,watched_at) VALUES
                    (41,7,'episode.mkv',7,4,1000),
                    (42,7,'episode.mkv',7,4,1000),
                    (43,9,'episode.mkv',7,4,1000);
                 INSERT INTO watched_file VALUES ('legacy.mkv',7,3,900);
                 INSERT INTO settings VALUES ('anilist_user_id','7');",
            )
            .unwrap();
            conn.execute("INSERT INTO pending_change VALUES (7,7,?)", [&payload])
                .unwrap();
        }
        let original: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let mut previous_key = None;
        for _ in 0..2 {
            let db = Db::open(&path).unwrap();
            let conn = db.0.lock();
            let rows = conn
                .prepare("SELECT id,account_id,path,media_id,episode,watched_at FROM watch_history ORDER BY id")
                .unwrap()
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?)))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                rows,
                vec![
                    (41, 7, "episode.mkv".into(), 7, 4, 1000),
                    (42, 7, "episode.mkv".into(), 7, 4, 1000),
                    (43, 9, "episode.mkv".into(), 7, 4, 1000),
                ]
            );
            let migrated: String = conn
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=7 AND media_id=7",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let mut migrated: serde_json::Value = serde_json::from_str(&migrated).unwrap();
            let key = migrated["patch"]
                .as_object_mut()
                .unwrap()
                .remove("creation_key")
                .unwrap();
            assert_eq!(migrated, original);
            assert_eq!(key.as_str().unwrap().len(), 32);
            if let Some(previous) = previous_key.replace(key.clone()) {
                assert_eq!(key, previous);
            }
            assert_eq!(
                conn.query_row(
                    "SELECT watched_at FROM watched_file WHERE path='legacy.mkv'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                900
            );
            assert_eq!(
                conn.query_row(
                    "SELECT seq FROM sqlite_sequence WHERE name='watch_history'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                43
            );
            assert_eq!(conn.query_row("SELECT \"unique\" FROM pragma_index_list('watch_history') WHERE name='watch_history_duplicate'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            let columns = conn
                .prepare(
                    "SELECT name FROM pragma_index_info('watch_history_duplicate') ORDER BY seqno",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                columns,
                ["account_id", "path", "media_id", "episode", "watched_at"]
            );
            assert_eq!(conn.query_row("SELECT \"unique\" FROM pragma_index_list('watch_history') WHERE name='watch_history_account_id'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            let paging_columns = conn
                .prepare(
                    "SELECT name FROM pragma_index_info('watch_history_account_id') ORDER BY seqno",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(paging_columns, ["account_id", "id"]);
            drop(conn);
            assert_eq!(
                db.get_setting("anilist_user_id").unwrap().as_deref(),
                Some("7")
            );
            assert_eq!(db.pending(7).unwrap()[0].patch.progress, Some(4));
            assert_eq!(db.watch_history(7, 10, None).unwrap().len(), 2);
            assert_eq!(db.watch_history(9, 10, None).unwrap().len(), 1);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn future_schema_is_not_rewritten_or_downgraded() {
        let path = temp_db_path("future-history-index");
        let _ = std::fs::remove_file(&path);
        build_db_at_version(&path, SCHEMA_VERSION);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE future_state (value TEXT);
                 INSERT INTO future_state VALUES ('preserved');
                 DROP INDEX watch_history_account_id;
                 PRAGMA user_version = 999;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let conn = db.0.lock();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            999
        );
        assert_eq!(
            conn.query_row("SELECT value FROM future_state", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='watch_history_account_id'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='watch_history_duplicate'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        drop(conn);
        assert_eq!(db.get_entry(7).unwrap().unwrap().progress, 3);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn history_duplicate_lookup_uses_a_covering_range_without_scanning_history() {
        use rusqlite::StatementStatus;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let conn = db.0.lock();
        conn.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<50000)
             INSERT INTO watch_history (account_id,path,media_id,episode,watched_at)
             SELECT i%10,'episode-' || i || '.mkv',i%100,1,1000+i FROM n;",
        )
        .unwrap();
        let sql = "SELECT EXISTS(SELECT 1 FROM watch_history
                   WHERE account_id=? AND path=? AND media_id=? AND episode=? AND watched_at>?)";
        let plans = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(
                rusqlite::params![7, "episode-49997.mkv", 97, 1, 50996],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            plans.iter().any(|p| p
                .contains("SEARCH watch_history USING COVERING INDEX watch_history_duplicate")
                && p.contains("watched_at>?")),
            "{plans:?}"
        );
        let measure = |sql: &str, path: &str, cutoff: i64| {
            let mut stmt = conn.prepare(sql).unwrap();
            let exists: bool = stmt
                .query_row(rusqlite::params![7, path, 97, 1, cutoff], |r| r.get(0))
                .unwrap();
            (
                exists,
                stmt.get_status(StatementStatus::FullscanStep),
                stmt.get_status(StatementStatus::VmStep),
            )
        };
        let indexed = measure(sql, "episode-49997.mkv", 50996);
        let missing = measure(sql, "absent.mkv", 50996);
        let expired = measure(sql, "episode-49997.mkv", 50997);
        let unindexed = measure(
            &sql.replace("FROM watch_history", "FROM watch_history NOT INDEXED"),
            "episode-49997.mkv",
            50996,
        );
        assert!(indexed.0);
        assert!(!missing.0);
        assert!(!expired.0);
        assert_eq!((indexed.1, missing.1, expired.1), (0, 0, 0));
        assert!(unindexed.1 >= 49996);
        assert!(indexed.2 < unindexed.2 / 100);
        println!("history lookup over 50000 rows: indexed {indexed:?}, missing {missing:?}, expired {expired:?}, unindexed {unindexed:?}");
    }

    #[test]
    fn entries_with_media_maps_the_joined_columns() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&Media {
            id: 9,
            id_mal: Some(99),
            title_romaji: Some("Romaji Title".into()),
            title_english: Some("English Title".into()),
            title_native: Some("Native".into()),
            cover_medium: Some("m.jpg".into()),
            cover_large: Some("l.jpg".into()),
            episodes: Some(24),
            format: Some("TV".into()),
            status: Some("FINISHED".into()),
            average_score: Some(82),
            season: Some("WINTER".into()),
            season_year: Some(2024),
            description: Some("Long html".into()),
            next_airing_episode: Some(4),
            next_airing_at: Some(1234),
            banner_image: Some("banner.jpg".into()),
            genres: Some(vec!["Action".into()]),
            duration: Some(24),
            source: Some("MANGA".into()),
            studios: Some(vec!["MAPPA".into()]),
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            id: Some(55),
            media_id: 9,
            status: "REPEATING".into(),
            progress: 6,
            score: Some(8.5),
            repeat: 1,
            updated_at: Some(42),
            media: None,
            queued_creation: None,
        })
        .unwrap();
        let rows = db.entries_with_media().unwrap();
        assert_eq!(rows.len(), 1);
        let e = &rows[0];
        assert_eq!(
            (e.id, e.media_id, e.status.as_str(), e.progress, e.repeat),
            (Some(55), 9, "REPEATING", 6, 1)
        );
        assert_eq!((e.score, e.updated_at), (Some(8.5), Some(42)));
        let m = e.media.as_ref().expect("media joined");
        assert_eq!(m.id, 9);
        assert_eq!(m.id_mal, Some(99));
        assert_eq!(m.title_romaji.as_deref(), Some("Romaji Title"));
        assert_eq!(m.title_english.as_deref(), Some("English Title"));
        assert_eq!(m.title_native.as_deref(), Some("Native"));
        assert_eq!(m.cover_medium.as_deref(), Some("m.jpg"));
        assert_eq!(m.cover_large.as_deref(), Some("l.jpg"));
        assert_eq!(m.episodes, Some(24));
        assert_eq!(m.format.as_deref(), Some("TV"));
        assert_eq!(m.status.as_deref(), Some("FINISHED"));
        assert_eq!(m.average_score, Some(82));
        assert_eq!(m.season.as_deref(), Some("WINTER"));
        assert_eq!(m.season_year, Some(2024));
        assert_eq!(m.description, None);
        assert_eq!(
            db.get_media(9).unwrap().unwrap().description.as_deref(),
            Some("Long html")
        );
        assert_eq!(m.next_airing_episode, Some(4));
        assert_eq!(m.next_airing_at, Some(1234));
        assert_eq!(
            m.banner_image, None,
            "detail fields stay out of the list join"
        );
        assert_eq!(m.genres, None);
        assert_eq!(m.duration, None);
        assert_eq!(m.source, None);
        assert_eq!(m.studios, None);
        let one = db.get_entry(9).unwrap().unwrap();
        assert_eq!(
            one.media.as_ref().unwrap().title_romaji.as_deref(),
            Some("Romaji Title")
        );
        assert_eq!(one.media.as_ref().unwrap().banner_image, None);
    }

    #[test]
    fn lean_upsert_preserves_detail_fields() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("Old Title".into()),
            banner_image: Some("banner.jpg".into()),
            genres: Some(vec!["Action".into(), "Drama".into()]),
            duration: Some(24),
            source: Some("MANGA".into()),
            studios: Some(vec!["MAPPA".into()]),
            ..Default::default()
        })
        .unwrap();
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("New Title".into()),
            ..Default::default()
        })
        .unwrap();
        let m = db.get_media(1).unwrap().unwrap();
        assert_eq!(m.title_english.as_deref(), Some("New Title"));
        assert_eq!(m.banner_image.as_deref(), Some("banner.jpg"));
        assert_eq!(
            m.genres,
            Some(vec!["Action".to_string(), "Drama".to_string()])
        );
        assert_eq!(m.duration, Some(24));
        assert_eq!(m.source.as_deref(), Some("MANGA"));
        assert_eq!(m.studios, Some(vec!["MAPPA".to_string()]));
    }

    #[test]
    fn detail_upsert_clears_removed_fields() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media_detail(&Media {
            id: 1,
            title_english: Some("Old Title".into()),
            banner_image: Some("banner.jpg".into()),
            genres: Some(vec!["Action".into()]),
            studios: Some(vec!["MAPPA".into()]),
            ..Default::default()
        })
        .unwrap();
        db.upsert_media_detail(&Media {
            id: 1,
            title_english: Some("Updated Title".into()),
            ..Default::default()
        })
        .unwrap();
        let m = db.get_media(1).unwrap().unwrap();
        assert_eq!(m.banner_image, None);
        assert_eq!(m.genres, None);
        assert_eq!(m.studios, None);
    }

    #[test]
    fn media_cache_prune_spares_list_rows() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("On List".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_media(&Media {
            id: 2,
            title_english: Some("Off List".into()),
            ..Default::default()
        })
        .unwrap();
        db.upsert_entry(&ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            ..Default::default()
        })
        .unwrap();
        db.0.lock()
            .execute("UPDATE media SET cached_at = 1", [])
            .unwrap();
        assert_eq!(db.prune_media_cache(30).unwrap(), 1);
        assert!(
            db.get_media(1).unwrap().is_some(),
            "list backed row survives"
        );
        assert!(db.get_media(2).unwrap().is_none(), "unreferenced row goes");
    }

    #[test]
    fn seen_prune_keeps_currently_carried_guids() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.mark_rss_seen(&["old-gone".into(), "old-carried".into()])
            .unwrap();
        db.0.lock()
            .execute("UPDATE rss_seen SET seen_at = 1", [])
            .unwrap();
        db.prune_rss_seen_keeping(60, &["old-carried".into()])
            .unwrap();
        let seen = db.rss_seen_set().unwrap();
        assert!(!seen.contains("old-gone"));
        assert!(seen.contains("old-carried"));
    }

    #[test]
    fn seen_prune_keeps_legacy_hash_encodings_for_current_releases() {
        let hex = "nyaa-show\u{1}abcdef0123456789abcdef0123456789abcdef01";
        let base32 = "nyaa-show\u{1}vpg66ajdivtytk6n54asgrlhrgv433yb";
        let dir = std::env::temp_dir().join(format!("kurisu-seen-hash-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        for (index, old_key) in [hex, base32].into_iter().enumerate() {
            let path = dir.join(format!("{index}.db"));
            let db = Db::open(&path).unwrap();
            db.mark_rss_seen(&[old_key.into(), "old-gone".into()])
                .unwrap();
            db.0.lock()
                .execute("UPDATE rss_seen SET seen_at = 1", [])
                .unwrap();
            db.prune_rss_seen_keeping(60, &[hex.into()]).unwrap();
            drop(db);
            let db = Db::open(&path).unwrap();
            let seen = db.rss_seen_set().unwrap();
            assert_eq!(seen, std::collections::HashSet::from([old_key.to_string()]));
            for hash in [
                "abcdef0123456789abcdef0123456789abcdef01",
                "VPG66AJDIVTYTK6N54ASGRLHRGV433YB",
            ] {
                let release = crate::rss::RawItem {
                    info_hash: Some(hash.into()),
                    ..Default::default()
                };
                assert!(crate::show_torrents::was_seen(&release, &seen));
            }
            db.prune_rss_seen_keeping(60, &[]).unwrap();
            assert!(db.rss_seen_set().unwrap().is_empty());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn media_cache_prune_preserves_watch_history_titles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(&Media {
            id: 1,
            title_english: Some("Remembered show".into()),
            ..Default::default()
        })
        .unwrap();
        db.record_watch(7, "/anime/episode.mkv", 1, 1).unwrap();
        db.0.lock()
            .execute("UPDATE media SET cached_at = 1", [])
            .unwrap();
        db.prune_media_cache(30).unwrap();
        assert_eq!(
            db.watch_history(7, 10, None).unwrap()[0].title,
            "Remembered show"
        );
    }

    #[test]
    fn scrub_setting_removes_the_value_from_the_db_files() {
        let path =
            std::env::temp_dir().join(format!("kurisu-scrub-test-{}.db", std::process::id()));
        let needle = b"sekrit-token-value-1234567890";
        let mut files = vec![path.clone()];
        for suffix in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_os_string();
            side.push(suffix);
            files.push(side.into());
        }
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        {
            let db = Db::open(&path).unwrap();
            db.set_setting("anilist_token", std::str::from_utf8(needle).unwrap())
                .unwrap();
            // Checkpoint first to test scrubbing both the database and WAL.
            db.0.lock()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
            db.scrub_setting("anilist_token").unwrap();
            assert_eq!(db.get_setting("anilist_token").unwrap(), None);
            // Inspect before closing, which would remove the WAL and hide leaks.
            for f in &files {
                if let Ok(raw) = std::fs::read(f) {
                    assert!(
                        !raw.windows(needle.len()).any(|w| w == needle),
                        "token bytes survived scrub in {}",
                        f.display()
                    );
                }
            }
        }
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
    }

    #[test]
    fn scoped_preference_write_or_clear_wipes_all_foreign_or_corrupt_owner_keys() {
        for (initial_owner, new_q) in [
            ("1", None),
            ("1", Some("bob-search")),
            ("corrupt", None),
            ("0", Some("bob-search")),
        ] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            db.set_settings(&[
                (LIST_OWNER_KEY, initial_owner),
                ("native_preference:kurisu.list.q", "alice-search"),
                ("native_preference:kurisu.list.filter", "DROPPED"),
                ("native_preference:kurisu.list.synced", "98765"),
                ("native_preference:kurisu.list.sort", "title"),
                ("anilist_user_id", "2"),
            ])
            .unwrap();

            db.set_scoped_preference("native_preference:kurisu.list.q", new_q, Some(2), true)
                .unwrap();

            assert_eq!(
                db.get_setting("native_preference:kurisu.list.q")
                    .unwrap()
                    .as_deref(),
                new_q,
                "owner={initial_owner}, new_q={new_q:?}"
            );
            assert_eq!(
                db.get_setting("native_preference:kurisu.list.filter")
                    .unwrap(),
                None,
                "owner={initial_owner}, new_q={new_q:?}"
            );
            assert_eq!(
                db.get_setting("native_preference:kurisu.list.synced")
                    .unwrap(),
                None,
                "owner={initial_owner}, new_q={new_q:?}"
            );
            assert_eq!(
                db.get_setting("native_preference:kurisu.list.sort")
                    .unwrap()
                    .as_deref(),
                Some("title")
            );
            assert_eq!(
                db.get_setting(LIST_OWNER_KEY).unwrap().as_deref(),
                Some("2"),
                "owner={initial_owner}, new_q={new_q:?}"
            );
        }
    }

    #[test]
    fn scoped_preference_reads_hide_foreign_owner_and_keep_matching_or_signed_out() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let entries = [
            ("native_preference:kurisu.list.q", "alice-search"),
            ("native_preference:kurisu.list.filter", "DROPPED"),
            ("native_preference:kurisu.list.synced", "98765"),
        ];
        db.set_settings(&[
            (LIST_OWNER_KEY, "1"),
            entries[0],
            entries[1],
            entries[2],
            ("anilist_user_id", "2"),
        ])
        .unwrap();

        for has_token in [false, true] {
            for (key, _) in entries {
                assert_eq!(
                    db.get_scoped_preference(key, has_token).unwrap(),
                    None,
                    "foreign owner key={key} has_token={has_token}"
                );
            }
        }

        for corrupt_saved in ["corrupt", "0", "-1", "2147483648"] {
            db.set_setting("anilist_user_id", corrupt_saved).unwrap();
            for has_token in [false, true] {
                for (key, _) in entries {
                    assert_eq!(
                        db.get_scoped_preference(key, has_token).unwrap(),
                        None,
                        "corrupt saved={corrupt_saved} key={key} has_token={has_token}"
                    );
                }
            }
        }

        db.delete_setting("anilist_user_id").unwrap();
        for (key, expected) in entries {
            assert_eq!(
                db.get_scoped_preference(key, false).unwrap().as_deref(),
                Some(expected),
                "signed out key={key}"
            );
            assert_eq!(
                db.get_scoped_preference(key, true).unwrap(),
                None,
                "unidentified token key={key}"
            );
        }

        db.set_setting("anilist_user_id", "1").unwrap();
        for has_token in [false, true] {
            for (key, expected) in entries {
                assert_eq!(
                    db.get_scoped_preference(key, has_token).unwrap().as_deref(),
                    Some(expected),
                    "matching owner key={key} has_token={has_token}"
                );
            }
        }

        db.delete_setting(LIST_OWNER_KEY).unwrap();
        for saved in ["2", "2147483648"] {
            db.set_setting("anilist_user_id", saved).unwrap();
            for has_token in [false, true] {
                for (key, expected) in entries {
                    assert_eq!(
                        db.get_scoped_preference(key, has_token).unwrap().as_deref(),
                        Some(expected),
                        "unowned legacy saved={saved} key={key} has_token={has_token}"
                    );
                }
            }
        }
    }

    #[test]
    fn schema_7_migration_backfills_queued_creation_for_active_account_only() {
        struct TempDb(std::path::PathBuf);
        impl TempDb {
            fn new(path: std::path::PathBuf) -> Self {
                Self::remove(&path);
                Self(path)
            }
            fn remove(path: &std::path::Path) {
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
                }
            }
        }
        impl Drop for TempDb {
            fn drop(&mut self) {
                Self::remove(&self.0);
            }
        }

        let temp = TempDb::new(temp_db_path("v7-queued-creation-backfill"));
        let path = &temp.0;
        build_db_at_version(path, 7);
        {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch(
                "INSERT INTO settings (key, value) VALUES ('anilist_user_id', '7');
                 INSERT INTO list_entry (media_id, entry_id, status, progress, repeat)
                 VALUES (1, NULL, 'CURRENT', 2, 0),
                        (2, NULL, 'CURRENT', 3, 0),
                        (3, NULL, 'CURRENT', 4, 0),
                        (4, NULL, 'CURRENT', 5, 0);
                 INSERT INTO pending_change (account_id, media_id, payload) VALUES
                 (7, 1, '{\"media_id\":1,\"base\":null,\"patch\":{\"status\":\"CURRENT\",\"progress\":2},\"conflict\":false}'),
                 (8, 2, '{\"media_id\":2,\"base\":null,\"patch\":{\"status\":\"CURRENT\",\"progress\":3},\"conflict\":false}'),
                 (7, 3, '{\"media_id\":3,\"deleting\":true,\"base\":null,\"patch\":{},\"conflict\":false}'),
                 (7, 4, '{\"media_id\":4,\"missing_media\":true,\"base\":null,\"patch\":{},\"conflict\":true}');",
            )
            .unwrap();
        }

        let db = Db::open(path).unwrap();
        let active_key = db
            .pending_change(7, 1)
            .unwrap()
            .unwrap()
            .patch
            .creation_key
            .unwrap();
        assert_eq!(active_key.len(), 32);
        assert_eq!(
            db.get_entry(1).unwrap().unwrap().queued_creation.as_deref(),
            Some(active_key.as_str())
        );
        assert_eq!(db.get_entry(2).unwrap().unwrap().queued_creation, None);
        assert_eq!(db.get_entry(3).unwrap().unwrap().queued_creation, None);
        assert_eq!(db.get_entry(4).unwrap().unwrap().queued_creation, None);
        assert_eq!(db.get_entry(7).unwrap().unwrap().queued_creation, None);
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    #[test]
    fn failed_durable_transactions_roll_back_and_restore_the_cache_policy() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let result = db.durable_write(|tx| {
            let mode: i64 = tx.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(mode, 2);
            tx.execute(
                "INSERT INTO settings VALUES ('durability-test', 'pending')",
                [],
            )?;
            anyhow::bail!("simulated write failure")
        });
        assert!(result.is_err());
        assert_eq!(db.get_setting("durability-test").unwrap(), None);
        let mode: i64 =
            db.0.lock()
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
        assert_eq!(mode, 1);
        db.durable_write(|tx| {
            tx.execute(
                "INSERT INTO settings VALUES ('durability-test', 'saved')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.get_setting("durability-test").unwrap().as_deref(),
            Some("saved")
        );
    }

    #[test]
    fn preserve_list_owner_clears_corrupt_or_unowned_preferences_and_hides_corrupt_reads() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.set_setting("anilist_user_id", "10").unwrap();
        db.set_setting(LIST_OWNER_KEY, "corrupt").unwrap();
        db.set_setting("native_preference:kurisu.list.q", "secret")
            .unwrap();
        for has_token in [false, true] {
            assert_eq!(
                db.get_scoped_preference("native_preference:kurisu.list.q", has_token)
                    .unwrap(),
                None
            );
        }
        db.set_scoped_preference("native_preference:kurisu.list.q", Some("new"), None, false)
            .unwrap();
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.q")
                .unwrap()
                .as_deref(),
            Some("secret")
        );
        db.preserve_list_owner().unwrap();
        assert_eq!(db.get_setting(LIST_OWNER_KEY).unwrap(), None);
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.q").unwrap(),
            None
        );

        db.delete_setting("anilist_user_id").unwrap();
        db.set_setting("native_preference:kurisu.list.filter", "CURRENT")
            .unwrap();
        db.preserve_list_owner().unwrap();
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.filter")
                .unwrap(),
            None
        );
    }

    #[test]
    fn write_episode_total_normalizes_non_positive_totals_to_none() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert!(db.cache_episode_total(42, Some(12)).unwrap());
        assert_eq!(db.episode_total(42).unwrap(), Some(12));

        assert!(db.cache_episode_total(42, Some(0)).unwrap());
        assert_eq!(db.episode_total(42).unwrap(), None);
        assert_eq!(db.cached_episode_total(42).unwrap(), Some(None));
        let raw: Option<i64> =
            db.0.lock()
                .query_row("SELECT episodes FROM media WHERE id = 42", [], |r| r.get(0))
                .unwrap();
        assert_eq!(raw, None);

        assert!(!db.cache_episode_total(42, Some(-3)).unwrap());
        assert!(!db.cache_episode_total(42, None).unwrap());
    }
}
