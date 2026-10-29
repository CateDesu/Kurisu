import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { library } from "#lib/library.svelte.ts";
import type { LibraryFile, LibraryScan, ListEntry } from "#lib/types.ts";
import { auth } from "./session.svelte";
import { button, deferred, settle, settleListUpdates } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: {
    localEntries: vi.fn(), localEntryUpdates: vi.fn(), getLibraryFolders: vi.fn(), scanLibrary: vi.fn(),
    getLibraryBindingDetails: vi.fn(), getLibraryBinding: vi.fn(), bindLibraryPath: vi.fn(),
  },
  listeners: new Map<string, Set<(message?: { payload: unknown }) => void>>(),
  openPath: vi.fn(),
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, callback: (message?: { payload: unknown }) => void) => {
    const handlers = mocks.listeners.get(event) ?? new Set();
    handlers.add(callback);
    mocks.listeners.set(event, handlers);
    return () => handlers.delete(callback);
  }),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openPath: mocks.openPath, revealItemInDir: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

import Library from "../src/routes/library/+page.svelte";

const entry: ListEntry = {
  media_id: 1, status: "CURRENT", progress: 0, repeat: 0, score: 25,
  media: { id: 1, title_english: "First display", episodes: 12, format: "TV" },
};
const second: ListEntry = { ...entry, media_id: 2, media: { ...entry.media!, id: 2, title_english: "Second display" } };
const files: LibraryFile[] = [
  { path: "/synthetic/one-extra.mkv", media_id: 1, matched: "Series 10", episode: null },
  { path: "/synthetic/one-2.mkv", media_id: 1, matched: "Other match", episode: 2, bound: true },
  { path: "/synthetic/one-1.mkv", media_id: 1, matched: "Series 10", episode: 1 },
  { path: "/synthetic/one-10000.mkv", media_id: 1, matched: "Series 10", episode: 10000 },
  { path: "/synthetic/two-2.mkv", media_id: 2, matched: "series 2", episode: 2 },
  { path: "/synthetic/two-2-copy.mkv", media_id: 2, matched: "series 2", episode: 2 },
  { path: "/synthetic/two-1.mkv", media_id: 2, matched: "series 2", episode: 1 },
  { path: "/synthetic/three-1.mkv", media_id: 3, matched: null, episode: 1 },
  { path: "/synthetic/unmatched-b.mkv", media_id: null, matched: null, episode: 2 },
  { path: "/synthetic/unmatched-a.mkv", media_id: null, matched: null, episode: 1 },
];
const copy = <T,>(value: T): T => JSON.parse(JSON.stringify(value));
let component: ReturnType<typeof mount> | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetAllMocks();
  mocks.listeners.clear();
  auth.epoch++;
  auth.isLoggedIn = true;
  library.reset();
  mocks.api.localEntries.mockImplementation(async () => copy([entry, second]));
  mocks.api.getLibraryFolders.mockResolvedValue(["/synthetic"]);
  mocks.api.scanLibrary.mockResolvedValue({ files: copy(files), unreadable: [] });
  mocks.api.getLibraryBindingDetails.mockResolvedValue(null);
  mocks.api.getLibraryBinding.mockResolvedValue(null);
});

afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  library.reset();
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

async function render() {
  const target = document.createElement("div");
  document.body.append(target);
  component = mount(Library, { target });
  await settle();
  await settle();
}

function emit(event = "kurisu://pending-changed") {
  for (const callback of mocks.listeners.get(event) ?? []) callback();
}

async function refresh(entries: ListEntry[], event?: string) {
  mocks.api.localEntries.mockImplementation(async () => copy(entries));
  emit(event);
  await settleListUpdates();
}

async function refreshScoped(mediaId: number, entry: ListEntry | null) {
  mocks.api.localEntryUpdates.mockResolvedValue([entry]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback({ payload: { media_ids: [mediaId] } });
  await settleListUpdates();
}

function section(title: string) {
  return button(title).closest<HTMLElement>("section.cv-group")!;
}

async function expand(title: string) {
  button(title).closest<HTMLElement>('[aria-expanded]')!.click();
  await settle();
  return section(title);
}

function groupTitles() {
  return [...document.querySelectorAll("section.cv-group")].map((group) => group.querySelector('button[title="Open details"]')!.textContent!.trim());
}

function fileNames(group: HTMLElement) {
  return [...group.querySelectorAll<HTMLElement>(".cv-row")].map((row) => row.title.replace("Play ", ""));
}

function countSorts() {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  const episodes = vi.fn();
  const titles = vi.fn();
  const sort = Array.prototype.sort;
  vi.spyOn(Array.prototype, "sort").mockImplementation(function (this: LibraryFile[], comparator) {
    if (this[0]?.path?.startsWith("/synthetic/")) episodes();
    const first = this[0];
    if (first && typeof first === "object" && "mediaId" in first && "title" in first) titles();
    return sort.call(this, comparator);
  });
  return { compare, episodes, titles };
}

test("scoped library progress and removal retain unrelated file groups without another scan", async () => {
  await render();
  const group = await expand("First display");
  const row = group.querySelector(".cv-row");
  mocks.api.localEntries.mockClear();
  mocks.api.scanLibrary.mockClear();
  mocks.api.localEntryUpdates.mockResolvedValue([{ ...entry, progress: 6 }, null]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback({ payload: { media_ids: [1, 2] } });
  await settleListUpdates();
  expect(mocks.api.localEntries).not.toHaveBeenCalled();
  expect(mocks.api.localEntryUpdates).toHaveBeenCalledExactlyOnceWith([1, 2], auth.user!.id);
  expect(mocks.api.scanLibrary).not.toHaveBeenCalled();
  expect(section("First display")).toBe(group);
  expect(group.querySelector(".cv-row")).toBe(row);
  expect(group.textContent).toContain("Ep 6/12 watched");
  expect(document.body.textContent).not.toContain("Second display");
  expect(document.body.textContent).toContain("series 2");
});

test("scoped entry changes do not reevaluate unrelated expanded groups", async () => {
  await render();
  const changed = await expand("First display");
  const untouched = await expand("Second display");
  const originalRows = [...untouched.querySelectorAll(".cv-row")];
  const control = originalRows[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  control.focus();
  const reads = new Map<number, () => void>();
  for (const file of library.files) {
    if (file.media_id == null) continue;
    const read = reads.get(file.media_id) ?? vi.fn();
    reads.set(file.media_id, read);
    const episode = file.episode;
    Object.defineProperty(file, "episode", { configurable: true, get: () => { read(); return episode; } });
  }
  await refreshScoped(1, { ...entry, progress: 1 });
  expect(reads.get(1)).toHaveBeenCalled();
  expect(reads.get(2)).not.toHaveBeenCalled();
  expect(reads.get(3)).not.toHaveBeenCalled();
  expect(changed.textContent).toContain("Ep 1/12 watched");
  expect(changed.querySelector('[title="Play one-1.mkv"] [title="Watched (per your list progress)"]')).not.toBeNull();
  expect([...untouched.querySelectorAll(".cv-row")]).toEqual(originalRows);
  expect(document.activeElement).toBe(control);
  button("Play Ep 2", changed).click();
  await settle();
  expect(mocks.openPath).toHaveBeenCalledExactlyOnceWith("/synthetic/one-2.mkv");
});

test("cached groups receive current metadata and numbering actions through entry removal and addition", async () => {
  await render();
  const group = await expand("First display");
  const row = group.querySelector<HTMLElement>('[title="Play one-1.mkv"]')!;
  const control = row.querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  control.focus();
  const movie: ListEntry = { ...entry, media: {
    ...entry.media!, title_english: "Updated movie", cover_medium: "/synthetic/movie.jpg", format: "MOVIE", episodes: 1,
  } };
  await refreshScoped(1, movie);
  expect(section("Updated movie")).toBe(group);
  expect(group.querySelector("img")?.getAttribute("src")).toBe("/synthetic/movie.jpg");
  expect(group.textContent).toContain("Ep 0/1 watched");
  expect(document.activeElement).toBe(control);
  expect(row.querySelector('[title="Adjust episode numbering"]')).toBe(control);
  button("Play movie", group).click();
  await settle();
  expect(mocks.openPath).toHaveBeenCalledExactlyOnceWith("/synthetic/one-1.mkv");
  control.click();
  await settle();
  const dialog = document.querySelector('[role="dialog"]')!;
  expect(dialog.textContent).toContain("Episode numbering · Updated movie");
  button("Cancel", dialog).click();
  await settle();

  await refreshScoped(1, null);
  expect(section("Series 10")).toBe(group);
  expect(group.querySelector("img")).toBeNull();
  expect(group.textContent).toContain("Not on your list");
  expect(group.querySelector('[title="Adjust episode numbering"]')).toBeNull();
  expect(group.querySelector('[title="Play one-1.mkv"]')).toBe(row);
  await refreshScoped(1, { ...movie, progress: 1 });
  expect(section("Updated movie")).toBe(group);
  expect(group.textContent).toContain("Ep 1/1 watched");
  expect(row.querySelector('[title="Watched (per your list progress)"]')).not.toBeNull();
  expect(row.querySelector('[title="Adjust episode numbering"]')).not.toBeNull();
});

test("file snapshots invalidate cached bindings, titles, episode order and controls", async () => {
  await render();
  const group = await expand("Second display");
  const first = group.querySelector<HTMLElement>('[title="Play two-2.mkv"]')!;
  const duplicate = group.querySelector<HTMLElement>('[title="Play two-2-copy.mkv"]')!;
  await refreshScoped(2, null);
  const next = copy(files);
  next[4] = { ...next[4], matched: "A changed match", bound: true };
  next[5] = { ...next[5], episode: 1 };
  [next[4], next[5]] = [next[5], next[4]];
  next[4].matched = "A changed match";
  mocks.api.scanLibrary.mockResolvedValue({ files: next, unreadable: [] });
  await library.scan();
  await settle();
  expect(library.files).toEqual(next);
  expect(section("A changed match")).toBe(group);
  expect(groupTitles()).toEqual(["#3", "A changed match", "First display"]);
  expect(fileNames(group)).toEqual(["two-2-copy.mkv", "two-1.mkv", "two-2.mkv"]);
  expect(group.querySelector('[title="Play two-2-copy.mkv"]')).toBe(duplicate);
  expect(group.querySelector('[title="Play two-2.mkv"]')).toBe(first);
  expect(duplicate.textContent).toContain("Ep 1");
  expect(group.querySelector('[title="Manually linked — click to remove the link"]')).not.toBeNull();
  button("Play Ep 1", group).click();
  await settle();
  expect(mocks.openPath).toHaveBeenCalledExactlyOnceWith("/synthetic/two-2-copy.mkv");
});

test("list updates remain responsive while the startup rescan is pending", async () => {
  await library.ensureScan();
  const scan = deferred<LibraryScan>();
  mocks.api.scanLibrary.mockReturnValueOnce(scan.promise);
  vi.setSystemTime(Date.now() + 60_001);
  await render();
  expect(library.scanning).toBe(true);
  const group = section("First display");
  await refresh([{ ...entry, progress: 7 }, second]);
  expect(library.scanning).toBe(true);
  expect(section("First display")).toBe(group);
  expect(group.textContent).toContain("Ep 7/12 watched");
  scan.resolve({ files: copy(files), unreadable: [] });
  await settle();
  expect(library.scanning).toBe(false);
  expect(group.textContent).toContain("Ep 7/12 watched");
});

test("entry refreshes reuse file sorting and preserve expanded rows and numbering edits", async () => {
  const sorts = countSorts();
  await render();
  expect(sorts.episodes).toHaveBeenCalledTimes(3);
  expect(sorts.compare).toHaveBeenCalled();
  const group = await expand("First display");
  const row = group.querySelector<HTMLElement>('[title="Play one-2.mkv"]')!;
  const numbering = row.querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  numbering.click();
  await settle();
  const offset = document.querySelector<HTMLInputElement>("#library-episode-offset")!;
  offset.value = "-7";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  sorts.episodes.mockClear();
  sorts.compare.mockClear();

  await refresh([entry, second]);
  const progress = { ...entry, progress: 1 };
  await refresh([progress, second], "kurisu://episode-updated");
  expect(group.textContent).toContain("Ep 1/12 watched");
  button("Play Ep 2", group).click();
  await settle();
  expect(mocks.openPath).toHaveBeenCalledWith("/synthetic/one-2.mkv");
  await refresh([{ ...progress, status: "PAUSED", score: 80 }, second]);
  await refresh([{ ...progress, status: "COMPLETED", progress: 12, score: 90 }, second]);

  expect(group.textContent).toContain("Ep 12/12 watched");
  expect(group.querySelectorAll('[title="Watched (per your list progress)"]')).toHaveLength(2);
  expect(group.querySelector('[title="Play one-2.mkv"]')).toBe(row);
  expect(row.querySelector('[title="Adjust episode numbering"]')).toBe(numbering);
  expect(section("First display")).toBe(group);
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(mocks.api.getLibraryBindingDetails).toHaveBeenCalledOnce();
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();

  const saving = deferred<void>();
  mocks.api.bindLibraryPath.mockReturnValue(saving.promise);
  button("Save numbering").click();
  await settle();
  expect(mocks.api.bindLibraryPath).toHaveBeenCalledWith("/synthetic/one-2.mkv", 1, -7);
  await refresh([{ ...progress, score: 95 }, second]);
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(offset.disabled).toBe(true);
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();
  saving.resolve();
  await settle();
  expect(mocks.api.scanLibrary).toHaveBeenCalledTimes(2);
  expect(document.querySelector("#library-episode-offset")).toBeNull();
});

test("identical scans skip sorting and preserve expanded rows, focus and numbering edits", async () => {
  const sorts = countSorts();
  await render();
  expect(sorts.episodes).toHaveBeenCalledTimes(3);
  expect(sorts.titles).toHaveBeenCalledOnce();
  expect(sorts.compare).toHaveBeenCalled();
  const snapshot = library.files;
  const episode = library.fileFor(1, 2);
  const group = await expand("First display");
  const row = group.querySelector<HTMLElement>('[title="Play one-2.mkv"]')!;
  row.querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!.click();
  await settle();
  const offset = document.querySelector<HTMLInputElement>("#library-episode-offset")!;
  offset.value = "-7";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  offset.focus();
  await settle();
  const initialScanAt = library.lastScanAt;
  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  sorts.titles.mockClear();

  const unreadable = [{ path: "/offline", error: "Drive unavailable" }];
  for (const [index, failures] of [[], unreadable, []].entries()) {
    const time = initialScanAt + (index + 1) * 60_001;
    vi.setSystemTime(time);
    mocks.api.scanLibrary.mockResolvedValueOnce({ files: copy(files), unreadable: failures });
    await library.scan();
    await settle();
    expect(library.lastScanAt).toBe(time);
    expect(library.files).toBe(snapshot);
    expect(library.fileFor(1, 2)).toBe(episode);
    expect(library.unreadable).toEqual(failures);
    expect(document.body.textContent?.includes("Drive unavailable")).toBe(failures.length > 0);
    expect(section("First display")).toBe(group);
    expect(group.querySelector('[title="Play one-2.mkv"]')).toBe(row);
    expect(document.querySelector("#library-episode-offset")).toBe(offset);
    expect(offset.value).toBe("-7");
    expect(document.activeElement).toBe(offset);
  }
  expect(mocks.api.scanLibrary).toHaveBeenCalledTimes(4);
  expect(mocks.api.getLibraryBindingDetails).toHaveBeenCalledOnce();
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.titles).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();
});

test("one new episode sorts only its group and preserves unrelated numbering edits", async () => {
  const sorts = countSorts();
  await render();
  const untouched = await expand("Second display");
  const row = untouched.querySelector<HTMLElement>('[title="Play two-2.mkv"]')!;
  row.querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!.click();
  await settle();
  const offset = document.querySelector<HTMLInputElement>("#library-episode-offset")!;
  offset.value = "-7";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  offset.focus();
  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  sorts.titles.mockClear();
  mocks.api.scanLibrary.mockResolvedValue({ files: [...copy(files), {
    path: "/synthetic/one-3.mkv", media_id: 1, matched: "Series 10", episode: 3,
  }], unreadable: [] });
  await library.scan();
  await settle();
  expect(sorts.episodes).toHaveBeenCalledOnce();
  expect(sorts.titles).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();
  expect(section("Second display")).toBe(untouched);
  expect(untouched.querySelector('[title="Play two-2.mkv"]')).toBe(row);
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(document.activeElement).toBe(offset);
  button("Cancel", document.querySelector('[role="dialog"]')!).click();
  await settle();
  const changed = await expand("First display");
  expect(fileNames(changed)).toEqual(["one-1.mkv", "one-2.mkv", "one-3.mkv", "one-extra.mkv", "one-10000.mkv"]);
});

test("unmatched changes leave matched episode and title ordering cached", async () => {
  const sorts = countSorts();
  await render();
  const group = await expand("First display");
  const rows = [...group.querySelectorAll(".cv-row")];
  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  sorts.titles.mockClear();
  mocks.api.scanLibrary.mockResolvedValue({ files: [...copy(files).slice(0, -2), {
    path: "/synthetic/unmatched-new.mkv", media_id: null, matched: null, episode: 1,
  }], unreadable: [] });
  await library.scan();
  await settle();
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.titles).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();
  expect([...group.querySelectorAll(".cv-row")]).toEqual(rows);
  expect(document.body.textContent).toContain("unmatched-new.mkv");
  expect(document.body.textContent).not.toContain("unmatched-a.mkv");
});

test("collated group ties follow first appearance after a scan reorders groups", async () => {
  const tied = copy(files).map((file) => file.media_id === 2 ? { ...file, matched: "series 10" } : file);
  mocks.api.scanLibrary.mockResolvedValue({ files: tied, unreadable: [] });
  const sorts = countSorts();
  await render();
  expect(groupTitles()).toEqual(["#3", "First display", "Second display"]);
  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  mocks.api.scanLibrary.mockResolvedValue({ files: [
    ...tied.filter((file) => file.media_id === 2), ...tied.filter((file) => file.media_id !== 2),
  ], unreadable: [] });
  await library.scan();
  await settle();
  expect(groupTitles()).toEqual(["#3", "Second display", "First display"]);
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.compare).toHaveBeenCalled();
});

test("an open linking search receives refreshed titles and status ordering", async () => {
  await render();
  document.querySelector<HTMLButtonElement>('[title="Link to a show on your list"]')!.click();
  await settle();
  const dialog = document.querySelector('[role="dialog"]')!;
  const choices = () => [...dialog.querySelectorAll<HTMLButtonElement>("button.cv-row")].map((choice) => choice.textContent!.trim().replace(/\s+/g, " "));
  expect(choices()).toEqual(["First display Watching", "Second display Watching"]);
  await refreshScoped(1, { ...entry, status: "PLANNING", media: { ...entry.media!, title_english: "A renamed title" } });
  expect(choices()).toEqual(["Second display Watching", "A renamed title Plan to Watch"]);
  const input = dialog.querySelector<HTMLInputElement>('input[placeholder="Search your list…"]')!;
  input.value = "renamed";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
  expect(choices()).toEqual(["A renamed title Plan to Watch"]);
  button("A renamed title Plan to Watch", dialog).click();
  await settle();
  expect(mocks.api.bindLibraryPath).toHaveBeenCalledWith("/synthetic/unmatched-b.mkv", 1);
});

test("scan replacements regroup files while retaining ordering and current list metadata", async () => {
  const sorts = countSorts();
  await render();
  expect(groupTitles()).toEqual(["#3", "Second display", "First display"]);
  const firstGroup = await expand("First display");
  const secondGroup = await expand("Second display");
  expect(fileNames(firstGroup)).toEqual(["one-1.mkv", "one-2.mkv", "one-extra.mkv", "one-10000.mkv"]);
  expect(fileNames(secondGroup)).toEqual(["two-1.mkv", "two-2.mkv", "two-2-copy.mkv"]);
  expect(firstGroup.querySelector('[title="Manually linked — click to remove the link"]')).not.toBeNull();
  const unmatched = document.querySelector('[title="Link to a show on your list"]')!.closest("section")!;
  expect([...unmatched.querySelectorAll(".cv-row span")].map((span) => span.textContent)).toEqual(["unmatched-b.mkv", "unmatched-a.mkv"]);
  expect(library.files).toEqual(files);

  await refresh([{ ...entry, progress: 1, media: { ...entry.media!, title_english: "Renamed display" } }, second]);
  expect(groupTitles()).toEqual(["#3", "Second display", "Renamed display"]);
  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  mocks.api.scanLibrary.mockResolvedValue({ files: [
    { ...files[2], episode: 4, matched: "A new scan title" },
    { ...files[4], media_id: 1, episode: 3, matched: "A new scan title", bound: true },
    { ...files[8], media_id: 2, episode: 1, matched: "series 2" },
    { path: "/synthetic/new-unmatched.mkv", media_id: null, matched: null, episode: null },
  ], unreadable: [] });
  await library.scan();
  await settle();

  expect(sorts.episodes).toHaveBeenCalledTimes(2);
  expect(sorts.compare).toHaveBeenCalled();
  expect(groupTitles()).toEqual(["Renamed display", "Second display"]);
  expect(section("Renamed display")).toBe(firstGroup);
  expect(fileNames(firstGroup)).toEqual(["two-2.mkv", "one-1.mkv"]);
  expect(fileNames(secondGroup)).toEqual(["unmatched-b.mkv"]);
  expect(firstGroup.textContent).toContain("Ep 1/12 watched");
  expect(document.body.textContent).toContain("new-unmatched.mkv");
  expect(document.body.textContent).not.toContain("unmatched-a.mkv");
  expect(firstGroup.querySelector('[title="Manually linked — click to remove the link"]')).not.toBeNull();

  sorts.episodes.mockClear();
  sorts.compare.mockClear();
  await refresh([second]);
  expect(groupTitles()).toEqual(["A new scan title", "Second display"]);
  expect(firstGroup.textContent).toContain("Not on your list");
  expect(firstGroup.querySelector('[title="Adjust episode numbering"]')).toBeNull();
  expect(sorts.episodes).not.toHaveBeenCalled();
  expect(sorts.compare).not.toHaveBeenCalled();
});

test("entry snapshots obey request and account guards", async () => {
  await render();
  const old = deferred<ListEntry[]>();
  const current = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(old.promise).mockReturnValueOnce(current.promise);
  emit("kurisu://episode-updated");
  await settleListUpdates();
  emit();
  old.resolve([{ ...entry, progress: 2 }]);
  await settleListUpdates();
  expect(section("First display").textContent).not.toContain("Ep 2/12 watched");
  current.resolve([{ ...entry, progress: 5 }]);
  await settleListUpdates();
  expect(section("First display").textContent).toContain("Ep 5/12 watched");

  const previousAccount = deferred<ListEntry[]>();
  mocks.api.localEntries.mockReturnValueOnce(previousAccount.promise);
  emit();
  await settleListUpdates();
  mocks.api.localEntries.mockResolvedValue([second]);
  auth.epoch++;
  await settleListUpdates();
  await settleListUpdates();
  previousAccount.resolve([{ ...entry, progress: 10 }]);
  await settleListUpdates();
  expect(section("Series 10").textContent).toContain("Not on your list");
  expect(document.body.textContent).not.toContain("Ep 10/12 watched");
});
