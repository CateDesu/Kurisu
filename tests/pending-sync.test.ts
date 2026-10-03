import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";
import type { PendingChange } from "$lib/types";

const mocks = vi.hoisted(() => ({
  api: { getPendingChanges: vi.fn(), syncPendingChanges: vi.fn(), resolvePendingChange: vi.fn() },
  listeners: new Map<string, () => void>(),
  registration: null as Promise<void> | null,
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/auth.svelte", async () => import("./session.svelte"));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, callback: () => void) => {
  if (mocks.registration) await mocks.registration;
  mocks.listeners.set(name, callback);
  return () => mocks.listeners.delete(name);
}) }));
import PendingSync from "$lib/PendingSync.svelte";

let component: ReturnType<typeof mount> | undefined;
const pending: PendingChange = {media_id:1,title:"Offline show",progress:4,status:null,score:null,repeat:null,error:"Newer progress on AniList",conflict:true,missing_media:false};
beforeEach(() => {
  vi.clearAllMocks();
  mocks.listeners.clear();
  mocks.registration = null;
  auth.epoch = 0;
  auth.isLoggedIn = true;
  mocks.api.getPendingChanges.mockResolvedValue([pending]);
  mocks.api.resolvePendingChange.mockResolvedValue(undefined);
  mocks.api.syncPendingChanges.mockResolvedValue([]);
});
afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  document.body.innerHTML = "";
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
  button("Keep AniList version").click();
  await settle();
  expect(mocks.api.resolvePendingChange).toHaveBeenCalledWith(1,false);
  expect(document.body.textContent).not.toContain("Offline show");
  old.resolve([pending]);
  await settle();
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
  mocks.api.getPendingChanges.mockResolvedValue([]);
  registration.resolve();
  await settle();
  expect(document.body.textContent).not.toContain("waiting to sync");
});
