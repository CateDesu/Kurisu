import { beforeEach, expect, test, vi } from "vitest";
import { exportUiPreferences, legacyProfileDatabasePath, readUiPreferences, UI_PREFERENCE_KEYS } from "#lib/profileTransfer.ts";

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));

beforeEach(() => mocks.invoke.mockReset());

test("exports only the nine UI values and keeps absent distinct from empty", async () => {
  const stored = new Map<string, string>([
    ["kurisu.list.q", ""],
    ["kurisu.torrents.filters", '{"category":"all"}'],
    ["anilist_token", "synthetic-token"],
  ]);
  const storage = { getItem: (key: string) => stored.get(key) ?? null };
  const values = readUiPreferences(storage);
  expect(Object.keys(values)).toEqual(UI_PREFERENCE_KEYS);
  expect(values["kurisu.list.q"]).toBe("");
  expect(values["kurisu.list.sort"]).toBeNull();
  expect(JSON.stringify(values)).not.toContain("synthetic-token");
  mocks.invoke.mockResolvedValue("/synthetic/preferences.json");
  await expect(exportUiPreferences(storage)).resolves.toBe("/synthetic/preferences.json");
  expect(mocks.invoke).toHaveBeenCalledExactlyOnceWith("export_profile_preferences", { preferences: values });
});

test("storage read failure stops export instead of writing defaults", async () => {
  const storage = { getItem: () => { throw new Error("storage blocked"); } };
  await expect(exportUiPreferences(storage)).rejects.toThrow("storage blocked");
  expect(mocks.invoke).not.toHaveBeenCalled();
});

test("legacy path is supplied by the running program", async () => {
  mocks.invoke.mockResolvedValue("/synthetic/kurisu.db");
  await expect(legacyProfileDatabasePath()).resolves.toBe("/synthetic/kurisu.db");
  expect(mocks.invoke).toHaveBeenCalledExactlyOnceWith("legacy_profile_database_path");
});

test("durable mirrors preserve change order and flush the latest selections", async () => {
  const { mirrorUiPreferences, flushUiPreferences } = await import("#lib/profileTransfer.ts");
  let finish!: () => void;
  const firstWrite = new Promise<void>((resolve) => { finish = resolve; });
  mocks.invoke.mockReturnValueOnce(firstWrite).mockResolvedValue(undefined);
  const stored = new Map<string, string>([["kurisu.list.q", "old"]]);
  const storage = { getItem: (key: string) => stored.get(key) ?? null };
  const first = mirrorUiPreferences(storage);
  await Promise.resolve();
  await Promise.resolve();
  stored.set("kurisu.list.q", "latest");
  const flush = flushUiPreferences(storage);
  expect(mocks.invoke).toHaveBeenCalledTimes(1);
  expect(mocks.invoke.mock.calls[0][1].preferences["kurisu.list.q"]).toBe("old");
  finish();
  await Promise.all([first, flush]);
  expect(mocks.invoke).toHaveBeenCalledTimes(2);
  expect(mocks.invoke.mock.calls[1]).toEqual(["mirror_profile_preferences", { preferences: readUiPreferences(storage) }]);
});

test("a failed mirror stops handoff and a retry saves current values", async () => {
  const { flushUiPreferences } = await import("#lib/profileTransfer.ts");
  mocks.invoke.mockRejectedValueOnce(new Error("Disk is full")).mockResolvedValue(undefined);
  const storage = { getItem: (key: string) => key === "kurisu.list.sort" ? "score" : null };
  await expect(flushUiPreferences(storage)).rejects.toThrow("Disk is full");
  await expect(flushUiPreferences(storage)).resolves.toBeUndefined();
  expect(mocks.invoke).toHaveBeenLastCalledWith("mirror_profile_preferences", { preferences: readUiPreferences(storage) });
});

test("a final flush waits for preference changes queued while it saves", async () => {
  const { mirrorUiPreferences, flushUiPreferences } = await import("#lib/profileTransfer.ts");
  let finish!: () => void;
  const write = new Promise<void>((resolve) => { finish = resolve; });
  mocks.invoke.mockReturnValueOnce(write).mockResolvedValue(undefined);
  let value = "first";
  const storage = { getItem: (key: string) => key === "kurisu.list.q" ? value : null };
  const flush = flushUiPreferences(storage);
  await Promise.resolve();
  await Promise.resolve();
  value = "during save";
  const update = mirrorUiPreferences(storage);
  finish();
  await flush;
  await update;
  expect(mocks.invoke).toHaveBeenLastCalledWith("mirror_profile_preferences", { preferences: readUiPreferences(storage) });
});

test("preference setters and removals automatically mirror the actual nine values", async () => {
  const { preferences } = await import("#lib/preferences.ts");
  const { flushUiPreferences } = await import("#lib/profileTransfer.ts");
  localStorage.clear();
  mocks.invoke.mockResolvedValue(undefined);
  for (const [index, key] of UI_PREFERENCE_KEYS.entries()) preferences.set(key, `choice-${index}`);
  preferences.remove("kurisu.list.q");
  preferences.set("unrelated-private-value", "never-export-this");
  await flushUiPreferences();
  const [command, args] = mocks.invoke.mock.calls.at(-1)!;
  expect(command).toBe("mirror_profile_preferences");
  expect(Object.keys(args.preferences)).toEqual(UI_PREFERENCE_KEYS);
  for (const [index, key] of UI_PREFERENCE_KEYS.entries()) {
    expect(args.preferences[key]).toBe(key === "kurisu.list.q" ? null : `choice-${index}`);
  }
  expect(JSON.stringify(args)).not.toContain("never-export-this");
  localStorage.clear();
});

test("a failed local selection write blocks handoff until that exact value can be saved", async () => {
  const { preferences } = await import("#lib/preferences.ts");
  const { flushUiPreferences } = await import("#lib/profileTransfer.ts");
  localStorage.clear();
  localStorage.setItem("kurisu.list.q", "previous");
  const write = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("Storage quota exceeded"); });
  preferences.set("kurisu.list.q", "latest selection");
  await expect(flushUiPreferences()).rejects.toThrow("Storage quota exceeded");
  expect(mocks.invoke).not.toHaveBeenCalled();
  write.mockRestore();
  mocks.invoke.mockResolvedValue(undefined);
  await flushUiPreferences();
  expect(localStorage.getItem("kurisu.list.q")).toBe("latest selection");
  expect(mocks.invoke).toHaveBeenLastCalledWith("mirror_profile_preferences", {
    preferences: expect.objectContaining({ "kurisu.list.q": "latest selection" }),
  });
  localStorage.clear();
});
