# Kurisu desktop

Kurisu's primary desktop uses GPUI and the shared Rust Core. It keeps the usual **Kurisu** window title, shortcut and Windows installation identity. The native profile remains separate from the previous Tauri profile. Existing native candidate profiles stay usable.

## Install and upgrade

Choose the native Windows installer or Linux archive on [Releases](https://github.com/CateDesu/Kurisu/releases) for a fresh installation. Extract the Linux archive and keep its license files beside `kurisu`. Native release downloads are distinct from the retained legacy bridge downloads. Until native assets are published, the latest stable release remains the Tauri program.

An existing Tauri installation updates through the final transition release. It saves your interface preferences and hands off to the native program, which copies your stopped profile safely and creates a standalone backup. This also works when you skip releases: the legacy downloads on every later native release contain the same frozen transition program. Normal upgrades do not require Python or manual file copying. See [Profile transfer and rollback](PROFILE_TRANSFER.md) for recovery and returning to the Tauri release.

## Build and launch

Run from the program root with Rust, a C/C++ toolchain, Python 3.10 or newer, and GitHub CLI:

```sh
python3 native/build.py
./native/target/release/kurisu-native
```

The helper discovers the newest published stable version, builds with the committed Cargo lock, checks the executable's version, and writes the updater's existing checksum sidecar. Discovery failure stops the build. It does not install or publish anything.

Build artifacts include `LICENSE` and `THIRD_PARTY_NOTICES.txt`. Keep these beside the executable when distributing it. The Windows installer includes both. Notices cover the target's locked normal and build dependencies, bundled fonts, and the local GPUI changes. Missing dependency license texts stop packaging. Sourced supplements for crates that omit their license files are in `packaging/licenses/`.

For a repeatable build with a verified release version, pass `--build-version VERSION`. Add `--offline` only after Cargo dependencies are cached. An offline build requires an explicit version. `KURISU_BUILD_VERSION` is also accepted. `--print-version` resolves the stamp without building.

Use a dedicated directory for an isolated profile:

```sh
./native/target/release/kurisu-native --data-dir /absolute/path/to/native-test-profile
./native/target/release/kurisu-native --version
```

An explicit `--data-dir` never discovers or imports the normal legacy profile. Do not point it at a Tauri profile: databases without a native marker are refused. Launching a second instance of an open native profile activates its existing window. A normal launch uses an existing native profile first, including the earlier Windows candidate location. Two competing default native profiles require an explicit choice through `--data-dir`.

## Platform dependencies

Linux builds require a C/C++ compiler, CMake, pkg-config, OpenSSL, D-Bus, Fontconfig, FreeType, XKB, XCB, Wayland and Vulkan development packages. Runtime needs a working graphics driver, system fonts, D-Bus, and an X11 or Wayland session. The tray uses StatusNotifierItem.

Windows builds require the MSVC toolchain and Windows SDK, including `fxc.exe` for GPUI release shaders. NSIS 3 packages the installer:

```powershell
python native/build.py --installer
```

The installer is written to `native/target/release/bundle/`. It keeps the normal Kurisu installation and Start menu entry. Quit Kurisu fully before installation if prompted. Uninstall leaves profile data intact.

## Checks

```sh
python3 -m unittest discover -s native -p test_build.py
python3 -m unittest discover -s native -p test_migrate.py
cargo fmt --manifest-path core/Cargo.toml -- --check
cargo fmt --manifest-path native/Cargo.toml -- --check
cargo test --locked --manifest-path core/Cargo.toml
cargo test --locked --manifest-path native/Cargo.toml -- --test-threads=1
cargo clippy --locked --manifest-path core/Cargo.toml --all-targets -- -D warnings
cargo clippy --locked --manifest-path native/Cargo.toml --all-targets -- -D warnings
```

Core tests use controlled MPV and D-Bus instances. Install MPV for playback regression tests and `dbus-daemon` for Linux MPRIS tests. Startup tests use disposable profiles and never discover the host's default profile. Keep the bridge and frontend checks in the root README until the final bridge is frozen.

On Linux, check the pinned accessibility adapter with `cargo test --locked --manifest-path native/Cargo.toml -p accesskit_atspi_common --lib`. Local dependency patches are documented in `vendor/README.md`. A CI build does not replace native platform interaction tests.

## Release transition

Release publication is manual. Pin the final bridge tag in `.github/release-transition.json`, then freeze and publish that Tauri transition release at a version above the previous stable release. Keep its frontend source, commit, tag and original legacy assets. Verify both platforms and profile handoff before publishing a native release with a higher version.

Every native release must republish the exact frozen bridge assets under their legacy names beside the native downloads. Old updaters select the highest stable release, so retaining the bridge only on its original release would strand users who skip it. Never rebuild those aliases from later source or prune the bridge release. The Windows native installer uses a different suffix from legacy setup installers so older selectors find only the bridge.

The native updater selects native assets. The transition release can hand off to the same published native release without requiring a second newer tag. The native executable carries that release's version, preventing a repeated update prompt.

Only after the final bridge is frozen may the primary branch remove the old frontend and Tauri adapter. Keep the frozen release and rollback downloads permanently. See [Profile transfer and rollback](PROFILE_TRANSFER.md) before returning to it: native-only changes, including pending edits, are not copied back automatically.
