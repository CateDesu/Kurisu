import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount, type Component } from "svelte";
import { openUrl } from "@tauri-apps/plugin-opener";
import { emit } from "@tauri-apps/api/event";
import { auth } from "./session.svelte";
import { button, deferred, selectOption, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries(["localEntries", "searchAnimePage", "getNotificationsPage", "markNotificationsRead", "getMediaDetail", "getEntry", "getRecommendations", "getUserStats", "getEntryDetails", "updateEntryDetails", "updateEntry", "setProgress"].map((name) => [name, vi.fn()])),
  navigation: [] as (() => void)[],
  listeners: new Map<string, Set<() => void>>(),
  registration: null as Promise<void> | null,
  nowPlaying: null as null | Record<string, unknown>,
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/auth.svelte", async () => import("./session.svelte"));
vi.mock("$lib/library.svelte", () => ({ library: { fileFor: () => undefined, ensureScan: async () => {} } }));
vi.mock("$lib/nowplaying.svelte", () => ({ nowPlaying: () => mocks.nowPlaying }));
vi.mock("$app/navigation", () => ({ goto: vi.fn(), afterNavigate: (callback: () => void) => mocks.navigation.push(callback) }));
vi.mock("$app/stores", async () => {
  const { writable } = await import("svelte/store");
  return { page: writable({ params: { id: "1" }, url: new URL("http://localhost/anime/1") }) };
});
vi.mock("@tauri-apps/api/event", () => ({ emit: vi.fn(), listen: vi.fn(async (event: string, callback: () => void) => {
  if (mocks.registration) await mocks.registration;
  const handlers = mocks.listeners.get(event) ?? new Set();
  handlers.add(callback);
  mocks.listeners.set(event, handlers);
  return () => handlers.delete(callback);
}) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn(), openUrl: vi.fn() }));
import Search from "../src/routes/search/+page.svelte";
import Notifications from "../src/routes/notifications/+page.svelte";
import Detail from "../src/routes/anime/[id]/+page.svelte";
import Stats from "../src/routes/stats/+page.svelte";
import EditEntry from "$lib/EditEntry.svelte";
import Now from "../src/routes/now/+page.svelte";
import { notificationUrl } from "$lib/types";

const media = { id: 1, title_english: "First Show", episodes: 12 };
const entry = { media_id: 1, status: "CURRENT", progress: 3, repeat: 0, score: 75, media };
const details = { notes: "Old note", started_at: { year: 2024, month: null, day: null }, completed_at: null, custom_lists: ["Favorites"], available_custom_lists: ["Favorites", "With friends"] };
let mounted: ReturnType<typeof mount>[] = [];
async function render(component: Component<any>, props?: any) {
  const target = document.createElement("div"); document.body.append(target);
  const instance = mount(component, { target, props }); mounted.push(instance);
  await settle(); for (const callback of mocks.navigation) callback(); await settle();
  return instance;
}
function input(selector: string, value: string) {
  const element = document.querySelector(selector) as HTMLInputElement;
  element.value = value; element.dispatchEvent(new Event("input", { bubbles: true }));
}
beforeEach(() => {
  vi.resetAllMocks(); mocks.navigation.length = 0; mocks.listeners.clear(); mocks.registration = null;
  mocks.nowPlaying = null;
  auth.epoch = 0; auth.isLoggedIn = true; auth.offline = false;
  auth.user = { id: 1, name: "Example", score_format: "POINT_100" };
  mocks.api.localEntries.mockResolvedValue([]);
  mocks.api.getEntry.mockResolvedValue(entry);
  mocks.api.getMediaDetail.mockResolvedValue({ media, relations: [], characters: [], staff: [] });
  mocks.api.getRecommendations.mockResolvedValue([]);
  mocks.api.getEntryDetails.mockResolvedValue(details);
});
afterEach(async () => { for (const instance of mounted) await unmount(instance); mounted = []; document.body.replaceChildren(); });

test("search appends further pages, retains results on failure, and can retry", async () => {
  mocks.api.searchAnimePage.mockResolvedValueOnce({ items: [media], page: 1, has_next_page: true })
    .mockRejectedValueOnce("temporary failure")
    .mockResolvedValueOnce({ items: [media, { ...media, id: 2, title_english: "Second Show" }], page: 2, has_next_page: false });
  await render(Search); input('[aria-label="Anime title"]', "show"); button("Search").click(); await settle();
  button("Load more anime").click(); await settle();
  expect(document.body.textContent).toContain("First Show"); expect(document.body.textContent).toContain("temporary failure");
  button("Load more anime").click(); await settle();
  expect(mocks.api.searchAnimePage).toHaveBeenLastCalledWith("show", 2);
  expect(document.body.textContent).toContain("Second Show");
  expect([...document.querySelectorAll('.anime-card')].filter((card) => card.textContent?.includes('First Show'))).toHaveLength(1);
  expect(document.body.textContent).not.toContain("Load more anime");
});

test("successful empty search explains how to recover", async () => {
  mocks.api.searchAnimePage.mockResolvedValue({ items: [], page: 1, has_next_page: false });
  await render(Search); input('[aria-label="Anime title"]', "missing title"); button("Search").click(); await settle();
  expect(document.body.textContent).toContain("No anime found for “missing title”");
});

test("notifications retain older pages and mark unread only on an explicit click", async () => {
  const comment = { id: 1, kind: "THREAD_COMMENT_REPLY", thread_id: 7, comment_id: 8, comment_url: "https://anilist.co/forum/thread/7/comment/8", user_name: "Friend" };
  mocks.api.getNotificationsPage.mockResolvedValueOnce({ items: [comment], page: 1, has_next_page: true, unread_count: 2 })
    .mockResolvedValueOnce({ items: [comment, { id: 2, kind: "FOLLOWING", user_name: "Other" }], page: 2, has_next_page: false, unread_count: 2 });
  mocks.api.markNotificationsRead.mockResolvedValue(undefined);
  await render(Notifications);
  expect(mocks.api.markNotificationsRead).not.toHaveBeenCalled();
  (document.querySelector("button.cv-row") as HTMLButtonElement).click(); await settle();
  expect(openUrl).toHaveBeenCalledWith(comment.comment_url);
  button("Load older notifications").click(); await settle();
  expect(document.querySelectorAll("button.cv-row")).toHaveLength(2);
  button("Mark all read").click(); await settle();
  expect(mocks.api.markNotificationsRead).toHaveBeenCalledTimes(1);
  expect(document.body.textContent).toContain("0 unread on AniList");
  expect(notificationUrl({ ...comment, comment_url: "https://anilist.co.evil.test/comment" })).toBe("https://anilist.co/forum/thread/7");
});

test("detail entry failure preserves anime content and prevents a misleading add", async () => {
  mocks.api.getEntry.mockRejectedValueOnce("entry offline").mockResolvedValueOnce(null);
  mocks.api.getRecommendations.mockRejectedValueOnce("recommendations offline").mockResolvedValueOnce([]);
  await render(Detail);
  expect(document.body.textContent).toContain("First Show");
  expect(document.body.textContent).toContain("Your list entry is unavailable");
  expect(document.body.textContent).not.toContain("Add to list:");
  button("Retry list entry").click(); await settle();
  expect(document.body.textContent).toContain("Add to list:");
  button("Retry recommendations").click(); await settle();
  expect(document.body.textContent).toContain("No recommendations yet.");
});

test.each([null, 10])("detail completion uses its current total instead of cached total %s", async (episodes) => {
  mocks.api.getEntry.mockResolvedValue({ ...entry, media: { ...media, episodes } });
  await render(Detail);
  button("Edit").click();
  await settle();
  selectOption("#ed-status", "COMPLETED");
  await settle();
  expect((document.querySelector("#ed-progress") as HTMLInputElement).value).toBe("12");
  expect((document.querySelector("#ed-progress") as HTMLInputElement).max).toBe("12");
});

test("saved detail metadata cannot cap a newer list entry total", async () => {
  mocks.api.getMediaDetail.mockResolvedValue({ media: { ...media, episodes: 10 }, relations: [], characters: [], staff: [], warning: "offline" });
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 10 });
  await render(Detail);
  expect((document.querySelector('[aria-label="One more episode"]') as HTMLButtonElement).disabled).toBe(false);
  button("Edit").click();
  await settle();
  selectOption("#ed-status", "COMPLETED");
  await settle();
  expect((document.querySelector("#ed-progress") as HTMLInputElement).value).toBe("12");
});

test("entry refresh retains an editor draft and the current detailed total", async () => {
  mocks.api.getEntry.mockResolvedValue({ ...entry, media: { ...media, episodes: 10 } });
  await render(Detail);
  button("Edit").click();
  await settle();
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  const draft = document.querySelector("#ed-notes");
  input("#ed-notes", "Keep my unsaved note");
  input("#ed-progress", "7");
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 5, media: { ...media, episodes: 10 } });
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.querySelector("#ed-notes")).toBe(draft);
  expect((draft as HTMLTextAreaElement).value).toBe("Keep my unsaved note");
  expect((document.querySelector("#ed-progress") as HTMLInputElement).value).toBe("7");
  expect((document.querySelector("#ed-progress") as HTMLInputElement).max).toBe("12");
});

test("stats unavailable state never invents zero totals", async () => {
  mocks.api.getUserStats.mockRejectedValue("Statistics unavailable");
  await render(Stats);
  expect(document.body.textContent).toContain("Statistics are unavailable");
  expect(document.body.textContent).not.toContain("Days watched");
});

test("entry notes save only changed metadata and keep partial dates intact", async () => {
  const onclose = vi.fn(); mocks.api.updateEntryDetails.mockResolvedValue({ ...details, notes: "New note" });
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  button("Notes, viewing dates and custom lists ▾").click(); await settle();
  expect((document.querySelector('[aria-label="Started year"]') as HTMLInputElement).value).toBe("2024");
  input("#ed-notes", "New note"); button("Save").click(); await settle();
  expect(mocks.api.updateEntryDetails).toHaveBeenCalledWith(1, "New note", null, null, null);
  expect(mocks.api.updateEntry).not.toHaveBeenCalled(); expect(onclose).toHaveBeenCalledTimes(1);
});

test("failed metadata save keeps draft and blocks closing while in flight", async () => {
  const onclose = vi.fn(); const write = deferred<typeof details>(); mocks.api.updateEntryDetails.mockReturnValue(write.promise);
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  button("Notes, viewing dates and custom lists ▾").click(); await settle();
  input("#ed-notes", "Keep this note"); button("Save").click(); await settle();
  expect(button("Cancel").disabled).toBe(true); write.reject("offline"); await settle();
  expect(onclose).not.toHaveBeenCalled();
  expect((document.querySelector("#ed-notes") as HTMLTextAreaElement).value).toBe("Keep this note");
  expect(document.body.textContent).toContain("offline");
});

test("detail drops a discarded queue entry when AniList has removed it", async () => {
  await render(Detail);
  expect(document.body.textContent).toContain("3/12");
  mocks.api.getEntry.mockResolvedValue(null);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.body.textContent).toContain("Add to list:");
  expect(document.body.textContent).not.toContain("3/12");
});

test("currently watching drops a discarded queue entry from playback and the current list", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 4, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  mocks.api.localEntries.mockResolvedValue([entry]);
  await render(Now);
  expect(document.body.textContent).toContain("Ep 3/12 on your list");
  expect(document.body.textContent).toContain("Continue watching");
  mocks.api.localEntries.mockResolvedValue([]);
  mocks.api.getEntry.mockResolvedValue(null);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.body.textContent).not.toContain("Ep 3/12 on your list");
  expect(document.body.textContent).not.toContain("Continue watching");
  expect(document.body.textContent).not.toContain("Update to Ep 4");
});

test("currently watching catches changes during initial list listener registration", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  mocks.api.localEntries.mockResolvedValue([entry]);
  await render(Now);
  mocks.api.localEntries.mockResolvedValue([]);
  registration.resolve();
  await settle();
  expect(document.body.textContent).not.toContain("Continue watching");
});

test("currently watching discards old account queue refresh responses", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 8, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  mocks.api.localEntries.mockResolvedValue([entry]);
  await render(Now);
  const oldEntry = deferred<typeof entry | null>();
  const oldList = deferred<unknown[]>();
  mocks.api.getEntry.mockReturnValueOnce(oldEntry.promise).mockResolvedValue({ ...entry, progress: 7 });
  mocks.api.localEntries.mockReturnValueOnce(oldList.promise).mockResolvedValue([]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  auth.epoch++;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  await settle();
  oldEntry.resolve(entry);
  oldList.resolve([entry]);
  await settle();
  expect(document.body.textContent).toContain("Ep 7/12 on your list");
  expect(document.body.textContent).not.toContain("Ep 3/12 on your list");
  expect(document.body.textContent).not.toContain("Continue watching");
});

test("currently watching releases delayed listeners without reading after teardown", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  const instance = await render(Now);
  await unmount(instance);
  mounted = mounted.filter((item) => item !== instance);
  registration.resolve();
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.getEntry).not.toHaveBeenCalled();
  expect([...mocks.listeners.values()].every((handlers) => handlers.size === 0)).toBe(true);
});

test("currently watching still loads when listener registration fails", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  mocks.api.localEntries.mockResolvedValue([entry]);
  await render(Now);
  registration.reject(new Error("event bridge unavailable"));
  await settle();
  expect(document.body.textContent).toContain("Continue watching");
  expect(document.body.textContent).toContain("First Show");
});

test("a queue refresh cannot overwrite newly saved currently watching progress", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 4, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  await render(Now);
  const write = deferred<typeof entry>();
  mocks.api.setProgress.mockReturnValue(write.promise);
  button("Update to Ep 4").click();
  await settle();
  const old = deferred<typeof entry>();
  mocks.api.getEntry.mockReturnValueOnce(old.promise);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  write.resolve({ ...entry, progress: 4 });
  await settle();
  expect(document.body.textContent).toContain("Ep 4/12 on your list");
  old.resolve(entry);
  await settle();
  expect(document.body.textContent).toContain("Ep 4/12 on your list");
  expect(document.body.textContent).not.toContain("Update to Ep 4");
  mocks.api.getEntry.mockResolvedValue({ ...entry, progress: 5 });
  for (const callback of mocks.listeners.get("kurisu://episode-updated") ?? []) callback();
  await settle();
  expect(document.body.textContent).toContain("Ep 5/12 on your list");
});

test("currently watching retains accepted progress when frontend event delivery fails", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 4, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  vi.mocked(emit).mockRejectedValue(new Error("event bridge unavailable"));
  mocks.api.setProgress.mockResolvedValue({ ...entry, progress: 4 });
  await render(Now);
  button("Update to Ep 4").click();
  await settle();
  expect(document.body.textContent).toContain("Ep 4/12 on your list");
  expect(document.body.textContent).not.toContain("event bridge unavailable");
});

test("currently watching reloads authoritative progress after a failed write", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 4, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  await render(Now);
  const write = deferred<typeof entry>();
  mocks.api.setProgress.mockReturnValue(write.promise);
  button("Update to Ep 4").click();
  await settle();
  const old = deferred<typeof entry>();
  mocks.api.getEntry.mockReturnValueOnce(old.promise).mockResolvedValue({ ...entry, progress: 2 });
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  write.reject(new Error("save rejected"));
  await settle();
  old.resolve(entry);
  await settle();
  expect(document.body.textContent).toContain("Ep 2/12 on your list");
  expect(document.body.textContent).toContain("save rejected");
  expect(button("Update to Ep 4").disabled).toBe(false);
});

test("currently watching keeps a confirmed removal when an older entry read settles", async () => {
  mocks.nowPlaying = { active: true, media_id: 1, matched: "First Show", episode: 4, player: "mpv", length_us: 100_000_000, position_us: 10_000_000 };
  await render(Now);
  const fresh = deferred<typeof entry | null>();
  const old = deferred<typeof entry>();
  mocks.api.getEntry.mockReturnValueOnce(fresh.promise).mockReturnValueOnce(old.promise);
  button("Update to Ep 4").click();
  await settle();
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  fresh.resolve(null);
  await settle();
  old.resolve(entry);
  await settle();
  expect(document.body.textContent).toContain("This show is no longer on your list");
  expect(document.body.textContent).not.toContain("Ep 3/12 on your list");
  expect(document.body.textContent).not.toContain("Update to Ep 4");
  expect(mocks.api.setProgress).not.toHaveBeenCalled();
});

test("search updates list badges after resolving a queued change", async () => {
  mocks.api.localEntries.mockResolvedValue([entry]);
  mocks.api.searchAnimePage.mockResolvedValue({ items: [media], page: 1, has_next_page: false });
  await render(Search);
  input('[aria-label="Anime title"]', "show"); button("Search").click(); await settle();
  expect(document.body.textContent).toContain("Watching · 3/12");
  mocks.api.localEntries.mockResolvedValue([]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  expect(document.body.textContent).not.toContain("Watching · 3/12");
});

test("late detail queue reload cannot restore an entry removed by a newer reload", async () => {
  await render(Detail);
  const old = deferred<typeof entry | null>();
  mocks.api.getEntry.mockReturnValueOnce(old.promise).mockResolvedValue(null);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  old.resolve(entry);
  await settle();
  expect(document.body.textContent).toContain("Add to list:");
  expect(document.body.textContent).not.toContain("3/12");
});

test("a search list refresh from the old account cannot replace the new account badges", async () => {
  mocks.api.localEntries.mockResolvedValue([entry]);
  mocks.api.searchAnimePage.mockResolvedValue({ items: [media], page: 1, has_next_page: false });
  await render(Search);
  input('[aria-label="Anime title"]', "show"); button("Search").click(); await settle();
  const old = deferred<typeof entry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise).mockResolvedValue([{ ...entry, status: "PLANNING", progress: 0 }]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await settle();
  auth.epoch++;
  await settle();
  old.resolve([entry]);
  await settle();
  expect(document.body.textContent).not.toContain("Watching · 3/12");
  expect(document.querySelector(".anime-card")?.textContent).toContain("Plan to Watch");
});

test("search catches list changes during delayed listener registration", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  mocks.api.localEntries.mockResolvedValue([entry]);
  mocks.api.searchAnimePage.mockResolvedValue({ items: [media], page: 1, has_next_page: false });
  await render(Search);
  input('[aria-label="Anime title"]', "show"); button("Search").click(); await settle();
  mocks.api.localEntries.mockResolvedValue([]);
  registration.resolve();
  await settle();
  expect(document.body.textContent).not.toContain("Watching · 3/12");
});

test("detail catches list changes during delayed listener registration", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  await render(Detail);
  mocks.api.getEntry.mockResolvedValue(null);
  registration.resolve();
  await settle();
  expect(document.body.textContent).toContain("Add to list:");
  expect(document.body.textContent).not.toContain("3/12");
});

test("search still reads the initial list if listener registration fails", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  await render(Search);
  registration.reject(new Error("listener unavailable"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain("listener unavailable");
});

test("late initial listener registration cannot read a departed account", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  await render(Search);
  auth.epoch++;
  await settle();
  registration.resolve();
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.listeners.get("kurisu://pending-changed")?.size).toBe(1);
  expect(mocks.listeners.get("kurisu://episode-updated")?.size).toBe(1);
});

test("retrying a partial editor save preserves server updates to untouched metadata", async () => {
  const onclose = vi.fn();
  mocks.api.updateEntryDetails.mockResolvedValue({ ...details, notes: "New note", started_at: { year: 2025, month: null, day: null } });
  mocks.api.updateEntry.mockRejectedValueOnce(new Error("progress save failed")).mockResolvedValue({ ...entry, progress: 4 });
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  button("Notes, viewing dates and custom lists ▾").click(); await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click(); await settle();
  expect(document.body.textContent).toContain("progress save failed");
  expect((document.querySelector('[aria-label="Started year"]') as HTMLInputElement).value).toBe("2025");
  button("Save").click(); await settle();
  expect(mocks.api.updateEntryDetails).toHaveBeenCalledOnce();
  expect(mocks.api.updateEntry).toHaveBeenCalledTimes(2);
  expect(onclose).toHaveBeenCalledOnce();
});

test("a new metadata edit after a partial save is sent against the refreshed baseline", async () => {
  const onclose = vi.fn();
  mocks.api.updateEntryDetails.mockResolvedValue({ ...details, notes: "New note", started_at: { year: 2025, month: null, day: null } });
  mocks.api.updateEntry.mockRejectedValueOnce(new Error("progress save failed")).mockResolvedValue({ ...entry, progress: 4 });
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  button("Notes, viewing dates and custom lists ▾").click(); await settle();
  input("#ed-notes", "New note"); input("#ed-progress", "4");
  button("Save").click(); await settle();
  input('[aria-label="Started year"]', "2026");
  button("Save").click(); await settle();
  expect(mocks.api.updateEntryDetails).toHaveBeenLastCalledWith(1, null, { year: 2026, month: null, day: null }, null, null);
  expect(onclose).toHaveBeenCalledOnce();
});

test("an account change between metadata and progress saves prevents the second write", async () => {
  const saved = deferred<typeof details>();
  const onclose = vi.fn();
  mocks.api.updateEntryDetails.mockReturnValue(saved.promise);
  await render(EditEntry, { entry, scoreFormat: "POINT_100", onclose });
  button("Notes, viewing dates and custom lists ▾").click(); await settle();
  input("#ed-notes", "New note"); input("#ed-progress", "4");
  button("Save").click(); await settle();
  auth.epoch++;
  saved.resolve({ ...details, notes: "New note" });
  await settle();
  expect(mocks.api.updateEntry).not.toHaveBeenCalled();
  expect(onclose).not.toHaveBeenCalled();
});

test("notification pagination and mark-read operations cannot overlap", async () => {
  const older = deferred<unknown>();
  const marking = deferred<void>();
  mocks.api.getNotificationsPage.mockResolvedValueOnce({ items: [{ id: 1, kind: "FOLLOWING" }], page: 1, has_next_page: true, unread_count: 2 }).mockReturnValueOnce(older.promise);
  mocks.api.markNotificationsRead.mockReturnValue(marking.promise);
  await render(Notifications);
  button("Load older notifications").click(); await settle();
  expect(button("Mark all read").disabled).toBe(true);
  button("Mark all read").click(); await settle();
  expect(mocks.api.markNotificationsRead).not.toHaveBeenCalled();
  older.resolve({ items: [{ id: 2, kind: "FOLLOWING" }], page: 2, has_next_page: true, unread_count: 3 });
  await settle();
  button("Mark all read").click(); await settle();
  expect(button("Load older notifications").disabled).toBe(true);
  expect(button("↻ Refresh").disabled).toBe(true);
  button("Load older notifications").click(); button("↻ Refresh").click(); await settle();
  expect(mocks.api.getNotificationsPage).toHaveBeenCalledTimes(2);
  marking.resolve(); await settle();
  expect(document.body.textContent).toContain("0 unread on AniList");
});

test("a previous account mark-read response cannot reset the new unread count", async () => {
  const marking = deferred<void>();
  mocks.api.getNotificationsPage.mockResolvedValueOnce({ items: [], page: 1, has_next_page: false, unread_count: 2 }).mockResolvedValue({ items: [], page: 1, has_next_page: false, unread_count: 8 });
  mocks.api.markNotificationsRead.mockReturnValue(marking.promise);
  await render(Notifications);
  button("Mark all read").click(); await settle();
  auth.epoch++; await settle();
  marking.resolve(); await settle();
  expect(document.body.textContent).toContain("8 unread on AniList");
});

test("a previous account notification page cannot replace the new unread count", async () => {
  const older = deferred<unknown>();
  mocks.api.getNotificationsPage.mockResolvedValueOnce({ items: [], page: 1, has_next_page: true, unread_count: 2 }).mockReturnValueOnce(older.promise).mockResolvedValue({ items: [], page: 1, has_next_page: false, unread_count: 8 });
  await render(Notifications);
  button("Load older notifications").click(); await settle();
  auth.epoch++; await settle();
  older.resolve({ items: [{ id: 1, kind: "FOLLOWING", user_name: "Old account friend" }], page: 2, has_next_page: false, unread_count: 2 });
  await settle();
  expect(document.body.textContent).toContain("8 unread on AniList");
  expect(document.body.textContent).not.toContain("Old account friend");
});
