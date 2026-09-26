import { api } from "$lib/api";

let installing = $state(false);

export function installInFlight(): boolean {
  return installing;
}

export async function runInstallUpdate(): Promise<string> {
  if (installing) throw new Error("an update is already being installed");
  installing = true;
  try {
    return await api.installUpdate();
  } finally {
    installing = false;
  }
}
