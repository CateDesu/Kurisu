import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { rememberScroll } from "#lib/scroll.ts";

let node: HTMLElement;
let memory: ReturnType<typeof rememberScroll>;
let height: number;
let top: number;
let resize: ResizeObserverCallback;
let disconnect: ReturnType<typeof vi.fn>;
let observed: Set<Element>;
let frames: Map<number, FrameRequestCallback>;

function paint() {
  const callbacks = [...frames.values()];
  frames.clear();
  for (const callback of callbacks) callback(0);
}

async function updateContent() {
  node.append(document.createElement("div"));
  await Promise.resolve();
}

beforeEach(() => {
  height = 2000;
  top = 0;
  disconnect = vi.fn();
  observed = new Set();
  frames = new Map();
  let nextFrame = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
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

test("suppresses scroll anchoring until restored content has painted", async () => {
  node.style.setProperty("overflow-anchor", "auto", "important");
  height = 400;
  memory.restore({ left: 0, top: 1200 });
  expect(node.style.overflowAnchor).toBe("none");
  expect(node.style.getPropertyPriority("overflow-anchor")).toBe("important");
  height = 2000;
  await updateContent();
  expect(node.scrollTop).toBe(1200);
  paint();
  expect(node.style.overflowAnchor).toBe("none");
  paint();
  expect(node.style.overflowAnchor).toBe("auto");
  expect(node.style.getPropertyPriority("overflow-anchor")).toBe("important");
  expect(frames.size).toBe(0);
});

test("a new restore cancels the previous anchoring cleanup", () => {
  memory.restore({ left: 0, top: 1200 });
  paint();
  memory.restore({ left: 0, top: 700 });
  paint();
  expect(node.style.overflowAnchor).toBe("none");
  expect(node.scrollTop).toBe(700);
  paint();
  expect(node.style.overflowAnchor).toBe("");
  expect(frames.size).toBe(0);
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
    expect(node.style.overflowAnchor).toBe("");
    resize([], {} as ResizeObserver);
    expect(node.scrollTop).toBe(300);
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
