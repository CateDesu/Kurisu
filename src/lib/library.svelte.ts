// Library scan state (M3). Holds the configured folders + last scan so the Library
// page and the edit modal's "play next" share one scan instead of each re-walking
// the disk. Scans are cheap (a full walk is sub-second), so nothing is persisted.
import { api } from "./api";
import type { LibraryFile, UnreadableFolder } from "./types";

let files = $state<LibraryFile[]>([]);
// Configured roots the last scan could not read (unmounted drive, permissions).
let unreadable = $state<UnreadableFolder[]>([]);
let folders = $state<string[]>([]);
let scanning = $state(false);
let lastScanAt = $state(0);
// Set when a scan is requested mid-scan (e.g. removeFolder's keep-it-honest
// re-scan): run one follow-up with the current folders when the active scan ends.
let pendingScan = false;

async function loadFolders() {
  try {
    folders = await api.getLibraryFolders();
  } catch {
    folders = [];
  }
}

async function scan() {
  if (scanning) {
    pendingScan = true;
    return;
  }
  scanning = true;
  try {
    const result = await api.scanLibrary();
    files = result.files;
    unreadable = result.unreadable;
    lastScanAt = Date.now();
  } finally {
    scanning = false;
    if (pendingScan) {
      pendingScan = false;
      await scan();
    }
  }
}

export const library = {
  get files() {
    return files;
  },
  get unreadable() {
    return unreadable;
  },
  get folders() {
    return folders;
  },
  get scanning() {
    return scanning;
  },
  get lastScanAt() {
    return lastScanAt;
  },
  get hasScan() {
    return lastScanAt > 0;
  },
  /** First scanned file for `mediaId` at `episode` (used by "play next"). */
  fileFor(mediaId: number, episode: number): LibraryFile | undefined {
    return files.find((f) => f.media_id === mediaId && f.episode === episode);
  },
  loadFolders,
  scan,
  /** Drop everything cached for the signed-in account (called from logout). */
  reset() {
    files = [];
    folders = [];
    unreadable = [];
    lastScanAt = 0;
    pendingScan = false;
  },
  async addFolder(path: string) {
    folders = await api.addLibraryFolder(path);
  },
  async removeFolder(path: string) {
    folders = await api.removeLibraryFolder(path);
    // The scan still holds files from the removed folder; re-scan to stay honest.
    if (lastScanAt > 0) await scan();
  },
};
