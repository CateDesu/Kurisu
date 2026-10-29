import { displayTitle, type ListEntry, type Media } from "./types.ts";

function sameData(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (!a || !b || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const keys = Object.keys(left);
  return keys.length === Object.keys(right).length && keys.every((key) =>
    Object.hasOwn(right, key) && sameData(left[key], right[key])
  );
}

export function reconcileListEntries(previous: ListEntry[], incoming: ListEntry[]): ListEntry[] {
  if (previous === incoming) return previous;
  const byId = new Map(previous.map((entry) => [entry.media_id, entry]));
  let changed = previous.length !== incoming.length;
  const reconciled = incoming.map((entry, index) => {
    const old = byId.get(entry.media_id);
    let next = entry;
    if (old) {
      if (sameData(old, entry)) next = old;
      else if (entry.media && old.media && sameData(old.media, entry.media)) {
        next = { ...entry, media: old.media };
      }
    }
    if (next !== previous[index]) changed = true;
    return next;
  });
  return changed ? reconciled : previous;
}

export function createTitleSorter(compare: (a: string, b: string) => number) {
  let previous = new Map<number, { title: string; status: string; group: number }>();
  let starts: number[] = [];
  let descending: boolean | undefined;
  return (entries: readonly ListEntry[], desc: boolean): ListEntry[] => {
    const titles = entries.map((entry) => displayTitle(entry.media));
    const reusable = descending === desc && previous.size === entries.length && entries.every((entry, index) => {
      const old = previous.get(entry.media_id);
      return old?.title === titles[index] && old.status === entry.status;
    });
    if (reusable) {
      const offsets = [...starts];
      const sorted = new Array<ListEntry>(entries.length);
      // Fill each collation tie in source order with the current entry references.
      for (const entry of entries) sorted[offsets[previous.get(entry.media_id)!.group]++] = entry;
      return sorted;
    }
    const next = new Map(entries.map((entry, index) => [entry.media_id, {
      title: titles[index], status: entry.status, group: 0,
    }]));
    const sorted = [...entries].sort((a, b) =>
      (desc ? -1 : 1) * compare(next.get(a.media_id)!.title, next.get(b.media_id)!.title)
    );
    starts = [];
    let lastTitle = "";
    sorted.forEach((entry, index) => {
      const key = next.get(entry.media_id)!;
      if (index === 0 || (key.title !== lastTitle && compare(lastTitle, key.title) !== 0)) starts.push(index);
      key.group = starts.length - 1;
      lastTitle = key.title;
    });
    previous = next;
    descending = desc;
    return sorted;
  };
}

const searchableTitles = new WeakMap<Media, string[]>();

export function matchesListQuery(entry: ListEntry, needle: string): boolean {
  const media = entry.media;
  if (!media) return false;
  let titles = searchableTitles.get(media);
  if (!titles) {
    titles = [media.title_english, media.title_romaji, media.title_native]
      .filter((title): title is string => title != null)
      .map((title) => title.toLowerCase());
    searchableTitles.set(media, titles);
  }
  return titles.some((title) => title.includes(needle));
}
