export function expiredAiringCutoff(expiries: number[], now: number): number {
  let low = 0;
  let high = expiries.length;
  while (low < high) {
    const middle = (low + high) >>> 1;
    if (expiries[middle] <= now) low = middle + 1;
    else high = middle;
  }
  return low ? expiries[low - 1] : -Infinity;
}
