use std::{
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub struct Sandbox(pub PathBuf);

impl Sandbox {
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "kurisu-recovery-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    pub fn shim(&self) -> PathBuf {
        let source = self.0.join("faults.c");
        let library = self.0.join("faults.so");
        std::fs::write(&source, include_str!("recovery_faults.c")).unwrap();
        assert!(
            Command::new("cc")
                .args(["-shared", "-fPIC", "-Wall", "-Werror"])
                .arg(&source)
                .args(["-ldl", "-o"])
                .arg(&library)
                .status()
                .unwrap()
                .success()
        );
        library
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

pub struct Process(pub Child);

impl Process {
    pub fn success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "child exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "child did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn gate(&mut self, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                self.0.try_wait().unwrap().is_none(),
                "child exited before gate"
            );
            assert!(Instant::now() < deadline, "child did not reach gate");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
