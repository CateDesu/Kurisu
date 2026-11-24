use std::{
    borrow::Cow,
    fs::{File, TryLockError},
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use gpui::{AssetSource, SharedString};
use kurisu_core::Platform;
use reqwest::Url;

mod profile_init;

pub struct NativePlatform;

impl Platform for NativePlatform {
    fn open_url(&self, url: &str) -> Result<(), String> {
        if !permitted_open_url(url) {
            return Err("Only HTTP, HTTPS and valid magnet links can be opened".into());
        }
        open::that_detached(url).map_err(|error| error.to_string())
    }

    fn open_path(&self, path: &Path) -> Result<(), String> {
        open::that_detached(path).map_err(|error| error.to_string())
    }

    fn reveal_path(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("Path has no parent")?;
        match reveal_with_file_manager(path)? {
            RevealOutcome::Selected => Ok(()),
            RevealOutcome::Unsupported => {
                open::that_detached(parent).map_err(|error| error.to_string())
            }
        }
    }
}

enum RevealOutcome {
    Selected,
    Unsupported,
}

#[cfg(target_os = "linux")]
fn reveal_with_file_manager(path: &Path) -> Result<RevealOutcome, String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    let url = Url::from_file_path(&absolute)
        .map_err(|()| "Could not form a file URL for selection".to_owned())?;
    let target = format!(
        "['{}']",
        url.as_str().replace('\\', "\\\\").replace('\'', "\\'")
    );
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--timeout",
            "3",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
            &target,
            "",
        ])
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(RevealOutcome::Selected),
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr);
            if file_manager_is_unsupported(&message) {
                Ok(RevealOutcome::Unsupported)
            } else {
                Err(format!("Could not select file: {}", message.trim()))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RevealOutcome::Unsupported)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(target_os = "linux")]
fn file_manager_is_unsupported(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "serviceunknown",
        "namehasnoowner",
        "unknownmethod",
        "unknowninterface",
        "unknownobject",
        "not provided by any .service files",
        "cannot autolaunch d-bus",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

#[cfg(target_os = "macos")]
fn reveal_with_file_manager(path: &Path) -> Result<RevealOutcome, String> {
    let result = Command::new("open").arg("-R").arg(path).status();
    match result {
        Ok(status) if status.success() => Ok(RevealOutcome::Selected),
        Ok(status) => Err(format!("Finder could not select file: {status}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RevealOutcome::Unsupported)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(target_os = "windows")]
fn reveal_with_file_manager(path: &Path) -> Result<RevealOutcome, String> {
    let result = Command::new("explorer.exe")
        .arg("/select,")
        .arg(path)
        .status();
    match result {
        Ok(status) if status.success() => Ok(RevealOutcome::Selected),
        Ok(status) => Err(format!("Explorer could not select file: {status}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(RevealOutcome::Unsupported)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn reveal_with_file_manager(_: &Path) -> Result<RevealOutcome, String> {
    Ok(RevealOutcome::Unsupported)
}

fn permitted_open_url(raw: &str) -> bool {
    if raw.len() > 4096 || raw.bytes().any(|byte| byte.is_ascii_control()) {
        return false;
    }
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    match url.scheme() {
        "http" | "https" => {
            url.host_str().is_some() && url.username().is_empty() && url.password().is_none()
        }
        "magnet" => {
            if url.host_str().is_some() || !url.path().is_empty() || url.fragment().is_some() {
                return false;
            }
            let mut hashes = url.query_pairs().filter(|(key, _)| key == "xt");
            let Some((_, exact_topic)) = hashes.next() else {
                return false;
            };
            let Some(hash) = exact_topic.strip_prefix("urn:btih:") else {
                return false;
            };
            let valid_hash = (hash.len() == 40
                && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                || (hash.len() == 32
                    && hash
                        .bytes()
                        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'2'..=b'7')));
            valid_hash && hashes.next().is_none()
        }
        _ => false,
    }
}

pub fn data_dir(argument: Option<PathBuf>) -> Result<PathBuf, String> {
    let path = match argument {
        Some(path) => path,
        None => directories::ProjectDirs::from("", "CateDesu", "kurisu-native")
            .ok_or("No user data directory available")?
            .data_dir()
            .to_path_buf(),
    };
    validate_profile_path(&path)?;
    create_private_dir(&path)?;
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    reject_forbidden(&path)?;
    let _initialization = profile_init::lock(&path)?;
    profile_init::recover(&path)?;
    if !has_native_profile_unlocked(&path)? {
        profile_init::create(&path)?;
    }
    Ok(path)
}

pub(crate) fn create_private_dir(path: &Path) -> Result<(), String> {
    validate_profile_path(path)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|error| error.to_string())
}

pub(crate) fn has_native_profile(path: &Path) -> Result<bool, String> {
    validate_profile_path(path)?;
    if !path_present(path)? {
        return Ok(false);
    }
    let _initialization = profile_init::lock(path)?;
    profile_init::recover(path)?;
    has_native_profile_unlocked(path)
}

fn has_native_profile_unlocked(path: &Path) -> Result<bool, String> {
    let marker = path.join(".kurisu-native-profile");
    if path_present(&marker)? {
        let metadata = std::fs::metadata(&marker).map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.len() != b"kurisu-native-v1\n".len() as u64
            || std::fs::read(&marker).map_err(|error| error.to_string())? != b"kurisu-native-v1\n"
        {
            return Err(format!(
                "The native profile marker is damaged. Keep this profile for recovery: {}",
                path.display()
            ));
        }
        let database = path.join("kurisu.db");
        if !path_present(&database)? {
            return Err(format!(
                "The native profile database is missing. Restore this profile before retrying: {}",
                path.display()
            ));
        }
        let metadata = std::fs::metadata(database).map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "The native profile database is damaged. Keep this profile for recovery: {}",
                path.display()
            ));
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = path.join(format!("kurisu.db{suffix}"));
            if path_present(&sidecar)?
                && !std::fs::metadata(sidecar)
                    .map_err(|error| error.to_string())?
                    .is_file()
            {
                return Err(format!(
                    "Native profile sidecars must be regular files. Keep this profile for recovery: {}",
                    path.display()
                ));
            }
        }
        return Ok(true);
    }
    for name in [
        "kurisu.db",
        "kurisu.db-wal",
        "kurisu.db-shm",
        "kurisu.db-journal",
        ".kurisu-transfer.json",
    ] {
        if path_present(&path.join(name))? {
            return Err("Existing profile data lacks the native profile marker. Keep it in place and use the Kurisu transition release.".into());
        }
    }
    Ok(false)
}

pub(crate) fn path_present(path: &Path) -> Result<bool, String> {
    validate_profile_path(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn validate_profile_path(path: &Path) -> Result<(), String> {
    reject_forbidden(path)?;
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("Data directory cannot use parent traversal".into());
    }
    reject_symlink_components(path)
}

fn reject_forbidden(path: &Path) -> Result<(), String> {
    if path.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("hentai")
    }) {
        return Err("Data directory is outside permitted locations".into());
    }
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if link_or_reparse(&metadata) => {
                return Err("Data directory cannot use symbolic links or reparse points".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn link_or_reparse(metadata: &std::fs::Metadata) -> bool {
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

pub struct InstanceGuard {
    pub profile_id: String,
    _profile_lock: File,
    activate: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

const INSTANCE_LOCK: &str = ".kurisu-native.lock";
#[cfg(windows)]
const INSTANCE_ENDPOINT: &str = ".kurisu-native.endpoint";
#[cfg(not(windows))]
const INSTANCE_ENDPOINT: &str = INSTANCE_LOCK;
const ACTIVATION_WAIT: Duration = Duration::from_secs(1);

fn open_instance_file(path: &Path, writable: bool) -> Result<File, String> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (link_or_reparse(&meta) || !meta.is_file())
    {
        return Err("Profile instance path must be a regular file".into());
    }
    let mut options = File::options();
    options
        .read(true)
        .write(writable)
        .create(writable)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| error.to_string())?;
    let meta = file.metadata().map_err(|error| error.to_string())?;
    if link_or_reparse(&meta) || !meta.is_file() {
        return Err("Profile instance path must be a regular file".into());
    }
    Ok(file)
}

fn activation_secret() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let a = RandomState::new().hash_one(std::process::id());
    let b = RandomState::new().hash_one(std::time::SystemTime::now());
    format!("{a:016x}{b:016x}")
}

fn read_activation_endpoint(path: &Path) -> Option<(u16, String)> {
    let mut content = String::new();
    open_instance_file(&path.join(INSTANCE_ENDPOINT), false)
        .ok()?
        .take(128)
        .read_to_string(&mut content)
        .ok()?;
    let line = content.lines().next()?.trim();
    let (port, secret) = line.split_once(':')?;
    let port = port.parse::<u16>().ok()?;
    (secret.len() == 32 && secret.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| (port, secret.to_string()))
}

impl InstanceGuard {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hash);
        let profile_id = format!("{:016x}", hash.finish());
        let profile_lock = open_instance_file(&path.join(INSTANCE_LOCK), true)?;
        match profile_lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let deadline = Instant::now() + ACTIVATION_WAIT;
                while Instant::now() < deadline {
                    if let Some((port, secret)) = read_activation_endpoint(path)
                        && activate_instance_at(port, &profile_id, &secret, deadline).is_some()
                    {
                        break;
                    }
                    thread::sleep(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(25)),
                    );
                }
                return Err("This data profile is already open".into());
            }
            Err(TryLockError::Error(error)) => return Err(error.to_string()),
        }
        // Tighten legacy lock permissions only after owning the lock and before publishing the secret.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            profile_lock
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        let mut guard = Self {
            profile_id,
            _profile_lock: profile_lock,
            activate: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(true)),
            listener_thread: None,
        };
        if let Err(error) = guard.start_activation(path) {
            log::warn!("Could not enable profile window activation: {error}");
        }
        Ok(guard)
    }

    fn start_activation(&mut self, _path: &Path) -> Result<(), String> {
        let listener = (0..16)
            .find_map(|attempt| TcpListener::bind(instance_address(&self.profile_id, attempt)).ok())
            .ok_or("Could not reserve a window activation port")?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let secret = activation_secret();
        // Windows prevents other processes from reading the lock file.
        #[cfg(windows)]
        let mut endpoint = open_instance_file(&_path.join(INSTANCE_ENDPOINT), true)?;
        #[cfg(not(windows))]
        let mut endpoint = &self._profile_lock;
        endpoint.set_len(0).map_err(|error| error.to_string())?;
        let record = format!("{port}:{secret}\n");
        endpoint
            .write_all(record.as_bytes())
            .map_err(|error| error.to_string())?;

        let activate = self.activate.clone();
        let running = self.running.clone();
        let identity = self.profile_id.clone();
        let expected_secret = secret.into_bytes();
        self.listener_thread = Some(
            thread::Builder::new()
                .spawn(move || {
                    while running.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                if stream.set_nonblocking(false).is_ok()
                                    && stream
                                        .set_read_timeout(Some(Duration::from_millis(250)))
                                        .is_ok()
                                    && stream
                                        .set_write_timeout(Some(Duration::from_millis(250)))
                                        .is_ok()
                                {
                                    let mut token = [0u8; 32];
                                    if stream.read_exact(&mut token).is_ok()
                                        && token == expected_secret.as_slice()
                                    {
                                        // Publish before acknowledging so later UI input can supersede it.
                                        activate.store(true, Ordering::Release);
                                        let _ = stream.write_all(identity.as_bytes());
                                    }
                                }
                            }
                            Err(_) => thread::sleep(Duration::from_millis(80)),
                        }
                    }
                })
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    pub fn take_activation(&self) -> bool {
        self.activate.swap(false, Ordering::AcqRel)
    }
}

fn instance_address(profile_id: &str, attempt: u64) -> std::net::SocketAddr {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (profile_id, attempt).hash(&mut hash);
    let port = 40000 + (hash.finish() % 20000) as u16;
    (std::net::Ipv4Addr::LOCALHOST, port).into()
}

fn activate_instance_at(
    port: u16,
    profile_id: &str,
    secret: &str,
    deadline: Instant,
) -> Option<()> {
    let timeout = || {
        let remaining = deadline.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then_some(remaining.min(Duration::from_millis(250)))
    };
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, timeout()?).ok()?;
    stream.set_write_timeout(Some(timeout()?)).ok()?;
    if stream.write(secret.as_bytes()).ok()? != secret.len() {
        return None;
    }
    let mut reply = [0u8; 16];
    let mut remaining = &mut reply[..];
    while !remaining.is_empty() {
        stream.set_read_timeout(Some(timeout()?)).ok()?;
        let count = stream.read(remaining).ok()?;
        if count == 0 {
            return None;
        }
        remaining = &mut remaining[count..];
    }
    (reply == profile_id.as_bytes()).then_some(())
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.listener_thread.take() {
            let _ = thread.join();
        }
    }
}

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let bytes: Option<&'static [u8]> = match path {
            "icons/list.svg" => Some(include_bytes!("../assets/list.svg")),
            "icons/play.svg" => Some(include_bytes!("../assets/play.svg")),
            "icons/folder.svg" => Some(include_bytes!("../assets/folder.svg")),
            "icons/download.svg" => Some(include_bytes!("../assets/download.svg")),
            "icons/sun.svg" => Some(include_bytes!("../assets/sun.svg")),
            "icons/calendar.svg" => Some(include_bytes!("../assets/calendar.svg")),
            "icons/search.svg" => Some(include_bytes!("../assets/search.svg")),
            "icons/chart.svg" => Some(include_bytes!("../assets/chart.svg")),
            "icons/bell.svg" => Some(include_bytes!("../assets/bell.svg")),
            "icons/sliders.svg" => Some(include_bytes!("../assets/sliders.svg")),
            "icons/back.svg" => Some(include_bytes!("../assets/back.svg")),
            "icons/logout.svg" => Some(include_bytes!("../assets/logout.svg")),
            "icons/x.svg" => Some(include_bytes!("../assets/x.svg")),
            "icons/minimize.svg" => Some(include_bytes!("../assets/minimize.svg")),
            "icons/maximize.svg" => Some(include_bytes!("../assets/maximize.svg")),
            "icons/restore.svg" => Some(include_bytes!("../assets/restore.svg")),
            "icons/gearwork.svg" => Some(include_bytes!("../assets/gearwork.svg")),
            "icons/gearwork-heading.svg" => Some(include_bytes!("../assets/gearwork-heading.svg")),
            "icons/gearwork-heading-accent.svg" => {
                Some(include_bytes!("../assets/gearwork-heading-accent.svg"))
            }
            "icons/wordmark.svg" => Some(include_bytes!("../assets/wordmark.svg")),
            "icons/wordmark-compact.svg" => Some(include_bytes!("../assets/wordmark-compact.svg")),
            _ => None,
        };
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        if path != "icons" {
            return Ok(Vec::new());
        }
        Ok([
            "list",
            "play",
            "folder",
            "download",
            "sun",
            "calendar",
            "search",
            "chart",
            "bell",
            "sliders",
            "back",
            "logout",
            "x",
            "gearwork",
            "gearwork-heading",
            "gearwork-heading-accent",
            "wordmark",
            "wordmark-compact",
        ]
        .into_iter()
        .map(|name| format!("{name}.svg").into())
        .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestProfile(PathBuf);

    impl TestProfile {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kurisu-native-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestProfile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn separate_profiles_get_separate_identity() {
        let a = TestProfile::new("a");
        let b = TestProfile::new("b");
        let left = InstanceGuard::acquire(&a.0).unwrap();
        let right = InstanceGuard::acquire(&b.0).unwrap();
        assert_ne!(left.profile_id, right.profile_id);
        assert!(InstanceGuard::acquire(&a.0).is_err());
    }

    #[test]
    fn instance_listener_stops_before_identity_is_reused() {
        let profile = TestProfile::new("reuse");
        let path = &profile.0;
        let first = InstanceGuard::acquire(path).unwrap();
        let identity = first.profile_id.clone();
        drop(first);
        let second = InstanceGuard::acquire(path).unwrap();
        assert_eq!(second.profile_id, identity);
    }

    #[test]
    fn duplicate_reply_publishes_activation_and_later_requests_survive_consumption() {
        let profile = TestProfile::new("activation-order");
        let first = InstanceGuard::acquire(&profile.0).unwrap();
        assert!(InstanceGuard::acquire(&profile.0).is_err());
        assert!(first.take_activation());
        assert!(!first.take_activation());
        assert!(InstanceGuard::acquire(&profile.0).is_err());
        assert!(first.take_activation());
        assert!(!first.take_activation());
    }

    #[test]
    fn duplicate_waits_for_delayed_endpoint_publication() {
        let profile = TestProfile::new("delayed-endpoint");
        let first = InstanceGuard::acquire(&profile.0).unwrap();
        let path = profile.0.join(INSTANCE_ENDPOINT);
        let (port, secret) = read_activation_endpoint(&profile.0).unwrap();
        let mut endpoint = open_instance_file(&path, true).unwrap();
        endpoint.set_len(0).unwrap();
        let publisher = thread::spawn(move || {
            thread::sleep(Duration::from_millis(350));
            writeln!(endpoint, "{port}:{secret}").unwrap();
        });

        assert_eq!(
            InstanceGuard::acquire(&profile.0).err().as_deref(),
            Some("This data profile is already open")
        );
        publisher.join().unwrap();
        assert!(first.take_activation());
    }

    #[cfg(windows)]
    #[test]
    fn endpoint_error_keeps_profile_open_and_locked() {
        let profile = TestProfile::new("endpoint-error");
        std::fs::create_dir(profile.0.join(INSTANCE_ENDPOINT)).unwrap();
        let first = InstanceGuard::acquire(&profile.0).unwrap();

        assert_eq!(
            InstanceGuard::acquire(&profile.0).err().as_deref(),
            Some("This data profile is already open")
        );
        assert!(!first.take_activation());
    }

    #[test]
    fn occupied_port_cannot_allow_two_instances_of_one_profile() {
        let profile = TestProfile::new("port-collision");
        let path = &profile.0;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hash);
        let profile_id = format!("{:016x}", hash.finish());
        let occupied = TcpListener::bind(instance_address(&profile_id, 0)).unwrap();
        let first = InstanceGuard::acquire(path).unwrap();
        drop(occupied);
        assert!(InstanceGuard::acquire(path).is_err());
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !first.take_activation() {
            assert!(
                std::time::Instant::now() < deadline,
                "Activation was not delivered"
            );
            thread::sleep(Duration::from_millis(1));
        }
        drop(first);
    }

    #[test]
    fn forbidden_data_dir_is_rejected_before_open() {
        assert!(reject_forbidden(Path::new("/tmp/Hentai copy/native")).is_err());
    }

    #[test]
    fn url_policy_accepts_production_magnets_and_web_links() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert!(permitted_open_url(&format!(
            "magnet:?xt=urn:btih:{hash}&dn=My%20Show"
        )));
        assert!(permitted_open_url(
            "magnet:?xt=urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
        ));
        assert!(permitted_open_url("http://nyaa.si/view/123"));
        assert!(permitted_open_url("https://nyaa.si/view/123"));
    }

    #[test]
    fn url_policy_rejects_unsafe_schemes_and_malformed_magnets() {
        assert!(!permitted_open_url("file:///tmp/payload"));
        assert!(!permitted_open_url("javascript:alert(1)"));
        assert!(!permitted_open_url("magnet:?xt=urn:btih:bad"));
        assert!(!permitted_open_url(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&xt=urn:btih:bad"
        ));
        assert!(!permitted_open_url(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\n"
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_reveal_falls_back_only_for_unsupported_manager() {
        assert!(file_manager_is_unsupported(
            "GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown"
        ));
        assert!(file_manager_is_unsupported("Cannot autolaunch D-Bus"));
        assert!(!file_manager_is_unsupported(
            "GDBus.Error:org.freedesktop.DBus.Error.NoReply"
        ));
        assert!(!file_manager_is_unsupported("Permission denied"));
    }

    #[test]
    fn unauthenticated_connection_cannot_activate_instance() {
        let dir = std::env::temp_dir().join(format!(
            "kurisu-instance-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let guard = InstanceGuard::acquire(&dir).unwrap();
        let (port, _secret) = read_activation_endpoint(&dir).unwrap();
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250)).unwrap();
        let _ = stream.write_all(&[b'0'; 32]);
        let mut reply = [0u8; 16];
        assert!(stream.read_exact(&mut reply).is_err());
        thread::sleep(Duration::from_millis(40));
        assert!(!guard.take_activation());
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn existing_instance_lock_permissions_are_tightened_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let profile = TestProfile::new("lock-perms");
        let lock_path = profile.0.join(INSTANCE_LOCK);
        std::fs::write(&lock_path, "").unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o644
        );

        let guard = InstanceGuard::acquire(&profile.0).unwrap();
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            InstanceGuard::acquire(&profile.0).err().as_deref(),
            Some("This data profile is already open")
        );
        assert!(guard.take_activation());
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(guard);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_instance_does_not_change_held_lock_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let profile = TestProfile::new("lock-perms-held");
        let lock_path = profile.0.join(INSTANCE_LOCK);
        let guard = InstanceGuard::acquire(&profile.0).unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            InstanceGuard::acquire(&profile.0).err().as_deref(),
            Some("This data profile is already open")
        );
        assert!(guard.take_activation());
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        drop(guard);

        let reopened = InstanceGuard::acquire(&profile.0).unwrap();
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(reopened);
    }
}
