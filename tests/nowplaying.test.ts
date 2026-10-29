import { beforeEach, expect, test, vi } from "vitest";
import type { NowPlaying } from "#lib/types.ts";

const events = vi.hoisted(() => ({ callback: null as null | ((event: { payload: NowPlaying }) => void), listen: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: events.listen }));

const snapshot: NowPlaying = {
  active: true, player: "VLC", title: "Show - 03", matched: "Show", media_id: 1,
  episode: 3, length_us: 1000, position_us: 500,
};

beforeEach(() => {
  vi.resetModules();
  events.callback = null;
  events.listen.mockReset().mockImplementation(async (_event, callback) => { events.callback = callback; return () => {}; });
});

test("duplicate paused snapshots retain state identity and a resumed position publishes", async () => {
  const { bindNowPlaying, nowPlaying } = await import("#lib/nowplaying.svelte.ts");
  await bindNowPlaying();
  events.callback!({ payload: { ...snapshot } });
  const previous = nowPlaying();
  for (let index = 0; index < 12; index++) events.callback!({ payload: { ...snapshot } });
  expect(nowPlaying()).toBe(previous);
  events.callback!({ payload: { ...snapshot, position_us: 600 } });
  expect(nowPlaying()).not.toBe(previous);
  expect(nowPlaying()?.position_us).toBe(600);
});

test.each([
  ["player", "mpv"], ["title", "Other title"], ["matched", null], ["media_id", 2],
  ["episode", 4], ["length_us", 2000], ["position_us", 0],
] as const)("a changed %s publishes a fresh snapshot", async (key, value) => {
  const { bindNowPlaying, nowPlaying } = await import("#lib/nowplaying.svelte.ts");
  await bindNowPlaying();
  events.callback!({ payload: { ...snapshot } });
  const previous = nowPlaying();
  events.callback!({ payload: { ...snapshot, [key]: value } });
  expect(nowPlaying()).not.toBe(previous);
  expect(nowPlaying()?.[key]).toBe(value);
});

test("idle clears playback and an identical later source becomes active again", async () => {
  const { bindNowPlaying, nowPlaying } = await import("#lib/nowplaying.svelte.ts");
  await Promise.all([bindNowPlaying(), bindNowPlaying()]);
  expect(events.listen).toHaveBeenCalledOnce();
  events.callback!({ payload: { ...snapshot } });
  events.callback!({ payload: { ...snapshot, active: false } });
  expect(nowPlaying()).toBeNull();
  events.callback!({ payload: { ...snapshot } });
  expect(nowPlaying()).toEqual(snapshot);
});
