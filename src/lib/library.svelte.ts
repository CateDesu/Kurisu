import { api } from "./api";
import type { LibraryFile, UnreadableFolder } from "./types";

let files = $state<LibraryFile[]>([]);
const filesByEpisode = $derived.by(() => {
  const byMedia = new Map<number, Map<number, LibraryFile>>();
  for (const file of files) {
    if (file.media_id == null || file.episode == null) continue;
    let episodes = byMedia.get(file.media_id);
    if (!episodes) {
      episodes = new Map();
      byMedia.set(file.media_id, episodes);
    }
    if (!episodes.has(file.episode)) episodes.set(file.episode, file);
  }
  return byMedia;
});
let unreadable = $state<UnreadableFolder[]>([]);
let folders = $state<string[]>([]);
let foldersFailed = $state(false);
let scanning = $state(false);
let lastScanAt = $state(0);
let pendingScan: { promise: Promise<void>; resolve: () => void; reject: (reason: unknown) => void } | null = null;
// Discard scan results from an account that has since logged out.
let scanGen = 0;
let folderRequest = 0;
let folderGeneration = 0;
let folderQueue = Promise.resolve();
const SCAN_AGE = 60_000;

async function loadFolders() {
  const request = ++folderRequest;
  try {
    const result = await api.getLibraryFolders();
    if (request !== folderRequest) return;
    folders = result;
    foldersFailed = false;
  } catch (e) {
    if (request !== folderRequest) return;
    folders = [];
    foldersFailed = true;
    console.error("could not read library folders", e);
  }
}

async function changeFolders(change: () => Promise<string[]>) {
  const generation = folderGeneration;
  const task = folderQueue.then(async () => {
    if (generation !== folderGeneration) return false;
    folderRequest++;
    const result = await change();
    if (generation !== folderGeneration) return false;
    folderRequest++;
    folders = result;
    foldersFailed = false;
    return true;
  });
  folderQueue = task.then(() => {}, () => {});
  return task;
}

async function ensureScan() {
  const gen = scanGen;
  await loadFolders();
  if (gen === scanGen && folders.length > 0 && (lastScanAt === 0 || Date.now() - lastScanAt >= SCAN_AGE)) await scan();
}

async function scan() {
  if (scanning) {
    if (!pendingScan) {
      let resolve!: () => void;
      let reject!: (reason: unknown) => void;
      const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
      pendingScan = { promise, resolve, reject };
    }
    return pendingScan.promise;
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
      const queued = pendingScan;
      pendingScan = null;
      void scan().then(queued.resolve, queued.reject);
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
    return filesByEpisode.get(mediaId)?.get(episode);
  },
  loadFolders,
  ensureScan,
  scan,
  reset() {
    scanGen++;
    folderRequest++;
    folderGeneration++;
    files = [];
    folders = [];
    unreadable = [];
    foldersFailed = false;
    lastScanAt = 0;
    pendingScan?.resolve();
    pendingScan = null;
  },
  async addFolder(path: string) {
    await changeFolders(() => api.addLibraryFolder(path));
  },
  async removeFolder(path: string) {
    if (!(await changeFolders(() => api.removeLibraryFolder(path)))) return;
    const refresh = lastScanAt > 0 || scanning;
    scanGen++;
    files = [];
    unreadable = [];
    lastScanAt = 0;
    if (refresh || folders.length > 0) await scan();
  },
};
