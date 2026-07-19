# ク Kurisu

Kurisu is an anime-tracking app inspired by [Taiga](https://taiga.moe), built in Rust for **Linux and Windows**. It syncs with [AniList](https://anilist.co), detects playback through the OS media sessions (MPRIS2 on Linux, GSMTC on Windows), and keeps a local cache for a fast, offline-friendly UI.

Named after Makise Kurisu (Steins;Gate).

> ⚠️ **Early development.** Kurisu is a work in progress — features are incomplete, the UI is rough, and things will change (and occasionally break) as it evolves. It's a personal project built for Linux, not a polished release yet. Expect bugs, and back up anything you care about.

## Features

- **AniList sync** — OAuth2 sign-in (built-in client, one click), search, list management, scoring in your preferred format (100 / 10 / 10-decimal / 5-star / 3-smiley).
- **My List** — grouped by status, with a −/+ episode stepper that auto-commits, "next episode airs" countdowns, and an in-place edit dialog.
- **Library scan** — point it at your anime folders; files are recognized against your list, watched state follows your progress, and you can play the next episode in one click.
- **Seasons + recommendations** — browse any AniList season, and get community recommendations from the edit dialog.
- **Playback tracking** — detects MPV / VLC / Celluloid on Linux (any MPRIS2 player) and GSMTC players on Windows (mpv.net, VLC — bare MPV doesn't register with Windows media controls), matches the title against your list, and (per mode) prompts or auto-updates progress to the detected episode. In-app only — no desktop/tray notifications.
- **Inbox** — native view of your AniList notifications.
- **Linux-native** — custom dark title bar, system tray, CSD window with edge/corner resize.

## Stack

Tauri 2 + Rust on the backend, SvelteKit 5 + Tailwind v4 on the frontend, SQLite for the local cache.

## Build

```fish
cd Kurisu
npm install
npm run check                # svelte-check (frontend types)
npx tauri build --no-bundle  # production binary at src-tauri/target/release/kurisu
npm run tauri dev            # live dev
```

Linux build dependencies: `webkit2gtk-4.1`, `rustup`, and a C toolchain.

Windows builds: use `cargo xwin build --target x86_64-pc-windows-msvc --release` (toolchain: `cargo-xwin` + an SDK/CRT splat in `~/.cache/xwin`, plus `clang-cl`/`lld-link`/`llvm-lib`) or the **`Windows build` GitHub Actions workflow** (manual trigger; MSI + NSIS installers land in the run's artifacts). On Windows itself, plain `npx tauri build` works too.

## License

MIT
