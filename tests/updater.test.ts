import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { button, deferred } from "./helpers";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

const mocks = vi.hoisted(() => ({
  api: Object.fromEntries(["takePendingUpdate", "takeUpdateFailed", "checkUpdate", "installUpdate", "getTrackingConfig", "getAppSetting"].map((name) => [name, vi.fn()])),
  listener: null as ((event: { payload: unknown }) => void) | null,
}));
vi.mock("$lib/api", () => ({ api: mocks.api }));
vi.mock("$lib/auth.svelte", () => ({ auth: { user: { id: 1, name: "Example" }, isLoggedIn: true } }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, callback: typeof mocks.listener) => {
  if (name === "kurisu://update-available") mocks.listener = callback;
  return () => {};
}) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

const update = { available: true, can_install: true, restart_pending: false, version: "2.0.0", tag: "v2.0.0", html_url: "https://example.com/release", body: "", current: "1.0.0" };
let runtime: typeof import("svelte");
let instances: ReturnType<typeof import("svelte")["mount"]>[] = [];

beforeEach(async () => {
  vi.resetModules();
  runtime = await import("svelte");
  vi.resetAllMocks();
  mocks.listener = null;
  mocks.api.takePendingUpdate.mockResolvedValue(null);
  mocks.api.takeUpdateFailed.mockResolvedValue(null);
  mocks.api.checkUpdate.mockResolvedValue(update);
  mocks.api.installUpdate.mockResolvedValue("installed");
  mocks.api.getTrackingConfig.mockResolvedValue({ mode: "off", prompt_seconds: 120, auto_percent: 80, auto_ask: true, mpv_ipc_socket: "", discord_enabled: true });
  mocks.api.getAppSetting.mockResolvedValue(null);
});

afterEach(async () => {
  for (const instance of instances) await runtime.unmount(instance);
  instances = [];
  document.body.replaceChildren();
});

async function settle() {
  for (let i = 0; i < 8; i++) await Promise.resolve();
  runtime.flushSync();
  await runtime.tick();
}

async function showSettings() {
  const { default: Settings } = await import("../src/routes/settings/+page.svelte");
  instances.push(runtime.mount(Settings, { target: document.body }));
  await settle();
  button("Check for updates").click();
  await settle();
}

async function showBanner(info = update) {
  const { default: Updater } = await import("$lib/Updater.svelte");
  instances.push(runtime.mount(Updater, { target: document.body }));
  await settle();
  mocks.listener!({ payload: info });
  await settle();
}

const installButtons = () => [...document.querySelectorAll("button")].filter((item) => /^(Download & install|Install 2\.0\.0)$/.test(item.textContent?.trim() ?? ""));

test("startup updates already installed offer restart guidance", async () => {
  await showBanner({ ...update, restart_pending: true });
  expect(installButtons()).toHaveLength(0);
  expect(document.body.textContent).toContain("restart Kurisu to finish");
});

test("checking after installation does not offer a second install", async () => {
  mocks.api.checkUpdate.mockResolvedValue({ ...update, restart_pending: true });
  await showSettings();
  expect(installButtons()).toHaveLength(0);
});

test("installing from the banner updates an already open settings page", async () => {
  await showSettings();
  await showBanner();
  button("Download & install").click();
  await settle();
  button("Got it").click();
  await settle();
  expect(installButtons()).toHaveLength(0);
});

test("duplicate startup delivery cannot erase installation success", async () => {
  await showBanner();
  button("Download & install").click();
  await settle();
  mocks.listener!({ payload: update });
  await settle();
  expect(installButtons()).toHaveLength(0);
  expect(document.body.textContent).toContain("restart Kurisu to finish");
});

test("startup results wait until update listeners are ready", async () => {
  const ready = deferred<() => void>();
  vi.mocked(listen).mockReturnValue(ready.promise);
  const { default: Updater } = await import("$lib/Updater.svelte");
  instances.push(runtime.mount(Updater, { target: document.body }));
  await settle();
  expect(mocks.api.takePendingUpdate).not.toHaveBeenCalled();
  mocks.api.takePendingUpdate.mockResolvedValue(update);
  ready.resolve(() => {});
  await settle();
  expect(button("Download & install").disabled).toBe(false);
});

test("updates without a compatible installer still offer release details", async () => {
  await showBanner({ ...update, can_install: false });
  expect(installButtons()).toHaveLength(0);
  button("View release").click();
  await settle();
  expect(openUrl).toHaveBeenCalledWith(update.html_url);
  expect(mocks.api.installUpdate).not.toHaveBeenCalled();
});

test("periodic checks respect dismissal and show a later release", async () => {
  await showBanner();
  button("Later").click();
  await settle();
  mocks.listener!({ payload: update });
  await settle();
  expect(installButtons()).toHaveLength(0);
  mocks.listener!({ payload: { ...update, version: "2.0.1", tag: "v2.0.1" } });
  await settle();
  expect(document.body.textContent).toContain("2.0.1");
  expect(button("Download & install").disabled).toBe(false);
});
