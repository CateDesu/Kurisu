import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { mount, unmount } from "svelte";
import { button, deferred, settle } from "./helpers";

const mocks = vi.hoisted(() => ({
  api: {
    currentUser: vi.fn(), isLoggedIn: vi.fn(),
    loginOauth: vi.fn(), loginWithToken: vi.fn(),
  },
}));
vi.mock("#lib/api.ts", () => ({ api: mocks.api }));
vi.mock("#lib/library.svelte.ts", () => ({ library: { reset: vi.fn() } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import Login from "#lib/Login.svelte";
import { auth } from "#lib/auth.svelte.ts";

let component: ReturnType<typeof mount>;
const user = { id: 7, name: "Tester" };

beforeEach(async () => {
  vi.resetAllMocks();
  mocks.api.currentUser.mockResolvedValue(null);
  mocks.api.isLoggedIn.mockResolvedValue(false);
  await auth.refresh();
  component = mount(Login, { target: document.body });
  await settle();
});

afterEach(async () => {
  await unmount(component);
  document.body.replaceChildren();
});

async function enterToken() {
  document.querySelector("details")!.open = true;
  const input = document.querySelector<HTMLInputElement>('input[type="password"]')!;
  input.value = "  example-token  ";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  await settle();
}

test("a pasted token can replace browser sign-in before its callback arrives", async () => {
  const browser = deferred<typeof user>();
  const token = deferred<typeof user>();
  mocks.api.loginOauth.mockReturnValue(browser.promise);
  mocks.api.loginWithToken.mockReturnValue(token.promise);
  button("Connect via AniList").click();
  await settle();
  expect(mocks.api.loginOauth).toHaveBeenCalledOnce();
  await enterToken();
  expect(button("Use").disabled).toBe(false);
  button("Use").click();
  await settle();
  expect(mocks.api.loginWithToken).toHaveBeenCalledExactlyOnceWith("example-token");
  expect(button("Use").disabled).toBe(true);
  browser.reject(new Error("A newer sign-in or sign-out replaced this attempt."));
  await settle();
  expect(document.body.textContent).not.toContain("replaced this attempt");
  expect(button("Use").disabled).toBe(true);
  token.resolve(user);
  await settle();
  expect(auth.user?.id).toBe(7);
  expect(auth.changingSession).toBe(false);
});

test("an old browser result cannot clear a token error or change its account", async () => {
  const browser = deferred<typeof user>();
  mocks.api.loginOauth.mockReturnValue(browser.promise);
  mocks.api.loginWithToken.mockRejectedValueOnce(new Error("Invalid Token"));
  button("Connect via AniList").click();
  await settle();
  await enterToken();
  button("Use").click();
  await settle();
  expect(document.body.textContent).toContain("Invalid Token");
  browser.resolve({ id: 8, name: "Old browser account" });
  await settle();
  expect(auth.user).toBeNull();
  expect(document.body.textContent).toContain("Invalid Token");
  expect(document.body.textContent).not.toContain("superseded");
  mocks.api.loginWithToken.mockResolvedValueOnce(user);
  button("Use").click();
  await settle();
  expect(auth.user?.id).toBe(7);
  expect(document.body.textContent).not.toContain("Invalid Token");
});

test("browser failures release sign-in controls and report the error", async () => {
  mocks.api.loginOauth.mockRejectedValueOnce(new Error("Could not open browser"));
  button("Connect via AniList").click();
  await settle();
  expect(document.body.textContent).toContain("Could not open browser");
  expect(button("Connect via AniList").disabled).toBe(false);
  await enterToken();
  expect(button("Use").disabled).toBe(false);
});
