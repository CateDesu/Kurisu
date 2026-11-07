import { flushPendingEdits } from "#lib/pendingEdits.ts";
import { api } from "#lib/api.ts";
import { flushUiPreferences } from "#lib/profileTransfer.ts";

let installing = $state(false);
let restartPending = $state(false);
let closeRejected = false;

export function installInFlight(): boolean {
  return installing;
}

export function updateNeedsRestart(): boolean {
  return restartPending;
}

export function releaseInstallForRetry(): void {
  closeRejected = true;
  if (restartPending) installing = false;
}

export function notePendingRestart(pending: boolean) {
  if (pending) restartPending = true;
}

export async function runInstallUpdate(): Promise<string> {
  if (installing) throw new Error("an update is already being installed");
  if (restartPending) throw new Error("restart Kurisu to finish the installed update");
  closeRejected = false;
  installing = true;
  let restarting = false;
  try {
    await flushPendingEdits();
    await flushUiPreferences();
    const result = await api.installUpdate();
    restarting = result === "restarting";
    if (result === "installed" || result === "restarting") restartPending = true;
    return result;
  } finally {
    installing = restarting && !closeRejected;
  }
}
