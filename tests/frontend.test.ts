import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount, type Component } from "svelte";
import { openUrl } from "@tauri-apps/plugin-opener";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries([
    "getEntry", "setProgress", "updateEntry", "deleteEntry", "getMedia", "getMediaDetail", "getRecommendations",
    "localEntries", "getAiringSchedule", "getTrackingConfig", "getAppSetting", "setTrackingConfig",
    "getLibraryBinding", "bindLibraryPath", "getRssFeeds", "fetchTorrents", "searchTorrents", "findShowTorrents", "markTorrentsSeen", "setAppSetting",
  ].map((name) => [name, vi.fn()])),
  listeners: new Map<string, Set<(event: { payload: unknown }) => void>>(),
  navigation: [] as (() => void)[],
  library: {
    files: [] as unknown[], folders: [] as string[], unreadable: [], scanning: false, hasScan: false,
    foldersFailed: false, fileFor: vi.fn(), loadFolders: vi.fn(), scan: vi.fn(), ensureScan: vi.fn(),
  },
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/auth.svelte", async () => import("./session.svelte"));
vi.mock("$lib/library.svelte", () => ({ library: mocks.library }));
vi.mock("$lib/nowplaying.svelte", () => ({ nowPlaying: () => null }));
vi.mock("$app/navigation", () => ({ goto: vi.fn(), afterNavigate: (callback: () => void) => mocks.navigation.push(callback) }));
vi.mock("$app/stores", async () => {
  const { writable } = await import("svelte/store");
  return { page: writable({ params: { id: "1" }, url: new URL("http://localhost/anime/1") }) };
});
vi.mock("@tauri-apps/api/event", () => ({
  emit: vi.fn(async () => {}),
  listen: vi.fn(async (event: string, handler: (event: { payload: unknown }) => void) => {
    const handlers = mocks.listeners.get(event) ?? new Set();
    handlers.add(handler);
    mocks.listeners.set(event, handlers);
    return () => handlers.delete(handler);
  }),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn(), openUrl: vi.fn(), revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import Tracking from "$lib/Tracking.svelte";
import LinkAnime from "$lib/LinkAnime.svelte";
import EditEntry from "$lib/EditEntry.svelte";
import Detail from "../src/routes/anime/[id]/+page.svelte";
import Calendar from "../src/routes/calendar/+page.svelte";
import Settings from "../src/routes/settings/+page.svelte";
import Library from "../src/routes/library/+page.svelte";
import Torrents from "../src/routes/torrents/+page.svelte";
import Search from "../src/routes/search/+page.svelte";
import ShowTorrentChoices from "$lib/ShowTorrentChoices.svelte";
import type { ShowTorrents } from "$lib/types";

const media = { id: 1, title_english: "Example Show", episodes: 12 };
const entry = { media_id: 1, status: "CURRENT", progress: 0, repeat: 0, score: 75, media };
const config = { mode: "off", prompt_seconds: 120, auto_percent: 80, auto_ask: true, mpv_ipc_socket: "", discord_enabled: true };
const emptyShowTorrents = { batches: [], episodes: [], other: [], next_episode: 1, warnings: [] };
function torrent(title: string) {
  return { title, guid: title, link: `https://example.com/${encodeURIComponent(title)}.torrent`, category_id: "1_2", trusted: true, remake: false, is_new: false, seen: false };
}
let mounted: ReturnType<typeof mount>[] = [];

async function render<C extends Component<any>>(component: C, props?: any) {
  const target = document.createElement("div");
  document.body.append(target);
  const instance = mount(component, { target, props });
  mounted.push(instance);
  await settle();
  for (const callback of mocks.navigation) callback();
  await settle();
  return instance as ReturnType<C>;
}

function event(name: string, payload: unknown) {
  for (const callback of mocks.listeners.get(name) ?? []) callback({ payload });
}

beforeEach(() => {
  vi.resetAllMocks();
  mocks.listeners.clear();
  mocks.navigation.length = 0;
  localStorage.clear();
  auth.epoch = 0;
  auth.isLoggedIn = true;
  auth.offline = false;
  auth.user = { id: 1, name: "Example", score_format: "POINT_100" };
  mocks.library.files = [];
  mocks.library.folders = [];
  mocks.library.hasScan = false;
  mocks.library.loadFolders.mockResolvedValue(undefined);
  mocks.library.ensureScan.mockResolvedValue(undefined);
  mocks.api.getEntry.mockResolvedValue({ ...entry });
  mocks.api.localEntries.mockResolvedValue([{ ...entry }]);
  mocks.api.getMedia.mockResolvedValue(media);
  mocks.api.getMediaDetail.mockResolvedValue({ media, relations: [], characters: [], staff: [] });
  mocks.api.getRecommendations.mockResolvedValue([]);
  mocks.api.getTrackingConfig.mockResolvedValue({ ...config });
  mocks.api.getAppSetting.mockResolvedValue(null);
  mocks.api.getRssFeeds.mockResolvedValue([]);
  mocks.api.fetchTorrents.mockResolvedValue({ items: [], failures: [] });
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents });
  mocks.api.markTorrentsSeen.mockResolvedValue(undefined);
});

afterEach(async () => {
  for (const instance of mounted) await unmount(instance);
  mounted = [];
  document.body.replaceChildren();
});

test("tracking cannot confirm a prompt from a previous account", async () => {
  const read = deferred<typeof entry>();
  mocks.api.getEntry.mockReturnValue(read.promise);
  await render(Tracking);
  event("kurisu://tracking-prompt", { media_id: 1, episode: 8, progress: 0, title: "Example Show" });
  await settle();
  button("Set progress to Ep 8").click();
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  await settle();
  read.resolve({ ...entry });
  await settle();
  expect(mocks.api.setProgress).not.toHaveBeenCalled();
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

test("tracking clears queued prompts on logout", async () => {
  await render(Tracking);
  event("kurisu://tracking-prompt", { media_id: 1, episode: 8, progress: 0, title: "First" });
  event("kurisu://tracking-prompt", { media_id: 2, episode: 3, progress: 0, title: "Second" });
  await settle();
  auth.epoch++;
  auth.isLoggedIn = false;
  auth.user = null;
  await settle();
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

test("tracking confirms against the latest progress baseline", async () => {
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 3 });
  mocks.api.setProgress.mockResolvedValue({ ...entry, progress: 8 });
  await render(Tracking);
  event("kurisu://tracking-prompt", { media_id: 1, episode: 8, progress: 0, title: "Example Show" });
  await settle();
  button("Set progress to Ep 8").click();
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledWith(1, 8, 3);
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

test("Completed immediately fills known episode totals", async () => {
  mocks.api.updateEntry.mockResolvedValue({ ...entry, status: "COMPLETED", progress: 12 });
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose: vi.fn() });
  button("Watching ▾").click();
  await settle();
  button("Completed").click();
  await settle();
  expect((document.querySelector("#ed-progress") as HTMLInputElement).value).toBe("12");
  button("Save").click();
  await settle();
  expect(mocks.api.updateEntry).toHaveBeenCalledWith(1, "COMPLETED", 12, null, null);
});

test.each(["save", "remove"])("a pending entry %s cannot close through its details link", async (action) => {
  const saving = deferred<typeof entry>();
  const onclose = vi.fn();
  mocks.api.updateEntry.mockReturnValue(saving.promise);
  mocks.api.deleteEntry.mockReturnValue(saving.promise);
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  const input = document.querySelector("#ed-progress") as HTMLInputElement;
  input.value = "3";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  if (action === "save") button("Save").click();
  else {
    button("Remove from list").click();
    await settle();
    button("Confirm remove").click();
  }
  await settle();
  for (const control of document.querySelectorAll("#ed-status, #ed-progress, #ed-score, #ed-repeat")) {
    expect(control.matches(":disabled")).toBe(true);
  }
  button("Example Show").click();
  expect(onclose).not.toHaveBeenCalled();
  saving.resolve({ ...entry, progress: 3 });
  await settle();
  expect(onclose).toHaveBeenCalledOnce();
});

test("a failed entry save keeps edits and allows retry", async () => {
  mocks.api.updateEntry.mockRejectedValueOnce(new Error("Connection lost"))
    .mockResolvedValue({ ...entry, progress: 3 });
  const onclose = vi.fn();
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  const input = document.querySelector("#ed-progress") as HTMLInputElement;
  input.value = "3";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Save").click();
  await settle();
  expect(document.body.textContent).toContain("Connection lost");
  expect(input.value).toBe("3");
  expect(input.matches(":disabled")).toBe(false);
  expect(onclose).not.toHaveBeenCalled();
  button("Save").click();
  await settle();
  expect(onclose).toHaveBeenCalledOnce();
});

test("a legacy offline profile cannot edit scores with an unknown scale", async () => {
  auth.offline = true;
  auth.user!.score_format = null;
  await render(EditEntry, { entry, scoreFormat: null, onclose: vi.fn() });
  expect(document.querySelector("#ed-score")).toBeNull();
  expect(document.body.textContent).toContain("Reconnect to AniList");
  expect(document.querySelector("#ed-progress")).not.toBeNull();
});

test("anime detail reflects tracking updates", async () => {
  await render(Detail);
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 3 });
  event("kurisu://episode-updated", { ...entry, progress: 3 });
  await settle();
  const stepper = document.querySelector('[aria-label="One more episode"]')?.parentElement;
  expect(stepper?.textContent?.replace(/\s/g, "")).toContain("3/12");
});

test("stale detail add actions preserve progress and rewatches", async () => {
  mocks.api.getEntry.mockResolvedValue(null);
  await render(Detail);
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 3, repeat: 2 });
  mocks.api.updateEntry.mockResolvedValue({ ...entry, status: "PLANNING", progress: 3, repeat: 2 });
  button("Plan to watch").click();
  await settle();
  expect(mocks.api.updateEntry).toHaveBeenCalledWith(1, "PLANNING", null, null, null);
});

test("failed calendar navigation does not retain the previous week's shows", async () => {
  mocks.api.getAiringSchedule.mockResolvedValue([{ airing_at: Date.now() / 1000, episode: 1, media }]);
  await render(Calendar);
  expect(document.body.textContent).toContain("Example Show");
  expect(mocks.api.getAiringSchedule).toHaveBeenCalledOnce();
  mocks.api.getAiringSchedule.mockRejectedValue(new Error("network"));
  (document.querySelector('[title="Next week"]') as HTMLButtonElement).click();
  await settle();
  expect(document.body.textContent).toContain("network");
  expect(document.body.textContent).not.toContain("Example Show");
});

test("tracking edits made during a save remain visibly unsaved", async () => {
  const saving = deferred<typeof config>();
  mocks.api.setTrackingConfig.mockReturnValue(saving.promise);
  await render(Settings);
  button("Save tracking").click();
  const radio = document.querySelector('input[value="auto"]') as HTMLInputElement;
  radio.checked = true;
  radio.dispatchEvent(new Event("input", { bubbles: true }));
  radio.dispatchEvent(new Event("change", { bubbles: true }));
  await settle();
  saving.resolve({ ...config });
  await settle();
  expect(radio.checked).toBe(true);
  expect(document.body.textContent).not.toContain("saved ✓");
});

test("a failed binding check cannot silently replace an existing link", async () => {
  mocks.api.getLibraryBinding.mockRejectedValue(new Error("database unavailable"));
  await render(LinkAnime, { path: "/anime/show/episode.mkv", roots: ["/anime"], entries: [entry], onclose: vi.fn(), onlinked: vi.fn() });
  const choice = button("Example Show Watching");
  expect(choice.disabled).toBe(true);
  expect(document.body.textContent).toContain("database unavailable");
});

test("retrying a binding check restores explicit relink confirmation", async () => {
  mocks.api.getLibraryBinding.mockRejectedValueOnce(new Error("database unavailable")).mockResolvedValue(99);
  await render(LinkAnime, { path: "/anime/show/episode.mkv", roots: ["/anime"], entries: [entry], onclose: vi.fn(), onlinked: vi.fn() });
  button("Retry").click();
  await settle();
  button("Example Show Watching").click();
  await settle();
  expect(mocks.api.bindLibraryPath).not.toHaveBeenCalled();
  button("Relink to Example Show Watching").click();
  await settle();
  expect(mocks.api.bindLibraryPath).toHaveBeenCalledWith("/anime/show", 1);
});

test("library dialogs close when the account changes", async () => {
  mocks.library.files = [{ path: "/anime/episode.mkv", media_id: null, episode: 1 }];
  mocks.library.hasScan = true;
  mocks.api.getLibraryBinding.mockResolvedValue(null);
  await render(Library);
  (document.querySelector('[title="Link to a show on your list"]') as HTMLButtonElement).click();
  await settle();
  expect(document.querySelector('[role="dialog"]')).not.toBeNull();
  auth.epoch++;
  auth.isLoggedIn = false;
  auth.user = null;
  await settle();
  expect(document.querySelector('[role="dialog"]')).toBeNull();
});

test("pending torrent searches resume after returning through history", async () => {
  const search = deferred<unknown[]>();
  mocks.api.searchTorrents.mockReturnValueOnce(search.promise).mockResolvedValue([]);
  const instance = await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.closest("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await settle();
  const snapshot = instance.snapshot.capture();
  await unmount(instance);
  mounted = mounted.filter((item) => item !== instance);
  document.body.replaceChildren();
  mocks.navigation.length = 0;
  const restored = await render(Torrents);
  restored.snapshot.restore(snapshot);
  for (const callback of mocks.navigation) callback();
  await settle();
  expect(mocks.api.searchTorrents).toHaveBeenCalledTimes(2);
  expect(mocks.api.searchTorrents).toHaveBeenLastCalledWith("Example", "1_0", "0");
});

test("torrent search results from an old account are discarded", async () => {
  const search = deferred<unknown[]>();
  mocks.api.searchTorrents.mockReturnValue(search.promise);
  await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.closest("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await settle();
  auth.epoch++;
  await settle();
  search.resolve([{ title: "Old account result", guid: "old", link: "https://example.com/release", seen: false }]);
  await settle();
  expect(document.body.textContent).not.toContain("Old account result");
});

test.each(["CURRENT", "REPEATING", "PLANNING", "PAUSED", "COMPLETED", "DROPPED"])("finished %s shows remain available without feed releases", async (status) => {
  mocks.api.localEntries.mockResolvedValue([{ ...entry, status, media: { ...media, status: "FINISHED" } }]);
  await render(Torrents);
  if (status !== "CURRENT") {
    (document.querySelector(`[aria-controls="torrents-${status}"]`) as HTMLButtonElement).click();
    await settle();
  }
  const header = button("Example Show").closest('[role="button"]') as HTMLElement;
  expect(header.getAttribute("aria-expanded")).toBe("false");
  expect(mocks.api.findShowTorrents).not.toHaveBeenCalled();
  header.click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledOnce();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledWith(1, "1_0", "0");
});

test("a saved new feed preference keeps the full list and show downloads available", async () => {
  localStorage.setItem("kurisu.torrents.new", "1");
  mocks.api.localEntries.mockResolvedValue([
    { ...entry, media: { ...media, status: "FINISHED" } },
    { ...entry, media_id: 2, status: "PLANNING", media: { ...media, id: 2, title_english: "Planned Show" } },
  ]);
  mocks.api.fetchTorrents.mockResolvedValue({ items: [{ ...torrent("Already seen feed release"), media_id: 1, episode: 1, seen: true }], failures: [] });
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Existing batch option")] });
  await render(Torrents);
  expect(mocks.api.findShowTorrents).not.toHaveBeenCalled();
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledWith(1, "1_0", "0");
  expect(document.body.textContent).toContain("Existing batch option");
  expect(document.body.textContent).not.toContain("Already seen feed release");
  (document.querySelector('[aria-controls="torrents-PLANNING"]') as HTMLButtonElement).click();
  await settle();
  expect(button("Planned Show")).toBeDefined();
});

test("torrent sections filter their own shows independently", async () => {
  mocks.api.localEntries.mockResolvedValue([
    { ...entry, media: { ...media, title_english: "Alpha Watch" } },
    { ...entry, media_id: 2, media: { ...media, id: 2, title_english: "Beta Watch" } },
    { ...entry, media_id: 3, status: "PLANNING", media: { ...media, id: 3, title_english: "Alpha Plan" } },
    { ...entry, media_id: 4, status: "PLANNING", media: { ...media, id: 4, title_english: "Beta Plan" } },
  ]);
  await render(Torrents);
  (document.querySelector('[aria-controls="torrents-PLANNING"]') as HTMLButtonElement).click();
  await settle();
  const watching = document.querySelector("#torrents-CURRENT")!;
  const planning = document.querySelector("#torrents-PLANNING")!;
  const watchFilter = document.querySelector('[aria-label="Filter Watching shows"]') as HTMLInputElement;
  const planFilter = document.querySelector('[aria-label="Filter Plan to Watch shows"]') as HTMLInputElement;
  watchFilter.value = "alpha";
  watchFilter.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  expect(watching.textContent).toContain("Alpha Watch");
  expect(watching.textContent).not.toContain("Beta Watch");
  expect(planning.textContent).toContain("Alpha Plan");
  expect(planning.textContent).toContain("Beta Plan");
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("2 shows");
  planFilter.value = "beta";
  planFilter.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  expect(planning.textContent).not.toContain("Alpha Plan");
  expect(planning.textContent).toContain("Beta Plan");
  expect(watching.textContent).toContain("Alpha Watch");
  expect(watching.textContent).not.toContain("Beta Watch");
  watchFilter.value = "";
  watchFilter.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  expect(watching.textContent).toContain("Beta Watch");
  expect(planning.textContent).not.toContain("Alpha Plan");
  expect(mocks.api.findShowTorrents).not.toHaveBeenCalled();
  expect([...document.querySelectorAll("button")].some((item) => item.textContent?.trim() === "All")).toBe(false);
  expect([...document.querySelectorAll("button")].some((item) => item.textContent?.trim() === "Watching")).toBe(false);
});

test("show downloads offer batches before next episode subgroup alternatives", async () => {
  mocks.api.localEntries.mockResolvedValue([{ ...entry, progress: 4 }]);
  const batches = [torrent("[Group A] Example Show Complete"), torrent("[Group B] Example Show Batch")];
  const episodes = [torrent("[Group A] Example Show - 05"), torrent("[Group C] Example Show - 05")];
  const other = [torrent("[Group D] Example Show Special")];
  mocks.api.findShowTorrents.mockResolvedValue({ batches, episodes, other, next_episode: 5, warnings: ["One title search was unavailable"] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  const text = document.body.textContent!;
  expect(text).toContain("Batches");
  expect(text).toContain("Next episode · 5");
  expect(text).toContain("Other releases");
  expect(text).toContain("One title search was unavailable");
  for (const release of [...batches, ...episodes, ...other]) expect(text).toContain(release.title);
  expect(text.indexOf(batches[1].title)).toBeLessThan(text.indexOf(episodes[0].title));
  expect(text.indexOf(episodes[1].title)).toBeLessThan(text.indexOf(other[0].title));
  expect(document.querySelectorAll('[title="Download .torrent"]')).toHaveLength(5);
});

test("downloading a suggested release also marks its matching feed release seen", async () => {
  const suggested = { ...torrent("[Group A] Example Show - 01"), guid: "nyaa-show\u0001one", media_id: 1, episode: 1 };
  const feed = { ...suggested, guid: "feed\u0001one", matched: "Example Show" };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, episodes: [suggested] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  const categoryHeader = document.querySelector('[aria-controls="torrents-CURRENT"]')!;
  expect(categoryHeader.textContent).toContain("1 new");
  const downloads = document.querySelectorAll('[title="Download .torrent"]');
  expect(downloads).toHaveLength(1);
  (downloads[0] as HTMLButtonElement).click();
  await settle();
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([suggested.guid, feed.guid]));
  expect(categoryHeader.textContent).not.toContain("1 new");
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
});

test.each([true, false])("a pending lookup preserves the feed download seen state after success=%s", async (success) => {
  const finding = deferred<unknown>();
  const suggested = { ...torrent("[Group A] Example Show - 01"), guid: "nyaa-show\u0001key", media_id: 1, episode: 1 };
  const feed = { ...suggested, guid: "feed\u0001one", seen_guid: suggested.guid, matched: "Example Show" };
  if (!success) vi.mocked(openUrl).mockRejectedValueOnce(new Error("Torrent client unavailable"));
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.findShowTorrents.mockReturnValue(finding.promise);
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  finding.resolve({ ...emptyShowTorrents, episodes: [suggested] });
  await settle();
  const downloads = document.querySelectorAll('[title="Download .torrent"]');
  expect(downloads).toHaveLength(1);
  expect(downloads[0].closest(".cv-row")!.classList.contains("opacity-60")).toBe(success);
  if (!success) expect(mocks.api.markTorrentsSeen).not.toHaveBeenCalled();
  const header = button("Example Show").closest('[role="button"]') as HTMLElement;
  header.click();
  await settle();
  header.click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledOnce();
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(success);
});

test("a hidden new feed release stays seen in a late lookup while its save is pending", async () => {
  localStorage.setItem("kurisu.torrents.new", "1");
  const finding = deferred<unknown>();
  const marking = deferred<void>();
  const suggested = { ...torrent("[Group A] Example Show - 01"), guid: "nyaa-show\u0001key", media_id: 1, episode: 1 };
  const feed = { ...suggested, guid: "feed\u0001one", seen_guid: suggested.guid, matched: "Example Show" };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.findShowTorrents.mockReturnValue(finding.promise);
  mocks.api.markTorrentsSeen.mockReturnValue(marking.promise);
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  expect(document.querySelector('[title="Download .torrent"]')).toBeNull();
  finding.resolve({ ...emptyShowTorrents, episodes: [suggested] });
  await settle();
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
  marking.resolve(undefined);
  await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).not.toContain("1 new");
});

test("downloading a feed release persists its stable discovery identity", async () => {
  const feed = { ...torrent("[Group A] Example Show - 01"), guid: "feed\u0001one", seen_guid: "nyaa-show\u0001key", media_id: 1, episode: 1 };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([feed.guid, feed.seen_guid]));
});

test("a feed arriving after a suggested download does not announce that release as new", async () => {
  const fetching = deferred<unknown>();
  const suggested = { ...torrent("[Group A] Example Show - 01"), guid: "nyaa-show\u0001one", media_id: 1, episode: 1 };
  const feed = { ...suggested, guid: "feed\u0001one", matched: "Example Show" };
  mocks.api.fetchTorrents.mockReturnValue(fetching.promise);
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, episodes: [suggested] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  fetching.resolve({ items: [feed], failures: [] });
  await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).not.toContain("1 new");
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([feed.guid]));
});

test("a failed download remains new when its feed arrives", async () => {
  const fetching = deferred<unknown>();
  const suggested = { ...torrent("[Group A] Example Show - 01"), guid: "nyaa-show\u0001one", media_id: 1, episode: 1 };
  const feed = { ...suggested, guid: "feed\u0001one", matched: "Example Show" };
  vi.mocked(openUrl).mockRejectedValueOnce(new Error("Torrent client unavailable"));
  mocks.api.fetchTorrents.mockReturnValue(fetching.promise);
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, episodes: [suggested] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  expect(document.body.textContent).toContain("Torrent client unavailable");
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(false);
  fetching.resolve({ items: [feed], failures: [] });
  await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("1 new");
  expect(mocks.api.markTorrentsSeen).not.toHaveBeenCalled();
});

test("large show download sections retain the remaining release options", async () => {
  const batches = Array.from({ length: 8 }, (_, i) => torrent(`[Group ${i + 1}] Example Show Complete`));
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, batches });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  const section = document.querySelector('section[aria-label="Batches"]')!;
  expect(section.querySelectorAll('[title="Download .torrent"]')).toHaveLength(6);
  expect(section.textContent).not.toContain(batches[7].title);
  const showAll = [...section.querySelectorAll("button")].find((item) => /^Show all 8\b/i.test(item.textContent?.trim() ?? ""))!;
  expect(showAll).toBeDefined();
  showAll.click();
  await settle();
  expect(section.querySelectorAll('[title="Download .torrent"]')).toHaveLength(8);
  expect(section.textContent).toContain(batches[7].title);
});

test("reopening a show reuses downloads while explicit refreshes search again", async () => {
  mocks.api.findShowTorrents.mockResolvedValueOnce({ ...emptyShowTorrents, batches: [torrent("Cached batch")] })
    .mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Fresh batch")] });
  await render(Torrents);
  const header = button("Example Show").closest('[role="button"]') as HTMLElement;
  header.click();
  await settle();
  expect(document.body.textContent).toContain("Cached batch");
  header.click();
  await settle();
  header.click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain("Cached batch");
  button("Search again").click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).toContain("Fresh batch");
  button("Refresh").click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledTimes(3);
});

test.each(["opening", "marking seen"])("a download %s after refresh cannot restore obsolete cached choices", async (stage) => {
  const pending = deferred<void>();
  if (stage === "opening") vi.mocked(openUrl).mockReturnValueOnce(pending.promise);
  else mocks.api.markTorrentsSeen.mockReturnValueOnce(pending.promise);
  mocks.api.findShowTorrents.mockResolvedValueOnce({ ...emptyShowTorrents, batches: [torrent("Obsolete batch")] })
    .mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Refreshed batch")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  button("Refresh").click();
  await settle();
  expect(document.body.textContent).toContain("Refreshed batch");
  pending.resolve(undefined);
  await settle();
  const header = button("Example Show").closest('[role="button"]') as HTMLElement;
  header.click();
  await settle();
  header.click();
  await settle();
  expect(document.body.textContent).toContain("Refreshed batch");
  expect(document.body.textContent).not.toContain("Obsolete batch");
});

test("a previous account download failure does not appear in the new account", async () => {
  const opening = deferred<void>();
  vi.mocked(openUrl).mockReturnValueOnce(opening.promise);
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Previous account batch")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  mocks.api.localEntries.mockResolvedValue([]);
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  await settle();
  opening.reject(new Error("Previous account download failure"));
  await settle();
  expect(document.body.textContent).not.toContain("Previous account download failure");
  expect(mocks.api.markTorrentsSeen).not.toHaveBeenCalled();
});

test("destroying cached download choices prevents late writes to their shared cache", async () => {
  const opening = deferred<boolean>();
  const cache = new Map<string, { savedAt: number; choices: ShowTorrents }>();
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Cached batch")] });
  const props = { entry, category: "1_0", filter: "0", feedItems: [], cache, onopen: () => opening.promise };
  const first = await render(ShowTorrentChoices, props);
  await unmount(first);
  mounted = mounted.filter((item) => item !== first);
  document.body.replaceChildren();
  const cached = await render(ShowTorrentChoices, props);
  expect(mocks.api.findShowTorrents).toHaveBeenCalledOnce();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  await unmount(cached);
  mounted = mounted.filter((item) => item !== cached);
  cache.clear();
  auth.epoch++;
  opening.resolve(true);
  await settle();
  expect(cache.size).toBe(0);
});

test.each(["resolve", "reject"])("an old account seen write can %s without changing new account choices", async (outcome) => {
  const marking = deferred<void>();
  mocks.api.markTorrentsSeen.mockReturnValueOnce(marking.promise);
  mocks.api.findShowTorrents.mockResolvedValueOnce({ ...emptyShowTorrents, batches: [torrent("Previous account batch")] })
    .mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("New account batch")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  await settle();
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  if (outcome === "resolve") marking.resolve(undefined);
  else marking.reject(new Error("Previous account seen write failed"));
  await settle();
  const header = button("Example Show").closest('[role="button"]') as HTMLElement;
  header.click();
  await settle();
  header.click();
  await settle();
  expect(document.body.textContent).toContain("New account batch");
  expect(document.body.textContent).not.toContain("Previous account");
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(false);
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledOnce();
});

test("torrent history restores section filters and expanded shows", async () => {
  const instance = await render(Torrents);
  const filter = document.querySelector('[aria-label="Filter Watching shows"]') as HTMLInputElement;
  filter.value = "example";
  filter.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  const snapshot = instance.snapshot.capture();
  await unmount(instance);
  mounted = mounted.filter((item) => item !== instance);
  document.body.replaceChildren();
  mocks.navigation.length = 0;
  const restored = await render(Torrents);
  restored.snapshot.restore(snapshot);
  for (const callback of mocks.navigation) callback();
  await settle();
  expect((document.querySelector('[aria-label="Filter Watching shows"]') as HTMLInputElement).value).toBe("example");
  expect(button("Example Show").closest('[role="button"]')!.getAttribute("aria-expanded")).toBe("true");
});

test("torrent history does not restore previous account filters or expansion", async () => {
  const instance = await render(Torrents);
  const filter = document.querySelector('[aria-label="Filter Watching shows"]') as HTMLInputElement;
  filter.value = "example";
  filter.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  const snapshot = instance.snapshot.capture();
  await unmount(instance);
  mounted = mounted.filter((item) => item !== instance);
  document.body.replaceChildren();
  mocks.navigation.length = 0;
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  const restored = await render(Torrents);
  restored.snapshot.restore(snapshot);
  for (const callback of mocks.navigation) callback();
  await settle();
  expect((document.querySelector('[aria-label="Filter Watching shows"]') as HTMLInputElement).value).toBe("");
  expect(button("Example Show").closest('[role="button"]')!.getAttribute("aria-expanded")).toBe("false");
});

test("suggested download titles stay text and unsafe links never reach the opener", async () => {
  const title = '<img src="x" onerror="alert(1)">';
  mocks.api.findShowTorrents.mockResolvedValue({ ...emptyShowTorrents, batches: [{ ...torrent(title), link: "file:///tmp/example.torrent" }] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(document.body.textContent).toContain(title);
  expect(document.querySelector('img[src="x"]')).toBeNull();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click();
  await settle();
  expect(document.body.textContent).toContain('Refused to open link with scheme "file:"');
  expect(openUrl).not.toHaveBeenCalled();
  expect(mocks.api.markTorrentsSeen).not.toHaveBeenCalled();
});

test("show download filters reload choices and discard older filter results", async () => {
  const languageSearch = deferred<unknown>();
  mocks.api.findShowTorrents.mockResolvedValueOnce({ ...emptyShowTorrents, batches: [torrent("Original choices")] })
    .mockReturnValueOnce(languageSearch.promise)
    .mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Trusted choices")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(document.body.textContent).toContain("Original choices");
  (document.querySelector("#torrent-category") as HTMLButtonElement).click();
  await settle();
  button("English-translated").click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenLastCalledWith(1, "1_2", "0");
  expect(document.body.textContent).not.toContain("Original choices");
  (document.querySelector("#torrent-release") as HTMLButtonElement).click();
  await settle();
  button("Trusted only").click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenLastCalledWith(1, "1_2", "2");
  expect(document.body.textContent).toContain("Trusted choices");
  languageSearch.resolve({ ...emptyShowTorrents, batches: [torrent("Stale language choices")] });
  await settle();
  expect(document.body.textContent).not.toContain("Stale language choices");
  expect(document.body.textContent).toContain("Trusted choices");
});

test("tracking progress refreshes the next episode download choices", async () => {
  mocks.api.findShowTorrents.mockResolvedValueOnce({ ...emptyShowTorrents, episodes: [torrent("Episode one choice")] })
    .mockResolvedValue({ ...emptyShowTorrents, next_episode: 2, episodes: [torrent("Episode two choice")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(document.body.textContent).toContain("Episode one choice");
  mocks.api.localEntries.mockResolvedValue([{ ...entry, progress: 1 }]);
  event("kurisu://episode-updated", { ...entry, progress: 1 });
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).toContain("Next episode · 2");
  expect(document.body.textContent).toContain("Episode two choice");
  expect(document.body.textContent).not.toContain("Episode one choice");
});

test("failed show download searches can be retried", async () => {
  mocks.api.findShowTorrents.mockRejectedValueOnce(new Error("Nyaa could not be reached"))
    .mockResolvedValue({ ...emptyShowTorrents, batches: [torrent("Recovered batch")] });
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(document.body.textContent).toContain("Nyaa could not be reached");
  button("Search again").click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).toContain("Recovered batch");
  expect(document.body.textContent).not.toContain("Nyaa could not be reached");
});

test("show download results from an old account are discarded", async () => {
  const search = deferred<unknown>();
  mocks.api.findShowTorrents.mockReturnValue(search.promise);
  await render(Torrents);
  (button("Example Show").closest('[role="button"]') as HTMLElement).click();
  await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledOnce();
  mocks.api.localEntries.mockResolvedValue([]);
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  await settle();
  search.resolve({ ...emptyShowTorrents, batches: [torrent("Previous account batch")] });
  await settle();
  expect(document.body.textContent).not.toContain("Previous account batch");
  expect(document.body.textContent).not.toContain("Example Show");
});

test("settings controls wait for their saved values before accepting edits", async () => {
  const tracking = deferred<typeof config>();
  const windowSetting = deferred<string>();
  mocks.api.getTrackingConfig.mockReturnValue(tracking.promise);
  mocks.api.getAppSetting.mockImplementation((key: string) => key === "close_to_tray" ? windowSetting.promise : Promise.resolve("0"));
  await render(Settings);
  expect(document.querySelector('input[value="auto"]')!.matches(":disabled")).toBe(true);
  const tray = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Hide to system tray"))!.querySelector("input")!;
  expect(tray.disabled).toBe(true);
  tracking.resolve({ ...config });
  windowSetting.resolve("0");
  await settle();
  expect(document.querySelector('input[value="auto"]')!.matches(":disabled")).toBe(false);
});

test("an unrelated window preference failure does not prevent saving loaded tracking settings", async () => {
  mocks.api.getAppSetting.mockImplementation(async (key: string) => {
    if (key === "close_to_tray") throw new Error("window setting unavailable");
    return "0";
  });
  await render(Settings);
  expect(button("Save tracking").disabled).toBe(false);
});

test("settings toggles cannot race while a save is pending", async () => {
  const saving = deferred<void>();
  mocks.api.setAppSetting.mockReturnValue(saving.promise);
  await render(Settings);
  const tray = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Hide to system tray"))!.querySelector("input")!;
  tray.click();
  await settle();
  expect(tray.disabled).toBe(true);
  saving.resolve(undefined);
  await settle();
  expect(tray.disabled).toBe(false);
});

test("hardware acceleration starts off and saves an explicit opt in for the next launch", async () => {
  const userAgent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Linux");
  try {
    const saving = deferred<void>();
    mocks.api.setAppSetting.mockReturnValue(saving.promise);
    await render(Settings);
    const acceleration = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Use hardware acceleration"))!.querySelector("input")!;
    expect(acceleration.checked).toBe(false);
    acceleration.click();
    await settle();
    expect(mocks.api.setAppSetting).toHaveBeenCalledWith("hardware_acceleration", "1");
    expect(acceleration.disabled).toBe(true);
    expect(document.body.textContent).not.toContain("Saved. Quit and reopen Kurisu to apply.");
    saving.resolve(undefined);
    await settle();
    expect(acceleration.checked).toBe(true);
    expect(acceleration.disabled).toBe(false);
    expect(document.body.textContent).toContain("Saved. Quit and reopen Kurisu to apply.");
  } finally {
    userAgent.mockRestore();
  }
});

test("hardware acceleration loads the saved opt in and restores it when a save fails", async () => {
  const userAgent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Linux");
  try {
    mocks.api.getAppSetting.mockImplementation(async (key: string) => key === "hardware_acceleration" ? "1" : null);
    mocks.api.setAppSetting.mockRejectedValueOnce(new Error("disk unavailable")).mockResolvedValue(undefined);
    await render(Settings);
    const acceleration = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Use hardware acceleration"))!.querySelector("input")!;
    expect(acceleration.checked).toBe(true);
    acceleration.click();
    await settle();
    expect(acceleration.checked).toBe(true);
    expect(document.body.textContent).toContain("disk unavailable");
    expect(document.body.textContent).not.toContain("Saved. Quit and reopen Kurisu to apply.");
    acceleration.click();
    await settle();
    expect(mocks.api.setAppSetting).toHaveBeenLastCalledWith("hardware_acceleration", "0");
    expect(acceleration.checked).toBe(false);
    expect(document.body.textContent).not.toContain("disk unavailable");
  } finally {
    userAgent.mockRestore();
  }
});

test("hardware acceleration stays disabled after a read failure until Retry loads the preference", async () => {
  const userAgent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Linux");
  try {
    mocks.api.getAppSetting.mockImplementation(async (key: string) => {
      if (key === "hardware_acceleration") throw new Error("rendering preference unavailable");
      return null;
    });
    await render(Settings);
    const acceleration = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Use hardware acceleration"))!.querySelector("input")!;
    expect(acceleration.disabled).toBe(true);
    acceleration.click();
    expect(mocks.api.setAppSetting).not.toHaveBeenCalled();
    mocks.api.getAppSetting.mockResolvedValue("1");
    button("Retry").click();
    await settle();
    expect(acceleration.disabled).toBe(false);
    expect(acceleration.checked).toBe(true);
    expect(document.body.textContent).not.toContain("rendering preference unavailable");
  } finally {
    userAgent.mockRestore();
  }
});

test("starting another list action keeps the first title disabled", async () => {
  const pending = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValue(pending.promise);
  const instance = await render(Search);
  instance.snapshot.restore({ query: "", results: [media, { ...media, id: 2, title_english: "Second Show" }], submittedQuery: "", resultPage: 1, hasMore: false, searched: true, pending: null });
  await settle();
  const cards = document.querySelectorAll(".anime-card");
  button("Watching", cards[0]).click();
  await settle();
  button("Watching", cards[1]).click();
  await settle();
  expect(button("Watching", cards[0]).disabled).toBe(true);
});

test("a failed list action remains visible after starting another title", async () => {
  const first = deferred<typeof entry>();
  const second = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  const instance = await render(Search);
  instance.snapshot.restore({ query: "", results: [media, { ...media, id: 2, title_english: "Second Show" }], submittedQuery: "", resultPage: 1, hasMore: false, searched: true, pending: null });
  await settle();
  const cards = document.querySelectorAll(".anime-card");
  button("Watching", cards[0]).click();
  await settle();
  button("Watching", cards[1]).click();
  first.reject(new Error("first title failed to save"));
  await settle();
  expect(document.body.textContent).toContain("first title failed to save");
});

test("a failed toggle save restores the persisted value and permits retry", async () => {
  mocks.api.setAppSetting.mockRejectedValueOnce(new Error("disk unavailable")).mockResolvedValue(undefined);
  await render(Settings);
  const tray = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Hide to system tray"))!.querySelector("input")!;
  tray.click();
  await settle();
  expect(tray.checked).toBe(false);
  expect(tray.disabled).toBe(false);
  expect(document.body.textContent).toContain("disk unavailable");
  tray.click();
  await settle();
  expect(tray.checked).toBe(true);
  expect(document.body.textContent).not.toContain("disk unavailable");
});

test("retrying a failed preference read enables only the loaded control", async () => {
  mocks.api.getAppSetting.mockImplementation(async (key: string) => {
    if (key === "close_to_tray") throw new Error("window unavailable");
    return "0";
  });
  await render(Settings);
  const tray = [...document.querySelectorAll("label")].find((label) => label.textContent?.includes("Hide to system tray"))!.querySelector("input")!;
  expect(tray.disabled).toBe(true);
  mocks.api.getAppSetting.mockResolvedValue("1");
  button("Retry").click();
  await settle();
  expect(tray.checked).toBe(true);
  expect(tray.disabled).toBe(false);
  expect(document.body.textContent).not.toContain("window unavailable");
});

test("manual torrent search downloads mark matching feed releases seen", async () => {
  const canonical = "nyaa-show\u0001search-result";
  const release = { ...torrent("[Group] Example Show - 01"), seen_guid: canonical };
  const feed = { ...release, guid: "feed\u0001release", media_id: 1, episode: 1 };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.searchTorrents.mockResolvedValue([release]);
  await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example Show"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([release.guid, canonical, feed.guid]));
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).not.toContain("1 new");
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
  button("Search").click(); await settle();
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
});

test("torrent groups reload when queue resolution removes a show", async () => {
  mocks.api.localEntries.mockResolvedValueOnce([entry]).mockResolvedValue([]);
  await render(Torrents);
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("1 show");
  event("kurisu://pending-changed", undefined); await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("0 shows");
  expect(document.body.textContent).not.toContain("Example Show");
});

test.each(["failed download", "release description"])("manual torrent search %s does not mark the release seen", async (action) => {
  const release = { ...torrent("[Group] Example Show - 01"), details_url: "https://nyaa.si/view/123" };
  mocks.api.searchTorrents.mockResolvedValue([release]);
  await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example Show"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  if (action === "failed download") vi.mocked(openUrl).mockRejectedValueOnce(new Error("No torrent client"));
  const title = action === "failed download" ? "Download .torrent" : "Open release page";
  (document.querySelector(`[title="${title}"]`) as HTMLButtonElement).click(); await settle();
  expect(mocks.api.markTorrentsSeen).not.toHaveBeenCalled();
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(false);
});

test("torrent startup reads the list after pending-change listeners attach", async () => {
  const { listen } = await import("@tauri-apps/api/event");
  const attached = deferred<void>();
  vi.mocked(listen).mockImplementation((_event, _handler) => attached.promise.then(() => () => {}));
  await render(Torrents);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  mocks.api.localEntries.mockResolvedValue([]);
  attached.resolve(); await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(1);
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("0 shows");
});

test("torrent startup still reads the list when event registration fails", async () => {
  const { listen } = await import("@tauri-apps/api/event");
  vi.mocked(listen).mockRejectedValue(new Error("event bridge unavailable"));
  await render(Torrents);
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(1);
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("1 show");
  expect(document.body.textContent).toContain("Could not listen for list updates");
});

test("late torrent listener registrations clean up without reading after teardown", async () => {
  const { listen } = await import("@tauri-apps/api/event");
  const attached = deferred<() => void>();
  vi.mocked(listen).mockReturnValue(attached.promise);
  const instance = await render(Torrents);
  await unmount(instance); mounted = mounted.filter((item) => item !== instance);
  const stop = vi.fn(); attached.resolve(stop); await settle();
  expect(stop).toHaveBeenCalledTimes(2);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
});

test("torrent teardown cleans each registered listener while another is pending", async () => {
  const { listen } = await import("@tauri-apps/api/event");
  const second = deferred<() => void>();
  const firstStop = vi.fn();
  vi.mocked(listen).mockResolvedValueOnce(firstStop).mockReturnValueOnce(second.promise);
  const instance = await render(Torrents);
  await unmount(instance); mounted = mounted.filter((item) => item !== instance);
  expect(firstStop).toHaveBeenCalledTimes(1);
  const secondStop = vi.fn(); second.resolve(secondStop); await settle();
  expect(firstStop).toHaveBeenCalledTimes(1);
  expect(secondStop).toHaveBeenCalledTimes(1);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
});

test("search downloads clear a feed new badge through canonical identity despite different links", async () => {
  const canonical = "nyaa-show\u0001same-hash";
  const search = { ...torrent("[Group] Example Show - 01"), seen_guid: canonical };
  const feed = { ...search, guid: "feed\u0001release", link: "magnet:?xt=urn:btih:abc", media_id: 1, episode: 1 };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.searchTorrents.mockResolvedValue([search]);
  await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example Show"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).not.toContain("1 new");
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([search.guid, canonical, feed.guid]));
});

test("late show choices deduplicate and retain feed seen state through canonical identity", async () => {
  const pending = deferred<ShowTorrents>();
  const canonical = "nyaa-show\u0001same-hash";
  const suggestion = { ...torrent("[Group] Example Show - 01"), guid: canonical, seen_guid: canonical };
  const feed = { ...suggestion, guid: "feed\u0001release", link: "magnet:?xt=urn:btih:abc", media_id: 1, episode: 1 };
  mocks.api.fetchTorrents.mockResolvedValue({ items: [feed], failures: [] });
  mocks.api.findShowTorrents.mockReturnValue(pending.promise);
  await render(Torrents);
  (document.querySelector('#torrents-CURRENT [role="button"]') as HTMLElement).click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  pending.resolve({ ...emptyShowTorrents, episodes: [suggestion] }); await settle();
  expect(document.querySelectorAll('[title="Download .torrent"]')).toHaveLength(1);
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
  (document.querySelector('#torrents-CURRENT [role="button"]') as HTMLElement).click(); await settle();
  (document.querySelector('#torrents-CURRENT [role="button"]') as HTMLElement).click(); await settle();
  expect(mocks.api.findShowTorrents).toHaveBeenCalledTimes(1);
  expect(document.querySelectorAll('[title="Download .torrent"]')).toHaveLength(1);
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);

});

test("a late feed with another URL remains seen after a canonical search download", async () => {
  localStorage.setItem("kurisu.torrents.new", "1");
  const loadingFeed = deferred<{ items: unknown[]; failures: unknown[] }>();
  const canonical = "nyaa-show\u0001same-hash";
  const search = { ...torrent("[Group] Example Show - 01"), seen_guid: canonical };
  const feed = { ...search, guid: "feed\u0001late", link: "https://mirror.example/release.torrent", media_id: 1, episode: 1 };
  mocks.api.fetchTorrents.mockReturnValue(loadingFeed.promise);
  mocks.api.searchTorrents.mockResolvedValue([search]);
  await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Example Show"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  loadingFeed.resolve({ items: [feed], failures: [] }); await settle();
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).not.toContain("1 new");
  expect(document.querySelector('[aria-controls="torrents-CURRENT"]')!.textContent).toContain("0 feed releases");
  expect(mocks.api.markTorrentsSeen).toHaveBeenCalledWith(expect.arrayContaining([feed.guid, canonical]));
});

test("torrent history retains downloaded search seen state without matching feeds", async () => {
  const release = { ...torrent("[Group] Search-only release"), seen_guid: "nyaa-show\u0001history" };
  mocks.api.searchTorrents.mockResolvedValue([release]);
  const instance = await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Search-only release"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
  const snapshot = instance.snapshot.capture();
  await unmount(instance); mounted = mounted.filter((item) => item !== instance);
  document.body.replaceChildren(); mocks.navigation.length = 0;
  const restored = await render(Torrents);
  restored.snapshot.restore(snapshot); for (const callback of mocks.navigation) callback(); await settle();
  expect(mocks.api.searchTorrents).toHaveBeenCalledTimes(1);
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(true);
});

test("torrent history reloads search seen state after an account change", async () => {
  const release = { ...torrent("[Group] Search-only release"), seen_guid: "nyaa-show\u0001history" };
  mocks.api.searchTorrents.mockResolvedValue([release]);
  const instance = await render(Torrents);
  const input = document.querySelector('[aria-label="Search Nyaa"]') as HTMLInputElement;
  input.value = "Search-only release"; input.dispatchEvent(new Event("input", { bubbles: true }));
  button("Search").click(); await settle();
  (document.querySelector('[title="Download .torrent"]') as HTMLButtonElement).click(); await settle();
  const snapshot = instance.snapshot.capture();
  expect(snapshot.results[0].seen).toBe(true);
  await unmount(instance); mounted = mounted.filter((item) => item !== instance);
  document.body.replaceChildren(); mocks.navigation.length = 0;
  auth.epoch++;
  auth.user = { id: 2, name: "Other", score_format: "POINT_100" };
  const restored = await render(Torrents);
  restored.snapshot.restore(snapshot); for (const callback of mocks.navigation) callback(); await settle();
  expect(mocks.api.searchTorrents).toHaveBeenCalledTimes(2);
  expect(document.querySelector('[title="Download .torrent"]')!.closest(".cv-row")!.classList.contains("opacity-60")).toBe(false);
});
