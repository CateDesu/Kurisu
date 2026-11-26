use anyhow::{bail, ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn check_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|component| matches!(component, Component::ParentDir)
                    || component
                        .as_os_str()
                        .to_string_lossy()
                        .to_lowercase()
                        .contains("hentai")),
        "Use an absolute permitted local path without parent traversal"
    );
    #[cfg(windows)]
    ensure!(
        matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))),
        "Use a local drive path. Network and device paths are not supported"
    );
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !link_or_reparse(&metadata),
                "Links and reparse points are not allowed"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn regular_file(path: &Path) -> Result<()> {
    check_path(path)?;
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "Expected a regular file: {}",
        path.display()
    );
    Ok(())
}

fn absent(path: &Path) -> Result<bool> {
    check_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn profile_files(source: &Path) -> Result<Vec<PathBuf>> {
    regular_file(source)?;
    ensure!(
        absent(&sidecar(source, "-journal"))?,
        "Profile has a rollback journal. Preserve it for recovery before retrying"
    );
    let mut files = vec![source.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let path = sidecar(source, suffix);
        if !absent(&path)? {
            regular_file(&path)?;
            files.push(path);
        }
    }
    Ok(files)
}

fn digest(path: &Path) -> Result<Vec<u8>> {
    regular_file(path)?;
    let mut hash = Sha256::new();
    let mut file = File::open(path)?;
    std::io::copy(&mut file, &mut hash)?;
    Ok(hash.finalize().to_vec())
}

fn profile_fingerprints(source: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    profile_files(source)?
        .into_iter()
        .map(|path| Ok((path.clone(), digest(&path)?)))
        .collect()
}

fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn copy_private(source: &Path, destination: &Path) -> Result<()> {
    let mut input = File::open(source)?;
    let mut output = create_private(destination)?;
    std::io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(())
}

fn copy_matches(source: &Path, destination: &Path, expected: &[u8]) -> Result<bool> {
    copy_private(source, destination)?;
    Ok(digest(destination)? == expected)
}

struct Stage(PathBuf);
impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        for _ in 0..100 {
            let path = parent.join(format!(
                ".kurisu-validate-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("Could not create private validation staging directory")
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn validate_database(path: &Path) -> Result<()> {
    validate_database_with_scratch(path, path.parent().context("Missing database parent")?)
}

/// Keep disposable validation files outside a strictly owned publication stage.
pub fn validate_database_with_scratch(path: &Path, scratch_parent: &Path) -> Result<()> {
    regular_file(path)?;
    check_path(scratch_parent)?;
    let stage = Stage::new(scratch_parent)?;
    let (db, original) = open_snapshot(path, &stage.0)?;
    drop(db);
    ensure!(
        original == profile_fingerprints(path)?,
        "Profile changed during validation. Stop Kurisu and retry"
    );
    Ok(())
}

fn validate_schema(db: &Connection) -> Result<()> {
    let integrity: Vec<String> = db
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(integrity == ["ok"], "Database integrity check failed");
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        (1..=crate::db::SCHEMA_VERSION).contains(&version),
        "Unsupported schema version: {version}"
    );
    let mut columns = vec![
        ("settings", vec!["key", "value"]),
        (
            "media",
            vec![
                "id",
                "id_mal",
                "title_romaji",
                "title_english",
                "title_native",
                "cover_medium",
                "cover_large",
                "episodes",
                "format",
                "status",
                "average_score",
                "season",
                "season_year",
                "description",
                "cached_at",
            ],
        ),
        (
            "list_entry",
            vec![
                "media_id",
                "entry_id",
                "status",
                "progress",
                "score",
                "repeat",
                "updated_at",
            ],
        ),
        (
            "watched_file",
            vec!["path", "media_id", "episode", "watched_at"],
        ),
    ];
    if version >= 2 {
        columns[1]
            .1
            .extend(["next_airing_episode", "next_airing_at"]);
    }
    if version >= 3 {
        columns[1]
            .1
            .extend(["banner_image", "genres", "duration", "source", "studios"]);
    }
    if version >= 4 {
        columns.push(("rss_seen", vec!["guid", "seen_at"]));
    }
    if version >= 5 {
        columns.push(("pending_change", vec!["account_id", "media_id", "payload"]));
        columns.push((
            "watch_history",
            vec![
                "id",
                "account_id",
                "path",
                "media_id",
                "episode",
                "watched_at",
            ],
        ));
    }
    if version >= 8 {
        columns[2].1.push("queued_creation");
    }
    let tables: BTreeSet<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (table, required) in columns {
        ensure!(
            tables.contains(table),
            "Database schema is missing the {table} table"
        );
        let present: BTreeSet<String> = db
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        ensure!(
            required.iter().all(|name| present.contains(*name)),
            "Database schema is missing {table} columns"
        );
    }
    let indexes: BTreeSet<String> = db
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'watch_history'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (minimum, name) in [
        (6, "watch_history_duplicate"),
        (7, "watch_history_account_id"),
    ] {
        ensure!(
            version < minimum || indexes.contains(name),
            "Database schema is missing required indexes"
        );
    }
    Ok(())
}

fn checksum(data: &[u8], little: bool, previous: (u32, u32)) -> (u32, u32) {
    let (mut first, mut second) = previous;
    for pair in data.as_chunks::<8>().0 {
        let word = |bytes: &[u8]| {
            let bytes: [u8; 4] = bytes.try_into().expect("four bytes");
            if little {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            }
        };
        first = first.wrapping_add(word(&pair[..4])).wrapping_add(second);
        second = second.wrapping_add(word(&pair[4..])).wrapping_add(first);
    }
    (first, second)
}

fn validate_wal(database: &Path, wal: &Path) -> Result<()> {
    let refuse = || {
        anyhow::anyhow!("WAL is incomplete, damaged or unsupported. Preserve the originals and verify a recovered copy before retrying")
    };
    let mut input = File::open(wal)?;
    if input.metadata()?.len() == 0 {
        return Ok(());
    }
    let mut header = [0; 32];
    input.read_exact(&mut header).map_err(|_| refuse())?;
    let word = |data: &[u8], start: usize| {
        u32::from_be_bytes(data[start..start + 4].try_into().expect("four bytes"))
    };
    let magic = word(&header, 0);
    let page_size = word(&header, 8);
    ensure!(
        matches!(magic, 0x377f0682 | 0x377f0683)
            && word(&header, 4) == 3007000
            && (512..=65536).contains(&page_size)
            && page_size.is_power_of_two(),
        "{}",
        refuse()
    );
    let mut main = [0; 18];
    File::open(database)?
        .read_exact(&mut main)
        .map_err(|_| refuse())?;
    let main_size = u16::from_be_bytes([main[16], main[17]]);
    ensure!(
        page_size
            == if main_size == 1 {
                65536
            } else {
                u32::from(main_size)
            },
        "{}",
        refuse()
    );
    let little = magic == 0x377f0682;
    let mut sum = checksum(&header[..24], little, (0, 0));
    ensure!(
        sum == (word(&header, 24), word(&header, 28)),
        "{}",
        refuse()
    );
    let remaining = input.metadata()?.len() - 32;
    let frame_size = u64::from(page_size) + 24;
    ensure!(remaining % frame_size == 0, "{}", refuse());
    let mut frame = vec![0; frame_size as usize];
    let salts = (word(&header, 16), word(&header, 20));
    let mut has_current_frame = false;
    let mut obsolete = None;
    let mut obsolete_orders = [true, true];
    for _ in 0..remaining / frame_size {
        input.read_exact(&mut frame).map_err(|_| refuse())?;
        ensure!(
            (1..=0xfffffffe).contains(&word(&frame, 0)) && word(&frame, 4) <= 0xfffffffe,
            "{}",
            refuse()
        );
        let frame_salts = (word(&frame, 8), word(&frame, 12));
        let frame_sum = (word(&frame, 16), word(&frame, 20));
        let frame_checksum =
            |little| checksum(&frame[24..], little, checksum(&frame[..8], little, sum));
        if frame_salts == salts {
            ensure!(
                obsolete.is_none() && frame_checksum(little) == frame_sum,
                "{}",
                refuse()
            );
            has_current_frame = true;
        } else {
            // A restarted WAL can spill frames before the new generation commits.
            ensure!(has_current_frame, "{}", refuse());
            if obsolete == Some(frame_salts) {
                for (possible, little) in obsolete_orders.iter_mut().zip([true, false]) {
                    *possible = *possible && frame_checksum(little) == frame_sum;
                }
                ensure!(
                    obsolete_orders.into_iter().any(|possible| possible),
                    "{}",
                    refuse()
                );
            } else {
                let previous = obsolete.unwrap_or(salts);
                // A current frame with damaged salts still continues the current checksum.
                ensure!(
                    (obsolete.is_some() || frame_checksum(little) != frame_sum)
                        && (1..=i32::MAX as u32).contains(&previous.0.wrapping_sub(frame_salts.0))
                        && previous.1 != frame_salts.1,
                    "{}",
                    refuse()
                );
                // SQLite uses the writer's byte order when it starts a new generation.
                obsolete_orders = [true, true];
            }
            obsolete = Some(frame_salts);
        }
        sum = frame_sum;
    }
    Ok(())
}

fn open_snapshot(
    source: &Path,
    scratch: &Path,
) -> Result<(Connection, BTreeMap<PathBuf, Vec<u8>>)> {
    let original = profile_fingerprints(source)?;
    let copied = scratch.join("kurisu.db");
    ensure!(
        copy_matches(source, &copied, &original[source])?,
        "Staged database differs from the original snapshot"
    );
    let wal = sidecar(source, "-wal");
    if let Some(expected) = original.get(&wal) {
        let copied_wal = sidecar(&copied, "-wal");
        ensure!(
            copy_matches(&wal, &copied_wal, expected)?,
            "Staged WAL differs from the original snapshot"
        );
        validate_wal(&copied, &copied_wal)?;
    }
    ensure!(
        original == profile_fingerprints(source)?,
        "Profile changed during validation. Stop Kurisu and retry"
    );
    let source_db = Connection::open_with_flags(&copied, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_schema(&source_db)?;
    Ok((source_db, original))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: Stage,
        db_path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            #[cfg(windows)]
            let parent = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
            #[cfg(not(windows))]
            let parent = std::env::temp_dir();
            let root = Stage::new(&parent).unwrap();
            let profile = root.0.join("profile");
            fs::create_dir(&profile).unwrap();
            let db_path = profile.join("kurisu.db");
            let db = crate::Db::open(&db_path).unwrap();
            db.0.lock().execute_batch(
                "INSERT INTO settings VALUES ('anilist_token','synthetic-private-token');
                 INSERT INTO settings VALUES ('anilist_user_id','42');
                 INSERT INTO settings VALUES ('library_folders','[\"/synthetic/media\"]');
                 INSERT INTO settings VALUES ('binding:/synthetic/show','7');
                 INSERT INTO media (id,title_romaji,episodes,cached_at) VALUES (7,'Fixture show',12,100);
                 INSERT INTO list_entry (media_id,entry_id,status,progress,score,repeat,updated_at)
                 VALUES (7,70,'CURRENT',5,8.5,2,101);
                 INSERT INTO pending_change VALUES (42,7,'{\"progress\":6}');
                 INSERT INTO pending_change VALUES (99,7,'{\"progress\":3}');
                 INSERT INTO watched_file VALUES ('/synthetic/file',7,5,102);
                 INSERT INTO watch_history (account_id,path,media_id,episode,watched_at) VALUES (42,'/synthetic/file',7,5,102);
                 INSERT INTO rss_seen VALUES ('fixture-guid',103);"
            ).unwrap();
            drop(db);
            Self { root, db_path }
        }
    }

    #[test]
    fn supported_versions_validate() {
        for version in 1..=crate::db::SCHEMA_VERSION {
            let fixture = Fixture::new();
            Connection::open(&fixture.db_path)
                .unwrap()
                .pragma_update(None, "user_version", version)
                .unwrap();
            validate_database(&fixture.db_path).unwrap();
        }
    }

    #[test]
    fn damaged_future_and_missing_schema_fail_validation() {
        for kind in ["future", "schema", "integrity", "journal"] {
            let fixture = Fixture::new();
            match kind {
                "future" => Connection::open(&fixture.db_path)
                    .unwrap()
                    .pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)
                    .unwrap(),
                "schema" => Connection::open(&fixture.db_path)
                    .unwrap()
                    .execute_batch("DROP TABLE pending_change")
                    .unwrap(),
                "integrity" => fs::write(&fixture.db_path, b"not SQLite").unwrap(),
                "journal" => {
                    fs::write(sidecar(&fixture.db_path, "-journal"), b"pending").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(validate_database(&fixture.db_path).is_err(), "{kind}");
        }
    }

    #[test]
    fn read_only_validation_never_creates_or_repairs_a_database() {
        let fixture = Fixture::new();
        let missing = fixture.root.0.join("missing.db");
        assert!(validate_database(&missing).is_err());
        assert!(!missing.exists());
        let empty = fixture.root.0.join("empty.db");
        Connection::open(&empty).unwrap();
        let before = fs::read(&empty).unwrap();
        assert!(validate_database(&empty).is_err());
        assert_eq!(fs::read(&empty).unwrap(), before);
        validate_database(&fixture.db_path).unwrap();
    }

    fn with_wal(fixture: &Fixture) {
        let live = fixture.root.0.join("fixture-wal.db");
        copy_private(&fixture.db_path, &live).unwrap();
        let db = Connection::open(&live).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE pending_change SET payload='committed WAL edit' WHERE account_id=42").unwrap();
        fs::copy(&live, &fixture.db_path).unwrap();
        fs::copy(sidecar(&live, "-wal"), sidecar(&fixture.db_path, "-wal")).unwrap();
    }

    fn set_wal_checksum_order(bytes: &mut [u8], little: bool) {
        let magic = if little { 0x377f0682u32 } else { 0x377f0683u32 };
        bytes[..4].copy_from_slice(&magic.to_be_bytes());
        let page_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let mut sum = checksum(&bytes[..24], little, (0, 0));
        bytes[24..28].copy_from_slice(&sum.0.to_be_bytes());
        bytes[28..32].copy_from_slice(&sum.1.to_be_bytes());
        for frame in bytes[32..].chunks_exact_mut(24 + page_size) {
            sum = checksum(&frame[24..], little, checksum(&frame[..8], little, sum));
            frame[16..20].copy_from_slice(&sum.0.to_be_bytes());
            frame[20..24].copy_from_slice(&sum.1.to_be_bytes());
        }
    }

    #[test]
    fn valid_uncommitted_wal_tail_is_excluded_and_both_checksum_orders_work() {
        for big in [false, true] {
            let fixture = Fixture::new();
            with_wal(&fixture);
            let wal = sidecar(&fixture.db_path, "-wal");
            let mut bytes = fs::read(&wal).unwrap();
            let page_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let frame_size = 24 + page_size;
            let mut tail = bytes[bytes.len() - frame_size..].to_vec();
            let committed = b"committed WAL edit";
            let uncommitted = b"uncommitted change";
            let index = tail
                .windows(committed.len())
                .position(|value| value == committed)
                .unwrap();
            tail[index..index + committed.len()].copy_from_slice(uncommitted);
            tail[4..8].copy_from_slice(&0u32.to_be_bytes());
            bytes.extend(tail);
            set_wal_checksum_order(&mut bytes, !big);
            fs::write(&wal, &bytes).unwrap();
            let before = profile_fingerprints(&fixture.db_path).unwrap();
            validate_database(&fixture.db_path).unwrap();
            assert_eq!(profile_fingerprints(&fixture.db_path).unwrap(), before);
            let stage = Stage::new(&fixture.root.0).unwrap();
            let (db, _) = open_snapshot(&fixture.db_path, &stage.0).unwrap();
            let actual: String = db
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=42",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, "committed WAL edit");
        }
        let fixture = Fixture::new();
        fs::write(sidecar(&fixture.db_path, "-wal"), b"").unwrap();
        validate_database(&fixture.db_path).unwrap();
    }

    fn with_recycled_wal(fixture: &Fixture, opposite_checksum_order: bool) {
        let live = fixture.root.0.join("recycled.db");
        copy_private(&fixture.db_path, &live).unwrap();
        let db = Connection::open(&live).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             INSERT INTO settings VALUES ('wal-padding',hex(zeroblob(32768)));",
        )
        .unwrap();
        let (live, db) = if opposite_checksum_order {
            let resumed = fixture.root.0.join("resumed.db");
            let mut bytes = fs::read(sidecar(&live, "-wal")).unwrap();
            let little = bytes[3] == 0x82;
            set_wal_checksum_order(&mut bytes, !little);
            fs::copy(&live, &resumed).unwrap();
            fs::write(sidecar(&resumed, "-wal"), bytes).unwrap();
            let resumed_db = Connection::open(&resumed).unwrap();
            (resumed, resumed_db)
        } else {
            (live, db)
        };
        let checkpoint: (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(RESTART)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
        assert_eq!(checkpoint.1, checkpoint.2);
        db.execute_batch(
            "UPDATE pending_change SET payload='first recycled edit' WHERE account_id=42;
             UPDATE pending_change SET payload='latest recycled edit' WHERE account_id=42;",
        )
        .unwrap();
        let bytes = fs::read(sidecar(&live, "-wal")).unwrap();
        let page_size = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let frames: Vec<_> = bytes[32..].chunks_exact(24 + page_size).collect();
        assert_eq!(&frames[0][8..16], &bytes[16..24]);
        assert_eq!(&frames[1][8..16], &bytes[16..24]);
        assert!(frames[2..]
            .iter()
            .any(|frame| frame[8..16] != bytes[16..24]));
        fs::copy(&live, &fixture.db_path).unwrap();
        fs::copy(sidecar(&live, "-wal"), sidecar(&fixture.db_path, "-wal")).unwrap();
    }

    #[test]
    fn recycled_wal_keeps_latest_committed_edit_during_validation() {
        assert_recycled_wal_validation(false);
    }

    #[test]
    fn recycled_wal_accepts_an_older_checksum_byte_order() {
        assert_recycled_wal_validation(true);
    }

    fn assert_recycled_wal_validation(opposite_checksum_order: bool) {
        let fixture = Fixture::new();
        with_recycled_wal(&fixture, opposite_checksum_order);
        let before = profile_fingerprints(&fixture.db_path).unwrap();
        let independent = fixture.root.0.join("sqlite-control.db");
        fs::copy(&fixture.db_path, &independent).unwrap();
        fs::copy(
            sidecar(&fixture.db_path, "-wal"),
            sidecar(&independent, "-wal"),
        )
        .unwrap();
        let control =
            Connection::open_with_flags(&independent, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        validate_schema(&control).unwrap();
        let latest: String = control
            .query_row(
                "SELECT payload FROM pending_change WHERE account_id=42",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest, "latest recycled edit");
        drop(control);
        validate_database(&fixture.db_path).unwrap();
        assert_eq!(before, profile_fingerprints(&fixture.db_path).unwrap());
    }

    #[test]
    fn recycled_wal_without_a_new_commit_keeps_checkpointed_rows() {
        let fixture = Fixture::new();
        let live = fixture.root.0.join("recycled-uncommitted.db");
        copy_private(&fixture.db_path, &live).unwrap();
        let db = Connection::open(&live).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<100)
             INSERT INTO settings SELECT 'padding-'||i, hex(zeroblob(2000)) FROM n;",
        )
        .unwrap();
        let (busy, frames, checkpointed): (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(RESTART)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert_eq!(frames, checkpointed);
        db.execute_batch(
            "BEGIN;
             UPDATE settings SET value=replace(value,'0','1')
             WHERE key LIKE 'padding-%' AND CAST(substr(key,9) AS INTEGER)<=50;",
        )
        .unwrap();
        db.cache_flush().unwrap();
        let bytes = fs::read(sidecar(&live, "-wal")).unwrap();
        let frame_size = 24 + u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let frames = bytes[32..].chunks_exact(frame_size);
        let current: Vec<_> = frames
            .clone()
            .take_while(|frame| frame[8..16] == bytes[16..24])
            .collect();
        assert!(
            current.len() > 1 && current.len() < frames.len(),
            "expected spilled current frames and an old tail, got {} of {} frames",
            current.len(),
            frames.len()
        );
        assert!(current.iter().all(|frame| frame[4..8] == [0; 4]));
        fs::copy(&live, &fixture.db_path).unwrap();
        fs::write(sidecar(&fixture.db_path, "-wal"), &bytes).unwrap();
        drop(db);

        let check_rows = |path: &Path| {
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            validate_schema(&db).unwrap();
            let counts: (i64, i64) = db
                .query_row(
                    "SELECT count(*), sum(value LIKE '%1%') FROM settings WHERE key LIKE 'padding-%'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(counts, (100, 0));
        };
        let control = fixture.root.0.join("sqlite-control.db");
        fs::copy(&fixture.db_path, &control).unwrap();
        fs::write(sidecar(&control, "-wal"), &bytes).unwrap();
        check_rows(&control);

        let wal = sidecar(&fixture.db_path, "-wal");
        let stale = 32 + current.len() * frame_size;
        let second = 32 + frame_size;
        let mut corrupted = bytes[..32 + 2 * frame_size].to_vec();
        corrupted[second + 8..second + 16].copy_from_slice(&bytes[stale + 8..stale + 16]);
        fs::write(&wal, &corrupted).unwrap();
        assert!(validate_database(&fixture.db_path).is_err());
        fs::write(&wal, &bytes).unwrap();

        let before = profile_fingerprints(&fixture.db_path).unwrap();
        validate_database(&fixture.db_path).unwrap();
        assert_eq!(before, profile_fingerprints(&fixture.db_path).unwrap());
    }

    #[test]
    fn recycled_wal_still_rejects_damaged_current_frames_and_mixed_tails() {
        for kind in [
            "current_checksum",
            "current_salt",
            "forged_old_salt",
            "return_to_current",
            "obsolete_checksum",
        ] {
            let fixture = Fixture::new();
            with_recycled_wal(&fixture, false);
            let wal = sidecar(&fixture.db_path, "-wal");
            let mut bytes = fs::read(&wal).unwrap();
            let frame_size = 24 + u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let second = 32 + frame_size;
            let third = second + frame_size;
            match kind {
                "current_checksum" => bytes[second + 24] ^= 1,
                "current_salt" => bytes[second + 12] ^= 1,
                "forged_old_salt" => {
                    let old = bytes[third + 8..third + 16].to_vec();
                    bytes[second + 8..second + 16].copy_from_slice(&old);
                }
                "return_to_current" => {
                    let current = bytes[16..24].to_vec();
                    bytes[third + frame_size + 8..third + frame_size + 16]
                        .copy_from_slice(&current);
                }
                "obsolete_checksum" => bytes[third + frame_size + 24] ^= 1,
                _ => unreachable!(),
            }
            fs::write(&wal, bytes).unwrap();
            let before = profile_fingerprints(&fixture.db_path).unwrap();
            assert!(validate_database(&fixture.db_path).is_err(), "{kind}");
            assert_eq!(
                before,
                profile_fingerprints(&fixture.db_path).unwrap(),
                "{kind}"
            );
        }
    }

    #[test]
    fn wal_damage_and_reset_tails_stop_validation() {
        for kind in [
            "header",
            "truncated",
            "salt",
            "checksum",
            "page_size",
            "short_main",
        ] {
            let fixture = Fixture::new();
            with_wal(&fixture);
            let wal = sidecar(&fixture.db_path, "-wal");
            let mut bytes = fs::read(&wal).unwrap();
            match kind {
                "header" => bytes[0] = 0,
                "truncated" => {
                    bytes.pop();
                }
                "salt" => bytes[40] ^= 1,
                "checksum" => bytes[60] ^= 1,
                "page_size" => bytes[10] ^= 1,
                "short_main" => fs::write(&fixture.db_path, b"SQLite format 3\x00\x01").unwrap(),
                _ => unreachable!(),
            }
            fs::write(&wal, bytes).unwrap();
            let before = profile_fingerprints(&fixture.db_path).unwrap();
            let error = validate_database(&fixture.db_path).unwrap_err().to_string();
            assert!(
                error.contains("WAL is incomplete, damaged or unsupported"),
                "{kind}: {error}"
            );
            assert_eq!(
                before,
                profile_fingerprints(&fixture.db_path).unwrap(),
                "{kind}"
            );
        }
    }

    #[test]
    fn restricted_paths_are_rejected_before_access() {
        assert!(check_path(Path::new("relative/file")).is_err());
        assert!(check_path(Path::new("/synthetic/../file")).is_err());
        assert!(check_path(Path::new("/synthetic/HeNtAi-file/kurisu.db")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_local_paths_reject_network_devices() {
        let fixture = Fixture::new();
        let root = &fixture.root.0;
        let canonical = root.canonicalize().unwrap();
        check_path(&canonical).unwrap();
        validate_database(&fixture.db_path.canonicalize().unwrap()).unwrap();
        for path in [
            r"\\server\share\kurisu.db",
            r"\\?\UNC\server\share\kurisu.db",
            r"\\.\C:\kurisu.db",
        ] {
            let error = check_path(Path::new(path)).unwrap_err().to_string();
            assert!(error.contains("local drive"), "{path}: {error}");
        }
    }
}
