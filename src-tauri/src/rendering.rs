use directories::BaseDirs;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::Path;

pub fn configure(identifier: &str) {
    let saved = BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join(identifier).join("kurisu.db"))
        .is_some_and(|path| saved_preference(&path));
    let enabled = enabled_for_launch(saved, std::env::var("KURISU_DMABUF").ok().as_deref());
    // WebKit reads this before creating its first window.
    if !enabled {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn enabled_for_launch(saved: bool, override_value: Option<&str>) -> bool {
    match override_value {
        Some("0") => false,
        Some("1") => true,
        _ => saved,
    }
}

fn saved_preference(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    // Reading the preference must not create or migrate the database.
    let read = || -> rusqlite::Result<bool> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'hardware_acceleration'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.as_deref() == Some("1"))
    };
    match read() {
        Ok(enabled) => enabled,
        Err(error) => {
            log::warn!("cannot read rendering preference, using software rendering: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_reads_the_saved_preference_without_creating_a_database() {
        let dir = std::env::temp_dir().join(format!("kurisu-rendering-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kurisu.db");
        assert!(!saved_preference(&path));
        assert!(!path.exists());

        let db = crate::db::Db::open(&path).unwrap();
        assert!(!saved_preference(&path));
        db.set_setting("hardware_acceleration", "1").unwrap();
        assert!(saved_preference(&path));
        db.set_setting("hardware_acceleration", "0").unwrap();
        assert!(!saved_preference(&path));
        db.set_setting("hardware_acceleration", "invalid").unwrap();
        assert!(!saved_preference(&path));
        drop(db);

        std::fs::write(&path, b"unreadable database").unwrap();
        assert!(!saved_preference(&path));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn launch_override_can_recover_from_a_bad_graphics_driver() {
        assert!(!enabled_for_launch(false, None));
        assert!(enabled_for_launch(true, None));
        assert!(!enabled_for_launch(true, Some("0")));
        assert!(enabled_for_launch(false, Some("1")));
        assert!(!enabled_for_launch(false, Some("invalid")));
    }
}
