//! Local SQLite cache. Holds the user's own list (mirror of AniList for offline +
//! fast UI), media metadata we've already looked up, and watched-file history for
//! the recognizer. Migrations run inline on open; no migration framework needed at
//! this scale.

use anyhow::Result;
use rusqlite::Connection;
use std::sync::Mutex;

use crate::models::{ListEntry, Media};

pub struct Db(pub Mutex<Connection>);

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        Self::migrate(&conn)?;
        Ok(Db(Mutex::new(conn)))
    }

    fn migrate(conn: &Connection) -> Result<()> {
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
        // Columns added after launch: back-fill them on existing DBs (CREATE TABLE
        // IF NOT EXISTS won't add columns to an existing table).
        Self::ensure_column(conn, "media", "next_airing_episode", "INTEGER")?;
        Self::ensure_column(conn, "media", "next_airing_at", "INTEGER")?;
        Ok(())
    }

    /// Add `col` to `table` if it isn't already there. Lets us evolve the schema
    /// without a migration framework.
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
        let c = self.0.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO media
             (id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
              episodes,format,status,average_score,season,season_year,description,
              next_airing_episode,next_airing_at,cached_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            rusqlite::params![
                m.id, m.id_mal, m.title_romaji, m.title_english, m.title_native,
                m.cover_medium, m.cover_large, m.episodes, m.format, m.status,
                m.average_score, m.season, m.season_year, m.description,
                m.next_airing_episode, m.next_airing_at,
                chrono::Utc::now().timestamp(),
            ],
        )?;
        Ok(())
    }

    pub fn get_media(&self, id: i64) -> Result<Option<Media>> {
        let c = self.0.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id,id_mal,title_romaji,title_english,title_native,cover_medium,cover_large,
                    episodes,format,status,average_score,season,season_year,description,
                    next_airing_episode,next_airing_at
             FROM media WHERE id = ?",
        )?;
        let row = stmt.query_row([id], |r| row_to_media(r)).ok();
        Ok(row)
    }

    pub fn upsert_entry(&self, e: &ListEntry) -> Result<()> {
        let c = self.0.lock().unwrap();
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
        self.0.lock().unwrap().execute(
            "DELETE FROM list_entry WHERE media_id = ?",
            [media_id],
        )?;
        Ok(())
    }

    /// All local entries with their cached media joined in. The frontend list view.
    pub fn entries_with_media(&self) -> Result<Vec<ListEntry>> {
        let c = self.0.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT e.media_id,e.entry_id,e.status,e.progress,e.score,e.repeat,e.updated_at,
                    m.id,m.id_mal,m.title_romaji,m.title_english,m.title_native,m.cover_medium,
                    m.cover_large,m.episodes,m.format,m.status,m.average_score,m.season,
                    m.season_year,m.description,m.next_airing_episode,m.next_airing_at
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

    pub fn get_entry(&self, media_id: i64) -> Result<Option<ListEntry>> {
        let c = self.0.lock().unwrap();
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
        self.0.lock().unwrap().execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?,?)",
            [key, value],
        )?;
        Ok(())
    }
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
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
        self.0.lock().unwrap().execute(
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
            .unwrap()
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
    })
}
