import { afterEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { button, deferred, settle } from "./helpers";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("#lib/auth.svelte.ts", () => ({ auth: { epoch: 0, isLoggedIn: true, offline: false } }));
vi.mock("#lib/library.svelte.ts", () => ({ library: { fileFor: () => undefined } }));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn() }));

import { api } from "#lib/api.ts";
import { flushPendingEdits } from "#lib/pendingEdits.ts";
import EditEntry from "#lib/EditEntry.svelte";

afterEach(() => { invoke.mockReset(); });

const mutations = [
  ["a list status edit", () => api.updateEntry(1, "COMPLETED", null, null, null)],
  ["a manual progress update", () => api.setProgress(1, 4, 3)],
  ["an entry deletion", () => api.deleteEntry(1)],
  ["an entry metadata edit", () => api.updateEntryDetails(1, "New note", null, null, null)],
] as const;

test.each(mutations)("the save barrier waits for %s submitted outside a stepper", async (_, submit) => {
  const response = deferred<unknown>();
  invoke.mockReturnValue(response.promise);
  const mutation = submit().catch(() => {});
  let flushed = false;
  const closing = flushPendingEdits().then(() => { flushed = true; });
  await Promise.resolve();
  const approvedEarly = flushed;
  response.resolve(undefined);
  await Promise.all([mutation, closing]);
  expect(approvedEarly).toBe(false);
  expect(flushed).toBe(true);
});

test.each(mutations)("%s cannot let an already requested close ignore a failure", async (_, submit) => {
  const response = deferred<unknown>();
  const failure = new Error("The edit could not be saved");
  invoke.mockReturnValue(response.promise);
  const mutation = submit().catch(() => {});
  const closing = flushPendingEdits();
  const rejected = expect(closing).rejects.toBe(failure);
  response.reject(failure);
  await Promise.all([mutation, rejected]);
  await expect(flushPendingEdits()).resolves.toBeUndefined();
});

test("a list read does not hold the user-edit save barrier open", async () => {
  const response = deferred<unknown>();
  invoke.mockReturnValue(response.promise);
  const read = api.localEntries();
  let flushed = false;
  const closing = flushPendingEdits().then(() => { flushed = true; });
  await Promise.resolve();
  const completedWhileReading = flushed;
  response.resolve(undefined);
  await Promise.all([read, closing]);
  expect(completedWhileReading).toBe(true);
});

test("the editor and shared API keep a combined save tracked across both IPC requests", async () => {
  const entry = { media_id: 1, status: "CURRENT", progress: 3, score: 75, repeat: 0, media: { id: 1, title_english: "Example", episodes: 12 } };
  const details = { notes: "Old note", started_at: null, completed_at: null, custom_lists: [], available_custom_lists: [] };
  const metadata = deferred<typeof details>();
  const progress = deferred<typeof entry>();
  invoke.mockImplementation((command) => {
    if (command === "get_recommendations") return Promise.resolve([]);
    if (command === "get_entry_details") return Promise.resolve(details);
    if (command === "update_entry_details") return metadata.promise;
    if (command === "update_entry") return progress.promise;
    throw new Error(`Unexpected command: ${command}`);
  });
  const target = document.createElement("div");
  document.body.append(target);
  const editor = mount(EditEntry, { target, props: { entry, onclose: vi.fn(), scoreFormat: "POINT_100" } });
  try {
    await settle();
    button("Notes, viewing dates and custom lists ▾").click();
    await settle();
    for (const [selector, value] of [["#ed-notes", "New note"], ["#ed-progress", "4"]]) {
      const control = target.querySelector<HTMLInputElement>(selector)!;
      control.value = value;
      control.dispatchEvent(new Event("input", { bubbles: true }));
    }
    button("Save").click();
    await settle();
    let flushed = false;
    const closing = flushPendingEdits().then(() => { flushed = true; });
    await settle();
    const approvedDuringMetadata = flushed;
    metadata.resolve({ ...details, notes: "New note" });
    await settle();
    const approvedDuringProgress = flushed;
    const sentProgress = invoke.mock.calls.some(([command]) => command === "update_entry");
    progress.resolve({ ...entry, progress: 4 });
    await closing;
    expect(approvedDuringMetadata).toBe(false);
    expect(sentProgress).toBe(true);
    expect(approvedDuringProgress).toBe(false);
    expect(flushed).toBe(true);
  } finally {
    await unmount(editor);
    target.remove();
  }
});
