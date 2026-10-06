import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  setProgress: vi.fn(), installUpdate: vi.fn(), close: vi.fn(), invoke: vi.fn(),
  onResized: vi.fn().mockResolvedValue(() => {}),
  isMaximized: vi.fn().mockResolvedValue(false),
  auth: { epoch: 0, isLoggedIn: true },
  listeners: new Map<string, () => Promise<void>>(),
}));
vi.mock("$lib/api", () => ({ api: { setProgress: mocks.setProgress, installUpdate: mocks.installUpdate } }));
vi.mock("$lib/auth.svelte", () => ({ auth: mocks.auth }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => mocks }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: () => Promise<void>) => {
    mocks.listeners.set(name, callback);
    return () => mocks.listeners.delete(name);
  }),
}));
import Stepper from "$lib/EpisodeStepper.svelte";
import TitleBar from "$lib/TitleBar.svelte";
import Shutdown from "$lib/Shutdown.svelte";
import { flushPendingEdits } from "$lib/pendingEdits";

let components: ReturnType<typeof mount>[] = [];
let target: HTMLDivElement;
beforeEach(async () => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  mocks.setProgress.mockReset().mockResolvedValue({ media_id: 1, progress: 4 });
  mocks.auth.isLoggedIn = true;
  target = document.createElement("div");
  document.body.append(target);
  components = [mount(Stepper, { target, props: { mediaId: 1, progress: 3, total: 12 } })];
  await settle();
});
afterEach(async () => {
  mocks.auth.epoch++;
  for (const component of components) await unmount(component);
  await flushPendingEdits();
  target.remove();
  vi.useRealTimers();
  vi.restoreAllMocks();
});
async function increment() {
  target.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
}
async function close() {
  target.querySelector<HTMLButtonElement>('[title="Close"]')!.click();
  await settle();
}

test("Close saves buffered progress and waits for acknowledgement", async () => {
  const write = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValue(write.promise);
  components.push(mount(TitleBar, { target }));
  await increment();
  await close();
  expect(mocks.setProgress).toHaveBeenCalledWith(1, 4, 3);
  expect(mocks.close).not.toHaveBeenCalled();
  write.resolve({ media_id: 1, progress: 4 });
  await settle();
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("a failed close retains the intended progress for retry", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  components.push(mount(TitleBar, { target }));
  await increment();
  await close();
  expect(mocks.close).not.toHaveBeenCalled();
  expect(target.textContent).toContain("Disk is full");
  await close();
  expect(mocks.setProgress).toHaveBeenNthCalledWith(2, 1, 4, 3);
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("close joins an in-flight save after the component is removed", async () => {
  const write = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValue(write.promise);
  await increment();
  await unmount(components.pop()!);
  components.push(mount(TitleBar, { target }));
  await settle();
  await close();
  expect(mocks.setProgress).toHaveBeenCalledOnce();
  expect(mocks.close).not.toHaveBeenCalled();
  write.resolve({ media_id: 1, progress: 4 });
  await settle();
  expect(mocks.close).toHaveBeenCalledOnce();
});

test("shutdown does not apply buffered edits to a replacement account", async () => {
  await increment();
  mocks.auth.epoch++;
  await flushPendingEdits();
  expect(mocks.setProgress).not.toHaveBeenCalled();
});

test("a new edit supersedes a failed edit retained from an earlier page", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  await increment();
  await vi.advanceTimersByTimeAsync(3000);
  await unmount(components.pop()!);

  let saved = 3;
  mocks.setProgress.mockImplementation(async (_id, value, expected) => {
    if (expected === saved) saved = value;
    return { media_id: 1, progress: saved };
  });
  components.push(mount(Stepper, { target, props: { mediaId: 1, progress: 3, total: 12 } }));
  await settle();
  await increment();
  await increment();
  await flushPendingEdits();
  expect(saved).toBe(5);
  expect(mocks.setProgress).toHaveBeenCalledTimes(2);
});

test("superseded timers cannot send older progress", async () => {
  await increment();
  await vi.advanceTimersByTimeAsync(1000);
  components.push(mount(Stepper, { target, props: { mediaId: 1, progress: 3, total: 12 } }));
  await settle();
  const plus = [...target.querySelectorAll<HTMLButtonElement>('[aria-label="One more episode"]')].at(-1)!;
  plus.click();
  plus.click();
  await vi.advanceTimersByTimeAsync(2000);
  expect(mocks.setProgress).not.toHaveBeenCalled();
  mocks.setProgress.mockResolvedValue({ media_id: 1, progress: 5 });
  await vi.advanceTimersByTimeAsync(1000);
  expect(mocks.setProgress).toHaveBeenCalledExactlyOnceWith(1, 5, 3);
});

test("an earlier acknowledgement cannot unregister a newer pending edit", async () => {
  const write = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValueOnce(write.promise);
  await increment();
  await vi.advanceTimersByTimeAsync(3000);
  await unmount(components.pop()!);
  components.push(mount(Stepper, { target, props: { mediaId: 1, progress: 4, total: 12 } }));
  await settle();
  await increment();
  write.resolve({ media_id: 1, progress: 4 });
  await settle();
  mocks.setProgress.mockResolvedValue({ media_id: 1, progress: 5 });
  await flushPendingEdits();
  expect(mocks.setProgress).toHaveBeenNthCalledWith(2, 1, 5, 4);
});

test("canceling a newer edit still waits for an older in-flight save", async () => {
  const write = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValueOnce(write.promise);
  await increment();
  await vi.advanceTimersByTimeAsync(3000);
  await unmount(components.pop()!);
  components.push(mount(Stepper, { target, props: { mediaId: 1, progress: 3, total: 12 } }));
  await settle();
  await increment();
  target.querySelector<HTMLButtonElement>('[aria-label="One less episode"]')!.click();
  let finished = false;
  const closing = flushPendingEdits().then(() => { finished = true; });
  await settle();
  expect(finished).toBe(false);
  write.resolve({ media_id: 1, progress: 4 });
  await closing;
  expect(finished).toBe(true);
  expect(mocks.setProgress).toHaveBeenCalledOnce();
});

test("native Quit waits for buffered edits before approving exit", async () => {
  const write = deferred<{ media_id: number; progress: number }>();
  mocks.setProgress.mockReturnValue(write.promise);
  components.push(mount(Shutdown, { target }));
  await increment();
  expect(mocks.invoke).toHaveBeenCalledWith("shutdown_ready");
  const closing = mocks.listeners.get("kurisu://shutdown-requested")!();
  await settle();
  expect(mocks.invoke).not.toHaveBeenCalledWith("finish_shutdown", expect.anything());
  write.resolve({ media_id: 1, progress: 4 });
  await closing;
  expect(mocks.invoke).toHaveBeenCalledWith("finish_shutdown", { success: true });
});

test("native Quit stays open after a failed save", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  components.push(mount(Shutdown, { target }));
  await increment();
  await mocks.listeners.get("kurisu://shutdown-requested")!();
  await settle();
  expect(mocks.invoke).toHaveBeenCalledWith("finish_shutdown", { success: false });
  expect(target.textContent).toContain("Kurisu is still open");
  await mocks.listeners.get("kurisu://shutdown-requested")!();
  expect(mocks.invoke).toHaveBeenLastCalledWith("finish_shutdown", { success: true });
});

test("installing flushes progress and prevents new buffered clicks", async () => {
  const { runInstallUpdate } = await import("$lib/update.svelte");
  const installer = deferred<string>();
  mocks.installUpdate.mockReturnValue(installer.promise);
  await increment();
  const installing = runInstallUpdate();
  await settle();
  expect(mocks.setProgress).toHaveBeenCalledWith(1, 4, 3);
  const plus = target.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!;
  expect(plus.disabled).toBe(true);
  plus.click();
  await vi.advanceTimersByTimeAsync(3000);
  expect(mocks.setProgress).toHaveBeenCalledOnce();
  installer.resolve("restarting");
  await installing;
  await settle();
  expect(plus.disabled).toBe(true);
});
