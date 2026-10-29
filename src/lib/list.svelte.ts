import { listen } from "@tauri-apps/api/event";
import { api } from "#lib/api.ts";
import { auth } from "#lib/auth.svelte.ts";
import type { ListEntry, Media } from "#lib/types.ts";

export type ListUpdateScope = readonly number[] | undefined;

function changeScope(event: string, payload: unknown): ListUpdateScope {
  if (!payload || typeof payload !== "object") return undefined;
  const value = payload as { media_ids?: unknown; media_id?: unknown };
  const ids = event === "kurisu://episode-updated" ? [value.media_id] : value.media_ids;
  return Array.isArray(ids) && ids.length > 0 && ids.every((id) => Number.isSafeInteger(id) && id > 0)
    ? [...new Set(ids as number[])] : undefined;
}

function mergeScope(left: ListUpdateScope, right: ListUpdateScope): ListUpdateScope {
  return left && right ? [...new Set([...left, ...right])] : undefined;
}

export async function readListUpdate(mediaIds?: readonly number[]) {
  const epoch = auth.epoch;
  const account = auth.user?.id;
  const ids = mediaIds && account != null ? [...mediaIds] : undefined;
  const entries = ids ? await api.localEntryUpdates(ids, account!) : await api.localEntries();
  if (epoch !== auth.epoch || !auth.isLoggedIn) throw "Account changed.";
  return { mediaIds: ids, entries };
}

export function applyListUpdate(previous: readonly ListEntry[], update: Awaited<ReturnType<typeof readListUpdate>>): ListEntry[] {
  if (!update.mediaIds) return update.entries as ListEntry[];
  const changes = new Map(update.mediaIds.map((id, index) => [id, update.entries[index]]));
  const next: ListEntry[] = [];
  for (const entry of previous) {
    const value = changes.has(entry.media_id) ? changes.get(entry.media_id) : entry;
    if (value) next.push(value);
    changes.delete(entry.media_id);
  }
  for (const entry of changes.values()) if (entry) next.push(entry);
  return next;
}

export function listenForListUpdates(
  refresh: (mediaIds?: readonly number[]) => void | Promise<unknown>,
  onError: (error: unknown) => void,
  ready = refresh,
  coalesce?: { invalidate: (mediaIds?: readonly number[]) => void; revision: () => number; delayMs?: number },
) {
  let alive = true;
  let registered = false;
  let queued: ReturnType<typeof setTimeout> | null = null;
  let running = false;
  let dirty = false;
  let scope: ListUpdateScope;
  let activeScope: ListUpdateScope;
  let initialized = false;
  const stops: (() => void)[] = [];
  function schedule() {
    if (!alive || !registered || queued !== null || running || !dirty) return;
    queued = setTimeout(() => { queued = null; void run(); }, coalesce?.delayMs ?? 16);
  }
  async function run() {
    if (!alive || running || !dirty) return;
    dirty = false;
    running = true;
    activeScope = initialized ? scope : undefined;
    let readRevision = coalesce?.revision();
    try {
      const request = refresh(activeScope);
      readRevision = coalesce?.revision();
      await request;
      initialized = true;
    } catch (error) {
      if (alive) onError(error);
    } finally {
      running = false;
      if (alive && coalesce && !dirty && readRevision !== coalesce.revision()) {
        scope = activeScope;
        dirty = true;
      }
      schedule();
    }
  }
  async function initialize() {
    try {
      await ready();
      initialized = true;
    } catch (error) {
      if (alive) onError(error);
    }
  }
  function update(next: ListUpdateScope) {
    if (!alive) return;
    if (!coalesce) { void refresh(next); return; }
    // Invalidate synchronously so an older snapshot cannot settle before the batch.
    coalesce.invalidate(next);
    scope = dirty ? mergeScope(scope, next) : running ? mergeScope(activeScope, next) : next;
    dirty = true;
    if (coalesce.delayMs !== undefined && queued !== null) {
      clearTimeout(queued);
      queued = null;
    }
    schedule();
  }
  const registrations = ["kurisu://episode-updated", "kurisu://pending-changed"].map((event) =>
    listen(event, (message) => update(changeScope(event, message?.payload)))
      .then((stop) => alive ? stops.push(stop) : stop())
      .catch((error) => { if (alive) onError(error); })
  );
  void Promise.all(registrations).then(() => {
    if (!alive) return;
    registered = true;
    if (coalesce && ready === refresh) {
      dirty = true;
      void run();
    } else {
      // Startup can include network requests or a library scan.
      dirty = false;
      void initialize();
      schedule();
    }
  });
  return () => {
    alive = false;
    if (queued !== null) clearTimeout(queued);
    for (const stop of stops) stop();
  };
}

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
