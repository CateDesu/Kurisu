import { listen } from "@tauri-apps/api/event";
import type { NowPlaying } from "./types";

let np = $state.raw<NowPlaying | null>(null);
let bound = false;

export function nowPlaying(): NowPlaying | null {
  return np;
}

export async function bindNowPlaying(): Promise<void> {
  if (bound) return;
  bound = true;
  try {
    // Keep the listener for the whole session.
    await listen<NowPlaying>("kurisu://now-playing", (e) => {
      const next = e.payload?.active ? e.payload : null;
      if (np === next || np && next &&
          np.active === next.active && np.player === next.player && np.title === next.title &&
          np.matched === next.matched && np.media_id === next.media_id && np.episode === next.episode &&
          np.length_us === next.length_us && np.position_us === next.position_us) return;
      np = next;
    });
  } catch (e) {
    bound = false;
    console.error("now-playing listener failed, will retry on next bind", e);
  }
}
