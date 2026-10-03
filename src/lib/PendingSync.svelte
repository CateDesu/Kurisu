<script lang="ts">
  import { onDestroy } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { STATUS_LABEL, type PendingChange } from "$lib/types";

  let items = $state<PendingChange[]>([]);
  let expanded = $state(false);
  let busy = $state(false);
  let error = $state("");
  let loadId = 0;
  let mounted = true;
  onDestroy(() => { mounted = false; loadId++; });

  $effect(() => {
    const epoch = auth.epoch;
    items = [];
    error = "";
    busy = false;
    expanded = false;
    if (!auth.isLoggedIn) return;
    let alive = true;
    let unlisten: (() => void) | undefined;
    const current = () => alive && epoch === auth.epoch && auth.isLoggedIn;
    async function load() {
      const id = ++loadId;
      try {
        const result = await api.getPendingChanges();
        if (current() && id === loadId) { items = result; error = ""; }
      } catch (e) {
        if (current() && id === loadId) error = String(e);
      }
    }
    const timer = setInterval(() => void load(), 30_000);
    listen("kurisu://pending-changed", () => void load()).then((stop) => {
      if (current()) unlisten = stop;
      else stop();
    }).catch((e) => { if (current()) error = String(e); }).then(() => {
      if (current()) void load();
    });
    const online = () => void retry();
    window.addEventListener("online", online);
    return () => {
      alive = false;
      loadId++;
      clearInterval(timer);
      unlisten?.();
      window.removeEventListener("online", online);
    };
  });

  async function run(action: () => Promise<unknown>) {
    if (busy || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    const current = () => mounted && epoch === auth.epoch && auth.isLoggedIn;
    busy = true;
    error = "";
    try {
      await action();
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (current()) {
        const id = ++loadId;
        try {
          const result = await api.getPendingChanges();
          if (current() && id === loadId) items = result;
        } catch (e) {
          if (current() && id === loadId) error = String(e);
        }
        if (current()) busy = false;
      }
    }
  }

  function retry() { return run(() => api.syncPendingChanges()); }
</script>

{#if auth.isLoggedIn && (items.length || error)}
  <div class="border-b border-edge bg-panel px-4 py-2 text-sm shrink-0 max-h-64 overflow-auto" aria-live="polite">
    <div class="flex flex-wrap items-center gap-3">
      <button class="text-accent" onclick={() => expanded = !expanded} aria-expanded={expanded}>
        {#if items.length}{items.length} saved {items.length === 1 ? "change" : "changes"} waiting to sync{:else}Sync status unavailable{/if}
      </button>
      <button class="px-2 py-1 rounded bg-panel-2 hover:bg-edge text-xs disabled:opacity-50" disabled={busy} onclick={retry}>{busy ? "Syncing…" : "Retry sync"}</button>
    </div>
    {#if error}<p class="text-red-400 mt-1">{error}</p>{/if}
    {#if expanded}
      <p class="text-ink-dim my-2">Saved on this device for your account. Kurisu retries automatically when AniList is available.</p>
      {#each items as item (item.media_id)}
        <div class="border-t border-edge py-2">
          <p>{item.title}{item.progress != null ? ` · Episode ${item.progress}` : ""}</p>
          <p class="text-xs text-ink-dim">
            {item.status ? (STATUS_LABEL[item.status] ?? item.status) : ""}
            {item.score != null ? ` · Score ${item.score}` : ""}
            {item.repeat != null ? ` · Rewatches ${item.repeat}` : ""}
          </p>
          {#if item.error}<p class="text-ink-dim text-xs mt-1">{item.error}</p>{/if}
          {#if item.conflict}
            <div class="flex gap-2 mt-2">
              <button class="px-2 py-1 rounded bg-panel-2 hover:bg-edge text-xs disabled:opacity-50" disabled={busy} onclick={() => run(() => api.resolvePendingChange(item.media_id, false))}>{item.missing_media ? "Discard saved change" : "Keep AniList version"}</button>
              {#if !item.missing_media}<button class="px-2 py-1 rounded bg-panel-2 hover:bg-edge text-xs disabled:opacity-50" disabled={busy} onclick={() => run(() => api.resolvePendingChange(item.media_id, true))}>Send saved change</button>{/if}
            </div>
          {/if}
        </div>
      {/each}
    {/if}
  </div>
{/if}
