import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import Harness from "./MeasuredListHarness.svelte";
import { measuredRows } from "#lib/measuredRows.ts";
import { flushPendingEdits } from "#lib/pendingEdits.ts";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  setProgress: vi.fn(),
  auth: { epoch: 0, isLoggedIn: true, changingSession: false },
}));
vi.mock("#lib/api.ts", () => ({ api: { setProgress: mocks.setProgress } }));
vi.mock("#lib/auth.svelte.ts", () => ({ auth: mocks.auth }));

class Sizes {
  static observers: Sizes[] = [];
  targets = new Set<Element>();
  observe = vi.fn((target: Element) => { this.targets.add(target); });
  unobserve = vi.fn((target: Element) => { this.targets.delete(target); });
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: ResizeObserverCallback) {
    Sizes.observers.push(this);
  }
  deliver(entries: Array<[Element, number, number]>) {
    this.callback(entries.map(([target, width, height]) => ({
      target, contentRect: { width, height },
    })) as ResizeObserverEntry[], this as unknown as ResizeObserver);
  }
}

class Proximity {
  static observers: Proximity[] = [];
  targets = new Set<Element>();
  observe = vi.fn((target: Element) => this.targets.add(target));
  unobserve = vi.fn((target: Element) => this.targets.delete(target));
  disconnect = vi.fn(() => this.targets.clear());
  constructor(readonly callback: IntersectionObserverCallback, readonly options: IntersectionObserverInit) {
    Proximity.observers.push(this);
  }
  deliver(entries: Array<[Element, boolean]>) {
    this.callback(entries.map(([target, isIntersecting]) => ({ target, isIntersecting })) as unknown as IntersectionObserverEntry[],
      this as unknown as IntersectionObserver);
  }
}

let frames: Map<number, FrameRequestCallback>;
let nextFrame: number;
let instance: ReturnType<typeof mount> | undefined;
let action: ReturnType<typeof measuredRows>;
let rows: HTMLDivElement[];
let root: HTMLDivElement;
let main: HTMLElement;
const items = Array.from({ length: 100 }, (_, index) => ({ media_id: index + 1, title: `Title ${index + 1}` }));

async function frame() {
  const pending = [...frames.values()];
  frames.clear();
  for (const callback of pending) callback(0);
  await settle();
}

async function render(stepper = false) {
  const target = document.createElement("div");
  document.body.append(target);
  instance = mount(Harness, { target, props: { items, stepper } });
  await settle();
  rows = [...document.querySelectorAll<HTMLDivElement>("[data-id]")];
  root = rows[0].parentElement as HTMLDivElement;
  main = document.querySelector("main")!;
  return instance as { replace: (next: typeof items) => void };
}

function renderRows(count: number, covers = false) {
  root = document.createElement("div");
  rows = Array.from({ length: count }, () => {
    const row = document.createElement("div");
    row.innerHTML = `${covers ? '<button data-measured-cover><img width="46" height="63"></button>' : ""}<span>Title</span>`;
    return row;
  });
  root.append(...rows);
  document.body.append(root);
  action = measuredRows(root);
}

function measure(height = 90, width = 600) {
  Sizes.observers[0].deliver([[root, width, rows.length * height], ...rows.map((row) => [row, width, height] as [Element, number, number])]);
}

beforeEach(() => {
  frames = new Map();
  nextFrame = 0;
  Sizes.observers = [];
  Proximity.observers = [];
  vi.stubGlobal("CSS", { supports: vi.fn(() => true) });
  vi.stubGlobal("ResizeObserver", Sizes);
  vi.stubGlobal("IntersectionObserver", undefined);
  vi.stubGlobal("requestAnimationFrame", vi.fn((callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  }));
  vi.stubGlobal("cancelAnimationFrame", vi.fn((id: number) => frames.delete(id)));
  mocks.setProgress.mockReset().mockResolvedValue({ media_id: 1, progress: 4 });
});

afterEach(async () => {
  mocks.auth.epoch++;
  if (instance) await unmount(instance);
  instance = undefined;
  action?.destroy();
  action = undefined;
  await flushPendingEdits();
  document.body.replaceChildren();
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

test("keeps natural variable heights until real measurements arrive", async () => {
  await render();
  expect(rows).toHaveLength(100);
  expect(rows.every((row) => !row.style.contentVisibility)).toBe(true);
  Sizes.observers[0].deliver([[root, 600, 9000], [rows[0], 600, 90.25], [rows[1], 600, 144.5]]);
  expect(rows[0].style.contentVisibility).toBe("");
  await frame();
  expect(rows[0].style.contentVisibility).toBe("auto");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90.25px");
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("144.5px");
  expect(rows[2].style.contentVisibility).toBe("");
});

test("batches measurements without reading layout or changing the main scroller", async () => {
  await render();
  const mainStyle = main.getAttribute("style");
  const rect = vi.spyOn(Element.prototype, "getBoundingClientRect");
  const listener = vi.spyOn(main, "addEventListener");
  const top = vi.spyOn(main, "scrollTop", "set");
  for (let i = 0; i < 20; i++) measure(90 + i);
  expect(frames.size).toBe(1);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("109px");
  expect(rect).not.toHaveBeenCalled();
  expect(top).not.toHaveBeenCalled();
  expect(listener).not.toHaveBeenCalled();
  expect(main.getAttribute("style")).toBe(mainStyle);
});

test("rapid wheel and keyboard scrolling keep every row and its measured space", async () => {
  await render();
  measure();
  await frame();
  for (const scrollTop of [7000, 90, 8800, 0, 4300]) {
    main.scrollTop = scrollTop;
    expect(main.dispatchEvent(new WheelEvent("wheel", { deltaY: 900, cancelable: true }))).toBe(true);
    expect(main.dispatchEvent(new KeyboardEvent("keydown", { key: "PageDown", bubbles: true, cancelable: true }))).toBe(true);
    main.dispatchEvent(new Event("scroll"));
  }
  await frame();
  expect([...root.children]).toEqual(rows);
  expect(rows.every((row) => row.style.getPropertyValue("contain-intrinsic-block-size") === "90px")).toBe(true);
  expect(main.scrollTop).toBe(4300);
});

test("unchanged observations do not schedule frames or rewrite measured styles", async () => {
  renderRows(1000);
  measure();
  await frame();
  vi.mocked(requestAnimationFrame).mockClear();
  const writes = vi.spyOn(CSSStyleDeclaration.prototype, "setProperty");
  const rect = vi.spyOn(Element.prototype, "getBoundingClientRect");
  for (let i = 0; i < 10; i++) {
    measure();
    await frame();
  }
  expect(requestAnimationFrame).not.toHaveBeenCalled();
  expect(writes).not.toHaveBeenCalled();
  expect(rect).not.toHaveBeenCalled();
  expect(rows.every((row) => row.style.contentVisibility === "auto")).toBe(true);
  Sizes.observers[0].deliver([[rows[0], 600, 120]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("120px");
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
});

test("a later observation can cancel a pending size change", async () => {
  renderRows(2);
  measure();
  await frame();
  const writes = vi.spyOn(CSSStyleDeclaration.prototype, "setProperty");
  Sizes.observers[0].deliver([[rows[0], 600, 120], [rows[0], 600, 90]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
  expect(writes).not.toHaveBeenCalled();
  Sizes.observers[0].deliver([[rows[0], 600, 90], [rows[0], 600, 144]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("144px");
});

test("nearby rows stay rendered after leaving the preparation margin", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  renderRows(3);
  measure();
  await frame();
  const rect = vi.spyOn(Element.prototype, "getBoundingClientRect");
  const proximity = Proximity.observers[0];
  expect(proximity.options).toEqual({ root: null, rootMargin: "800px 0px" });
  proximity.deliver([[rows[1], true]]);
  await frame();
  expect(rows[1].style.contentVisibility).toBe("visible");
  expect(rows[0].style.contentVisibility).toBe("auto");
  expect(proximity.unobserve).toHaveBeenCalledExactlyOnceWith(rows[1]);
  expect(proximity.targets).toEqual(new Set([rows[0], rows[2]]));
  proximity.deliver([[rows[1], false]]);
  Sizes.observers[0].deliver([[rows[1], 600, 120]]);
  await frame();
  expect(rows[1].style.contentVisibility).toBe("visible");
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  rows[1].firstChild!.textContent = "Changed after returning";
  await settle();
  await frame();
  Sizes.observers[0].deliver([[rows[1], 600, 145]]);
  await frame();
  expect(rows[1].style.contentVisibility).toBe("visible");
  expect(rows[0].style.contentVisibility).toBe("auto");
  expect(rect).not.toHaveBeenCalled();
  action!.destroy();
  action = undefined;
  expect(proximity.disconnect).toHaveBeenCalledOnce();
  expect(rows.every((row) => !row.style.contentVisibility)).toBe(true);
});

test("1000 retained rows need no remeasurement across ten content updates", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  renderRows(1001);
  measure();
  await frame();
  const retained = rows.slice(0, 1000);
  const distant = rows[1000];
  const sizes = Sizes.observers[0];
  Proximity.observers[0].deliver(retained.map((row) => [row, true]));
  await frame();
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  vi.mocked(requestAnimationFrame).mockClear();
  for (let update = 0; update < 10; update++) {
    for (const row of retained) row.firstChild!.firstChild!.nodeValue = `Updated title ${update}`;
    await settle();
    await frame();
  }
  expect({
    requestedFrames: vi.mocked(requestAnimationFrame).mock.calls.length,
    reobservedRows: sizes.observe.mock.calls.length,
    unobservedRows: sizes.unobserve.mock.calls.length,
    observedTargets: sizes.targets.size,
  }).toEqual({ requestedFrames: 0, reobservedRows: 0, unobservedRows: 0, observedTargets: 2 });
  expect([...root.children]).toEqual(rows);
  expect(retained.every((row) => row.style.contentVisibility === "visible" &&
    !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  expect(distant.style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");

  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  distant.firstChild!.firstChild!.nodeValue = "A longer unseen title";
  await settle();
  await frame();
  expect(sizes.observe).toHaveBeenCalledExactlyOnceWith(distant);
  expect(sizes.unobserve).toHaveBeenCalledExactlyOnceWith(distant);
  expect(distant.style.contentVisibility).toBe("visible");
  sizes.deliver([[distant, 600, 145]]);
  await frame();
  expect(distant.style.contentVisibility).toBe("auto");
  expect(distant.style.getPropertyValue("contain-intrinsic-block-size")).toBe("145px");
});

test("retention drops queued invalidations and ignores late sizes while repairing external styles", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  renderRows(2);
  measure();
  await frame();
  const sizes = Sizes.observers[0];
  rows[0].firstChild!.textContent = "A longer nearby title";
  await settle();
  sizes.deliver([[rows[1], 600, 120]]);
  Proximity.observers[0].deliver(rows.map((row) => [row, true]));
  sizes.observe.mockClear();
  await frame();
  expect(sizes.observe).not.toHaveBeenCalled();
  expect(sizes.targets).toEqual(new Set([root]));
  sizes.deliver([[root, 350, 300], [rows[0], 350, 145], [rows[1], 350, 155]]);
  expect(frames.size).toBe(0);
  expect(rows.every((row) => row.style.contentVisibility === "visible")).toBe(true);

  rows[0].style.cssText = "content-visibility: auto; contain-intrinsic-block-size: 1px; color: red";
  await settle();
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  expect(rows[0].style.color).toBe("red");
  expect(sizes.observe).not.toHaveBeenCalled();
  expect(frames.size).toBe(0);
});

test("preparation uses the main scroller and retains keyed rows across moves", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  const mounted = await render();
  measure();
  await frame();
  const proximity = Proximity.observers[0];
  expect(proximity.options.root).toBe(main);
  proximity.deliver([[rows[3], true], [rows[8], false]]);
  await frame();
  expect(rows[3].style.contentVisibility).toBe("visible");
  expect(rows[8].style.contentVisibility).toBe("auto");
  mounted.replace([items[8], items[3]]);
  await settle();
  await frame();
  expect(rows[3].style.contentVisibility).toBe("visible");
  expect(proximity.targets).toEqual(new Set([rows[8]]));
  Sizes.observers[0].deliver([[root, 350, 180]]);
  await frame();
  Sizes.observers[0].deliver([[rows[3], 350, 145], [rows[8], 350, 145]]);
  await frame();
  expect(rows[3].style.contentVisibility).toBe("visible");
  expect(rows[8].style.contentVisibility).toBe("auto");
});

test("removed and destroyed rows cannot be prepared by late observations", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  renderRows(2);
  const proximity = Proximity.observers[0];
  proximity.deliver([[rows[0], true]]);
  rows[0].remove();
  await settle();
  await frame();
  expect(rows[0].style.contentVisibility).toBe("");
  expect(proximity.targets).toEqual(new Set([rows[1]]));
  proximity.deliver([[rows[0], true]]);
  expect(frames.size).toBe(0);
  action!.destroy();
  action = undefined;
  proximity.deliver([[rows[1], true]]);
  expect(frames.size).toBe(0);
  expect(rows[1].style.contentVisibility).toBe("");
  expect(proximity.targets.size).toBe(0);
});

test("preparation wins over queued size changes and content invalidation", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  renderRows(2);
  measure();
  await frame();
  Sizes.observers[0].deliver([[rows[0], 600, 120]]);
  Proximity.observers[0].deliver([[rows[0], true]]);
  rows[0].querySelector("span")!.textContent = "A longer nearby title";
  await settle();
  await frame();
  Sizes.observers[0].deliver([[rows[0], 600, 145]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  expect(rows[1].style.contentVisibility).toBe("auto");
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
  expect(frames.size).toBe(0);
});

test("a zero-size initial layout stays natural and recovers when shown", async () => {
  await render();
  measure(0, 0);
  await frame();
  expect(rows.every((row) => !row.style.contentVisibility)).toBe(true);
  measure();
  await frame();
  expect(rows.every((row) => row.style.contentVisibility === "auto")).toBe(true);
});

test("width changes discard old heights before measuring wrapped titles again", async () => {
  await render();
  measure();
  await frame();
  Sizes.observers[0].deliver([[root, 350, 9000], [rows[0], 350, 90]]);
  await frame();
  expect(rows.every((row) => row.style.contentVisibility === "visible")).toBe(true);
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  measure(162, 350);
  await frame();
  expect(rows.every((row) => row.style.contentVisibility === "auto")).toBe(true);
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("162px");
  for (let i = 0; i < 3; i++) {
    measure(162, 350);
    await frame();
    expect(rows[0].style.contentVisibility).toBe("auto");
    expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("162px");
  }
  expect(frames.size).toBe(0);
});

test("a row width change can remeasure even when the list width stays unchanged", async () => {
  await render();
  measure();
  await frame();
  Sizes.observers[0].deliver([[rows[0], 500, 90]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  for (let i = 0; i < 3; i++) {
    Sizes.observers[0].deliver([[rows[0], 500, 135]]);
    await frame();
    expect(rows[0].style.contentVisibility).toBe("auto");
    expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("135px");
  }
});

test("sort and filter preserve stable nodes, original indices and observer ownership", async () => {
  const mounted = await render();
  measure();
  await frame();
  mounted.replace([items[8], items[3]]);
  await settle();
  await frame();
  expect([...root.children]).toEqual([rows[8], rows[3]]);
  expect(rows[8].dataset.index).toBe("0");
  expect(rows[3].dataset.index).toBe("1");
  expect(Sizes.observers[0].targets).toEqual(new Set([root, rows[8], rows[3]]));
  mounted.replace([]);
  await settle();
  expect(root.children).toHaveLength(0);
  expect(Sizes.observers[0].targets).toEqual(new Set([root]));
  mounted.replace(items);
  await settle();
  expect(root.children).toHaveLength(100);
  expect(Sizes.observers[0].targets.size).toBe(101);
});

test("content updates invalidate only the affected row, including offscreen titles", async () => {
  const mounted = await render();
  measure();
  await frame();
  mounted.replace(items.map((item) => item.media_id === 40 ? { ...item, title: "A much longer wrapped title" } : item));
  await settle();
  await frame();
  expect(rows[39].style.contentVisibility).toBe("visible");
  expect(rows[38].style.contentVisibility).toBe("auto");
  Sizes.observers[0].deliver([[rows[39], 600, 145]]);
  await frame();
  expect(rows[39].style.getPropertyValue("contain-intrinsic-block-size")).toBe("145px");
});

test("a text change and its measurement writes avoid membership scans across 1000 rows", async () => {
  renderRows(1000);
  measure();
  await frame();
  const children = root.children;
  const iterate = children[Symbol.iterator].bind(children);
  const work = { scans: 0, childVisits: 0 };
  vi.spyOn(root, "children", "get").mockImplementation(() => {
    work.scans++;
    return children;
  });
  vi.spyOn(children, Symbol.iterator).mockImplementation(function* () {
    for (const child of iterate()) {
      work.childVisits++;
      yield child;
    }
    return undefined;
  });
  const untouchedParent = vi.spyOn(rows[0], "parentElement", "get");
  rows[499].firstChild!.firstChild!.nodeValue = "A much longer wrapped title";
  await settle();
  await frame();
  expect(rows[499].style.contentVisibility).toBe("visible");
  expect(rows[0].style.contentVisibility).toBe("auto");
  Sizes.observers[0].deliver([[rows[499], 600, 145]]);
  await frame();
  expect(rows[499].style.getPropertyValue("contain-intrinsic-block-size")).toBe("145px");
  expect(work).toEqual({ scans: 0, childVisits: 0 });
  expect(untouchedParent).not.toHaveBeenCalled();
  expect(frames.size).toBe(0);
});

test.each([
  ["descendant child changes", (row: HTMLElement) => row.firstElementChild!.replaceChildren("Updated title")],
  ["descendant attributes", (row: HTMLElement) => row.firstElementChild!.setAttribute("class", "expanded")],
  ["external row styles", (row: HTMLElement) => { row.style.paddingTop = "10px"; }],
] as const)("%s remeasure their row without scanning membership", async (_, change) => {
  renderRows(2);
  measure();
  await frame();
  const scans = vi.spyOn(root, "children", "get");
  change(rows[0]);
  await settle();
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[1].style.contentVisibility).toBe("auto");
  Sizes.observers[0].deliver([[rows[0], 600, 120]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("120px");
  expect(scans).not.toHaveBeenCalled();
  expect(frames.size).toBe(0);
});

test("direct text, comments and SVG children do not change observed row membership", async () => {
  renderRows(2);
  measure();
  await frame();
  const scans = vi.spyOn(root, "children", "get");
  root.append(document.createTextNode(" "), document.createComment("anchor"),
    document.createElementNS("http://www.w3.org/2000/svg", "svg"));
  await settle();
  expect(scans).not.toHaveBeenCalled();
  expect(Sizes.observers[0].targets).toEqual(new Set([root, ...rows]));
  expect(rows.every((row) => row.style.contentVisibility === "auto")).toBe(true);
  expect(frames.size).toBe(0);
});

test("direct row changes update membership without scans and restore removed rows before pending work", async () => {
  const list = document.createElement("div");
  const removed = document.createElement("div");
  const kept = document.createElement("div");
  const added = document.createElement("div");
  removed.style.contentVisibility = "visible";
  removed.style.setProperty("contain-intrinsic-block-size", "101px");
  removed.style.color = "red";
  list.append(removed, kept);
  document.body.append(list);
  action = measuredRows(list);
  const sizes = Sizes.observers[0];
  sizes.deliver([[list, 600, 180], [removed, 600, 90], [kept, 600, 90]]);
  await frame();
  removed.append("Changed title");
  await settle();
  sizes.deliver([[kept, 600, 120]]);
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  const scans = vi.spyOn(list, "children", "get");
  removed.remove();
  list.append(added);
  list.append(kept);
  await settle();
  await frame();
  expect(scans).not.toHaveBeenCalled();
  expect(sizes.targets).toEqual(new Set([list, kept, added]));
  expect(sizes.observe).toHaveBeenCalledExactlyOnceWith(added);
  expect(sizes.unobserve).toHaveBeenCalledExactlyOnceWith(removed);
  expect(removed.style.contentVisibility).toBe("visible");
  expect(removed.style.getPropertyValue("contain-intrinsic-block-size")).toBe("101px");
  expect(removed.style.color).toBe("red");
  expect(kept.style.getPropertyValue("contain-intrinsic-block-size")).toBe("120px");
  expect(added.style.contentVisibility).toBe("");
  list.append(removed);
  await settle();
  expect(sizes.observe).toHaveBeenLastCalledWith(removed);
  sizes.deliver([[removed, 600, 150], [added, 600, 80]]);
  await frame();
  expect(removed.style.getPropertyValue("contain-intrinsic-block-size")).toBe("150px");
  expect(added.style.contentVisibility).toBe("auto");
});

test("32 staged batches visit only added and removed rows beside 2000 existing rows", async () => {
  root = document.createElement("div");
  rows = Array.from({ length: 3024 }, () => document.createElement("div"));
  root.append(...rows.slice(0, 2000));
  document.body.append(root);
  const children = root.children;
  const iterate = children[Symbol.iterator].bind(children);
  const parentElement = Object.getOwnPropertyDescriptor(Node.prototype, "parentElement")!.get!;
  const work = { scans: 0, childVisits: 0, parentVisits: 0 };
  const candidates = new Set(rows);
  vi.spyOn(root, "children", "get").mockImplementation(() => {
    work.scans++;
    return children;
  });
  vi.spyOn(children, Symbol.iterator).mockImplementation(function* () {
    for (const child of iterate()) {
      work.childVisits++;
      yield child;
    }
    return undefined;
  });
  vi.spyOn(Node.prototype, "parentElement", "get").mockImplementation(function (this: Node) {
    if (candidates.has(this as HTMLDivElement)) work.parentVisits++;
    return parentElement.call(this);
  });
  action = measuredRows(root);
  expect(work).toEqual({ scans: 1, childVisits: 2000, parentVisits: 0 });
  Object.assign(work, { scans: 0, childVisits: 0, parentVisits: 0 });
  const sizes = Sizes.observers[0];
  sizes.observe.mockClear();
  for (let index = 2000; index < rows.length; index += 32) {
    root.append(...rows.slice(index, index + 32));
    await settle();
  }
  const additions = { ...work };
  Object.assign(work, { scans: 0, childVisits: 0, parentVisits: 0 });
  for (let index = 2000; index < rows.length; index += 32) {
    for (const row of rows.slice(index, index + 32)) row.remove();
    await settle();
  }
  expect({ additions, removals: work }).toEqual({
    additions: { scans: 0, childVisits: 0, parentVisits: 1024 },
    removals: { scans: 0, childVisits: 0, parentVisits: 1024 },
  });
  expect(sizes.observe).toHaveBeenCalledTimes(1024);
  expect(sizes.unobserve).toHaveBeenCalledTimes(1024);
  expect(sizes.targets).toEqual(new Set([root, ...rows.slice(0, 2000)]));
  expect(frames.size).toBe(0);
});

test("same-microtask remove and readd preserves registration and pending measurements", async () => {
  renderRows(3);
  measure();
  await frame();
  const sizes = Sizes.observers[0];
  sizes.deliver([[rows[0], 600, 120]]);
  rows[1].append("A longer title");
  await settle();
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  rows[0].remove();
  root.append(rows[0]);
  rows[1].remove();
  root.prepend(rows[1]);
  root.prepend(rows[2]);
  await settle();
  expect([...root.children]).toEqual([rows[2], rows[1], rows[0]]);
  expect(sizes.targets).toEqual(new Set([root, ...rows]));
  expect(sizes.observe).not.toHaveBeenCalled();
  expect(sizes.unobserve).not.toHaveBeenCalled();
  sizes.deliver([[rows[1], 600, 150]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("120px");
  expect(rows[1].style.contentVisibility).toBe("visible");
  expect(rows[2].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
  expect(sizes.observe).toHaveBeenCalledExactlyOnceWith(rows[1]);
  expect(sizes.unobserve).toHaveBeenCalledExactlyOnceWith(rows[1]);
  sizes.deliver([[rows[1], 600, 150]]);
  await frame();
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("150px");
  expect(frames.size).toBe(0);
});

test.each([false, true])("moving between measured containers transfers ownership with destination first: %s", async (destinationFirst) => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  const source = document.createElement("div");
  const destination = document.createElement("div");
  const moved = document.createElement("div");
  const kept = document.createElement("div");
  moved.style.contentVisibility = "visible";
  moved.style.setProperty("contain-intrinsic-block-size", "101px");
  moved.style.color = "red";
  source.append(moved);
  destination.append(kept);
  document.body.append(source, destination);
  const first = measuredRows(destinationFirst ? destination : source)!;
  const second = measuredRows(destinationFirst ? source : destination)!;
  try {
    const sourceSizes = Sizes.observers.find((sizes) => sizes.targets.has(source))!;
    const destinationSizes = Sizes.observers.find((sizes) => sizes.targets.has(destination))!;
    const sourceProximity = Proximity.observers.find((proximity) => proximity.targets.has(moved))!;
    const destinationProximity = Proximity.observers.find((proximity) => proximity.targets.has(kept))!;
    sourceSizes.deliver([[source, 600, 90], [moved, 600, 90]]);
    destinationSizes.deliver([[destination, 600, 90], [kept, 600, 90]]);
    await frame();
    sourceProximity.deliver([[moved, true]]);
    await frame();
    expect(moved.style.contentVisibility).toBe("visible");
    expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
    sourceSizes.deliver([[moved, 600, 120]]);
    sourceSizes.observe.mockClear();
    sourceSizes.unobserve.mockClear();
    destinationSizes.observe.mockClear();
    destination.append(moved);
    await settle();
    expect(sourceSizes.targets).toEqual(new Set([source]));
    expect(destinationSizes.targets).toEqual(new Set([destination, kept, moved]));
    expect(sourceSizes.unobserve).toHaveBeenCalledExactlyOnceWith(moved);
    expect(sourceSizes.observe).not.toHaveBeenCalled();
    expect(destinationSizes.observe).toHaveBeenCalledExactlyOnceWith(moved);
    expect(moved.style.contentVisibility).toBe("visible");
    expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("101px");
    await frame();
    expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("101px");
    sourceSizes.deliver([[moved, 600, 300]]);
    sourceProximity.deliver([[moved, true]]);
    expect(frames.size).toBe(0);
    expect(sourceProximity.targets.size).toBe(0);
    expect(destinationProximity.targets).toEqual(new Set([kept, moved]));
    destinationProximity.deliver([[moved, false]]);
    destinationSizes.deliver([[moved, 600, 150]]);
    await frame();
    expect(moved.style.contentVisibility).toBe("auto");
    expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("150px");
    expect(kept.style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
    destinationProximity.deliver([[moved, true]]);
    await frame();
    expect(moved.style.contentVisibility).toBe("visible");
    expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  } finally {
    first.destroy();
    second.destroy();
  }
  expect(moved.style.contentVisibility).toBe("visible");
  expect(moved.style.getPropertyValue("contain-intrinsic-block-size")).toBe("101px");
  expect(moved.style.color).toBe("red");
  expect(Sizes.observers.every((sizes) => sizes.targets.size === 0)).toBe(true);
  expect(Proximity.observers.every((proximity) => proximity.targets.size === 0)).toBe(true);
});

test("mixed additions register only direct HTML rows and descendants still invalidate their row", async () => {
  renderRows(1);
  measure();
  await frame();
  const added = document.createElement("section");
  added.innerHTML = '<span>Title</span><svg><text>Label</text></svg>';
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  const foreignObject = document.createElementNS("http://www.w3.org/2000/svg", "foreignObject");
  foreignObject.append(document.createElement("div"));
  svg.append(foreignObject);
  const directText = document.createTextNode(" ");
  const fragment = document.createDocumentFragment();
  fragment.append(directText, document.createComment("anchor"), svg, added);
  root.append(fragment);
  const sizes = Sizes.observers[0];
  sizes.observe.mockClear();
  await settle();
  expect(sizes.observe).toHaveBeenCalledExactlyOnceWith(added);
  expect(sizes.targets).toEqual(new Set([root, rows[0], added]));
  sizes.deliver([[added, 600, 120]]);
  await frame();
  const transient = document.createElement("div");
  transient.style.contentVisibility = "visible";
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  root.append(transient);
  transient.remove();
  directText.nodeValue = "Ignored text";
  foreignObject.firstElementChild!.setAttribute("class", "ignored");
  await settle();
  expect(frames.size).toBe(0);
  expect(sizes.observe).not.toHaveBeenCalled();
  expect(sizes.unobserve).not.toHaveBeenCalled();
  expect(transient.style.contentVisibility).toBe("visible");
  added.querySelector("text")!.firstChild!.nodeValue = "A longer SVG label";
  await settle();
  await frame();
  expect(added.style.contentVisibility).toBe("visible");
  expect(rows[0].style.contentVisibility).toBe("auto");
  sizes.deliver([[added, 600, 150]]);
  await frame();
  added.querySelector("svg")!.setAttribute("width", "300");
  await settle();
  await frame();
  expect(added.style.contentVisibility).toBe("visible");
  expect(rows[0].style.contentVisibility).toBe("auto");
});

test("clearing drops pending rows and later detached mutations or measurements do no work", async () => {
  renderRows(3);
  measure();
  await frame();
  const sizes = Sizes.observers[0];
  rows[0].firstChild!.textContent = "Changed title";
  await settle();
  sizes.deliver([[rows[1], 600, 130]]);
  root.replaceChildren();
  await settle();
  expect(sizes.targets).toEqual(new Set([root]));
  expect(rows.every((row) => !row.style.contentVisibility &&
    !row.style.getPropertyValue("contain-intrinsic-block-size"))).toBe(true);
  await frame();
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  rows[0].firstChild!.textContent = "Detached title";
  rows[1].setAttribute("class", "detached");
  sizes.deliver(rows.map((row) => [row, 600, 200]));
  await settle();
  expect(frames.size).toBe(0);
  expect(sizes.observe).not.toHaveBeenCalled();
  expect(sizes.unobserve).not.toHaveBeenCalled();
  action!.destroy();
  action = undefined;
  root.append(...rows);
  rows[0].append("After teardown");
  sizes.deliver([[rows[0], 600, 300]]);
  await settle();
  expect(sizes.targets.size).toBe(0);
  expect(frames.size).toBe(0);
  expect(rows.every((row) => !row.style.contentVisibility)).toBe(true);
});

test("1000 reserved cover loads keep measured rows without scheduling or reobserving them", async () => {
  renderRows(1000, true);
  measure();
  await frame();
  const sizes = Sizes.observers[0];
  sizes.observe.mockClear();
  sizes.unobserve.mockClear();
  vi.mocked(requestAnimationFrame).mockClear();
  const rect = vi.spyOn(Element.prototype, "getBoundingClientRect");
  for (const row of rows) row.querySelector("img")!.dispatchEvent(new Event("load"));
  await frame();
  expect({
    requestedFrames: vi.mocked(requestAnimationFrame).mock.calls.length,
    reobservedRows: sizes.observe.mock.calls.length,
    unobservedRows: sizes.unobserve.mock.calls.length,
    revealedRows: rows.filter((row) => row.style.contentVisibility === "visible").length,
  }).toEqual({ requestedFrames: 0, reobservedRows: 0, unobservedRows: 0, revealedRows: 0 });
  expect(rows.every((row) => row.style.getPropertyValue("contain-intrinsic-block-size") === "90px")).toBe(true);
  expect(rect).not.toHaveBeenCalled();

  sizes.deliver([[rows[0], 600, 135]]);
  rows[0].querySelector("img")!.dispatchEvent(new Event("load"));
  await frame();
  expect(rows[0].style.contentVisibility).toBe("auto");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("135px");
  rows[0].querySelector("span")!.textContent = "A longer title";
  await settle();
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[1].style.contentVisibility).toBe("auto");
});

test.each([
  ["unmarked dimensioned images", '<img width="46" height="63">', "img"],
  ["nested variable images", "<div data-measured-cover><span><img></span></div>", "img"],
  ["other load targets", "<div data-measured-cover><span></span></div>", "span"],
])("%s still remeasure their row", async (_, markup, selector) => {
  renderRows(2);
  rows[0].innerHTML = markup;
  await settle();
  await frame();
  measure();
  await frame();
  rows[0].querySelector(selector)!.dispatchEvent(new Event("load"));
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[1].style.contentVisibility).toBe("auto");
  Sizes.observers[0].deliver([[rows[0], 600, 145]]);
  await frame();
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("145px");
});

test("variable image loads, font completion and list attributes still invalidate measurements", async () => {
  const owner = document.implementation.createHTMLDocument();
  const fonts = new EventTarget();
  Object.defineProperty(owner, "fonts", { value: fonts });
  const removeFontListener = vi.spyOn(fonts, "removeEventListener");
  const list = owner.createElement("div");
  const first = owner.createElement("div");
  const second = owner.createElement("div");
  const image = owner.createElement("img");
  first.append(image);
  list.append(first, second);
  owner.body.append(list);
  action = measuredRows(list);
  const sizes = Sizes.observers[0];
  sizes.deliver([[list, 600, 180], [first, 600, 90], [second, 600, 90]]);
  await frame();
  image.dispatchEvent(new Event("load"));
  await frame();
  expect(first.style.contentVisibility).toBe("visible");
  expect(second.style.contentVisibility).toBe("auto");
  sizes.deliver([[first, 600, 120]]);
  await frame();
  expect(first.style.getPropertyValue("contain-intrinsic-block-size")).toBe("120px");
  fonts.dispatchEvent(new Event("loadingdone"));
  await frame();
  expect([first, second].every((row) => row.style.contentVisibility === "visible")).toBe(true);
  sizes.deliver([[first, 600, 150], [second, 600, 120]]);
  await frame();
  list.setAttribute("class", "wrapped");
  await settle();
  await frame();
  expect([first, second].every((row) => row.style.contentVisibility === "visible")).toBe(true);
  action!.destroy();
  action = undefined;
  expect(removeFontListener).toHaveBeenCalledExactlyOnceWith("loadingdone", expect.any(Function));
  fonts.dispatchEvent(new Event("loadingdone"));
  image.dispatchEvent(new Event("load"));
  sizes.deliver([[first, 600, 200]]);
  await frame();
  expect(frames.size).toBe(0);
  expect([first, second].every((row) => !row.style.contentVisibility)).toBe(true);
});

test("focused rows stay rendered and keyboard traversal has no window edges", async () => {
  await render();
  measure();
  await frame();
  const first = rows[0].querySelector("button")!;
  first.focus();
  expect(rows[0].style.contentVisibility).toBe("visible");
  await frame();
  Sizes.observers[0].deliver([[rows[0], 600, 90]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  main.scrollTop = 8800;
  main.dispatchEvent(new Event("scroll"));
  await frame();
  expect(document.activeElement).toBe(first);
  expect(first.isConnected).toBe(true);
  for (const key of ["Tab", "ArrowDown", "Home", "End"]) {
    expect(first.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }))).toBe(true);
  }
  const last = rows.at(-1)!.querySelector("button")!;
  last.focus();
  expect(last.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }))).toBe(true);
  document.querySelector<HTMLButtonElement>("[data-after]")!.focus();
  expect(document.activeElement?.hasAttribute("data-after")).toBe(true);
  document.querySelector<HTMLButtonElement>("[data-before]")!.focus();
  expect(document.activeElement?.hasAttribute("data-before")).toBe(true);
});

test("blur reobserves an unchanged focused row and restores auto visibility", async () => {
  await render();
  measure();
  await frame();
  rows[0].querySelector("button")!.focus();
  await frame();
  Sizes.observers[0].deliver([[rows[0], 600, 90]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("visible");
  const observe = Sizes.observers[0].observe;
  observe.mockClear();
  document.querySelector<HTMLButtonElement>("[data-before]")!.focus();
  await frame();
  expect(observe).toHaveBeenCalledWith(rows[0]);
  Sizes.observers[0].deliver([[rows[0], 600, 90]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("auto");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
});

test("blur preserves prepared rows while distant focused rows return to measured containment", async () => {
  vi.stubGlobal("IntersectionObserver", Proximity);
  await render();
  measure();
  await frame();
  Proximity.observers[0].deliver([[rows[0], true]]);
  await frame();
  const outside = document.querySelector<HTMLButtonElement>("[data-before]")!;
  for (const row of rows.slice(0, 2)) {
    row.querySelector("button")!.focus();
    await frame();
    Sizes.observers[0].deliver([[row, 600, 90]]);
    await frame();
    expect(document.activeElement).toBe(row.querySelector("button"));
    expect(row.style.contentVisibility).toBe("visible");
    outside.focus();
    await frame();
    Sizes.observers[0].deliver([[row, 600, 90]]);
    await frame();
  }
  expect(rows[0].style.contentVisibility).toBe("visible");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
  expect(rows[1].style.contentVisibility).toBe("auto");
  expect(rows[1].style.getPropertyValue("contain-intrinsic-block-size")).toBe("90px");
  expect(frames.size).toBe(0);
});

test("scroll restoration offsets survive measurement and later content updates", async () => {
  const mounted = await render();
  main.scrollTop = 6123;
  measure();
  await frame();
  mounted.replace(items.map((item) => ({ ...item, title: `${item.title} updated` })));
  await settle();
  await frame();
  measure(120);
  await frame();
  expect(main.scrollTop).toBe(6123);
});

test.each([false, true])("scrolling preserves pending episode values and the original debounce with retained rows: %s", async (retained) => {
  vi.useFakeTimers();
  if (retained) vi.stubGlobal("IntersectionObserver", Proximity);
  await render(true);
  measure();
  await frame();
  if (retained) {
    Proximity.observers[0].deliver([[rows[0], true]]);
    await frame();
  }
  const plus = rows[0].querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  plus.click();
  await settle();
  main.scrollTop = 8500;
  main.dispatchEvent(new Event("scroll"));
  await frame();
  main.scrollTop = 0;
  main.dispatchEvent(new Event("scroll"));
  plus.click();
  await settle();
  expect(rows[0].textContent).toContain("5/12");
  expect(mocks.setProgress).not.toHaveBeenCalled();
  mocks.setProgress.mockResolvedValue({ media_id: 1, progress: 5 });
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 5, 3);
});

test("a failed pending save remains visible and retryable after scrolling", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  await render(true);
  mocks.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  rows[0].querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  await expect(flushPendingEdits()).rejects.toThrow("Disk is full");
  main.scrollTop = 8500;
  main.dispatchEvent(new Event("scroll"));
  main.scrollTop = 0;
  main.dispatchEvent(new Event("scroll"));
  await settle();
  expect(rows[0].textContent).toContain("4/12");
  expect(rows[0].querySelector('[title="Not saved: Error: Disk is full"]')).not.toBeNull();
  await flushPendingEdits();
  expect(mocks.setProgress).toHaveBeenNthCalledWith(2, 1, 4, 3);
});

test("an in-flight save remains owned by the same row throughout scrolling", async () => {
  await render(true);
  const save = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValueOnce(save.promise);
  rows[0].querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  const saving = flushPendingEdits();
  await settle();
  main.scrollTop = 8500;
  main.dispatchEvent(new Event("scroll"));
  expect(rows[0].querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.disabled).toBe(true);
  save.resolve({ media_id: 1, progress: 4 });
  await saving;
  await settle();
  expect(rows[0].textContent).toContain("4/12");
  expect(mocks.setProgress).toHaveBeenCalledOnce();
});

test("teardown cancels pending work and disconnects observers and listeners", async () => {
  await render();
  measure();
  expect(frames.size).toBe(1);
  const remove = vi.spyOn(root, "removeEventListener");
  await unmount(instance!);
  instance = undefined;
  expect(frames.size).toBe(0);
  expect(Sizes.observers[0].disconnect).toHaveBeenCalledOnce();
  expect(remove.mock.calls.map(([event]) => event)).toEqual(["focusin", "focusout", "load"]);
  Sizes.observers[0].deliver([[rows[0], 600, 300]]);
  await frame();
  expect(rows[0].style.contentVisibility).toBe("");
  expect(rows[0].style.getPropertyValue("contain-intrinsic-block-size")).toBe("");
});

test("the action restores pre-existing containment styles on teardown", async () => {
  const list = document.createElement("div");
  const row = document.createElement("div");
  row.style.contentVisibility = "visible";
  row.style.setProperty("contain-intrinsic-block-size", "101px");
  list.append(row);
  document.body.append(list);
  const action = measuredRows(list)!;
  Sizes.observers[0].deliver([[list, 600, 90], [row, 600, 90]]);
  await frame();
  action.destroy();
  expect(row.style.contentVisibility).toBe("visible");
  expect(row.style.getPropertyValue("contain-intrinsic-block-size")).toBe("101px");
});

test("browsers without both containment properties retain ordinary rows", async () => {
  vi.mocked(CSS.supports).mockImplementation((property) => property !== "contain-intrinsic-block-size");
  await render();
  expect(Sizes.observers).toHaveLength(0);
  expect(rows).toHaveLength(100);
  expect(rows.every((row) => !row.style.contentVisibility)).toBe(true);
});
