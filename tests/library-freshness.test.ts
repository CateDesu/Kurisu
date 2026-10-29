import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { LibraryFile, LibraryScan } from "#lib/types.ts";
import { deferred, settle } from "./helpers";

const api = vi.hoisted(() => ({ getLibraryFolders: vi.fn(), scanLibrary: vi.fn() }));
vi.mock("#lib/api.ts", () => ({ api }));

beforeEach(() => { vi.resetModules(); vi.resetAllMocks(); });
afterEach(() => { vi.restoreAllMocks(); });

const files: LibraryFile[] = [
  { path: "/anime/first.mkv", media_id: 1, matched: "First", episode: 1 },
  { path: "/anime/second.mkv", media_id: 2, matched: "Second", episode: 2 },
];
const copyFiles = () => files.map((file) => ({ ...file }));

test("identical scans retain their snapshot and episode objects while freshness advances", async () => {
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  api.scanLibrary.mockImplementation(async () => ({ files: copyFiles(), unreadable: [] }));
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  const snapshot = library.files;
  const episode = library.fileFor(1, 1);
  for (const time of [1_060_000, 1_120_000, 1_180_000]) {
    now.mockReturnValue(time);
    await library.scan();
    expect(library.files).toBe(snapshot);
    expect(library.fileFor(1, 1)).toBe(episode);
    expect(library.lastScanAt).toBe(time);
  }
  expect(api.scanLibrary).toHaveBeenCalledTimes(4);
});

test.each([
  ["path", { path: "/anime/renamed.mkv" }],
  ["media_id", { media_id: 3 }],
  ["matched", { matched: "Renamed" }],
  ["episode", { episode: 3 }],
  ["bound", { bound: true }],
  ["unmatched media", { media_id: null }],
  ["missing title", { matched: null }],
  ["unnumbered episode", { episode: null }],
] satisfies [string, Partial<LibraryFile>][])("a changed %s replaces its file and reuses unaffected files", async (_field, change) => {
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  const previous = library.files;
  const changed = { ...files[0], ...change };
  api.scanLibrary.mockResolvedValueOnce({ files: [changed, { ...files[1] }], unreadable: [] });
  await library.scan();
  expect(library.files).not.toBe(previous);
  expect(library.files[0]).toBe(changed);
  expect(library.files[1]).toBe(previous[1]);
  expect(library.files).toEqual([changed, files[1]]);
  if (changed.media_id != null && changed.episode != null) {
    expect(library.fileFor(changed.media_id, changed.episode)).toBe(changed);
  }
  if (changed.media_id !== 1 || changed.episode !== 1) {
    expect(library.fileFor(1, 1)).toBeUndefined();
  }
});

test("omitted and false bound values are equivalent while removing a manual link updates", async () => {
  const { library } = await import("#lib/library.svelte.ts");
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable: [] });
  await library.scan();
  const unbound = library.files;
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles().map((file) => ({ ...file, bound: false })), unreadable: [] });
  await library.scan();
  expect(library.files).toBe(unbound);
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles().map((file) => ({ ...file, bound: true })), unreadable: [] });
  await library.scan();
  const bound = library.files;
  expect(bound).not.toBe(unbound);
  expect(library.fileFor(1, 1)?.bound).toBe(true);
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles().map((file) => ({ ...file, bound: false })), unreadable: [] });
  await library.scan();
  const explicitFalse = library.files;
  expect(explicitFalse).not.toBe(bound);
  expect(library.fileFor(1, 1)?.bound).toBe(false);
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable: [] });
  await library.scan();
  expect(library.files).toBe(explicitFalse);
});

test("reordering, adding and removing files update snapshots and first duplicate selection", async () => {
  const duplicate = { ...files[0], path: "/anime/duplicate.mkv" };
  api.scanLibrary.mockResolvedValueOnce({ files: [{ ...files[0] }, duplicate, { ...files[1] }], unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  const previous = library.files;
  expect(library.fileFor(1, 1)).toBe(previous[0]);
  api.scanLibrary.mockResolvedValueOnce({ files: [duplicate, files[0], files[1]].map((file) => ({ ...file })), unreadable: [] });
  await library.scan();
  const reordered = library.files;
  expect(reordered).not.toBe(previous);
  expect(reordered[0]).toBe(previous[1]);
  expect(reordered[1]).toBe(previous[0]);
  expect(reordered[2]).toBe(previous[2]);
  expect(library.fileFor(1, 1)).toBe(previous[1]);

  const added = { ...files[0], path: "/anime/new.mkv", media_id: 3 };
  api.scanLibrary.mockResolvedValueOnce({ files: [...reordered.map((file) => ({ ...file })), added], unreadable: [] });
  await library.scan();
  const extended = library.files;
  expect(extended).not.toBe(reordered);
  reordered.forEach((file, index) => expect(extended[index]).toBe(file));
  expect(library.fileFor(3, 1)).toBe(added);

  api.scanLibrary.mockResolvedValueOnce({ files: [{ ...added }, { ...files[0] }], unreadable: [] });
  await library.scan();
  expect(library.files).not.toBe(extended);
  expect(library.files).toEqual([added, files[0]]);
  expect(library.files[0]).toBe(added);
  expect(library.files[1]).toBe(previous[0]);
  expect(library.fileFor(1, 1)).toBe(previous[0]);
  expect(library.fileFor(2, 2)).toBeUndefined();

  api.scanLibrary.mockResolvedValue({ files: [], unreadable: [] });
  await library.scan();
  const empty = library.files;
  expect(empty).toEqual([]);
  expect(library.fileFor(1, 1)).toBeUndefined();
  expect(library.fileFor(3, 1)).toBeUndefined();
  api.scanLibrary.mockResolvedValue({ files: [], unreadable: [] });
  await library.scan();
  expect(library.files).toBe(empty);
});

test("unreadable results update independently and a failed scan leaves freshness unchanged", async () => {
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  const snapshot = library.files;
  for (const unreadable of [
    [{ path: "/offline", error: "Drive unavailable" }],
    [{ path: "/offline", error: "Permission denied" }],
    [],
  ]) {
    now.mockReturnValue(library.lastScanAt + 60_000);
    api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable });
    await library.scan();
    expect(library.files).toBe(snapshot);
    expect(library.unreadable).toEqual(unreadable);
    expect(library.lastScanAt).toBe(Date.now());
  }
  const successfulAt = library.lastScanAt;
  now.mockReturnValue(successfulAt + 60_000);
  api.scanLibrary.mockRejectedValueOnce(new Error("scan unavailable"));
  await expect(library.scan()).rejects.toThrow("scan unavailable");
  expect(library.files).toBe(snapshot);
  expect(library.unreadable).toEqual([]);
  expect(library.lastScanAt).toBe(successfulAt);
  expect(library.scanning).toBe(false);
});

test("account reset rejects an identical old scan before accepting the queued account scan", async () => {
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  const old = deferred<LibraryScan>();
  const next = deferred<LibraryScan>();
  api.scanLibrary.mockResolvedValueOnce({ files: copyFiles(), unreadable: [] })
    .mockReturnValueOnce(old.promise).mockReturnValueOnce(next.promise);
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  const previous = library.files;
  const scanning = library.scan();
  const cancelled = library.scan();
  library.reset();
  await cancelled;
  const queued = library.scan();
  old.resolve({ files: copyFiles(), unreadable: [{ path: "/old", error: "Old account" }] });
  await scanning;
  await settle();
  expect(api.scanLibrary).toHaveBeenCalledTimes(3);
  expect(library.scanning).toBe(true);
  expect(library.files).toEqual([]);
  expect(library.fileFor(1, 1)).toBeUndefined();
  expect(library.unreadable).toEqual([]);
  expect(library.lastScanAt).toBe(0);

  const current = copyFiles();
  now.mockReturnValue(1_060_000);
  next.resolve({ files: current, unreadable: [] });
  await queued;
  expect(library.files).not.toBe(previous);
  expect(library.fileFor(1, 1)).toBe(current[0]);
  expect(library.lastScanAt).toBe(1_060_000);
  expect(library.scanning).toBe(false);
});

test("library reuses a recent scan and refreshes it once stale", async () => {
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  api.getLibraryFolders.mockResolvedValue(["/anime"]);
  api.scanLibrary.mockResolvedValueOnce({ files: [], unreadable: [] }).mockResolvedValue({ files: [{ path: "/anime/new.mkv", media_id: 1, episode: 1 }], unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.ensureScan();
  now.mockReturnValue(1_059_999);
  await library.ensureScan();
  expect(api.scanLibrary).toHaveBeenCalledOnce();
  now.mockReturnValue(1_060_000);
  await library.ensureScan();
  expect(api.scanLibrary).toHaveBeenCalledTimes(2);
  expect(library.fileFor(1, 1)?.path).toBe("/anime/new.mkv");
});

test("unnumbered movie extras are never selected as the next episode", async () => {
  api.scanLibrary.mockResolvedValue({ files: [
    { path: "/anime/preview.mkv", media_id: 1, episode: null },
    { path: "/anime/movie.mkv", media_id: 2, episode: 1 },
  ], unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  expect(library.fileFor(1, 1)).toBeUndefined();
  expect(library.fileFor(2, 1)?.path).toBe("/anime/movie.mkv");
});

test("episode lookups preserve the first duplicate and follow rescans and resets", async () => {
  api.scanLibrary.mockResolvedValueOnce({ files: [
    { path: "/anime/first.mkv", media_id: 1, episode: 1 },
    { path: "/anime/second.mkv", media_id: 1, episode: 1 },
    { path: "/anime/unmatched.mkv", media_id: null, episode: 1 },
    { path: "/anime/extra.mkv", media_id: 2, episode: null },
  ], unreadable: [] }).mockResolvedValueOnce({ files: [
    { path: "/anime/first.mkv", media_id: 2, episode: 5 },
    { path: "/anime/second.mkv", media_id: 1, episode: 1 },
  ], unreadable: [] });
  const { library } = await import("#lib/library.svelte.ts");
  await library.scan();
  expect(library.fileFor(1, 1)?.path).toBe("/anime/first.mkv");
  expect(library.fileFor(2, 1)).toBeUndefined();

  await library.scan();
  expect(library.fileFor(1, 1)?.path).toBe("/anime/second.mkv");
  expect(library.fileFor(2, 5)?.path).toBe("/anime/first.mkv");

  library.reset();
  expect(library.fileFor(1, 1)).toBeUndefined();
  expect(library.fileFor(2, 5)).toBeUndefined();
});
