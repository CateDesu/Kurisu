<script module lang="ts">
  // Which account the cached list was last reconciled against. Kept at
  // module scope so it survives the route being recreated. As component
  // state it reset on every navigation, so the account-switch check only
  // fired when the user logged out and back in without leaving this page.
  // After a restart or any other route, the previous account's rows
  // rendered as the new one's.
  let syncedFor: number | null = null;

  // Auto-sync on empty fires once per session. Kept at module scope for
  // the same reason. As component state it reset on every mount, so an
  // empty list triggered a full sync on every visit to this tab.
  let autoSynced = false;

  // One collator for the session. localeCompare with options builds a fresh
  // Intl.Collator on every call. The title sort runs O(n log n) over a
  // 1280-entry list on every keystroke.
  const COLLATOR = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });
</script>

<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { nowMs } from "$lib/now.svelte";
  import {
    airingLabel,
    displayTitle,
    scoreLabel,
    STATUS_LABEL,
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
  let filter = $state<string>("CURRENT");
  let editing = $state<ListEntry | null>(null);

  const statuses = ["CURRENT", "PLANNING", "COMPLETED", "PAUSED", "DROPPED", "REPEATING"];

  // filter and sort. Persisted UI state.
  type SortKey = "title" | "score" | "progress" | "updated" | "airing";
  const SORT_OPTIONS: Array<{ value: SortKey; label: string }> = [
    { value: "title", label: "Title" },
    { value: "score", label: "Score" },
    { value: "progress", label: "Progress" },
    { value: "updated", label: "Last updated" },
    { value: "airing", label: "Next airing" },
  ];
  /// Natural direction per key. Picking a key resets to it. The arrow flips it.
  const SORT_DEFAULT_DESC: Record<SortKey, boolean> = {
    title: false,
    score: true,
    progress: true,
    updated: true,
    airing: false,
  };
  function readSort(): SortKey {
    try {
      const v = localStorage.getItem("kurisu.list.sort");
      return SORT_OPTIONS.some((o) => o.value === v) ? (v as SortKey) : "title";
    } catch {
      return "title";
    }
  }
  function readDesc(key: SortKey): boolean {
    try {
      const v = localStorage.getItem("kurisu.list.dir");
      return v === null ? SORT_DEFAULT_DESC[key] : v === "desc";
    } catch {
      return SORT_DEFAULT_DESC[key];
    }
  }
  let q = $state("");
  let sortKey = $state<SortKey>(readSort());
  let sortDesc = $state(readDesc(readSort()));
  function persistSort() {
    try {
      localStorage.setItem("kurisu.list.sort", sortKey);
      localStorage.setItem("kurisu.list.dir", sortDesc ? "desc" : "asc");
    } catch {
      // storage unavailable. Choice won't persist.
    }
  }
  function pickSort(k: SortKey) {
    sortDesc = SORT_DEFAULT_DESC[k];
    persistSort();
  }
  function flipDir() {
    sortDesc = !sortDesc;
    persistSort();
  }

  function matchesQuery(e: ListEntry, needle: string): boolean {
    const m = e.media;
    if (!m) return false;
    return [m.title_english, m.title_romaji, m.title_native].some((t) =>
      t?.toLowerCase().includes(needle)
    );
  }

  const titleCmp = (a: ListEntry, b: ListEntry) =>
    COLLATOR.compare(displayTitle(a.media), displayTitle(b.media));

  /// Sort value for the active key. Null sorts last.
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

  // Overlapping loads resolve latest wins. Stale responses are dropped.
  let loadId = 0;
  // Local cache isn't namespaced per account. If the user changes, force a
  // sync. Also purges rows the new account doesn't have.
  async function load() {
    const id = ++loadId;
    loading = true;
    error = "";
    try {
      const list = await api.localEntries();
      if (id !== loadId) return;
      // Decide before rendering. On account switch the cached rows belong
      // to the previous user. Showing them invites edits against the wrong
      // list, so drop them and let sync fill the view. If the reconcile
      // sync then fails, an empty list under an error banner beats the
      // previous account's rows with live steppers.
      const uid = auth.user?.id ?? null;
      const switched = uid !== null && syncedFor !== null && uid !== syncedFor;
      entries = switched ? [] : list;
      syncedFor = uid;
      if ((list.length === 0 && !autoSynced) || switched) {
        autoSynced = true;
        await sync(id);
      }
    } catch (e) {
      if (id === loadId) error = String(e);
    } finally {
      if (id === loadId) loading = false;
    }
  }

  // fromLoad ties this sync to a load request id. If a newer load started
  // mid-sync, the stale result is dropped.
  async function sync(fromLoad?: number) {
    syncing = true;
    error = "";
    try {
      const list = await api.syncMyList();
      if (fromLoad === undefined || fromLoad === loadId) entries = list;
    } catch (e) {
      if (fromLoad === undefined || fromLoad === loadId) error = String(e);
    } finally {
      syncing = false;
    }
  }

  /// Merge a saved entry back into the local list without a full reload.
  function applyEntry(entry: ListEntry) {
    entries = entries.map((x) =>
      x.media_id === entry.media_id ? { ...entry, media: entry.media ?? x.media } : x
    );
  }

  $effect(() => {
    if (auth.isLoggedIn) load();
  });

  // Refresh when the watcher or prompt modal updates an episode. Those
  // paths don't update the row in place, unlike the stepper. Debounced
  // so a burst of events collapses into one reload.
  $effect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    let debounce: ReturnType<typeof setTimeout> | null = null;
    listen("kurisu://episode-updated", () => {
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => {
        debounce = null;
        load();
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
  <div class="p-5 max-w-7xl mx-auto">
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

    <div class="flex gap-1 mb-5 border-b border-edge">
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

    {#if loading}
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
              // Only when the row itself is focused. The row has real buttons
              // whose click handlers stop propagation, but keydown wasn't
              // stopped. So Enter on the stepper both cancelled its click via
              // preventDefault and opened this modal.
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
              <div class="truncate font-medium">{displayTitle(e.media)}</div>
              <div class="text-xs text-ink-dim truncate flex items-center gap-1.5">
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
