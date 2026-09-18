<script lang="ts">
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { displayTitle, STATUS_LABEL, type ListEntry, type Media } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";
  import type { Snapshot } from "./$types";

  let query = $state("");
  let results = $state<Media[]>([]);
  let searching = $state(false);
  let error = $state("");
  let adding = $state<number | null>(null);

  // Own list status per result, from the local cache. Reassign on change,
  // mutating a Map would not rerun the lookups in the template.
  let listByMedia = $state(new Map<number, ListEntry>());

  $effect(() => {
    if (!auth.isLoggedIn) return;
    api
      .localEntries()
      .then((es) => (listByMedia = new Map(es.map((e) => [e.media_id, e]))))
      .catch((e) => console.error("could not read local list", e));
  });

  /// Badge line for a result that's already on the list, with progress when
  /// there is any. Null when the show is not tracked.
  function listBadge(m: Media): string | null {
    const e = listByMedia.get(m.id);
    if (!e) return null;
    const label = STATUS_LABEL[e.status] ?? e.status;
    if (e.progress <= 0) return label;
    return m.episodes ? `${label} · ${e.progress}/${m.episodes}` : `${label} · ${e.progress}`;
  }

  // Keep the last search alive across detail pages so Back returns to the
  // results instead of the empty form. Fresh visits still start clean.
  export const snapshot: Snapshot<{ query: string; results: Media[] }> = {
    capture: () => ({ query, results }),
    restore: (v) => {
      query = v.query;
      results = v.results;
    },
  };

  const status_options = [
    { v: "CURRENT", label: "Watching" },
    { v: "PLANNING", label: "Plan to watch" },
    { v: "COMPLETED", label: "Completed" },
  ];

  async function run(e: Event) {
    e.preventDefault();
    if (!query.trim()) return;
    searching = true;
    error = "";
    try {
      results = await api.searchAnime(query.trim());
    } catch (err) {
      error = String(err);
    } finally {
      searching = false;
    }
  }

  async function add(m: Media, status: string) {
    adding = m.id;
    error = "";
    try {
      // Adding writes status and progress unconditionally. Refuse to clobber
      // an entry that's already on the list.
      if (await api.getEntry(m.id)) {
        error = `${displayTitle(m)} is already on your list.`;
        return;
      }
      const entry = await api.updateEntry(m.id, status, 0, null, 0);
      listByMedia = new Map(listByMedia).set(m.id, entry);
    } catch (err) {
      error = String(err);
    } finally {
      adding = null;
    }
  }
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
<div class="p-5 max-w-5xl mx-auto">
  <h1 class="text-xl font-semibold mb-4">Search</h1>

  <form onsubmit={run} class="flex gap-2 mb-5">
    <input
      bind:value={query}
      placeholder="Anime title…"
      class="flex-1 bg-panel border border-edge rounded-md px-3 py-2 focus:outline-none focus:border-accent"
    />
    <button class="px-4 py-2 rounded-md bg-accent hover:bg-accent-2 text-white" disabled={searching}>
      {searching ? "…" : "Search"}
    </button>
  </form>

  {#if error}
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
      {error}
    </div>
  {/if}

  <div class="grid grid-cols-2 md:grid-cols-3 gap-3">
    {#each results as m (m.id)}
      {@const entry = listByMedia.get(m.id)}
      <div class="cv-card bg-panel border border-edge rounded-lg overflow-hidden flex flex-col">
        <button type="button" onclick={() => goto(`/anime/${m.id}`)} title="Open details" class="block">
          {#if m.cover_large}
            <Img src={m.cover_large} class="w-full h-44 object-cover" />
          {:else}
            <div class="w-full h-44 bg-panel-2"></div>
          {/if}
        </button>
        <div class="p-2.5 flex-1 flex flex-col">
          <button
            type="button"
            onclick={() => goto(`/anime/${m.id}`)}
            title="Open details"
            class="text-sm font-medium leading-tight line-clamp-2 mb-1 text-left hover:text-accent transition-colors"
          >
            {displayTitle(m)}
          </button>
          <div class="text-xs text-ink-dim mb-2">
            {#if m.format}{m.format}{/if}
            {#if m.season_year}· {m.season_year}{/if}
            {#if m.episodes}· {m.episodes} eps{/if}
            {#if m.average_score}· ★ {m.average_score}{/if}
          </div>
          {#if entry}
            <div class="mb-2">
              <span class="inline-block text-xs px-2 py-0.5 rounded bg-accent/15 text-accent">
                {listBadge(m)}
              </span>
            </div>
          {/if}
          <div class="mt-auto flex gap-1 flex-wrap">
            {#each status_options as o}
              <button
                onclick={() => add(m, o.v)}
                disabled={adding === m.id}
                class="text-xs px-2 py-1 rounded disabled:opacity-50 {entry?.status === o.v
                  ? 'bg-accent text-white'
                  : 'bg-panel-2 hover:bg-edge'}"
              >
                {o.label}
              </button>
            {/each}
          </div>
        </div>
      </div>
    {/each}
  </div>
</div>
{/if}
