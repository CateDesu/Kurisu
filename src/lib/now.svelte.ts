// Shared ticking "now" so relative labels like timeAgo and airingLabel refresh
// while a page sits idle instead of going stale until the next navigation.
// The root layout owns the interval. Everyone else just reads nowMs() from
// reactive code, templates or $derived, which subscribes them to the ticks.

let now = $state(Date.now());

/** Current time in ms. Reactive. */
export function nowMs(): number {
  return now;
}

/** Drive the clock from the root layout's $effect. Returns the cleanup. */
export function runClock(intervalMs = 30_000): () => void {
  const t = setInterval(() => {
    now = Date.now();
  }, intervalMs);
  return () => clearInterval(t);
}
