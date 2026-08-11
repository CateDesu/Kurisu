// Shared now playing state. MPRIS watcher pushes kurisu://now-playing every
// tick. The root layout binds one listener here so the banner and the
// Currently Watching tab don't each attach their own.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { NowPlaying } from "./types";

let np = $state<NowPlaying | null>(null);
let bound = false;

/** Current playback snapshot, or null when nothing is playing. Reactive. */
export function nowPlaying(): NowPlaying | null {
  return np;
}

/** Bind the single listener from the root layout $effect. Idempotent. */
export async function bindNowPlaying(): Promise<void> {
  if (bound) return;
  bound = true;
  const u: UnlistenFn = await listen<NowPlaying>("kurisu://now-playing", (e) => {
    // active false means playback stopped. Clear.
    np = e.payload?.active ? e.payload : null;
  });
  // Listener lives for the whole session. Keep the unlisten handle so dev
  // hot-reload can't leak a second one. The bound guard already prevents
  // dupes. Belt and suspenders.
  void u;
}
