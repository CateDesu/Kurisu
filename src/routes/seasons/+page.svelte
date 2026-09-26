<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { afterNavigate } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import type { ListEntry, Media } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import AnimeCard from "$lib/AnimeCard.svelte";
  import { addToList } from "$lib/list";
  import type { Snapshot } from "./$types";

  const SEASONS = ["WINTER", "SPRING", "SUMMER", "FALL"] as const;
  const SEASON_LABEL: Record<string, string> = {
    WINTER: "Winter",
    SPRING: "Spring",
    SUMMER: "Summer",
    FALL: "Fall",
  };

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
  let entriesError = $state("");
  let adding = $state<number | null>(null);
  const loggedIn = $derived(auth.isLoggedIn);

  const onList = $derived(new Map(entries.map((e) => [e.media_id, e])));

  export const snapshot: Snapshot<{
    season: string;
    year: number;
    media: Media[] | null;
  }> = {
    capture: () => ({ season, year, media: loadedOnce ? media : null }),
    restore: (v) => {
      season = v.season;
      year = v.year;
      media = v.media ?? [];
      loadId++;
      loadedOnce = v.media !== null;
      loading = false;
      error = "";
    },
  };

  let loadId = 0;
  let entriesLoadId = 0;
  let addId = 0;
  let loadedOnce = false;
  let navigationReady = false;
  let alive = true;

  async function refreshEntries() {
    const id = ++entriesLoadId;
    const epoch = auth.epoch;
    const current = () => alive && id === entriesLoadId && epoch === auth.epoch && auth.isLoggedIn;
    entriesError = "";
    try {
      const list = await api.localEntries();
      if (current()) entries = list;
    } catch (e) {
      if (current()) entriesError = String(e);
    }
  }

  async function load() {
    const id = ++loadId;
    const epoch = auth.epoch;
    const current = () => alive && id === loadId && epoch === auth.epoch && auth.isLoggedIn;
    loadedOnce = false;
    media = [];
    loading = true;
    error = "";
    try {
      const seasonMedia = await api.getSeason(season, year);
      if (!current()) return;
      media = seasonMedia;
      loadedOnce = true;
    } catch (e) {
      if (current()) {
        error = String(e);
      }
    } finally {
      if (current()) loading = false;
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
    void refreshEntries();
  }

  async function add(m: Media, status: string) {
    const id = ++addId;
    const epoch = auth.epoch;
    const current = () => alive && epoch === auth.epoch && auth.isLoggedIn;
    adding = m.id;
    error = "";
    try {
      const entry = await addToList(m, status);
      if (!current()) return;
      entries = [...entries.filter((e) => e.media_id !== m.id), entry];
      void refreshEntries();
    } catch (e) {
      if (current() && id === addId) error = String(e);
    } finally {
      if (current() && id === addId) adding = null;
    }
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    untrack(() => {
      loadId++;
      entriesLoadId++;
      addId++;
      if (loading) loadedOnce = false;
      entries = [];
      loading = false;
      adding = null;
      error = "";
      entriesError = "";
    });
  });

  $effect(() => {
    auth.epoch;
    if (!auth.isLoggedIn) return;
    untrack(() => {
      if (!navigationReady) return;
      if (!loadedOnce && !loading) void load();
      void refreshEntries();
    });
  });

  afterNavigate(() => {
    // Snapshots restore after navigation callbacks.
    queueMicrotask(() => {
      if (!alive) return;
      navigationReady = true;
      if (!auth.isLoggedIn) return;
      if (!loadedOnce && !loading) void load();
      void refreshEntries();
    });
  });

  onDestroy(() => { alive = false; });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <div class="flex flex-wrap items-center gap-3 mb-5">
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

    {#if error || entriesError}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error || entriesError}
      </div>
    {/if}

    {#if loading}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if media.length === 0}
      {#if !error}
        <div class="text-ink-dim py-10 text-center">Nothing found for this season.</div>
      {/if}
    {:else}
      <div class="media-grid">
        {#each media as m (m.id)}
          <AnimeCard media={m} entry={onList.get(m.id)} adding={adding === m.id} onadd={add} />
        {/each}
      </div>
    {/if}
  </div>
{/if}
