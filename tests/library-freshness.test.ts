import { afterEach, beforeEach, expect, test, vi } from "vitest";

const api = vi.hoisted(() => ({ getLibraryFolders: vi.fn(), scanLibrary: vi.fn() }));
vi.mock("$lib/api", () => ({ api }));

beforeEach(() => { vi.resetModules(); vi.resetAllMocks(); });
afterEach(() => { vi.restoreAllMocks(); });

test("library reuses a recent scan and refreshes it once stale", async () => {
  const now = vi.spyOn(Date, "now").mockReturnValue(1_000_000);
  api.getLibraryFolders.mockResolvedValue(["/anime"]);
  api.scanLibrary.mockResolvedValueOnce({ files: [], unreadable: [] }).mockResolvedValue({ files: [{ path: "/anime/new.mkv", media_id: 1, episode: 1 }], unreadable: [] });
  const { library } = await import("$lib/library.svelte");
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
  const { library } = await import("$lib/library.svelte");
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
  const { library } = await import("$lib/library.svelte");
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
