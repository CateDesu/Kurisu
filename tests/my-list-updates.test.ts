import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { nowMs, runClock } from "#lib/now.svelte.ts";
import type { ListEntry } from "#lib/types.ts";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: { localEntries: vi.fn(), localEntryUpdates: vi.fn(), syncMyList: vi.fn(), setProgress: vi.fn() },
  entries: [] as ListEntry[],
  listeners: new Map<string, Set<(event: { payload: unknown }) => void>>(),
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, callback: (event: { payload: unknown }) => void) => {
    const handlers = mocks.listeners.get(event) ?? new Set();
    handlers.add(callback);
    mocks.listeners.set(event, handlers);
    return () => handlers.delete(callback);
  }),
}));

import Collection from "../src/routes/+page.svelte";

const copy = <T,>(value: T): T => structuredClone(value);
const makeEntry = (id: number, title: string, progress = 3): ListEntry => ({
  id: id + 100, media_id: id, status: "CURRENT", progress, repeat: 0,
  media: { id, title_english: title, episodes: 12, format: "TV" },
});
let component: ReturnType<typeof mount> | undefined;
let stopClock: () => void;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(Date.UTC(2026, 9, 27, 12));
  stopClock = runClock();
  vi.advanceTimersByTime(30_000);
  vi.resetAllMocks();
  mocks.listeners.clear();
  localStorage.clear();
  auth.epoch++;
  auth.isLoggedIn = true;
  auth.offline = true;
  auth.user = { id: 77, name: "Synthetic", score_format: "POINT_100" };
  mocks.entries = [makeEntry(1, "Alpha"), makeEntry(2, "Beta", 11)];
  mocks.api.localEntries.mockImplementation(async () => copy(mocks.entries));
  mocks.api.syncMyList.mockImplementation(async () => copy(mocks.entries));
  mocks.api.localEntryUpdates.mockImplementation(async (ids: number[]) =>
    ids.map((id) => copy(mocks.entries.find((entry) => entry.media_id === id) ?? null))
  );
  mocks.api.setProgress.mockImplementation(async (id: number, progress: number) => {
    const entry = mocks.entries.find((item) => item.media_id === id)!;
    Object.assign(entry, { progress, status: progress === 12 ? "COMPLETED" : entry.status });
    return copy(entry);
  });
});

afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  stopClock();
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

async function render() {
  const target = document.createElement("div");
  document.body.append(target);
  component = mount(Collection, { target });
  await settle();
}

function emit(event: string, payload?: unknown) {
  for (const callback of mocks.listeners.get(event) ?? []) callback({ payload });
}

function row(title: string) {
  return [...document.querySelectorAll<HTMLElement>(".collection-row")]
    .find((item) => item.querySelector(".row-title")?.textContent === title);
}

test("fresh automatic minutes and focus do not read the full local list", async () => {
  auth.offline = false;
  await render();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  await vi.advanceTimersByTimeAsync(240_000);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  expect(row("Alpha")).toBeDefined();
});

test.each(["startup", "manual"])("a %s sync resolving hidden records wall time and keeps its full freshness window", async (source) => {
  auth.offline = false;
  const sync = deferred<ListEntry[]>();
  if (source === "startup") mocks.api.syncMyList.mockReturnValueOnce(sync.promise);
  await render();
  if (source === "manual") {
    mocks.api.syncMyList.mockReturnValueOnce(sync.promise);
    button("Sync").click();
    await settle();
  }
  const visibleAt = nowMs();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.advanceTimersByTimeAsync(360_000);
  expect(nowMs()).toBe(visibleAt);
  sync.resolve(copy(mocks.entries));
  await settle();
  expect(Number(localStorage.getItem("kurisu.list.synced"))).toBe(Date.now());
  expect(Number(localStorage.getItem("kurisu.list.synced"))).toBeGreaterThan(visibleAt);
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  visibility.mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  window.dispatchEvent(new Event("focus"));
  await vi.advanceTimersByTimeAsync(299_999);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
});

test("hidden native tracking events still refresh My List while display time stays paused", async () => {
  await render();
  const visibleAt = nowMs();
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange"));
  mocks.entries[0].progress = 5;
  emit("kurisu://episode-updated", copy(mocks.entries[0]));
  await vi.advanceTimersByTimeAsync(30_000);
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1], 77);
  expect(row("Alpha")?.textContent).toContain("5/12");
  expect(nowMs()).toBe(visibleAt);
});

test("the exact five-minute boundary recovers missed entries and metadata", async () => {
  auth.offline = false;
  await render();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  mocks.entries[0] = { ...mocks.entries[0], progress: 5, media: { id: 1, title_english: "Renamed Alpha", format: "MOVIE", episodes: 24 } };
  mocks.entries.push(makeEntry(3, "Gamma"));
  await vi.advanceTimersByTimeAsync(299_999);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  expect(row("Alpha")).toBeDefined();
  await vi.advanceTimersByTimeAsync(1);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(row("Renamed Alpha")?.textContent).toContain("5/24");
  expect(row("Renamed Alpha")?.textContent).toContain("MOVIE");
  expect(row("Gamma")).toBeDefined();
});

test("a failed due sync retries after one minute without treating the attempt as a fresh sync", async () => {
  auth.offline = false;
  await render();
  const syncedAt = localStorage.getItem("kurisu.list.synced");
  mocks.api.syncMyList.mockRejectedValueOnce(new Error("temporary sync failure"));
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  await vi.advanceTimersByTimeAsync(300_000);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain("temporary sync failure");
  expect(localStorage.getItem("kurisu.list.synced")).toBe(syncedAt);
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  for (let index = 0; index < 5; index++) window.dispatchEvent(new Event("focus"));
  await vi.advanceTimersByTimeAsync(59_999);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(Number(localStorage.getItem("kurisu.list.synced"))).toBe(Date.now());
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  await vi.advanceTimersByTimeAsync(60_000);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
});

test("failed startup syncs also back off repeated focus until the retry boundary", async () => {
  auth.offline = false;
  mocks.api.syncMyList.mockRejectedValueOnce(new Error("startup sync failure"));
  await render();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  window.dispatchEvent(new Event("focus"));
  await vi.advanceTimersByTimeAsync(59_999);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
});

test.each(["hidden", "offline"])("%s automatic refreshes wait for a visible online focus", async (state) => {
  auth.offline = false;
  await render();
  const visibility = vi.spyOn(document, "visibilityState", "get");
  if (state === "hidden") visibility.mockReturnValue("hidden");
  else auth.offline = true;
  await settle();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  await vi.advanceTimersByTimeAsync(360_000);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  visibility.mockReturnValue("visible");
  auth.offline = false;
  await settle();
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
});

test("loading and syncing suppress automatic work and a remount joins the pending sync", async () => {
  auth.offline = false;
  const initial = deferred<ListEntry[]>();
  const sync = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(initial.promise);
  mocks.api.syncMyList.mockReturnValueOnce(sync.promise);
  await render();
  window.dispatchEvent(new Event("focus"));
  await vi.advanceTimersByTimeAsync(60_000);
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  initial.resolve(copy(mocks.entries));
  await settle();
  window.dispatchEvent(new Event("focus"));
  await vi.advanceTimersByTimeAsync(60_000);
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  await unmount(component!);
  component = undefined;
  document.body.replaceChildren();
  await render();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  sync.resolve(copy(mocks.entries));
  await settle();
  expect(row("Alpha")).toBeDefined();
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
});

test.each([false, true])("epoch changes ignore freshness and old pending syncs, account switch %s", async (switchAccount) => {
  auth.offline = false;
  const old = deferred<ListEntry[]>();
  mocks.api.syncMyList.mockReturnValueOnce(old.promise);
  await render();
  mocks.entries = [makeEntry(3, "New session")];
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  auth.epoch++;
  if (switchAccount) auth.user = { ...auth.user!, id: 88 };
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(row("New session")).toBeDefined();
  old.resolve([makeEntry(1, "Old session")]);
  await settle();
  expect(row("Old session")).toBeUndefined();
  expect(row("New session")).toBeDefined();
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
});

test("fresh scoped events, full recovery events and manual sync retain their reads", async () => {
  auth.offline = false;
  await render();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  mocks.entries[0].progress = 5;
  emit("kurisu://pending-changed", { media_ids: [1] });
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1], 77);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(row("Alpha")?.textContent).toContain("5/12");
  mocks.entries.push(makeEntry(3, "Recovered"));
  emit("kurisu://pending-changed");
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(row("Recovered")).toBeDefined();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  mocks.entries.push(makeEntry(4, "Manual sync"));
  button("Sync").click();
  await settle();
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(row("Manual sync")).toBeDefined();
});

test("a fresh remount still loads its baseline and retries a failed local read", async () => {
  auth.offline = false;
  await render();
  await unmount(component!);
  component = undefined;
  document.body.replaceChildren();
  mocks.api.localEntries.mockRejectedValueOnce(new Error("temporary local failure"));
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  await render();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain("temporary local failure");
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(2);
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  expect(row("Alpha")).toBeDefined();
});

test.each(["full", "scoped"])("a failed %s recovery read retries on focus despite a fresh sync", async (scope) => {
  auth.offline = false;
  await render();
  mocks.api.localEntries.mockClear();
  mocks.api.syncMyList.mockClear();
  mocks.entries[0].progress = 5;
  const read = scope === "full" ? mocks.api.localEntries : mocks.api.localEntryUpdates;
  read.mockRejectedValueOnce(new Error("temporary recovery failure"));
  emit("kurisu://pending-changed", scope === "scoped" ? { media_ids: [1] } : undefined);
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(read).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain("temporary recovery failure");
  expect(row("Alpha")?.textContent).toContain("3/12");
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(scope === "full" ? 2 : 1);
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  expect(row("Alpha")?.textContent).toContain("5/12");
  expect(document.body.textContent).not.toContain("temporary recovery failure");
});

test("an offline baseline keeps automatic refresh disabled and explicit full recovery available", async () => {
  await render();
  mocks.api.localEntries.mockClear();
  mocks.entries.push(makeEntry(3, "Offline recovery"));
  await vi.advanceTimersByTimeAsync(360_000);
  window.dispatchEvent(new Event("focus"));
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  emit("kurisu://pending-changed");
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(mocks.api.syncMyList).not.toHaveBeenCalled();
  expect(row("Offline recovery")).toBeDefined();
});

test("paired scoped events read one changed entry after the My List debounce", async () => {
  await render();
  const beta = row("Beta");
  mocks.entries[0].progress = 4;
  emit("kurisu://episode-updated", copy(mocks.entries[0]));
  setTimeout(() => emit("kurisu://pending-changed", { media_ids: [1] }), 0);
  await vi.advanceTimersByTimeAsync(299);
  expect(mocks.api.localEntryUpdates).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1], 77);
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(row("Alpha")?.textContent).toContain("4/12");
  expect(row("Beta")).toBe(beta);
});

test("a scoped refresh handles removal, addition and completion while retaining other controls", async () => {
  await render();
  const beta = row("Beta");
  mocks.entries = [mocks.entries[1], makeEntry(3, "Gamma")];
  emit("kurisu://pending-changed", { media_ids: [1, 3] });
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(row("Alpha")).toBeUndefined();
  expect(row("Gamma")).toBeDefined();
  expect(row("Beta")).toBe(beta);
  mocks.entries[1] = { ...mocks.entries[1], status: "COMPLETED", progress: 12 };
  emit("kurisu://episode-updated", copy(mocks.entries[1]));
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(row("Gamma")).toBeUndefined();
  button("Completed 1").click();
  await settle();
  expect(row("Gamma")?.textContent).toContain("12/12");
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
});

test.each([false, true])("a broad event keeps the full refresh when mixed with a scoped event %s", async (broadFirst) => {
  await render();
  mocks.entries.push(makeEntry(3, "Gamma"));
  const scoped = () => emit("kurisu://episode-updated", copy(mocks.entries[0]));
  const broad = () => emit("kurisu://pending-changed");
  (broadFirst ? broad : scoped)();
  (broadFirst ? scoped : broad)();
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(mocks.api.localEntryUpdates).not.toHaveBeenCalled();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(2);
  expect(row("Gamma")).toBeDefined();
});

test("a newer event retains every invalidated entry in the trailing scoped refresh", async () => {
  await render();
  const old = deferred<(ListEntry | null)[]>();
  mocks.api.localEntryUpdates.mockReturnValueOnce(old.promise);
  emit("kurisu://episode-updated", copy(mocks.entries[0]));
  await vi.advanceTimersByTimeAsync(300);
  mocks.entries[0].progress = 4;
  mocks.entries[1].progress = 5;
  emit("kurisu://pending-changed", { media_ids: [2] });
  old.resolve([makeEntry(1, "Alpha", 1)]);
  await settle();
  expect(row("Alpha")?.textContent).toContain("3/12");
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(new Set(mocks.api.localEntryUpdates.mock.calls[1][0])).toEqual(new Set([1, 2]));
  expect(row("Alpha")?.textContent).toContain("4/12");
  expect(row("Beta")?.textContent).toContain("5/12");
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
});

test("an accepted edit to another entry cannot discard a queued scoped change", async () => {
  await render();
  mocks.entries[0].progress = 4;
  emit("kurisu://episode-updated", copy(mocks.entries[0]));
  row("Beta")!.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(2, 12, 11);
  expect(row("Beta")).toBeUndefined();
  await vi.advanceTimersByTimeAsync(300);
  await settle();
  expect(row("Alpha")?.textContent).toContain("4/12");
  expect(row("Beta")).toBeUndefined();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1], 77);
});
