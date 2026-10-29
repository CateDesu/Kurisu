import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount, type Component } from "svelte";
import { auth } from "./session.svelte";
import { button, deferred, selectOption, settle } from "./helpers";
import type { ListEntry, Media, SearchPage } from "#lib/types.ts";

const mocks = vi.hoisted(() => ({
  api: { localEntries: vi.fn(), searchAnimePage: vi.fn(), getSeason: vi.fn(), updateEntry: vi.fn() },
  navigation: [] as (() => void)[],
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/auth.svelte.ts", async () => import("./session.svelte"));
vi.mock("$app/navigation", () => ({ goto: vi.fn(), afterNavigate: (callback: () => void) => mocks.navigation.push(callback) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}), emit: vi.fn() }));

import Search from "../src/routes/search/+page.svelte";
import Seasons from "../src/routes/seasons/+page.svelte";

class Sizes {
  static observers: Sizes[] = [];
  targets = new Set<Element>();
  observe = (target: Element) => { this.targets.add(target); };
  unobserve = (target: Element) => { this.targets.delete(target); };
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: ResizeObserverCallback) { Sizes.observers.push(this); }
  deliver(entries: Array<[Element, number, number]>) {
    this.callback(entries.map(([target, width, height]) => ({
      target, contentRect: { width, height },
    })) as ResizeObserverEntry[], this as unknown as ResizeObserver);
  }
}

const items: Media[] = [
  { id: 1, title_english: "Cedar Show", episodes: 12, format: "TV" },
  { id: 2, title_english: "Amber Film", episodes: 1, format: "MOVIE" },
  { id: 3, title_english: "Bamboo Show", episodes: 12, format: "TV" },
];
const routes = [["search", Search], ["seasons", Seasons]] as const;
let instance: ReturnType<typeof mount> | undefined;
let main: HTMLElement;
let frames: Map<number, FrameRequestCallback>;
let nextFrame: number;

function input(selector: string, value: string) {
  const element = document.querySelector<HTMLInputElement>(selector)!;
  element.value = value;
  element.dispatchEvent(new Event("input", { bubbles: true }));
}

async function render<C extends Component<any>>(component: C) {
  main = document.createElement("main");
  main.style.cssText = "overflow: auto; height: 300px";
  document.body.append(main);
  instance = mount(component, { target: main });
  await settle();
  for (const callback of mocks.navigation) callback();
  await settle();
  if (Object.is(component, Search)) {
    input('[aria-label="Anime title"]', "show");
    button("Search").click();
    await settle();
  }
  return instance as ReturnType<C>;
}

function grid() { return document.querySelector<HTMLDivElement>(".media-grid")!; }
function cards() { return [...document.querySelectorAll<HTMLDivElement>(".anime-card")]; }
function observer(root: Element) { return Sizes.observers.find((sizes) => sizes.targets.has(root))!; }

function expectCards(root: HTMLElement, expected: HTMLElement[]) {
  expect(root.children).toHaveLength(expected.length);
  expected.forEach((card, index) => expect(root.children[index]).toBe(card));
}

async function frame() {
  const pending = [...frames.values()];
  frames.clear();
  for (const callback of pending) callback(0);
  await settle();
}

async function measure(root: HTMLElement) {
  observer(root).deliver([
    [root, 720, 700],
    ...[...root.children].map((card, index) => [card, 232, 288 + index * 24] as [Element, number, number]),
  ]);
  await frame();
}

beforeEach(() => {
  vi.resetAllMocks();
  mocks.navigation.length = 0;
  mocks.api.localEntries.mockResolvedValue([]);
  mocks.api.searchAnimePage.mockResolvedValue({ items, page: 1, has_next_page: false });
  mocks.api.getSeason.mockResolvedValue(items);
  auth.epoch++;
  auth.isLoggedIn = true;
  auth.offline = false;
  auth.user = { id: 1, name: "Example", score_format: "POINT_100" };
  Sizes.observers = [];
  frames = new Map();
  nextFrame = 0;
  vi.stubGlobal("CSS", { supports: () => true });
  vi.stubGlobal("ResizeObserver", Sizes);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
});

afterEach(async () => {
  if (instance) await unmount(instance);
  instance = undefined;
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

test.each(routes)("%s measures direct grid cards before containment and preserves focus during scrolling", async (_, component) => {
  await render(component);
  const root = grid();
  const original = cards();
  expectCards(root, original);
  expect(original).toHaveLength(items.length);
  expect(original.every((card) => !card.style.contentVisibility)).toBe(true);
  expect(Sizes.observers).toHaveLength(1);
  observer(root).deliver([[root, 720, 700], [original[0], 232, 288.25], [original[1], 232, 344.5]]);
  expect(original[0].style.contentVisibility).toBe("");
  await frame();
  expect(original[0].style.contentVisibility).toBe("auto");
  expect(original[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("288.25px");
  expect(original[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("344.5px");
  expect(original[2].style.contentVisibility).toBe("");

  const title = button("Cedar Show", original[0]);
  title.focus();
  expect(original[0].style.contentVisibility).toBe("visible");
  main.scrollTop = 1500;
  expect(main.dispatchEvent(new WheelEvent("wheel", { deltaY: 1500, cancelable: true }))).toBe(true);
  expect(title.dispatchEvent(new KeyboardEvent("keydown", { key: "PageDown", bubbles: true, cancelable: true }))).toBe(true);
  main.dispatchEvent(new Event("scroll"));
  await frame();
  await measure(root);
  expectCards(root, original);
  expect(document.activeElement).toBe(title);
  expect(original[0].style.contentVisibility).toBe("visible");
  expect(main.scrollTop).toBe(1500);
});

test.each(routes)("%s reserved cover loads keep measurements and image errors still remeasure", async (_, component) => {
  const covered = items.map((item) => ({ ...item, cover_large: `cover-${item.id}.jpg` }));
  mocks.api.searchAnimePage.mockResolvedValue({ items: covered, page: 1, has_next_page: false });
  mocks.api.getSeason.mockResolvedValue(covered);
  await render(component);
  const root = grid();
  const original = cards();
  await measure(root);
  const heights = original.map((card) => card.style.getPropertyValue("contain-intrinsic-block-size"));
  const images = [...root.querySelectorAll("img")];
  expect(images).toHaveLength(items.length);
  for (const image of images) image.dispatchEvent(new Event("load"));
  expect(frames.size).toBe(0);
  expect(original.every((card) => card.style.contentVisibility === "auto")).toBe(true);
  expect(original.map((card) => card.style.getPropertyValue("contain-intrinsic-block-size"))).toEqual(heights);

  images[0].dispatchEvent(new Event("error"));
  await settle();
  await frame();
  expect(original[0].querySelector("img")).toBeNull();
  expect(original[0].style.contentVisibility).toBe("visible");
  expect(original[1].style.contentVisibility).toBe("auto");
  await measure(root);
  expect(original[0].style.contentVisibility).toBe("auto");
});

test("search paging retries retain cards and append unique results in response order", async () => {
  const updated = { ...items[1], title_english: "Amber Film with a longer title" };
  mocks.api.searchAnimePage.mockResolvedValueOnce({ items: items.slice(0, 2), page: 1, has_next_page: true })
    .mockRejectedValueOnce("Try again")
    .mockResolvedValueOnce({ items: [updated, items[2]], page: 2, has_next_page: false });
  const mounted = await render(Search);
  const root = grid();
  const original = cards();
  await measure(root);
  const title = button("Cedar Show", original[0]);
  title.focus();
  button("Load more anime").click();
  await settle();
  expectCards(root, original);
  expect(document.body.textContent).toContain("Try again");
  button("Load more anime").click();
  await settle();
  await frame();
  const appended = cards()[2];
  expectCards(root, [...original, appended]);
  expect(appended.style.contentVisibility).toBe("");
  expect(document.activeElement).toBe(title);
  expect(mocks.api.searchAnimePage.mock.calls).toEqual([["show", 1], ["show", 2], ["show", 2]]);
  expect(mounted.snapshot.capture()).toMatchObject({ results: [items[0], updated, items[2]], resultPage: 2, hasMore: false });
  expect(document.body.textContent).not.toContain("Load more anime");
  await measure(root);
  expect(appended.style.contentVisibility).toBe("auto");
  expect(observer(root).targets).toEqual(new Set([root, ...cards()]));
});

test("season filters and restored ordering keep cards as direct grid children", async () => {
  mocks.api.localEntries.mockResolvedValue([{ media_id: 1, status: "CURRENT", progress: 2, repeat: 0, media: items[0] }]);
  const mounted = await render(Seasons);
  const root = grid();
  const original = cards();
  await measure(root);
  input('[aria-label="Filter season titles"]', "Show");
  selectOption("#season-format", "TV");
  selectOption("#season-list-status", "UNLISTED");
  await settle();
  await frame();
  expectCards(root, [original[2]]);
  expect(observer(root).targets).toEqual(new Set([root, original[2]]));
  const snapshot = mounted.snapshot.capture();
  mounted.snapshot.restore({ ...snapshot, media: [items[2], items[0]], listFilter: "ALL" });
  await settle();
  await frame();
  expect(root.children[0]).toBe(original[2]);
  expect(cards().map((card) => card.querySelectorAll("button")[1].textContent?.trim())).toEqual(["Bamboo Show", "Cedar Show"]);
  expectCards(root, cards());
  expect(mocks.api.getSeason).toHaveBeenCalledOnce();
  const sizes = observer(root);
  input('[aria-label="Filter season titles"]', "absent");
  await settle();
  expect(grid()).toBeNull();
  expect(sizes.disconnect).toHaveBeenCalledOnce();
  expect(sizes.targets.size).toBe(0);
  input('[aria-label="Filter season titles"]', "Show");
  await settle();
  expect(cards()).toHaveLength(2);
  await measure(grid());
  expect(cards().every((card) => card.style.contentVisibility === "auto")).toBe(true);
});

test.each(routes)("%s keeps pending list additions mounted and refreshes their badge", async (_, component) => {
  const save = deferred<ListEntry>();
  mocks.api.updateEntry.mockReturnValueOnce(save.promise);
  await render(component);
  const root = grid();
  const original = cards();
  await measure(root);
  button("Watching", original[0]).click();
  await settle();
  await frame();
  expect(button("Watching", original[0]).disabled).toBe(true);
  expect(button("Watching", original[1]).disabled).toBe(false);
  main.scrollTop = 1500;
  main.dispatchEvent(new Event("scroll"));
  await frame();
  main.scrollTop = 0;
  main.dispatchEvent(new Event("scroll"));
  expectCards(root, original);
  expect(button("Watching", original[0]).disabled).toBe(true);
  const entry: ListEntry = { media_id: 1, status: "CURRENT", progress: 2, repeat: 0, media: items[0] };
  mocks.api.localEntries.mockResolvedValue([entry]);
  save.resolve(entry);
  await settle();
  await frame();
  expect(mocks.api.updateEntry).toHaveBeenCalledExactlyOnceWith(1, "CURRENT", null, null, null);
  expectCards(root, original);
  expect(original[0].textContent).toMatch(/Watching · 2\/12/);
  await measure(root);
  expect(original[0].style.contentVisibility).toBe("auto");
});

test.each(routes)("%s snapshot restoration rejects a stale response without replacing the restored card", async (_, component) => {
  const response = deferred<Media[] | SearchPage>();
  mocks.api.searchAnimePage.mockReturnValueOnce(response.promise);
  mocks.api.getSeason.mockReturnValueOnce(response.promise);
  const mounted = await render(component);
  if (component === Search) {
    (mounted as ReturnType<typeof Search>).snapshot.restore({
      query: "restored", results: [items[2]], submittedQuery: "restored", resultPage: 2,
      hasMore: false, searched: true, pending: null,
    });
  } else {
    (mounted as ReturnType<typeof Seasons>).snapshot.restore({
      season: "SPRING", year: 2001, media: [items[2]], titleFilter: "", formatFilter: "ALL", listFilter: "ALL",
    });
  }
  await settle();
  const root = grid();
  const restored = cards()[0];
  await measure(root);
  response.resolve(component === Search ? { items: [items[0]], page: 1, has_next_page: false } : [items[0]]);
  await settle();
  await frame();
  expectCards(root, [restored]);
  expect(restored.textContent).toContain("Bamboo Show");
  expect(document.body.textContent).not.toContain("Cedar Show");
  expect(restored.style.contentVisibility).toBe("auto");
});
