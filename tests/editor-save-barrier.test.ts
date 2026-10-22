import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries(["getRecommendations", "getEntryDetails", "updateEntryDetails", "updateEntry", "deleteEntry", "installUpdate", "currentUser", "isLoggedIn", "logout", "loginWithToken"].map((name) => [name, vi.fn()])),
  invoke: vi.fn(),
  goto: vi.fn(),
  listeners: new Map<string, (event?: { payload: unknown }) => void | Promise<void>>(),
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/library.svelte.ts", () => ({ library: { fileFor: () => undefined, reset: vi.fn() } }));
vi.mock("$app/navigation", () => ({ goto: mocks.goto }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, callback: (event?: { payload: unknown }) => void | Promise<void>) => {
  mocks.listeners.set(name, callback);
  return () => mocks.listeners.delete(name);
}) }));

import EditEntry from "#lib/EditEntry.svelte";
import Shutdown from "#lib/Shutdown.svelte";
import Tracking from "#lib/Tracking.svelte";
import { auth } from "#lib/auth.svelte.ts";
import { runInstallUpdate } from "#lib/update.svelte.ts";

const entry = { media_id: 1, status: "CURRENT", progress: 3, score: 75, repeat: 0, media: { id: 1, title_english: "Example Show", episodes: 12 } };
const details = { notes: "Old note", started_at: null, completed_at: null, custom_lists: [], available_custom_lists: [] };
let instances: ReturnType<typeof mount>[] = [];

beforeEach(async () => {
  vi.resetAllMocks();
  mocks.listeners.clear();
  mocks.api.getRecommendations.mockResolvedValue([]);
  mocks.api.getEntryDetails.mockResolvedValue(details);
  mocks.api.updateEntry.mockResolvedValue({ ...entry, progress: 4 });
  mocks.api.deleteEntry.mockResolvedValue(undefined);
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Example" });
  mocks.api.logout.mockResolvedValue(undefined);
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "Other" });
  await auth.refresh();
  instances.push(mount(EditEntry, { target: document.body, props: { entry, onclose: vi.fn(), scoreFormat: "POINT_100" } }));
  instances.push(mount(Shutdown, { target: document.body }));
  await settle();
});

afterEach(async () => {
  for (const instance of instances) await unmount(instance);
  instances = [];
  document.body.replaceChildren();
});

function input(selector: string, value: string) {
  const control = document.querySelector<HTMLInputElement>(selector)!;
  control.value = value;
  control.dispatchEvent(new Event("input", { bubbles: true }));
}

function requestShutdown() {
  return mocks.listeners.get("kurisu://shutdown-requested")!();
}

test("native Quit waits for an editor save after the editor is removed", async () => {
  const write = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValue(write.promise);
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  const closing = requestShutdown();
  await settle();
  await unmount(instances.shift()!);
  const approvedEarly = mocks.invoke.mock.calls.some(([command]) => command === "finish_shutdown");
  write.resolve({ ...entry, progress: 4 });
  await closing;
  expect(approvedEarly).toBe(false);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("native Quit waits for metadata and the following progress save", async () => {
  const metadata = deferred<typeof details>();
  const progress = deferred<typeof entry>();
  mocks.api.updateEntryDetails.mockReturnValue(metadata.promise);
  mocks.api.updateEntry.mockReturnValue(progress.promise);
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  expect(mocks.api.updateEntry).not.toHaveBeenCalled();
  const closing = requestShutdown();
  await settle();
  const approvedBeforeMetadata = mocks.invoke.mock.calls.some(([command]) => command === "finish_shutdown");
  metadata.resolve({ ...details, notes: "New note" });
  await settle();
  const approvedBeforeProgress = mocks.invoke.mock.calls.some(([command]) => command === "finish_shutdown");
  expect(mocks.api.updateEntry).toHaveBeenCalledExactlyOnceWith(1, null, 4, null, null);
  progress.resolve({ ...entry, progress: 4 });
  await closing;
  expect(approvedBeforeMetadata).toBe(false);
  expect(approvedBeforeProgress).toBe(false);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("a failed editor save keeps native Quit open and an explicit retry can finish", async () => {
  const write = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValueOnce(write.promise);
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  const closing = requestShutdown();
  await settle();
  write.reject(new Error("Disk is full"));
  await closing;
  await settle();
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: false });
  expect(document.querySelector<HTMLInputElement>("#ed-progress")!.value).toBe("4");
  expect(document.body.textContent).toContain("Disk is full");
  button("Save").click();
  await settle();
  await requestShutdown();
  expect(mocks.api.updateEntry).toHaveBeenCalledTimes(2);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("native Quit waits for a confirmed editor removal", async () => {
  const removal = deferred<void>();
  mocks.api.deleteEntry.mockReturnValue(removal.promise);
  button("Remove from list").click();
  await settle();
  button("Confirm remove").click();
  await settle();
  expect(mocks.api.deleteEntry).toHaveBeenCalledExactlyOnceWith(1);
  const closing = requestShutdown();
  await settle();
  const approvedEarly = mocks.invoke.mock.calls.some(([command]) => command === "finish_shutdown");
  removal.resolve();
  await closing;
  expect(approvedEarly).toBe(false);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("failed metadata keeps native Quit open before the basic edit is sent", async () => {
  const metadata = deferred<typeof details>();
  mocks.api.updateEntryDetails.mockReturnValue(metadata.promise);
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  const closing = requestShutdown();
  await settle();
  metadata.reject(new Error("AniList is unavailable"));
  await closing;
  await settle();
  expect(mocks.api.updateEntry).not.toHaveBeenCalled();
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: false });
  expect(document.querySelector<HTMLTextAreaElement>("#ed-notes")!.value).toBe("New note");
  expect(document.querySelector<HTMLInputElement>("#ed-progress")!.value).toBe("4");
});

test("a failed removal keeps native Quit open and allows an explicit retry", async () => {
  const removal = deferred<void>();
  mocks.api.deleteEntry.mockReturnValueOnce(removal.promise);
  button("Remove from list").click();
  await settle();
  button("Confirm remove").click();
  await settle();
  const closing = requestShutdown();
  await settle();
  removal.reject(new Error("AniList is unavailable"));
  await closing;
  await settle();
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: false });
  expect(document.body.textContent).toContain("AniList is unavailable");
  button("Remove from list").click();
  await settle();
  button("Confirm remove").click();
  await settle();
  await requestShutdown();
  expect(mocks.api.deleteEntry).toHaveBeenCalledTimes(2);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test.each(["logout", "loginWithToken"] as const)("%s waits for both phases of an editor save", async (method) => {
  const metadata = deferred<typeof details>();
  const progress = deferred<typeof entry>();
  mocks.api.updateEntryDetails.mockReturnValue(metadata.promise);
  mocks.api.updateEntry.mockReturnValue(progress.promise);
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  const changing = method === "logout" ? auth.logout() : auth.loginWithToken("replacement token");
  await settle();
  expect(auth.changingSession).toBe(true);
  expect(mocks.api[method]).not.toHaveBeenCalled();
  metadata.resolve({ ...details, notes: "New note" });
  await settle();
  expect(mocks.api.updateEntry).toHaveBeenCalledExactlyOnceWith(1, null, 4, null, null);
  expect(mocks.api[method]).not.toHaveBeenCalled();
  progress.resolve({ ...entry, progress: 4 });
  await changing;
  expect(mocks.api[method]).toHaveBeenCalledOnce();
  expect(auth.changingSession).toBe(false);
  expect(auth.user?.id ?? null).toBe(method === "logout" ? null : 2);
});

test("a tracking navigation preserves the rest of a submitted editor save", async () => {
  const metadata = deferred<typeof details>();
  const progress = deferred<typeof entry>();
  mocks.api.updateEntryDetails.mockReturnValue(metadata.promise);
  mocks.api.updateEntry.mockReturnValue(progress.promise);
  instances.push(mount(Tracking, { target: document.body }));
  await settle();
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  let navigation: Promise<void> | undefined;
  mocks.goto.mockImplementation((path: string) => {
    if (path === "/now") navigation = unmount(instances.shift()!);
    return navigation;
  });
  mocks.listeners.get("kurisu://tracking-ask")!({ payload: { media_id: 2, episode: 5, progress: 4, title: "Another Show" } });
  await navigation;
  await settle();
  expect(mocks.goto).toHaveBeenCalledExactlyOnceWith("/now");
  expect(document.querySelector("#ed-progress")).toBeNull();
  const closing = requestShutdown();
  await settle();
  metadata.resolve({ ...details, notes: "New note" });
  await settle();
  const submittedProgress = [...mocks.api.updateEntry.mock.calls];
  const approvedBeforeProgress = mocks.invoke.mock.calls.some(([command]) => command === "finish_shutdown");
  progress.resolve({ ...entry, progress: 4 });
  await closing;
  expect(submittedProgress).toEqual([[1, null, 4, null, null]]);
  expect(approvedBeforeProgress).toBe(false);
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("session expiry stops the remaining editor write after navigation", async () => {
  const metadata = deferred<typeof details>();
  mocks.api.updateEntryDetails.mockReturnValue(metadata.promise);
  button("Notes, viewing dates and custom lists ▾").click();
  await settle();
  input("#ed-notes", "New note");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  await unmount(instances.shift()!);
  mocks.api.currentUser.mockResolvedValue(null);
  await auth.refresh();
  expect(auth.isLoggedIn).toBe(false);
  const closing = requestShutdown();
  metadata.resolve({ ...details, notes: "New note" });
  await closing;
  expect(mocks.api.updateEntry).not.toHaveBeenCalled();
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("installation waits for an editor save already in flight", async () => {
  const write = deferred<typeof entry>();
  mocks.api.updateEntry.mockReturnValue(write.promise);
  mocks.api.installUpdate.mockResolvedValue("installed");
  input("#ed-progress", "4");
  button("Save").click();
  await settle();
  const installing = runInstallUpdate();
  await settle();
  const installedEarly = mocks.api.installUpdate.mock.calls.length > 0;
  write.resolve({ ...entry, progress: 4 });
  await installing;
  expect(installedEarly).toBe(false);
  expect(mocks.api.installUpdate).toHaveBeenCalledOnce();
});
