<script lang="ts">
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { displayTitle, type Media } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

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
    error = "";
    try {
      // Adding writes status/progress unconditionally — refuse to clobber an
      // entry that's already on the list.
      if (await api.getEntry(m.id)) {
        error = `${displayTitle(m)} is already on your list.`;
        return;
      }
      await api.updateEntry(m.id, status, 0, null, 0);
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
{/if}
