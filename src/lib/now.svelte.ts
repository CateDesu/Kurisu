let now = $state(Date.now());

export function nowMs(): number {
  return now;
}

export function runClock(intervalMs = 30_000): () => void {
  const publish = () => {
    if (document.visibilityState === "visible") now = Date.now();
  };
  const t = setInterval(publish, intervalMs);
  document.addEventListener("visibilitychange", publish);
  return () => {
    clearInterval(t);
    document.removeEventListener("visibilitychange", publish);
  };
}
