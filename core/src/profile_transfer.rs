use anyhow::{bail, ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const PREFERENCES_FILE: &str = "kurisu-ui-preferences.json";
pub const UI_PREFERENCE_KEYS: [&str; 9] = [
    "kurisu.list.sort",
    "kurisu.list.dir",
    "kurisu.list.filter",
    "kurisu.list.q",
    "kurisu.list.synced",
    "kurisu.cal.mine",
    "kurisu.torrents.new",
    "kurisu.torrents.statuses",
    "kurisu.torrents.filters",
];
const FORMAT: &str = "kurisu-ui-preferences-v1";
const MARKER: &str = ".kurisu-native-profile";
const MARKER_CONTENT: &[u8] = b"kurisu-native-v1\n";
pub const RECEIPT: &str = ".kurisu-transfer.json";
const MAX_PREFERENCES_SIZE: u64 = 1024 * 1024;
const MAX_RECEIPT_SIZE: u64 = 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub source_db: PathBuf,
    pub preferences: PathBuf,
    pub backup_db: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferOutcome {
    Created,
    AlreadyTransferred,
}

#[derive(Clone, Copy, Debug)]
pub enum LegacyPlatform {
    Linux,
    Windows,
}

/// Paths only. Callers decide which stopped legacy profile to offer.
pub fn legacy_sources(data_local: &Path, platform: LegacyPlatform) -> Vec<PathBuf> {
    let older = match platform {
        LegacyPlatform::Linux => data_local.join("kurisu"),
        LegacyPlatform::Windows => data_local.join("catedesu/kurisu/data"),
    };
    vec![
        data_local.join("com.catedesu.kurisu/kurisu.db"),
        older.join("kurisu.db"),
    ]
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    format: String,
    preferences: BTreeMap<String, Option<String>>,
}

fn validate_preference_keys(values: &BTreeMap<String, Option<String>>) -> Result<()> {
    ensure!(
        values.len() == UI_PREFERENCE_KEYS.len()
            && UI_PREFERENCE_KEYS
                .iter()
                .all(|key| values.contains_key(*key)),
        "Expected exactly the nine UI preferences"
    );
    Ok(())
}

pub fn encode_preferences(values: &BTreeMap<String, Option<String>>) -> Result<Vec<u8>> {
    validate_preference_keys(values)?;
    let mut bytes = serde_json::to_vec_pretty(&Preferences {
        format: FORMAT.into(),
        preferences: values.clone(),
    })?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() as u64 <= MAX_PREFERENCES_SIZE,
        "UI preference export is too large"
    );
    Ok(bytes)
}

fn read_preferences(path: &Path) -> Result<Vec<u8>> {
    regular_file(path)?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_PREFERENCES_SIZE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PREFERENCES_SIZE,
        "UI preference export is too large"
    );
    Ok(bytes)
}

fn load_preferences(bytes: &[u8]) -> Result<BTreeMap<String, Option<String>>> {
    let values: Preferences =
        serde_json::from_slice(bytes).context("Preference export is not valid UTF-8 JSON")?;
    ensure!(
        values.format == FORMAT,
        "Unsupported preference export format"
    );
    validate_preference_keys(&values.preferences)?;
    Ok(values.preferences)
}

fn native_value(key: &str, value: &Option<String>) -> Option<String> {
    if key != "kurisu.list.synced" {
        return value.clone();
    }
    let milliseconds = value.as_ref()?.trim().parse::<f64>().ok()?;
    let seconds = (milliseconds / 1000.0).floor();
    if !milliseconds.is_finite() || milliseconds <= 0.0 || seconds >= 9223372036854775808.0 {
        return None;
    }
    Some((seconds as i64).to_string())
}

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

fn within(path: &Path, parent: &Path) -> bool {
    #[cfg(windows)]
    {
        let path: Vec<_> = path.components().map(windows_component).collect();
        let parent: Vec<_> = parent.components().map(windows_component).collect();
        path.starts_with(&parent)
    }
    #[cfg(not(windows))]
    path.starts_with(parent)
}

#[cfg(windows)]
fn windows_component(component: Component<'_>) -> String {
    if let Component::Prefix(prefix) = component {
        if let std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) =
            prefix.kind()
        {
            return format!("{}:", char::from(drive).to_ascii_lowercase());
        }
    }
    component.as_os_str().to_string_lossy().to_lowercase()
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

fn source_files(source: &Path) -> Result<Vec<PathBuf>> {
    regular_file(source)?;
    ensure!(
        source
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("kurisu.db")),
        "Source must be the legacy kurisu.db shown in Settings"
    );
    ensure!(
        absent(
            &source
                .parent()
                .context("Missing source directory")?
                .join(MARKER)
        )?,
        "Source is already a native profile"
    );
    profile_files(source)
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

fn fingerprints(source: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    source_files(source)?;
    profile_fingerprints(source)
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

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = create_private(path)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_published_parent(path: &Path, description: &str) -> Result<()> {
    let parent = path.parent().context("Missing destination parent")?;
    sync_directory(parent).with_context(|| {
        format!(
            "Could not sync the parent of the published {description} at {}",
            path.display()
        )
    })
}

struct Stage(PathBuf);
impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        for _ in 0..100 {
            let path = parent.join(format!(
                ".kurisu-transfer-{}-{}",
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
        bail!("Could not create private migration staging directory")
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
        "Unsupported legacy schema version: {version}"
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
        anyhow::anyhow!("WAL is incomplete, damaged or unsupported. Preserve the originals and verify a recovered copy before transfer")
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
    File::open(database)?.read_exact(&mut main)?;
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

fn standalone(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        ensure!(
            absent(&sidecar(path, suffix))?,
            "Snapshot still has SQLite sidecars"
        );
    }
    OpenOptions::new().write(true).open(path)?.sync_all()?;
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
        "Source changed during transfer. Stop both programs and retry"
    );
    let source_db = Connection::open_with_flags(&copied, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_schema(&source_db)?;
    Ok((source_db, original))
}

fn snapshot(source: &Path, scratch: &Path, backup: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    source_files(source)?;
    let (source_db, original) = open_snapshot(source, scratch)?;
    create_private(backup)?;
    let mut backup_db = Connection::open(backup)?;
    rusqlite::backup::Backup::new(&source_db, &mut backup_db)?.run_to_completion(
        128,
        Duration::from_millis(10),
        None,
    )?;
    backup_db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    validate_schema(&backup_db)?;
    drop(backup_db);
    drop(source_db);
    standalone(backup)?;
    ensure!(
        original == fingerprints(source)?,
        "Source changed during transfer. Stop both programs and retry"
    );
    Ok(original)
}

fn populate(
    backup: &Path,
    backup_digest: &[u8],
    stage: &Path,
    values: BTreeMap<String, Option<String>>,
    request: &TransferRequest,
) -> Result<()> {
    let database = stage.join("kurisu.db");
    ensure!(
        copy_matches(backup, &database, backup_digest)?,
        "Native database copy differs from the prepared snapshot"
    );
    let mut db = Connection::open(&database)?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    validate_schema(&db)?;
    let tx = db.transaction()?;
    for (key, value) in &values {
        let scoped = format!("native_preference:{key}");
        if let Some(value) = native_value(key, value) {
            tx.execute(
                "INSERT OR REPLACE INTO settings(key,value) VALUES (?,?)",
                [&scoped, &value],
            )?;
        } else {
            tx.execute("DELETE FROM settings WHERE key = ?", [&scoped])?;
        }
    }
    tx.commit()?;
    for (key, value) in values {
        use rusqlite::OptionalExtension;
        let actual: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE key = ?",
                [format!("native_preference:{key}")],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            actual == native_value(&key, &value),
            "Native preference verification failed"
        );
    }
    validate_schema(&db)?;
    drop(db);
    standalone(&database)?;
    write_private(&stage.join(MARKER), MARKER_CONTENT)?;
    write_private(&stage.join(RECEIPT), &serde_json::to_vec(request)?)?;
    sync_directory(stage)
}

fn rename_no_replace(stage: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(stage.as_os_str().as_bytes())?;
        let target = CString::new(destination.as_os_str().as_bytes())?;
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH},
        };
        let source = stage.canonicalize()?;
        let parent = destination.parent().context("Missing destination parent")?;
        let name = destination
            .file_name()
            .context("Missing destination name")?;
        let target = parent.canonicalize()?.join(name);
        let source: Vec<_> = source.as_os_str().encode_wide().chain([0]).collect();
        let target: Vec<_> = target.as_os_str().encode_wide().chain([0]).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )?;
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    bail!("Profile transfer supports Linux and Windows only");
    Ok(())
}

/// Copies a stopped legacy profile. Never opens the source with SQLite.
/// Keep the legacy profile stopped and the native profile unopened during the copy.
pub fn transfer(request: &TransferRequest) -> Result<TransferOutcome> {
    let TransferRequest {
        source_db: source,
        preferences,
        backup_db: backup,
        destination,
    } = request;
    for path in [source, preferences, backup, destination] {
        check_path(path)?;
    }
    #[cfg(windows)]
    {
        let root = PathBuf::from(
            std::env::var_os("LOCALAPPDATA").context("Windows LOCALAPPDATA is unavailable")?,
        );
        check_path(&root)?;
        ensure!(
            within(backup, &root) && within(destination, &root),
            "Place backup and native profile under LOCALAPPDATA"
        );
    }
    let source_parent = source.parent().context("Missing source directory")?;
    ensure!(
        !within(destination, source_parent) && !within(backup, source_parent),
        "Backup and native profile must be outside the legacy profile"
    );
    ensure!(
        !within(backup, destination) && !within(preferences, destination),
        "Inputs and backup must be outside the native profile"
    );
    ensure!(
        [source, preferences, backup, destination]
            .iter()
            .enumerate()
            .all(
                |(index, path)| [source, preferences, backup, destination][..index]
                    .iter()
                    .all(|other| !(within(path, other) && within(other, path)))
            ),
        "Transfer paths must be distinct"
    );
    if !absent(destination)? {
        let database = destination.join("kurisu.db");
        let marker = destination.join(MARKER);
        let receipt = destination.join(RECEIPT);
        regular_file(&marker)?;
        regular_file(&receipt)?;
        let mut contents = Vec::new();
        File::open(marker)?
            .take(MARKER_CONTENT.len() as u64 + 1)
            .read_to_end(&mut contents)?;
        ensure!(
            contents == MARKER_CONTENT,
            "Existing native profile has an unknown marker"
        );
        contents.clear();
        File::open(receipt)?
            .take(MAX_RECEIPT_SIZE + 1)
            .read_to_end(&mut contents)?;
        ensure!(
            contents.len() as u64 <= MAX_RECEIPT_SIZE,
            "Transfer receipt is too large"
        );
        let receipt: TransferRequest = serde_json::from_slice(&contents)?;
        ensure!(
            receipt == *request,
            "Existing native profile belongs to another transfer"
        );
        let scratch_parent = destination.parent().context("Missing destination parent")?;
        validate_database_with_scratch(&database, scratch_parent)
            .context("Could not validate the existing native profile")?;
        sync_published_parent(destination, "native profile")?;
        return Ok(TransferOutcome::AlreadyTransferred);
    }
    let export = read_preferences(preferences)?;
    let values = load_preferences(&export)?;
    let backup_parent = backup.parent().context("Missing backup parent")?;
    let destination_parent = destination.parent().context("Missing destination parent")?;
    ensure!(
        backup_parent.is_dir() && destination_parent.is_dir(),
        "Create destination parents first"
    );
    check_backup_sidecars(backup)?;
    let scratch = Stage::new(backup_parent)?;
    let prepared_backup = scratch.0.join("backup.db");
    let source_stage = Stage::new(&scratch.0)?;
    let original = snapshot(source, &source_stage.0, &prepared_backup)?;
    let backup_digest = digest(&prepared_backup)?;
    let native_source = if absent(backup)? {
        rename_no_replace(&prepared_backup, backup)
            .context("Could not publish standalone backup without replacing existing data")?;
        backup
    } else {
        regular_file(backup)?;
        ensure!(
            digest(backup)? == backup_digest,
            "Existing backup differs from this source. Keep it and choose a new backup path"
        );
        &prepared_backup
    };
    sync_published_parent(backup, "standalone backup")?;
    let native_stage = Stage::new(destination_parent)?;
    populate(
        native_source,
        &backup_digest,
        &native_stage.0,
        values,
        request,
    )?;
    check_backup_sidecars(backup)?;
    ensure!(
        digest(backup)? == backup_digest
            && original == fingerprints(source)?
            && read_preferences(preferences)? == export,
        "Source, backup or preferences changed during transfer"
    );
    rename_no_replace(&native_stage.0, destination).context("Could not publish new profile")?;
    sync_published_parent(destination, "native profile")?;
    Ok(TransferOutcome::Created)
}

fn check_backup_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        ensure!(
            absent(&sidecar(path, suffix))?,
            "Backup path has SQLite sidecars"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: Stage,
        request: TransferRequest,
    }
    impl Fixture {
        fn new() -> Self {
            #[cfg(windows)]
            let parent = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
            #[cfg(not(windows))]
            let parent = std::env::temp_dir();
            let root = Stage::new(&parent).unwrap();
            let legacy = root.0.join("legacy");
            fs::create_dir(&legacy).unwrap();
            let request = TransferRequest {
                source_db: legacy.join("kurisu.db"),
                preferences: legacy.join(PREFERENCES_FILE),
                backup_db: root.0.join("rollback.db"),
                destination: root.0.join("native"),
            };
            let db = crate::Db::open(&request.source_db).unwrap();
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
            let values: BTreeMap<_, _> = UI_PREFERENCE_KEYS
                .into_iter()
                .zip([
                    Some("title"),
                    Some("desc"),
                    Some("CURRENT"),
                    Some(""),
                    Some("1730000000123"),
                    Some("true"),
                    Some("false"),
                    Some("[\"CURRENT\"]"),
                    Some("{\"query\":\"hello\"}"),
                ])
                .map(|(key, value)| (key.into(), value.map(String::from)))
                .collect();
            fs::write(&request.preferences, encode_preferences(&values).unwrap()).unwrap();
            Self { root, request }
        }
    }

    fn all_rows(db: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let mut query = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap();
        let columns = query.column_count();
        query
            .query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn transfers_actual_rows_nine_preferences_and_private_standalone_backup() {
        let fixture = Fixture::new();
        let r = &fixture.request;
        let original = fingerprints(&r.source_db).unwrap();
        assert_eq!(transfer(r).unwrap(), TransferOutcome::Created);
        assert_eq!(fingerprints(&r.source_db).unwrap(), original);
        let source =
            Connection::open_with_flags(&r.source_db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let native = Connection::open(r.destination.join("kurisu.db")).unwrap();
        let backup = Connection::open(&r.backup_db).unwrap();
        for table in [
            "media",
            "list_entry",
            "pending_change",
            "watched_file",
            "watch_history",
            "rss_seen",
        ] {
            assert_eq!(
                all_rows(&source, table),
                all_rows(&native, table),
                "{table}"
            );
            assert_eq!(
                all_rows(&source, table),
                all_rows(&backup, table),
                "backup {table}"
            );
        }
        assert_eq!(all_rows(&source, "settings"), all_rows(&backup, "settings"));
        for row in all_rows(&source, "settings") {
            let value: String = native
                .query_row("SELECT value FROM settings WHERE key = ?", [&row[0]], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(rusqlite::types::Value::Text(value), row[1]);
        }
        let values = load_preferences(&read_preferences(&r.preferences).unwrap()).unwrap();
        for key in UI_PREFERENCE_KEYS {
            let actual: String = native
                .query_row(
                    "SELECT value FROM settings WHERE key = ?",
                    [format!("native_preference:{key}")],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(Some(actual), native_value(key, &values[key]), "{key}");
        }
        drop(source);
        drop(native);
        drop(backup);
        standalone(&r.backup_db).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&r.backup_db).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&r.destination).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn idempotence_keeps_native_pending_changes_and_rejects_existing_candidates() {
        let fixture = Fixture::new();
        let r = &fixture.request;
        transfer(r).unwrap();
        let native = r.destination.join("kurisu.db");
        Connection::open(&native)
            .unwrap()
            .execute("UPDATE pending_change SET payload='native edit'", [])
            .unwrap();
        let before = digest(&native).unwrap();
        assert_eq!(transfer(r).unwrap(), TransferOutcome::AlreadyTransferred);
        assert_eq!(digest(&native).unwrap(), before);
        fs::remove_file(r.destination.join(RECEIPT)).unwrap();
        assert!(transfer(r).is_err());
        assert_eq!(digest(&native).unwrap(), before);
    }

    #[test]
    fn retry_refuses_a_truncated_database_with_matching_receipt() {
        let fixture = Fixture::new();
        let request = &fixture.request;
        transfer(request).unwrap();
        let native = request.destination.join("kurisu.db");
        let truncated = fs::read(&native).unwrap()[..101].to_vec();
        fs::write(&native, &truncated).unwrap();
        fs::remove_file(&request.source_db).unwrap();
        let error = transfer(request).unwrap_err();
        assert!(error
            .to_string()
            .contains("validate the existing native profile"));
        assert_eq!(fs::read(&native).unwrap(), truncated);
    }

    #[test]
    fn preference_size_limit_uses_the_export_bytes() {
        let fixture = Fixture::new();
        let request = &fixture.request;
        let mut export: Preferences =
            serde_json::from_slice(&read_preferences(&request.preferences).unwrap()).unwrap();
        let query = export.preferences.get_mut("kurisu.list.q").unwrap();
        *query = Some(String::new());
        let overhead = serde_json::to_vec(&export).unwrap().len();
        let query = "x".repeat(MAX_PREFERENCES_SIZE as usize - overhead);
        export
            .preferences
            .insert("kurisu.list.q".into(), Some(query));
        let mut bytes = serde_json::to_vec(&export).unwrap();
        bytes.push(b'\n');
        fs::write(&request.preferences, &bytes).unwrap();
        let error = transfer(request).unwrap_err();
        assert!(error.to_string().contains("export is too large"));
        assert!(!request.backup_db.exists());
        bytes.pop();
        fs::write(&request.preferences, &bytes).unwrap();
        assert_eq!(transfer(request).unwrap(), TransferOutcome::Created);
    }

    #[test]
    fn rejects_changed_backup_copy_before_applying_native_preferences() {
        let fixture = Fixture::new();
        let request = &fixture.request;
        let scratch = Stage::new(&fixture.root.0).unwrap();
        snapshot(&request.source_db, &scratch.0, &request.backup_db).unwrap();
        let expected = digest(&request.backup_db).unwrap();
        let changed = Connection::open(&request.backup_db).unwrap();
        changed
            .execute(
                "UPDATE settings SET value='43' WHERE key='anilist_user_id'",
                [],
            )
            .unwrap();
        validate_schema(&changed).unwrap();
        drop(changed);

        let stage = Stage::new(&fixture.root.0).unwrap();
        let values = load_preferences(&read_preferences(&request.preferences).unwrap()).unwrap();
        let error = populate(&request.backup_db, &expected, &stage.0, values, request).unwrap_err();
        assert!(error
            .to_string()
            .contains("differs from the prepared snapshot"));
        assert_eq!(
            digest(&stage.0.join("kurisu.db")).unwrap(),
            digest(&request.backup_db).unwrap()
        );
        assert!(!stage.0.join(MARKER).exists());
        assert!(!stage.0.join(RECEIPT).exists());
        assert!(!request.destination.exists());
    }

    #[test]
    fn interrupted_transfer_reuses_only_an_identical_standalone_backup() {
        let fixture = Fixture::new();
        let r = &fixture.request;
        transfer(r).unwrap();
        let before = digest(&r.backup_db).unwrap();
        fs::remove_dir_all(&r.destination).unwrap();
        assert_eq!(transfer(r).unwrap(), TransferOutcome::Created);
        assert_eq!(digest(&r.backup_db).unwrap(), before);
        fs::remove_dir_all(&r.destination).unwrap();
        Connection::open(&r.source_db)
            .unwrap()
            .execute(
                "UPDATE settings SET value='changed' WHERE key='anilist_user_id'",
                [],
            )
            .unwrap();
        assert!(transfer(r)
            .unwrap_err()
            .to_string()
            .contains("Existing backup differs"));
        assert!(!r.destination.exists());
        assert_eq!(digest(&r.backup_db).unwrap(), before);
    }

    #[test]
    fn null_preferences_delete_stale_keys_and_synced_rejects_invalid_values() {
        for input in [
            None,
            Some(""),
            Some("NaN"),
            Some("inf"),
            Some("-1"),
            Some("1e99"),
        ] {
            assert_eq!(
                native_value("kurisu.list.synced", &input.map(String::from)),
                None
            );
        }
        assert_eq!(
            native_value("kurisu.list.synced", &Some("999".into())),
            Some("0".into())
        );
        let fixture = Fixture::new();
        let r = &fixture.request;
        Connection::open(&r.source_db)
            .unwrap()
            .execute(
                "INSERT INTO settings VALUES ('native_preference:kurisu.list.q','stale')",
                [],
            )
            .unwrap();
        let values = UI_PREFERENCE_KEYS
            .into_iter()
            .map(|key| (key.into(), None))
            .collect();
        fs::write(&r.preferences, encode_preferences(&values).unwrap()).unwrap();
        transfer(r).unwrap();
        let db = Connection::open(r.destination.join("kurisu.db")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM settings WHERE key LIKE 'native_preference:%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn supported_versions_preserve_source_and_backup_version() {
        for version in 1..=crate::db::SCHEMA_VERSION {
            let fixture = Fixture::new();
            let r = &fixture.request;
            Connection::open(&r.source_db)
                .unwrap()
                .pragma_update(None, "user_version", version)
                .unwrap();
            transfer(r).unwrap();
            for path in [&r.source_db, &r.backup_db, &r.destination.join("kurisu.db")] {
                assert_eq!(
                    Connection::open(path)
                        .unwrap()
                        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                        .unwrap(),
                    version
                );
            }
        }
    }

    #[test]
    fn transferred_queue_keys_survive_upgrade_and_reopen() {
        for version in [7, 8] {
            let fixture = Fixture::new();
            let request = &fixture.request;
            let source = Connection::open(&request.source_db).unwrap();
            source.execute_batch(
                "UPDATE list_entry SET entry_id = NULL WHERE media_id = 7;
                 UPDATE pending_change SET payload =
                    '{\"media_id\":7,\"base\":null,\"patch\":{\"status\":\"CURRENT\",\"progress\":5},\"conflict\":false}'
                 WHERE account_id = 42 AND media_id = 7;"
            ).unwrap();
            if version == 7 {
                source.execute_batch("ALTER TABLE list_entry DROP COLUMN queued_creation; PRAGMA user_version = 7;").unwrap();
            } else {
                source.execute_batch(
                    "UPDATE list_entry SET queued_creation = 'fixture-creation';
                     UPDATE pending_change SET payload = json_set(payload, '$.patch.creation_key', 'fixture-creation')
                     WHERE account_id = 42 AND media_id = 7;"
                ).unwrap();
            }
            drop(source);
            let original = fingerprints(&request.source_db).unwrap();
            transfer(request).unwrap();
            let backup = Connection::open(&request.backup_db).unwrap();
            assert_eq!(
                backup
                    .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                version
            );
            let installed = request.destination.join("kurisu.db");
            let key = || {
                crate::Db::open(&installed)
                    .unwrap()
                    .pending_change(42, 7)
                    .unwrap()
                    .unwrap()
                    .patch
                    .creation_key
                    .unwrap()
            };
            let first = key();
            if version == 7 {
                assert_eq!(first.len(), 32);
            } else {
                assert_eq!(first, "fixture-creation");
                assert_eq!(
                    crate::Db::open(&installed)
                        .unwrap()
                        .get_entry(7)
                        .unwrap()
                        .unwrap()
                        .queued_creation
                        .as_deref(),
                    Some(first.as_str())
                );
            }
            assert_eq!(key(), first);
            assert_eq!(fingerprints(&request.source_db).unwrap(), original);
        }
    }

    #[test]
    fn damaged_future_and_missing_schema_never_publish() {
        for kind in ["future", "schema", "integrity", "journal", "preferences"] {
            let fixture = Fixture::new();
            let r = &fixture.request;
            match kind {
                "future" => Connection::open(&r.source_db)
                    .unwrap()
                    .pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)
                    .unwrap(),
                "schema" => Connection::open(&r.source_db)
                    .unwrap()
                    .execute_batch("DROP TABLE pending_change")
                    .unwrap(),
                "integrity" => fs::write(&r.source_db, b"not SQLite").unwrap(),
                "journal" => fs::write(sidecar(&r.source_db, "-journal"), b"pending").unwrap(),
                "preferences" => fs::write(&r.preferences, b"{}").unwrap(),
                _ => unreachable!(),
            }
            assert!(transfer(r).is_err(), "{kind}");
            assert!(!r.destination.exists(), "{kind}");
            assert!(!r.backup_db.exists(), "{kind}");
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
        validate_database(&fixture.request.source_db).unwrap();
    }

    fn with_wal(fixture: &Fixture) {
        let live = fixture.root.0.join("fixture-wal.db");
        copy_private(&fixture.request.source_db, &live).unwrap();
        let db = Connection::open(&live).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE pending_change SET payload='committed WAL edit' WHERE account_id=42").unwrap();
        fs::copy(&live, &fixture.request.source_db).unwrap();
        fs::copy(
            sidecar(&live, "-wal"),
            sidecar(&fixture.request.source_db, "-wal"),
        )
        .unwrap();
    }

    #[test]
    fn committed_wal_rows_survive_without_writing_source_or_sidecars() {
        let fixture = Fixture::new();
        with_wal(&fixture);
        let r = &fixture.request;
        let before = fingerprints(&r.source_db).unwrap();
        transfer(r).unwrap();
        assert_eq!(before, fingerprints(&r.source_db).unwrap());
        for path in [&r.backup_db, &r.destination.join("kurisu.db")] {
            let actual: String = Connection::open(path)
                .unwrap()
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=42",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, "committed WAL edit");
        }
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
            let r = &fixture.request;
            let wal = sidecar(&r.source_db, "-wal");
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
            let before = fingerprints(&r.source_db).unwrap();
            transfer(r).unwrap();
            assert_eq!(fingerprints(&r.source_db).unwrap(), before);
            let actual: String = Connection::open(r.destination.join("kurisu.db"))
                .unwrap()
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=42",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual, "committed WAL edit");
        }
        let fixture = Fixture::new();
        fs::write(sidecar(&fixture.request.source_db, "-wal"), b"").unwrap();
        transfer(&fixture.request).unwrap();
    }

    fn with_recycled_wal(fixture: &Fixture, opposite_checksum_order: bool) {
        let live = fixture.root.0.join("recycled.db");
        copy_private(&fixture.request.source_db, &live).unwrap();
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
        fs::copy(&live, &fixture.request.source_db).unwrap();
        fs::copy(
            sidecar(&live, "-wal"),
            sidecar(&fixture.request.source_db, "-wal"),
        )
        .unwrap();
    }

    #[test]
    fn recycled_wal_keeps_latest_committed_edit_during_validation_and_transfer() {
        assert_recycled_wal_transfer(false);
    }

    #[test]
    fn recycled_wal_accepts_an_older_checksum_byte_order() {
        assert_recycled_wal_transfer(true);
    }

    fn assert_recycled_wal_transfer(opposite_checksum_order: bool) {
        let fixture = Fixture::new();
        with_recycled_wal(&fixture, opposite_checksum_order);
        let r = &fixture.request;
        let before = fingerprints(&r.source_db).unwrap();
        let independent = fixture.root.0.join("sqlite-control.db");
        fs::copy(&r.source_db, &independent).unwrap();
        fs::copy(sidecar(&r.source_db, "-wal"), sidecar(&independent, "-wal")).unwrap();
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
        validate_database(&r.source_db).unwrap();
        assert_eq!(before, fingerprints(&r.source_db).unwrap());
        transfer(r).unwrap();
        assert_eq!(before, fingerprints(&r.source_db).unwrap());
        for path in [&r.backup_db, &r.destination.join("kurisu.db")] {
            let db = Connection::open(path).unwrap();
            let actual: String = db
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=42",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(actual, latest);
        }
    }

    #[test]
    fn recycled_wal_without_a_new_commit_keeps_checkpointed_rows() {
        let fixture = Fixture::new();
        let r = &fixture.request;
        let live = fixture.root.0.join("recycled-uncommitted.db");
        copy_private(&r.source_db, &live).unwrap();
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
        fs::copy(&live, &r.source_db).unwrap();
        fs::write(sidecar(&r.source_db, "-wal"), &bytes).unwrap();
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
        fs::copy(&r.source_db, &control).unwrap();
        fs::write(sidecar(&control, "-wal"), &bytes).unwrap();
        check_rows(&control);

        let wal = sidecar(&r.source_db, "-wal");
        let stale = 32 + current.len() * frame_size;
        let second = 32 + frame_size;
        let mut corrupted = bytes[..32 + 2 * frame_size].to_vec();
        corrupted[second + 8..second + 16].copy_from_slice(&bytes[stale + 8..stale + 16]);
        fs::write(&wal, &corrupted).unwrap();
        assert!(validate_database(&r.source_db).is_err());
        fs::write(&wal, &bytes).unwrap();

        let before = fingerprints(&r.source_db).unwrap();
        validate_database(&r.source_db).unwrap();
        transfer(r).unwrap();
        check_rows(&r.backup_db);
        check_rows(&r.destination.join("kurisu.db"));
        assert_eq!(before, fingerprints(&r.source_db).unwrap());
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
            let r = &fixture.request;
            let wal = sidecar(&r.source_db, "-wal");
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
            let before = fingerprints(&r.source_db).unwrap();
            assert!(validate_database(&r.source_db).is_err(), "{kind}");
            assert!(transfer(r).is_err(), "{kind}");
            assert_eq!(before, fingerprints(&r.source_db).unwrap(), "{kind}");
            assert!(!r.backup_db.exists(), "{kind}");
            assert!(!r.destination.exists(), "{kind}");
        }
    }

    #[test]
    fn wal_damage_and_reset_tails_stop_before_publication() {
        for kind in ["header", "truncated", "salt", "checksum", "page_size"] {
            let fixture = Fixture::new();
            with_wal(&fixture);
            let r = &fixture.request;
            let wal = sidecar(&r.source_db, "-wal");
            let mut bytes = fs::read(&wal).unwrap();
            match kind {
                "header" => bytes[0] = 0,
                "truncated" => {
                    bytes.pop();
                }
                "salt" => bytes[40] ^= 1,
                "checksum" => bytes[60] ^= 1,
                "page_size" => bytes[10] ^= 1,
                _ => unreachable!(),
            }
            fs::write(&wal, bytes).unwrap();
            let before = fingerprints(&r.source_db).unwrap();
            assert!(transfer(r).is_err(), "{kind}");
            assert_eq!(before, fingerprints(&r.source_db).unwrap(), "{kind}");
            assert!(!r.backup_db.exists(), "{kind}");
            assert!(!r.destination.exists(), "{kind}");
        }
    }

    #[test]
    fn path_discovery_is_pure_and_restricted_paths_are_rejected_before_access() {
        assert_eq!(
            legacy_sources(Path::new("/synthetic/local"), LegacyPlatform::Linux),
            vec![
                PathBuf::from("/synthetic/local/com.catedesu.kurisu/kurisu.db"),
                PathBuf::from("/synthetic/local/kurisu/kurisu.db")
            ]
        );
        assert_eq!(
            legacy_sources(Path::new("/synthetic/local"), LegacyPlatform::Windows)[1],
            PathBuf::from("/synthetic/local/catedesu/kurisu/data/kurisu.db")
        );
        assert!(check_path(Path::new("relative/file")).is_err());
        assert!(check_path(Path::new("/synthetic/../file")).is_err());
        assert!(check_path(Path::new("/synthetic/HeNtAi-file/kurisu.db")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_local_paths_keep_drive_ownership_and_reject_network_devices() {
        let fixture = Fixture::new();
        let root = &fixture.root.0;
        let canonical = root.canonicalize().unwrap();
        check_path(&canonical).unwrap();
        validate_database(&fixture.request.source_db.canonicalize().unwrap()).unwrap();
        assert!(within(&canonical, root));
        assert!(within(root, &canonical));
        assert!(within(&canonical.join("child"), root));
        assert!(!within(root, &canonical.join("child")));
        assert!(!within(&canonical, &root.with_file_name("other-profile")));
        for path in [
            r"\\server\share\kurisu.db",
            r"\\?\UNC\server\share\kurisu.db",
            r"\\.\C:\kurisu.db",
        ] {
            let error = check_path(Path::new(path)).unwrap_err().to_string();
            assert!(error.contains("local drive"), "{path}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_and_existing_destinations_are_never_replaced() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let r = &fixture.request;
        fs::create_dir(&r.destination).unwrap();
        assert!(transfer(r).is_err());
        assert!(fs::read_dir(&r.destination).unwrap().next().is_none());
        fs::remove_dir(&r.destination).unwrap();
        symlink(&r.source_db, &r.backup_db).unwrap();
        assert!(transfer(r).is_err());
        assert!(fs::symlink_metadata(&r.backup_db)
            .unwrap()
            .file_type()
            .is_symlink());
        let stage = Stage::new(&fixture.root.0).unwrap();
        fs::create_dir(&r.destination).unwrap();
        assert!(rename_no_replace(&stage.0, &r.destination).is_err());
    }
}
