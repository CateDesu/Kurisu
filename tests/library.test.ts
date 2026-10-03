import { beforeEach, expect, test, vi } from "vitest";
import { deferred, settle } from "./helpers";

const api = vi.hoisted(() => ({
  getLibraryFolders: vi.fn(),
  addLibraryFolder: vi.fn(),
  removeLibraryFolder: vi.fn(),
  scanLibrary: vi.fn(),
}));
vi.mock("$lib/api", () => ({ api }));

beforeEach(() => { vi.resetModules(); vi.resetAllMocks(); });

test("adding a folder invalidates older folder reads", async () => {
  const old = deferred<string[]>();
  api.getLibraryFolders.mockReturnValue(old.promise);
  api.addLibraryFolder.mockResolvedValue(["/anime/new"]);
  const { library } = await import("$lib/library.svelte");
  const read = library.loadFolders();
  await library.addFolder("/anime/new");
  old.resolve([]);
  await read;
  expect(library.folders).toEqual(["/anime/new"]);
});

test("reset rejects old folder reads and scans", async () => {
  const folders = deferred<string[]>();
  const scan = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  api.getLibraryFolders.mockReturnValue(folders.promise);
  api.scanLibrary.mockReturnValue(scan.promise);
  const { library } = await import("$lib/library.svelte");
  const read = library.loadFolders();
  const scanning = library.scan();
  library.reset();
  folders.resolve(["/anime/old"]);
  scan.resolve({ files: [{ path: "/anime/old/file.mkv", media_id: 1 }], unreadable: [] });
  await Promise.all([read, scanning]);
  expect(library.folders).toEqual([]);
  expect(library.files).toEqual([]);
  expect(library.hasScan).toBe(false);
});

test("a queued account scan waits for its own result after an old scan fails", async () => {
  const old = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  const next = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  api.scanLibrary.mockReturnValueOnce(old.promise).mockReturnValueOnce(next.promise);
  api.getLibraryFolders.mockResolvedValue(["/anime/new"]);
  const { library } = await import("$lib/library.svelte");
  const previous = library.scan().catch(() => {});
  library.reset();
  let finished = false;
  const pending = library.ensureScan().then(() => { finished = true; });
  await settle();
  expect(finished).toBe(false);
  old.reject(new Error("old drive unavailable"));
  await settle();
  expect(api.scanLibrary).toHaveBeenCalledTimes(2);
  expect(library.scanning).toBe(true);
  next.resolve({ files: [{ path: "/anime/new/file.mkv", media_id: 2, episode: 1 }], unreadable: [] });
  await Promise.all([pending, previous]);
  expect(library.scanning).toBe(false);
  expect(library.files[0]?.media_id).toBe(2);
});

test("a queued scan failure reaches the caller who requested it", async () => {
  const first = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  api.scanLibrary.mockReturnValueOnce(first.promise).mockRejectedValueOnce(new Error("new scan failed"));
  const { library } = await import("$lib/library.svelte");
  const scanning = library.scan().catch(() => {});
  const queued = library.scan();
  const result = expect(queued).rejects.toThrow("new scan failed");
  first.resolve({ files: [], unreadable: [] });
  await Promise.all([scanning, result]);
  expect(library.scanning).toBe(false);
});

test("removing a folder during the first scan excludes its files", async () => {
  const first = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  api.scanLibrary.mockReturnValueOnce(first.promise).mockResolvedValue({ files: [], unreadable: [] });
  api.removeLibraryFolder.mockResolvedValue([]);
  const { library } = await import("$lib/library.svelte");
  const scanning = library.scan();
  const removing = library.removeFolder("/anime/old");
  await settle();
  first.resolve({ files: [{ path: "/anime/old/episode.mkv", media_id: 1, episode: 1 }], unreadable: [] });
  await Promise.all([scanning, removing]);
  expect(library.files).toEqual([]);
});

test("a later failed folder mutation cannot hide an earlier successful one", async () => {
  const add = deferred<string[]>();
  const remove = deferred<string[]>();
  api.addLibraryFolder.mockReturnValue(add.promise);
  api.removeLibraryFolder.mockReturnValue(remove.promise);
  api.getLibraryFolders.mockResolvedValue(["/anime/new"]);
  const { library } = await import("$lib/library.svelte");
  const adding = library.addFolder("/anime/new");
  const removing = library.removeFolder("/anime/missing").catch(() => {});
  add.resolve(["/anime/new"]);
  await adding;
  remove.reject(new Error("cannot remove"));
  await removing;
  expect(library.folders).toEqual(["/anime/new"]);
});

test("removing an unscanned folder still scans the remaining roots", async () => {
  api.removeLibraryFolder.mockResolvedValue(["/anime/remaining"]);
  api.scanLibrary.mockResolvedValue({ files: [{ path: "/anime/remaining/episode.mkv", media_id: 1, episode: 1 }], unreadable: [] });
  const { library } = await import("$lib/library.svelte");
  await library.removeFolder("/anime/old");
  expect(api.scanLibrary).toHaveBeenCalledOnce();
  expect(library.files[0]?.path).toBe("/anime/remaining/episode.mkv");
});

test("removing a scanned folder clears episode lookups before the next scan finishes", async () => {
  const next = deferred<{ files: unknown[]; unreadable: unknown[] }>();
  api.scanLibrary.mockResolvedValueOnce({ files: [
    { path: "/anime/old/episode.mkv", media_id: 1, episode: 1 },
  ], unreadable: [] }).mockReturnValueOnce(next.promise);
  api.removeLibraryFolder.mockResolvedValue([]);
  const { library } = await import("$lib/library.svelte");
  await library.scan();
  expect(library.fileFor(1, 1)).toBeDefined();

  const removing = library.removeFolder("/anime/old");
  await settle();
  expect(library.fileFor(1, 1)).toBeUndefined();
  next.resolve({ files: [], unreadable: [] });
  await removing;
  expect(library.fileFor(1, 1)).toBeUndefined();
});
