import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount, type Component } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries([
    "getEntry", "setProgress", "updateEntry", "getMedia", "getMediaDetail", "getRecommendations",
    "localEntries", "getAiringSchedule", "getTrackingConfig", "getAppSetting", "setTrackingConfig",
    "getLibraryBinding", "bindLibraryPath", "getRssFeeds", "fetchTorrents", "searchTorrents", "setAppSetting",
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

const media = { id: 1, title_english: "Example Show", episodes: 12 };
const entry = { media_id: 1, status: "CURRENT", progress: 0, repeat: 0, score: 75, media };
const config = { mode: "off", prompt_seconds: 120, auto_percent: 80, auto_ask: true, mpv_ipc_socket: "", discord_enabled: true };
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

test("starting another list action keeps the first title disabled", async () => {
  const pending = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValue(pending.promise);
  const instance = await render(Search);
  instance.snapshot.restore({ query: "", results: [media, { ...media, id: 2, title_english: "Second Show" }], pendingQuery: null });
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
  instance.snapshot.restore({ query: "", results: [media, { ...media, id: 2, title_english: "Second Show" }], pendingQuery: null });
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
