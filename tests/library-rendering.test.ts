import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { library } from "#lib/library.svelte.ts";
import type { LibraryFile, ListEntry } from "#lib/types.ts";
import { auth } from "./session.svelte";
import { button, settle, settleListUpdates } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: {
    localEntries: vi.fn(), localEntryUpdates: vi.fn(), getLibraryFolders: vi.fn(), scanLibrary: vi.fn(),
    getLibraryBindingDetails: vi.fn(), bindLibraryPath: vi.fn(),
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

class Sizes {
  static observers: Sizes[] = [];
  targets = new Set<Element>();
  observe = vi.fn((target: Element) => { this.targets.add(target); });
  unobserve = vi.fn((target: Element) => { this.targets.delete(target); });
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: ResizeObserverCallback) { Sizes.observers.push(this); }
  deliver(entries: Array<[Element, number, number]>) {
    this.callback(entries.map(([target, width, height]) => ({
      target, contentRect: { width, height },
    })) as ResizeObserverEntry[], this as unknown as ResizeObserver);
  }
}

class Proximity {
  static observers: Proximity[] = [];
  targets = new Set<Element>();
  observe = vi.fn((target: Element) => { this.targets.add(target); });
  unobserve = vi.fn((target: Element) => { this.targets.delete(target); });
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: IntersectionObserverCallback, readonly options: IntersectionObserverInit) {
    Proximity.observers.push(this);
  }
  deliver(entries: Array<[Element, boolean]>) {
    this.callback(entries.map(([target, isIntersecting]) => ({ target, isIntersecting })) as IntersectionObserverEntry[],
      this as unknown as IntersectionObserver);
  }
}

const entry: ListEntry = {
  media_id: 1, status: "CURRENT", progress: 0, repeat: 0,
  media: { id: 1, title_english: "Cedar Show", episodes: 12, format: "TV", cover_medium: "/synthetic/cedar.jpg" },
};
const files: LibraryFile[] = [
  { path: "/synthetic/cedar-2.mkv", media_id: 1, matched: "Cedar", episode: 2 },
  { path: "/synthetic/cedar-1.mkv", media_id: 1, matched: "Cedar", episode: 1 },
  { path: "/synthetic/cedar-extra.mkv", media_id: 1, matched: "Cedar", episode: null },
  { path: "/synthetic/maple-1.mkv", media_id: 2, matched: "Maple", episode: 1 },
  { path: "/synthetic/unmatched-b.mkv", media_id: null, matched: null, episode: 2 },
  { path: "/synthetic/unmatched-a.mkv", media_id: null, matched: null, episode: 1 },
];
const copy = <T,>(value: T): T => JSON.parse(JSON.stringify(value));
let instance: ReturnType<typeof mount> | undefined;
let main: HTMLElement;
let frames: Map<number, FrameRequestCallback>;
let nextFrame: number;

function group() { return button("Cedar Show").closest<HTMLElement>("section.cv-group")!; }
function collection() { return group().parentElement!; }
function rows(root: HTMLElement) { return [...root.children] as HTMLElement[]; }
function episodes(section = group()) { return section.querySelector<HTMLElement>(".cv-row")!.parentElement!; }
function unmatched() { return document.querySelector('[title="Link to a show on your list"]')!.closest("section")!; }
function expectRows(root: HTMLElement, expected: HTMLElement[]) {
  expect(rows(root)).toHaveLength(expected.length);
  expected.forEach((row, index) => expect(root.children[index]).toBe(row));
}
function observer(root: HTMLElement) {
  const result = Sizes.observers.find((sizes) => sizes.targets.has(root));
  expect(result, "wrapper has its own measurement observer").toBeDefined();
  return result!;
}

function proximity(root: HTMLElement) {
  const result = Proximity.observers.find((nearby) => nearby.targets.has(root.firstElementChild!));
  expect(result, "wrapper has its own proximity observer").toBeDefined();
  expect(result!.options).toEqual({ root: main, rootMargin: "800px 0px" });
  return result!;
}

async function frame() {
  const pending = [...frames.values()];
  frames.clear();
  for (const callback of pending) callback(0);
  await settle();
}

async function drain() {
  await settle();
  for (let i = 0; i < 8 && frames.size; i++) await frame();
  expect(frames.size, "nested observers settle").toBe(0);
}

function measure(root: HTMLElement, heights: number[], width = 720) {
  expect(rows(root)).toHaveLength(heights.length);
  observer(root).deliver([
    [root, width, heights.reduce((sum, height) => sum + height, 0)],
    ...rows(root).map((row, index) => [row, width, heights[index]] as [Element, number, number]),
  ]);
}

async function render() {
  main = document.createElement("main");
  main.style.cssText = "overflow: auto; height: 300px";
  document.body.append(main);
  instance = mount(Library, { target: main });
  await settle();
  await settle();
}

async function expand(section = group()) {
  section.querySelector<HTMLElement>("[aria-expanded]")!.click();
  await settle();
  return episodes(section);
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetAllMocks();
  mocks.listeners.clear();
  auth.epoch++;
  auth.isLoggedIn = true;
  library.reset();
  mocks.api.localEntries.mockImplementation(async () => copy([entry]));
  mocks.api.getLibraryFolders.mockResolvedValue(["/synthetic"]);
  mocks.api.scanLibrary.mockImplementation(async () => ({ files: copy(files), unreadable: [] }));
  mocks.api.getLibraryBindingDetails.mockResolvedValue(null);
  Sizes.observers = [];
  Proximity.observers = [];
  frames = new Map();
  nextFrame = 0;
  vi.stubGlobal("CSS", { supports: () => true });
  vi.stubGlobal("ResizeObserver", Sizes);
  vi.stubGlobal("IntersectionObserver", Proximity);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
});

afterEach(async () => {
  if (instance) await unmount(instance);
  instance = undefined;
  library.reset();
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

test.each([false, true])("Library measures natural collection and episode sizes with proximity support: %s", async (supported) => {
  if (!supported) vi.stubGlobal("IntersectionObserver", undefined);
  await render();
  const root = collection();
  const sections = rows(root);
  const loose = episodes(unmatched());
  expect(sections).toHaveLength(3);
  expect(sections.every((section) => !section.style.contentVisibility)).toBe(true);
  expect(observer(root).targets).toEqual(new Set([root, ...sections]));
  expect(observer(loose).targets).toEqual(new Set([loose, ...rows(loose)]));
  measure(root, [80.25, 80.5, 130.75]);
  expect(sections[0].style.contentVisibility).toBe("");
  await drain();
  expect(sections.map((section) => section.style.getPropertyValue("contain-intrinsic-block-size")))
    .toEqual(["80.25px", "80.5px", "130.75px"]);
  expect(sections.every((section) => section.style.contentVisibility === "auto")).toBe(true);

  const list = await expand();
  expect(rows(list).map((row) => row.title)).toEqual(["Play cedar-1.mkv", "Play cedar-2.mkv", "Play cedar-extra.mkv"]);
  expect(rows(list).every((row) => !row.style.contentVisibility)).toBe(true);
  expect(observer(list).targets).toEqual(new Set([list, ...rows(list)]));
  await drain();
  measure(list, [32.25, 48.5, 64.75]);
  measure(loose, [35.25, 51.5]);
  await drain();
  measure(root, [226.75, 80.5, 130.75]);
  await drain();
  expect(rows(list).map((row) => row.style.getPropertyValue("contain-intrinsic-block-size")))
    .toEqual(["32.25px", "48.5px", "64.75px"]);
  expect([...rows(list), ...rows(loose)].every((row) => row.style.contentVisibility === "auto")).toBe(true);
  expectRows(root, sections);
  expect(root.querySelector("section section")).toBeNull();
});

test("Library retains nearby nested rows through rescans and resize and releases collapsed observers", async () => {
  await render();
  const root = collection();
  const sections = rows(root);
  const loose = episodes(unmatched());
  const looseRows = rows(loose);
  const list = await expand();
  const original = rows(list);
  await drain();
  measure(list, [32, 48, 64]);
  measure(loose, [35, 51]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  const outer = proximity(root);
  const inner = proximity(list);
  const unmatchedProximity = proximity(loose);
  outer.deliver([[sections[0], true], [sections[1], false], [sections[2], true]]);
  inner.deliver([[original[1], true], [original[2], false]]);
  unmatchedProximity.deliver([[looseRows[0], true], [looseRows[1], false]]);
  await drain();
  measure(list, [32, 48, 64]);
  measure(loose, [35, 51]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  expect([sections[0], sections[2], original[1], looseRows[0]].every((row) =>
    row.style.contentVisibility === "visible" && !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  expect([sections[1], original[0], original[2], looseRows[1]].every((row) => row.style.contentVisibility === "auto")).toBe(true);
  expect(outer.targets).toEqual(new Set([sections[1]]));
  expect(inner.targets).toEqual(new Set([original[0], original[2]]));
  expect(unmatchedProximity.targets).toEqual(new Set([looseRows[1]]));

  original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!.focus();
  original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!.click();
  await settle();
  const offset = document.querySelector<HTMLInputElement>("#library-episode-offset")!;
  offset.value = "-7";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  offset.focus();
  await library.scan();
  await drain();
  expectRows(root, sections);
  expectRows(list, original);
  expectRows(loose, looseRows);
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(document.activeElement).toBe(offset);
  outer.deliver([[sections[0], false], [sections[2], false]]);
  inner.deliver([[original[1], false]]);
  unmatchedProximity.deliver([[looseRows[0], false]]);
  measure(list, [48, 64, 80], 400);
  measure(loose, [51, 67], 400);
  measure(root, [272, 104, 162], 400);
  await drain();
  measure(list, [48, 64, 80], 400);
  measure(loose, [51, 67], 400);
  await drain();
  measure(root, [272, 104, 162], 400);
  await drain();
  expect([sections[0], sections[2], original[1], looseRows[0]].every((row) =>
    row.style.contentVisibility === "visible" && !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  expect(original[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("48px");
  expect(looseRows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("67px");
  button("Cancel", document.querySelector('[role="dialog"]')!).click();
  await drain();
  expect(document.activeElement).toBe(original[1].querySelector('[title="Adjust episode numbering"]'));
  sections[0].querySelector<HTMLElement>("[aria-expanded]")!.click();
  await drain();
  expect(inner.disconnect).toHaveBeenCalledOnce();
  expect(inner.targets.size).toBe(0);
  expect(original.every((row) => !row.style.contentVisibility)).toBe(true);
  const reopened = await expand();
  const reopenedProximity = proximity(reopened);
  expect(reopenedProximity).not.toBe(inner);
  inner.deliver([[original[1], true]]);
  expect(rows(reopened).every((row) => !row.style.contentVisibility)).toBe(true);
  await unmount(instance!);
  instance = undefined;
  expect(Proximity.observers.every((nearby) => nearby.targets.size === 0 && nearby.disconnect.mock.calls.length === 1)).toBe(true);
  expect(frames.size).toBe(0);
});

test.each(["observer", "CSS", "visibility", "size"])("Library keeps natural rows and controls without %s support", async (missing) => {
  if (missing === "observer") vi.stubGlobal("ResizeObserver", undefined);
  else if (missing === "CSS") vi.stubGlobal("CSS", undefined);
  else vi.stubGlobal("CSS", { supports: (property: string) =>
    property !== (missing === "visibility" ? "content-visibility" : "contain-intrinsic-block-size") });
  await render();
  const root = collection();
  const sections = rows(root);
  const list = await expand();
  const original = rows(list);
  const loose = episodes(unmatched());
  expect(Sizes.observers).toHaveLength(0);
  expect(Proximity.observers).toHaveLength(0);
  expect([...sections, ...original, ...rows(loose)].every((row) =>
    !row.style.contentVisibility && !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  const numbering = original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  numbering.focus();
  numbering.click();
  await settle();
  expect(document.querySelector("#library-episode-offset")).not.toBeNull();
  button("Cancel", document.querySelector('[role="dialog"]')!).click();
  await settle();
  expect(document.activeElement).toBe(numbering);
  expectRows(root, sections);
  expectRows(list, original);
  expect(mocks.openPath).not.toHaveBeenCalled();
});

test("retained Library groups leave episode remeasurement to their nested observer", async () => {
  await render();
  const root = collection();
  const section = group();
  const list = await expand();
  const original = rows(list);
  await drain();
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  proximity(root).deliver([[section, true]]);
  proximity(list).deliver([[original[1], true]]);
  await drain();
  const outer = observer(root);
  const inner = observer(list);
  outer.observe.mockClear();
  outer.unobserve.mockClear();
  inner.observe.mockClear();
  inner.unobserve.mockClear();
  const control = original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  control.focus();
  main.scrollTop = 1500;
  mocks.api.localEntryUpdates.mockResolvedValue([{ ...entry, progress: 2 }]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback({ payload: { media_ids: [1] } });
  await settleListUpdates();
  await drain();
  expect(original[0].querySelector('[title="Watched (per your list progress)"]')).not.toBeNull();
  expect(original[1].querySelector('[title="Watched (per your list progress)"]')).not.toBeNull();
  expect(outer.observe).not.toHaveBeenCalled();
  expect(outer.unobserve).not.toHaveBeenCalled();
  expect(inner.observe).toHaveBeenCalledExactlyOnceWith(original[0]);
  expect(inner.unobserve).toHaveBeenCalledExactlyOnceWith(original[0]);
  inner.deliver([[original[0], 720, 40]]);
  await drain();
  expect(original[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("40px");
  expect([section, original[1]].every((row) => row.style.contentVisibility === "visible" &&
    !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  expect(outer.targets.has(section)).toBe(false);
  expect(inner.targets.has(original[1])).toBe(false);
  expectRows(list, original);
  expect(document.activeElement).toBe(control);
  expect(main.scrollTop).toBe(1500);
  expect(mocks.openPath).not.toHaveBeenCalled();
});

test("nested Library observers stop scheduling on identical measurements and invalidate geometry on resize", async () => {
  await render();
  const root = collection();
  const list = await expand();
  await drain();
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  const sizes = Sizes.observers.map((sizes) => [sizes.observe.mock.calls.length, sizes.unobserve.mock.calls.length]);
  for (let i = 0; i < 3; i++) {
    measure(list, [32, 48, 64]);
    measure(root, [224, 80, 130]);
    expect(frames.size).toBe(0);
    await drain();
    expect(group().style.contentVisibility).toBe("auto");
  }
  expect(Sizes.observers.map((sizes) => [sizes.observe.mock.calls.length, sizes.unobserve.mock.calls.length])).toEqual(sizes);

  measure(list, [48, 64, 80], 400);
  measure(root, [272, 104, 162], 400);
  await drain();
  expect([...rows(root), ...rows(list)].every((row) => row.style.contentVisibility === "visible")).toBe(true);
  expect([...rows(root), ...rows(list)].every((row) => !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  measure(list, [48, 64, 80], 400);
  await drain();
  measure(root, [272, 104, 162], 400);
  await drain();
  expect(group().style.getPropertyValue("contain-intrinsic-block-size")).toBe("272px");
  expect(rows(list)[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("64px");
});

test("Library preserves focused nested controls and native wheel and keyboard events", async () => {
  await render();
  const root = collection();
  const list = await expand();
  const original = rows(list);
  await drain();
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  const control = original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  control.focus();
  expect(original[1].style.contentVisibility).toBe("visible");
  expect(group().style.contentVisibility).toBe("visible");
  await drain();
  main.scrollTop = 1500;
  expect(main.dispatchEvent(new WheelEvent("wheel", { deltaY: 1500, bubbles: true, cancelable: true }))).toBe(true);
  expect(control.dispatchEvent(new KeyboardEvent("keydown", { key: "PageDown", bubbles: true, cancelable: true }))).toBe(true);
  main.dispatchEvent(new Event("scroll"));
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  expect(document.activeElement).toBe(control);
  expect(main.scrollTop).toBe(1500);
  expectRows(list, original);
  expect(original[1].style.contentVisibility).toBe("visible");
  expect(group().style.contentVisibility).toBe("visible");
  expect(mocks.openPath).not.toHaveBeenCalled();
  button("+ Add folder").focus();
  await drain();
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  expect(original[1].style.contentVisibility).toBe("auto");
  expect(group().style.contentVisibility).toBe("auto");
});

test("reserved Library covers keep group geometry on load and remeasure after errors", async () => {
  await render();
  const root = collection();
  measure(root, [80, 80, 130]);
  await drain();
  const image = group().querySelector("img")!;
  expect(image.parentElement!.hasAttribute("data-measured-cover")).toBe(true);
  image.dispatchEvent(new Event("load"));
  await settle();
  expect(frames.size).toBe(0);
  expect(group().style.contentVisibility).toBe("auto");
  expect(group().style.getPropertyValue("contain-intrinsic-block-size")).toBe("80px");
  image.dispatchEvent(new Event("error"));
  await drain();
  expect(group().querySelector("img")).toBeNull();
  expect(group().style.contentVisibility).toBe("visible");
  expect(rows(root)[1].style.contentVisibility).toBe("auto");
  measure(root, [80, 80, 130]);
  await drain();
  expect(group().style.contentVisibility).toBe("auto");
});

test("Library retains keyed files and numbering edits through scans and progress updates and cleans up expansions", async () => {
  await render();
  const root = collection();
  const section = group();
  const list = await expand();
  const original = rows(list);
  await drain();
  measure(list, [32, 48, 64]);
  await drain();
  measure(root, [224, 80, 130]);
  await drain();
  const numbering = original[1].querySelector<HTMLButtonElement>('[title="Adjust episode numbering"]')!;
  numbering.click();
  await settle();
  const offset = document.querySelector<HTMLInputElement>("#library-episode-offset")!;
  offset.value = "-7";
  offset.dispatchEvent(new Event("input", { bubbles: true }));
  offset.focus();
  mocks.api.localEntryUpdates.mockResolvedValue([{ ...entry, progress: 1 }]);
  for (const callback of mocks.listeners.get("kurisu://pending-changed") ?? []) callback({ payload: { media_ids: [1] } });
  await settleListUpdates();
  await library.scan();
  await drain();
  expect(group()).toBe(section);
  expectRows(list, original);
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(document.activeElement).toBe(offset);
  expect(section.querySelector("[aria-expanded]")!.getAttribute("aria-expanded")).toBe("true");
  expect(original[0].querySelector('[title="Watched (per your list progress)"]')).not.toBeNull();
  mocks.api.scanLibrary.mockResolvedValueOnce({ files: [
    ...files.map((file) => file.path === "/synthetic/cedar-2.mkv" ? { ...file, episode: 4 } : { ...file }),
    { path: "/synthetic/cedar-3.mkv", media_id: 1, matched: "Cedar", episode: 3 },
  ], unreadable: [] });
  await library.scan();
  await drain();
  const reordered = rows(list);
  expect(reordered.map((row) => row.title)).toEqual([
    "Play cedar-1.mkv", "Play cedar-3.mkv", "Play cedar-2.mkv", "Play cedar-extra.mkv",
  ]);
  expectRows(list, [original[0], reordered[1], original[1], original[2]]);
  expect(original[1].querySelector('[title="Adjust episode numbering"]')).toBe(numbering);
  expect(original[1].textContent).toContain("Ep 4");
  expect(document.querySelector("#library-episode-offset")).toBe(offset);
  expect(offset.value).toBe("-7");
  expect(document.activeElement).toBe(offset);
  expect(section.style.contentVisibility).toBe("visible");
  measure(list, [32, 40, 48, 64]);
  await drain();
  measure(root, [264, 80, 130]);
  await drain();
  expect(section.style.getPropertyValue("contain-intrinsic-block-size")).toBe("264px");
  button("Cancel", document.querySelector('[role="dialog"]')!).click();
  await settle();
  const sizes = observer(list);
  section.querySelector<HTMLElement>("[aria-expanded]")!.click();
  await drain();
  expect(sizes.disconnect).toHaveBeenCalledOnce();
  expect(sizes.targets.size).toBe(0);
  expect(reordered.every((row) => !row.style.contentVisibility && !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  expect(observer(root).targets).toEqual(new Set([root, ...rows(root)]));
  const reopened = await expand(section);
  expect(rows(reopened).map((row) => row.title)).toEqual(reordered.map((row) => row.title));
  expect(observer(reopened)).not.toBe(sizes);
  await unmount(instance!);
  instance = undefined;
  expect(Sizes.observers.every((sizes) => sizes.targets.size === 0 && sizes.disconnect.mock.calls.length === 1)).toBe(true);
  expect(frames.size).toBe(0);
});
