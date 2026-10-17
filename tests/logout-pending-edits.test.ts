import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: {
    currentUser: vi.fn(), isLoggedIn: vi.fn(), logout: vi.fn(), setProgress: vi.fn(),
    loginWithToken: vi.fn(), loginOauth: vi.fn(), getTrackingConfig: vi.fn(), getAppSetting: vi.fn(),
  },
  reset: vi.fn(),
  listeners: new Map<string, () => void>(),
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/library.svelte", () => ({ library: { reset: mocks.reset } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (event: string, callback: () => void) => {
  mocks.listeners.set(event, callback);
  return () => mocks.listeners.delete(event);
}) }));

let components: ReturnType<typeof mount>[] = [];
import { auth } from "$lib/auth.svelte";
import { flushPendingEdits } from "$lib/pendingEdits";
import Stepper from "$lib/EpisodeStepper.svelte";
import Settings from "../src/routes/settings/+page.svelte";

beforeEach(async () => {
  vi.resetAllMocks();
  vi.useFakeTimers();
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Example" });
  mocks.api.logout.mockResolvedValue(undefined);
  mocks.api.setProgress.mockResolvedValue({ media_id: 1, progress: 4 });
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "Other" });
  mocks.api.loginOauth.mockResolvedValue({ id: 2, name: "Other" });
  mocks.api.getTrackingConfig.mockResolvedValue({ mode: "off", prompt_seconds: 120, auto_percent: 80, auto_ask: true, mpv_ipc_socket: "", discord_enabled: true });
  mocks.api.getAppSetting.mockResolvedValue(null);
  await auth.refresh();
  const target = document.createElement("div");
  document.body.append(target);
  components.push(mount(Stepper, { target, props: { mediaId: 1, progress: 3, total: 12 } }));
  await settle();
  document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.click();
  await settle();
});

afterEach(async () => {
  mocks.api.setProgress.mockResolvedValue({ media_id: 1, progress: 4 });
  for (const component of components) await unmount(component);
  components = [];
  await flushPendingEdits();
  document.body.replaceChildren();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

test("logout waits for buffered progress to be saved for the current account", async () => {
  const saving = deferred<{ media_id: number; progress: number }>();
  mocks.api.setProgress.mockReturnValue(saving.promise);
  const loggingOut = auth.logout();
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(mocks.api.logout).not.toHaveBeenCalled();
  expect(document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.disabled).toBe(true);
  saving.resolve({ media_id: 1, progress: 4 });
  await loggingOut;
  expect(mocks.api.logout).toHaveBeenCalledOnce();
  expect(auth.isLoggedIn).toBe(false);
});

test("failed progress persistence keeps logout from discarding the account and allows retry", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.api.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  await expect(auth.logout()).rejects.toThrow("Disk is full");
  expect(mocks.api.logout).not.toHaveBeenCalled();
  expect(auth.user?.id).toBe(1);
  expect(document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.disabled).toBe(false);
  await auth.logout();
  expect(mocks.api.setProgress).toHaveBeenNthCalledWith(2, 1, 4, 3);
  expect(mocks.api.logout).toHaveBeenCalledOnce();
  expect(auth.isLoggedIn).toBe(false);
});

test("logout joins a save already running after the stepper was removed", async () => {
  const saving = deferred<{ media_id: number; progress: number }>();
  mocks.api.setProgress.mockReturnValue(saving.promise);
  await vi.advanceTimersByTimeAsync(3000);
  await unmount(components.pop()!);
  const loggingOut = auth.logout();
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledOnce();
  expect(mocks.api.logout).not.toHaveBeenCalled();
  saving.resolve({ media_id: 1, progress: 4 });
  await loggingOut;
  expect(mocks.api.logout).toHaveBeenCalledOnce();
});

test.each(["loginWithToken", "loginOauth"] as const)("%s persists old-account edits before replacing its session", async (method) => {
  const saving = deferred<{ media_id: number; progress: number }>();
  mocks.api.setProgress.mockReturnValue(saving.promise);
  const signingIn = method === "loginWithToken" ? auth.loginWithToken("new token") : auth.loginOauth();
  await settle();
  expect(mocks.api[method]).not.toHaveBeenCalled();
  expect(auth.changingSession).toBe(true);
  saving.resolve({ media_id: 1, progress: 4 });
  await signingIn;
  expect(mocks.api[method]).toHaveBeenCalledOnce();
  expect(auth.user?.id).toBe(2);
  expect(auth.changingSession).toBe(false);
});

test("a newer login supersedes logout while both wait for the same save", async () => {
  const saving = deferred<{ media_id: number; progress: number }>();
  mocks.api.setProgress.mockReturnValue(saving.promise);
  const loggingOut = auth.logout();
  const signingIn = auth.loginWithToken("new token");
  await settle();
  expect(mocks.api.setProgress).toHaveBeenCalledOnce();
  saving.resolve({ media_id: 1, progress: 4 });
  await Promise.all([loggingOut, signingIn]);
  expect(mocks.api.logout).not.toHaveBeenCalled();
  expect(mocks.api.loginWithToken).toHaveBeenCalledOnce();
  expect(auth.user?.id).toBe(2);
  expect(auth.changingSession).toBe(false);
});

test("a failed account replacement keeps the old session and releases controls", async () => {
  mocks.api.loginWithToken.mockRejectedValueOnce(new Error("Invalid token"));
  await expect(auth.loginWithToken("invalid")).rejects.toThrow("Invalid token");
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(auth.user?.id).toBe(1);
  expect(auth.changingSession).toBe(false);
  await settle();
  expect(document.querySelector<HTMLButtonElement>('[aria-label="One more episode"]')!.disabled).toBe(false);
});

test("a failed progress save blocks replacement sign-in and keeps its pending edit retryable", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.api.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  await expect(auth.loginWithToken("new token")).rejects.toThrow("Disk is full");
  expect(mocks.api.loginWithToken).not.toHaveBeenCalled();
  expect(auth.user?.id).toBe(1);
  expect(auth.changingSession).toBe(false);
  await auth.loginWithToken("new token");
  expect(mocks.api.setProgress).toHaveBeenNthCalledWith(2, 1, 4, 3);
  expect(auth.user?.id).toBe(2);
});

test("a backend logout failure still clears the frontend session after progress is saved", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.api.logout.mockRejectedValueOnce(new Error("Token could not be scrubbed"));
  await expect(auth.logout()).rejects.toThrow("Token could not be scrubbed");
  expect(mocks.api.setProgress).toHaveBeenCalledExactlyOnceWith(1, 4, 3);
  expect(auth.isLoggedIn).toBe(false);
  expect(auth.changingSession).toBe(false);
});

test("session expiry during the save barrier does not restore the expired account", async () => {
  const saving = deferred<{ media_id: number; progress: number }>();
  mocks.api.setProgress.mockReturnValue(saving.promise);
  const loggingOut = auth.logout();
  await settle();
  mocks.listeners.get("kurisu://auth-expired")!();
  saving.reject(new Error("Session expired"));
  await loggingOut;
  expect(auth.isLoggedIn).toBe(false);
  expect(auth.changingSession).toBe(false);
});

test("settings shows a failed logout save while the current account stays signed in", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  mocks.api.setProgress.mockRejectedValueOnce(new Error("Disk is full"));
  components.push(mount(Settings, { target: document.body }));
  await settle();
  button("Log out").click();
  await settle();
  button("Log out", document.querySelector('[role="dialog"]')!).click();
  await settle();
  await settle();
  expect(auth.user?.id).toBe(1);
  expect(document.querySelector('[role="alert"]')?.textContent).toContain("Disk is full");
  expect(document.querySelector('[role="dialog"]')).toBeNull();
  expect(mocks.api.logout).not.toHaveBeenCalled();
});
