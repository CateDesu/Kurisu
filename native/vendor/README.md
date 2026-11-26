# Native dependency patches

These are the published `gpui-unofficial`, `gpui-linux-gpui-unofficial`, and `gpui-windows-gpui-unofficial` 1.27.0 crate sources. Their `LICENSE-APACHE` files are preserved. The pinned Cargo patches keep the crate versions at 1.27.0.

Eight runtime source files differ from the published crates:

- `gpui-unofficial-1.27.0/src/platform.rs` adds a fallible platform visibility method and an optional exact minimized-state query.
- `gpui-unofficial-1.27.0/src/window.rs` exposes them through `Window::set_visible` and `Window::is_minimized`. Accessibility dispatch refreshes stale frames and checks the active modal scope before calling listeners or forwarding clicks.
- `gpui-unofficial-1.27.0/src/window/a11y.rs` hides nodes outside the active modal from assistive technology.
- `gpui-linux-gpui-unofficial-1.27.0/src/linux/x11/window.rs` maps and unmaps the existing X11 window and reads its window-manager minimized state.
- `gpui-linux-gpui-unofficial-1.27.0/src/linux/x11/client.rs` requests XKB detectable autorepeat and marks repeated presses of a held key.
- `gpui-linux-gpui-unofficial-1.27.0/src/linux/wayland/window.rs` unmaps the existing xdg surface with a null buffer, parks rendering, then remaps through a fresh configure and draw.
- `gpui-windows-gpui-unofficial-1.27.0/src/window.rs` shows and hides the existing HWND, reads `IsIconic`, restores it before activation, and toggles `SW_MAXIMIZE` and `SW_NORMAL` in `zoom`.
- `gpui-unofficial-1.27.0/src/elements/list.rs` preserves row height estimates during first layout and width changes, and reports wheel visibility from the installed scroll position. Visible rows are still measured at the new width. Kurisu and UI library tests cover the range and bounded row work.

The patch retains the existing view, renderer, and inactive window throttling. Native shell close and tray actions use this method after the Workspace save barrier.

Two test-support source files also differ from the published `gpui-unofficial` crate:

- `gpui-unofficial-1.27.0/src/app/test_context.rs` simulates test-window accessibility activation and action requests, reads captured tree updates, queries rendered window control areas, and runs test teardown when `TestAppContext` drops.
- `gpui-unofficial-1.27.0/src/platform/test/window.rs` retains the test window's accessibility callbacks and latest tree update.

Three example images have their metadata removed with identical visible pixels. The JPEG orientation test creates its EXIF orientation in memory. The [dragon example](https://commons.wikimedia.org/wiki/File:Dragon_clip_art.svg) retains its CC BY-SA 3.0 license.
Wayland xdg-shell does not report an exact minimized state. The shell tracks its own Minimize action and remaps that retained surface on tray restore. Minimize initiated outside the program remains compositor dependent.

`accesskit_atspi_common-0.19.1` is the published AccessKit AT-SPI adapter at revision `c88605b96d04431f9c3c792464a0f2f253480e94`. Its version is unchanged. The preserved license and author files come from that revision, as recorded in `packaging/licenses/index.json`.

Only `src/node.rs` differs. It reports disabled controls as unavailable, keeps enabled read-only inputs available, and exposes expandable and expanded states. The original adapter reported disabled buttons as enabled and omitted expansion states. Tests cover the translated states and expansion transitions. Private AT-SPI checks exercise the program controls that use them. Windows accessibility is unaffected.
