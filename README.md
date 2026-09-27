# ク Kurisu

Linux · Windows · [Downloads](https://github.com/CateDesu/Kurisu/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

Kurisu is an anime tracking program inspired by [Taiga](https://taiga.moe). It syncs with [AniList](https://anilist.co), detects playback, and caches your list locally.

![Kurisu showing the anime list and episode progress](docs/screenshot.png)

## Features

- Manage your AniList statuses, scores, episode progress, and rewatches.
- Detect playback, confirm progress updates or save them automatically, and share what you're watching through optional Discord Rich Presence.
- Scan local media folders, link files to anime, and play the next unwatched episode.
- Browse anime details, recommendations, seasons, an airing calendar, notifications, and profile statistics.
- Match torrent RSS releases to your list, search Nyaa, and open magnet or torrent links in your preferred client.

## Getting started

1. Get the Windows installer or Linux binary from [Releases](https://github.com/CateDesu/Kurisu/releases). On Linux, make the downloaded binary executable with `chmod +x kurisu`. The Windows installer includes the WebView2 bootstrapper.
2. Choose **Connect via AniList** and authorize Kurisu in your browser. Your list syncs when you sign in; **Sync** refreshes it manually.
3. Optionally add media folders in **Library**. In **Settings → Playback tracking**, choose prompts or automatic updates and click **Save tracking**. Automatic progress updates are off by default.

Playback is matched against the anime on your list, so add a show before watching it. You can browse your cached list offline; syncing and saving changes require an AniList connection.

Settings also controls Discord Rich Presence, closing to the system tray, and automatic update checks.

Kurisu checks for new releases at startup and every hour while running, including release builds made from a clone. When an update is available, a dialog shows the release notes and a **Download & install** button on supported Windows and Linux systems. On Windows, follow the installer and launch the installed copy afterward. On Linux, restart Kurisu after installation. Checks are on by default and can be disabled in Settings.

AniList tokens are stored locally in plaintext. See [Privacy](PRIVACY.md) and [Terms](TERMS.md).

## Player setup

Kurisu detects players through MPRIS on Linux and Windows media sessions. For bare MPV, enable IPC in `mpv.conf`:

Linux:

```ini
input-ipc-server=/tmp/mpvsocket
```

Windows:

```ini
input-ipc-server=\\.\pipe\mpvsocket
```

Restart MPV after changing its configuration. If you use a different socket path, enter it in **Settings → Playback tracking**. Players already detected through media sessions need no IPC setup.

## Build from source

Requires Node.js 22, Rust, and a C toolchain. Linux also needs the WebKitGTK 4.1, GTK 3, Ayatana AppIndicator, and librsvg development packages. Windows needs the MSVC build tools and WebView2.

```sh
npm ci
npx tauri build --no-bundle
```

The executable is in `src-tauri/target/release/`. Use `npm run tauri dev` for development, or `npx tauri build --bundles nsis` on Windows to create an installer. Build through the Tauri CLI so the frontend is compiled and embedded.

For local release builds, set `KURISU_BUILD_VERSION` to the newest published release version to avoid update prompts for that same release. Installing an update switches to the published build, so disable automatic update checks in Settings if you want to keep a custom build. Development builds do not check automatically unless a version is explicitly stamped.

Run the checks:

```sh
npm run check
npm test
cargo test --manifest-path src-tauri/Cargo.toml
```

## License

[MIT](LICENSE)
