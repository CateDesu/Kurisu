import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { rememberScroll } from "$lib/scroll";

let node: HTMLElement;
let memory: ReturnType<typeof rememberScroll>;
let height: number;
let top: number;
let resize: ResizeObserverCallback;
let disconnect: ReturnType<typeof vi.fn>;
let observed: Set<Element>;

async function updateContent() {
  node.append(document.createElement("div"));
  await Promise.resolve();
}

beforeEach(() => {
  height = 2000;
  top = 0;
  disconnect = vi.fn();
  observed = new Set();
  vi.stubGlobal("ResizeObserver", class {
    constructor(callback: ResizeObserverCallback) { resize = callback; }
    observe(element: Element) { observed.add(element); }
    unobserve(element: Element) { observed.delete(element); }
    disconnect = disconnect;
  });
  node = document.createElement("main");
  Object.defineProperty(node, "scrollTop", {
    get: () => top,
    set: (value: number) => { top = Math.max(0, Math.min(value, height - 400)); },
  });
  document.body.append(node);
  memory = rememberScroll(node);
});

afterEach(() => {
  memory.destroy();
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

test("captures both axes and resets a new page to the top", () => {
  node.scrollTop = 1200;
  node.scrollLeft = 40;
  expect(memory.capture()).toEqual({ left: 40, top: 1200 });
  memory.reset();
  expect(memory.capture()).toEqual({ left: 0, top: 0 });
});

test("restores immediately when the content is already available", () => {
  memory.restore({ left: 20, top: 1200 });
  expect(memory.capture()).toEqual({ left: 20, top: 1200 });
  expect(disconnect).not.toHaveBeenCalled();
});

test("waits for cached rows before completing Back restoration", async () => {
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  expect(node.scrollTop).toBe(0);
  height = 900;
  await updateContent();
  expect(node.scrollTop).toBe(500);
  height = 2000;
  await updateContent();
  expect(node.scrollTop).toBe(1200);
  expect(disconnect).toHaveBeenCalledOnce();
  node.scrollTop = 600;
  await updateContent();
  expect(node.scrollTop).toBe(600);
});

test("retries when loaded covers change the content height", () => {
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  height = 2000;
  resize([], {} as ResizeObserver);
  expect(node.scrollTop).toBe(1200);
  expect(disconnect).toHaveBeenCalledOnce();
});

test("keeps watching content replaced while Back restoration is pending", async () => {
  const loading = document.createElement("div");
  node.append(loading);
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  const content = document.createElement("div");
  node.replaceChildren(content);
  await Promise.resolve();
  expect(observed.has(content)).toBe(true);
  expect(observed.has(loading)).toBe(false);
  height = 2000;
  resize([], {} as ResizeObserver);
  expect(node.scrollTop).toBe(1200);
});

test.each(["wheel", "pointerdown", "touchstart", "keydown"])(
  "%s cancels pending restoration so it cannot pull the user back", async (type) => {
    height = 400;
    memory.restore({ left: 0, top: 1200 });
    (type === "keydown" ? window : node).dispatchEvent(new Event(type));
    height = 2000;
    node.scrollTop = 300;
    await updateContent();
    expect(node.scrollTop).toBe(300);
    expect(disconnect).toHaveBeenCalledOnce();
  },
);

test.each(["reset", "destroy"] as const)("%s cancels pending restoration", async (method) => {
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  memory[method]();
  height = 2000;
  await updateContent();
  expect(node.scrollTop).toBe(0);
  expect(disconnect).toHaveBeenCalledOnce();
});

test("a second restoration replaces the pending destination", async () => {
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  memory.restore({ left: 0, top: 700 });
  height = 2000;
  await updateContent();
  expect(node.scrollTop).toBe(700);
  expect(disconnect).toHaveBeenCalledTimes(2);
});
