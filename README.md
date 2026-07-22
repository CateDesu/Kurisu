# ク Kurisu

Kurisu is an anime tracker inspired by [Taiga](https://taiga.moe), built in Rust. It runs on Linux and Windows, syncs with [AniList](https://anilist.co), detects what you're watching through the OS media APIs, and caches your list locally.

> ⚠️ **Early development.** Kurisu is a work in progress. Features are incomplete and things will change as it evolves. It's a personal project, not a polished release. Expect bugs.

## Features

- **AniList sync** - OAuth2 sign-in (built-in client, no setup), search, list management, scoring in your format (100 / 10 / 10-decimal / 5-star / 3-smiley). The token is stored locally in plaintext (`kurisu.db`).
- **My List** - grouped by status, −/+ episode stepper that auto-commits, "next episode airs" countdowns, in-place edit dialog, text filter and sorting (title / score / progress / last updated / next airing).
- **Detail pages** - native anime pages with banner, genres, description, your entry controls, related anime, characters and staff, and recommendations. Covers and titles link to them everywhere.
- **Airing calendar** - the week's airing schedule grouped by day, for your shows or everything.
- **Torrent feeds** - watch nyaa-style RSS feeds; releases are matched against your list, new episodes past your progress get flagged, open as magnet or .torrent.
- **Stats** - your AniList profile statistics: time watched, score distribution, genres, formats, release years.
- **Library scan** - point it at your anime folders; files are matched against your list, watched state follows your progress, play the next episode directly. Unmatched files can be linked to a show by hand (per file or whole folder).
- **Seasons + recommendations** - browse any AniList season; the edit dialog shows community recommendations.
- **Playback tracking** - detects playback, matches the title against your list, and prompts or auto-updates progress to the detected episode.
- **Notifications** - your AniList inbox, mirroring anilist.co/notifications.
- **Desktop integration** - custom dark title bar, system tray, borderless window with edge/corner resize.
- **Self-update** - CI builds check the rolling GitHub release on startup (Settings → Updates, on by default) and install in place on Linux and Windows. Builds are verified against a SHA-256 sidecar before anything is run. Locally compiled builds don't auto-check, so developing never nags.

## Windows

Playback detection uses GSMTC (Windows media controls). Any player that registers with it works - mpv.net and VLC do; **bare MPV does not**.

Getting a build:

- **Download:** grab the NSIS installer exe from Releases - `/releases/latest` always points at the newest rolling main build (older rolling builds are pruned automatically). The WebView2 bootstrapper is embedded, so the installer handles everything itself. No manual extra downloads. The bare `kurisu.exe` in the same release works on machines that already have WebView2 (any Windows 11).
- **Build on Windows:** `npx tauri build` (needs Rust, Node, and WebView2 - preinstalled on Windows 11).
- **Cross-build from Linux:** `cargo xwin build --target x86_64-pc-windows-msvc --release` in `src-tauri` (needs `cargo-xwin`, an `xwin splat` SDK in `~/.cache/xwin`, and `clang-cl`/`lld-link`/`llvm-lib`). Output goes to `target/x86_64-pc-windows-msvc/release/kurisu.exe`. Installers can't be bundled this way - use CI for those.

## Stack

Tauri 2 + Rust, SvelteKit 5 + Tailwind v4, SQLite.

## Build (Linux)

```fish
cd Kurisu
npm install
npm run check                # svelte-check (frontend types)
npx tauri build --no-bundle  # production binary at src-tauri/target/release/kurisu
npm run tauri dev            # live dev
```

Dependencies: `webkit2gtk-4.1`, `rustup`, and a C toolchain.

> **Use `npx tauri build`, not a bare `cargo build` in `src-tauri`.** The Tauri CLI runs the frontend build first and embeds the output in the binary. A bare `cargo build` skips that step, so the binary ships with no frontend and the window opens to "Could not connect to localhost: Connection refused" with no list visible.

## License

MIT
