This is the published glib 0.18.5 crate with the fix for RUSTSEC-2024-0429
backported from gtk-rs/gtk-rs-core commit b5a4071e439bef2b5eea76c3aa25e5ae84839e34:
https://github.com/gtk-rs/gtk-rs-core/pull/1343

Only src/variant_iter.rs differs from the published source. Its out-pointer is
mutable and passed as &mut p. The upstream license and package metadata remain.
GTK 0.18 requires this GLib series, which upstream no longer releases.

Remove this patch when the GTK dependency accepts a fixed GLib release.
