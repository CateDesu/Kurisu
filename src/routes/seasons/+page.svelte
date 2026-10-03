<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { onDestroy, untrack } from "svelte";
  import { afterNavigate } from "$app/navigation";
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { displayTitle, STATUS_LABEL, type ListEntry, type Media } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import AnimeCard from "$lib/AnimeCard.svelte";
  import Select from "$lib/Select.svelte";
  import { createListActions } from "$lib/list.svelte";
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
  let selectedSeason = $state(currentSeason().season);
  let selectedYear = $state(currentSeason().year);
  let titleFilter = $state("");
  let formatFilter = $state("ALL");
  let listFilter = $state("ALL");
  let media = $state<Media[]>([]);
  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let error = $state("");
  let entriesError = $state("");
  let entriesLoaded = $state(false);
  const actions = createListActions();
  const loggedIn = $derived(auth.isLoggedIn);

  const onList = $derived(new Map(entries.map((e) => [e.media_id, e])));
  const seasonOptions = SEASONS.map((value) => ({ value, label: SEASON_LABEL[value] }));
  const listOptions = [{ value: "ALL", label: "Any list status" }, { value: "UNLISTED", label: "Not on my list" },
    ...Object.entries(STATUS_LABEL).map(([value, label]) => ({ value, label }))];
  const formatOptions = [{ value: "ALL", label: "All formats" }, ...["TV", "TV_SHORT", "MOVIE", "SPECIAL", "OVA", "ONA", "MUSIC"].map((value) => ({ value, label: value.replaceAll("_", " ") }))];
  const visible = $derived(media.filter((item) => {
    const needle = titleFilter.trim().toLowerCase();
    return (!needle || [displayTitle(item), item.title_romaji, item.title_native].some((title) => title?.toLowerCase().includes(needle)))
      && (formatFilter === "ALL" || item.format === formatFilter)
      && (listFilter === "ALL" || (listFilter === "UNLISTED" ? !onList.has(item.id) : onList.get(item.id)?.status === listFilter));
  }));

  export const snapshot: Snapshot<{
    season: string;
    year: number;
    media: Media[] | null;
    titleFilter: string;
    formatFilter: string;
    listFilter: string;
  }> = {
    capture: () => ({ season, year, media: loadedOnce ? media : null, titleFilter, formatFilter, listFilter }),
    restore: (v) => {
      season = v.season;
      year = v.year;
      selectedSeason = v.season;
      selectedYear = v.year;
      titleFilter = v.titleFilter ?? "";
      formatFilter = v.formatFilter ?? "ALL";
      listFilter = v.listFilter ?? "ALL";
      media = v.media ?? [];
      loadId++;
      loadedOnce = v.media !== null;
      loading = false;
      error = "";
    },
  };

  let loadId = 0;
  let entriesLoadId = 0;
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
      if (current()) { entries = list; entriesLoaded = true; }
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
    selectedSeason = season;
    selectedYear = year;
    load();
    void refreshEntries();
  }

  function jump() {
    if (!Number.isInteger(selectedYear) || selectedYear < 1 || selectedYear > 9999) {
      error = "Enter a valid year.";
      return;
    }
    season = selectedSeason;
    year = selectedYear;
    void load();
    void refreshEntries();
  }

  async function add(m: Media, status: string) {
    error = "";
    const entry = await actions.add(m, status);
    if (!entry || !alive) return;
    entries = [...entries.filter((e) => e.media_id !== m.id), entry];
    void refreshEntries();
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    untrack(() => {
      loadId++;
      entriesLoadId++;
      actions.reset();
      if (loading) loadedOnce = false;
      entries = [];
      entriesLoaded = false;
      loading = false;
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
    });
  });

  afterNavigate(() => {
    // Snapshots restore after navigation callbacks.
    queueMicrotask(() => {
      if (!alive) return;
      navigationReady = true;
      if (!auth.isLoggedIn) return;
      if (!loadedOnce && !loading) void load();
    });
  });

  $effect(() => {
    const epoch = auth.epoch;
    if (!auth.isLoggedIn) return;
    let active = true;
    const stops: (() => void)[] = [];
    const registrations = ["kurisu://episode-updated", "kurisu://pending-changed"].map((event) =>
      listen(event, () => {
        if (active && epoch === auth.epoch) void refreshEntries();
      }).then((stop) => active ? stops.push(stop) : stop()).catch((e) => {
        if (active && epoch === auth.epoch) entriesError = String(e);
      })
    );
    void Promise.all(registrations).then(() => {
      if (active && epoch === auth.epoch) void refreshEntries();
    });
    return () => { active = false; for (const stop of stops) stop(); };
  });

  onDestroy(() => { alive = false; actions.reset(); });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <PageHeading section="Seasons" title={`${SEASON_LABEL[season]} ${year}`} description="Browse the season">
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
    </PageHeading>

    <form onsubmit={(event) => { event.preventDefault(); jump(); }} class="flex items-end gap-2 flex-wrap mb-4">
      <div class="w-36"><label for="season-jump" class="block text-xs text-ink-dim mb-1">Season</label><Select id="season-jump" bind:value={selectedSeason} options={seasonOptions} /></div>
      <div><label for="season-year" class="block text-xs text-ink-dim mb-1">Year</label><input id="season-year" type="number" min="1" max="9999" step="1" bind:value={selectedYear} class="w-24 bg-panel-2 border border-edge rounded px-3 py-2 text-sm" /></div>
      <button type="submit" disabled={loading} class="px-3 py-2 rounded bg-panel-2 text-sm disabled:opacity-50">Go to season</button>
    </form>
    <div class="flex items-center gap-3 flex-wrap mb-4">
      <input aria-label="Filter season titles" bind:value={titleFilter} placeholder="Filter titles…" class="min-w-0 flex-1 bg-panel-2 border border-edge rounded px-3 py-2 text-sm" />
      <Select id="season-format" bind:value={formatFilter} options={formatOptions} class="w-40" />
      <Select id="season-list-status" bind:value={listFilter} options={listOptions} class="w-44" />
      <span class="text-xs text-ink-dim">{visible.length} of {media.length} shows</span>
    </div>

    {#if error || entriesError || actions.error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error || entriesError || actions.error}
      </div>
    {/if}

    {#if loading}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if listFilter !== "ALL" && !entriesLoaded}
      {#if !entriesError}<div class="text-ink-dim py-10 text-center">Loading your list…</div>{/if}
    {:else if visible.length === 0}
      {#if !error}
        <div class="text-ink-dim py-10 text-center">{media.length ? "No shows match these filters." : "Nothing found for this season."}</div>
      {/if}
    {:else}
      <div class="media-grid">
        {#each visible as m (m.id)}
          <AnimeCard media={m} entry={onList.get(m.id)} adding={actions.pending(m.id)} onadd={add} />
        {/each}
      </div>
    {/if}
  </div>
{/if}
