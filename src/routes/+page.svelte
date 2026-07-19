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

  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let syncing = $state(false);
  let error = $state("");
  let filter = $state<string>("CURRENT");
  let editing = $state<ListEntry | null>(null);

  const statuses = ["CURRENT", "PLANNING", "COMPLETED", "PAUSED", "DROPPED", "REPEATING"];

  const visible = $derived(
    entries.filter((e) => e.status === filter).sort((a, b) =>
      displayTitle(a.media).localeCompare(displayTitle(b.media))
    )
  );

  async function load() {
    loading = true;
    error = "";
    try {
      entries = await api.localEntries();
      if (entries.length === 0) await sync();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function sync() {
    syncing = true;
    error = "";
    try {
      entries = await api.syncMyList();
    } catch (e) {
      error = String(e);
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

  // Refresh when the watcher (auto mode) or the prompt modal updates an episode,
  // so the list reflects new progress even if we're sitting on this page idle.
  $effect(() => {
    let un: (() => void) | undefined;
    listen("kurisu://episode-updated", () => {
      load();
    }).then((u) => (un = u));
    return () => un?.();
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-5xl mx-auto">
    <div class="flex items-center gap-3 mb-4">
      <h1 class="text-xl font-semibold flex-1">My List</h1>
      <button
        onclick={sync}
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
              <img src={e.media.cover_medium} alt="" loading="lazy" decoding="async" class="w-10 h-14 object-cover rounded shrink-0" />
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
