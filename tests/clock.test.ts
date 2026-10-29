import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { nowMs } from "#lib/now.svelte.ts";
import { settle } from "./helpers";
import ClockHarness from "./ClockHarness.svelte";

let component: ReturnType<typeof mount> | undefined;
const observe = vi.fn();

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(Date.UTC(2026, 9, 27, 12));
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  observe.mockClear();
});

afterEach(async () => {
  if (component) await unmount(component);
  component = undefined;
  document.body.replaceChildren();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

test.each([30_000, 1_250])("clock retains its %i ms interval and stops on lifecycle cleanup", async (intervalMs) => {
  component = mount(ClockHarness, { target: document.body, props: { observe, ...(intervalMs === 30_000 ? {} : { intervalMs }) } });
  await settle();
  observe.mockClear();
  await vi.advanceTimersByTimeAsync(intervalMs - 1);
  expect(observe).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1);
  await settle();
  expect(observe).toHaveBeenCalledExactlyOnceWith(Date.now());
  expect(document.body.textContent).toBe(String(Date.now()));
  await unmount(component);
  component = undefined;
  const stoppedAt = nowMs();
  document.body.replaceChildren();
  await vi.advanceTimersByTimeAsync(intervalMs * 3);
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  expect(vi.getTimerCount()).toBe(0);
  expect(nowMs()).toBe(stoppedAt);
});

test("hidden clock ticks publish nothing and visibility return immediately catches up", async () => {
  component = mount(ClockHarness, { target: document.body, props: { observe } });
  await settle();
  await vi.advanceTimersByTimeAsync(30_000);
  await settle();
  const visibleAt = nowMs();
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange"));
  observe.mockClear();
  await vi.advanceTimersByTimeAsync(300_000);
  await settle();
  expect(nowMs()).toBe(visibleAt);
  expect(document.body.textContent).toBe(String(visibleAt));
  expect(observe).not.toHaveBeenCalled();
  visibility.mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  expect(nowMs()).toBe(Date.now());
  expect(observe).toHaveBeenCalledExactlyOnceWith(Date.now());
  observe.mockClear();
  await vi.advanceTimersByTimeAsync(30_000);
  await settle();
  expect(observe).toHaveBeenCalledExactlyOnceWith(Date.now());
});

test("a clock mounted hidden waits for visibility and removes its listener on unmount", async () => {
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  const before = nowMs();
  component = mount(ClockHarness, { target: document.body, props: { observe, intervalMs: 1_000 } });
  await settle();
  observe.mockClear();
  await vi.advanceTimersByTimeAsync(90_000);
  expect(nowMs()).toBe(before);
  expect(observe).not.toHaveBeenCalled();
  await unmount(component);
  component = undefined;
  visibility.mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  await settle();
  expect(nowMs()).toBe(before);
  expect(vi.getTimerCount()).toBe(0);
});
