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
const RECEIPT: &str = ".kurisu-transfer.json";
const MAX_PREFERENCES_SIZE: u64 = 1024 * 1024;
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

pub fn encode_preferences(values: &BTreeMap<String, Option<String>>) -> Result<Vec<u8>> {
    ensure!(
        values.len() == UI_PREFERENCE_KEYS.len()
            && UI_PREFERENCE_KEYS
                .iter()
                .all(|key| values.contains_key(*key)),
        "Expected exactly the nine UI preferences"
    );
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

fn load_preferences(path: &Path) -> Result<BTreeMap<String, Option<String>>> {
    regular_file(path)?;
    ensure!(
        fs::metadata(path)?.len() <= MAX_PREFERENCES_SIZE,
        "UI preference export is too large"
    );
    let values: Preferences = serde_json::from_slice(&fs::read(path)?)
        .context("Preference export is not valid UTF-8 JSON")?;
    ensure!(
        values.format == FORMAT,
        "Unsupported preference export format"
    );
    encode_preferences(&values.preferences)?;
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
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
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
        (1..=7).contains(&version),
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
    for (table, required) in columns {
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
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")?
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
    let mut committed = false;
    let mut obsolete = None;
    for _ in 0..remaining / frame_size {
        input.read_exact(&mut frame).map_err(|_| refuse())?;
        ensure!(
            (1..=0xfffffffe).contains(&word(&frame, 0)) && word(&frame, 4) <= 0xfffffffe,
            "{}",
            refuse()
        );
        let frame_salts = (word(&frame, 8), word(&frame, 12));
        if frame_salts != salts {
            // SQLite keeps checkpointed frames when it reuses a WAL.
            ensure!(committed, "{}", refuse());
            if obsolete == Some(frame_salts) {
                sum = checksum(&frame[..8], little, sum);
                sum = checksum(&frame[24..], little, sum);
                ensure!(sum == (word(&frame, 16), word(&frame, 20)), "{}", refuse());
            } else {
                let previous = obsolete.unwrap_or(salts);
                ensure!(
                    (1..=i32::MAX as u32).contains(&previous.0.wrapping_sub(frame_salts.0))
                        && previous.1 != frame_salts.1,
                    "{}",
                    refuse()
                );
            }
            obsolete = Some(frame_salts);
            sum = (word(&frame, 16), word(&frame, 20));
            continue;
        }
        ensure!(obsolete.is_none(), "{}", refuse());
        sum = checksum(&frame[..8], little, sum);
        sum = checksum(&frame[24..], little, sum);
        ensure!(sum == (word(&frame, 16), word(&frame, 20)), "{}", refuse());
        committed |= word(&frame, 4) != 0;
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
    copy_private(source, &copied)?;
    let wal = sidecar(source, "-wal");
    if original.contains_key(&wal) {
        copy_private(&wal, &sidecar(&copied, "-wal"))?;
        validate_wal(&copied, &sidecar(&copied, "-wal"))?;
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
    stage: &Path,
    values: BTreeMap<String, Option<String>>,
    request: &TransferRequest,
) -> Result<()> {
    let database = stage.join("kurisu.db");
    copy_private(backup, &database)?;
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

fn publish_directory(stage: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(stage.as_os_str().as_bytes())?;
        let target = CString::new(destination.as_os_str().as_bytes())?;
        // No replacement, including an empty directory created by another launch.
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
    fs::rename(stage, destination)?;
    #[cfg(not(any(windows, target_os = "linux")))]
    bail!("Profile transfer supports Linux and Windows only");
    sync_directory(destination.parent().context("Missing destination parent")?)
}

/// Copies a stopped legacy profile. Never opens the source with SQLite.
/// The caller must stop both programs and keep them stopped until this returns.
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
        regular_file(&destination.join(MARKER))?;
        regular_file(&destination.join(RECEIPT))?;
        regular_file(&destination.join("kurisu.db"))?;
        ensure!(
            fs::read(destination.join(MARKER))? == MARKER_CONTENT,
            "Existing native profile has an unknown marker"
        );
        let receipt: TransferRequest =
            serde_json::from_slice(&fs::read(destination.join(RECEIPT))?)?;
        ensure!(
            receipt == *request,
            "Existing native profile belongs to another transfer"
        );
        return Ok(TransferOutcome::AlreadyTransferred);
    }
    let preferences_digest = digest(preferences)?;
    let values = load_preferences(preferences)?;
    ensure!(
        digest(preferences)? == preferences_digest,
        "Preferences changed while reading the export"
    );
    let backup_parent = backup.parent().context("Missing backup parent")?;
    let destination_parent = destination.parent().context("Missing destination parent")?;
    ensure!(
        backup_parent.is_dir() && destination_parent.is_dir(),
        "Create destination parents first"
    );
    standalone_if_absent(backup)?;
    let scratch = Stage::new(backup_parent)?;
    let prepared_backup = scratch.0.join("backup.db");
    let source_stage = Stage::new(&scratch.0)?;
    let original = snapshot(source, &source_stage.0, &prepared_backup)?;
    let backup_digest = digest(&prepared_backup)?;
    if absent(backup)? {
        fs::hard_link(&prepared_backup, backup)
            .context("Could not publish standalone backup without replacing existing data")?;
        sync_directory(backup_parent)?;
    } else {
        regular_file(backup)?;
        ensure!(
            digest(backup)? == backup_digest,
            "Existing backup differs from this source. Keep it and choose a new backup path"
        );
    }
    let native_stage = Stage::new(destination_parent)?;
    populate(&prepared_backup, &native_stage.0, values, request)?;
    ensure!(
        digest(backup)? == backup_digest
            && original == fingerprints(source)?
            && digest(preferences)? == preferences_digest,
        "Source, backup or preferences changed during transfer"
    );
    publish_directory(&native_stage.0, destination)?;
    Ok(TransferOutcome::Created)
}

fn standalone_if_absent(path: &Path) -> Result<()> {
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
                 INSERT INTO list_entry VALUES (7,70,'CURRENT',5,8.5,2,101);
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
        let values = load_preferences(&r.preferences).unwrap();
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
    fn versions_one_through_seven_preserve_source_and_backup_version() {
        for version in 1..=7 {
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
    fn damaged_future_and_missing_schema_never_publish() {
        for kind in ["future", "schema", "integrity", "journal", "preferences"] {
            let fixture = Fixture::new();
            let r = &fixture.request;
            match kind {
                "future" => Connection::open(&r.source_db)
                    .unwrap()
                    .pragma_update(None, "user_version", 8)
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
            let magic = if big { 0x377f0683u32 } else { 0x377f0682u32 };
            bytes[..4].copy_from_slice(&magic.to_be_bytes());
            let mut sum = checksum(&bytes[..24], !big, (0, 0));
            bytes[24..28].copy_from_slice(&sum.0.to_be_bytes());
            bytes[28..32].copy_from_slice(&sum.1.to_be_bytes());
            for frame in bytes[32..].chunks_exact_mut(frame_size) {
                sum = checksum(&frame[..8], !big, sum);
                sum = checksum(&frame[24..], !big, sum);
                frame[16..20].copy_from_slice(&sum.0.to_be_bytes());
                frame[20..24].copy_from_slice(&sum.1.to_be_bytes());
            }
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

    fn with_recycled_wal(fixture: &Fixture) {
        let live = fixture.root.0.join("recycled.db");
        copy_private(&fixture.request.source_db, &live).unwrap();
        let db = Connection::open(&live).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             INSERT INTO settings VALUES ('wal-padding',hex(zeroblob(32768)));",
        )
        .unwrap();
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
        let fixture = Fixture::new();
        with_recycled_wal(&fixture);
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
    fn recycled_wal_still_rejects_damaged_current_frames_and_mixed_tails() {
        for kind in [
            "current_checksum",
            "current_salt",
            "forged_old_salt",
            "return_to_current",
            "obsolete_checksum",
        ] {
            let fixture = Fixture::new();
            with_recycled_wal(&fixture);
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
        assert!(publish_directory(&stage.0, &r.destination).is_err());
    }
}
