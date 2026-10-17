import { beforeEach, expect, test, vi } from "vitest";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: { currentUser: vi.fn(), loginWithToken: vi.fn(), loginOauth: vi.fn(), isLoggedIn: vi.fn(), logout: vi.fn() },
  reset: vi.fn(),
  listeners: new Map<string, () => Promise<void> | void>(),
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/library.svelte", () => ({ library: { reset: mocks.reset } }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: () => Promise<void> | void) => {
    mocks.listeners.set(name, callback);
    return () => mocks.listeners.delete(name);
  }),
}));

beforeEach(() => {
  vi.resetModules();
  vi.resetAllMocks();
  mocks.listeners.clear();
  mocks.api.currentUser.mockResolvedValue(null);
});

test.each(["loginWithToken", "loginOauth"] as const)("%s clears matches from the previous account", async (method) => {
  mocks.api[method].mockResolvedValue({ id: 2, name: "Other" });
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  if (method === "loginWithToken") await auth.loginWithToken("token");
  else await auth.loginOauth();
  expect(auth.user?.id).toBe(2);
  expect(mocks.reset).toHaveBeenCalledOnce();
});

test("a cached profile keeps the frontend offline until viewer recovery", async () => {
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Example", score_format: "POINT_5", offline: true });
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  expect(auth.offline).toBe(true);
  expect(auth.user?.score_format).toBe("POINT_5");
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Example", score_format: "POINT_5", offline: false });
  await auth.refresh();
  expect(auth.offline).toBe(false);
});

test("a delayed OAuth result cannot replace a newer token login", async () => {
  const oauth = deferred<{ id: number; name: string }>();
  mocks.api.loginOauth.mockReturnValue(oauth.promise);
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "New account" });
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const old = auth.loginOauth().catch(() => {});
  await settle();
  expect(mocks.api.loginOauth).toHaveBeenCalledOnce();
  await auth.loginWithToken("new token");
  oauth.resolve({ id: 1, name: "Old account" });
  await old;
  expect(auth.user?.id).toBe(2);
});

test("logging out invalidates an unfinished sign-in", async () => {
  const signingIn = deferred<{ id: number; name: string }>();
  mocks.api.loginWithToken.mockReturnValue(signingIn.promise);
  mocks.api.logout.mockResolvedValue(undefined);
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const pending = auth.loginWithToken("token").catch(() => {});
  await settle();
  expect(mocks.api.loginWithToken).toHaveBeenCalledOnce();
  await auth.logout();
  signingIn.resolve({ id: 1, name: "Old account" });
  await pending;
  expect(auth.isLoggedIn).toBe(false);
});

test("an older logout result cannot clear a newer login", async () => {
  const signingOut = deferred<void>();
  mocks.api.logout.mockReturnValue(signingOut.promise);
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "New account" });
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const pending = auth.logout();
  await settle();
  expect(mocks.api.logout).toHaveBeenCalledOnce();
  await auth.loginWithToken("new token");
  signingOut.resolve(undefined);
  await pending;
  expect(auth.user?.id).toBe(2);
});

test("an expiry event delivered after a new login keeps the authenticated session", async () => {
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "New account" });
  mocks.api.isLoggedIn.mockResolvedValue(true);
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  await auth.loginWithToken("new token");
  const epoch = auth.epoch;
  await mocks.listeners.get("kurisu://auth-expired")!();
  expect(auth.user?.id).toBe(2);
  expect(auth.epoch).toBe(epoch);
  expect(mocks.reset).toHaveBeenCalledOnce();
});

test("an expiry check cannot clear a login accepted while it was pending", async () => {
  const expired = deferred<boolean>();
  mocks.api.isLoggedIn.mockReturnValue(expired.promise);
  mocks.api.loginWithToken.mockResolvedValue({ id: 2, name: "New account" });
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const checking = mocks.listeners.get("kurisu://auth-expired")!();
  await auth.loginWithToken("new token");
  const epoch = auth.epoch;
  expired.resolve(false);
  await checking;
  expect(auth.user?.id).toBe(2);
  expect(auth.epoch).toBe(epoch);
});

test("an expired backend session clears the cached frontend account", async () => {
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Expired account" });
  mocks.api.isLoggedIn.mockResolvedValue(false);
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const epoch = auth.epoch;
  await mocks.listeners.get("kurisu://auth-expired")!();
  expect(auth.isLoggedIn).toBe(false);
  expect(auth.epoch).toBe(epoch + 1);
  expect(mocks.reset).toHaveBeenCalledOnce();
});

test("a failed replacement login cannot suppress a pending expiry check", async () => {
  const expired = deferred<boolean>();
  mocks.api.currentUser.mockResolvedValue({ id: 1, name: "Expired account" });
  mocks.api.isLoggedIn.mockReturnValue(expired.promise);
  mocks.api.loginWithToken.mockRejectedValue(new Error("Invalid replacement token"));
  const { auth } = await import("$lib/auth.svelte");
  await settle();
  const checking = mocks.listeners.get("kurisu://auth-expired")!();
  await expect(auth.loginWithToken("bad token")).rejects.toThrow("Invalid replacement token");
  expired.resolve(false);
  await checking;
  expect(auth.isLoggedIn).toBe(false);
});
