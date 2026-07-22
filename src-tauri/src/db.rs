//! Local SQLite cache. Holds the user's own list (mirror of AniList for offline +
//! fast UI), media metadata we've already looked up, and watched-file history for
//! the recognizer. Migrations run inline on open; no migration framework needed at
//! this scale.

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::models::{ListEntry, Media};

/// Current schema version (PRAGMA user_version). Bump and add a rung to the
/// ladder in `migrate` for every schema change.
const SCHEMA_VERSION: i64 = 3;

pub struct Db(pub Mutex<Connection>);

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        Self::migrate(&conn)?;
        // The settings table holds the AniList token in plaintext: keep the DB
        // and its WAL sidecars owner-only (Connection::open uses the process
        // umask, typically 0644). Best-effort fix-up on every open.
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
        Ok(Db(Mutex::new(conn)))
    }

    /// Schema ladder keyed off PRAGMA user_version: each `if version < N` rung
    /// upgrades N-1 → N. CREATE TABLEs stay IF NOT EXISTS so a fresh DB (version
    /// 0) and an old one converge on the same schema.
    fn migrate(conn: &Connection) -> Result<()> {
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
            // Columns added after launch: back-fill them on existing DBs (CREATE
            // TABLE IF NOT EXISTS won't add columns to an existing table).
            Self::ensure_column(conn, "media", "next_airing_episode", "INTEGER")?;
            Self::ensure_column(conn, "media", "next_airing_at", "INTEGER")?;
        }
        if version < 3 {
            // Detail-page fields (M5). genres/studios are JSON arrays as TEXT.
            Self::ensure_column(conn, "media", "banner_image", "TEXT")?;
            Self::ensure_column(conn, "media", "genres", "TEXT")?;
            Self::ensure_column(conn, "media", "duration", "INTEGER")?;
            Self::ensure_column(conn, "media", "source", "TEXT")?;
            Self::ensure_column(conn, "media", "studios", "TEXT")?;
        }
        if version < SCHEMA_VERSION {
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        }
        Ok(())
    }

    /// Add `col` to `table` if it isn't already there. Lets us evolve the schema
    /// without a migration framework. NOTE: only nullable columns (or ones with a
    /// DEFAULT) can go through here — SQLite refuses ADD COLUMN ... NOT NULL on
    /// an existing table.
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
        // The rich detail-only fields (banner/genres/duration/source/studios) are
        // COALESCEd: a lean upsert (search / season / list sync doesn't fetch
        // them) must not wipe values a detail fetch already cached. Everything
        // the lean queries DO fetch takes the fresh value.
        c.execute(
            "INSERT INTO media
             (id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
              episodes,format,status,average_score,season,season_year,description,
              next_airing_episode,next_airing_at,banner_image,genres,duration,source,studios,cached_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
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
              cached_at=excluded.cached_at",
            rusqlite::params![
                m.id, m.id_mal, m.title_romaji, m.title_english, m.title_native,
                m.cover_medium, m.cover_large, m.episodes, m.format, m.status,
                m.average_score, m.season, m.season_year, m.description,
                m.next_airing_episode, m.next_airing_at,
                m.banner_image,
                m.genres.as_ref().and_then(|g| serde_json::to_string(g).ok()),
                m.duration, m.source,
                m.studios.as_ref().and_then(|s| serde_json::to_string(s).ok()),
                chrono::Utc::now().timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_media(&self, id: i64) -> Result<Option<Media>> {
        let c = self.0.lock();
        let mut stmt = c.prepare(
            "SELECT id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
                    episodes,format,status,average_score,season,season_year,description,
                    next_airing_episode,next_airing_at,banner_image,genres,duration,source,studios
             FROM media WHERE id = ?",
        )?;
        let row = stmt.query_row([id], row_to_media).ok();
        Ok(row)
    }

    pub fn upsert_entry(&self, e: &ListEntry) -> Result<()> {
        let c = self.0.lock();
        c.execute(
            "INSERT OR REPLACE INTO list_entry
             (media_id,entry_id,status,progress,score,repeat,updated_at)
             VALUES (?,?,?,?,?,?,?)",
            rusqlite::params![
                e.media_id, e.id, e.status, e.progress, e.score, e.repeat, e.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delete_entry(&self, media_id: i64) -> Result<()> {
        self.0.lock().execute(
            "DELETE FROM list_entry WHERE media_id = ?",
            [media_id],
        )?;
        Ok(())
    }

    /// Delete every local entry whose media_id is NOT in `keep`. Used after a
    /// successful full-list sync: rows the remote no longer has were deleted
    /// elsewhere (or belong to a previously signed-in account) and must not
    /// linger — the recognizer would still match them and the watcher could
    /// resurrect them on AniList.
    pub fn delete_entries_not_in(&self, keep: &std::collections::HashSet<i64>) -> Result<()> {
        let mut c = self.0.lock();
        let stale: Vec<i64> = {
            let mut stmt = c.prepare("SELECT media_id FROM list_entry")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .filter_map(Result::ok)
                .filter(|id| !keep.contains(id))
                .collect();
            ids
        };
        let tx = c.transaction()?;
        for id in stale {
            tx.execute("DELETE FROM list_entry WHERE media_id = ?", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// All local entries with their cached media joined in. The frontend list view.
    pub fn entries_with_media(&self) -> Result<Vec<ListEntry>> {
        let c = self.0.lock();
        let mut stmt = c.prepare(
            "SELECT e.media_id,e.entry_id,e.status,e.progress,e.score,e.repeat,e.updated_at,
                    m.id,m.id_mal,m.title_romaji,m.title_english,m.title_native,m.cover_medium,
                    m.cover_large,m.episodes,m.format,m.status,m.average_score,m.season,
                    m.season_year,m.description,m.next_airing_episode,m.next_airing_at,
                    m.banner_image,m.genres,m.duration,m.source,m.studios
             FROM list_entry e LEFT JOIN media m ON m.id = e.media_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ListEntry {
                id: r.get::<_, Option<i64>>(1)?,
                media_id: r.get(0)?,
                status: r.get(2)?,
                progress: r.get(3)?,
                score: r.get(4)?,
                repeat: r.get(5)?,
                updated_at: r.get(6)?,
                media: row_to_media_offset(r, 7).ok(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Just the media ids of the local list (cheap membership set — used by the
    /// calendar to decide which airing media are worth caching).
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
        let row = c
            .query_row(
                "SELECT media_id,entry_id,status,progress,score,repeat,updated_at
                 FROM list_entry WHERE media_id = ?",
                [media_id],
                |r| {
                    Ok(ListEntry {
                        id: r.get(1)?,
                        media_id: r.get(0)?,
                        status: r.get(2)?,
                        progress: r.get(3)?,
                        score: r.get(4)?,
                        repeat: r.get(5)?,
                        updated_at: r.get(6)?,
                        media: None,
                    })
                },
            )
            .ok();
        Ok(row)
    }

    // ---- settings (key/value) ----
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
            [key, value],
        )?;
        Ok(())
    }
    /// Multi-key upsert in ONE transaction: a concurrent reader never sees a
    /// half-saved group (e.g. the tracking config's three keys).
    pub fn set_settings(&self, kvs: &[(&str, &str)]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        for (k, v) in kvs {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [k, v],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Flush + truncate the WAL. Used on logout so the old token doesn't linger
    /// in the -wal sidecar after the settings row is overwritten.
    pub fn checkpoint(&self) -> Result<()> {
        self.0.lock().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [key],
                |r| r.get::<_, String>(0),
            )
            .ok())
    }

    // ---- watched-file log (recognizer dedup) ----
    #[allow(dead_code)]
    pub fn mark_watched(&self, path: &str, media_id: i64, episode: i64) -> Result<()> {
        self.0.lock().execute(
            "INSERT OR REPLACE INTO watched_file (path,media_id,episode,watched_at)
             VALUES (?,?,?,?)",
            rusqlite::params![path, media_id, episode, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }
    #[allow(dead_code)]
    pub fn is_watched(&self, path: &str) -> Result<bool> {
        Ok(self
            .0
            .lock()
            .query_row(
                "SELECT 1 FROM watched_file WHERE path = ?",
                [path],
                |_| Ok(()),
            )
            .is_ok())
    }
}

fn row_to_media(r: &rusqlite::Row) -> rusqlite::Result<Media> {
    row_to_media_offset(r, 0)
}

fn row_to_media_offset(r: &rusqlite::Row, o: usize) -> rusqlite::Result<Media> {
    // genres/studios live in the DB as JSON text.
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
        episodes: r.get(o + 7)?,
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

    /// A lean upsert (search/season/sync — no detail fields) must not wipe the
    /// rich fields a detail fetch already cached; everything lean queries do
    /// fetch takes the fresh value.
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
        assert_eq!(m.genres, Some(vec!["Action".to_string(), "Drama".to_string()]));
        assert_eq!(m.duration, Some(24));
        assert_eq!(m.source.as_deref(), Some("MANGA"));
        assert_eq!(m.studios, Some(vec!["MAPPA".to_string()]));
    }
}
