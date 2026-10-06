import { flushPendingEdits } from "$lib/pendingEdits";
import { api } from "$lib/api";

let installing = $state(false);
let restartPending = $state(false);

export function installInFlight(): boolean {
  return installing;
}

export function updateNeedsRestart(): boolean {
  return restartPending;
}

export function notePendingRestart(pending: boolean) {
  if (pending) restartPending = true;
}

export async function runInstallUpdate(): Promise<string> {
  if (installing) throw new Error("an update is already being installed");
  if (restartPending) throw new Error("restart Kurisu to finish the installed update");
  installing = true;
  let restarting = false;
  try {
    await flushPendingEdits();
    const result = await api.installUpdate();
    restarting = result === "restarting";
    if (result === "installed" || result === "restarting") restartPending = true;
    return result;
  } finally {
    installing = restarting;
  }
}
