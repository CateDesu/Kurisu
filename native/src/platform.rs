use std::{
    borrow::Cow,
    fs::{File, TryLockError},
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
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

#[cfg(any(target_os = "windows", test))]
fn explorer_select_arg(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "Path contains invalid UTF-8 for Explorer".to_owned())?;
    let normalized = text.replace('/', r"\");
    let stripped = if let Some(verbatim) = normalized.strip_prefix(r"\\?\") {
        if verbatim.is_empty() {
            return Err("Path contains unsupported characters for Explorer".into());
        }
        if let Some(unc) = verbatim
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case(r"UNC\"))
            .and_then(|_| verbatim.get(4..))
        {
            if unc.trim_matches('\\').is_empty() {
                return Err("Path contains unsupported characters for Explorer".into());
            }
            format!(r"\\{unc}")
        } else {
            let bytes = verbatim.as_bytes();
            let drive_path = bytes.len() >= 2
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes.len() == 2 || bytes[2] == b'\\');
            if drive_path {
                verbatim.to_owned()
            } else {
                normalized
            }
        }
    } else {
        normalized
    };
    let trimmed = stripped.trim_end_matches('\\');
    let target = if trimmed.len() == 2
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
        && trimmed.as_bytes()[1] == b':'
    {
        format!(r"{trimmed}\")
    } else {
        trimmed.to_owned()
    };
    if target.is_empty()
        || target
            .bytes()
            .any(|byte| byte == b'"' || byte.is_ascii_control())
    {
        return Err("Path contains unsupported characters for Explorer".into());
    }
    Ok(format!(r#"/select,"{target}""#))
}

#[cfg(any(target_os = "windows", test))]
fn explorer_exit_succeeded(code: Option<i32>) -> bool {
    // Explorer returns 1 after delegating selection to an existing shell process.
    matches!(code, Some(0 | 1))
}

#[cfg(target_os = "windows")]
fn reveal_with_file_manager(path: &Path) -> Result<RevealOutcome, String> {
    use std::os::windows::process::CommandExt;

    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    let arg = explorer_select_arg(&absolute)?;
    let result = Command::new("explorer.exe").raw_arg(arg).status();
    match result {
        Ok(status) if explorer_exit_succeeded(status.code()) => Ok(RevealOutcome::Selected),
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
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    if !path_present(&path)? {
        create_private_dir(&path)?;
    }
    let path = path.canonicalize().map_err(|error| error.to_string())?;
    reject_forbidden(&path)?;
    let _initialization = profile_init::lock(&path)?;
    profile_init::recover(&path)?;
    if !has_native_profile_unlocked(&path)? {
        sync_parent(&path)?;
        profile_init::create(&path)?;
    } else {
        #[cfg(unix)]
        if path_present(&path.join(kurisu_core::profile_transfer::RECEIPT))? {
            // Sync the selected directory entry without following paths in the receipt.
            sync_parent(&path)?;
        }
    }
    Ok(path)
}

pub(crate) fn create_private_dir(path: &Path) -> Result<(), String> {
    validate_profile_path(path)?;
    #[cfg(unix)]
    return create_private_dir_synced(path);
    #[cfg(not(unix))]
    std::fs::create_dir_all(path).map_err(|error| error.to_string())
}

#[cfg(unix)]
fn create_private_dir_synced(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;

    if !path.is_dir() {
        // Sync each entry before creating descendants. The deepest directory
        // already present may be the last entry made by an interrupted attempt.
        if let Some(parent) = path.parent() {
            create_private_dir_synced(parent)?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        profile_init::sync_directory(parent).map_err(|error| {
            format!(
                "Could not sync the parent directory of {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
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
            match std::fs::symlink_metadata(sidecar) {
                Ok(metadata) if link_or_reparse(&metadata) => {
                    return Err("Data directory cannot use symbolic links or reparse points".into());
                }
                Ok(metadata) if !metadata.is_file() => {
                    return Err(format!(
                        "Native profile sidecars must be regular files. Keep this profile for recovery: {}",
                        path.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        return Ok(true);
    }
    for name in [
        "kurisu.db",
        "kurisu.db-wal",
        "kurisu.db-shm",
        "kurisu.db-journal",
        kurisu_core::profile_transfer::RECEIPT,
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
    activation: Arc<Mutex<Activation>>,
    listener_thread: Option<JoinHandle<()>>,
}

struct Activation {
    running: bool,
    pending: Vec<TcpStream>,
}

fn awaiting_activation(stream: &TcpStream) -> bool {
    matches!(stream.peek(&mut [0]), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
}

#[derive(Debug)]
pub enum InstanceError {
    Activated,
    Declined,
    Occupied,
    Open(String),
}

const INSTANCE_LOCK: &str = ".kurisu-native.lock";
#[cfg(windows)]
const INSTANCE_ENDPOINT: &str = ".kurisu-native.endpoint";
#[cfg(not(windows))]
const INSTANCE_ENDPOINT: &str = INSTANCE_LOCK;
const ACTIVATION_WAIT: Duration = Duration::from_secs(1);
const MAX_PENDING_ACTIVATIONS: usize = 32;
const ACTIVATION_DECLINED: &[u8; 16] = b"request declined";

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
    pub fn acquire(path: &Path) -> Result<Self, InstanceError> {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hash);
        let profile_id = format!("{:016x}", hash.finish());
        let profile_lock =
            open_instance_file(&path.join(INSTANCE_LOCK), true).map_err(InstanceError::Open)?;
        let deadline = Instant::now() + ACTIVATION_WAIT;
        loop {
            match profile_lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::Error(error)) => {
                    return Err(InstanceError::Open(error.to_string()));
                }
                Err(TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= deadline {
                return Err(InstanceError::Occupied);
            }
            if let Some((port, secret)) = read_activation_endpoint(path)
                && let Some(reply) = activate_instance_at(port, &profile_id, &secret, deadline)
            {
                return Err(reply);
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            );
        }
        // Tighten legacy lock permissions only after owning the lock and before publishing the secret.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            profile_lock
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| InstanceError::Open(error.to_string()))?;
        }
        Ok(Self {
            profile_id,
            _profile_lock: profile_lock,
            activation: Arc::new(Mutex::new(Activation {
                running: true,
                pending: Vec::new(),
            })),
            listener_thread: None,
        })
    }

    pub(crate) fn start_activation(&mut self, _path: &Path) -> Result<(), String> {
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

        let activation = self.activation.clone();
        let expected_secret = secret.into_bytes();
        self.listener_thread = Some(
            thread::Builder::new()
                .spawn(move || {
                    while activation.lock().unwrap().running {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                if stream.set_nonblocking(false).is_ok()
                                    && stream
                                        .set_read_timeout(Some(Duration::from_millis(250)))
                                        .is_ok()
                                {
                                    let mut token = [0u8; 32];
                                    if stream.read_exact(&mut token).is_ok()
                                        && token == expected_secret.as_slice()
                                        && stream.set_nonblocking(true).is_ok()
                                    {
                                        let mut activation = activation.lock().unwrap();
                                        activation.pending.retain(awaiting_activation);
                                        if activation.running
                                            && activation.pending.len() < MAX_PENDING_ACTIVATIONS
                                        {
                                            activation.pending.push(stream);
                                        }
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

    pub(crate) fn has_pending_activation(&self) -> bool {
        let mut activation = self.activation.lock().unwrap();
        activation.pending.retain(awaiting_activation);
        !activation.pending.is_empty()
    }

    fn reply_to_activations(&self, reply: &[u8]) {
        let mut activation = self.activation.lock().unwrap();
        for mut stream in activation.pending.drain(..).filter(awaiting_activation) {
            let _ = stream.write_all(reply);
        }
    }

    pub(crate) fn acknowledge_activations(&self) {
        self.reply_to_activations(self.profile_id.as_bytes());
    }

    pub(crate) fn decline_activations(&self) {
        self.reply_to_activations(ACTIVATION_DECLINED);
    }

    pub(crate) fn stop_activation(&self) {
        let mut activation = self.activation.lock().unwrap();
        activation.running = false;
        activation.pending.clear();
    }

    #[cfg(test)]
    pub(crate) fn wait_for_pending_activations(&self, count: usize) {
        let deadline = Instant::now() + ACTIVATION_WAIT;
        loop {
            let pending = self.activation.lock().unwrap().pending.len();
            if pending >= count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "only {pending} requests reached the owner"
            );
            thread::sleep(Duration::from_millis(1));
        }
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
) -> Option<InstanceError> {
    let timeout = || {
        let remaining = deadline.saturating_duration_since(Instant::now());
        (!remaining.is_zero()).then_some(remaining.min(Duration::from_millis(250)))
    };
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, timeout()?).ok()?;
    stream.set_write_timeout(Some(timeout()?)).ok()?;
    stream.write_all(secret.as_bytes()).ok()?;
    let mut reply = [0u8; 16];
    let mut remaining = &mut reply[..];
    while !remaining.is_empty() {
        let remaining_time = deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return None;
        }
        stream
            .set_read_timeout(Some(remaining_time.min(Duration::from_millis(400))))
            .ok()?;
        let count = stream.read(remaining).ok()?;
        if count == 0 {
            return None;
        }
        remaining = &mut remaining[count..];
    }
    if reply == profile_id.as_bytes() {
        Some(InstanceError::Activated)
    } else if reply == *ACTIVATION_DECLINED {
        Some(InstanceError::Declined)
    } else {
        None
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.stop_activation();
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
            "minimize",
            "maximize",
            "restore",
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

    fn active_instance(path: &Path) -> InstanceGuard {
        let mut guard = InstanceGuard::acquire(path).unwrap();
        guard.start_activation(path).unwrap();
        guard
    }

    fn assert_duplicate_activates(owner: &InstanceGuard, path: &Path) {
        thread::scope(|scope| {
            let contender = scope.spawn(|| InstanceGuard::acquire(path));
            owner.wait_for_pending_activations(1);
            assert!(
                !contender.is_finished(),
                "activation was acknowledged before acceptance"
            );
            owner.acknowledge_activations();
            assert!(matches!(
                contender.join().unwrap(),
                Err(InstanceError::Activated)
            ));
            assert!(!owner.has_pending_activation());
        });
    }

    #[test]
    fn separate_profiles_get_separate_identity() {
        let a = TestProfile::new("a");
        let b = TestProfile::new("b");
        let left = active_instance(&a.0);
        let right = active_instance(&b.0);
        assert_ne!(left.profile_id, right.profile_id);
        assert_duplicate_activates(&left, &a.0);
    }

    #[test]
    fn instance_listener_stops_before_identity_is_reused() {
        let profile = TestProfile::new("reuse");
        let path = &profile.0;
        let first = active_instance(path);
        let identity = first.profile_id.clone();
        drop(first);
        let second = active_instance(path);
        assert_eq!(second.profile_id, identity);
    }

    #[test]
    fn duplicate_waits_for_acceptance_and_later_requests_survive_consumption() {
        let profile = TestProfile::new("activation-order");
        let first = active_instance(&profile.0);
        assert_duplicate_activates(&first, &profile.0);
        assert_duplicate_activates(&first, &profile.0);
    }

    #[test]
    fn duplicate_acquires_lock_after_owner_disconnects_without_acknowledgment() {
        let profile = TestProfile::new("released-lock");
        let owner = active_instance(&profile.0);
        thread::scope(|scope| {
            let contender = scope.spawn(|| InstanceGuard::acquire(&profile.0));
            owner.wait_for_pending_activations(1);
            owner.stop_activation();
            drop(owner);
            let guard = contender.join().unwrap().unwrap();
            assert!(!guard.has_pending_activation());
        });
    }

    #[test]
    fn duplicate_reports_occupied_without_acknowledgment() {
        let profile = TestProfile::new("unresponsive-owner");
        let owner = open_instance_file(&profile.0.join(INSTANCE_LOCK), true).unwrap();
        owner.try_lock().unwrap();
        assert!(matches!(
            InstanceGuard::acquire(&profile.0),
            Err(InstanceError::Occupied)
        ));
    }

    #[test]
    fn duplicate_waits_for_delayed_endpoint_publication() {
        let profile = TestProfile::new("delayed-endpoint");
        let mut first = InstanceGuard::acquire(&profile.0).unwrap();
        thread::scope(|scope| {
            let contender = scope.spawn(|| InstanceGuard::acquire(&profile.0));
            thread::sleep(Duration::from_millis(350));
            assert!(!contender.is_finished());
            first.start_activation(&profile.0).unwrap();
            first.wait_for_pending_activations(1);
            first.acknowledge_activations();
            assert!(matches!(
                contender.join().unwrap(),
                Err(InstanceError::Activated)
            ));
        });
    }

    #[cfg(windows)]
    #[test]
    fn endpoint_error_keeps_profile_open_and_locked() {
        let profile = TestProfile::new("endpoint-error");
        std::fs::create_dir(profile.0.join(INSTANCE_ENDPOINT)).unwrap();
        let mut first = InstanceGuard::acquire(&profile.0).unwrap();
        assert!(first.start_activation(&profile.0).is_err());

        assert!(matches!(
            InstanceGuard::acquire(&profile.0),
            Err(InstanceError::Occupied)
        ));
        assert!(!first.has_pending_activation());
    }

    #[test]
    fn occupied_port_cannot_allow_two_instances_of_one_profile() {
        let profile = TestProfile::new("port-collision");
        let path = &profile.0;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hash);
        let profile_id = format!("{:016x}", hash.finish());
        let occupied = TcpListener::bind(instance_address(&profile_id, 0)).unwrap();
        let first = active_instance(path);
        drop(occupied);
        assert_duplicate_activates(&first, path);
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
        let guard = active_instance(&dir);
        let (port, _secret) = read_activation_endpoint(&dir).unwrap();
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250)).unwrap();
        let _ = stream.write_all(&[b'0'; 32]);
        let mut reply = [0u8; 16];
        assert!(stream.read_exact(&mut reply).is_err());
        thread::sleep(Duration::from_millis(40));
        assert!(!guard.has_pending_activation());
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

        let guard = active_instance(&profile.0);
        assert_eq!(
            std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_duplicate_activates(&guard, &profile.0);
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
        let guard = active_instance(&profile.0);
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_duplicate_activates(&guard, &profile.0);
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

    #[test]
    fn assets_list_includes_all_bundled_icons() {
        let listed = Assets.list("icons").unwrap();
        for name in [
            "list.svg",
            "minimize.svg",
            "maximize.svg",
            "restore.svg",
            "x.svg",
            "wordmark-compact.svg",
        ] {
            assert!(
                listed.iter().any(|entry| entry.as_ref() == name),
                "missing {name} in Assets::list(\"icons\")"
            );
        }
        for entry in &listed {
            let loaded = Assets.load(&format!("icons/{entry}")).unwrap();
            assert!(
                loaded.as_ref().is_some_and(|bytes| !bytes.is_empty()),
                "listed icon icons/{entry} failed to load"
            );
        }
    }

    #[test]
    fn explorer_select_formats_quoted_paths_and_accepts_handoff_exit_code() {
        assert_eq!(
            explorer_select_arg(Path::new(r"C:\Anime\Kaguya, Love is War\ep 01.mkv")).unwrap(),
            r#"/select,"C:\Anime\Kaguya, Love is War\ep 01.mkv""#
        );
        assert_eq!(
            explorer_select_arg(Path::new("C:/Anime/Show/ep01.mkv")).unwrap(),
            r#"/select,"C:\Anime\Show\ep01.mkv""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"\\?\C:\Anime\Show\ep01.mkv")).unwrap(),
            r#"/select,"C:\Anime\Show\ep01.mkv""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"\\?\UNC\nas\anime\ep01.mkv")).unwrap(),
            r#"/select,"\\nas\anime\ep01.mkv""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"C:\Anime\Show\")).unwrap(),
            r#"/select,"C:\Anime\Show""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"C:\")).unwrap(),
            r#"/select,"C:\""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"\\?\C:\")).unwrap(),
            r#"/select,"C:\""#
        );
        assert_eq!(
            explorer_select_arg(Path::new(r"\\?\Volume{1234}\ep01.mkv")).unwrap(),
            r#"/select,"\\?\Volume{1234}\ep01.mkv""#
        );
        assert!(explorer_select_arg(Path::new(r#"C:\Anime\bad"name.mkv"#)).is_err());
        assert!(explorer_select_arg(Path::new("C:\\Anime\\bad\nname.mkv")).is_err());
        assert!(explorer_select_arg(Path::new(r"\\?\")).is_err());
        assert!(explorer_select_arg(Path::new(r"\\?\UNC\")).is_err());

        assert!(explorer_exit_succeeded(Some(0)));
        assert!(explorer_exit_succeeded(Some(1)));
        assert!(!explorer_exit_succeeded(Some(2)));
        assert!(!explorer_exit_succeeded(None));
    }
}
