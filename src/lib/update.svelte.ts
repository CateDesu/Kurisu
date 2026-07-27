// Shared install-in-progress state for the updater UI. The always-mounted
// update modal (Updater.svelte) and the Settings page both start installs;
// before this store each kept its own busy flag, so two installs could run
// concurrently and corrupt each other's scratch files. One module-level flag
// serializes them: the modal also shows "busy" while an install started from
// Settings runs, and vice versa.
import { api } from "$lib/api";

let installing = $state(false);

/** True while an install_update command is in flight — reactive. */
export function installInFlight(): boolean {
  return installing;
}

/** Run the install under the shared flag; overlapping calls are refused. */
export async function runInstallUpdate(): Promise<string> {
  if (installing) throw new Error("an update is already being installed");
  installing = true;
  try {
    return await api.installUpdate();
  } finally {
    installing = false;
  }
}
