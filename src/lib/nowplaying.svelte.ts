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
  // Only claim bound once the listener exists. A listen() rejection on a slow
  // cold boot used to leave bound stuck true, killing now playing for the
  // whole session with no retry.
  bound = true;
  try {
    const u: UnlistenFn = await listen<NowPlaying>("kurisu://now-playing", (e) => {
      // active false means playback stopped. Clear.
      np = e.payload?.active ? e.payload : null;
    });
    // Listener lives for the whole session. Kept so a future teardown or dev
    // hot reload path has the handle. The bound guard prevents duplicates.
    void u;
  } catch (e) {
    bound = false;
    console.error("now-playing listener failed, will retry on next bind", e);
  }
}
