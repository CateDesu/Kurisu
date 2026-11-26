use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use gpui::{
    AppContext, Context, Entity, FocusHandle, Render, Role, Window, div, prelude::*, px, rgb,
};
use kurisu_core::{Core, Services, db_validate};

use crate::{
    components::{button, heading},
    platform::{self, InstanceError, InstanceGuard, NativePlatform},
    shell::{Quit, Shell, TabBackward, TabForward},
    theme::{BASE, DIM, ERROR, INK, sans_font},
};

#[cfg(all(test, target_os = "linux"))]
#[path = "../../core/tests/fixtures/recovery.rs"]
mod recovery;

struct DefaultPaths {
    native: PathBuf,
    previous_native: Option<PathBuf>,
}

fn default_paths() -> Result<DefaultPaths, String> {
    let native = directories::ProjectDirs::from("", "CateDesu", "kurisu-native")
        .ok_or("No user data directory available")?;
    let previous = native.data_dir().to_path_buf();
    let destination = native.data_local_dir().to_path_buf();
    Ok(DefaultPaths {
        previous_native: (previous != destination).then_some(previous),
        native: destination,
    })
}

fn prepare_profile_with(
    argument: Option<PathBuf>,
    resolve_default: impl FnOnce() -> Result<DefaultPaths, String>,
) -> Result<PathBuf, String> {
    if let Some(path) = argument {
        return platform::data_dir(Some(path));
    }
    prepare_default(resolve_default()?)
}

fn prepare_default(paths: DefaultPaths) -> Result<PathBuf, String> {
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
    platform::data_dir(Some(paths.native))
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
    let mut instance = match InstanceGuard::acquire(&data_dir) {
        Ok(instance) => instance,
        Err(InstanceError::Activated) => return Ok(OpenOutcome::Activated),
        Err(InstanceError::Declined) => {
            return Err(
                "This data profile is already open, and its window was minimized or hidden. Retry to bring it forward."
                    .into(),
            );
        }
        Err(InstanceError::Occupied) => {
            return Err(
                "This data profile is already open, but its window did not respond. Close the other instance or retry."
                    .into(),
            );
        }
        Err(InstanceError::Open(error)) => return Err(error),
    };
    db_validate::validate_database(&data_dir.join("kurisu.db")).map_err(|error| {
        format!(
            "The native profile could not be validated: {error:#}. Keep this profile and its sidecars for recovery before retrying. Profile: {}",
            data_dir.display()
        )
    })?;
    let core = Core::open(data_dir.clone(), runtime, Arc::new(NativePlatform))?;
    if let Err(error) = instance.start_activation(&data_dir) {
        log::warn!("Could not enable profile window activation: {error}");
    }
    Ok(OpenOutcome::Ready(Ready {
        core,
        instance: Arc::new(instance),
    }))
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
    use kurisu_core::Db;
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_profile_recovers_through_open_profile() {
        use recovery::{Process, Sandbox};
        use std::process::Command;

        const TEST: &str = "startup::tests::fresh_profile_recovers_through_open_profile";
        if let Some(path) = std::env::var_os("KURISU_FRESH_PROFILE") {
            let path = PathBuf::from(path);
            let runtime = tokio::runtime::Runtime::new().unwrap();
            if let Ok(expected) = std::env::var("KURISU_FRESH_EXPECT_ERROR") {
                let error = open_profile(Some(path), runtime.handle().clone())
                    .err()
                    .expect("initialization should reject the collision");
                assert!(error.contains(&expected), "{error}");
                return;
            }
            if std::env::var_os("KURISU_FRESH_RETRY").is_some() {
                let first = open_profile(Some(path.clone()), runtime.handle().clone());
                assert!(
                    first.is_err(),
                    "the requested storage fault was not exercised"
                );
                eprintln!("injected first-open failure: {}", first.err().unwrap());
                if std::env::var_os("KURISU_FRESH_PARENT_SYNC").is_some() {
                    let error = open_profile(Some(path.clone()), runtime.handle().clone())
                        .err()
                        .expect("retry must still wait for the parent sync");
                    assert!(
                        error.contains("Could not sync the parent directory"),
                        "{error}"
                    );
                    assert!(!path.join("kurisu.db").exists());
                }
                std::fs::remove_file(std::env::var_os("KURISU_RECOVERY_ARM").unwrap()).unwrap();
                if std::env::var("KURISU_FRESH_RETRY").unwrap() == "restart" {
                    return;
                }
            }
            let outcome = open_profile(Some(path), runtime.handle().clone())
                .unwrap_or_else(|error| panic!("Retry/restart failed: {error}"));
            assert!(
                matches!(outcome, OpenOutcome::Ready(_))
                    || (std::env::var_os("KURISU_FRESH_CONCURRENT").is_some()
                        && matches!(outcome, OpenOutcome::Activated))
            );
            return;
        }

        let sandbox = Sandbox::new("profile");
        let shim = sandbox.shim();
        let command = |path: &std::path::Path| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture"])
                .env("KURISU_FRESH_PROFILE", path);
            command
        };
        for (recovery, fail_deep) in [("retry", false), ("restart", true)] {
            let root = sandbox.0.join(format!("ancestor-{recovery}"));
            std::fs::create_dir(&root).unwrap();
            let parent = root.join("data");
            let profile = parent.join("native");
            let arm = root.join("arm");
            std::fs::write(&arm, []).unwrap();
            Process(
                command(&profile)
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_FRESH_RETRY", recovery)
                    .env("KURISU_FRESH_PARENT_SYNC", "1")
                    .env("KURISU_RECOVERY_OP", "sync")
                    .env(
                        "KURISU_RECOVERY_MATCH",
                        if fail_deep { &parent } else { &root },
                    )
                    .env("KURISU_RECOVERY_MATCH_EXACT", "1")
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .spawn()
                    .unwrap(),
            )
            .success();
            Process(command(&profile).spawn().unwrap()).success();
            assert!(platform::has_native_profile(&profile).unwrap());
        }
        for (operation, name, expected) in [
            ("rename", ".kurisu-native-init", "File exists"),
            ("create", "kurisu.db", "conflicts with initialization"),
        ] {
            let root = sandbox.0.join(format!("collision-{operation}"));
            std::fs::create_dir(&root).unwrap();
            let profile = root.join("native");
            let ready = profile.join(".kurisu-native-init");
            let collision = profile.join(name);
            let arm = root.join("arm");
            let gate = root.join("gate");
            std::fs::write(&arm, []).unwrap();
            let mut child = command(&profile);
            if operation == "rename" {
                child
                    .env("KURISU_RECOVERY_MATCH", &ready)
                    .env("KURISU_RECOVERY_MATCH_EXACT", "1");
            } else {
                child
                    .env("KURISU_RECOVERY_MATCH", "/.kurisu-validate-")
                    .env("KURISU_RECOVERY_AFTER", &ready);
            }
            let mut child = Process(
                child
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_FRESH_EXPECT_ERROR", expected)
                    .env("KURISU_RECOVERY_OP", operation)
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            child.gate(&gate);
            if operation == "rename" {
                std::fs::create_dir(&collision).unwrap();
            } else {
                std::fs::write(&collision, b"preserve collision").unwrap();
            }
            std::fs::remove_file(&arm).unwrap();
            child.success();
            if operation == "rename" {
                assert!(std::fs::read_dir(&collision).unwrap().next().is_none());
                assert!(!profile.join("kurisu.db").exists());
                std::fs::remove_dir(&collision).unwrap();
            } else {
                assert_eq!(std::fs::read(&collision).unwrap(), b"preserve collision");
                assert!(ready.join("kurisu.db").is_file());
                std::fs::remove_file(&collision).unwrap();
            }
            assert!(!profile.join(".kurisu-native-profile").exists());
            Process(command(&profile).spawn().unwrap()).success();
        }
        for (index, (operation, name)) in [
            ("create", "/kurisu.db"),
            ("create", "/.kurisu-native-profile"),
            ("write", "/.kurisu-native-profile"),
            ("sync", "/.kurisu-native-profile"),
        ]
        .into_iter()
        .enumerate()
        {
            for recovery in ["retry", "restart"] {
                let profile = sandbox.0.join(format!("fault-{index}-{recovery}"));
                std::fs::create_dir(&profile).unwrap();
                std::fs::write(profile.join("unrelated"), b"keep user data").unwrap();
                let arm = sandbox.0.join(format!("arm-{index}"));
                std::fs::write(&arm, []).unwrap();
                Process(
                    command(&profile)
                        .env("LD_PRELOAD", &shim)
                        .env("KURISU_FRESH_RETRY", recovery)
                        .env("KURISU_RECOVERY_OP", operation)
                        .env("KURISU_RECOVERY_MATCH", name)
                        .env("KURISU_RECOVERY_ARM", &arm)
                        .spawn()
                        .unwrap(),
                )
                .success();
                Process(command(&profile).spawn().unwrap()).success();
                assert_eq!(
                    std::fs::read(profile.join("unrelated")).unwrap(),
                    b"keep user data"
                );
                assert!(!profile.join(".kurisu-native-init").exists());
            }
        }

        for (index, (operation, name)) in [
            ("sync", "/.kurisu-native-profile"),
            ("renamed", "/.kurisu-native-init"),
            ("linked", "/kurisu.db"),
            ("linked", "/.kurisu-native-profile"),
        ]
        .into_iter()
        .enumerate()
        {
            let profile = sandbox.0.join(format!("crash-{index}"));
            let arm = sandbox.0.join(format!("crash-arm-{index}"));
            let gate = sandbox.0.join(format!("crash-gate-{index}"));
            std::fs::write(&arm, []).unwrap();
            let mut child = Process(
                command(&profile)
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_RECOVERY_OP", operation)
                    .env("KURISU_RECOVERY_MATCH", name)
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            child.gate(&gate);
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            std::fs::remove_file(&arm).unwrap();
            let defaults = DefaultPaths {
                native: profile.clone(),
                previous_native: None,
            };
            assert_eq!(prepare_default(defaults).unwrap(), profile);
            Process(command(&profile).spawn().unwrap()).success();
        }

        let profile = sandbox.0.join("concurrent");
        let arm = sandbox.0.join("concurrent-arm");
        let gate = sandbox.0.join("concurrent-gate");
        std::fs::write(&arm, []).unwrap();
        let mut first = Process(
            command(&profile)
                .env("KURISU_FRESH_CONCURRENT", "1")
                .env("LD_PRELOAD", &shim)
                .env("KURISU_RECOVERY_OP", "sync")
                .env("KURISU_RECOVERY_MATCH", "/.kurisu-native-profile")
                .env("KURISU_RECOVERY_ARM", &arm)
                .env("KURISU_RECOVERY_GATE", &gate)
                .spawn()
                .unwrap(),
        );
        first.gate(&gate);
        let second_arm = sandbox.0.join("second-arm");
        let second_gate = sandbox.0.join("second-gate");
        std::fs::write(&second_arm, []).unwrap();
        let mut second = Process(
            command(&profile)
                .env("KURISU_FRESH_CONCURRENT", "1")
                .env("LD_PRELOAD", &shim)
                .env("KURISU_RECOVERY_OP", "create")
                .env("KURISU_RECOVERY_MATCH", "/.kurisu-native-init.lock")
                .env("KURISU_RECOVERY_ARM", &second_arm)
                .env("KURISU_RECOVERY_GATE", &second_gate)
                .spawn()
                .unwrap(),
        );
        second.gate(&second_gate);
        std::fs::remove_file(&second_arm).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(second.0.try_wait().unwrap().is_none());
        assert!(!profile.join("kurisu.db").exists());
        std::fs::remove_file(&arm).unwrap();
        first.success();
        second.success();
        assert!(platform::has_native_profile(&profile).unwrap());
        assert!(!profile.join(".kurisu-native-init").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_validation_survives_process_death() {
        use recovery::{Process, Sandbox};
        use std::process::Command;

        const TEST: &str = "startup::tests::prepared_validation_survives_process_death";
        if let Some(profile) = std::env::var_os("KURISU_VALIDATION_PROFILE") {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            assert!(matches!(
                open_profile(Some(profile.into()), runtime.handle().clone())
                    .unwrap_or_else(|error| panic!("validation recovery failed: {error}")),
                OpenOutcome::Ready(_)
            ));
            return;
        }
        let sandbox = Sandbox::new("validation-crash");
        let shim = sandbox.shim();
        for recovery in [false, true] {
            let profile = sandbox.0.join(format!("profile-{recovery}"));
            let command = || {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", TEST, "--nocapture"])
                    .env("KURISU_VALIDATION_PROFILE", &profile);
                command
            };
            let arm = sandbox.0.join("arm");
            let gate = sandbox.0.join("gate");
            let ready = profile.join(".kurisu-native-init");
            if recovery {
                std::fs::write(&arm, []).unwrap();
                let mut child = Process(
                    command()
                        .env("LD_PRELOAD", &shim)
                        .env("KURISU_RECOVERY_OP", "renamed")
                        .env("KURISU_RECOVERY_MATCH", "/.kurisu-native-init")
                        .env("KURISU_RECOVERY_ARM", &arm)
                        .env("KURISU_RECOVERY_GATE", &gate)
                        .spawn()
                        .unwrap(),
                );
                child.gate(&gate);
                child.0.kill().unwrap();
                child.0.wait().unwrap();
                std::fs::remove_file(&gate).unwrap();
            }
            std::fs::write(&arm, []).unwrap();
            let mut child = Process(
                command()
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_RECOVERY_OP", "create")
                    .env("KURISU_RECOVERY_MATCH", "/.kurisu-validate-")
                    .env("KURISU_RECOVERY_AFTER", &ready)
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            child.gate(&gate);
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            std::fs::remove_file(&arm).unwrap();
            std::fs::remove_file(&gate).unwrap();
            assert!(ready.join("kurisu.db").is_file());
            assert!(!profile.join("kurisu.db").exists());
            let unknown = ready.join("unrelated");
            std::fs::write(&unknown, b"preserve").unwrap();
            assert!(
                platform::data_dir(Some(profile.clone()))
                    .unwrap_err()
                    .contains("unexpected")
            );
            assert_eq!(std::fs::read(&unknown).unwrap(), b"preserve");
            std::fs::remove_file(unknown).unwrap();
            Process(command().spawn().unwrap()).success();
            Process(command().spawn().unwrap()).success();
            assert!(!ready.exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn validation_in_progress_cannot_acknowledge_a_duplicate_launch() {
        use recovery::{Process, Sandbox};
        use std::process::Command;

        const TEST: &str =
            "startup::tests::validation_in_progress_cannot_acknowledge_a_duplicate_launch";
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        if let Some(profile) = std::env::var_os("KURISU_VALIDATION_HANDOFF_PROFILE") {
            assert!(matches!(
                open_profile(Some(profile.into()), runtime.handle().clone()).unwrap(),
                OpenOutcome::Ready(_)
            ));
            return;
        }
        let sandbox = Sandbox::new("validation-handoff");
        let profile = platform::data_dir(Some(sandbox.0.join("profile"))).unwrap();
        let arm = sandbox.0.join("arm");
        let gate = sandbox.0.join("gate");
        std::fs::write(&arm, []).unwrap();
        let mut child = Process(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("KURISU_VALIDATION_HANDOFF_PROFILE", &profile)
                .env("LD_PRELOAD", sandbox.shim())
                .env("KURISU_RECOVERY_OP", "create")
                .env("KURISU_RECOVERY_MATCH", "/.kurisu-validate-")
                .env("KURISU_RECOVERY_ARM", &arm)
                .env("KURISU_RECOVERY_GATE", &gate)
                .spawn()
                .unwrap(),
        );
        child.gate(&gate);
        let result = open_profile(Some(profile.clone()), runtime.handle().clone());
        assert!(result.is_err_and(|error| error.contains("window did not respond")));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(matches!(
            open_profile(Some(profile), runtime.handle().clone()).unwrap(),
            OpenOutcome::Ready(_)
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fresh_profile_without_hard_links() {
        use recovery::{Process, Sandbox};
        use std::process::Command;

        const TEST: &str = "startup::tests::fresh_profile_without_hard_links";
        if let Some(profile) = std::env::var_os("KURISU_NO_LINK_PROFILE") {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let profile = PathBuf::from(profile);
            if std::env::var_os("KURISU_RECOVERY_COLLISION").is_some() {
                let error = open_profile(Some(profile), runtime.handle().clone())
                    .err()
                    .expect("the competing database should prevent publication");
                assert_eq!(
                    error,
                    std::io::Error::from_raw_os_error(libc::EEXIST).to_string()
                );
                return;
            }
            if let Ok(recovery) = std::env::var("KURISU_NO_LINK_RETRY") {
                let first_error = open_profile(Some(profile.clone()), runtime.handle().clone())
                    .err()
                    .expect("the requested storage fault was not exercised");
                if recovery == "record-directory" {
                    let ready = profile.join(".kurisu-native-init");
                    assert!(ready.join(".publish-by-move").is_file());
                    let error = open_profile(Some(profile.clone()), runtime.handle().clone())
                        .err()
                        .expect("retry should still wait for the move record sync");
                    assert_eq!(error, first_error);
                    assert!(ready.join("kurisu.db").is_file());
                    assert!(!profile.join("kurisu.db").exists());
                }
                std::fs::remove_file(std::env::var_os("KURISU_RECOVERY_ARM").unwrap()).unwrap();
                if recovery == "restart" {
                    return;
                }
            }
            assert!(matches!(
                open_profile(Some(profile), runtime.handle().clone())
                    .unwrap_or_else(|error| panic!("no-link recovery failed: {error}")),
                OpenOutcome::Ready(_)
            ));
            return;
        }
        let sandbox = Sandbox::new("no-links");
        let shim = sandbox.shim();
        let command = |profile: &std::path::Path| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", TEST, "--nocapture"])
                .env("KURISU_NO_LINK_PROFILE", profile)
                .env("LD_PRELOAD", &shim)
                .env("KURISU_RECOVERY_NO_LINKS", "/.kurisu-native-init/");
            command
        };
        let profile = sandbox.0.join("fresh");
        std::fs::create_dir(&profile).unwrap();
        std::fs::write(profile.join("unrelated"), b"preserve").unwrap();
        Process(command(&profile).spawn().unwrap()).success();
        Process(command(&profile).spawn().unwrap()).success();
        assert_eq!(
            std::fs::read(profile.join("unrelated")).unwrap(),
            b"preserve"
        );
        assert!(!profile.join(".kurisu-native-init").exists());

        let profile = sandbox.0.join("eperm");
        Process(
            command(&profile)
                .env("KURISU_RECOVERY_LINK_ERRNO", "perm")
                .spawn()
                .unwrap(),
        )
        .success();
        Process(
            command(&profile)
                .env("KURISU_RECOVERY_LINK_ERRNO", "perm")
                .spawn()
                .unwrap(),
        )
        .success();

        let profile = sandbox.0.join("record-directory");
        let ready = profile.join(".kurisu-native-init");
        let arm = sandbox.0.join("arm");
        std::fs::write(&arm, []).unwrap();
        Process(
            command(&profile)
                .env("KURISU_NO_LINK_RETRY", "record-directory")
                .env("KURISU_RECOVERY_OP", "sync")
                .env("KURISU_RECOVERY_MATCH", &ready)
                .env("KURISU_RECOVERY_MATCH_EXACT", "1")
                .env("KURISU_RECOVERY_AFTER", ready.join(".publish-by-move"))
                .env("KURISU_RECOVERY_ARM", &arm)
                .spawn()
                .unwrap(),
        )
        .success();
        assert!(!ready.exists());

        for operation in ["create", "write", "sync"] {
            for recovery in ["retry", "restart"] {
                let profile = sandbox.0.join(format!("record-{operation}-{recovery}"));
                let arm = sandbox.0.join("arm");
                std::fs::write(&arm, []).unwrap();
                Process(
                    command(&profile)
                        .env("KURISU_NO_LINK_RETRY", recovery)
                        .env("KURISU_RECOVERY_OP", operation)
                        .env("KURISU_RECOVERY_MATCH", "/.publish-by-move")
                        .env("KURISU_RECOVERY_ARM", &arm)
                        .spawn()
                        .unwrap(),
                )
                .success();
                Process(command(&profile).spawn().unwrap()).success();
                assert!(!profile.join(".kurisu-native-init").exists());
            }
        }

        let profile = sandbox.0.join("partly-linked");
        let arm = sandbox.0.join("arm");
        let gate = sandbox.0.join("gate");
        std::fs::write(&arm, []).unwrap();
        let mut child = Process(
            command(&profile)
                .env_remove("KURISU_RECOVERY_NO_LINKS")
                .env("KURISU_RECOVERY_OP", "linked")
                .env("KURISU_RECOVERY_MATCH", "/kurisu.db")
                .env("KURISU_RECOVERY_ARM", &arm)
                .env("KURISU_RECOVERY_GATE", &gate)
                .spawn()
                .unwrap(),
        );
        child.gate(&gate);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        std::fs::remove_file(&arm).unwrap();
        std::fs::remove_file(&gate).unwrap();
        Process(command(&profile).spawn().unwrap()).success();
        assert!(!profile.join(".kurisu-native-init").exists());

        for (index, (operation, name)) in [
            ("write", "/.publish-by-move"),
            ("sync", "/.publish-by-move"),
            ("renamed", "/.publish-by-move"),
            ("renamed", "/kurisu.db"),
            ("renamed", "/.kurisu-native-profile"),
        ]
        .into_iter()
        .enumerate()
        {
            let profile = sandbox.0.join(format!("interrupted-{index}"));
            let arm = sandbox.0.join("arm");
            let gate = sandbox.0.join("gate");
            std::fs::write(&arm, []).unwrap();
            let mut child = Process(
                command(&profile)
                    .env("KURISU_RECOVERY_OP", operation)
                    .env("KURISU_RECOVERY_MATCH", name)
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            child.gate(&gate);
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            std::fs::remove_file(&arm).unwrap();
            std::fs::remove_file(&gate).unwrap();
            Process(command(&profile).spawn().unwrap()).success();
            Process(command(&profile).spawn().unwrap()).success();
            assert!(!profile.join(".kurisu-native-init").exists());
        }

        let profile = sandbox.0.join("collision");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let arm = sandbox.0.join("arm");
        let gate = sandbox.0.join("gate");
        std::fs::write(&arm, []).unwrap();
        let mut child = Process(
            command(&profile)
                .env("KURISU_RECOVERY_OP", "renamed")
                .env("KURISU_RECOVERY_MATCH", "/.kurisu-native-init")
                .env("KURISU_RECOVERY_ARM", &arm)
                .env("KURISU_RECOVERY_GATE", &gate)
                .spawn()
                .unwrap(),
        );
        child.gate(&gate);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        std::fs::remove_file(&arm).unwrap();
        std::fs::remove_file(&gate).unwrap();
        // The fixture creates the destination just before renameat2.
        Process(
            command(&profile)
                .env("KURISU_RECOVERY_COLLISION", profile.join("kurisu.db"))
                .spawn()
                .unwrap(),
        )
        .success();
        assert_eq!(
            std::fs::read(profile.join("kurisu.db")).unwrap(),
            b"preserve collision"
        );
        assert!(!profile.join(".kurisu-native-profile").exists());
        assert!(open_profile(Some(profile.clone()), runtime.handle().clone()).is_err());
        assert_eq!(
            std::fs::read(profile.join("kurisu.db")).unwrap(),
            b"preserve collision"
        );
    }

    #[cfg(target_os = "linux")]
    fn linux_update_fixture(sandbox: &recovery::Sandbox) -> PathBuf {
        // The updater copies and syncs this executable several times per case.
        let fixture = sandbox.0.join("fixture");
        let status = std::process::Command::new("strip")
            .arg("--strip-debug")
            .arg("-o")
            .arg(&fixture)
            .arg(std::env::current_exe().unwrap())
            .status()
            .expect("run strip on the Linux update fixture");
        assert!(status.success(), "strip exited with {status}");
        fixture
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_update_reports_install_directory_sync_failures() {
        use kurisu_core::updater::{
            apply_linux_update, init_install_path, sweep_install_dir, update_applied,
        };
        use recovery::{Process, Sandbox};
        use std::io::Write;
        use std::process::Command;

        const TEST: &str = "startup::tests::linux_update_reports_install_directory_sync_failures";
        if let Ok(phase) = std::env::var("KURISU_UPDATE_SYNC_PHASE") {
            init_install_path().unwrap();
            let exe = std::env::current_exe().unwrap();
            if phase == "sweep" {
                sweep_install_dir();
            } else {
                let payload = exe.with_file_name("payload");
                let error =
                    apply_linux_update(&mut std::fs::File::open(payload).unwrap()).unwrap_err();
                assert!(error.contains("os error 28"), "{error}");
                let detail = if phase == "backup" {
                    "could not secure the rollback executable"
                } else {
                    "the executable was replaced, but the installation could not be synced"
                };
                assert!(error.contains(detail), "{error}");
                assert_eq!(update_applied(), phase == "swap");
            }
            return;
        }

        let sandbox = Sandbox::new("update-dir-sync");
        let shim = sandbox.shim();
        let fixture = linux_update_fixture(&sandbox);
        let fixture_size = std::fs::metadata(&fixture).unwrap().len();
        for (phase, skip, swapped) in [("backup", "0", false), ("swap", "1", true)] {
            let dir = sandbox.0.join(phase);
            std::fs::create_dir(&dir).unwrap();
            let exe = dir.join("install");
            let backup = exe.with_file_name("install.kurisu-old");
            let payload = exe.with_file_name("payload");
            std::fs::copy(&fixture, &exe).unwrap();
            std::fs::copy(&fixture, &payload).unwrap();
            let suffix = b"replacement";
            std::fs::OpenOptions::new()
                .append(true)
                .open(&payload)
                .unwrap()
                .write_all(suffix)
                .unwrap();
            let arm = dir.join("arm");
            std::fs::write(&arm, []).unwrap();
            let command = |phase: &str, skip: &str| {
                let mut cmd = Command::new(&exe);
                cmd.args(["--exact", TEST, "--nocapture"])
                    .env("KURISU_UPDATE_SYNC_PHASE", phase)
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_RECOVERY_OP", "sync")
                    .env("KURISU_RECOVERY_MATCH", &dir)
                    .env("KURISU_RECOVERY_MATCH_EXACT", "1")
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_SKIP", skip)
                    .env("KURISU_RECOVERY_NO_LINKS", &exe);
                cmd
            };
            Process(command(phase, skip).spawn().unwrap()).success();
            assert_eq!(std::fs::metadata(&backup).unwrap().len(), fixture_size);
            assert_eq!(
                std::fs::metadata(&exe).unwrap().len(),
                fixture_size + if swapped { suffix.len() as u64 } else { 0 }
            );
            Process(command("sweep", "0").spawn().unwrap()).success();
            assert!(backup.is_file());
            if swapped {
                Process(command("backup", "0").spawn().unwrap()).success();
                assert_eq!(std::fs::metadata(&backup).unwrap().len(), fixture_size);
                std::fs::remove_file(&arm).unwrap();
                Process(command("sweep", "0").spawn().unwrap()).success();
                assert!(!backup.exists());
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_linux_update_survives_other_profile_startup_cleanup() {
        use kurisu_core::updater::{apply_linux_update, init_install_path, sweep_install_dir};
        use recovery::{Process, Sandbox};
        use std::io::{Read, Write};
        use std::process::Command;

        const TEST: &str =
            "startup::tests::live_linux_update_survives_other_profile_startup_cleanup";
        if let Ok(mode) = std::env::var("KURISU_INSTALL_CASE") {
            let path = init_install_path();
            if mode == "backup-sweep" {
                assert!(path.is_err());
            } else {
                path.unwrap();
            }
            let exe = std::env::current_exe().unwrap();
            let dir = exe.parent().unwrap();
            let profile = dir.join(format!("profile-{mode}"));
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let OpenOutcome::Ready(_ready) =
                open_profile(Some(profile), runtime.handle().clone()).unwrap()
            else {
                panic!("startup must open the test profile")
            };
            if mode == "waiting-sweep" {
                std::fs::write(dir.join("sweep-ready"), []).unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !dir.join("sweep-go").exists() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if mode.ends_with("sweep") {
                assert!(!sweep_install_dir());
                return;
            }
            if mode == "apply-valid" {
                let payload = dir.join("payload");
                apply_linux_update(&mut std::fs::File::open(payload).unwrap()).unwrap();
                return;
            }
            let mut header = [0; 20];
            std::fs::File::open(&exe)
                .unwrap()
                .read_exact(&mut header)
                .unwrap();
            let mut replacement = header.to_vec();
            replacement.extend_from_slice(b"kurisu isolated update fixture");
            let payload = dir.join("payload");
            std::fs::write(&payload, &replacement).unwrap();
            if mode == "busy" {
                let error =
                    apply_linux_update(&mut std::fs::File::open(payload).unwrap()).unwrap_err();
                assert!(error.contains("already in progress"), "{error}");
                return;
            }
            apply_linux_update(&mut std::fs::File::open(payload).unwrap()).unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), replacement);
            assert!(dir.join("install.kurisu-old").is_file());
            return;
        }

        let phases = ["sync", "linked"];
        let selected = std::env::var_os("KURISU_RECOVERY_UPDATE_PHASE");
        if let Some(phase) = &selected {
            assert!(
                phases.iter().any(|&name| phase == name),
                "unknown update fixture phase {phase:?}"
            );
        }
        let sandbox = Sandbox::new("update");
        let shim = sandbox.shim();
        let fixture = linux_update_fixture(&sandbox);
        let command = |path: &std::path::Path, mode: &str| {
            let mut command = Command::new(path);
            command
                .args(["--exact", TEST, "--nocapture"])
                .env("KURISU_INSTALL_CASE", mode);
            command
        };
        for (index, operation) in phases.into_iter().enumerate() {
            if selected.as_ref().is_some_and(|phase| phase != operation) {
                continue;
            }
            let dir = sandbox.0.join(format!("install-{index}"));
            std::fs::create_dir(&dir).unwrap();
            let installer = dir.join("install");
            let sweeper = dir.join("sweep");
            let backup = dir.join("install.kurisu-old");
            std::fs::copy(&fixture, &installer).unwrap();
            std::fs::hard_link(&installer, &sweeper).unwrap();
            let original = std::fs::metadata(&installer).unwrap().len();
            std::fs::copy(&installer, &backup).unwrap();
            Process(command(&installer, "sweep").spawn().unwrap()).success();
            assert!(backup.is_file());

            let mut waiting = Process(command(&installer, "waiting-sweep").spawn().unwrap());
            waiting.gate(&dir.join("sweep-ready"));

            let arm = dir.join("arm");
            let gate = dir.join("gate");
            std::fs::write(&arm, []).unwrap();
            let mut install = Process(
                command(&installer, "apply")
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_RECOVERY_OP", operation)
                    .env("KURISU_RECOVERY_MATCH", "/.kurisu-new-")
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            install.gate(&gate);
            let stages = || {
                std::fs::read_dir(&dir)
                    .unwrap()
                    .flatten()
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".kurisu-new-")
                    })
                    .map(|entry| entry.path())
                    .collect::<Vec<_>>()
            };
            let live = stages();
            assert_eq!(live.len(), index + 1);
            Process(command(&sweeper, "sweep").spawn().unwrap()).success();
            assert!(
                live.iter().all(|path| path.is_file()),
                "startup removed a live stage"
            );
            Process(command(&sweeper, "busy").spawn().unwrap()).success();
            std::fs::remove_file(&arm).unwrap();
            install.success();
            assert_eq!(std::fs::metadata(&backup).unwrap().len(), original);
            assert!(stages().is_empty());
            assert!(command(&installer, "sweep").spawn().is_err());

            std::fs::write(dir.join("sweep-go"), []).unwrap();
            waiting.success();
            assert!(backup.is_file());

            let abandoned = dir.join(".kurisu-new-abandoned");
            let only_copy = dir.join("missing.kurisu-old");
            std::fs::write(&abandoned, b"abandoned stage").unwrap();
            std::fs::write(&only_copy, b"last working copy").unwrap();
            Process(command(&sweeper, "sweep").spawn().unwrap()).success();
            assert!(!abandoned.exists());
            assert!(backup.is_file());

            let replacement = dir.join("valid-replacement");
            std::fs::copy(&sweeper, &replacement).unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(&replacement)
                .unwrap()
                .write_all(b"valid replacement")
                .unwrap();
            std::fs::rename(&replacement, &installer).unwrap();
            Process(command(&installer, "sweep").spawn().unwrap()).success();
            assert!(!backup.exists());
            assert_eq!(std::fs::read(&only_copy).unwrap(), b"last working copy");
            std::fs::remove_dir_all(dir).unwrap();
        }

        for (label, published, copied) in [
            ("linked-backup", false, false),
            ("copied-backup", false, true),
            ("published", true, false),
        ] {
            let dir = sandbox.0.join(label);
            std::fs::create_dir(&dir).unwrap();
            let installer = dir.join("install");
            let original = dir.join("original");
            let backup = dir.join("install.kurisu-old");
            let payload = dir.join("payload");
            for (path, suffix) in [
                (&installer, b"old executable"),
                (&payload, b"new executable"),
            ] {
                std::fs::copy(&fixture, path).unwrap();
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(suffix)
                    .unwrap();
            }
            std::fs::hard_link(&installer, &original).unwrap();
            let original_size = std::fs::metadata(&original).unwrap().len();

            let arm = dir.join("arm");
            let gate = dir.join("gate");
            std::fs::write(&arm, []).unwrap();
            let mut update = command(&installer, "apply-valid");
            if copied {
                update.env("KURISU_RECOVERY_NO_LINKS", &installer);
            }
            let mut update = Process(
                update
                    .env("LD_PRELOAD", &shim)
                    .env("KURISU_RECOVERY_OP", "renamed")
                    .env(
                        "KURISU_RECOVERY_MATCH",
                        if published { &installer } else { &backup },
                    )
                    .env("KURISU_RECOVERY_MATCH_EXACT", "1")
                    .env("KURISU_RECOVERY_ARM", &arm)
                    .env("KURISU_RECOVERY_GATE", &gate)
                    .spawn()
                    .unwrap(),
            );
            update.gate(&gate);
            assert_eq!(
                same_file::is_same_file(&installer, &original).unwrap(),
                !published
            );
            assert_eq!(
                same_file::is_same_file(&backup, &original).unwrap(),
                !copied
            );
            assert_eq!(std::fs::metadata(&backup).unwrap().len(), original_size);
            update.0.kill().unwrap();
            update.0.wait().unwrap();

            Process(command(&backup, "backup-sweep").spawn().unwrap()).success();
            assert!(backup.is_file());
            Process(command(&installer, "sweep").spawn().unwrap()).success();
            assert_eq!(backup.exists(), !published);
            assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|entry| {
                !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kurisu-new-")
            }));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn initialization_preserves_unmarked_data_and_unrelated_stages() {
        let paths = TestPaths::new();
        let profile = paths.0.join("unmarked");
        std::fs::create_dir(&profile).unwrap();
        for name in [
            "kurisu.db",
            "kurisu.db-wal",
            "kurisu.db-shm",
            "kurisu.db-journal",
        ] {
            std::fs::write(profile.join(name), format!("preserve {name}")).unwrap();
        }
        assert!(
            platform::data_dir(Some(profile.clone()))
                .unwrap_err()
                .contains("lacks")
        );
        for name in [
            "kurisu.db",
            "kurisu.db-wal",
            "kurisu.db-shm",
            "kurisu.db-journal",
        ] {
            assert_eq!(
                std::fs::read(profile.join(name)).unwrap(),
                format!("preserve {name}").as_bytes()
            );
        }
        let stage = profile.join(".kurisu-native-init");
        std::fs::create_dir(&stage).unwrap();
        let fresh = platform::data_dir(Some(paths.0.join("fresh"))).unwrap();
        for name in ["kurisu.db", ".kurisu-native-profile"] {
            std::fs::copy(fresh.join(name), stage.join(name)).unwrap();
        }
        std::fs::remove_file(profile.join("kurisu.db-wal")).unwrap();
        std::fs::remove_file(profile.join("kurisu.db-shm")).unwrap();
        std::fs::remove_file(profile.join("kurisu.db-journal")).unwrap();
        // Even byte-identical data does not establish ownership of an unmarked DB.
        std::fs::copy(stage.join("kurisu.db"), profile.join("kurisu.db")).unwrap();
        let before = std::fs::read(profile.join("kurisu.db")).unwrap();
        assert!(
            platform::data_dir(Some(profile.clone()))
                .unwrap_err()
                .contains("conflicts")
        );
        assert_eq!(std::fs::read(profile.join("kurisu.db")).unwrap(), before);
        assert!(!profile.join(".kurisu-native-profile").exists());
        assert!(stage.join("kurisu.db").exists());
        std::fs::write(
            stage.join(".publish-by-move"),
            b"kurisu-native-init-move-v1\n",
        )
        .unwrap();
        assert!(
            platform::data_dir(Some(profile.clone()))
                .unwrap_err()
                .contains("conflicts")
        );
        assert_eq!(std::fs::read(profile.join("kurisu.db")).unwrap(), before);
        assert!(!profile.join(".kurisu-native-profile").exists());
        assert!(stage.join("kurisu.db").exists());
    }

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
            }
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn explicit_profile_never_discovers_default_profile() {
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
    fn existing_native_profile_keeps_candidate_data() {
        let paths = TestPaths::new();
        let native = platform::data_dir(Some(paths.defaults().native)).unwrap();
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        db.set_setting("anilist_token", "candidate-token").unwrap();
        drop(db);
        assert_eq!(prepare_default(paths.defaults()).unwrap(), native);
        let db = Db::open(&native.join("kurisu.db")).unwrap();
        assert_eq!(
            db.get_setting("anilist_token").unwrap().as_deref(),
            Some("candidate-token")
        );
    }

    #[test]
    fn previous_native_location_remains_usable() {
        let paths = TestPaths::new();
        let previous = platform::data_dir(Some(paths.0.join("previous-native"))).unwrap();
        let mut defaults = paths.defaults();
        defaults.previous_native = Some(previous.clone());
        assert_eq!(prepare_default(defaults).unwrap(), previous);
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
            prepare_default(defaults)
                .unwrap_err()
                .contains("Two native profiles")
        );
        assert!(platform::has_native_profile(&current).unwrap());
        assert!(platform::has_native_profile(&previous).unwrap());
    }

    #[test]
    fn new_install_creates_profile() {
        let paths = TestPaths::new();
        let native = prepare_default(paths.defaults()).unwrap();
        assert!(platform::has_native_profile(&native).unwrap());
    }

    #[test]
    fn missing_or_damaged_native_database_does_not_create_empty_profile() {
        let paths = TestPaths::new();
        let native = paths.defaults().native;
        std::fs::create_dir(&native).unwrap();
        std::fs::write(native.join(".kurisu-native-profile"), b"kurisu-native-v1\n").unwrap();
        let error = prepare_default(paths.defaults()).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert!(!native.join("kurisu.db").exists());
        std::fs::write(native.join("kurisu.db"), b"").unwrap();
        assert!(
            prepare_default(paths.defaults())
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
             INSERT INTO pending_change VALUES(42,7,'{\"progress\":6}');
             UPDATE pending_change SET payload='{\"progress\":7}' WHERE account_id=42 AND media_id=7;",
            )
            .unwrap();
        let native = platform::data_dir(Some(paths.defaults().native)).unwrap();
        let database = native.join("kurisu.db");
        let wal_path = native.join("kurisu.db-wal");
        std::fs::copy(&writer_path, &database).unwrap();
        let valid_wal = std::fs::read(paths.0.join("writer.db-wal")).unwrap();
        drop(writer);
        let mut damaged_wal = valid_wal.clone();
        let page_size = u32::from_be_bytes(valid_wal[8..12].try_into().unwrap()) as usize;
        let last_frame = valid_wal.len() - page_size - 24;
        assert_ne!(valid_wal[last_frame + 4..last_frame + 8], [0; 4]);
        damaged_wal[last_frame + 4..last_frame + 8].fill(0);
        std::fs::write(&wal_path, &damaged_wal).unwrap();
        let before = std::fs::read(&database).unwrap();
        let pending = |db: &Db| -> String {
            db.0.lock()
                .query_row(
                    "SELECT payload FROM pending_change WHERE account_id=42 AND media_id=7",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let control = paths.0.join("control.db");
        std::fs::copy(&database, &control).unwrap();
        std::fs::write(paths.0.join("control.db-wal"), &damaged_wal).unwrap();
        let db = Db::open(&control).unwrap();
        let integrity: String =
            db.0.lock()
                .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                .unwrap();
        assert_eq!(integrity, "ok");
        assert_eq!(pending(&db), r#"{"progress":6}"#);
        drop(db);
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
        assert_eq!(pending(&db), r#"{"progress":7}"#);
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
        std::thread::scope(|scope| {
            let contender = scope.spawn(|| open_profile(argument, runtime.handle().clone()));
            first.instance.wait_for_pending_activations(1);
            assert!(!contender.is_finished());
            first.instance.acknowledge_activations();
            assert!(matches!(
                contender.join().unwrap().unwrap(),
                OpenOutcome::Activated
            ));
        });
    }

    #[test]
    fn damaged_native_schema_blocks_without_repairing() {
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
    }
}
