// Shared install state for the updater UI. The always mounted update modal
// in Updater.svelte and the Settings page both start installs. Before this
// store each kept its own busy flag, so two installs could run at once and
// corrupt each other's scratch files. One module level flag serializes them.
// The modal also shows "busy" while an install started from Settings runs,
// and vice versa.
import { api } from "$lib/api";

let installing = $state(false);

/** True while an install_update command is in flight. Reactive. */
export function installInFlight(): boolean {
  return installing;
}

/** Run the install under the shared flag. Overlapping calls are refused. */
export async function runInstallUpdate(): Promise<string> {
  if (installing) throw new Error("an update is already being installed");
  installing = true;
  try {
    return await api.installUpdate();
  } finally {
    installing = false;
  }
}
