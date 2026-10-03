import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { smoothScroll } from "$lib/smooth-scroll";

let node: HTMLElement;
let action: ReturnType<typeof smoothScroll>;
let frames: Map<number, FrameRequestCallback>;
let now: number;
let frameId: number;
let motion: { matches: boolean; addEventListener: ReturnType<typeof vi.fn>; removeEventListener: ReturnType<typeof vi.fn> };

function scroller(height = 2000, parent?: HTMLElement) {
  const element = document.createElement("div");
  element.style.overflowY = "auto";
  element.style.lineHeight = "20px";
  Object.defineProperties(element, {
    scrollHeight: { value: height, configurable: true },
    clientHeight: { value: 400 },
  });
  parent?.append(element);
  return element;
}

function wheel(deltaY = 120, options: WheelEventInit = {}, target: HTMLElement = node) {
  const event = new WheelEvent("wheel", { deltaY, bubbles: true, cancelable: true, ...options });
  target.dispatchEvent(event);
  return event;
}

function frame(elapsed = 16) {
  now += elapsed;
  const callbacks = [...frames.values()];
  frames.clear();
  for (const callback of callbacks) callback(now);
}

function finish() {
  for (let index = 0; index < 100 && frames.size; index++) frame();
  expect(frames.size).toBe(0);
}

beforeEach(() => {
  frames = new Map();
  now = 1000;
  frameId = 0;
  motion = { matches: false, addEventListener: vi.fn(), removeEventListener: vi.fn() };
  vi.stubGlobal("matchMedia", () => motion);
  vi.spyOn(performance, "now").mockImplementation(() => now);
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++frameId, callback);
    return frameId;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  node = scroller();
  document.body.append(node);
  action = smoothScroll(node);
});

afterEach(() => {
  action.destroy();
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

test("a coarse wheel step moves over several frames and preserves its distance", () => {
  expect(wheel().defaultPrevented).toBe(true);
  expect(node.scrollTop).toBe(0);
  frame();
  expect(node.scrollTop).toBeGreaterThan(0);
  expect(node.scrollTop).toBeLessThan(120);
  finish();
  expect(node.scrollTop).toBe(120);
});

test("rapid wheel steps accumulate without starting multiple animations", () => {
  wheel();
  frame();
  wheel();
  wheel();
  expect(frames.size).toBe(1);
  finish();
  expect(node.scrollTop).toBe(360);
});

test("a new wheel tick blends into the current velocity", () => {
  wheel();
  frame(32);
  const before = node.scrollTop;
  frame(1);
  const speed = node.scrollTop - before;
  wheel();
  const after = node.scrollTop;
  frame(1);
  const nextSpeed = node.scrollTop - after;
  expect(nextSpeed).toBeGreaterThan(speed);
  expect(nextSpeed - speed).toBeLessThan(0.25);
  finish();
  expect(node.scrollTop).toBe(240);
});

test("wheel motion starts gently and settles without overshooting", () => {
  wheel();
  frame();
  const firstStep = node.scrollTop;
  frame();
  expect(node.scrollTop - firstStep).toBeGreaterThan(firstStep);
  let previous = node.scrollTop;
  for (let index = 0; index < 100 && frames.size; index++) {
    frame();
    expect(node.scrollTop).toBeGreaterThanOrEqual(previous);
    expect(node.scrollTop).toBeLessThanOrEqual(120);
    previous = node.scrollTop;
  }
  expect(node.scrollTop).toBe(120);
  expect(frames.size).toBe(0);
});

test("reversing direction discards the old destination immediately", () => {
  node.scrollTop = 500;
  wheel();
  frame();
  const turningPoint = node.scrollTop;
  wheel(-120);
  frame();
  expect(node.scrollTop).toBeLessThan(turningPoint);
  finish();
  expect(node.scrollTop).toBeCloseTo(turningPoint - 120);
});

test.each([8, 16, 33])("settles at the same position with %s ms frames", (interval) => {
  wheel();
  for (let index = 0; index < 100 && frames.size; index++) frame(interval);
  expect(node.scrollTop).toBe(120);
  expect(frames.size).toBe(0);
});

test("rounding in the webview cannot leave the last pixel animating forever", () => {
  let top = 0;
  Object.defineProperty(node, "scrollTop", {
    get: () => top,
    set: (value: number) => { top = Math.round(value); },
  });
  wheel();
  finish();
  expect(node.scrollTop).toBe(120);
});

test("rounded scroll positions follow the same motion at different refresh rates", () => {
  let top = 0;
  Object.defineProperty(node, "scrollTop", {
    get: () => top,
    set: (value: number) => { top = Math.round(value); },
  });
  const positions = [4, 8, 16, 32].map((interval) => {
    action.update();
    top = 0;
    wheel();
    for (let elapsed = 0; elapsed < 128; elapsed += interval) frame(interval);
    return top;
  });
  expect(Math.max(...positions) - Math.min(...positions)).toBeLessThanOrEqual(1);
  expect(positions[0]).toBeGreaterThan(110);
});

test("a delayed frame finishes elapsed motion without a lingering catch up", () => {
  wheel();
  frame(500);
  expect(node.scrollTop).toBe(120);
  expect(frames.size).toBe(0);
});

test("an older frame timestamp cannot count wheel event time twice", () => {
  const positions = [0, 8].map((lag) => {
    action.update();
    node.scrollTop = 0;
    now = 1000;
    wheel();
    now += 12;
    wheel();
    const callbacks = [...frames.values()];
    frames.clear();
    for (const callback of callbacks) callback(now - lag);
    frame(20);
    return node.scrollTop;
  });
  expect(positions[1]).toBeCloseTo(positions[0]);
});

test.each([3.5, 12, 39])("precise pixel delta %s stays native", (delta) => {
  expect(wheel(delta).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
});

test("a trackpad gesture stays native when later deltas grow", () => {
  wheel(12);
  for (let index = 0; index < 30; index++) {
    now += 16;
    expect(wheel(80).defaultPrevented).toBe(false);
  }
  expect(frames.size).toBe(0);
  now += 300;
  expect(wheel(120).defaultPrevented).toBe(true);
});

test("a diagonal trackpad gesture stays native when it becomes vertical", () => {
  expect(wheel(80, { deltaX: 20 }).defaultPrevented).toBe(false);
  now += 16;
  expect(wheel(80).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
});

test.each([
  { ctrlKey: true }, { metaKey: true }, { altKey: true }, { shiftKey: true },
  { deltaX: 30 }, { cancelable: false },
])("modified or noncancelable wheel input stays native: %j", (options) => {
  expect(wheel(120, options).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
});

test("reduced motion bypasses wheel easing", () => {
  motion.matches = true;
  expect(wheel().defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
});

test.each([[WheelEvent.DOM_DELTA_LINE, 3, 60], [WheelEvent.DOM_DELTA_PAGE, 1, 400]])(
  "converts wheel mode %s to the scroller's units", (deltaMode, delta, expected) => {
    wheel(delta, { deltaMode });
    finish();
    expect(node.scrollTop).toBe(expected);
  },
);

test("nested scroll areas receive the motion instead of the page", () => {
  const inner = scroller(1000, node);
  wheel(120, {}, inner);
  finish();
  expect(inner.scrollTop).toBe(120);
  expect(node.scrollTop).toBe(0);
});

test("a contained nested scroll area cannot move the page at its boundary", () => {
  const inner = scroller(1000, node);
  inner.style.overscrollBehaviorY = "contain";
  inner.scrollTop = 600;
  expect(wheel(120, {}, inner).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
  expect(node.scrollTop).toBe(0);
});

test("a contained menu cannot move the page when all its options fit", () => {
  const inner = scroller(400, node);
  inner.style.overscrollBehaviorY = "contain";
  expect(wheel(120, {}, inner).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
  expect(node.scrollTop).toBe(0);
});

test("an ordinary nested area can pass scrolling to its parent at the boundary", () => {
  const inner = scroller(1000, node);
  inner.scrollTop = 600;
  wheel(120, {}, inner);
  finish();
  expect(node.scrollTop).toBe(120);
});

test("the destination stops at the bottom without storing extra movement", () => {
  node.scrollTop = 1580;
  wheel();
  finish();
  expect(node.scrollTop).toBe(1600);
  expect(wheel().defaultPrevented).toBe(false);
  wheel(-120);
  finish();
  expect(node.scrollTop).toBe(1480);
});

test.each(["textarea", "select", "input"])("%s keeps its native wheel behavior", (tag) => {
  const control = document.createElement(tag);
  if (control instanceof HTMLInputElement) control.type = "number";
  node.append(control);
  expect(wheel(120, {}, control).defaultPrevented).toBe(false);
  expect(frames.size).toBe(0);
});

test("keyboard and scrollbar input interrupt an active animation", () => {
  wheel();
  frame();
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "PageDown" }));
  const afterKey = node.scrollTop;
  frame();
  expect(node.scrollTop).toBe(afterKey);
  wheel();
  node.dispatchEvent(new Event("pointerdown"));
  expect(frames.size).toBe(0);
});

test("an external scroll cancels easing instead of pulling the page back", () => {
  wheel();
  frame();
  node.scrollTop = 800;
  frame();
  expect(node.scrollTop).toBe(800);
  expect(frames.size).toBe(0);
});

test.each(["update", "destroy"] as const)("%s stops motion during navigation", (method) => {
  wheel();
  action[method]();
  frame();
  expect(node.scrollTop).toBe(0);
  expect(frames.size).toBe(0);
});
