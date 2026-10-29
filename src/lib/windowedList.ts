import type { ListEntry } from "./types";

export const estimatedRowHeight = 90;
export type WindowedGeometry = {
  heights: [number, number][];
  width: number;
  anchor?: { id: number; offset: number };
};

export function listLayout(items: readonly ListEntry[], heights: ReadonlyMap<number, number>) {
  const offsets = [0];
  const indices = new Map<number, number>();
  for (let index = 0; index < items.length; index++) {
    const id = items[index].media_id;
    indices.set(id, index);
    offsets.push(offsets[index] + (heights.get(id) ?? estimatedRowHeight));
  }
  return { offsets, indices };
}

export function rowAt(offsets: readonly number[], position: number) {
  let low = 0;
  let high = Math.max(0, offsets.length - 2);
  while (low < high) {
    const middle = Math.ceil((low + high) / 2);
    if (offsets[middle] <= position) low = middle;
    else high = middle - 1;
  }
  return low;
}
