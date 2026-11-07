# ク Kurisu

Linux · Windows · [Downloads](https://github.com/CateDesu/Kurisu/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

Kurisu is an anime tracking program for [AniList](https://anilist.co), inspired by [Taiga](https://taiga.moe). It detects playback, syncs your progress, and helps you find and play local episodes.

The [GPUI desktop](native/README.md) is the primary implementation. A final Tauri transition release carries existing installations forward and remains available for rollback.

![Kurisu showing the anime list and episode progress](docs/screenshot.png)

## Getting started

1. Download the native Windows installer or Linux archive from [Releases](https://github.com/CateDesu/Kurisu/releases). On Linux, extract the archive and run `chmod +x kurisu` first. Keep its license files beside the executable. Windows keeps the usual **Kurisu** shortcut. Until native releases are published, the latest stable download remains the Tauri version.
2. Choose **Connect via AniList** and authorize Kurisu in your browser to sync your list.
3. Optionally add media folders in **Library**. Configure **Settings → Playback tracking** and click **Save tracking**. Automatic progress updates are off by default.

Add shows to your list before watching them so playback can be matched. Your cached list is available offline. Progress, status, score, and rewatch changes are saved on this device during connection failures and retried for the same AniList account. Conflicting remote edits appear in the pending sync bar for you to resolve. Deleting entries and editing notes, dates, or custom lists require a connection.

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

## Upgrading an existing installation

Use Kurisu's normal update action. Older installations first receive the final Tauri transition release, which saves your interface preferences. Its next update opens the GPUI program with a separate native profile and a standalone backup. No Python or manual file copying is needed. Quit the old program fully if Kurisu asks, then select **Retry**.

This route also works when you skip releases: legacy downloads on later native release pages still contain the same transition program. Fresh installations should choose the native downloads. If you downloaded a native build directly and it cannot find prepared preferences, run the final transition release once with your original profile, quit it, then retry the native build.

Your original profile stays available. Existing native candidate profiles keep their data. Missing, damaged or busy profiles stop the transfer and show recovery instructions. See [Profile transfer and rollback](native/PROFILE_TRANSFER.md) before returning to the Tauri release, especially if you have unsynced native changes.

<details>
<summary>Build from source</summary>

Requires Rust, a C/C++ toolchain, Python 3.10 or newer for packaging, and GitHub CLI for release version discovery. Linux and Windows dependencies are listed in the [native build guide](native/README.md).

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
cargo fmt --manifest-path native/Cargo.toml -- --check
cargo test --locked --manifest-path core/Cargo.toml
cargo test --locked --manifest-path native/Cargo.toml -- --test-threads=1
cargo clippy --locked --manifest-path core/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --manifest-path native/Cargo.toml --all-targets -- -D warnings
```

See the [native guide](native/README.md) for release transition sequencing and the remaining bridge checks.

</details>

<details>
<summary>Build the retained Tauri transition release</summary>

Keep this source intact until the final bridge release is frozen. It requires patched Node.js 22.23.4, 24.22.1 or 26.12.1 and newer releases in those lines, Rust and a C toolchain. CI uses `.node-version`. Linux needs WebKitGTK 4.1, GTK 3, Ayatana AppIndicator and librsvg development packages. Windows needs MSVC and WebView2.

```sh
npm ci
npm run check
npm test
node --test .github/release-version.test.mjs
cargo test --locked --manifest-path src-tauri/Cargo.toml
npm run build
cargo test --locked --release --manifest-path src-tauri/Cargo.toml --test glib_variant_iter
cargo clippy --locked --manifest-path src-tauri/Cargo.toml -- -D warnings
npx tauri build --no-bundle
```

Output: `src-tauri/target/release/`. Use `npm run tauri dev` for bridge development. Stamp local release builds with the newest published stable `KURISU_BUILD_VERSION`. Rebuild with `npx tauri build --no-bundle` after release Cargo tests, which can replace the shortcut executable with a build expecting a development server.

For Linux graphics problems in the Tauri bridge, use `KURISU_DMABUF=0 ./kurisu`.

</details>

AniList tokens are stored locally in plaintext. See [Privacy](PRIVACY.md) and [Terms](TERMS.md).

Browser sign-in uses Kurisu's hosted authorization-code exchange. Its AniList client secret stays on the server and is not included in desktop builds.

[MIT License](LICENSE)
