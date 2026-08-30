// Library scan state for M3. Holds the configured folders and last scan so the
// Library page and the edit modal's "play next" share one scan instead of each
// walking the disk again. Scans are cheap, a full walk takes under a second,
// so nothing is persisted.
import { api } from "./api";
import type { LibraryFile, UnreadableFolder } from "./types";

let files = $state<LibraryFile[]>([]);
// Configured roots the last scan could not read. Unmounted drive, permissions.
let unreadable = $state<UnreadableFolder[]>([]);
let folders = $state<string[]>([]);
let scanning = $state(false);
let lastScanAt = $state(0);
// Set when a scan is requested during another scan. For example, removeFolder's
// rescan to stay honest. Runs one follow up with the current folders when the
// active scan ends.
let pendingScan = false;
// Bumped by reset(). A scan in flight when the account changed must not write
// its results back, they belong to the previous account's recognizer state.
let scanGen = 0;

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
  const gen = scanGen;
  try {
    const result = await api.scanLibrary();
    if (gen !== scanGen) return;
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
  /** First scanned file for `mediaId` at `episode`. Used by "play next". */
  fileFor(mediaId: number, episode: number): LibraryFile | undefined {
    return files.find((f) => f.media_id === mediaId && f.episode === episode);
  },
  loadFolders,
  scan,
  /** Drop everything cached for the current account. Called from logout. */
  reset() {
    // Invalidate any scan still in flight so its results are not written
    // back over the reset.
    scanGen++;
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
    // The scan still holds files from the removed folder. Rescan to stay honest.
    if (lastScanAt > 0) await scan();
  },
};
