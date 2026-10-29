export function appendStagedRows<T extends { index: number }>(rows: T[], added: T[]): T[] {
  if (!added.length) return rows;
  if (!rows.length || rows[rows.length - 1].index < added[0].index) {
    rows.push(...added);
    return rows;
  }
  const merged: T[] = [];
  let a = 0;
  let b = 0;
  while (a < rows.length || b < added.length) {
    if (b === added.length || (a < rows.length && rows[a].index < added[b].index)) merged.push(rows[a++]);
    else merged.push(added[b++]);
  }
  return merged;
}
