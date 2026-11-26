# Kurisu desktop

Kurisu keeps the usual window title, shortcut and Windows installation identity. The native profile remains separate from the legacy profile. Existing native candidate profiles stay usable.

## Install and upgrade

Choose the native Windows installer or Linux archive on [Releases](https://github.com/CateDesu/Kurisu/releases) for a fresh installation. Extract the Linux archive and keep its license files beside `kurisu`. Native release downloads are distinct from the retained legacy bridge downloads.

An existing legacy installation updates through the final transition release. It saves your interface preferences and hands off to the native program, which copies your stopped profile safely and creates a standalone backup. This also works when you skip releases: the legacy downloads on every later native release contain the same frozen transition program. Normal upgrades do not require Python or manual file copying. See [Profile transfer and rollback](PROFILE_TRANSFER.md) for recovery and returning to the legacy release.

## Build and launch

Run from the program root with Rust, a C/C++ toolchain, Python 3.10 or newer, and GitHub CLI (plus Node.js for the release protocol checks):

```sh
python3 native/build.py
./native/target/release/kurisu-native
```

The helper discovers the newest published stable version from `CateDesu/Kurisu`, builds with the committed Cargo lock, checks the executable's version, and writes the updater's existing checksum sidecar. Discovery failure stops the build. It does not install or publish anything. For staging, set `KURISU_RELEASE_REPOSITORY` to the staging repository. In GitHub Actions, the repository variable of the same name also selects where the candidate workflow finds the pinned bridge fixture.

Build artifacts include `LICENSE` and `THIRD_PARTY_NOTICES.txt`. Keep these beside the executable when distributing it. Release executables also embed these texts. On Linux, startup creates missing `kurisu.LICENSE` and `kurisu.THIRD_PARTY_NOTICES.txt` companions, including after an update from the frozen bridge. Existing files are preserved. Use `--license` or `--notices` to read the current bundled texts, including when the installation directory is read only. The Windows installer includes both. Notices cover the target's locked normal and build dependencies, bundled fonts, and local dependency patches. Missing dependency license texts stop packaging. Sourced supplements for crates that omit their license files are in `packaging/licenses/`.

For a repeatable build with a verified release version, pass `--build-version VERSION`. Add `--offline` only after Cargo dependencies are cached. An offline build requires an explicit version. `KURISU_BUILD_VERSION` is also accepted. `--print-version` resolves the stamp without building. Use this helper for release builds so the executable embeds its current dependency notices. Linux packaging supports x86_64 GNU and musl hosts. The helper explicitly selects the host target on Linux and x86_64 MSVC on Windows, overriding Cargo target configuration.

Use a dedicated directory for an isolated profile:

```sh
./native/target/release/kurisu-native --data-dir /absolute/path/to/native-test-profile
./native/target/release/kurisu-native --version
```

An explicit `--data-dir` never discovers or imports the normal legacy profile. Do not point it at a legacy profile: databases without a native marker are refused. A second launch of an open native profile can activate its existing window when both instances support the authenticated activation protocol. If activation is unavailable, the second launch exits and the profile stays open in the first instance. A normal launch uses an existing native profile first, including the earlier Windows candidate location. Two competing default native profiles require an explicit choice through `--data-dir`.

## Platform dependencies

Linux builds require a C/C++ compiler, CMake, pkg-config, OpenSSL, D-Bus, Fontconfig, FreeType, XKB, XCB, Wayland and Vulkan development packages. Runtime needs a working graphics driver, system fonts, D-Bus, and an X11 or Wayland session. The tray uses StatusNotifierItem.

Windows builds require the MSVC toolchain and Windows SDK, including `fxc.exe` to compile release shaders. NSIS 3 packages the installer:

```powershell
python native/build.py --installer
```

The installer is written to `native/target/release/bundle/`. It keeps the normal Kurisu installation and Start menu entry. Quit Kurisu fully before installation if prompted. Uninstall leaves profile data intact.

## Checks

```sh
node --test .github/release-transition.test.mjs .github/prune-native-releases.test.mjs
python3 .github/test_discord_notify.py
python3 -m unittest discover -s native -p test_migrate.py
cargo fmt --manifest-path core/Cargo.toml -- --check
cargo fmt --manifest-path native/Cargo.toml -- --check
cargo test --locked --manifest-path core/Cargo.toml
python3 -m unittest discover -s native -p test_build.py
cargo test --locked --manifest-path native/Cargo.toml --features fixture-runtime -- --test-threads=1
cargo clippy --locked --manifest-path core/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --manifest-path native/Cargo.toml --features fixture-runtime --all-targets -- -D warnings
```

Core tests use controlled MPV and D-Bus instances. Install MPV for playback regression tests and `dbus-daemon` for Linux MPRIS tests. Linux updater tests use `strip` from binutils to keep fixture executables small. Startup tests use disposable profiles and never discover the host's default profile. The frozen bridge source and its frontend checks remain at [v1.0.0.78](https://github.com/CateDesu/Kurisu/tree/v1.0.0.78).

On Linux, check the pinned accessibility adapter with `cargo test --locked --manifest-path native/Cargo.toml -p accesskit_atspi_common --lib`. On Windows, check the patched window zoom helpers with `cargo test --locked --manifest-path native/Cargo.toml -p gpui-windows-gpui-unofficial --lib window::tests::test_zoom_toggle_helpers -- --exact`. Local dependency patches are documented in `vendor/README.md`. A CI build does not replace native platform interaction tests.

## Release transition

Native releases are published manually. The Windows candidate check installs the published bridge pinned in `.github/release-transition.json` and upgrades it with the candidate installer. Verify both platforms and profile handoff before publishing.

Every native release must republish the exact frozen bridge assets under their legacy names beside the native downloads. Old updaters select the highest stable release, so retaining the bridge only on its original release would strand users who skip it. Never rebuild those aliases from later source or prune the bridge release. The Windows native installer uses a different suffix from legacy setup installers so older selectors find only the bridge.

If publishing or retention fails after the build finishes, use **Re-run failed jobs** on the original Native release run. Its publish job reuses the original platform artifacts, verifies the tag and asset set, and can continue an existing draft or verify a completed publication before retention. If the workflow artifacts have expired, publish requires another verified copy of those exact files. Keep the release body marker so retries can confirm ownership and the original checksums.

After publication, the release workflow downloads and verifies the new assets and frozen bridge aliases before removing older stable native releases. Legacy releases, drafts, prereleases and Git tags are retained. Users can skip intermediate native versions and update directly to the newest release.

The native updater selects native assets. The transition release can hand off to the same published native release without requiring a second newer tag. The native executable carries that release's version, preventing a repeated update prompt.

The primary branch contains the native program and shared Core. The legacy frontend and desktop adapter remain in the frozen bridge tag. Keep the frozen release and rollback downloads permanently. See [Profile transfer and rollback](PROFILE_TRANSFER.md) before returning to it: native-only changes, including pending edits, are not copied back automatically.
