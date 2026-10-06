# ク Kurisu

Linux · Windows · [Downloads](https://github.com/CateDesu/Kurisu/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

Kurisu is an anime tracking program for [AniList](https://anilist.co), inspired by [Taiga](https://taiga.moe). It detects playback, syncs your progress, and helps you find and play local episodes.

![Kurisu showing the anime list and episode progress](docs/screenshot.png)

## Getting started

1. Download the Windows installer or Linux binary from [Releases](https://github.com/CateDesu/Kurisu/releases). On Linux, run `chmod +x kurisu` first.
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

On Linux, hardware acceleration is off by default. Enable **Settings → Performance → Use hardware acceleration** to try smoother scrolling, then quit and reopen Kurisu. If your graphics driver causes a blank window or crashes, launch with `KURISU_DMABUF=0 ./kurisu` and turn the setting off.

<details>
<summary>Build from source</summary>

Requires Node.js 22, Rust, and a C toolchain. Linux also needs the WebKitGTK 4.1, GTK 3, Ayatana AppIndicator, and librsvg development packages. Windows needs the MSVC build tools and WebView2.

```sh
npm ci
npx tauri build --no-bundle
```

Output: `src-tauri/target/release/`. Use `npm run tauri dev` for development.

Set `KURISU_BUILD_VERSION` to the newest published version for local release builds to avoid repeat update prompts. Disable automatic checks to keep a custom build.

Run the CI checks from the program root. Rust tests on Unix require MPV on `PATH` for the live IPC test.

```sh
npm run check
npm test
node --test .github/release-version.test.mjs
cargo test --locked --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml -- -D warnings
```

</details>

AniList tokens are stored locally in plaintext. See [Privacy](PRIVACY.md) and [Terms](TERMS.md).

[MIT License](LICENSE)
