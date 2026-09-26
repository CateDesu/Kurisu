import { api } from "$lib/api";
import { auth } from "$lib/auth.svelte";
import { displayTitle, type Media } from "$lib/types";

export async function addToList(media: Media, status: string) {
  const epoch = auth.epoch;
  if (!auth.isLoggedIn) throw "Not signed in.";
  // Restored badges may be stale.
  const entry = await api.getEntry(media.id);
  if (epoch !== auth.epoch || !auth.isLoggedIn) throw "Account changed.";
  if (entry) throw `${displayTitle(media)} is already on your list.`;
  return api.updateEntry(media.id, status, 0, null, 0);
}
