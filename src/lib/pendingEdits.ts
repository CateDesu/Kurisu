const edits = new Map<unknown, () => Promise<void>>();
// Replacing a buffered edit must not stop shutdown from waiting for its active save.
const saves = new Set<Promise<void>>();

export function registerPendingEdit(flush: () => Promise<void>, key: unknown = flush) {
  edits.set(key, flush);
  return () => { if (edits.get(key) === flush) edits.delete(key); };
}

export function isPendingEdit(key: unknown, flush: () => Promise<void>) {
  return edits.get(key) === flush;
}

export function trackPendingSave(save: Promise<void>) {
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
