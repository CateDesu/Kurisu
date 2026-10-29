import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, settleListUpdates as settle } from "./helpers";
import type { ListEntry } from "#lib/types.ts";

const mocks = vi.hoisted(() => ({
  api: { localEntries: vi.fn(), localEntryUpdates: vi.fn(), getAiringSchedule: vi.fn() },
  listeners: new Map<string, Set<(message?: { payload: unknown }) => void>>(),
  listen: vi.fn(),
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: mocks.listen }));
import { applyListUpdate, listenForListUpdates, readListUpdate } from "#lib/list.svelte.ts";
import Calendar from "../src/routes/calendar/+page.svelte";

const events = ["kurisu://episode-updated", "kurisu://pending-changed"];
const media = { id: 1, title_english: "Synthetic Show", episodes: 12 };
const entry: ListEntry = { media_id: 1, status: "CURRENT", progress: 2, repeat: 0, media };
const item = { media, episode: 3, airing_at: Math.floor(Date.now() / 1000) };
let component: ReturnType<typeof mount> | undefined;
let stops: (() => void)[] = [];

function emit(event = events[1], payload?: unknown) {
  for (const callback of mocks.listeners.get(event) ?? []) callback({ payload });
}

function register(event: string, callback: (message?: { payload: unknown }) => void) {
  const handlers = mocks.listeners.get(event) ?? new Set();
  handlers.add(callback);
  mocks.listeners.set(event, handlers);
  return vi.fn(() => { handlers.delete(callback); });
}

function subscribe(refresh: () => void | Promise<unknown>, invalidate: () => void = vi.fn(), ready?: () => void | Promise<unknown>) {
  const onError = vi.fn();
  let revision = 0;
  const stop = listenForListUpdates(refresh, onError, ready, {
    invalidate: () => { revision++; invalidate(); },
    revision: () => revision,
  });
  stops.push(stop);
  return { stop, onError, invalidate };
}

async function render() {
  const target = document.createElement("div");
  document.body.append(target);
  component = mount(Calendar, { target });
  await settle();
  return component as { snapshot: { capture: () => any; restore: (value: any) => void } };
}

beforeEach(() => {
  vi.resetAllMocks();
  mocks.listeners.clear();
  localStorage.clear();
  auth.epoch++;
  auth.isLoggedIn = true;
  auth.user = { id: 1, name: "Synthetic", score_format: "POINT_100" };
  mocks.listen.mockImplementation(async (event: string, callback: () => void) => register(event, callback));
  mocks.api.localEntries.mockResolvedValue([entry]);
  mocks.api.localEntryUpdates.mockResolvedValue([entry]);
  mocks.api.getAiringSchedule.mockResolvedValue([item]);
});

afterEach(async () => {
  for (const stop of stops) stop();
  stops = [];
  if (component) await unmount(component);
  component = undefined;
  document.body.replaceChildren();
  vi.useRealTimers();
});

test("paired list events perform one refresh and invalidate each snapshot immediately", async () => {
  const refresh = vi.fn(async () => {});
  const { invalidate } = subscribe(refresh);
  await settle();
  refresh.mockClear();
  emit(events[0]);
  await Promise.resolve();
  emit(events[1]);
  expect(invalidate).toHaveBeenCalledTimes(2);
  expect(refresh).not.toHaveBeenCalled();
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
});

test("paired IPC events delivered in separate timer tasks perform one list read", async () => {
  vi.useFakeTimers();
  const refresh = vi.fn(async () => {});
  const { invalidate } = subscribe(refresh);
  await settle();
  refresh.mockClear();
  emit(events[0]);
  setTimeout(() => emit(events[1]), 0);
  await vi.advanceTimersByTimeAsync(0);
  expect(invalidate).toHaveBeenCalledTimes(2);
  expect(refresh).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(15);
  expect(refresh).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(refresh).toHaveBeenCalledOnce();
});

test("a newer write barrier retains queued broad changes for a current read", async () => {
  let revision = 0;
  const refresh = vi.fn(async () => {});
  stops.push(listenForListUpdates(refresh, vi.fn(), undefined, {
    invalidate: () => { revision++; },
    revision: () => revision,
  }));
  await settle();
  refresh.mockClear();
  emit();
  revision++;
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
  emit();
  await settle();
  expect(refresh).toHaveBeenCalledTimes(2);
});

test("events during a fetch produce one trailing refresh without concurrent fetches", async () => {
  const first = deferred<void>();
  const trailing = deferred<void>();
  const refresh = vi.fn().mockReturnValueOnce(first.promise).mockReturnValueOnce(trailing.promise).mockResolvedValue(undefined);
  subscribe(refresh);
  await settle();
  for (let i = 0; i < 6; i++) emit(events[i % 2]);
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
  first.resolve();
  await settle();
  expect(refresh).toHaveBeenCalledTimes(2);
  emit();
  emit();
  await settle();
  expect(refresh).toHaveBeenCalledTimes(2);
  trailing.resolve();
  await settle();
  expect(refresh).toHaveBeenCalledTimes(3);
});

test("an invalidation rejects a snapshot whose promise already settled before the event", async () => {
  const old = deferred<number>();
  let revision = 0;
  const applied: number[] = [];
  let reads = 0;
  subscribe(async () => {
    const request = revision;
    const value = await (reads++ === 0 ? old.promise : Promise.resolve(9));
    if (request === revision) applied.push(value);
  }, () => { revision++; });
  await settle();
  old.resolve(2);
  emit(events[0]);
  emit(events[1]);
  await settle();
  expect(applied).toEqual([9]);
  expect(reads).toBe(2);
});

test("cleanup cancels a queued event batch", async () => {
  const refresh = vi.fn(async () => {});
  const { stop } = subscribe(refresh);
  await settle();
  refresh.mockClear();
  emit();
  stop();
  await settle();
  expect(refresh).not.toHaveBeenCalled();
  for (const event of events) expect(mocks.listeners.get(event)?.size).toBe(0);
});

test("cleanup suppresses trailing work and errors from a failed in-flight refresh", async () => {
  const fetch = deferred<void>();
  const refresh = vi.fn(() => fetch.promise);
  const { stop, onError } = subscribe(refresh);
  await settle();
  emit();
  stop();
  fetch.reject(new Error("late failure"));
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
  expect(onError).not.toHaveBeenCalled();
});

test("a failed refresh still runs the queued newer refresh", async () => {
  const fetch = deferred<void>();
  const refresh = vi.fn().mockReturnValueOnce(fetch.promise).mockResolvedValue(undefined);
  const { onError } = subscribe(refresh);
  await settle();
  emit();
  fetch.reject(new Error("read failed"));
  await settle();
  expect(onError).toHaveBeenCalledOnce();
  expect(refresh).toHaveBeenCalledTimes(2);
});

test("readiness waits for both registrations and includes early events in its first read", async () => {
  const registration = deferred<() => void>();
  mocks.listen.mockImplementation((event: string, callback: () => void) => {
    const stop = register(event, callback);
    return event === events[0] ? Promise.resolve(stop) : registration.promise;
  });
  const refresh = vi.fn(async () => {});
  const { invalidate } = subscribe(refresh);
  await settle();
  emit(events[0]);
  await settle();
  expect(invalidate).toHaveBeenCalledOnce();
  expect(refresh).not.toHaveBeenCalled();
  registration.resolve(() => {});
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
});

test("slow custom readiness does not delay batched list events", async () => {
  const initial = deferred<void>();
  const refresh = vi.fn(async () => {});
  const ready = vi.fn(() => initial.promise);
  subscribe(refresh, vi.fn(), ready);
  await settle();
  emit(events[0]);
  setTimeout(() => emit(events[1]), 0);
  await settle();
  expect(ready).toHaveBeenCalledOnce();
  expect(refresh).toHaveBeenCalledOnce();
  initial.resolve();
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
});

test("cleanup suppresses errors from a custom readiness request", async () => {
  const initial = deferred<void>();
  const { stop, onError } = subscribe(vi.fn(), vi.fn(), () => initial.promise);
  await settle();
  stop();
  initial.reject(new Error("late startup failure"));
  await settle();
  expect(onError).not.toHaveBeenCalled();
});

test("late registrations are released without readiness reads after cleanup", async () => {
  const registration = deferred<() => void>();
  mocks.listen.mockReturnValue(registration.promise);
  const refresh = vi.fn(async () => {});
  const { stop } = subscribe(refresh);
  stop();
  const unlisten = vi.fn();
  registration.resolve(unlisten);
  await settle();
  expect(unlisten).toHaveBeenCalledTimes(2);
  expect(refresh).not.toHaveBeenCalled();
});

test("one failed registration preserves readiness and the other event listener", async () => {
  mocks.listen.mockImplementation(async (event: string, callback: () => void) => {
    if (event === events[0]) throw new Error("registration failed");
    return register(event, callback);
  });
  const refresh = vi.fn(async () => {});
  const ready = vi.fn();
  const { onError } = subscribe(refresh, vi.fn(), ready);
  await settle();
  expect(onError).toHaveBeenCalledOnce();
  expect(ready).toHaveBeenCalledOnce();
  emit();
  emit();
  await settle();
  expect(refresh).toHaveBeenCalledOnce();
});

test("legacy revision callbacks still run synchronously before pending snapshots settle", async () => {
  let revision = 0;
  const refresh = vi.fn(() => { revision++; });
  stops.push(listenForListUpdates(refresh, vi.fn(), () => {}));
  await settle();
  const snapshotRevision = revision;
  const applied = vi.fn();
  const snapshot = Promise.resolve().then(() => { if (snapshotRevision === revision) applied(); });
  emit();
  expect(revision).toBe(snapshotRevision + 1);
  await snapshot;
  expect(applied).not.toHaveBeenCalled();
});

test("Calendar retains its row node through a paired background refresh", async () => {
  await render();
  const row = document.querySelector(".cv-row")!;
  const fetch = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(fetch.promise);
  mocks.api.localEntries.mockClear();
  emit(events[0]);
  emit(events[1]);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledOnce();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(document.body.textContent).not.toContain("Loading your list");
  fetch.resolve([{ ...entry, progress: 3 }]);
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(row.textContent).toContain("you: 3/3");
});

test("Calendar keeps successful rows on a background read failure and retries in place", async () => {
  await render();
  const row = document.querySelector(".cv-row")!;
  mocks.api.localEntries.mockRejectedValueOnce(new Error("temporary read failure"));
  emit();
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(document.body.textContent).toContain("Could not read your list");
  const retry = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(retry.promise);
  button("Retry list").click();
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  retry.resolve([]);
  await settle();
  expect(document.querySelector(".cv-row")).toBeNull();
  expect(document.body.textContent).toContain("None of your Watching shows air this week.");
  expect(document.body.textContent).not.toContain("Could not read your list");
});

test("Calendar distinguishes initial list loading and failure from a successful empty list", async () => {
  const fetch = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(fetch.promise);
  await render();
  expect(document.body.textContent).toContain("Loading your list");
  expect(document.body.textContent).not.toContain("None of your Watching");
  fetch.reject(new Error("initial failure"));
  await settle();
  expect(document.body.textContent).toContain("initial failure");
  expect(document.body.textContent).not.toContain("Loading your list");
  expect(document.body.textContent).not.toContain("None of your Watching");
  mocks.api.localEntries.mockResolvedValueOnce([]);
  button("Retry list").click();
  await settle();
  expect(document.body.textContent).toContain("None of your Watching shows air this week.");
});

test("Calendar invalidates a resolved old list before the trailing refresh can start", async () => {
  await render();
  const row = document.querySelector(".cv-row")!;
  const old = deferred<ListEntry[]>();
  const current = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise).mockReturnValueOnce(current.promise);
  emit();
  await settle();
  old.resolve([]);
  emit();
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  current.resolve([{ ...entry, progress: 4 }]);
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(row.textContent).toContain("you: 4/3");
});

test("Calendar snapshot restoration invalidates an older list read", async () => {
  const calendar = await render();
  const snapshot = calendar.snapshot.capture();
  const row = document.querySelector(".cv-row")!;
  const old = deferred<ListEntry[]>();
  const restored = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise).mockReturnValueOnce(restored.promise).mockResolvedValue([{ ...entry, progress: 5 }]);
  emit();
  await settle();
  calendar.snapshot.restore(snapshot);
  old.resolve([]);
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  restored.resolve([{ ...entry, progress: 5 }]);
  await settle();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(row.textContent).toContain("you: 5/3");
});

test("Calendar clears the previous account and ignores its late background list response", async () => {
  await render();
  const old = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise);
  emit();
  await settle();
  const next = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(next.promise);
  auth.epoch++;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  expect(document.querySelector(".cv-row")).toBeNull();
  expect(document.body.textContent).toContain("Loading your list");
  old.resolve([entry]);
  await settle();
  expect(document.querySelector(".cv-row")).toBeNull();
  next.resolve([]);
  await settle();
  expect(document.body.textContent).toContain("None of your Watching shows air this week.");
});

test("Calendar cancels queued work and clears rows on logout", async () => {
  await render();
  mocks.api.localEntries.mockClear();
  emit();
  auth.epoch++;
  auth.isLoggedIn = false;
  auth.user = null;
  await settle();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(document.querySelector(".cv-row")).toBeNull();
  for (const event of events) expect(mocks.listeners.get(event)?.size).toBe(0);
});

test("Calendar reads one changed entry for paired notifications delivered in separate tasks", async () => {
  vi.useFakeTimers();
  await render();
  const row = document.querySelector(".cv-row");
  mocks.api.localEntries.mockClear();
  mocks.api.localEntryUpdates.mockResolvedValue([{ ...entry, progress: 4 }]);
  emit(events[0], { ...entry, progress: 11 });
  setTimeout(() => emit(events[1], { media_ids: [1] }), 0);
  await vi.advanceTimersByTimeAsync(0);
  expect(mocks.api.localEntryUpdates).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(16);
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1], 1);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(document.querySelector(".cv-row")).toBe(row);
  expect(row?.textContent).toContain("you: 4/3");
  expect(row?.textContent).not.toContain("you: 11/3");
});

test("Calendar merges changed IDs and removals while preserving an unrelated row", async () => {
  const second = { ...entry, media_id: 2, media: { ...media, id: 2, title_english: "Second Show" } };
  const third = { ...entry, media_id: 3, media: { ...media, id: 3, title_english: "Third Show" } };
  mocks.api.localEntries.mockResolvedValue([entry, second, third]);
  mocks.api.getAiringSchedule.mockResolvedValue([item, { ...item, media: second.media }, { ...item, media: third.media }]);
  await render();
  const thirdRow = [...document.querySelectorAll(".cv-row")].find((row) => row.textContent?.includes("Third Show"));
  mocks.api.localEntries.mockClear();
  mocks.api.localEntryUpdates.mockResolvedValue([{ ...entry, progress: 6 }, null]);
  emit(events[0], entry);
  emit(events[1], { media_ids: [2, 1, 2] });
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1, 2], 1);
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(document.querySelectorAll(".cv-row")).toHaveLength(2);
  expect(document.body.textContent).toContain("you: 6/3");
  expect(document.body.textContent).not.toContain("Second Show");
  expect([...document.querySelectorAll(".cv-row")]).toContain(thirdRow);
});

test.each([false, true])("a broad change dominates specific changes in either arrival order %s", async (broadFirst) => {
  const refresh = vi.fn(async (_ids?: readonly number[]) => {});
  subscribe(refresh);
  await settle();
  refresh.mockClear();
  emit(broadFirst ? events[1] : events[0], broadFirst ? undefined : entry);
  emit(broadFirst ? events[0] : events[1], broadFirst ? entry : undefined);
  await settle();
  expect(refresh).toHaveBeenCalledExactlyOnceWith(undefined);
});

test.each([{}, { media_ids: [] }, { media_ids: [1, "2"] }, { media_ids: [0] }])("unknown pending scope falls back to a full refresh %j", async (payload) => {
  const refresh = vi.fn(async (_ids?: readonly number[]) => {});
  subscribe(refresh);
  await settle();
  refresh.mockClear();
  emit(events[1], payload);
  await settle();
  expect(refresh).toHaveBeenCalledExactlyOnceWith(undefined);
});

test("an invalidated targeted read retains its IDs in the one trailing batch", async () => {
  const second = { ...entry, media_id: 2, media: { ...media, id: 2, title_english: "Second Show" } };
  mocks.api.localEntries.mockResolvedValue([entry, second]);
  mocks.api.getAiringSchedule.mockResolvedValue([item, { ...item, media: second.media }]);
  await render();
  const old = deferred<(ListEntry | null)[]>();
  mocks.api.localEntryUpdates.mockReturnValueOnce(old.promise).mockResolvedValueOnce([{ ...entry, progress: 4 }, null]);
  emit(events[0], entry);
  await settle();
  emit(events[1], { media_ids: [2] });
  await settle();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledTimes(1);
  old.resolve([{ ...entry, progress: 8 }]);
  await settle();
  expect(mocks.api.localEntryUpdates.mock.calls).toEqual([[[1], 1], [[1, 2], 1]]);
  expect(document.body.textContent).toContain("you: 4/3");
  expect(document.body.textContent).not.toContain("you: 8/3");
  expect(document.body.textContent).not.toContain("Second Show");
});

test("a delayed targeted response from the old account cannot replace a new snapshot", async () => {
  await render();
  const old = deferred<(ListEntry | null)[]>();
  mocks.api.localEntryUpdates.mockReturnValueOnce(old.promise);
  emit(events[1], { media_ids: [1] });
  await settle();
  mocks.api.localEntries.mockResolvedValue([]);
  auth.epoch++;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  old.resolve([{ ...entry, progress: 11 }]);
  await settle();
  expect(document.querySelector(".cv-row")).toBeNull();
});

test("disposing during a targeted read suppresses trailing reads and errors", async () => {
  const old = deferred<void>();
  const refresh = vi.fn().mockResolvedValueOnce(undefined).mockReturnValueOnce(old.promise);
  const { stop, onError } = subscribe(refresh);
  await settle();
  emit(events[0], entry);
  await settle();
  emit(events[1], { media_ids: [2] });
  stop();
  old.reject(new Error("disposed read"));
  await settle();
  expect(refresh).toHaveBeenCalledTimes(2);
  expect(onError).not.toHaveBeenCalled();
});

test("the list patch preserves unrelated entry references and appends an added ID", async () => {
  const second = { ...entry, media_id: 2 };
  mocks.api.localEntryUpdates.mockResolvedValue([null, { ...entry, media_id: 3 }]);
  const update = await readListUpdate([1, 3]);
  const patched = applyListUpdate([entry, second], update);
  expect(patched.map((value) => value.media_id)).toEqual([2, 3]);
  expect(patched[0]).toBe(second);
});

test("offline startup without a known account uses a full snapshot", async () => {
  auth.user = null;
  expect(applyListUpdate([], await readListUpdate([1]))).toEqual([entry]);
  expect(mocks.api.localEntryUpdates).not.toHaveBeenCalled();
});

test("the configurable My List delay preserves synchronous invalidation and scoped batching", async () => {
  vi.useFakeTimers();
  let revision = 0;
  const refresh = vi.fn(async (_ids?: readonly number[]) => {});
  stops.push(listenForListUpdates(refresh, vi.fn(), undefined, {
    invalidate: () => { revision++; }, revision: () => revision, delayMs: 300,
  }));
  await settle();
  refresh.mockClear();
  emit(events[0], entry);
  await vi.advanceTimersByTimeAsync(100);
  emit(events[1], { media_ids: [2] });
  expect(revision).toBe(2);
  await vi.advanceTimersByTimeAsync(299);
  expect(refresh).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  expect(refresh).toHaveBeenCalledExactlyOnceWith([1, 2]);
});

test("a newer accepted write cannot drop a queued change to another ID", async () => {
  let revision = 0;
  const refresh = vi.fn(async (_ids?: readonly number[]) => {});
  stops.push(listenForListUpdates(refresh, vi.fn(), undefined, {
    invalidate: () => { revision++; }, revision: () => revision,
  }));
  await settle();
  refresh.mockClear();
  emit(events[1], { media_ids: [2] });
  revision++;
  await settle();
  expect(refresh).toHaveBeenCalledExactlyOnceWith([2]);
});

test("an accepted write during a targeted read retries its affected IDs", async () => {
  let revision = 0;
  let rows = [entry, { ...entry, media_id: 2 }];
  const old = deferred<(ListEntry | null)[]>();
  const refresh = vi.fn(async (ids?: readonly number[]) => {
    const request = ++revision;
    const update = await readListUpdate(ids);
    if (request === revision) rows = applyListUpdate(rows, update);
  });
  stops.push(listenForListUpdates(refresh, vi.fn(), undefined, {
    invalidate: () => { revision++; }, revision: () => revision,
  }));
  mocks.api.localEntries.mockResolvedValue(rows);
  await settle();
  mocks.api.localEntryUpdates.mockReturnValueOnce(old.promise).mockResolvedValueOnce([{ ...entry, media_id: 2, progress: 7 }]);
  emit(events[1], { media_ids: [2] });
  await settle();
  revision++;
  rows = [{ ...entry, progress: 5 }, rows[1]];
  old.resolve([{ ...entry, media_id: 2, progress: 4 }]);
  await settle();
  expect(mocks.api.localEntryUpdates.mock.calls).toEqual([[[2], 1], [[2], 1]]);
  expect(rows.map((row) => [row.media_id, row.progress])).toEqual([[1, 5], [2, 7]]);
});

test("an accepted write during a broad read retains unknown changes for the trailing snapshot", async () => {
  let revision = 0;
  let rows = [entry, { ...entry, media_id: 2 }];
  const refresh = async (ids?: readonly number[]) => {
    const request = ++revision;
    const update = await readListUpdate(ids);
    if (request === revision) rows = applyListUpdate(rows, update);
  };
  stops.push(listenForListUpdates(refresh, vi.fn(), undefined, {
    invalidate: () => { revision++; }, revision: () => revision,
  }));
  mocks.api.localEntries.mockResolvedValue(rows);
  await settle();
  const old = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise).mockResolvedValue([{ ...entry, progress: 5 }, { ...entry, media_id: 2, progress: 7 }]);
  mocks.api.localEntries.mockClear();
  emit();
  await settle();
  revision++;
  rows = [{ ...entry, progress: 5 }, rows[1]];
  old.resolve([entry, { ...entry, media_id: 2 }]);
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(2);
  expect(mocks.api.localEntryUpdates).not.toHaveBeenCalled();
  expect(rows.map((row) => [row.media_id, row.progress])).toEqual([[1, 5], [2, 7]]);
});
