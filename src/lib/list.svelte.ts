import { api } from "$lib/api";
import { auth } from "$lib/auth.svelte";
import type { Media } from "$lib/types";

export async function addToList(media: Media, status: string) {
  const epoch = auth.epoch;
  if (!auth.isLoggedIn) throw "Not signed in.";
  const entry = await api.updateEntry(media.id, status, null, null, null);
  if (epoch !== auth.epoch || !auth.isLoggedIn) throw "Account changed.";
  return entry;
}

export function createListActions() {
  let pending = $state(new Set<number>());
  let error = $state("");
  let generation = 0;
  return {
    get error() { return error; },
    pending: (mediaId: number) => pending.has(mediaId),
    reset() {
      generation++;
      pending = new Set();
      error = "";
    },
    async add(media: Media, status: string) {
      if (pending.has(media.id) || !auth.isLoggedIn) return null;
      const request = generation;
      const epoch = auth.epoch;
      const current = () => request === generation && epoch === auth.epoch && auth.isLoggedIn;
      pending = new Set(pending).add(media.id);
      error = "";
      try {
        const entry = await addToList(media, status);
        return current() ? entry : null;
      } catch (e) {
        if (current()) error = String(e);
        return null;
      } finally {
        if (current()) {
          const next = new Set(pending);
          next.delete(media.id);
          pending = next;
        }
      }
    },
  };
}
