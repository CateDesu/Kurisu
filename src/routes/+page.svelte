<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
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

  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let syncing = $state(false);
  let error = $state("");
  let filter = $state<string>("CURRENT");
  let editing = $state<ListEntry | null>(null);

  const statuses = ["CURRENT", "PLANNING", "COMPLETED", "PAUSED", "DROPPED", "REPEATING"];

  const visible = $derived(
    entries.filter((e) => e.status === filter).sort((a, b) =>
      displayTitle(a.media).localeCompare(displayTitle(b.media), undefined, {
        sensitivity: "base",
        numeric: true,
      })
    )
  );

  // Overlapping loads (initial + episode-updated + edit-close) resolve
  // latest-wins; stale responses are dropped.
  let loadId = 0;
  // The local cache isn't namespaced per account: if the user changes under us
  // (logout → different login), force a sync — it also purges rows the new
  // account doesn't have. Auto-sync on an empty list happens once per session.
  let syncedFor = $state<number | null>(null);
  let autoSynced = false;
  async function load() {
    const id = ++loadId;
    loading = true;
    error = "";
    try {
      const list = await api.localEntries();
      if (id !== loadId) return;
      entries = list;
      const uid = auth.user?.id ?? null;
      const switched = syncedFor !== null && uid !== syncedFor;
      if (syncedFor !== uid) syncedFor = uid;
      if ((entries.length === 0 && !autoSynced) || switched) {
        autoSynced = true;
        await sync(id);
      }
    } catch (e) {
      if (id === loadId) error = String(e);
    } finally {
      if (id === loadId) loading = false;
    }
  }

  // `fromLoad` ties this sync to a load()'s request id: if a newer load started
  // while the sync was in flight, the stale result is dropped like any other.
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

  /// Merge a freshly-saved entry back into the local list (from the stepper or the
  /// edit modal) without a full reload.
  function applyEntry(entry: ListEntry) {
    entries = entries.map((x) =>
      x.media_id === entry.media_id ? { ...entry, media: entry.media ?? x.media } : x
    );
  }

  $effect(() => {
    if (auth.isLoggedIn) load();
  });

  // Refresh when the watcher (auto mode) or the prompt modal updates an episode —
  // those paths have no in-place row update here, unlike the stepper which calls
  // applyEntry itself. Debounced so a burst of events collapses into one reload.
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
    <div class="flex items-center gap-3 mb-4">
      <h1 class="text-xl font-semibold flex-1">My List</h1>
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
      <div class="text-ink-dim py-10 text-center">Nothing here yet.</div>
    {:else}
      <div class="grid grid-cols-1 gap-2">
        {#each visible as e (e.media_id)}
          {@const air = airingLabel(e.media)}
          {@const sc = scoreLabel(e.score, auth.user?.score_format)}
          <div
            onclick={() => (editing = e)}
            onkeydown={(ev) => {
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
              <Img src={e.media.cover_medium} class="w-10 h-14 object-cover rounded shrink-0" />
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
                onchange={applyEntry}
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
