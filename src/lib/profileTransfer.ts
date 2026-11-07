import { invoke } from "@tauri-apps/api/core";

export const UI_PREFERENCE_KEYS = [
  "kurisu.list.sort",
  "kurisu.list.dir",
  "kurisu.list.filter",
  "kurisu.list.q",
  "kurisu.list.synced",
  "kurisu.cal.mine",
  "kurisu.torrents.new",
  "kurisu.torrents.statuses",
  "kurisu.torrents.filters",
] as const;

type UiPreferenceKey = (typeof UI_PREFERENCE_KEYS)[number];
export type UiPreferenceExport = Record<UiPreferenceKey, string | null>;

let mirror: Promise<void> = Promise.resolve();
const failedWrites = new Map<string, string | null>();

export function notePreferenceWriteFailure(key: string, value: string | null): void {
  if ((UI_PREFERENCE_KEYS as readonly string[]).includes(key)) failedWrites.set(key, value);
}

function retryPreferenceWrites(storage: Pick<Storage, "getItem">): void {
  if (storage !== localStorage) return;
  for (const [key, value] of failedWrites) {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
    failedWrites.delete(key);
  }
}

export function mirrorUiPreferences(storage: Pick<Storage, "getItem"> = localStorage): Promise<void> {
  const values = readUiPreferences(storage);
  const write = () => invoke<void>("mirror_profile_preferences", { preferences: values });
  const next = mirror.then(write, write);
  mirror = next;
  return next;
}

export async function flushUiPreferences(storage: Pick<Storage, "getItem"> = localStorage): Promise<void> {
  retryPreferenceWrites(storage);
  mirrorUiPreferences(storage);
  while (true) {
    const pending = mirror;
    await pending;
    if (pending === mirror) return;
  }
}

export function mirrorPreferenceChange(key: string): void {
  if (!(UI_PREFERENCE_KEYS as readonly string[]).includes(key)) return;
  failedWrites.delete(key);
  try {
    void mirrorUiPreferences().catch(() => {});
  } catch {
  }
}

export function readUiPreferences(storage: Pick<Storage, "getItem">): UiPreferenceExport {
  const values = {} as UiPreferenceExport;
  for (const key of UI_PREFERENCE_KEYS) values[key] = storage.getItem(key);
  return values;
}

export function legacyProfileDatabasePath(): Promise<string> {
  return invoke<string>("legacy_profile_database_path");
}

export async function exportUiPreferences(storage: Pick<Storage, "getItem"> = localStorage): Promise<string | null> {
  retryPreferenceWrites(storage);
  return invoke<string | null>("export_profile_preferences", {
    preferences: readUiPreferences(storage),
  });
}
