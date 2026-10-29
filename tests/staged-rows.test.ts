import { expect, test } from "vitest";
import { appendStagedRows } from "#lib/stagedRows.ts";

test.each([2000, 10000])("appending %i rows has linear writes without copying the mounted prefix", (count) => {
  let writes = 0;
  let reads = 0;
  const rows = new Proxy<{ index: number }[]>([], {
    get(target, property, receiver) {
      if (typeof property === "string" && /^\d+$/.test(property)) reads++;
      return Reflect.get(target, property, receiver);
    },
    set(target, property, value, receiver) {
      if (typeof property === "string" && /^\d+$/.test(property)) writes++;
      return Reflect.set(target, property, value, receiver);
    },
  });
  for (let start = 0; start < count; start += 32) {
    const added = Array.from({ length: Math.min(32, count - start) }, (_, index) => ({ index: start + index }));
    expect(appendStagedRows(rows, added)).toBe(rows);
  }
  expect(writes).toBe(count);
  expect(reads).toBeLessThanOrEqual(Math.ceil(count / 32));
  expect(rows.map((row) => row.index)).toEqual(Array.from({ length: count }, (_, index) => index));
});

test("sparse insertion preserves ordering and existing row identities", () => {
  const existing = [{ index: 1 }, { index: 3 }];
  const added = [{ index: 0 }, { index: 2 }, { index: 4 }];
  const merged = appendStagedRows(existing, added);
  expect(merged.map((row) => row.index)).toEqual([0, 1, 2, 3, 4]);
  expect(merged[1]).toBe(existing[0]);
  expect(merged[3]).toBe(existing[1]);
  expect(appendStagedRows(existing, [])).toBe(existing);
});
