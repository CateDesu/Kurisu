# ク Kurisu

Linux · Windows · [Downloads](https://github.com/CateDesu/Kurisu/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

Kurisu is an anime tracking program inspired by [Taiga](https://taiga.moe). It syncs with [AniList](https://anilist.co), detects playback, and caches your list locally.

**Early development.** Expect bugs and incomplete features.

## Features

- AniList sign-in, list editing, scoring, search, and notifications.
- Anime details, recommendations, seasonal browsing, airing calendar, and profile statistics.
- Local library scanning, manual file links, and next episode playback.
- Playback detection through Linux MPRIS2, Windows GSMTC, or MPV IPC, with progress prompts or automatic updates and optional Discord Rich Presence.
- Torrent RSS feeds matched to your list, nyaa search, and magnet or torrent links opened in your client.
- Custom window controls, system tray, and self-updates on Linux and Windows. Updates require a matching SHA-256 sidecar and can be disabled in Settings.

AniList tokens are stored locally in plaintext. See [Privacy](PRIVACY.md) and [Terms](TERMS.md).

## MPV

Enable IPC in `mpv.conf` for bare MPV detection:

```ini
input-ipc-server=/tmp/mpvsocket
```

On Windows, use `input-ipc-server=\\.\pipe\mpvsocket`. Set a custom path in Settings → Playback tracking, or leave it blank to try the defaults. Players exposing MPRIS2 or GSMTC need no IPC setup.

## Build

Install Rust, Node, and a C toolchain. Linux also needs WebKitGTK 4.1, GTK 3, Ayatana AppIndicator, and librsvg development packages. Windows needs WebView2.

```sh
npm ci
npm run check
npx tauri build --no-bundle
```

The binary is in `src-tauri/target/release/`. Use `npm run tauri dev` for development or `npx tauri build` on Windows to create an installer. The downloadable Windows installer includes the WebView2 bootstrapper. The bare executable needs WebView2 already installed.

Build through the Tauri CLI so it compiles and embeds the frontend. Set `KURISU_BUILD_VERSION` to the release version for local release builds. Startup update checks skip debug builds and unstamped Windows builds.

To cross-build a Windows executable from Linux, build the frontend with `npm run build`, then run `cargo xwin build --target x86_64-pc-windows-msvc --release` in `src-tauri`. This requires `cargo-xwin`, an `xwin splat` SDK in `~/.cache/xwin`, and `clang-cl`, `lld-link`, and `llvm-lib`. The binary goes to `target/x86_64-pc-windows-msvc/release/kurisu.exe`. Use the Windows workflow for installers.

## License

[MIT](LICENSE)
