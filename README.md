# ク Kurisu

Linux · Windows · [Downloads](https://github.com/CateDesu/Kurisu/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

Kurisu is an anime tracking program for [AniList](https://anilist.co), inspired by [Taiga](https://taiga.moe). It detects playback, syncs your progress, and helps you find and play local episodes.

Browse seasonal anime and airing schedules, search torrent feeds, view watch statistics, and share playback through Discord Rich Presence.

![Kurisu showing the anime list and episode progress](docs/screenshot.png)

## Getting started

1. Download the native Windows installer or Linux archive from [Releases](https://github.com/CateDesu/Kurisu/releases). On Windows, install and open **Kurisu**. On Linux, extract the archive, run `chmod +x kurisu`, then launch `./kurisu`. Keep its license files beside the executable.
2. Choose **Connect via AniList** and authorize Kurisu in your browser to sync your list.
3. Optionally add media folders in **Library**. Configure **Settings → Playback tracking** and click **Save tracking**. Automatic progress updates are off by default.

Add shows to your list before watching them so playback can be matched. Your cached list is available offline. Progress, status, score, and rewatch changes are saved on this device during connection failures and retried for the same AniList account. Conflicting remote edits appear in the pending sync bar for you to resolve. Deleting entries and editing notes, dates, or custom lists require a connection.

For older installations, see [Profile transfer and rollback](native/PROFILE_TRANSFER.md) for upgrade and recovery instructions.

## Player setup

Kurisu detects players through Linux MPRIS and Windows media sessions. For bare MPV, add the appropriate line to `mpv.conf`:

Linux:

```ini
input-ipc-server=/tmp/mpvsocket
```

Windows:

```ini
input-ipc-server=\\.\pipe\mpvsocket
```

Restart MPV afterward. Set custom socket paths in **Settings → Playback tracking**. Players detected through media sessions need no IPC setup.

In Library, use **Episode numbering** to adjust files that continue numbering from an earlier season. An offset of `-12` maps file 13 to episode 1. Known single-episode movies can be recognized without a numbered filename. **Watch history** shows local playback that reached the watch threshold and was accepted for tracking, or was already marked watched on your list.

<details>
<summary>Build from source</summary>

Kurisu uses GPUI and a shared Rust core. Building requires Rust, a C/C++ toolchain, Python 3.10 or newer, and GitHub CLI for release version discovery. Linux and Windows dependencies are listed in the [build guide](native/README.md#platform-dependencies).

```sh
python3 native/build.py
./native/target/release/kurisu-native
```

Output: `native/target/release/`. Add `--installer` on Windows to package the installer. The helper does not install or publish anything. For an isolated profile, pass `--data-dir /absolute/path/to/test-profile` to the executable. An explicit profile never imports the working profile automatically.

The build helper stamps the newest published stable version to prevent repeated update prompts. For a verified fixed version, pass `--build-version VERSION`. Offline builds require an explicit version. Disable automatic checks to keep a custom build.

Run the checks from the program root. Core playback tests need MPV on `PATH`; Linux MPRIS tests need `dbus-daemon`. Tests use controlled profiles and playback sessions.

```sh
python3 -m unittest discover -s native -p test_build.py
python3 -m unittest discover -s native -p test_migrate.py
cargo fmt --manifest-path core/Cargo.toml -- --check
cargo fmt --manifest-path native/Cargo.toml -- --check
cargo test --locked --manifest-path core/Cargo.toml
cargo test --locked --manifest-path native/Cargo.toml --features fixture-runtime -- --test-threads=1
cargo clippy --locked --manifest-path core/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --manifest-path native/Cargo.toml --features fixture-runtime --all-targets -- -D warnings
```

See the [build guide](native/README.md) for packaging and release maintenance.

</details>

AniList tokens are stored locally in plaintext. See [Privacy](PRIVACY.md) and [Terms](TERMS.md).

Browser sign-in uses Kurisu's hosted authorization-code exchange. Its AniList client secret stays on the server and is not included in desktop builds.

[MIT License](LICENSE)
