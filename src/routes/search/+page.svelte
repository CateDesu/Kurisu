<script lang="ts">
  import { api } from "$lib/api";
  import { displayTitle, type Media } from "$lib/types";

  let query = $state("");
  let results = $state<Media[]>([]);
  let searching = $state(false);
  let error = $state("");
  let adding = $state<number | null>(null);

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
    try {
      await api.updateEntry(m.id, status, 0, null);
    } catch (err) {
      error = String(err);
    } finally {
      adding = null;
    }
  }
</script>

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
      <div class="bg-panel border border-edge rounded-lg overflow-hidden flex flex-col">
        {#if m.cover_large}
          <img src={m.cover_large} alt="" class="w-full h-44 object-cover" />
        {:else}
          <div class="w-full h-44 bg-panel-2"></div>
        {/if}
        <div class="p-2.5 flex-1 flex flex-col">
          <div class="text-sm font-medium leading-tight line-clamp-2 mb-1">{displayTitle(m)}</div>
          <div class="text-xs text-ink-dim mb-2">
            {#if m.format}{m.format}{/if}
            {#if m.season_year}· {m.season_year}{/if}
            {#if m.episodes}· {m.episodes} eps{/if}
            {#if m.average_score}· ★ {m.average_score}{/if}
          </div>
          <div class="mt-auto flex gap-1 flex-wrap">
            {#each status_options as o}
              <button
                onclick={() => add(m, o.v)}
                disabled={adding === m.id}
                class="text-xs px-2 py-1 rounded bg-panel-2 hover:bg-edge disabled:opacity-50"
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
