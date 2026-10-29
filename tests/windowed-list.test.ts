import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import Harness from "./WindowedListHarness.svelte";
import { listLayout, rowAt, type WindowedGeometry } from "#lib/windowedList.ts";
import { rememberScroll } from "#lib/scroll.ts";
import { flushPendingEdits } from "#lib/pendingEdits.ts";
import type { ListEntry } from "#lib/types.ts";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  setProgress: vi.fn(), auth: { epoch: 0, isLoggedIn: true, changingSession: false },
}));
vi.mock("#lib/api.ts", () => ({ api: { setProgress: mocks.setProgress } }));
vi.mock("#lib/auth.svelte.ts", () => ({ auth: mocks.auth }));

class Sizes {
  static all: Sizes[] = [];
  targets = new Set<Element>();
  observe = (node: Element) => { this.targets.add(node); };
  unobserve = (node: Element) => { this.targets.delete(node); };
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: ResizeObserverCallback) { Sizes.all.push(this); }
  deliver(node: Element, height: number, width = 800) {
    expect(this.targets.has(node), "measurement must come from an active observation").toBe(true);
    this.notify(node, height, width);
  }
  deliverLate(node: Element, height: number, width = 800) {
    expect(this.targets.has(node)).toBe(false);
    this.notify(node, height, width);
  }
  private notify(node: Element, height: number, width: number) {
    if (node instanceof HTMLElement && node.hasAttribute("data-windowed-id")) geometry.set(Number(node.dataset.windowedId), height);
    this.callback([{ target: node, borderBoxSize: [{ blockSize: height }], contentRect: { height, width } } as unknown as ResizeObserverEntry], this as unknown as ResizeObserver);
  }
}

const items = Array.from({ length: 2000 }, (_, index): ListEntry => ({
  media_id: index + 1, status: "CURRENT", progress: 3, repeat: 0,
  media: { id: index + 1, title_english: `Title ${index + 1}` },
}));
let instance: ReturnType<typeof mount> | undefined;
let replace: (items: ListEntry[]) => void;
let main: HTMLElement;
let top = 0;
const geometry = new Map<number, number>();
let frames = new Map<number, FrameRequestCallback>();
let memory: ReturnType<typeof rememberScroll> | undefined;
let originalScrollIntoView: PropertyDescriptor | undefined;
const rows = () => [...document.querySelectorAll<HTMLElement>("[data-id]")];
const row = (id: number) => document.querySelector<HTMLElement>(`[data-id="${id}"]`);
const wrapper = (id: number) => document.querySelector<HTMLElement>(`[data-windowed-id="${id}"]`)!;

async function frame() {
  const callbacks = [...frames.values()];
  frames.clear();
  for (const callback of callbacks) callback(0);
  await settle();
}

async function render(list = items, stepper = false) {
  const target = document.createElement("div");
  document.body.append(target);
  instance = mount(Harness, { target, props: { items: list, stepper } });
  replace = (instance as { replace: typeof replace }).replace;
  await settle();
  main = document.querySelector("main")!;
  return instance as { capture: () => WindowedGeometry; restore: (saved: WindowedGeometry) => void; show: (visible: boolean) => void };
}

async function scroll(position: number) {
  main.scrollTop = position;
  main.dispatchEvent(new Event("scroll"));
  await frame();
}

beforeEach(() => {
  vi.useFakeTimers();
  Sizes.all = [];
  top = 0;
  geometry.clear();
  frames = new Map();
  let next = 0;
  vi.stubGlobal("ResizeObserver", Sizes);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => { frames.set(++next, callback); return next; });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("CSS", { supports: () => false });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
    return { top: this.hasAttribute("data-windowed-list") ? 150 - top : 0, height: this.hasAttribute("data-windowed-id") ? geometry.get(Number(this.dataset.windowedId)) ?? 90 : 600, width: 800 } as DOMRect;
  });
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(600);
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
    return 150 + [...this.querySelectorAll<HTMLElement>("[data-windowed-list] > div")].reduce((sum, node) => sum + (node.hasAttribute("data-windowed-id") ? geometry.get(Number(node.dataset.windowedId)) ?? 90 : parseFloat(node.style.height) || 0), 0);
  });
  vi.spyOn(HTMLElement.prototype, "scrollTop", "get").mockImplementation(() => top);
  vi.spyOn(HTMLElement.prototype, "scrollTop", "set").mockImplementation(function (this: HTMLElement, value) { top = Math.max(0, Math.min(value, this.scrollHeight - 600)); });
  originalScrollIntoView = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollIntoView");
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value: vi.fn() });
  mocks.auth.isLoggedIn = true;
  mocks.setProgress.mockReset().mockResolvedValue({ ...items[0], progress: 4 });
});

afterEach(async () => {
  memory?.destroy();
  memory = undefined;
  mocks.auth.epoch++;
  if (instance) await unmount(instance);
  instance = undefined;
  await flushPendingEdits();
  document.body.replaceChildren();
  vi.restoreAllMocks();
  if (originalScrollIntoView) Object.defineProperty(HTMLElement.prototype, "scrollIntoView", originalScrollIntoView);
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test.each([2000, 10000])("%i titles keep a bounded DOM through scrolling and expose the full scroll extent", async (count) => {
  const list = Array.from({ length: count }, (_, index) => ({ ...items[0], media_id: index + 1 }));
  await render(list);
  expect(rows().length).toBeLessThan(40);
  expect(main.scrollHeight).toBe(count * 90 + 150);
  for (const position of [6000, 40000, count * 90]) {
    await scroll(position);
    expect(rows().length).toBeLessThan(40);
    expect(main.scrollHeight).toBe(count * 90 + 150);
  }
  expect(row(count)).not.toBeNull();
  expect(row(1)).toBeNull();
  await scroll(0);
  expect(row(1)).not.toBeNull();
  expect(row(count)).toBeNull();
});

test("measurement changes preserve the visible anchor and identical sizes schedule no work", async () => {
  await render();
  await scroll(9150);
  const before = main.scrollTop;
  const target = wrapper(95);
  Sizes.all[0].deliver(target, 150);
  await frame();
  expect(main.scrollTop).toBe(before + 60);
  expect(main.scrollHeight).toBe(180210);
  Sizes.all[0].deliver(target, 150);
  expect(frames.size).toBe(0);
});

test("scrolling keeps the original observers and listeners connected", async () => {
  await render();
  const observers = Sizes.all.slice();
  expect(observers).toHaveLength(2);
  for (const position of [9150, 40000, 2000]) await scroll(position);
  expect(Sizes.all).toEqual(observers);
  expect(observers.every((observer) => observer.disconnect.mock.calls.length === 0)).toBe(true);
  expect(observers[1].targets.has(main)).toBe(true);
  expect(main.style.overflowAnchor).toBe("");
  expect(document.querySelector<HTMLElement>("[data-windowed-list]")!.style.overflowAnchor).toBe("none");
});

test("the latest size wins when a row returns to its cached height before a frame", async () => {
  await render();
  await scroll(9150);
  const target = wrapper(95);
  Sizes.all[0].deliver(target, 150);
  Sizes.all[0].deliver(target, 90);
  await frame();
  expect(main.scrollTop).toBe(9150);
  expect(main.scrollHeight).toBe(180150);
  expect((instance as { capture: () => WindowedGeometry }).capture().heights).not.toContainEqual([95, 150]);
});

test("focused and pending controls stay mounted until their save finishes", async () => {
  const saved = deferred<ListEntry>();
  mocks.setProgress.mockReturnValue(saved.promise);
  await render(items, true);
  const first = row(1)!;
  const increment = first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  increment.focus();
  increment.click();
  await settle();
  await scroll(50000);
  expect(row(1)).toBe(first);
  expect(document.activeElement).toBe(increment);
  document.querySelector<HTMLButtonElement>("[data-before]")!.focus();
  await settle();
  await vi.advanceTimersByTimeAsync(3000);
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(row(1)).toBe(first);
  saved.resolve({ ...items[0], progress: 4 });
  await settle();
  expect(row(1)).toBeNull();
});

test("Tab across a virtual boundary mounts the logical next row and Shift Tab returns", async () => {
  await render();
  const last = rows().at(-1)!;
  const id = Number(last.dataset.id);
  const source = last.querySelector("button")!;
  source.focus();
  const key = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
  source.dispatchEvent(key);
  await settle();
  expect(key.defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(row(id + 1));
  expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalled();
  await scroll(60000);
  const current = row(id + 1)!;
  current.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }));
  await settle();
  expect(document.activeElement).toBe(row(id)!.querySelector("button"));
});

test("a reordered list cancels an obsolete Tab destination before mounting settles", async () => {
  await render();
  const last = rows().at(-1)!;
  const id = Number(last.dataset.id);
  const source = last.querySelector("button")!;
  source.focus();
  source.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  const next = items.slice();
  [next[id], next[id + 1]] = [next[id + 1], next[id]];
  replace(next);
  await settle();
  expect(document.activeElement).toBe(source);
  expect(HTMLElement.prototype.scrollIntoView).not.toHaveBeenCalled();
});

test.each(["wheel", "pointerdown", "touchstart", "Escape", "ArrowDown", "End"])("%s cancels a queued virtual Tab move", async (type) => {
  await render();
  const source = rows().at(-1)!.querySelector("button")!;
  source.focus();
  source.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  if (["wheel", "pointerdown", "touchstart"].includes(type)) main.dispatchEvent(new Event(type));
  else source.dispatchEvent(new KeyboardEvent("keydown", { key: type, bubbles: true, cancelable: true }));
  await settle();
  expect(document.activeElement).toBe(source);
  expect(HTMLElement.prototype.scrollIntoView).not.toHaveBeenCalled();
});

test("keyed moves preserve a focused control and its buffered edit outside the viewport", async () => {
  await render(items, true);
  const first = row(1)!;
  const control = first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  control.focus();
  control.click();
  await settle();
  replace([...items.slice(1), items[0]]);
  await settle();
  expect(row(1)).toBe(first);
  expect(document.activeElement).toBe(control);
  await vi.advanceTimersByTimeAsync(3000);
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(document.activeElement).toBe(control);
});

test("End and Home reach both ends without mounting the full list", async () => {
  await render();
  row(1)!.focus();
  row(1)!.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true }));
  await frame();
  expect(row(2000)).not.toBeNull();
  row(2000)!.focus();
  row(2000)!.dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true, cancelable: true }));
  await frame();
  expect(main.scrollTop).toBe(0);
  expect(row(1)).not.toBeNull();
  expect(rows().length).toBeLessThan(40);
});

test("End keeps the final row visible as its window reports taller rows", async () => {
  await render();
  row(1)!.focus();
  row(1)!.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true }));
  await frame();
  for (const node of document.querySelectorAll<HTMLElement>("[data-windowed-id]")) {
    if (node.dataset.windowedId !== "1") Sizes.all[0].deliver(node, 180);
  }
  await frame();
  expect(main.scrollTop).toBe(main.scrollHeight - main.clientHeight);
  expect(row(2000)).not.toBeNull();
  expect(rows().length).toBeLessThan(40);
});

test("reordering and filtering update entries while retaining keyed controls", async () => {
  await render(items, true);
  const first = row(1)!;
  const control = first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  control.click();
  await settle();
  replace([{ ...items[1], progress: 8 }, { ...items[0], media: { ...items[0].media!, title_english: "Renamed" } }]);
  await settle();
  expect(row(1)).toBe(first);
  expect(first.textContent).toContain("Renamed");
  expect(row(2)?.textContent).toContain("8");
  expect(rows().map((node) => node.dataset.id)).toEqual(["2", "1"]);
  await vi.advanceTimersByTimeAsync(3000);
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
});

test("scroll memory restores a deep position without staging every preceding row", async () => {
  await render();
  memory = rememberScroll(main);
  memory.restore({ left: 0, top: 40000 });
  main.dispatchEvent(new Event("scroll"));
  await frame();
  expect(main.scrollTop).toBe(40000);
  expect(rows().length).toBeLessThan(40);
  expect(row(443)).not.toBeNull();
});

test("scroll restoration and a remounted list keep separate anchoring ownership", async () => {
  const list = await render();
  list.show(false);
  await settle();
  memory = rememberScroll(main);
  memory.restore({ left: 0, top: 40000 });
  expect(main.style.overflowAnchor).toBe("none");
  list.show(true);
  await settle();
  await frame();
  await frame();
  await frame();
  expect(main.scrollTop).toBe(40000);
  expect.soft(main.style.overflowAnchor).toBe("");
  expect.soft(document.querySelector<HTMLElement>("[data-windowed-list]")!.style.overflowAnchor).toBe("none");
  list.show(false);
  await settle();
  expect(main.style.overflowAnchor).toBe("");
});

test("history restores measured geometry and its logical anchor after remount", async () => {
  await render();
  await scroll(9150);
  Sizes.all[0].deliver(wrapper(95), 150);
  await frame();
  const oldTop = main.scrollTop;
  const saved = (instance as { capture: () => WindowedGeometry }).capture();
  await unmount(instance!);
  instance = undefined;
  top = 0;
  const remounted = await render();
  remounted.restore(saved);
  await settle();
  await frame();
  expect(main.scrollTop).toBe(oldTop);
  expect(row(101)).not.toBeNull();
});

test.each(["wheel", "pointerdown", "touchstart", "Home"])("%s cancels a queued history anchor before its frame", async (type) => {
  const list = await render();
  list.restore({ width: 800, heights: [], anchor: { id: 101, offset: 0 } });
  await settle();
  if (type === "Home") row(1)!.dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true, cancelable: true }));
  else {
    main.dispatchEvent(new Event(type));
    main.scrollTop = 2000;
  }
  await frame();
  expect(main.scrollTop).toBe(type === "Home" ? 0 : 2000);
});

test("failed saves stay pinned and account changes cannot submit the old edit", async () => {
  const failure = vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.setProgress.mockRejectedValueOnce("offline");
  await render(items, true);
  const first = row(1)!;
  first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  await scroll(40000);
  await vi.advanceTimersByTimeAsync(3000);
  expect(row(1)).toBe(first);
  expect(first.querySelector('[title="Not saved: offline"]')).not.toBeNull();
  mocks.auth.epoch++;
  replace([]);
  await settle();
  await flushPendingEdits();
  expect(mocks.setProgress).toHaveBeenCalledOnce();
  failure.mockRestore();
});

test("width changes keep the current row anchored and request fresh measurements", async () => {
  await render();
  await scroll(9150);
  Sizes.all[0].deliver(wrapper(95), 150);
  await frame();
  Sizes.all[1].deliver(main, 600, 800);
  await frame();
  Sizes.all[1].deliver(main, 600, 600);
  await frame();
  expect(row(101)).not.toBeNull();
  Sizes.all[0].deliver(wrapper(95), 180, 600);
  await frame();
  expect(main.scrollTop).toBe(9240);
});

test("a hidden width does not hide a later change in row wrapping", async () => {
  await render();
  await scroll(9150);
  Sizes.all[0].deliver(wrapper(95), 150);
  await frame();
  Sizes.all[1].deliver(main, 600, 800);
  await frame();
  await scroll(40000);
  Sizes.all[1].deliver(main, 0, 0);
  await frame();
  Sizes.all[1].deliver(main, 600, 600);
  await frame();
  expect((instance as { capture: () => WindowedGeometry }).capture().heights).not.toContainEqual([95, 150]);
});

test("late measurements from a removed row cannot change the scroll geometry", async () => {
  await render();
  const removed = wrapper(1);
  await scroll(40000);
  expect(removed.isConnected).toBe(false);
  Sizes.all[0].deliverLate(removed, 180);
  await frame();
  expect(main.scrollTop).toBe(40000);
  expect(main.scrollHeight).toBe(180150);
  expect((instance as { capture: () => WindowedGeometry }).capture().heights).not.toContainEqual([1, 180]);
});

test("teardown disconnects observers, cancels frames and ignores late measurements", async () => {
  await render();
  const target = wrapper(1);
  main.dispatchEvent(new Event("scroll"));
  await unmount(instance!);
  instance = undefined;
  expect(frames.size).toBe(0);
  expect(Sizes.all.every((observer) => observer.disconnect.mock.calls.length > 0)).toBe(true);
  Sizes.all[0].deliverLate(target, 150);
  expect(frames.size).toBe(0);
  expect(main.style.overflowAnchor).toBe("");
});

test("a disconnected container observer ignores a late width change", async () => {
  await render();
  Sizes.all[1].deliver(main, 600, 800);
  await frame();
  await unmount(instance!);
  instance = undefined;
  expect(() => Sizes.all[1].deliverLate(main, 600, 600)).not.toThrow();
  expect(frames.size).toBe(0);
});

test("without ResizeObserver the staged fallback mounts every row and cleans up", async () => {
  vi.stubGlobal("ResizeObserver", undefined);
  await render(items.slice(0, 96));
  expect(document.querySelector("[data-windowed-list]")).toBeNull();
  expect(rows()).toHaveLength(32);
  await frame();
  await frame();
  expect(rows()).toHaveLength(96);
  expect(document.querySelector("[data-pending-rows]")).toBeNull();
  expect(Sizes.all).toHaveLength(0);
  expect(frames.size).toBe(0);
});

test("layout resolves variable heights, exact boundaries and missing cached sizes", () => {
  const layout = listLayout(items.slice(0, 3), new Map([[2, 150]]));
  expect(layout.offsets).toEqual([0, 90, 240, 330]);
  expect([-1, 0, 89, 90, 239, 240, 330, 900].map((position) => rowAt(layout.offsets, position))).toEqual([0, 0, 0, 1, 1, 2, 2, 2]);
});
