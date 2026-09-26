import { api } from "./api";
import type { LibraryFile, UnreadableFolder } from "./types";

let files = $state<LibraryFile[]>([]);
let unreadable = $state<UnreadableFolder[]>([]);
let folders = $state<string[]>([]);
let foldersFailed = $state(false);
let scanning = $state(false);
let lastScanAt = $state(0);
let pendingScan = false;
// Discard scan results from an account that has since logged out.
let scanGen = 0;

async function loadFolders() {
  try {
    folders = await api.getLibraryFolders();
    foldersFailed = false;
  } catch (e) {
    folders = [];
    foldersFailed = true;
    console.error("could not read library folders", e);
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
  get foldersFailed() {
    return foldersFailed;
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
  fileFor(mediaId: number, episode: number): LibraryFile | undefined {
    return files.find((f) => f.media_id === mediaId && f.episode === episode);
  },
  loadFolders,
  scan,
  reset() {
    scanGen++;
    files = [];
    folders = [];
    unreadable = [];
    foldersFailed = false;
    lastScanAt = 0;
    pendingScan = false;
  },
  async addFolder(path: string) {
    folders = await api.addLibraryFolder(path);
  },
  async removeFolder(path: string) {
    folders = await api.removeLibraryFolder(path);
    if (lastScanAt > 0) await scan();
  },
};
