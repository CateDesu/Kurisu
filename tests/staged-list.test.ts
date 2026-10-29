import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import Harness from "./StagedListHarness.svelte";
import type { ListEntry } from "#lib/types.ts";
import { rememberScroll } from "#lib/scroll.ts";
import { flushPendingEdits } from "#lib/pendingEdits.ts";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  setProgress: vi.fn(), auth: { epoch: 0, isLoggedIn: true, changingSession: false },
}));
vi.mock("#lib/api.ts", () => ({ api: { setProgress: mocks.setProgress } }));
vi.mock("#lib/auth.svelte.ts", () => ({ auth: mocks.auth }));

class Sizes {
  static observers: Sizes[] = [];
  observe = vi.fn();
  unobserve = vi.fn();
  disconnect = vi.fn();
  constructor(readonly callback: ResizeObserverCallback) { Sizes.observers.push(this); }
  deliver() { this.callback([], this as unknown as ResizeObserver); }
}

let frames: Map<number, FrameRequestCallback>;
let instance: ReturnType<typeof mount> | undefined;
let memory: ReturnType<typeof rememberScroll> | undefined;
let replace: (items: ListEntry[]) => void;
let main: HTMLElement;
const items = Array.from({ length: 2000 }, (_, index): ListEntry => ({
  media_id: index + 1, status: "CURRENT", progress: 3, repeat: 0,
  media: { id: index + 1, title_english: `Title ${index + 1}` },
}));
const rows = () => [...document.querySelectorAll<HTMLElement>("[data-id]")];
const row = (id: number) => document.querySelector<HTMLElement>(`[data-id="${id}"]`)!;

async function frame() {
  const pending = [...frames.values()];
  frames.clear();
  for (const callback of pending) callback(0);
  await settle();
}

async function finish() {
  for (let i = 0; frames.size && i < 400; i++) await frame();
  expect(document.querySelector("[data-pending-rows]")).toBeNull();
}

async function render(list = items, stepper = false, onOpen?: (entry: ListEntry) => void) {
  const target = document.createElement("div");
  document.body.append(target);
  instance = mount(Harness, { target, props: { items: list, stepper, onOpen } });
  replace = (instance as { replace: typeof replace }).replace;
  await settle();
  main = document.querySelector("main")!;
}

function scrollGeometry() {
  let top = 0;
  let extraHeight = 0;
  Object.defineProperty(main, "scrollHeight", { get: () => rows().length * 90 + extraHeight });
  Object.defineProperty(main, "scrollTop", {
    get: () => top,
    set: (value: number) => { top = Math.max(0, Math.min(value, main.scrollHeight - 400)); },
  });
  return (height: number) => { extraHeight = height; };
}

beforeEach(() => {
  vi.useFakeTimers();
  frames = new Map();
  let next = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => { frames.set(++next, callback); return next; });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  vi.stubGlobal("CSS", { supports: () => false });
  vi.stubGlobal("ResizeObserver", Sizes);
  Sizes.observers = [];
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
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test("2000 real rows mount in bounded tasks and remain mounted after completion", async () => {
  await render();
  const additions = [rows().length];
  expect(additions[0]).toBe(32);
  const first = rows();
  while (frames.size) {
    const before = rows().length;
    await frame();
    additions.push(rows().length - before);
  }
  expect(additions).toHaveLength(63);
  expect(additions.every((count) => count > 0 && count <= 32)).toBe(true);
  expect(rows()).toHaveLength(2000);
  expect(rows().slice(0, 32)).toEqual(first);
  expect(rows().map((element) => element.dataset.stagedIndex)).toEqual(items.map((_, index) => String(index)));
  expect(document.querySelector("[data-staged-rows]")?.getAttribute("aria-busy")).toBe("false");
  expect(document.querySelector('[role="status"]')).toBeNull();
  main.scrollTop = 100000;
  main.dispatchEvent(new Event("scroll"));
  expect(rows()[0]).toBe(first[0]);
  expect(vi.getTimerCount()).toBe(0);
});

test("sequential staging writes each mounted row once", async () => {
  const count = 2000;
  const list = Array.from({ length: count }, (_, index) => ({ ...items[0], media_id: index + 1 }));
  const push = Array.prototype.push;
  let writes = 0;
  Array.prototype.push = function (...values: unknown[]) {
    const first = values[0];
    if (first && typeof first === "object" && "entry" in first && "index" in first) writes += values.length;
    return push.apply(this, values);
  };
  try {
    await render(list);
    const first = row(1);
    await finish();
    expect(rows()).toHaveLength(count);
    expect(row(1)).toBe(first);
    // Queue and batch construction each write every row once.
    expect(writes - 2 * count).toBe(count);
  } finally {
    Array.prototype.push = push;
  }
}, 30000);

test("small collections mount immediately without scheduling", async () => {
  await render(items.slice(0, 24));
  expect(rows()).toHaveLength(24);
  expect(frames.size).toBe(0);
  expect(vi.getTimerCount()).toBe(0);
});

test("batches and sort changes preserve focus and a buffered episode save", async () => {
  await render(items.slice(0, 160), true);
  const first = row(1);
  const plus = first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  plus.focus();
  plus.click();
  await settle();
  await frame();
  replace([...items.slice(0, 160)].reverse());
  await settle();
  expect(row(1)).toBe(first);
  expect(document.activeElement).toBe(plus);
  expect(first.dataset.stagedIndex).toBe("159");
  expect(first.textContent).toContain("160. Title 1");
  expect(first.textContent).toContain("4/12");
  await finish();
  expect(mocks.setProgress).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(3000);
  await settle();
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(row(1)).toBe(first);
  expect(document.activeElement).toBe(plus);
});

test("an in-flight save survives later batches and reorder without another request", async () => {
  const save = deferred<ListEntry>();
  mocks.setProgress.mockReturnValueOnce(save.promise);
  await render(items.slice(0, 128), true);
  const first = row(1);
  first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  const saving = flushPendingEdits();
  await settle();
  replace([...items.slice(0, 128)].reverse());
  await settle();
  await finish();
  expect(row(1)).toBe(first);
  expect(first.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.disabled).toBe(true);
  save.resolve({ ...items[0], progress: 4 });
  await saving;
  await settle();
  expect(first.textContent).toContain("4/12");
  expect(mocks.setProgress).toHaveBeenCalledOnce();
});

test("filter changes remove departed controls and flush their pending edit as before", async () => {
  await render(items.slice(0, 128), true);
  row(1).querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
  replace(items.slice(100, 120));
  await settle();
  expect(rows().map((element) => Number(element.dataset.id))).toEqual(items.slice(100, 120).map((item) => item.media_id));
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(frames.size).toBe(0);
});

test("rapid large replacements cancel old batches and retain surviving rows", async () => {
  await render(items.slice(0, 160));
  const first = row(1);
  replace(items.slice(0, 320).reverse());
  await settle();
  replace(items.slice(0, 128));
  await settle();
  await finish();
  expect(row(1)).toBe(first);
  expect(rows().map((element) => Number(element.dataset.id))).toEqual(items.slice(0, 128).map((item) => item.media_id));
  expect(frames.size).toBe(0);
  expect(vi.getTimerCount()).toBe(0);
});

test("mixed insertions, removals and metadata changes preserve nodes and current entry references", async () => {
  const opened = vi.fn();
  await render(items.slice(0, 96), false, opened);
  const first = row(1);
  first.querySelector<HTMLButtonElement>("button")!.click();
  expect(opened.mock.calls[0][0]).toBe(items[0]);
  await frame();
  const surviving = rows().filter((element) => element.dataset.id !== "2");
  const removed = row(2);
  const updated = { ...items[0], media: { ...items[0].media!, title_english: "Changed title" } };
  const next = [items[94], updated, ...items.slice(2, 64)];
  replace(next);
  await settle();
  expect(removed.isConnected).toBe(false);
  expect(row(1)).toBe(first);
  expect(first.textContent).toContain("2. Changed title");
  first.querySelector<HTMLButtonElement>("button")!.click();
  expect(opened.mock.calls[1][0]).toBe(updated);
  await finish();
  expect(rows().map((element) => Number(element.dataset.id))).toEqual(next.map((entry) => entry.media_id));
  expect(rows().map((element) => Number(element.dataset.stagedIndex))).toEqual(next.map((_, index) => index));
  for (const element of surviving) expect(row(Number(element.dataset.id))).toBe(element);
  row(95).querySelector<HTMLButtonElement>("button")!.click();
  expect(opened.mock.calls[2][0]).toBe(items[94]);
});

test("Tab waits for the next real row rather than skipping to the footer", async () => {
  await render(items.slice(0, 128));
  const source = row(32).querySelector<HTMLButtonElement>("button")!;
  source.focus();
  const event = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
  source.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(source);
  await frame();
  expect(document.activeElement).toBe(row(33));
});

test("Shift Tab fills an unmounted predecessor in a sparse reordered list", async () => {
  await render(items.slice(0, 160));
  const source = row(1);
  source.focus();
  replace([...items.slice(0, 160)].reverse());
  await settle();
  const boundary = row(32);
  boundary.focus();
  const event = new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true });
  boundary.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
  await frame();
  expect(document.activeElement).toBe(row(33).querySelector("button"));
  expect(row(1)).toBe(source);
  const predecessor = row(33);
  await finish();
  expect(row(33)).toBe(predecessor);
  expect(document.activeElement).toBe(predecessor.querySelector("button"));
  expect(rows().map((element) => Number(element.dataset.id))).toEqual(items.slice(0, 160).reverse().map((entry) => entry.media_id));
});

test("a changed subset cancels pending keyboard focus", async () => {
  await render(items.slice(0, 160));
  const source = row(32).querySelector<HTMLButtonElement>("button")!;
  source.focus();
  source.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }));
  replace(items.slice(0, 32));
  await settle();
  expect(document.activeElement).toBe(source);
  expect(frames.size).toBe(0);
});

test("End reaches the completed list without unmounting or moving focused controls", async () => {
  await render();
  scrollGeometry();
  const source = row(1).querySelector<HTMLButtonElement>("button")!;
  source.focus();
  const event = new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true });
  source.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
  await finish();
  expect(main.scrollTop).toBe(179600);
  expect(document.activeElement).toBe(source);
  expect(row(2000).textContent).toContain("2000. Title 2000");
});

test.each(["wheel", "pointerdown", "touchstart"])("%s cancels a pending End destination", async (type) => {
  await render(items.slice(0, 128));
  scrollGeometry();
  row(1).dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true }));
  main.dispatchEvent(new Event(type));
  main.scrollTop = 300;
  await finish();
  expect(main.scrollTop).toBe(300);
});

test("End in the query field retains its native text behavior", async () => {
  await render(items.slice(0, 128));
  const event = new KeyboardEvent("keydown", { key: "End", bubbles: true, cancelable: true });
  document.querySelector("input")!.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(false);
});

test.each([6000, 40000, 150000])("Back waits for full mounting before restoring to %i", async (top) => {
  await render();
  scrollGeometry();
  memory = rememberScroll(main);
  memory.restore({ left: 0, top });
  expect(main.scrollTop).toBe(0);
  await frame();
  expect(main.scrollTop).toBe(0);
  await finish();
  expect(main.scrollTop).toBe(top);
  main.scrollTop = 200;
  main.append(document.createElement("div"));
  await settle();
  expect(main.scrollTop).toBe(200);
});

test("Back keeps waiting when full mounting precedes the final measured geometry", async () => {
  await render(items.slice(0, 128));
  const resize = scrollGeometry();
  memory = rememberScroll(main);
  memory.restore({ left: 0, top: 15000 });
  await finish();
  expect(main.scrollTop).toBe(11120);
  expect(Sizes.observers[0].disconnect).not.toHaveBeenCalled();
  resize(5000);
  Sizes.observers[0].deliver();
  expect(main.scrollTop).toBe(15000);
  expect(Sizes.observers[0].disconnect).toHaveBeenCalledOnce();
});

test("user input cancels Back restoration while staging continues", async () => {
  await render(items.slice(0, 128));
  scrollGeometry();
  memory = rememberScroll(main);
  memory.restore({ left: 0, top: 9000 });
  main.dispatchEvent(new Event("pointerdown"));
  main.scrollTop = 300;
  await finish();
  expect(main.scrollTop).toBe(300);
  expect(Sizes.observers[0].disconnect).toHaveBeenCalledOnce();
});

test.each(["unavailable", "suspended"])("a timer fallback finishes when animation frames are %s", async (mode) => {
  if (mode === "unavailable") vi.stubGlobal("requestAnimationFrame", undefined);
  await render(items.slice(0, 96));
  expect(rows()).toHaveLength(32);
  await vi.advanceTimersByTimeAsync(100);
  await settle();
  expect(rows()).toHaveLength(64);
  await vi.advanceTimersByTimeAsync(100);
  await settle();
  expect(rows()).toHaveLength(96);
  expect(document.querySelector("[data-pending-rows]")).toBeNull();
  expect(frames.size).toBe(0);
  expect(vi.getTimerCount()).toBe(0);
});

test("teardown cancels frame and fallback work and removes navigation listeners", async () => {
  await render();
  const remove = vi.spyOn(main, "removeEventListener");
  await unmount(instance!);
  instance = undefined;
  expect(frames.size).toBe(0);
  expect(vi.getTimerCount()).toBe(0);
  expect(remove.mock.calls.map(([name]) => name)).toEqual(["keydown", "wheel", "pointerdown", "touchstart"]);
});
