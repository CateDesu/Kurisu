import { beforeEach, expect, test, vi } from "vitest";

const api = vi.hoisted(() => ({ getEntry: vi.fn(), updateEntry: vi.fn() }));
vi.mock("$lib/api", () => ({ api }));
vi.mock("$lib/auth.svelte", async () => import("./session.svelte"));
import { addToList, createListActions } from "$lib/list.svelte";
import { deferred } from "./helpers";

beforeEach(() => { vi.resetAllMocks(); });

test("existing list status actions preserve other fields", async () => {
  api.getEntry.mockResolvedValue({ media_id: 1, status: "CURRENT", progress: 3, repeat: 2, score: 75 });
  api.updateEntry.mockResolvedValue({ media_id: 1, status: "COMPLETED", progress: 12, repeat: 2, score: 75 });
  const result = await addToList({ id: 1, episodes: 12 }, "COMPLETED");
  expect(api.updateEntry).toHaveBeenCalledWith(1, "COMPLETED", null, null, null);
  expect(result.progress).toBe(12);
});

test("adding a show leaves remote progress and rewatches unspecified", async () => {
  api.getEntry.mockResolvedValue(null);
  await addToList({ id: 1 }, "PLANNING");
  expect(api.updateEntry).toHaveBeenCalledWith(1, "PLANNING", null, null, null);
});

test("duplicate actions for one title cannot submit competing writes", async () => {
  const saved = deferred<{ media_id: number; progress: number }>();
  api.updateEntry.mockReturnValue(saved.promise);
  const actions = createListActions();
  const first = actions.add({ id: 1 }, "CURRENT");
  expect(await actions.add({ id: 1 }, "PLANNING")).toBeNull();
  expect(api.updateEntry).toHaveBeenCalledOnce();
  expect(actions.pending(1)).toBe(true);
  saved.resolve({ media_id: 1, progress: 0 });
  await first;
  expect(actions.pending(1)).toBe(false);
});

test("reset discards older list action results without clearing newer pending work", async () => {
  const old = deferred<{ media_id: number; progress: number }>();
  const next = deferred<{ media_id: number; progress: number }>();
  api.updateEntry.mockReturnValueOnce(old.promise).mockReturnValueOnce(next.promise);
  const actions = createListActions();
  const first = actions.add({ id: 1 }, "CURRENT");
  actions.reset();
  const second = actions.add({ id: 1 }, "PLANNING");
  old.resolve({ media_id: 1, progress: 0 });
  expect(await first).toBeNull();
  expect(actions.pending(1)).toBe(true);
  next.resolve({ media_id: 1, progress: 0 });
  await second;
  expect(actions.pending(1)).toBe(false);
});
