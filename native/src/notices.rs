pub const LICENSE: &str = include_str!("../../LICENSE");
pub const DEPENDENCIES: &str = include_str!(concat!(env!("OUT_DIR"), "/THIRD_PARTY_NOTICES.txt"));

#[cfg(target_os = "linux")]
pub fn install() -> std::io::Result<()> {
    if DEPENDENCIES.is_empty() {
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    install_in(
        executable
            .parent()
            .ok_or_else(|| std::io::Error::other("no install directory"))?,
        DEPENDENCIES,
    )
}

#[cfg(target_os = "linux")]
fn install_in(directory: &std::path::Path, dependencies: &str) -> std::io::Result<()> {
    install_in_with(directory, dependencies, std::time::SystemTime::now, |_| {})
}

#[cfg(target_os = "linux")]
fn install_in_with(
    directory: &std::path::Path,
    dependencies: &str,
    now: impl Fn() -> std::time::SystemTime,
    mut before_publish: impl FnMut(&std::path::Path),
) -> std::io::Result<()> {
    use std::io::{ErrorKind, Write};

    const MAX_STAGING_RETRIES: usize = 64;

    for (name, text) in [
        ("kurisu.LICENSE", LICENSE),
        ("kurisu.THIRD_PARTY_NOTICES.txt", dependencies),
    ] {
        let destination = directory.join(name);
        match std::fs::symlink_metadata(&destination) {
            Ok(_) => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let nonce = now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        let staging_name = format!(".kurisu-notices-{}-{nonce}", std::process::id());
        let mut staging = directory.join(&staging_name);
        let mut attempt = 0;
        let mut file = loop {
            match std::fs::File::create_new(&staging) {
                Err(error)
                    if error.kind() == ErrorKind::AlreadyExists
                        && attempt < MAX_STAGING_RETRIES =>
                {
                    attempt += 1;
                    staging = directory.join(format!("{staging_name}-{attempt}"));
                }
                result => break result?,
            }
        };
        let result = (|| {
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            before_publish(&destination);
            // Publish complete text without replacing a concurrently created entry.
            match std::fs::hard_link(&staging, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
                Err(error) => return Err(error),
            }
            std::fs::File::open(directory)?.sync_all()
        })();
        drop(file);
        let cleanup = std::fs::remove_file(staging);
        result?;
        cleanup?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "kurisu-notices-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_only_missing_kurisu_companions() {
        let directory = TestDirectory::new();
        install_in(&directory.0, "current dependency texts").unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.LICENSE")).unwrap(),
            LICENSE
        );
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt")).unwrap(),
            "current dependency texts"
        );
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
        install_in(&directory.0, "updated dependency texts").unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt")).unwrap(),
            "current dependency texts"
        );
        std::fs::remove_file(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt")).unwrap();
        install_in(&directory.0, "updated dependency texts").unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt")).unwrap(),
            "updated dependency texts"
        );
    }

    #[test]
    fn preserves_unrelated_files_and_existing_companions() {
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::new();
        let names = [
            "LICENSE",
            "THIRD_PARTY_NOTICES.txt",
            "kurisu.LICENSE",
            "kurisu.THIRD_PARTY_NOTICES.txt",
            ".kurisu-notices-unrelated",
            ".kurisu-notices.lock",
        ];
        let originals: Vec<_> = names
            .iter()
            .map(|name| {
                let path = directory.0.join(name);
                std::fs::write(&path, format!("user text for {name}")).unwrap();
                (path.clone(), std::fs::metadata(path).unwrap().ino())
            })
            .collect();
        install_in(&directory.0, "new dependency texts").unwrap();
        for ((path, inode), name) in originals.iter().zip(names) {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                format!("user text for {name}")
            );
            assert_eq!(std::fs::metadata(path).unwrap().ino(), *inode);
        }
        assert_eq!(
            std::fs::read_dir(&directory.0).unwrap().count(),
            names.len()
        );
    }

    #[test]
    fn preserves_symlinks_hardlinks_and_directories() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let license = directory.0.join("kurisu.LICENSE");
        let notices = directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt");
        let target = directory.0.join("user-text");
        symlink(&target, &license).unwrap();
        std::fs::create_dir(&notices).unwrap();
        install_in(&directory.0, "dependency texts").unwrap();
        assert_eq!(std::fs::read_link(&license).unwrap(), target);
        assert!(!target.exists());
        assert!(notices.is_dir());
        std::fs::remove_dir(&notices).unwrap();
        std::fs::write(&target, "user text").unwrap();
        std::fs::hard_link(&target, &notices).unwrap();
        install_in(&directory.0, "dependency texts").unwrap();
        assert_eq!(std::fs::read_link(license).unwrap(), target);
        assert_eq!(std::fs::read_to_string(target).unwrap(), "user text");
        assert_eq!(std::fs::read_to_string(notices).unwrap(), "user text");
    }

    #[test]
    fn preserves_user_files_created_before_publication() {
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::new();
        let mut originals = Vec::new();
        let before_publish = |destination: &std::path::Path| {
            assert!(!destination.exists());
            let text = format!("concurrent user text for {}", destination.display());
            let metadata = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        use std::io::Write;

                        let mut file = std::fs::File::create_new(destination).unwrap();
                        file.write_all(text.as_bytes()).unwrap();
                        file.metadata().unwrap()
                    })
                    .join()
                    .unwrap()
            });
            originals.push((destination.to_path_buf(), text, metadata));
        };
        install_in_with(
            &directory.0,
            "bundled dependency texts",
            std::time::SystemTime::now,
            before_publish,
        )
        .unwrap();
        assert_eq!(originals.len(), 2);
        for (path, text, original) in originals {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
            let preserved = std::fs::metadata(path).unwrap();
            assert_eq!(preserved.dev(), original.dev());
            assert_eq!(preserved.ino(), original.ino());
        }
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }

    #[test]
    fn retries_occupied_staging_names_without_touching_them() {
        use std::os::unix::fs::MetadataExt;

        let directory = TestDirectory::new();
        let prefix = format!(".kurisu-notices-{}-0", std::process::id());
        let occupied = ["", "-1"].map(|suffix| {
            let path = directory.0.join(format!("{prefix}{suffix}"));
            std::fs::write(&path, "another writer's text").unwrap();
            let inode = std::fs::metadata(&path).unwrap().ino();
            (path, inode)
        });

        install_in_with(
            &directory.0,
            "complete dependency texts",
            || std::time::UNIX_EPOCH,
            |_| {},
        )
        .expect("occupied staging names should not prevent companion publication");

        for (path, inode) in occupied {
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "another writer's text"
            );
            assert_eq!(std::fs::metadata(path).unwrap().ino(), inode);
        }
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.LICENSE")).unwrap(),
            LICENSE
        );
        assert_eq!(
            std::fs::read_to_string(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt")).unwrap(),
            "complete dependency texts"
        );
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 4);
    }

    #[test]
    fn concurrent_launches_publish_complete_companions() {
        let directory = TestDirectory::new();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    install_in(&directory.0, "complete dependency texts").unwrap();
                    assert_eq!(
                        std::fs::read_to_string(directory.0.join("kurisu.LICENSE")).unwrap(),
                        LICENSE
                    );
                    assert_eq!(
                        std::fs::read_to_string(directory.0.join("kurisu.THIRD_PARTY_NOTICES.txt"))
                            .unwrap(),
                        "complete dependency texts"
                    );
                });
            }
        });
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }
}
