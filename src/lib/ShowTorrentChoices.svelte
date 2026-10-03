<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import type { ListEntry, ShowTorrents, TorrentItem } from "$lib/types";
  import TorrentRow from "$lib/TorrentRow.svelte";

  let { entry, category, filter, feedItems, seenLinks = new Set<string>(), seenIdentities = new Set<string>(), cache, onopen }: {
    entry: ListEntry;
    category: string;
    filter: string;
    feedItems: TorrentItem[];
    seenLinks?: ReadonlySet<string>;
    seenIdentities?: ReadonlySet<string>;
    cache: Map<string, { savedAt: number; choices: ShowTorrents }>;
    onopen: (item: TorrentItem, url: string) => Promise<boolean>;
  } = $props();

  let choices = $state<ShowTorrents | null>(null);
  let loading = $state(false);
  let error = $state("");
  let retry = $state(0);
  let expandedSections = $state<Set<string>>(new Set());
  let requestId = 0;
  onDestroy(() => { requestId++; });
  const requestKey = $derived(JSON.stringify([
    auth.epoch, entry.media_id, entry.progress, entry.status, entry.media, category, filter,
  ]));

  const sections = $derived((choices ? [
    { title: "Batches", items: choices.batches },
    { title: choices.next_episode == null ? "Episodes" : `Next episode · ${choices.next_episode}`, items: choices.episodes },
    { title: "Other releases", items: choices.other },
  ] : []).map((section) => ({
    ...section,
    items: section.items.map((item) => seenLinks.has(item.link) || seenIdentities.has(item.seen_guid ?? item.guid) ? { ...item, seen: true } : item),
  })));
  const foundLinks = $derived(new Set(sections.flatMap((section) => section.items.map((item) => item.link))));
  const foundIdentities = $derived(new Set(sections.flatMap((section) => section.items.map((item) => item.seen_guid ?? item.guid))));
  const remainingFeed = $derived(feedItems.filter((item) => !foundLinks.has(item.link) && !foundIdentities.has(item.seen_guid ?? item.guid)));
  const foundCount = $derived(sections.reduce((count, section) => count + section.items.length, 0));

  async function openChoice(item: TorrentItem, url: string) {
    const id = requestId;
    const key = requestKey;
    if (await onopen(item, url) && choices && requestId === id && requestKey === key) {
      for (const section of [choices.batches, choices.episodes, choices.other]) {
        const selected = section.find((choice) => choice.guid === item.guid);
        if (selected) selected.seen = true;
      }
      cache.set(key, { savedAt: Date.now(), choices: $state.snapshot(choices) });
    }
  }

  function toggleSection(title: string) {
    const next = new Set(expandedSections);
    if (next.has(title)) next.delete(title);
    else next.add(title);
    expandedSections = next;
  }

  $effect(() => {
    const key = requestKey;
    const epoch = auth.epoch;
    const loggedIn = auth.isLoggedIn;
    const [mediaId, selectedCategory, selectedFilter] = untrack(() => [entry.media_id, category, filter] as const);
    const force = retry > 0;
    requestId++;
    let active = true;
    choices = null;
    expandedSections = new Set();
    error = "";
    loading = loggedIn;
    if (!loggedIn) return;
    const cached = cache.get(key);
    if (!force && cached && Date.now() - cached.savedAt < 5 * 60_000) {
      choices = cached.choices;
      loading = false;
      return;
    }
    const current = () => active && epoch === auth.epoch && auth.isLoggedIn;
    api.findShowTorrents(mediaId, selectedCategory, selectedFilter).then((result) => {
      if (current()) {
        choices = result;
        cache.delete(key);
        cache.set(key, { savedAt: Date.now(), choices: result });
        if (cache.size > 50) cache.delete(cache.keys().next().value!);
      }
    }).catch((e) => {
      if (current()) error = String(e);
    }).finally(() => {
      if (current()) loading = false;
    });
    return () => { active = false; };
  });
</script>

<div class="border-b border-edge px-3 py-2.5 flex items-center gap-3">
  <p class="text-xs text-ink-dim flex-1" aria-live="polite">
    {#if loading}
      Finding batches and episode options…
    {:else if choices}
      {foundCount} download option{foundCount === 1 ? "" : "s"} · Choose a release group below
    {:else}
      Batches and episode options
    {/if}
  </p>
  <button type="button" onclick={() => retry++} disabled={loading}
    class="text-xs text-accent hover:underline disabled:opacity-50 shrink-0">Search again</button>
</div>
{#if error}
  <p class="px-3 py-3 text-sm text-red-400" role="alert">Could not find download options: {error}</p>
{/if}
{#if choices}
  {#each choices.warnings as warning}
    <p class="px-3 py-2 text-xs text-amber-400">{warning}</p>
  {/each}
  {#each sections as section}
    {#if section.items.length > 0}
      <section aria-label={section.title}>
        <h3 class="px-3 py-2 text-xs font-semibold text-ink-dim bg-panel-2 border-y border-edge">{section.title}</h3>
        <div class="divide-y divide-edge/60">
          {#each expandedSections.has(section.title) ? section.items : section.items.slice(0, 6) as item (item.guid)}
            <TorrentRow torrent={item} onopen={(url) => openChoice(item, url)} />
          {/each}
        </div>
        {#if section.items.length > 6}
          <button type="button" onclick={() => toggleSection(section.title)}
            aria-expanded={expandedSections.has(section.title)}
            class="px-3 py-2 text-xs text-accent hover:underline">
            {expandedSections.has(section.title) ? "Show fewer" : `Show all ${section.items.length}`} {section.title.toLowerCase()}
          </button>
        {/if}
      </section>
    {/if}
  {/each}
  {#if choices.batches.length === 0 && choices.episodes.length === 0}
    <p class="px-3 py-3 text-sm text-ink-dim">
      {choices.next_episode == null
        ? "No batches found with these filters. Your progress is already at the known episode total."
        : `No batches or episode ${choices.next_episode} releases found with these filters.`}
    </p>
  {/if}
{/if}
{#if remainingFeed.length > 0}
  <section aria-label="Recent feed releases">
    <h3 class="px-3 py-2 text-xs font-semibold text-ink-dim bg-panel-2 border-y border-edge">Recent feed releases</h3>
    <div class="divide-y divide-edge/60">
      {#each remainingFeed as item (item.guid)}
        <TorrentRow torrent={item} onopen={(url) => onopen(item, url)} />
      {/each}
    </div>
  </section>
{/if}
