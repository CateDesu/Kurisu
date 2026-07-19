// Library scan state (M3). Holds the configured folders + last scan so the Library
// page and the edit modal's "play next" share one scan instead of each re-walking
// the disk. Scans are cheap (a full walk is sub-second), so nothing is persisted.
import { api } from "./api";
import type { LibraryFile } from "./types";

let files = $state<LibraryFile[]>([]);
let folders = $state<string[]>([]);
let scanning = $state(false);
let lastScanAt = $state(0);

async function loadFolders() {
  try {
    folders = await api.getLibraryFolders();
  } catch {
    folders = [];
  }
}

async function scan() {
  if (scanning) return;
  scanning = true;
  try {
    files = await api.scanLibrary();
    lastScanAt = Date.now();
  } finally {
    scanning = false;
  }
}

export const library = {
  get files() {
    return files;
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
  async addFolder(path: string) {
    folders = await api.addLibraryFolder(path);
  },
  async removeFolder(path: string) {
    folders = await api.removeLibraryFolder(path);
    // The scan still holds files from the removed folder; re-scan to stay honest.
    if (lastScanAt > 0) await scan();
  },
};
