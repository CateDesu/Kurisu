# ク Kurisu

Kurisu is an anime tracker inspired by [Taiga](https://taiga.moe), built in Rust. It runs on Linux and Windows, syncs with [AniList](https://anilist.co), detects what you're watching through the OS media APIs, and caches your list locally.

Named after Makise Kurisu (Steins;Gate).

> ⚠️ **Early development.** Kurisu is a work in progress — features are incomplete, the UI is rough, and things will change (and occasionally break) as it evolves. It's a personal project, not a polished release. Expect bugs, and back up anything you care about.

## Features

- **AniList sync** — OAuth2 sign-in (built-in client, no setup), search, list management, scoring in your format (100 / 10 / 10-decimal / 5-star / 3-smiley).
- **My List** — grouped by status, −/+ episode stepper that auto-commits, "next episode airs" countdowns, in-place edit dialog.
- **Library scan** — point it at your anime folders; files are matched against your list, watched state follows your progress, play the next episode directly.
- **Seasons + recommendations** — browse any AniList season; the edit dialog shows community recommendations.
- **Playback tracking** — detects playback (platform notes below), matches the title against your list, and prompts or auto-updates progress to the detected episode. In-app only, no desktop notifications.
- **Inbox** — your AniList notifications.
- **Desktop integration** — custom dark title bar, system tray, borderless window with edge/corner resize.

## Windows

Playback detection uses GSMTC (Windows media controls). Any player that registers with it works — mpv.net and VLC do; **bare MPV does not**. Browsers are ignored on both platforms.

Getting a build:

- **Download:** the latest *Windows build* Actions run has MSI/NSIS installers under Artifacts; tagged releases attach them to the Releases page.
- **Build on Windows:** `npx tauri build` (needs Rust, Node, and WebView2 — preinstalled on Windows 11).
- **Cross-build from Linux:** `cargo xwin build --target x86_64-pc-windows-msvc --release` in `src-tauri` (needs `cargo-xwin`, an `xwin splat` SDK in `~/.cache/xwin`, and `clang-cl`/`lld-link`/`llvm-lib`). Output goes to `target/x86_64-pc-windows-msvc/release/kurisu.exe`. Installers can't be bundled this way — use CI for those.

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

## License

MIT
