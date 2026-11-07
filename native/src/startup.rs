use std::{
    io::Write,
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    AppContext, Context, Entity, FocusHandle, Render, Role, Window, div, prelude::*, px, rgb,
};
use kurisu_core::{
    Core, Services, UpdateChannel,
    profile_transfer::{self, LegacyPlatform, PREFERENCES_FILE, TransferRequest},
};

use crate::{
    components::{button, heading},
    platform::{self, InstanceGuard, NativePlatform},
    shell::{Quit, Shell, TabBackward, TabForward},
    theme::{BASE, DIM, ERROR, INK, sans_font},
};

struct DefaultPaths {
    native: PathBuf,
    previous_native: Option<PathBuf>,
    legacy: Vec<PathBuf>,
    backup: PathBuf,
}

fn default_paths() -> Result<DefaultPaths, String> {
    let base = directories::BaseDirs::new().ok_or("No user data directory available")?;
    let native = directories::ProjectDirs::from("", "CateDesu", "kurisu-native")
        .ok_or("No user data directory available")?;
    #[cfg(target_os = "windows")]
    let platform = LegacyPlatform::Windows;
    #[cfg(not(target_os = "windows"))]
    let platform = LegacyPlatform::Linux;
    let previous = native.data_dir().to_path_buf();
    let destination = native.data_local_dir().to_path_buf();
    Ok(DefaultPaths {
        previous_native: (previous != destination).then_some(previous),
        native: destination,
        legacy: profile_transfer::legacy_sources(base.data_local_dir(), platform),
        backup: base.data_local_dir().join("kurisu-legacy-backup.db"),
    })
}

fn prepare_profile_with(
    argument: Option<PathBuf>,
    resolve_default: impl FnOnce() -> Result<DefaultPaths, String>,
) -> Result<PathBuf, String> {
    if let Some(path) = argument {
        return platform::data_dir(Some(path));
    }
    prepare_default(resolve_default()?, legacy_stop_guard)
}

fn prepare_default<G>(
    paths: DefaultPaths,
    stopped: impl FnOnce() -> Result<G, String>,
) -> Result<PathBuf, String> {
    let current = platform::has_native_profile(&paths.native)?;
    let previous = match paths.previous_native {
        Some(path) if platform::path_present(&path)? => {
            if !platform::has_native_profile(&path)? {
                return Err(format!(
                    "An earlier native profile is incomplete at {}. Keep it for recovery before retrying.",
                    path.display()
                ));
            }
            Some(path)
        }
        _ => None,
    };
    if current && previous.is_some() {
        return Err("Two native profiles were found. Quit Kurisu and use --data-dir with the profile you want to keep.".into());
    }
    if let Some(path) = previous {
        return platform::data_dir(Some(path));
    }
    if current {
        return platform::data_dir(Some(paths.native));
    }
    let mut sources = Vec::new();
    let mut missing = Vec::new();
    let mut prepared = Vec::new();
    for source in paths.legacy {
        let parent = source.parent().ok_or("Legacy profile has no parent")?;
        if platform::path_present(&source)? {
            if platform::path_present(&parent.join(PREFERENCES_FILE))? {
                prepared.push(source.clone());
            }
            sources.push(source);
        } else if platform::path_present(parent)? {
            missing.push(parent.to_path_buf());
        }
    }
    let source = if prepared.len() == 1 {
        prepared.pop()
    } else if sources.len() > 1 {
        return Err("More than one previous Kurisu profile was found. Open the transition release with the profile you want to move, then retry after resolving the duplicate profiles.".into());
    } else if let Some(parent) = missing.first() {
        return Err(format!(
            "The previous Kurisu profile at {} is missing its database. Restore its database and sidecars before retrying. No empty profile was created.",
            parent.display()
        ));
    } else {
        sources.pop()
    };
    if source.is_none() && platform::path_present(&paths.backup)? {
        return Err(format!(
            "A previous Kurisu backup exists, but its original profile is missing. Preserve this backup and recover the profile before retrying. No empty profile was created. Backup: {}",
            paths.backup.display()
        ));
    }
    if let Some(source_db) = source {
        let _stopped = stopped()?;
        platform::create_private_dir(
            paths
                .native
                .parent()
                .ok_or("Native profile has no parent")?,
        )?;
        platform::create_private_dir(paths.backup.parent().ok_or("Backup has no parent")?)?;
        let preferences = source_db
            .parent()
            .ok_or("Legacy profile has no parent")?
            .join(PREFERENCES_FILE);
        let request = TransferRequest {
            source_db,
            preferences,
            backup_db: paths.backup.clone(),
            destination: paths.native.clone(),
        };
        profile_transfer::transfer(&request).map_err(|error| {
            format!(
                "Could not move your previous Kurisu profile: {error:#}\nQuit the previous Kurisu, including its tray process. If preferences are missing, open the transition release once, then quit it and retry. Keep the original database and all its sidecars together. Backup location: {}",
                paths.backup.display()
            )
        })?;
    }
    platform::data_dir(Some(paths.native))
}

fn legacy_stop_guard() -> Result<LegacyStopGuard, String> {
    stop_guard_at(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 39418)),
        Duration::from_secs(10),
    )
}

struct LegacyStopGuard {
    running: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LegacyStopGuard {
    fn new(listener: TcpListener) -> Result<Self, String> {
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let running = Arc::new(AtomicBool::new(true));
        let active = running.clone();
        let worker = std::thread::Builder::new()
            .name("kurisu-transfer-guard".into())
            .spawn(move || {
                while active.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if stream
                                .set_write_timeout(Some(Duration::from_millis(250)))
                                .is_ok()
                            {
                                // Old clients exit only after receiving Kurisu's ACK.
                                let _ = stream.write_all(b"k");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            running,
            worker: Some(worker),
        })
    }
}

impl Drop for LegacyStopGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn stop_guard_at(address: SocketAddr, wait: Duration) -> Result<LegacyStopGuard, String> {
    let deadline = Instant::now() + wait;
    loop {
        match TcpListener::bind(address) {
            Ok(guard) => return LegacyStopGuard::new(guard),
            Err(error)
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(format!(
                    "The previous Kurisu may still be running. Quit it, including its tray process, then select Retry. Your profile has not been moved. Could not reserve {address}: {error}"
                ));
            }
        }
    }
}

struct Ready {
    core: Core,
    instance: Arc<InstanceGuard>,
}

enum OpenOutcome {
    Ready(Ready),
    Activated,
}

fn open_profile(
    argument: Option<PathBuf>,
    runtime: tokio::runtime::Handle,
) -> Result<OpenOutcome, String> {
    let data_dir = prepare_profile_with(argument, default_paths)?;
    let instance = match InstanceGuard::acquire(&data_dir) {
        Ok(instance) => Arc::new(instance),
        Err(error) if error == "This data profile is already open" => {
            return Ok(OpenOutcome::Activated);
        }
        Err(error) => return Err(error),
    };
    profile_transfer::validate_database(&data_dir.join("kurisu.db")).map_err(|error| {
        format!(
            "The native profile could not be validated: {error:#}. Keep this profile and its sidecars for recovery before retrying. Profile: {}",
            data_dir.display()
        )
    })?;
    let core = Core::open(
        data_dir,
        runtime,
        Arc::new(NativePlatform),
        UpdateChannel::Native,
    )?;
    Ok(OpenOutcome::Ready(Ready { core, instance }))
}

pub(crate) struct Startup {
    argument: Option<PathBuf>,
    runtime: tokio::runtime::Handle,
    services: Arc<Mutex<Option<Services>>>,
    shell: Option<Entity<Shell>>,
    pending: bool,
    error: String,
    focus: FocusHandle,
}

impl Startup {
    pub(crate) fn new(
        argument: Option<PathBuf>,
        runtime: tokio::runtime::Handle,
        services: Arc<Mutex<Option<Services>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        window.on_window_should_close(cx, |_, cx| {
            cx.quit();
            true
        });
        let mut startup = Self {
            argument,
            runtime,
            services,
            shell: None,
            pending: false,
            error: String::new(),
            focus,
        };
        startup.retry(window, cx);
        startup
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending || self.shell.is_some() {
            return;
        }
        self.pending = true;
        self.error.clear();
        let argument = self.argument.clone();
        let runtime = self.runtime.clone();
        let job = self
            .runtime
            .spawn_blocking(move || open_profile(argument, runtime));
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await.unwrap_or_else(|error| Err(error.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending = false;
                match result {
                    Ok(OpenOutcome::Ready(ready)) => {
                        *this.services.lock().unwrap() = Some(ready.core.start_services());
                        this.shell = Some(cx.new(|cx| {
                            Shell::new(
                                ready.core,
                                this.services.clone(),
                                ready.instance,
                                window,
                                cx,
                            )
                        }));
                    }
                    Ok(OpenOutcome::Activated) => cx.quit(),
                    Err(error) => {
                        log::error!("Kurisu startup: {error}");
                        this.error = error;
                        window.focus(&this.focus, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for Startup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(shell) = &self.shell {
            return shell.clone().into_any_element();
        }
        div()
            .id("kurisu-startup")
            .role(Role::Application)
            .aria_label("Kurisu")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .p(px(48.))
            .gap(px(18.))
            .bg(rgb(BASE))
            .text_color(rgb(INK))
            .font(sans_font())
            .text_size(px(15.))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .on_action(cx.listener(|_, _: &TabForward, window, cx| window.focus_next(cx)))
            .on_action(cx.listener(|_, _: &TabBackward, window, cx| window.focus_prev(cx)))
            .child(heading(if self.pending {
                "Preparing Kurisu"
            } else {
                "Kurisu needs your attention"
            }))
            .child(if self.pending {
                div().text_color(rgb(DIM)).child(
                    "Opening your profile. A previous Kurisu process may need a moment to quit.",
                ).into_any_element()
            } else {
                div()
                    .id("startup-error")
                    .debug_selector(|| "startup-error".into())
                    .role(Role::Alert)
                    .aria_label(self.error.clone())
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .text_color(rgb(ERROR))
                    .child(self.error.clone())
                    .into_any_element()
            })
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(
                        button("startup-retry", "Retry", self.pending)
                            .debug_selector(|| "startup-retry".into())
                            .on_click(cx.listener(|this, _, window, cx| this.retry(window, cx))),
                    )
                    .child(
                        button("startup-quit", "Quit", false)
                            .debug_selector(|| "startup-quit".into())
                            .on_click(|_, _, cx| cx.quit()),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurisu_core::{Db, profile_transfer::UI_PREFERENCE_KEYS};

    struct TestPaths(PathBuf);

    impl TestPaths {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            #[cfg(windows)]
            let temporary =
                PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA")).join("Temp");
            #[cfg(not(windows))]
            let temporary = std::env::temp_dir();
            std::fs::create_dir_all(&temporary).unwrap();
            let path = temporary.join(format!("kurisu-startup-{}-{nonce}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn defaults(&self) -> DefaultPaths {
            DefaultPaths {
                native: self.0.join("native"),
                previous_native: None,
                legacy: vec![self.0.join("legacy/kurisu.db")],
                backup: self.0.join("backup.db"),
            }
        }

        fn legacy(&self, preferences: bool) {
            let profile = self.0.join("legacy");
            std::fs::create_dir(&profile).unwrap();
            let db = Db::open(&profile.join("kurisu.db")).unwrap();
            db.set_setting("anilist_token", "controlled-token").unwrap();
            db.set_setting("anilist_user_id", "7").unwrap();
            db.0.lock().execute(
                "INSERT INTO pending_change(account_id,media_id,payload) VALUES(7,42,'{\"progress\":3}')",
                [],
            ).unwrap();
            drop(db);
            if preferences {
                let mut values = serde_json::Map::new();
                for key in UI_PREFERENCE_KEYS {
                    values.insert(key.into(), serde_json::Value::Null);
                }
                values.insert("kurisu.list.q".into(), "prepared query".into());
                values.insert("kurisu.list.synced".into(), "1730000000123".into());
                std::fs::write(
                    profile.join(PREFERENCES_FILE),
                    serde_json::to_vec(&serde_json::json!({
                        "format": "kurisu-ui-preferences-v1",
                        "preferences": values,
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn explicit_profile_never_discovers_legacy_state() {
        let paths = TestPaths::new();
        let destination = paths.0.join("isolated");
        let result = prepare_profile_with(Some(destination.clone()), || {
            panic!("isolated launch must not discover live profiles")
        })
        .unwrap();
        assert_eq!(result, destination.canonicalize().unwrap());
        assert!(platform::has_native_profile(&result).unwrap());
        let db = Db::open(&result.join("kurisu.db")).unwrap();
        assert_eq!(db.get_setting("anilist_token").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_profile_is_private_and_rejects_linked_sidecars() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let paths = TestPaths::new();
        let native = platform::data_dir(Some(paths.0.join("isolated"))).unwrap();
        assert_eq!(
            std::fs::metadata(&native).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(native.join("kurisu.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let target = paths.0.join("unrelated");
        std::fs::write(&target, b"keep this file").unwrap();
        symlink(&target, native.join("kurisu.db-wal")).unwrap();
        let error = platform::data_dir(Some(native)).unwrap_err();
        assert!(error.contains("links"), "{error}");
        assert_eq!(std::fs::read(target).unwrap(), b"keep this file");
    }

    #[gpui::test]
    fn recovery_controls_remain_visible_with_long_errors(cx: &mut gpui::TestAppContext) {
        let paths = TestPaths::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (_startup, cx) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            window.focus(&focus, cx);
            Startup {
                argument: Some(paths.0.join("isolated")),
                runtime: runtime.handle().clone(),
                services: Arc::new(Mutex::new(None)),
                shell: None,
                pending: false,
                error: "Keep your profile for recovery. ".repeat(200),
                focus,
            }
        });
        cx.run_until_parked();
        let error = cx.debug_bounds("startup-error").unwrap();
        let retry = cx.debug_bounds("startup-retry").unwrap();
        let quit = cx.debug_bounds("startup-quit").unwrap();
        assert!(error.size.height <= px(300.));
        assert!(retry.top() >= error.bottom());
        assert!(quit.top() >= error.bottom());
        let viewport = cx.update(|window, _| window.viewport_size());
        assert!(retry.bottom() <= viewport.height);
        assert!(quit.bottom() <= viewport.height);
    }

    #[test]
    fn existing_native_profile_keeps_candidate_data_without_legacy_checks() {
        let paths = TestPaths::new();
        let native = platform::data_dir(Some(paths.defaults().native)).unwrap();
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        db.set_setting("anilist_token", "candidate-token").unwrap();
        drop(db);
        let mut defaults = paths.defaults();
        defaults.legacy = vec![PathBuf::from("/unreachable/Hentai profile/kurisu.db")];
        assert_eq!(
            prepare_default(defaults, || -> Result<(), String> {
                panic!("must not import")
            })
            .unwrap(),
            native
        );
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("candidate-token")
        );
        assert!(!paths.0.join("backup.db").exists());
    }

    #[test]
    fn previous_native_location_remains_usable() {
        let paths = TestPaths::new();
        let previous = platform::data_dir(Some(paths.0.join("previous-native"))).unwrap();
        let mut defaults = paths.defaults();
        defaults.previous_native = Some(previous.clone());
        assert_eq!(prepare_default(defaults, || Ok(())).unwrap(), previous);
        assert!(!paths.defaults().native.exists());
    }

    #[test]
    fn competing_native_profiles_require_an_explicit_choice() {
        let paths = TestPaths::new();
        let current = platform::data_dir(Some(paths.defaults().native)).unwrap();
        let previous = platform::data_dir(Some(paths.0.join("previous-native"))).unwrap();
        let mut defaults = paths.defaults();
        defaults.previous_native = Some(previous.clone());
        assert!(
            prepare_default(defaults, || Ok(()))
                .unwrap_err()
                .contains("Two native profiles")
        );
        assert!(platform::has_native_profile(&current).unwrap());
        assert!(platform::has_native_profile(&previous).unwrap());
        assert!(!paths.0.join("backup.db").exists());
    }

    #[test]
    fn orphaned_backup_blocks_empty_startup_without_touching_backup() {
        let paths = TestPaths::new();
        let backup = paths.0.join("backup.db");
        std::fs::write(&backup, b"preserved previous backup").unwrap();
        let error = prepare_default(paths.defaults(), || Ok(())).unwrap_err();
        assert!(error.contains("original profile is missing"), "{error}");
        assert!(!paths.defaults().native.exists());
        assert_eq!(std::fs::read(backup).unwrap(), b"preserved previous backup");
    }

    #[test]
    fn new_install_creates_profile_without_legacy_or_backup() {
        let paths = TestPaths::new();
        let native = prepare_default(paths.defaults(), || -> Result<(), String> {
            panic!("fresh install has no source to lock")
        })
        .unwrap();
        assert!(platform::has_native_profile(&native).unwrap());
        assert!(!paths.0.join("backup.db").exists());
    }

    #[test]
    fn automatic_transfer_preserves_source_backup_preferences_and_pending_changes() {
        let paths = TestPaths::new();
        paths.legacy(true);
        let source = paths.0.join("legacy/kurisu.db");
        let original = std::fs::read(&source).unwrap();
        let native = prepare_default(paths.defaults(), || Ok(())).unwrap();
        assert_eq!(std::fs::read(source).unwrap(), original);
        assert!(paths.0.join("backup.db").is_file());
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("controlled-token")
        );
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.q")
                .unwrap()
                .as_deref(),
            Some("prepared query")
        );
        assert_eq!(
            db.get_setting("native_preference:kurisu.list.synced")
                .unwrap()
                .as_deref(),
            Some("1730000000")
        );
        let pending: i64 =
            db.0.lock()
                .query_row(
                    "SELECT COUNT(*) FROM pending_change WHERE account_id=7 AND media_id=42",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(pending, 1);
        db.set_setting("anilist_token", "native-only-token")
            .unwrap();
        drop(db);
        let snapshot = std::fs::read(paths.0.join("backup.db")).unwrap();
        std::fs::write(paths.0.join("inspect-backup.db"), &snapshot).unwrap();
        let backup = Db::open(&paths.0.join("inspect-backup.db")).unwrap();
        assert_eq!(
            backup.get_setting("anilist_token").unwrap().as_deref(),
            Some("controlled-token")
        );
        drop(backup);
        assert_eq!(std::fs::read(paths.0.join("backup.db")).unwrap(), snapshot);
        assert!(!paths.0.join("backup.db-wal").exists());
        assert_eq!(
            prepare_default(paths.defaults(), || Ok(())).unwrap(),
            native
        );
    }

    #[test]
    fn missing_preferences_blocks_then_retry_transfers_without_empty_profile() {
        let paths = TestPaths::new();
        paths.legacy(false);
        let error = prepare_default(paths.defaults(), || Ok(())).unwrap_err();
        assert!(error.contains("preferences"), "{error}");
        assert!(!paths.defaults().native.exists());
        assert!(!paths.0.join("backup.db").exists());
        let values: serde_json::Map<String, serde_json::Value> = UI_PREFERENCE_KEYS
            .into_iter()
            .map(|key| (key.into(), serde_json::Value::Null))
            .collect();
        std::fs::write(
            paths.0.join("legacy").join(PREFERENCES_FILE),
            serde_json::to_vec(
                &serde_json::json!({"format":"kurisu-ui-preferences-v1","preferences":values}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(prepare_default(paths.defaults(), || Ok(())).is_ok());
    }

    #[test]
    fn running_legacy_blocks_before_transfer() {
        let paths = TestPaths::new();
        paths.legacy(true);
        assert_eq!(
            prepare_default(paths.defaults(), || Err::<(), _>(
                "Quit legacy first".into()
            ))
            .unwrap_err(),
            "Quit legacy first"
        );
        assert!(!paths.defaults().native.exists());
        assert!(!paths.0.join("backup.db").exists());
    }

    #[test]
    fn missing_or_damaged_native_database_does_not_create_empty_profile() {
        let paths = TestPaths::new();
        let native = paths.defaults().native;
        std::fs::create_dir(&native).unwrap();
        std::fs::write(native.join(".kurisu-native-profile"), b"kurisu-native-v1\n").unwrap();
        let error = prepare_default(paths.defaults(), || Ok(())).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert!(!native.join("kurisu.db").exists());
        std::fs::write(native.join("kurisu.db"), b"").unwrap();
        assert!(
            prepare_default(paths.defaults(), || Ok(()))
                .unwrap_err()
                .contains("damaged")
        );
    }

    #[test]
    fn damaged_native_wal_stops_before_open_and_preserves_pending_recovery() {
        let paths = TestPaths::new();
        let writer_path = paths.0.join("writer.db");
        let writer = Db::open(&writer_path).unwrap();
        writer
            .0
            .lock()
            .execute_batch(
                "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA wal_autocheckpoint=0;
             INSERT INTO pending_change VALUES(42,7,'{\"progress\":6}');",
            )
            .unwrap();
        let native = platform::data_dir(Some(paths.defaults().native)).unwrap();
        let database = native.join("kurisu.db");
        let wal_path = native.join("kurisu.db-wal");
        std::fs::copy(&writer_path, &database).unwrap();
        let valid_wal = std::fs::read(paths.0.join("writer.db-wal")).unwrap();
        drop(writer);
        let mut damaged_wal = valid_wal.clone();
        damaged_wal[48] ^= 1;
        std::fs::write(&wal_path, &damaged_wal).unwrap();
        let before = std::fs::read(&database).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = match open_profile(Some(native.clone()), runtime.handle().clone()) {
            Err(error) => error,
            Ok(_) => panic!("Damaged WAL must stop startup"),
        };
        assert!(error.contains("WAL"), "{error}");
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(std::fs::read(&wal_path).unwrap(), damaged_wal);
        assert!(!native.join("kurisu.db-shm").exists());

        std::fs::write(&wal_path, valid_wal).unwrap();
        let ready = open_profile(Some(native), runtime.handle().clone()).unwrap();
        assert!(matches!(ready, OpenOutcome::Ready(_)));
        drop(ready);
        let db = Db::open(&database).unwrap();
        let pending: i64 =
            db.0.lock()
                .query_row(
                    "SELECT COUNT(*) FROM pending_change WHERE account_id=42 AND media_id=7",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(pending, 1);
    }

    #[test]
    fn missing_legacy_database_and_duplicate_sources_block_empty_startup() {
        let paths = TestPaths::new();
        std::fs::create_dir(paths.0.join("legacy")).unwrap();
        let error = prepare_default(paths.defaults(), || Ok(())).unwrap_err();
        assert!(error.contains("missing its database"), "{error}");
        assert!(!paths.defaults().native.exists());
        std::fs::remove_dir(paths.0.join("legacy")).unwrap();
        paths.legacy(true);
        std::fs::create_dir(paths.0.join("legacy2")).unwrap();
        std::fs::write(paths.0.join("legacy2/kurisu.db"), b"not inspected").unwrap();
        std::fs::copy(
            paths.0.join("legacy").join(PREFERENCES_FILE),
            paths.0.join("legacy2").join(PREFERENCES_FILE),
        )
        .unwrap();
        let mut defaults = paths.defaults();
        defaults.legacy.push(paths.0.join("legacy2/kurisu.db"));
        assert!(
            prepare_default(defaults, || Ok(()))
                .unwrap_err()
                .contains("More than one")
        );
        assert!(!paths.defaults().native.exists());
    }

    #[test]
    fn unique_prepared_source_wins_over_retained_old_database() {
        let paths = TestPaths::new();
        paths.legacy(true);
        std::fs::create_dir(paths.0.join("older")).unwrap();
        std::fs::write(paths.0.join("older/kurisu.db"), b"retained old database").unwrap();
        let mut defaults = paths.defaults();
        defaults.legacy.push(paths.0.join("older/kurisu.db"));
        let native = prepare_default(defaults, || Ok(())).unwrap();
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("controlled-token")
        );
        assert_eq!(
            std::fs::read(paths.0.join("older/kurisu.db")).unwrap(),
            b"retained old database"
        );
    }

    #[test]
    fn second_launch_activates_existing_profile_without_starting_services() {
        let paths = TestPaths::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let argument = Some(paths.0.join("isolated"));
        let first = open_profile(argument.clone(), runtime.handle().clone()).unwrap();
        let OpenOutcome::Ready(first) = first else {
            panic!("first launch must open the controlled profile")
        };
        assert!(matches!(
            open_profile(argument, runtime.handle().clone()).unwrap(),
            OpenOutcome::Activated
        ));
        assert!(first.instance.take_activation());
        assert_eq!(first.core.channel(), UpdateChannel::Native);
    }

    #[test]
    fn damaged_native_schema_blocks_without_repairing_or_importing() {
        let paths = TestPaths::new();
        let native = platform::data_dir(Some(paths.0.join("isolated"))).unwrap();
        let database = native.join("kurisu.db");
        let db = Db::open(&database).unwrap();
        db.0.lock()
            .execute_batch("DROP TABLE pending_change")
            .unwrap();
        drop(db);
        let original = std::fs::read(&database).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = open_profile(Some(native), runtime.handle().clone());
        let Err(error) = result else {
            panic!("damaged native schema must block startup")
        };
        assert!(error.contains("could not be validated"), "{error}");
        assert_eq!(std::fs::read(database).unwrap(), original);
        assert!(!paths.0.join("backup.db").exists());
    }

    #[test]
    fn stopped_source_guard_holds_port_and_refuses_occupied_port() {
        use std::{io::Read, net::TcpStream};
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = occupied.local_addr().unwrap();
        assert!(stop_guard_at(address, Duration::ZERO).is_err());
        drop(occupied);
        let guard = stop_guard_at(address, Duration::ZERO).unwrap();
        assert!(TcpListener::bind(address).is_err());
        for _ in 0..2 {
            let mut client = TcpStream::connect(address).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut ack = [0];
            client.read_exact(&mut ack).unwrap();
            assert_eq!(ack, *b"k");
        }
        let (stopped, complete) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(guard);
            let _ = stopped.send(());
        });
        complete
            .recv_timeout(Duration::from_secs(2))
            .expect("transfer guard must stop without waiting for another connection");
        assert!(TcpListener::bind(address).is_ok());
    }
}
