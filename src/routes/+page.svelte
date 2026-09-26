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
  <div class="page-content">
    <div class="flex items-center gap-2 mb-4 flex-wrap">
      <h1 class="text-xl font-semibold flex-1">My List</h1>
      <input
        bind:value={q}
        placeholder="Filter…"
        class="w-40 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
      />
      <Select bind:value={sortKey} options={SORT_OPTIONS} class="w-36" onchange={pickSort} />
      <button
        onclick={flipDir}
        title={sortDesc ? "Descending — click for ascending" : "Ascending — click for descending"}
        class="px-2.5 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
      >
        {sortDesc ? "↓" : "↑"}
      </button>
      <button
        onclick={() => sync()}
        disabled={syncing}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50 flex items-center gap-1.5"
      >
        {#if syncing}Syncing…{:else}<Icon name="refresh" size={14} /> Sync{/if}
      </button>
      {#if syncedFlash}
        <span class="text-xs text-accent">Synced ✓</span>
      {:else if syncLabel}
        <span class="text-xs text-ink-dim">{syncLabel}</span>
      {/if}
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

    <div class="flex flex-wrap gap-1 mb-5 border-b border-edge">
      {#each statuses as s}
        {@const count = entries.filter((e) => e.status === s).length}
        <button
          onclick={() => (filter = s)}
          class="px-3 py-2 text-sm border-b-2 -mb-px transition-colors
            {filter === s ? 'border-accent text-ink' : 'border-transparent text-ink-dim hover:text-ink'}"
        >
          {STATUS_LABEL[s]} <span class="opacity-50">{count}</span>
        </button>
      {/each}
    </div>

    {#if loading && entries.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if visible.length === 0}
      <div class="text-ink-dim py-10 text-center">{q.trim() ? "No matches." : "Nothing here yet."}</div>
    {:else}
      <div class="grid grid-cols-1 gap-2">
        {#each visible as e (e.media_id)}
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
            class="cv-row flex items-center gap-3 bg-panel border border-edge rounded-lg p-2.5 hover:bg-panel-2/60 cursor-pointer focus:outline-none focus:ring-1 focus:ring-accent"
          >
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
                <Img src={e.media.cover_medium} class="w-10 h-14 object-cover rounded" />
              </button>
            {:else}
              <div class="w-10 h-14 bg-panel-2 rounded shrink-0"></div>
            {/if}
            <div class="flex-1 min-w-0">
              <div class="font-medium">{displayTitle(e.media)}</div>
              <div class="text-xs text-ink-dim flex flex-wrap items-center gap-1.5">
                {#if air}<span>{air}</span>{/if}
                {#if air && sc}<span class="opacity-40">·</span>{/if}
                {#if sc}<span>{sc}</span>{/if}
                {#if !air && !sc}<span class="opacity-50">Ep {e.progress}</span>{/if}
              </div>
            </div>
            <div class="shrink-0">
              <EpisodeStepper
                mediaId={e.media_id}
                progress={e.progress}
                total={e.media?.episodes ?? null}
                onchange={(entry) => { stepError = ""; applyEntry(entry); }}
                onerror={(msg) => { stepError = msg; }}
              />
            </div>
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/if}

{#if editing}
  <EditEntry
    entry={editing}
    scoreFormat={auth.user?.score_format ?? null}
    onclose={() => { editing = null; load(); }}
  />
{/if}
