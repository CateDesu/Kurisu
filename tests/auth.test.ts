import { beforeEach, expect, test, vi } from "vitest";
import { deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: { currentUser: vi.fn(), loginWithToken: vi.fn(), loginOauth: vi.fn(), isLoggedIn: vi.fn(), logout: vi.fn() },
  reset: vi.fn(),
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/library.svelte", () => ({ library: { reset: mocks.reset } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

beforeEach(() => {
  vi.resetModules();
  vi.resetAllMocks();
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
  await auth.loginWithToken("new token");
  signingOut.resolve(undefined);
  await pending;
  expect(auth.user?.id).toBe(2);
});
