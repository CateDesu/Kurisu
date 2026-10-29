import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";
import type { PendingChange } from "#lib/types.ts";

const mocks = vi.hoisted(() => ({
  api: { getPendingChanges: vi.fn(), syncPendingChanges: vi.fn(), resolvePendingChange: vi.fn() },
  listeners: new Map<string, () => void>(),
  registration: null as Promise<void> | null,
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, callback: () => void) => {
  if (mocks.registration) await mocks.registration;
  mocks.listeners.set(name, callback);
  return () => mocks.listeners.delete(name);
}) }));
import PendingSync from "#lib/PendingSync.svelte";

let component: ReturnType<typeof mount> | undefined;
const pending: PendingChange = {media_id:1,title:"Offline show",progress:4,status:null,score:null,repeat:null,error:"Newer progress on AniList",conflict:true,missing_media:false};
beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  mocks.listeners.clear();
  mocks.registration = null;
  auth.epoch = 0;
  auth.isLoggedIn = true;
  mocks.api.getPendingChanges.mockReset().mockResolvedValue([pending]);
  mocks.api.resolvePendingChange.mockReset().mockResolvedValue(undefined);
  mocks.api.syncPendingChanges.mockReset().mockResolvedValue([]);
});
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  document.body.innerHTML = "";
  vi.restoreAllMocks();
  vi.useRealTimers();
});

test("conflicts wait for an explicit choice and resolved changes cannot return from a stale refresh", async () => {
  component = mount(PendingSync, {target:document.body});
  await settle();
  expect(mocks.api.resolvePendingChange).not.toHaveBeenCalled();
  button("1 saved change waiting to sync").click();
  await settle();
  const old = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  mocks.listeners.get("kurisu://pending-changed")?.();
  await settle();
  button("Keep AniList version").click();
  await settle();
  expect(mocks.api.resolvePendingChange).toHaveBeenCalledWith(1,false);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(button("Syncing…").disabled).toBe(true);
  old.resolve([pending]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(3);
  expect(document.body.textContent).not.toContain("Offline show");
});

test("a pending queue from the previous account never appears after switching accounts", async () => {
  const old = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  component = mount(PendingSync, {target:document.body});
  await settle();
  auth.epoch++;
  await settle();
  old.resolve([pending]);
  await settle();
  expect(document.body.textContent).not.toContain("Offline show");
  expect(document.body.textContent).not.toContain("waiting to sync");
});


test("removed media keeps its pending evidence with a working discard action", async () => {
  mocks.api.getPendingChanges.mockResolvedValue([{...pending,missing_media:true}]);
  component = mount(PendingSync, {target:document.body});
  await settle();
  button("1 saved change waiting to sync").click();
  await settle();
  expect(document.body.textContent).toContain("Offline show");
  expect(document.body.textContent).not.toContain("Send saved change");
  mocks.api.getPendingChanges.mockResolvedValue([]);
  button("Discard saved change").click();
  await settle();
  expect(mocks.api.resolvePendingChange).toHaveBeenCalledWith(1,false);
  expect(document.body.textContent).not.toContain("Offline show");
});

test("a stale post-sync status failure cannot replace a newer successful status", async () => {
  component = mount(PendingSync, {target:document.body});
  await settle();
  const old = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  button("Retry sync").click();
  await settle();
  mocks.listeners.get("kurisu://pending-changed")?.();
  await settle();
  old.reject(new Error("obsolete status read failed"));
  await settle();
  expect(document.body.textContent).not.toContain("obsolete status read failed");
  expect(document.body.textContent).not.toContain("Sync status unavailable");
});

test("a current post-sync status failure remains visible for retry", async () => {
  component = mount(PendingSync, {target:document.body});
  await settle();
  mocks.api.getPendingChanges.mockRejectedValueOnce(new Error("current status read failed"));
  button("Retry sync").click();
  await settle();
  expect(document.body.textContent).toContain("current status read failed");
  expect(button("Retry sync").disabled).toBe(false);
});

test("pending status catches queue changes during initial listener registration", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  component = mount(PendingSync, {target:document.body});
  await settle();
  expect(mocks.api.getPendingChanges).not.toHaveBeenCalled();
  mocks.api.getPendingChanges.mockResolvedValue([]);
  registration.resolve();
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  expect(document.body.textContent).not.toContain("waiting to sync");
});

function emitChange() {
  mocks.listeners.get("kurisu://pending-changed")?.();
}

async function render() {
  component = mount(PendingSync, { target: document.body });
  await settle();
}

test("bursts share one read and inflight changes require only one trailing read", async () => {
  await render();
  button("1 saved change waiting to sync").click();
  await settle();
  const old = deferred<PendingChange[]>();
  const latest = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockClear();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockReturnValueOnce(latest.promise);
  for (let i = 0; i < 20; i++) emitChange();
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  for (let i = 0; i < 20; i++) emitChange();
  await vi.advanceTimersByTimeAsync(90_000);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  old.resolve([{ ...pending, title: "Obsolete queue" }]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).not.toContain("Obsolete queue");
  expect(document.body.textContent).toContain("Offline show");
  latest.resolve([{ ...pending, title: "Current queue" }]);
  await settle();
  expect(document.body.textContent).toContain("Current queue");
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
});

test("periodic ticks join a slow read without invalidating it or scheduling a trailing read", async () => {
  const initial = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(initial.promise);
  await render();
  await vi.advanceTimersByTimeAsync(90_000);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  initial.resolve([pending]);
  await settle();
  expect(document.body.textContent).toContain("1 saved change waiting to sync");
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
});

test("action events and completion share an authoritative read after the action settles", async () => {
  await render();
  const action = deferred<PendingChange[]>();
  const status = deferred<PendingChange[]>();
  mocks.api.syncPendingChanges.mockReturnValueOnce(action.promise);
  mocks.api.getPendingChanges.mockClear();
  mocks.api.getPendingChanges.mockReturnValueOnce(status.promise);
  button("Retry sync").click();
  for (let i = 0; i < 20; i++) emitChange();
  await vi.advanceTimersByTimeAsync(90_000);
  expect(mocks.api.getPendingChanges).not.toHaveBeenCalled();
  action.resolve([pending]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  expect(button("Syncing…").disabled).toBe(true);
  window.dispatchEvent(new Event("online"));
  expect(mocks.api.syncPendingChanges).toHaveBeenCalledOnce();
  status.resolve([]);
  await settle();
  expect(document.body.textContent).not.toContain("waiting to sync");
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
});

test.each(["response", "failure"])("starting an action immediately invalidates an older status %s", async (result) => {
  await render();
  button("1 saved change waiting to sync").click();
  await settle();
  const old = deferred<PendingChange[]>();
  const action = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  mocks.api.syncPendingChanges.mockReturnValueOnce(action.promise);
  emitChange();
  await settle();
  button("Retry sync").click();
  if (result === "response") old.resolve([{ ...pending, title: "Obsolete status" }]);
  else old.reject(new Error("Obsolete status"));
  await settle();
  expect(document.body.textContent).not.toContain("Obsolete status");
  expect(document.body.textContent).toContain("Offline show");
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(button("Syncing…").disabled).toBe(true);
  action.resolve([]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(3);
  expect(document.body.textContent).not.toContain("waiting to sync");
});

test.each(["Retry sync", "Keep AniList version", "Send saved change"])("%s errors survive successful status recovery and allow another action", async (label) => {
  await render();
  button("1 saved change waiting to sync").click();
  await settle();
  const action = label === "Retry sync" ? mocks.api.syncPendingChanges : mocks.api.resolvePendingChange;
  action.mockImplementationOnce(async () => {
    emitChange();
    throw new Error("Action failed");
  });
  mocks.api.getPendingChanges.mockClear();
  button(label).click();
  await settle();
  expect(document.body.textContent).toContain("Action failed");
  expect(button("Retry sync").disabled).toBe(false);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  button(label).click();
  await settle();
  expect(action).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).not.toContain("Action failed");
});

test("hidden periodic reads stop while events, online retry and manual actions still work", async () => {
  await render();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange"));
  mocks.api.getPendingChanges.mockClear();
  await vi.advanceTimersByTimeAsync(300_000);
  expect(mocks.api.getPendingChanges).not.toHaveBeenCalled();
  emitChange();
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  window.dispatchEvent(new Event("online"));
  await settle();
  expect(mocks.api.syncPendingChanges).toHaveBeenCalledOnce();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  button("1 saved change waiting to sync").click();
  await settle();
  button("Keep AniList version").click();
  await settle();
  expect(mocks.api.resolvePendingChange).toHaveBeenCalledExactlyOnceWith(1, false);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(3);
  visibility.mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(4);
});

test("visibility return invalidates a hidden read and joins its trailing refresh", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  const old = deferred<PendingChange[]>();
  const latest = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockReturnValueOnce(latest.promise);
  await render();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledOnce();
  visibility.mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  emitChange();
  old.resolve([pending]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).not.toContain("waiting to sync");
  latest.resolve([]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
});

test("visible empty queues still recover missed events through periodic reads", async () => {
  mocks.api.getPendingChanges.mockResolvedValueOnce([]);
  await render();
  expect(document.body.textContent).not.toContain("waiting to sync");
  await vi.advanceTimersByTimeAsync(30_000);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).toContain("1 saved change waiting to sync");
});

test("listener failure still loads status and recovers a failed read with visible polling", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  mocks.api.getPendingChanges.mockRejectedValueOnce(new Error("Initial read failed"));
  await render();
  registration.reject(new Error("Listener failed"));
  await settle();
  expect(document.body.textContent).toContain("Initial read failed");
  expect(mocks.listeners.size).toBe(0);
  await vi.advanceTimersByTimeAsync(30_000);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(document.body.textContent).toContain("1 saved change waiting to sync");
  expect(document.body.textContent).not.toContain("Initial read failed");
});

test("account changes serialize status reads and ignore old action errors", async () => {
  await render();
  const old = deferred<PendingChange[]>();
  const action = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise).mockResolvedValue([]);
  mocks.api.syncPendingChanges.mockReturnValueOnce(action.promise);
  emitChange();
  await settle();
  button("Retry sync").click();
  auth.epoch++;
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  old.resolve([pending]);
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(3);
  action.reject(new Error("Previous account failed"));
  await settle();
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(3);
  expect(document.body.textContent).not.toContain("waiting to sync");
  expect(document.body.textContent).not.toContain("Previous account failed");
});

test.each(["logout", "unmount"])("%s cancels trailing reads and removes live listeners and timers", async (exit) => {
  await render();
  const old = deferred<PendingChange[]>();
  mocks.api.getPendingChanges.mockReturnValueOnce(old.promise);
  emitChange();
  await settle();
  emitChange();
  if (exit === "logout") auth.isLoggedIn = false;
  else {
    await unmount(component!);
    component = undefined;
  }
  await settle();
  expect(mocks.listeners.size).toBe(0);
  old.reject(new Error("Departed read failed"));
  window.dispatchEvent(new Event("online"));
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.advanceTimersByTimeAsync(90_000);
  await settle();
  expect(vi.getTimerCount()).toBe(0);
  expect(mocks.api.getPendingChanges).toHaveBeenCalledTimes(2);
  expect(mocks.api.syncPendingChanges).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain("Departed read failed");
});

test("late listener registration is disposed after unmount without reading status", async () => {
  const registration = deferred<void>();
  mocks.registration = registration.promise;
  await render();
  await unmount(component!);
  component = undefined;
  registration.resolve();
  await settle();
  expect(mocks.listeners.size).toBe(0);
  expect(mocks.api.getPendingChanges).not.toHaveBeenCalled();
  expect(vi.getTimerCount()).toBe(0);
});
