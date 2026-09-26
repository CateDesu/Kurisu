let now = $state(Date.now());

export function nowMs(): number {
  return now;
}

export function runClock(intervalMs = 30_000): () => void {
  const t = setInterval(() => {
    now = Date.now();
  }, intervalMs);
  return () => clearInterval(t);
}
