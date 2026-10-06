import { listen } from "@tauri-apps/api/event";
import type { NowPlaying } from "./types";

let np = $state<NowPlaying | null>(null);
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
      np = e.payload?.active ? e.payload : null;
    });
  } catch (e) {
    bound = false;
    console.error("now-playing listener failed, will retry on next bind", e);
  }
}
