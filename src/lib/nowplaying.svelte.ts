// Shared "now playing" state. The MPRIS watcher pushes a `kurisu://now-playing`
// event every tick; rather than have each interested component (the top banner
// AND the Currently Watching tab) attach its own listener, the root layout binds
// ONE listener here and everyone reads the same reactive value.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { NowPlaying } from "./types";

let np = $state<NowPlaying | null>(null);
let bound = false;

/** The current playback snapshot, or null when nothing is playing. Reactive. */
export function nowPlaying(): NowPlaying | null {
  return np;
}

/** Drive the single listener from the root layout's $effect. Idempotent. */
export async function bindNowPlaying(): Promise<void> {
  if (bound) return;
  bound = true;
  const u: UnlistenFn = await listen<NowPlaying>("kurisu://now-playing", (e) => {
    // `active: false` means playback stopped -> clear.
    np = e.payload?.active ? e.payload : null;
  });
  // The listener lives for the whole app session; keep the unlisten handle so a
  // future hot-reload / re-mount in dev doesn't leak a second one (the `bound`
  // guard already prevents duplicates, this is belt-and-braces).
  void u;
}
