<script module lang="ts">
  import { api } from "$lib/api";

  // Retain the reconciled account across route changes.
  let syncedFor: number | null = null;

  let autoSynced = false;
  let autoSyncedEpoch = 0;
  let pendingSync: { epoch: number; promise: ReturnType<typeof api.syncMyList> } | null = null;

  const COLLATOR = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });
</script>

<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { untrack } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { preferences } from "$lib/preferences";
  import { auth } from "$lib/auth.svelte";
  import { nowMs } from "$lib/now.svelte";
  import {
    airingLabel,
    displayTitle,
    scoreLabel,
    STATUS_LABEL,
    timeAgo,
    type ListEntry,
  } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import EditEntry from "$lib/EditEntry.svelte";
  import EpisodeStepper from "$lib/EpisodeStepper.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import Select from "$lib/Select.svelte";

  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let syncing = $state(false);
  let error = $state("");
  let stepError = $state("");
  let editing = $state<ListEntry | null>(null);
  const loggedIn = $derived(auth.isLoggedIn);

  const statuses = ["CURRENT", "PLANNING", "COMPLETED", "PAUSED", "DROPPED", "REPEATING"];
  const statusColors: Record<string, string> = {
    CURRENT: "var(--color-sage)", PLANNING: "var(--color-blue)",
    COMPLETED: "var(--color-ochre)", PAUSED: "var(--color-accent)",
    DROPPED: "#b18992", REPEATING: "#aa98bd",
  };
  const watchedEpisodes = $derived(entries.reduce((sum, entry) => sum + entry.progress, 0));

  type SortKey = "title" | "score" | "progress" | "updated" | "airing";
  const SORT_OPTIONS: Array<{ value: SortKey; label: string }> = [
    { value: "title", label: "Title" },
    { value: "score", label: "Score" },
    { value: "progress", label: "Progress" },
    { value: "updated", label: "Last updated" },
    { value: "airing", label: "Next airing" },
  ];
  const SORT_DEFAULT_DESC: Record<SortKey, boolean> = {
    title: false,
    score: true,
    progress: true,
    updated: true,
    airing: false,
  };
  function readSort(): SortKey {
    const v = preferences.get("kurisu.list.sort");
    return SORT_OPTIONS.some((o) => o.value === v) ? (v as SortKey) : "title";
  }
  function readDesc(key: SortKey): boolean {
    const v = preferences.get("kurisu.list.dir");
    return v === null ? SORT_DEFAULT_DESC[key] : v === "desc";
  }
  function readFilter(): string {
    const v = preferences.get("kurisu.list.filter");
    return v !== null && statuses.includes(v) ? v : "CURRENT";
  }
  let filter = $state(readFilter());
  let q = $state(preferences.get("kurisu.list.q") ?? "");
  let sortKey = $state<SortKey>(readSort());
  let sortDesc = $state(readDesc(readSort()));
  function persistSort() {
    preferences.set("kurisu.list.sort", sortKey);
    preferences.set("kurisu.list.dir", sortDesc ? "desc" : "asc");
  }
  function pickSort(k: SortKey) {
    sortDesc = SORT_DEFAULT_DESC[k];
    persistSort();
  }
  function flipDir() {
    sortDesc = !sortDesc;
    persistSort();
  }
  $effect(() => {
    preferences.set("kurisu.list.filter", filter);
    preferences.set("kurisu.list.q", q);
  });

  function readSyncedAt(): number {
    const v = Number(preferences.get("kurisu.list.synced"));
    return Number.isFinite(v) && v > 0 ? v : 0;
  }
  let syncedAt = $state(readSyncedAt());
  let syncedFlash = $state(false);
  let flashTimer: ReturnType<typeof setTimeout> | null = null;
  function markSynced() {
    // Use the shared clock so the saved time cannot be ahead of relative labels.
    syncedAt = nowMs();
    preferences.set("kurisu.list.synced", String(syncedAt));
    syncedFlash = true;
    if (flashTimer) clearTimeout(flashTimer);
    flashTimer = setTimeout(() => {
      syncedFlash = false;
      flashTimer = null;
    }, 3000);
  }
  const syncLabel = $derived.by(() => {
    if (!syncedAt) return "";
    const t = timeAgo(Math.floor(syncedAt / 1000));
    return t === "just now" ? "Synced just now" : `Synced ${t} ago`;
  });

  function matchesQuery(e: ListEntry, needle: string): boolean {
    const m = e.media;
    if (!m) return false;
    return [m.title_english, m.title_romaji, m.title_native].some((t) =>
      t?.toLowerCase().includes(needle)
    );
  }

  const titleCmp = (a: ListEntry, b: ListEntry) =>
    COLLATOR.compare(displayTitle(a.media), displayTitle(b.media));

  function keyVal(e: ListEntry): number | null {
    switch (sortKey) {
      case "score":
        return e.score && e.score > 0 ? e.score : null;
      case "progress":
        return e.progress;
      case "updated":
        return e.updated_at ?? null;
      case "airing": {
        const at = e.media?.next_airing_at;
        return at && at * 1000 > nowMs() ? at : null;
      }
      default:
        return null;
    }
  }

  const visible = $derived.by(() => {
    const needle = q.trim().toLowerCase();
    const filtered = entries.filter(
      (e) => e.status === filter && (!needle || matchesQuery(e, needle))
    );
    if (sortKey === "title") {
      return filtered.sort((a, b) => (sortDesc ? -titleCmp(a, b) : titleCmp(a, b)));
    }
    return filtered.sort((a, b) => {
      const va = keyVal(a);
      const vb = keyVal(b);
      if (va == null && vb == null) return titleCmp(a, b);
      if (va == null) return 1;
      if (vb == null) return -1;
      const r = sortDesc ? vb - va : va - vb;
      return r !== 0 ? r : titleCmp(a, b);
    });
  });

  let loadId = 0;
  let syncId = 0;
  async function load() {
    if (!auth.isLoggedIn) return;
    const id = ++loadId;
    const epoch = auth.epoch;
    const current = () => id === loadId && epoch === auth.epoch && auth.isLoggedIn;
    loading = true;
    error = "";
    const uid = auth.user?.id ?? null;
    const switched = uid !== null && syncedFor !== null && uid !== syncedFor;
    if (switched) {
      entries = [];
      filter = "CURRENT";
      q = "";
      syncedAt = 0;
      preferences.remove("kurisu.list.filter");
      preferences.remove("kurisu.list.q");
      preferences.remove("kurisu.list.synced");
    }
    try {
      if (pendingSync?.epoch === epoch) {
        await sync();
        return;
      }
      const list = await api.localEntries();
      if (!current()) return;
      entries = switched ? [] : list;
      syncedFor = uid;
      if (auth.epoch !== autoSyncedEpoch) {
        autoSynced = false;
        autoSyncedEpoch = auth.epoch;
      }
      if ((list.length === 0 && !autoSynced) || switched) {
        autoSynced = true;
        await sync();
      }
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (current()) loading = false;
    }
  }

  async function sync() {
    if (!auth.isLoggedIn) return;
    const id = ++syncId;
    const epoch = auth.epoch;
    const current = () => id === syncId && epoch === auth.epoch && auth.isLoggedIn;
    syncing = true;
    error = "";
    const request = pendingSync?.epoch === epoch
      ? pendingSync
      : { epoch, promise: api.syncMyList() };
    pendingSync = request;
    try {
      const list = await request.promise;
      if (current()) {
        loadId++;
        loading = false;
        entries = list;
        syncedFor = auth.user?.id ?? null;
        markSynced();
      }
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (pendingSync === request) pendingSync = null;
      if (current()) syncing = false;
    }
  }

  function applyEntry(entry: ListEntry) {
    entries = entries.map((x) =>
      x.media_id === entry.media_id ? { ...entry, media: entry.media ?? x.media } : x
    );
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    entries = [];
    editing = null;
    loading = false;
    syncing = false;
    error = "";
    stepError = "";
    syncedFlash = false;
    if (flashTimer) clearTimeout(flashTimer);
    return () => { loadId++; syncId++; };
  });

  $effect(() => {
    auth.epoch;
    if (auth.isLoggedIn) untrack(() => load());
  });

  $effect(() => {
    const epoch = auth.epoch;
    if (!auth.isLoggedIn) return;
    let alive = true;
    let un: (() => void) | undefined;
    let debounce: ReturnType<typeof setTimeout> | null = null;
    listen("kurisu://episode-updated", () => {
      if (!alive || epoch !== auth.epoch || !auth.isLoggedIn) return;
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => {
        debounce = null;
        if (alive && epoch === auth.epoch && auth.isLoggedIn) void load();
      }, 300);
    }).then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
      if (debounce) clearTimeout(debounce);
    };
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content collection-page">
    <PageHeading section="Your collection" title="My List" description={`${entries.length.toLocaleString()} Titles · ${watchedEpisodes.toLocaleString()} Episodes Tracked`} />
    <div class="collection-toolbar">
      <label class="list-search">
        <Icon name="search" size={15} />
        <span class="sr-only">Filter your list</span>
      <input
        bind:value={q}
        placeholder="Find a title on your shelf…"
        class="min-w-0 w-full bg-transparent text-sm focus:outline-none"
      />
      </label>
      <span class="eyebrow sort-label">Sort by</span>
      <Select bind:value={sortKey} options={SORT_OPTIONS} class="w-36" onchange={pickSort} />
      <button
        onclick={flipDir}
        title={sortDesc ? "Descending — click for ascending" : "Ascending — click for descending"}
        class="toolbar-button"
      >
        {sortDesc ? "↓" : "↑"}
      </button>
      <button
        onclick={() => sync()}
        disabled={syncing}
        class="toolbar-button sync-button disabled:opacity-50 flex items-center gap-1.5"
      >
        {#if syncing}Syncing…{:else}<Icon name="refresh" size={14} /> Sync{/if}
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if stepError}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4 flex items-center justify-between gap-2">
        <span>Episode update failed: {stepError}</span>
        <button onclick={() => (stepError = "")} class="text-ink-dim hover:text-ink shrink-0">
          <Icon name="x" size={14} />
        </button>
      </div>
    {/if}

    <div class="status-tabs" aria-label="List status">
      {#each statuses as s}
        {@const count = entries.filter((e) => e.status === s).length}
        <button
          onclick={() => (filter = s)}
          aria-pressed={filter === s}
          style:--status-color={statusColors[s]}
          class="status-tab {filter === s ? 'selected' : ''}"
        >
          <span class="status-dot" aria-hidden="true"></span>
          {STATUS_LABEL[s]} <span class="status-count">{count}</span>
        </button>
      {/each}
    </div>

    {#if loading && entries.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if visible.length === 0}
      <div class="empty-shelf">
        <span class="eyebrow">{q.trim() ? "No matching titles" : "A little room on the shelf"}</span>
        <p>{q.trim() ? "Try another title." : "Your next story is still out there."}</p>
        {#if !q.trim()}<a href="/search">Find something to watch <span aria-hidden="true">↗</span></a>{/if}
      </div>
    {:else}
      <div class="list-columns eyebrow" aria-hidden="true"><span>Title / {STATUS_LABEL[filter]}</span><span>Episode progress</span></div>
      <div class="collection-rows">
        {#each visible as e, index (e.media_id)}
          {@const air = airingLabel(e.media)}
          {@const sc = scoreLabel(e.score, auth.user?.score_format)}
          <div
            onclick={() => (editing = e)}
            onkeydown={(ev) => {
              // Let nested buttons handle their own keyboard activation.
              if (ev.currentTarget !== ev.target) return;
              if (ev.key === "Enter" || ev.key === " ") {
                ev.preventDefault();
                editing = e;
              }
            }}
            role="button"
            tabindex="0"
            class="cv-row collection-row cursor-pointer focus:outline-none focus:ring-1 focus:ring-accent"
          >
            <span class="row-number" aria-hidden="true">{String(index + 1).padStart(2, '0')}</span>
            {#if e.media?.cover_medium}
              <button
                type="button"
                onclick={(ev) => {
                  ev.stopPropagation();
                  goto(`/anime/${e.media_id}`);
                }}
                title="Open details"
                class="shrink-0"
              >
                <Img src={e.media.cover_medium} class="list-cover object-cover" />
              </button>
            {:else}
              <div class="list-cover empty-cover shrink-0" aria-hidden="true">ク</div>
            {/if}
            <div class="flex-1 min-w-0">
              <div class="row-title">{displayTitle(e.media)}</div>
              <div class="row-meta flex flex-wrap items-center gap-x-2 gap-y-1">
                {#if e.media?.format}<span>{e.media.format.replaceAll('_', ' ')}</span>{/if}
                {#if e.media?.season_year}<span>{e.media.season_year}</span>{/if}
                {#if sc}<span class="row-score">{sc}</span>{/if}
                {#if air}<span class="row-airing">{air}</span>{/if}
              </div>
            </div>
            <div class="row-progress shrink-0">
              <EpisodeStepper
                mediaId={e.media_id}
                progress={e.progress}
                total={e.media?.episodes ?? null}
                onchange={(entry) => { stepError = ""; applyEntry(entry); }}
                onerror={(msg) => { stepError = msg; }}
              />
              {#if e.media?.episodes && e.media.episodes > 0}
                <div class="episode-track" aria-hidden="true"><span style:width="{Math.min(100, e.progress / e.media.episodes * 100)}%" style:background={statusColors[filter]}></span></div>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    {/if}
    <footer class="collection-footer eyebrow">
      <span>{visible.length} {visible.length === 1 ? 'title' : 'titles'} / {STATUS_LABEL[filter]}</span>
      <span aria-live="polite">{syncedFlash ? 'Synced ✓' : syncLabel || 'Cached on this device'}</span>
    </footer>
  </div>
{/if}

<style>
  .collection-toolbar { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-bottom: 22px; }
  .list-search { display: flex; align-items: center; gap: 10px; min-width: 160px; flex: 1; color: var(--color-ink-dim); padding: 10px 12px; border: 1px solid var(--color-edge); background: var(--color-panel); }
  .list-search:focus-within { border-color: var(--color-accent); }
  .list-search input { font-size: 15px; }
  .sort-label { margin-left: 12px; font-size: 12px; }
  .toolbar-button { height: 42px; padding: 0 13px; border: 1px solid var(--color-edge); background: transparent; color: var(--color-ink-dim); font-size: 15px; }
  .toolbar-button:hover { background: var(--color-panel-2); color: var(--color-ink); }
  .sync-button { color: var(--color-accent); }
  .status-tabs { display: flex; flex-wrap: wrap; gap: 0 16px; border-bottom: 1px solid var(--color-edge); margin-bottom: 16px; }
  .status-tab { display: flex; align-items: baseline; gap: 6px; padding: 10px 0 12px; border-bottom: 2px solid transparent; margin-bottom: -1px; font-size: 14px; color: var(--color-ink-dim); white-space: nowrap; }
  .status-tab:hover, .status-tab.selected { color: var(--color-ink); }
  .status-tab.selected { border-bottom-color: var(--status-color); }
  .status-dot { align-self: center; width: 5px; height: 5px; border: 1px solid var(--status-color); border-radius: 50%; }
  .selected .status-dot { background: var(--status-color); }
  .status-count { font: 13px var(--font-mono); color: var(--status-color); padding-left: 3px; }
  .list-columns { display: flex; justify-content: space-between; padding: 4px 16px 12px 38px; font-size: 12px; }
  .collection-rows { border-top: 1px solid var(--color-edge); }
  .collection-row { display: flex; align-items: center; gap: 15px; min-height: 90px; padding: 11px 13px 11px 0; border-bottom: 1px solid var(--color-edge); }
  .collection-row:hover { background: var(--color-panel-2); }
  .row-number { width: 23px; flex-shrink: 0; font: 12px var(--font-mono); color: var(--color-ink-dim); text-align: center; }
  .collection-row :global(.list-cover) { width: 46px; height: 63px; border: 1px solid var(--color-edge); }
  .empty-cover { display: grid; place-items: center; background: var(--color-panel-2); color: var(--color-sage); font-size: 24px; }
  .row-title { font-size: 15px; line-height: 1.4; color: var(--color-ink); }
  .row-meta { margin-top: 7px; font: 13px var(--font-mono); color: var(--color-ink-dim); }
  .row-meta > span + span::before { content: '/'; margin-right: 8px; color: var(--color-ink-dim); }
  .row-score { color: var(--color-ochre); }
  .row-airing { color: var(--color-blue); }
  .row-progress { width: 150px; }
  .episode-track { height: 2px; background: var(--color-edge); margin: 9px 0 0 16px; }
  .episode-track span { display: block; height: 100%; }
  .collection-footer { display: flex; flex-wrap: wrap; justify-content: space-between; gap: 12px; padding: 18px 0; font-size: 12px; letter-spacing: .06em; }
  .empty-shelf { padding: 50px 15px; text-align: center; border-bottom: 1px solid var(--color-edge); }
  .empty-shelf p { font: 24px var(--font-serif); margin: 13px 0 20px; }
  .empty-shelf a { color: var(--color-accent); font-size: 15px; }
  @media (max-width: 960px) {
    .sort-label { display: none; }
    .status-tabs { gap: 0 12px; }
    .collection-row { gap: 10px; }
    .row-number { width: 18px; }
    .row-progress { width: 145px; }
  }
</style>

{#if editing}
  <EditEntry
    entry={editing}
    scoreFormat={auth.user?.score_format ?? null}
    onclose={() => { editing = null; load(); }}
  />
{/if}
