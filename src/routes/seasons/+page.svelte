<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { displayTitle, STATUS_LABEL, type ListEntry, type Media } from "$lib/types";
  import Login from "$lib/Login.svelte";

  const SEASONS = ["WINTER", "SPRING", "SUMMER", "FALL"] as const;
  const SEASON_LABEL: Record<string, string> = {
    WINTER: "Winter",
    SPRING: "Spring",
    SUMMER: "Summer",
    FALL: "Fall",
  };
  const status_options = [
    { v: "CURRENT", label: "Watching" },
    { v: "PLANNING", label: "Plan to watch" },
    { v: "COMPLETED", label: "Completed" },
  ];

  function currentSeason(): { season: string; year: number } {
    const now = new Date();
    return { season: SEASONS[Math.floor(now.getMonth() / 3)], year: now.getFullYear() };
  }

  let season = $state(currentSeason().season);
  let year = $state(currentSeason().year);
  let media = $state<Media[]>([]);
  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let error = $state("");
  let adding = $state<number | null>(null);

  // media_id → list status, for the on-list badges.
  const onList = $derived(new Map(entries.map((e) => [e.media_id, e.status])));

  async function load() {
    loading = true;
    error = "";
    try {
      media = await api.getSeason(season, year, 1);
      entries = await api.localEntries();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function shift(delta: number) {
    let i = SEASONS.indexOf(season as (typeof SEASONS)[number]) + delta;
    if (i < 0) {
      i = SEASONS.length - 1;
      year -= 1;
    } else if (i >= SEASONS.length) {
      i = 0;
      year += 1;
    }
    season = SEASONS[i];
    load();
  }

  async function add(m: Media, status: string) {
    adding = m.id;
    error = "";
    try {
      const entry = await api.updateEntry(m.id, status, 0, null, 0);
      entries = [...entries.filter((e) => e.media_id !== m.id), entry];
    } catch (e) {
      error = String(e);
    } finally {
      adding = null;
    }
  }

  $effect(() => {
    if (auth.isLoggedIn) untrack(() => load());
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-5xl mx-auto">
    <div class="flex items-center gap-3 mb-5">
      <h1 class="text-xl font-semibold flex-1">
        {SEASON_LABEL[season]} {year}
      </h1>
      <button
        onclick={() => shift(-1)}
        disabled={loading}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
        title="Previous season"
      >
        ← Prev
      </button>
      <button
        onclick={() => shift(1)}
        disabled={loading}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
        title="Next season"
      >
        Next →
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if loading}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if media.length === 0}
      <div class="text-ink-dim py-10 text-center">Nothing found for this season.</div>
    {:else}
      <div class="grid grid-cols-2 md:grid-cols-3 gap-3">
        {#each media as m (m.id)}
          {@const listed = onList.get(m.id)}
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
                {#if m.episodes}· {m.episodes} eps{/if}
                {#if m.average_score}· ★ {m.average_score}{/if}
              </div>
              <div class="mt-auto">
                {#if listed}
                  <span class="text-xs px-2 py-1 rounded bg-panel-2 text-accent">
                    ✓ {STATUS_LABEL[listed] ?? listed}
                  </span>
                {:else}
                  <div class="flex gap-1 flex-wrap">
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
                {/if}
              </div>
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/if}
