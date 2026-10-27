import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, settle } from "./helpers";
import type { ListEntry } from "#lib/types.ts";

const mocks = vi.hoisted(() => ({ invoke: vi.fn(), close: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("#lib/library.svelte.ts", () => ({ library: { fileFor: () => undefined } }));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: vi.fn() }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({
  close: mocks.close,
  isMaximized: async () => false,
  onResized: async () => () => {},
}) }));

import EditEntry from "#lib/EditEntry.svelte";
import EpisodeStepper from "#lib/EpisodeStepper.svelte";
import TitleBar from "#lib/TitleBar.svelte";
import { flushPendingEdits, registerPendingEdit } from "#lib/pendingEdits.ts";

const original: ListEntry = { media_id: 1, status: "CURRENT", progress: 3, repeat: 0,
  media: { id: 1, title_english: "Removed show", episodes: 12 } };
let entries: Map<number, ListEntry>;
let components: ReturnType<typeof mount>[];
let editKeys: Set<string>;
let finishOperations: (() => void)[];

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetAllMocks();
  vi.spyOn(console, "error").mockImplementation(() => {});
  auth.epoch++;
  auth.isLoggedIn = true;
  components = [];
  editKeys = new Set();
  finishOperations = [];
  entries = new Map([[1, { ...original }]]);
  mocks.invoke.mockImplementation(async (command: string, args: any) => {
    if (command === "get_recommendations") return [];
    if (command === "delete_entry_cmd") { entries.delete(args.mediaId); return; }
    if (command === "set_progress") {
      const entry = entries.get(args.mediaId);
      if (!entry) throw "the entry is no longer on your list";
      const updated = { ...entry, progress: args.expected === entry.progress ? args.progress : entry.progress };
      entries.set(args.mediaId, updated);
      return updated;
    }
    throw new Error(`Unexpected command: ${command}`);
  });
});

afterEach(async () => {
  try {
    for (const finish of finishOperations) finish();
    await settle();
    auth.epoch++;
    for (const component of components) await unmount(component);
    // Clear only fixture registrations after every observation has finished.
    for (const key of editKeys) registerPendingEdit(async () => {}, key)();
    await flushPendingEdits();
  } finally {
    document.body.replaceChildren();
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.restoreAllMocks();
  }
});

function operation<T>(fallback: T) {
  const result = deferred<T>();
  finishOperations.push(() => result.resolve(fallback));
  return result;
}

async function step(mediaId = 1, progress = 3) {
  editKeys.add(`${auth.epoch}:${mediaId}`);
  const target = document.createElement("div");
  document.body.append(target);
  const component = mount(EpisodeStepper, { target, props: { mediaId, progress, total: 12 } });
  components.push(component);
  await settle();
  target.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  return component;
}

async function remove() {
  editKeys.add(`${auth.epoch}:${original.media_id}`);
  const component = mount(EditEntry, { target: document.body, props: { entry: original, onclose: vi.fn() } });
  components.push(component);
  await settle();
  button("Remove from list").click();
  await settle();
  button("Confirm remove").click();
  await settle();
  return component;
}

async function detach(component: ReturnType<typeof mount>) {
  components.splice(components.indexOf(component), 1);
  await unmount(component);
  await settle();
}

async function close() {
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await settle();
}

async function closeFinished() {
  await vi.waitFor(() => {
    expect(document.querySelector<HTMLButtonElement>('[title="Close"]')!.disabled).toBe(false);
  }, { timeout: 100, interval: 1 });
}

test("removing an entry cancels its buffered progress and still saves other shows before close", async () => {
  entries.set(2, { ...original, media_id: 2, progress: 6 });
  const removedStepper = await step();
  await step(2, 6);
  await remove();
  await unmount(removedStepper);
  components.splice(components.indexOf(removedStepper), 1);
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await settle();

  expect(entries.has(1)).toBe(false);
  expect(entries.get(2)?.progress).toBe(7);
  expect(mocks.close).toHaveBeenCalledOnce();
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 2, progress: 7, expected: 6 }]]);
});

test("a failed removal retains its buffered progress for persistence", async () => {
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? Promise.reject("AniList is offline") : implementation(command, args));
  await step();
  await remove();
  expect(document.body.textContent).toContain("AniList is offline");

  await flushPendingEdits();
  expect(entries.get(1)?.progress).toBe(4);
  expect(mocks.invoke).toHaveBeenCalledWith("set_progress", { mediaId: 1, progress: 4, expected: 3 });
});

test("closing during a removal waits without submitting the buffered progress", async () => {
  const deletion = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletion.promise.then(() => { entries.delete(1); }) : implementation(command, args));
  await step();
  await remove();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await vi.advanceTimersByTimeAsync(3000);
  const closedEarly = mocks.close.mock.calls.length > 0;
  deletion.resolve();
  await settle();

  expect(closedEarly).toBe(false);
  expect(entries.has(1)).toBe(false);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress")).toEqual([]);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("a failed pending removal keeps Close open and restores progress for retry", async () => {
  const deletion = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletion.promise : implementation(command, args));
  await step();
  await remove();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await vi.advanceTimersByTimeAsync(3000);
  deletion.reject("AniList is offline");
  await settle();
  const closedAfterFailure = mocks.close.mock.calls.length > 0;
  const failureVisible = document.body.textContent?.includes("AniList is offline");
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await settle();

  expect(closedAfterFailure).toBe(false);
  expect(failureVisible).toBe(true);
  expect(entries.get(1)?.progress).toBe(4);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("a save completed during a failed removal can still leave the save barrier", async () => {
  const deletion = operation<void>(undefined);
  const progress = operation<ListEntry>({ ...original, progress: 4 });
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletion.promise : command === "set_progress" ? progress.promise.then((saved) => {
      entries.set(1, saved);
      return saved;
    }) : implementation(command, args));
  await step();
  await vi.advanceTimersByTimeAsync(3000);
  await remove();
  progress.resolve({ ...original, progress: 4 });
  await settle();
  deletion.reject("AniList is offline");
  await settle();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  document.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await settle();

  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(entries.get(1)?.progress).toBe(4);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("a failed removal does not replace a newer buffered edit for the same show", async () => {
  const deletion = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletion.promise : implementation(command, args));
  await step();
  await remove();
  await step();
  [...document.querySelectorAll<HTMLButtonElement>('[aria-label="One more episode"]')].at(-1)!.click();
  await settle();
  deletion.reject("AniList is offline");
  await settle();

  await flushPendingEdits();
  expect(entries.get(1)?.progress).toBe(5);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 5, expected: 3 }]]);
});

test("a late removal cannot cancel buffered progress for a replacement account", async () => {
  const deletion = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletion.promise : implementation(command, args));
  await step();
  await remove();
  auth.epoch++;
  entries.set(1, { ...original, progress: 6 });
  await step(1, 6);
  deletion.resolve();
  await settle();

  await flushPendingEdits();
  expect(entries.get(1)?.progress).toBe(7);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 7, expected: 6 }]]);
});

test("overlapping failed removals after a route remount retain progress for retry", async () => {
  const first = operation<void>(undefined);
  const second = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? (++removals === 1 ? first.promise : second.promise) : implementation(command, args));
  const oldStepper = await step();
  await detach(await remove());
  await detach(oldStepper);
  await remove();
  expect(removals).toBe(2);
  first.reject("AniList is offline");
  await settle();
  second.reject("AniList is still offline");
  await settle();
  expect(entries.get(1)?.progress).toBe(3);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress")).toEqual([]);

  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  await closeFinished();
  expect(entries.get(1)?.progress).toBe(4);
  expect(mocks.close).toHaveBeenCalledOnce();
  await close();
  await closeFinished();
  expect(mocks.close).toHaveBeenCalledTimes(2);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(document.querySelector('[role="alert"]')).toBeNull();
});

const removalOutcomes = [
  [false, false, false], [false, false, true], [false, true, false], [false, true, true],
  [true, false, false], [true, false, true], [true, true, false], [true, true, true],
];

test.each(removalOutcomes.flatMap((outcomes) => [
  { outcomes, order: [0, 1, 2], label: `${outcomes.join(", ")} forward` },
  { outcomes, order: [2, 1, 0], label: `${outcomes.join(", ")} reverse` },
]))("three overlapping removals retain progress only when all fail: $label", async ({ outcomes, order }) => {
  const deletions = outcomes.map(() => operation<void>(undefined));
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? deletions[removals++].promise.then(() => { entries.delete(args.mediaId); }) : implementation(command, args));
  const oldStepper = await step();
  for (let index = 0; index < deletions.length; index++) {
    const editor = await remove();
    if (index === 0) await detach(oldStepper);
    if (index < deletions.length - 1) await detach(editor);
  }
  expect(removals).toBe(3);
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  for (const index of order) {
    expect(mocks.close).not.toHaveBeenCalled();
    expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress")).toEqual([]);
    if (outcomes[index]) deletions[index].resolve();
    else deletions[index].reject(`Removal ${index + 1} is offline`);
    await settle();
  }
  await closeFinished();
  const allSucceeded = outcomes.every(Boolean);
  expect(mocks.close).toHaveBeenCalledTimes(allSucceeded ? 1 : 0);
  if (!allSucceeded) {
    expect(document.querySelector('[role="alert"]')?.textContent).toContain("is offline");
    await close();
  }
  await closeFinished();
  expect(mocks.close).toHaveBeenCalledOnce();
  const deleted = outcomes.some(Boolean);
  expect(entries.get(1)?.progress).toBe(deleted ? undefined : 4);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual(deleted ? [] : [["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(document.querySelector('[role="alert"]')).toBeNull();
});

test.each(["before", "after"])("a newer same-show edit %s the second removal survives both failures", async (when) => {
  const first = operation<void>(undefined);
  const second = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? (++removals === 1 ? first.promise : second.promise) : implementation(command, args));
  const oldStepper = await step();
  await detach(await remove());
  await detach(oldStepper);
  async function newerEdit() {
    await step();
    [...document.querySelectorAll<HTMLButtonElement>('[aria-label="One more episode"]')].at(-1)!.click();
    await settle();
  }
  if (when === "before") await newerEdit();
  await remove();
  if (when === "after") await newerEdit();
  first.reject("First removal is offline");
  await settle();
  second.reject("Second removal is offline");
  await settle();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  await closeFinished();

  expect(entries.get(1)?.progress).toBe(5);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 5, expected: 3 }]]);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("a failed removal cannot resume the progress timer while another removal is pending", async () => {
  const first = operation<void>(undefined);
  const second = operation<void>(undefined);
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? (++removals === 1 ? first.promise : second.promise) : implementation(command, args));
  await step();
  await detach(await remove());
  await remove();
  first.reject("First removal is offline");
  await settle();
  await vi.advanceTimersByTimeAsync(3000);
  expect(entries.get(1)?.progress).toBe(3);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress")).toEqual([]);
  second.reject("Second removal is offline");
  await settle();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  await closeFinished();

  expect(entries.get(1)?.progress).toBe(4);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("successful overlapping removals still wait for a previously submitted save acknowledgement", async () => {
  const first = operation<void>(undefined);
  const second = operation<void>(undefined);
  const progress = operation<ListEntry>({ ...original, progress: 4 });
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => {
    if (command === "set_progress") {
      entries.set(1, { ...original, progress: args.progress });
      return progress.promise;
    }
    if (command === "delete_entry_cmd") {
      return (++removals === 1 ? first.promise : second.promise).then(() => { entries.delete(1); });
    }
    return implementation(command, args);
  });
  const oldStepper = await step();
  await vi.advanceTimersByTimeAsync(3000);
  await detach(await remove());
  await detach(oldStepper);
  await remove();
  first.resolve();
  await settle();
  second.resolve();
  await settle();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  expect(mocks.close).not.toHaveBeenCalled();
  expect(entries.has(1)).toBe(false);
  progress.resolve({ ...original, progress: 4 });
  await settle();
  await closeFinished();

  expect(entries.has(1)).toBe(false);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 4, expected: 3 }]]);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test.each([false, true])("overlapping removals cannot replace another account's progress when the last succeeds: %s", async (succeeds) => {
  const first = operation<void>(undefined);
  const second = operation<void>(undefined);
  const oldEntries = entries;
  const implementation = mocks.invoke.getMockImplementation()!;
  let removals = 0;
  mocks.invoke.mockImplementation((command, args) => command === "delete_entry_cmd"
    ? (++removals === 1 ? first.promise : second.promise).then(() => { oldEntries.delete(1); })
    : implementation(command, args));
  const oldStepper = await step();
  await detach(await remove());
  await detach(oldStepper);
  await remove();
  auth.epoch++;
  entries = new Map([[1, { ...original, progress: 6 }]]);
  await step(1, 6);
  first.reject("The old account is offline");
  await settle();
  if (succeeds) second.resolve();
  else second.reject("The old account is still offline");
  await settle();
  components.push(mount(TitleBar, { target: document.body }));
  await settle();
  await close();
  await closeFinished();

  expect(entries.get(1)?.progress).toBe(7);
  expect(mocks.invoke.mock.calls.filter(([command]) => command === "set_progress"))
    .toEqual([["set_progress", { mediaId: 1, progress: 7, expected: 6 }]]);
  expect(mocks.close).toHaveBeenCalledOnce();
});
