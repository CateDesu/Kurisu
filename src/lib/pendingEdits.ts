const edits = new Map<unknown, () => Promise<void>>();
const removals = new WeakMap<() => Promise<void>, {
  pending: (() => Promise<void>) | undefined;
  active: Set<Promise<unknown>>;
}>();
// Replacing a buffered edit must not stop shutdown from waiting for its active save.
const saves = new Set<Promise<unknown>>();

export function registerPendingEdit(flush: () => Promise<void>, key: unknown = flush) {
  edits.set(key, flush);
  return () => { if (edits.get(key) === flush) edits.delete(key); };
}

export function isPendingEdit(key: unknown, flush: () => Promise<void>) {
  return edits.get(key) === flush;
}

export async function discardPendingEditAfter<T>(key: unknown, removal: Promise<T>): Promise<T> {
  const pending = edits.get(key);
  const suspended = pending && removals.get(pending);
  const state = suspended ?? { pending, active: new Set<Promise<unknown>>() };
  const wait = suspended ? pending : async () => { await Promise.all([...state.active]); };
  state.active.add(removal);
  if (pending && !suspended) {
    removals.set(wait, state);
    edits.set(key, wait);
  }
  try {
    const result = await removal;
    // Any successful removal supersedes the buffered edit shared by these requests.
    state.pending = undefined;
    return result;
  } finally {
    state.active.delete(removal);
    if (!state.active.size && isPendingEdit(key, wait)) {
      if (state.pending) edits.set(key, state.pending);
      else edits.delete(key);
    }
  }
}

export function trackPendingSave<T>(save: Promise<T>): Promise<T> {
  saves.add(save);
  void save.then(() => saves.delete(save), () => saves.delete(save));
  return save;
}

export async function flushPendingEdits() {
  while (edits.size || saves.size) {
    const results = await Promise.allSettled([...saves, ...[...edits.values()].map((flush) => flush())]);
    const failed = results.find((result) => result.status === "rejected");
    if (failed?.status === "rejected") throw failed.reason;
  }
}
