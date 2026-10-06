import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount, type Component } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, selectOption, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries(["localEntries", "syncMyList", "getSeason", "getAiringSchedule", "getLibraryBindingDetails", "bindLibraryPath", "getWatchHistory"].map((name) => [name, vi.fn()])),
  library: { files: [] as unknown[], folders: [] as string[], unreadable: [], foldersFailed: false, scanning: false, hasScan: true, ensureScan: vi.fn(), scan: vi.fn(), fileFor: vi.fn() },
  listeners: new Map<string, Set<() => void>>(),
  navigation: [] as (() => void)[],
  openUrl: vi.fn(),
  registration: null as Promise<void> | null,
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/auth.svelte", async () => import("./session.svelte"));
vi.mock("$lib/library.svelte", () => ({ library: mocks.library }));
vi.mock("$app/navigation", () => ({ goto: vi.fn(), afterNavigate: (callback: () => void) => mocks.navigation.push(callback) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, callback: () => void) => {
    if (mocks.registration) await mocks.registration;
    const handlers = mocks.listeners.get(event) ?? new Set();
    handlers.add(callback);
    mocks.listeners.set(event, handlers);
    return () => handlers.delete(callback);
  }),
  emit: vi.fn(),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: mocks.openUrl, openPath: vi.fn(), revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import Collection from "../src/routes/+page.svelte";
import Seasons from "../src/routes/seasons/+page.svelte";
import Calendar from "../src/routes/calendar/+page.svelte";
import Library from "../src/routes/library/+page.svelte";
import TorrentRow from "$lib/TorrentRow.svelte";

const media = { id: 1, title_english: "Example Show", episodes: 12, format: "TV" };
const entry = { media_id: 1, status: "CURRENT", progress: 0, repeat: 0, media };
let mounted: ReturnType<typeof mount>[] = [];
let epoch = 0;
async function render(component: Component<any>, props?: any) {
  const target = document.createElement("div");
  document.body.append(target);
  const instance = mount(component, { target, props });
  mounted.push(instance);
  await settle();
  for (const callback of mocks.navigation) callback();
  await settle();
  return instance as { snapshot?: { capture: () => any; restore: (snapshot: any) => void } };
}

beforeEach(() => {
  vi.resetAllMocks();
  mocks.listeners.clear();
  mocks.navigation.length = 0;
  mocks.registration = null;
  localStorage.clear();
  auth.epoch = ++epoch;
  auth.isLoggedIn = true;
  auth.offline = false;
  auth.user = { id: 1, name: "Example", score_format: "POINT_100" };
  mocks.api.localEntries.mockResolvedValue([entry]);
  mocks.api.syncMyList.mockResolvedValue([entry]);
  mocks.api.getSeason.mockResolvedValue([media]);
  mocks.api.getAiringSchedule.mockResolvedValue([]);
  mocks.api.getWatchHistory.mockResolvedValue([]);
  mocks.library.files = [];
  mocks.library.folders = ["/anime"];
  mocks.library.ensureScan.mockResolvedValue(undefined);
  mocks.library.scan.mockResolvedValue(undefined);
});

afterEach(async () => {
  for (const instance of mounted) await unmount(instance);
  mounted = [];
  document.body.replaceChildren();
  vi.useRealTimers();
});

test("a cached list remains visible while its startup refresh is pending", async () => {
  const sync = deferred<unknown>();
  mocks.api.syncMyList.mockReturnValue(sync.promise);
  await render(Collection);
  expect(document.body.textContent).toContain("Example Show");
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  sync.resolve([{ ...entry, progress: 3 }]);
  await settle();
  expect(document.body.textContent).toContain("3 Episodes Tracked");
});

test("an offline cached list does not attempt an automatic network sync", async () => {
  auth.offline = true;
  await render(Collection);
  expect(document.body.textContent).toContain("Example Show");
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
});

test("switching accounts retains that account's saved entries when network sync fails", async () => {
  await render(Collection);
  const saved = { ...entry, media_id: 2, progress: 4, media: { ...media, id: 2, title_english: "Saved second account show" } };
  mocks.api.localEntries.mockResolvedValue([saved]);
  mocks.api.syncMyList.mockRejectedValue(new Error("AniList unavailable"));
  auth.epoch = ++epoch;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  await settle();
  expect(document.body.textContent).toContain("Saved second account show");
  expect(document.body.textContent).toContain("4 Episodes Tracked");
  expect(document.body.textContent).toContain("AniList unavailable");
  expect(document.body.textContent).not.toContain("Example Show");
});

test("a previous account's pending local read cannot replace the new account's saved list", async () => {
  const old = deferred<unknown[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise);
  await render(Collection);
  const saved = { ...entry, media_id: 2, progress: 4, media: { ...media, id: 2, title_english: "Saved next account show" } };
  mocks.api.localEntries.mockResolvedValue([saved]);
  mocks.api.syncMyList.mockRejectedValue(new Error("AniList unavailable"));
  auth.epoch = ++epoch;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  await settle();
  old.resolve([entry]);
  await settle();
  expect(document.body.textContent).toContain("Saved next account show");
  expect(document.body.textContent).not.toContain("Example Show");
});

test("seasons can jump directly to a year and combine title, format and list filters", async () => {
  mocks.api.getSeason.mockResolvedValue([
    media,
    { ...media, id: 2, title_english: "Example Movie", format: "MOVIE" },
    { ...media, id: 3, title_english: "Different Show" },
  ]);
  await render(Seasons);
  const year = document.querySelector("#season-year") as HTMLInputElement;
  year.value = "2001";
  year.dispatchEvent(new Event("input", { bubbles: true }));
  button("Go to season").click();
  await settle();
  expect(mocks.api.getSeason).toHaveBeenLastCalledWith(expect.any(String), 2001);
  const filter = document.querySelector('[aria-label="Filter season titles"]') as HTMLInputElement;
  filter.value = "example";
  filter.dispatchEvent(new Event("input", { bubbles: true }));
  selectOption("#season-format", "MOVIE");
  selectOption("#season-list-status", "UNLISTED");
  await settle();
  expect(document.body.textContent).toContain("Example Movie");
  expect(document.body.textContent).not.toContain("Example Show");
  expect(document.body.textContent).not.toContain("Different Show");
  mocks.api.getSeason.mockResolvedValue([media]);
  button("Next →").click();
  await settle();
  expect((document.querySelector("#season-format") as HTMLSelectElement).value).toBe("MOVIE");
  expect(document.body.textContent).toContain("No shows match these filters.");
});

test("calendar restores the absolute schedule window rather than shifting it with today", async () => {
  const instance = await render(Calendar);
  const snapshot = instance.snapshot!.capture();
  const oldStart = new Date(2024, 0, 10).setHours(0, 0, 0, 0);
  instance.snapshot!.restore({ ...snapshot, anchorDay: oldStart, weekOffset: 0, items: [{ media, episode: 1, airing_at: oldStart / 1000 + 3600 }] });
  await settle();
  expect(document.body.textContent).toContain("Jan 10");
  expect(document.body.textContent).toContain("Jan 16");
  expect(button("Today").disabled).toBe(false);
  button("Refresh").click();
  await settle();
  expect(mocks.api.getAiringSchedule).toHaveBeenLastCalledWith(oldStart / 1000, new Date(2024, 0, 17).getTime() / 1000);
});

test("library numbering saves a chosen folder offset and rescans", async () => {
  mocks.library.files = [{ path: "/anime/show/13.mkv", media_id: 1, matched: "Example Show", episode: 13 }];
  mocks.api.getLibraryBindingDetails.mockResolvedValue(null);
  mocks.api.bindLibraryPath.mockResolvedValue(undefined);
  await render(Library);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Adjust episode numbering"]') as HTMLButtonElement).click();
  await settle();
  (document.querySelector('input[value="folder"]') as HTMLInputElement).click();
  await settle();
  const offset = document.querySelector("#library-episode-offset") as HTMLInputElement;
  offset.value = "-12";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  button("Save numbering").click();
  await settle();
  expect(mocks.api.bindLibraryPath).toHaveBeenCalledWith("/anime/show", 1, -12);
  expect(mocks.library.scan).toHaveBeenCalledOnce();
});

test("file numbering preserves the effective inherited folder offset", async () => {
  mocks.library.files = [{ path: "/anime/show/13.mkv", media_id: 1, matched: "Example Show", episode: 1, bound: true }];
  mocks.api.getLibraryBindingDetails.mockResolvedValue({ media_id: 1, episode_offset: -12 });
  mocks.api.bindLibraryPath.mockResolvedValue(undefined);
  await render(Library);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Adjust episode numbering"]') as HTMLButtonElement).click();
  await settle();
  expect((document.querySelector("#library-episode-offset") as HTMLInputElement).value).toBe("-12");
  button("Save numbering").click();
  await settle();
  expect(mocks.api.bindLibraryPath).toHaveBeenCalledWith("/anime/show/13.mkv", 1, -12);
});

test("watch history loads on demand and follows its older-entry cursor", async () => {
  const rows = Array.from({ length: 50 }, (_, index) => ({ id: 100 - index, media_id: 1, title: "Example Show", episode: 1, watched_at: 1700000000, path: "/anime/show/1.mkv" }));
  mocks.api.getWatchHistory.mockResolvedValueOnce(rows).mockResolvedValue([]);
  await render(Library);
  expect(mocks.api.getWatchHistory).not.toHaveBeenCalled();
  (document.querySelector('[aria-controls="library-watch-history"]') as HTMLButtonElement).click();
  await settle();
  button("Load older history").click();
  await settle();
  expect(mocks.api.getWatchHistory).toHaveBeenLastCalledWith(50, 51);
});

test("a new watch history event supersedes a pending history page", async () => {
  const old = deferred<unknown[]>();
  mocks.api.getWatchHistory.mockReturnValueOnce(old.promise).mockResolvedValue([{ id: 2, media_id: 1, title: "New playback", episode: 2, watched_at: 1700000000, path: "/anime/2.mkv" }]);
  await render(Library);
  (document.querySelector('[aria-controls="library-watch-history"]') as HTMLButtonElement).click();
  await settle();
  for (const callback of mocks.listeners.get("kurisu://watch-history-updated") ?? []) callback();
  await settle();
  old.resolve([{ id: 1, media_id: 1, title: "Obsolete playback", episode: 1, watched_at: 1700000000, path: "/anime/1.mkv" }]);
  await settle();
  expect(document.body.textContent).toContain("New playback");
  expect(document.body.textContent).not.toContain("Obsolete playback");
});

test("old account history cannot populate the next account", async () => {
  const old = deferred<unknown[]>();
  mocks.api.getWatchHistory.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  await render(Library);
  (document.querySelector('[aria-controls="library-watch-history"]') as HTMLButtonElement).click();
  await settle();
  auth.epoch = ++epoch;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  old.resolve([{ id: 1, media_id: 1, title: "Previous account playback", episode: 1, watched_at: 1700000000, path: "/anime/1.mkv" }]);
  await settle();
  (document.querySelector('[aria-controls="library-watch-history"]') as HTMLButtonElement).click();
  await settle();
  expect(document.body.textContent).not.toContain("Previous account playback");
  expect(document.body.textContent).toContain("No watched episodes recorded yet.");
});

test("library refuses numbering that would overwrite a different folder binding", async () => {
  mocks.library.files = [{ path: "/anime/show/13.mkv", media_id: 1, matched: "Example Show", episode: 13 }];
  mocks.api.getLibraryBindingDetails.mockResolvedValueOnce(null).mockResolvedValue({ media_id: 2, episode_offset: 0 });
  await render(Library);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Adjust episode numbering"]') as HTMLButtonElement).click();
  await settle();
  (document.querySelector('input[value="folder"]') as HTMLInputElement).click();
  await settle();
  expect(button("Save numbering").disabled).toBe(true);
  expect(document.body.textContent).toContain("This location is linked to a different show.");
  expect(mocks.api.bindLibraryPath).not.toHaveBeenCalled();
});

test("viewing a torrent release page does not mark it downloaded", async () => {
  const onopen = vi.fn();
  await render(TorrentRow, { torrent: { title: "Example release", guid: "one", link: "https://nyaa.si/download/1.torrent", details_url: "https://nyaa.si/view/1", seen: false, is_new: false }, onopen });
  (document.querySelector('[title="Open release page"]') as HTMLButtonElement).click();
  await settle();
  expect(mocks.openUrl).toHaveBeenCalledWith("https://nyaa.si/view/1");
  expect(onopen).not.toHaveBeenCalled();
});

test("torrent release page action rejects non-web addresses", async () => {
  const onopen = vi.fn();
  await render(TorrentRow, { torrent: { title: "Example release", guid: "one", link: "https://nyaa.si/download/1.torrent", details_url: "file:///tmp/example", seen: false, is_new: false }, onopen });
  (document.querySelector('[title="Open release page"]') as HTMLButtonElement).click();
  await settle();
  expect(mocks.openUrl).not.toHaveBeenCalled();
  expect(onopen).not.toHaveBeenCalled();
  expect(document.body.textContent).toContain("Unsupported release page address");
});

test("seasons re-evaluates its list filter when a queued entry is discarded", async () => {
  await render(Seasons);
  selectOption("#season-list-status", "CURRENT");
  await settle();
  expect(document.body.textContent).toContain("Example Show");
  mocks.api.localEntries.mockResolvedValue([]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.body.textContent).not.toContain("Example Show");
  expect(document.body.textContent).toContain("No shows match these filters.");
});

test("calendar watching view drops a discarded queued entry", async () => {
  mocks.api.getAiringSchedule.mockResolvedValue([{ media, episode: 1, airing_at: Date.now() / 1000 }]);
  await render(Calendar);
  expect(document.body.textContent).toContain("Example Show");
  mocks.api.localEntries.mockResolvedValue([]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.body.textContent).not.toContain("Example Show");
});

test.each([["seasons", Seasons], ["calendar", Calendar], ["library", Library]] as const)("%s catches changes while initial list listeners register", async (name, component) => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  mocks.api.getAiringSchedule.mockResolvedValue([{ media, episode: 1, airing_at: Date.now() / 1000 }]);
  mocks.library.files = [{ path: "/anime/show/1.mkv", media_id: 1, matched: "Example Show", episode: 1 }];
  await render(component);
  if (name === "seasons") {
    selectOption("#season-list-status", "CURRENT");
    await settle();
  }
  mocks.api.localEntries.mockResolvedValue([]);
  registration.resolve();
  await settle();
  if (name === "library") expect(document.body.textContent).toContain("Not on your list");
  else expect(document.body.textContent).not.toContain("Example Show");
});

test("offline My List catches changes during initial listener registration", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  auth.offline = true;
  await render(Collection);
  mocks.api.localEntries.mockResolvedValue([]);
  registration.resolve();
  await settle();
  expect(document.body.textContent).not.toContain("Example Show");
});
