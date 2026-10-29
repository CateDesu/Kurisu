import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { createTitleSorter, matchesListQuery, reconcileListEntries } from "#lib/myList.ts";
import { runClock } from "#lib/now.svelte.ts";
import { displayTitle, type ListEntry, type Media } from "#lib/types.ts";
import { auth } from "./session.svelte";
import { button, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: { localEntries: vi.fn(), syncMyList: vi.fn(), setProgress: vi.fn() },
  listeners: new Map<string, Set<() => void>>(),
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("$app/navigation", () => ({ goto: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, callback: () => void) => {
    const handlers = mocks.listeners.get(event) ?? new Set();
    handlers.add(callback);
    mocks.listeners.set(event, handlers);
    return () => handlers.delete(callback);
  }),
}));

import Collection from "../src/routes/+page.svelte";

const media: Media = {
  id: 1, title_english: "Alpha Show", title_romaji: "First Romaji", title_native: "最初",
  cover_medium: "alpha.jpg", episodes: 12, format: "TV", season_year: 2025,
  next_airing_episode: 4, next_airing_at: 4_000_000_000, genres: ["Drama"], studios: ["Studio"],
};
const entry: ListEntry = { id: 10, media_id: 1, status: "CURRENT", progress: 3, repeat: 0, score: 75, updated_at: 100, media };
const second: ListEntry = { ...entry, media_id: 2, media: { ...media, id: 2, title_english: "Beta Show" } };
const copy = <T,>(value: T): T => JSON.parse(JSON.stringify(value));
let component: ReturnType<typeof mount> | undefined;

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetAllMocks();
  mocks.listeners.clear();
  localStorage.clear();
  auth.epoch++;
  auth.isLoggedIn = true;
  auth.offline = true;
  auth.user = { id: 1, name: "Example", score_format: "POINT_100" };
  mocks.api.localEntries.mockImplementation(async () => copy([second, entry]));
});

afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
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

async function search(value: string) {
  const input = document.querySelector<HTMLInputElement>(".list-search input")!;
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
}

async function refresh() {
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback();
  await vi.advanceTimersByTimeAsync(300);
  await settle();
}

function rowTitles() {
  return [...document.querySelectorAll(".row-title")].map((row) => row.textContent);
}

test("a rehydrated snapshot retains the array, entries and all unchanged media", () => {
  const previous = [entry, second];
  const next = reconcileListEntries(previous, copy(previous));
  expect(next).toBe(previous);
  expect(next[0]).toBe(entry);
  expect(next[0].media).toBe(media);
  const empty: ListEntry[] = [];
  expect(reconcileListEntries(empty, [])).toBe(empty);
});

test("reconciliation follows server order, removals and additions while retaining surviving data", () => {
  const third = { ...second, media_id: 3, media: { id: 3, title_english: "Third Show" } };
  const fourth = { ...second, media_id: 4, media: { id: 4, title_english: "Fourth Show" } };
  const previous = [entry, second, third];
  const incoming = [copy(third), { ...copy(entry), progress: 4, repeat: 1, updated_at: 200 }, fourth];
  const next = reconcileListEntries(previous, incoming);
  expect(next).toEqual(incoming);
  expect(next[0]).toBe(third);
  expect(next[1]).not.toBe(entry);
  expect(next[1].media).toBe(media);
  expect(next[2]).toBe(fourth);
  expect(previous).toEqual([entry, second, third]);
  expect(entry.progress).toBe(3);
});

test.each([
  { media_id: 1, status: "PLANNING", progress: 0, repeat: 0 },
  { id: null, media_id: 1, status: "PLANNING", progress: 0, repeat: 0, score: null, updated_at: null, media: null },
  { media_id: 1, status: "PLANNING", progress: 0, repeat: 0, score: 0, media: { id: 1, title_english: null, episodes: null } },
])("refresh data can clear optional values without restoring cached fields: %j", (incoming) => {
  const next = reconcileListEntries([entry], [incoming]);
  expect(next).toStrictEqual([incoming]);
  expect(next[0]).not.toBe(entry);
  expect(entry.media).toBe(media);
});

test("explicitly absent and undefined metadata stay distinct", () => {
  const withoutScore = { media_id: 1, status: "CURRENT", progress: 3, repeat: 0 };
  const withScore = { ...withoutScore, score: undefined };
  expect(reconcileListEntries([withoutScore], [withScore])[0]).toBe(withScore);
  expect(reconcileListEntries([withScore], [withoutScore])[0]).toBe(withoutScore);
});

test("secondary metadata and ordered metadata arrays are not hidden by unchanged list fields", () => {
  const changed = {
    ...copy(entry),
    media: { ...copy(media), cover_large: "new-large.jpg", description: "New description", genres: ["Drama", "Comedy"], studios: [] },
  };
  const next = reconcileListEntries([entry, second], [changed, copy(second)]);
  expect(next).toEqual([changed, second]);
  expect(next[0].media).not.toBe(media);
  expect(next[1]).toBe(second);
});

test("title search covers each language and follows refreshed titles", () => {
  expect(matchesListQuery(entry, "alpha")).toBe(true);
  expect(matchesListQuery(entry, "romaji")).toBe(true);
  expect(matchesListQuery(entry, "最初")).toBe(true);
  expect(matchesListQuery(entry, "showfirst")).toBe(false);
  const [renamed] = reconcileListEntries([entry], [{ ...copy(entry), media: { ...copy(media), title_romaji: "Replacement" } }]);
  expect(matchesListQuery(renamed, "replacement")).toBe(true);
  expect(matchesListQuery(renamed, "romaji")).toBe(false);
  expect(matchesListQuery({ ...entry, media: null }, "alpha")).toBe(false);
});

test("typing filters immediately without repeating title comparisons or storage writes", async () => {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  const writes = vi.spyOn(Storage.prototype, "setItem");
  await render();
  expect(rowTitles()).toEqual(["Alpha Show", "Beta Show"]);
  expect(compare).toHaveBeenCalled();
  compare.mockClear();
  writes.mockClear();
  await search("A");
  await search("AL");
  await search(" ALPHA ");
  expect(rowTitles()).toEqual(["Alpha Show"]);
  expect(compare).not.toHaveBeenCalled();
  expect(writes).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(300);
  expect(writes.mock.calls).toEqual([["kurisu.list.q", " ALPHA "]]);
  await search("");
  expect(rowTitles()).toEqual(["Alpha Show", "Beta Show"]);
  expect(compare).not.toHaveBeenCalled();
});

test("equivalent cache and sync refreshes do not sort unchanged entries again", async () => {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  mocks.api.syncMyList.mockImplementation(async () => copy([second, entry]));
  await render();
  compare.mockClear();
  await refresh();
  button("Sync").click();
  await settle();
  expect(mocks.api.localEntries).toHaveBeenCalledTimes(2);
  expect(mocks.api.syncMyList).toHaveBeenCalledOnce();
  expect(rowTitles()).toEqual(["Alpha Show", "Beta Show"]);
  expect(compare).not.toHaveBeenCalled();
});

test.each([
  { progress: 4 }, { score: 90 }, { repeat: 1 }, { updated_at: 200 },
])("title order survives an entry update without collation: %j", async (change) => {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  await render();
  const rows = [...document.querySelectorAll(".collection-row")];
  compare.mockClear();
  mocks.api.localEntries.mockResolvedValue([copy(second), { ...copy(entry), ...change }]);
  await refresh();
  expect(rowTitles()).toEqual(["Alpha Show", "Beta Show"]);
  expect([...document.querySelectorAll(".collection-row")]).toEqual(rows);
  if ("progress" in change) expect(rows[0].textContent).toContain("4/12");
  if ("score" in change) expect(rows[0].textContent).toContain("★ 90");
  expect(compare).not.toHaveBeenCalled();
});

test("title ordering reuses 1000 ranks with current references after list fields change", () => {
  const collate = new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare;
  const compare = vi.fn(collate);
  const sort = createTitleSorter(compare);
  const previous = Array.from({ length: 1000 }, (_, index): ListEntry => ({
    ...entry, media_id: index + 1, media: { id: index + 1, title_english: `Show ${1000 - index}` },
  }));
  const original = sort(previous, false);
  expect(compare).toHaveBeenCalled();
  compare.mockClear();
  const current = previous.map((item, index) => ({ ...item, progress: index, score: index % 100, repeat: 2, updated_at: 300 }));
  const sorted = sort(current, false);
  expect(sorted.map((item) => item.media_id)).toEqual(original.map((item) => item.media_id));
  for (let index = 0; index < sorted.length; index++) expect(sorted[index]).toBe(current[999 - index]);
  expect(compare).not.toHaveBeenCalled();
  expect(previous[0].progress).toBe(3);
});

test.each([false, true])("cached title ties follow source order in direction %s without new collation", (desc) => {
  const collate = new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare;
  const compare = vi.fn(collate);
  const sort = createTitleSorter(compare);
  const list = ["Show 2", "Shów 2", "SHOW 2", "Show 10", "Alpha", "álpha"].map((title, index): ListEntry => ({
    ...entry, media_id: index + 1, media: { id: index + 1, title_english: title },
  }));
  sort(list, desc);
  compare.mockClear();
  const incoming = copy(list).reverse();
  const expected = [...incoming].sort((a, b) => (desc ? -1 : 1) * collate(displayTitle(a.media), displayTitle(b.media)));
  const sorted = sort(incoming, desc);
  sorted.forEach((item, index) => expect(item).toBe(expected[index]));
  expect(compare).not.toHaveBeenCalled();
});

test.each(["membership", "status", "displayed title", "fallback title", "direction"])(
  "%s changes invalidate title ordering", (change) => {
    const collate = new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare;
    const compare = vi.fn(collate);
    const sort = createTitleSorter(compare);
    sort([second, entry], false);
    compare.mockClear();
    let incoming = copy([second, entry]);
    if (change === "membership") incoming = [incoming[0], { ...entry, media_id: 3 }];
    if (change === "status") incoming[1].status = "COMPLETED";
    if (change === "displayed title") incoming[1].media!.title_english = "Zulu";
    if (change === "fallback title") incoming[1].media = { id: 1, title_romaji: "Zulu" };
    const desc = change === "direction";
    const sorted = sort(incoming, desc);
    expect(sorted).toEqual([...incoming].sort((a, b) => (desc ? -1 : 1) * collate(displayTitle(a.media), displayTitle(b.media))));
    expect(compare).toHaveBeenCalled();
  },
);

test("secondary titles and metadata update search and rows without invalidating title order", async () => {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  await render();
  compare.mockClear();
  mocks.api.localEntries.mockResolvedValue([
    copy(second),
    { ...copy(entry), media: { ...copy(media), title_romaji: "Replacement", cover_medium: "new.jpg", format: "MOVIE" } },
  ]);
  await refresh();
  await search("replacement");
  expect(rowTitles()).toEqual(["Alpha Show"]);
  expect(document.querySelector(".collection-row")!.textContent).toContain("MOVIE");
  expect(document.querySelector(".collection-row img")?.getAttribute("src")).toBe("new.jpg");
  await search("romaji");
  expect(rowTitles()).toEqual(["Beta Show"]);
  expect(compare).not.toHaveBeenCalled();
});

test("a focused stepper retains its pending save and title order after an accepted progress edit", async () => {
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  mocks.api.setProgress.mockResolvedValue({ ...copy(entry), progress: 4 });
  await render();
  const plus = document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  plus.focus();
  compare.mockClear();
  plus.click();
  await settle();
  expect(mocks.api.setProgress).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(document.activeElement).toBe(plus);
  expect(document.querySelector(".row-progress")!.textContent).toContain("4/12");
  expect(document.body.textContent).toContain("7 Episodes Tracked");
  expect(compare).not.toHaveBeenCalled();
});

test.each(["score", "progress", "updated"] as const)("%s updates still reorder a non-title list", async (key) => {
  localStorage.setItem("kurisu.list.sort", key);
  await render();
  expect(rowTitles()).toEqual(["Alpha Show", "Beta Show"]);
  const changes = { score: 90, progress: 8, updated_at: 200 };
  const field = key === "updated" ? "updated_at" : key;
  mocks.api.localEntries.mockResolvedValue([{ ...copy(second), [field]: changes[field] }, copy(entry)]);
  await refresh();
  expect(rowTitles()).toEqual(["Beta Show", "Alpha Show"]);
});

test("refreshes update displayed metadata and matching titles while preserving a buffered episode edit", async () => {
  mocks.api.localEntries.mockResolvedValue([copy(entry)]);
  mocks.api.setProgress.mockResolvedValue({ ...entry, progress: 4 });
  await render();
  document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  await search("show");
  expect(document.querySelector(".row-progress")?.textContent).toContain("4/12");
  const updated = {
    ...copy(entry), score: 90,
    media: { ...copy(media), title_english: "Replacement Show", cover_medium: "replacement.jpg", episodes: 24, format: "TV_SHORT", season_year: 2026, next_airing_episode: 5 },
  };
  mocks.api.localEntries.mockResolvedValue([updated]);
  await refresh();
  await search("replacement");
  expect(rowTitles()).toEqual(["Replacement Show"]);
  const row = document.querySelector(".collection-row")!;
  expect(row.textContent).toContain("TV SHORT");
  expect(row.textContent).toContain("2026");
  expect(row.textContent).toContain("★ 90");
  expect(row.textContent).toContain("Ep 5 airs");
  expect(row.querySelector("img")?.getAttribute("src")).toBe("replacement.jpg");
  expect(row.querySelector(".row-progress")?.textContent).toContain("4/24");
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
});

test("navigation flushes the latest filter text even before its effect or timer runs", async () => {
  await render();
  const input = document.querySelector<HTMLInputElement>(".list-search input")!;
  input.value = "Beta";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await unmount(component!);
  component = undefined;
  document.body.replaceChildren();
  expect(localStorage.getItem("kurisu.list.q")).toBe("Beta");
  await render();
  expect(document.querySelector<HTMLInputElement>(".list-search input")!.value).toBe("Beta");
  expect(rowTitles()).toEqual(["Beta Show"]);
});

test("pagehide saves the latest query once and cancels the delayed write", async () => {
  await render();
  const writes = vi.spyOn(Storage.prototype, "setItem");
  await search("Alpha");
  await vi.advanceTimersByTimeAsync(100);
  await search("Beta");
  expect(writes).not.toHaveBeenCalled();
  window.dispatchEvent(new Event("pagehide"));
  expect(writes.mock.calls).toEqual([["kurisu.list.q", "Beta"]]);
  await vi.advanceTimersByTimeAsync(1000);
  expect(writes).toHaveBeenCalledOnce();
});

test.each([
  ["title", ["Alpha Show", "Beta Show", "Show 2", "Show 10", "Undated Show"], ["Undated Show", "Show 10", "Show 2", "Beta Show", "Alpha Show"]],
  ["score", ["Beta Show", "Alpha Show", "Show 10", "Show 2", "Undated Show"], ["Alpha Show", "Show 10", "Beta Show", "Show 2", "Undated Show"]],
  ["progress", ["Beta Show", "Alpha Show", "Show 10", "Undated Show", "Show 2"], ["Show 2", "Undated Show", "Alpha Show", "Show 10", "Beta Show"]],
  ["updated", ["Alpha Show", "Show 10", "Beta Show", "Show 2", "Undated Show"], ["Beta Show", "Alpha Show", "Show 10", "Show 2", "Undated Show"]],
  ["airing", ["Alpha Show", "Beta Show", "Show 2", "Show 10", "Undated Show"], ["Beta Show", "Alpha Show", "Show 2", "Show 10", "Undated Show"]],
] as const)("%s sorting retains direction, title ties and missing values through query changes", async (key, forward, reverse) => {
  localStorage.setItem("kurisu.list.sort", key);
  mocks.api.localEntries.mockResolvedValue([
    entry,
    { ...second, score: 90, progress: 8, updated_at: 50, media: { ...second.media!, next_airing_at: 4_000_000_200 } },
    { ...entry, media_id: 3, media: { id: 3, title_english: "Show 10", next_airing_at: null } },
    { ...entry, media_id: 4, score: 0, progress: 0, updated_at: null, media: { id: 4, title_english: "Show 2", next_airing_at: 1 } },
    { ...entry, media_id: 5, score: null, progress: 2, updated_at: null, media: { id: 5, title_english: "Undated Show" } },
  ]);
  await render();
  expect(rowTitles()).toEqual(forward);
  await search("show");
  expect(rowTitles()).toEqual(forward);
  button(key === "title" || key === "airing" ? "↑" : "↓").click();
  await settle();
  expect(rowTitles()).toEqual(reverse);
  await search("show 1");
  expect(rowTitles()).toEqual(["Show 10"]);
  await search("");
  expect(rowTitles()).toEqual(reverse);
});

test("a refresh can clear displayed metadata and replace a title with another language", async () => {
  mocks.api.localEntries.mockResolvedValue([copy(entry)]);
  await render();
  mocks.api.localEntries.mockResolvedValue([{
    ...entry, score: null,
    media: { id: 1, title_english: null, title_romaji: "Fallback Romaji", cover_medium: null, episodes: null },
  }]);
  await refresh();
  expect(rowTitles()).toEqual(["Fallback Romaji"]);
  expect(document.querySelector(".row-meta")!.textContent?.trim()).toBe("");
  expect(document.querySelector(".list-cover")!.tagName).toBe("DIV");
  expect(document.querySelector(".row-progress")!.textContent).not.toContain("/12");
  await search("alpha");
  expect(rowTitles()).toEqual([]);
  await search("fallback");
  expect(rowTitles()).toEqual(["Fallback Romaji"]);
});

test("an account switch cancels the previous account's staged query", async () => {
  await render();
  await search("Alpha");
  mocks.api.localEntries.mockResolvedValue([second]);
  auth.epoch++;
  auth.user = { ...auth.user!, id: 2 };
  await settle();
  expect(document.querySelector<HTMLInputElement>(".list-search input")!.value).toBe("");
  expect(rowTitles()).toEqual(["Beta Show"]);
  await vi.advanceTimersByTimeAsync(1000);
  expect(localStorage.getItem("kurisu.list.q")).toBeNull();
});

test("airing sorts only at relevant expiries, including exact boundaries and clock jumps", async () => {
  const start = Date.UTC(2026, 9, 27, 12);
  vi.setSystemTime(start - 30_000);
  const stopClock = runClock();
  await vi.advanceTimersByTimeAsync(30_000);
  const compare = vi.fn(new Intl.Collator(undefined, { sensitivity: "base", numeric: true }).compare);
  vi.spyOn(Intl.Collator.prototype, "compare", "get").mockReturnValue(compare);
  localStorage.setItem("kurisu.list.sort", "airing");
  const airing = (id: number, title: string, offset: number | null): ListEntry => ({
    ...entry, media_id: id,
    media: { id, title_english: title, next_airing_at: offset === null ? null : (start + offset) / 1000 },
  });
  mocks.api.localEntries.mockResolvedValue([
    airing(1, "Zulu", 60_000), airing(2, "Beta", 120_000),
    airing(3, "Alpha", null), airing(4, "Charlie", null),
    { ...airing(5, "Other status", 30_000), status: "PLANNING" },
  ]);
  try {
    await render();
    expect(rowTitles()).toEqual(["Zulu", "Beta", "Alpha", "Charlie"]);
    compare.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(compare).not.toHaveBeenCalled();
    expect(rowTitles()).toEqual(["Zulu", "Beta", "Alpha", "Charlie"]);
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(rowTitles()).toEqual(["Beta", "Alpha", "Charlie", "Zulu"]);
    expect(compare).toHaveBeenCalled();
    compare.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(compare).not.toHaveBeenCalled();
    vi.setSystemTime(start + 180_000);
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(rowTitles()).toEqual(["Alpha", "Beta", "Charlie", "Zulu"]);
    vi.setSystemTime(start);
    await vi.advanceTimersByTimeAsync(30_000);
    await settle();
    expect(rowTitles()).toEqual(["Zulu", "Beta", "Alpha", "Charlie"]);
    button("↑").click();
    await settle();
    expect(rowTitles()).toEqual(["Beta", "Zulu", "Alpha", "Charlie"]);
    [...document.querySelectorAll<HTMLButtonElement>(".status-tab")].find((tab) => tab.textContent?.includes("Plan to Watch"))!.click();
    await settle();
    expect(rowTitles()).toEqual(["Other status"]);
  } finally {
    stopClock();
  }
});

test("fresh entries and a newly selected status rebuild the relevant airing cutoff", async () => {
  const start = Date.UTC(2026, 9, 27, 12);
  vi.setSystemTime(start - 30_000);
  const stopClock = runClock();
  await vi.advanceTimersByTimeAsync(30_000);
  localStorage.setItem("kurisu.list.sort", "airing");
  const list: ListEntry[] = [
    { ...entry, media: { ...media, next_airing_at: (start - 1000) / 1000 } },
    { ...second, media: { ...second.media!, next_airing_at: (start + 120_000) / 1000 } },
  ];
  mocks.api.localEntries.mockResolvedValue(list);
  try {
    await render();
    expect(rowTitles()).toEqual(["Beta Show", "Alpha Show"]);
    mocks.api.localEntries.mockResolvedValue([
      ...list,
      { ...entry, media_id: 3, media: { id: 3, title_english: "Fresh Show", next_airing_at: (start + 60_000) / 1000 } },
    ]);
    await refresh();
    expect(rowTitles()).toEqual(["Fresh Show", "Beta Show", "Alpha Show"]);
    await search("fresh");
    await vi.advanceTimersByTimeAsync(60_000);
    await settle();
    expect(rowTitles()).toEqual(["Fresh Show"]);
    await search("");
    expect(rowTitles()).toEqual(["Beta Show", "Alpha Show", "Fresh Show"]);
  } finally {
    stopClock();
  }
});
