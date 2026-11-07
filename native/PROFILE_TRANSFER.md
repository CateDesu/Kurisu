# Profile transfer and rollback

Kurisu's native desktop uses a separate profile. Upgrading copies your previous stopped profile, creates a standalone backup and keeps the original available for rollback. It never merges into or replaces an existing native candidate profile.

## Normal upgrade

1. Use the previous Kurisu program's normal update action. It first installs the final Tauri transition release, which saves your interface preferences.
2. Use its update action to move to the native desktop. The usual Kurisu shortcut and Windows installation identity remain.
3. Let the old program quit. The native program waits briefly for it to close, then transfers your profile automatically. If a recovery message appears, follow it and select **Retry**.

No Python or manual copying is needed. Accounts, pending local changes, library folders and bindings, watch history and saved settings move with the database. The transition release also prepares your list, calendar and torrent interface preferences.

Older updaters always choose the highest stable release. For this reason, legacy downloads on later native release pages still contain the frozen transition program. Fresh installations should choose the native downloads instead. If you downloaded native Kurisu directly and preferences are missing, run the final transition release once with the original profile, quit it fully, then retry native Kurisu.

An existing valid native profile takes precedence and keeps its native-only changes. Earlier native candidate profiles remain usable. An explicit `--data-dir` opens only that isolated native profile and never searches for a previous Tauri profile.

## What is kept

The original Tauri profile and its database sidecars remain unchanged. The native profile is published only after the complete copy passes validation. The backup is a standalone SQLite snapshot stored outside both profiles as `kurisu-legacy-backup.db` in your local application data directory. On Linux this is normally `$XDG_DATA_HOME` or `~/.local/share`; on Windows it is `%LOCALAPPDATA%`.

New native profiles use a separate `kurisu-native` application data directory. On Windows, new transfers use local application data; an existing candidate in the earlier roaming location stays there. If both default native locations contain profiles, Kurisu asks you to choose one through `--data-dir` instead of silently picking data.

The backup includes your plaintext AniList token and private local history. Keep it private. Keep the original profile and backup until you are satisfied with the transfer. Neither uninstall nor migration should delete unsynced data.

## If transfer stops

Kurisu shows the reason with **Retry** and **Quit**. It does not start with an empty profile when existing data is missing, damaged or uncertain.

- **Previous Kurisu still running:** quit it fully, including its tray process, then retry. Do not run both programs during transfer.
- **Interface preferences missing:** open the final transition release with the original profile, let it finish starting, then quit it and retry.
- **Database missing or damaged:** keep the profile directory and all sidecars together. Restore a verified complete copy before retrying. Do not delete a WAL or journal to bypass validation.
- **Multiple previous profiles:** preserve both and resolve which profile the transition release should use before retrying. Kurisu will not merge them automatically.
- **Backup already exists:** an interrupted transfer can reuse only an identical complete backup. If it differs, preserve it. Use a separate controlled recovery destination instead of overwriting it.
- **Native marker or database damaged:** preserve the native profile, including unsynced changes. Recover it before retrying; the legacy copy must not replace it automatically.

A complete backup can remain after a later transfer step fails. A failed transfer never publishes a partial native profile. Retry keeps the original source and any complete backup.

## Validation and controlled recovery

Transfer accepts legacy schema versions 1 through 7. It validates SQLite integrity and required schema, copies committed WAL changes into a private standalone snapshot, applies prepared interface preferences to a separate native copy, and publishes the new profile atomically. Supported schema upgrades happen in the native copy. The original and backup retain their source schema version.

A nonempty WAL must have a complete valid header, page size, salts and frame checksums. Valid uncommitted frames are allowed and excluded from the snapshot. Incomplete or damaged frames and mixed salts stop transfer. These checks cannot detect missing files or damage that still forms a valid WAL. Recover uncertain input in a separate copy with SQLite-aware tools, verify its latest changes, then transfer that verified copy. Never discard the original WAL to make a check pass.

Paths must be absolute and local and cannot use symlinks, reparse points or prohibited directory names. A new native destination and backup must be outside the source profile. Windows transfers keep them under `%LOCALAPPDATA%`. No media files are read and no AniList request is made during transfer.

The optional `native/migrate.py` source helper remains available for controlled recovery into an explicitly chosen new destination. It requires Python 3.10 or newer and the transition release's exported UI preferences. Run `python3 native/migrate.py --help` from the program root. Existing destination and backup paths are refused by this helper. Normal packages omit this helper because ordinary updates do not need it. Do not point either program at the other's database directory.

## Return to the Tauri program

Quit native Kurisu fully. Download and install or run the retained final Tauri transition release from its original release page. It uses the preserved original Tauri profile. Avoid its native update action while you need to remain on that release.

Changes made only in the native profile are not copied back. This includes unsynced pending edits, local playback history and interface choices. Sync or resolve what you need before switching, and preserve the native profile for returning later. Do not replace the original database with the native database or delete either profile as part of rollback.
