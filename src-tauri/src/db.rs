use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

use crate::models::{ListEntry, Media};

const SCHEMA_VERSION: i64 = 4;

pub struct Db(pub Mutex<Connection>);

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // NORMAL avoids a sync on every commit. A power loss may discard recent cache writes.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
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
        Ok(Db(Mutex::new(conn)))
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
              banner_image=excluded.banner_image,
              genres=excluded.genres,
              duration=excluded.duration,
              source=excluded.source,
              studios=excluded.studios,
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
        let row = stmt.query_row([id], row_to_media).optional()?;
        Ok(row)
    }

    pub fn upsert_entry(&self, e: &ListEntry) -> Result<()> {
        let c = self.0.lock();
        c.execute(
            "INSERT OR REPLACE INTO list_entry
             (media_id,entry_id,status,progress,score,repeat,updated_at)
             VALUES (?,?,?,?,?,?,?)",
            rusqlite::params![
                e.media_id,
                e.id,
                e.status,
                e.progress,
                e.score,
                e.repeat,
                e.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn delete_entry(&self, media_id: i64) -> Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM list_entry WHERE media_id = ?", [media_id])?;
        Ok(())
    }

    pub fn delete_media(&self, media_id: i64) -> Result<()> {
        self.0
            .lock()
            .execute("DELETE FROM media WHERE id = ?", [media_id])?;
        Ok(())
    }

    pub fn replace_list_snapshot(&self, entries: &[ListEntry]) -> Result<()> {
        let mut c = self.0.lock();
        let tx = c.transaction()?;
        for e in entries {
            if let Some(m) = &e.media {
                upsert_media_row(&tx, m)?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO list_entry
                 (media_id,entry_id,status,progress,score,repeat,updated_at)
                 VALUES (?,?,?,?,?,?,?)",
                rusqlite::params![
                    e.media_id,
                    e.id,
                    e.status,
                    e.progress,
                    e.score,
                    e.repeat,
                    e.updated_at
                ],
            )?;
        }
        // A JSON array avoids SQLite's bound parameter limit.
        let keep: Vec<i64> = entries.iter().map(|e| e.media_id).collect();
        tx.execute(
            "DELETE FROM list_entry WHERE media_id NOT IN (SELECT value FROM json_each(?))",
            [serde_json::to_string(&keep)?],
        )?;
        Ok(tx.commit()?)
    }

    pub fn entries_with_media(&self) -> Result<Vec<ListEntry>> {
        let c = self.0.lock();
        let mut stmt = c.prepare(
            // Omit large detail fields while preserving the column layout used by row_to_media_offset.
            "SELECT e.media_id,e.entry_id,e.status,e.progress,e.score,e.repeat,e.updated_at,
                    m.id,m.id_mal,m.title_romaji,m.title_english,m.title_native,m.cover_medium,
                    m.cover_large,m.episodes,m.format,m.status,m.average_score,m.season,
                    m.season_year,NULL,m.next_airing_episode,m.next_airing_at,
                    NULL,NULL,NULL,NULL,NULL
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
                        NULL,NULL,NULL,NULL,NULL
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
                        media: row_to_media_offset(r, 7).ok(),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn replace_account(&self, token: &str, user: &crate::models::User) -> Result<bool> {
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
        if changed {
            tx.execute("DELETE FROM list_entry", [])?;
        }
        for (key, value) in [
            ("anilist_token", token),
            ("anilist_username", user.name.as_str()),
            ("anilist_user_id", id.as_str()),
            ("anilist_profile", profile.as_str()),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [key, value],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn cached_user(&self) -> Result<Option<crate::models::User>> {
        let settings = self.get_settings_batch(&["anilist_user_id", "anilist_profile"])?;
        let user = settings
            .get("anilist_profile")
            .and_then(|value| serde_json::from_str::<crate::models::User>(value).ok())
            .filter(|user| settings.get("anilist_user_id") == Some(&user.id.to_string()));
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
            tx.execute(
                "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
                [k, v],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Remove the value from freed database pages and the WAL as well as the live table.
    pub fn scrub_setting(&self, key: &str) -> Result<()> {
        let c = self.0.lock();
        c.execute("DELETE FROM settings WHERE key = ?", [key])?;
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
        self.0.lock().execute(
            "DELETE FROM rss_seen WHERE seen_at < ? AND guid NOT IN (SELECT value FROM json_each(?))",
            rusqlite::params![cutoff, serde_json::to_string(keep)?],
        )?;
        Ok(())
    }

    pub fn prune_media_cache(&self, days: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now().timestamp() - days * 86_400;
        let n = self.0.lock().execute(
            "DELETE FROM media WHERE cached_at < ? AND id NOT IN (SELECT media_id FROM list_entry)",
            [cutoff],
        )?;
        Ok(n)
    }

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
            .query_row("SELECT 1 FROM watched_file WHERE path = ?", [path], |_| {
                Ok(())
            })
            .is_ok())
    }
}

/// Preserve cached detail fields that lean queries never fetch.
fn upsert_media_row(c: &Connection, m: &Media) -> Result<()> {
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
            m.id,
            m.id_mal,
            m.title_romaji,
            m.title_english,
            m.title_native,
            m.cover_medium,
            m.cover_large,
            m.episodes,
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
        ],
    )?;
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
                    ..Default::default()
                }),
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
            drop(db);
            let _ = std::fs::remove_file(&path);
        }
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
            ..Default::default()
        })
        .unwrap();
        db.upsert_media(&Media {
            id: 2,
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
}
