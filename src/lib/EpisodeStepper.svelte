<script lang="ts">
  // Compact −/+ episode stepper that replaces the old +1 button. Edits buffer for
  // 3s then auto-commit ("locks in"); pending changes are flushed on unmount so a
  // quick tweak isn't lost. Grey surfaces only — no white.
  import { untrack } from "svelte";
  import { emit } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import type { ListEntry } from "$lib/types";

  let {
    mediaId,
    progress,
    total = null,
    onchange,
  }: {
    mediaId: number;
    progress: number;
    total?: number | null;
    onchange?: (e: ListEntry) => void;
  } = $props();

  let pending = $state(untrack(() => progress));
  let saved = $state(untrack(() => progress));
  let saving = $state(false);
  let timer: ReturnType<typeof setTimeout> | null = null;

  const dirty = $derived(pending !== saved);
  const atMin = $derived(pending <= 0);
  const atMax = $derived(total != null && pending >= total);

  function step(delta: number) {
    let next = pending + delta;
    if (next < 0) next = 0;
    if (total != null && next > total) next = total;
    if (next === pending) return;
    pending = next;
    if (timer) clearTimeout(timer);
    timer = setTimeout(commit, 3000);
  }

  async function commit() {
    timer = null;
    if (pending === saved || saving) return;
    saving = true;
    const v = pending;
    try {
      const entry = await api.setProgress(mediaId, v);
      saved = v;
      onchange?.(entry);
      await emit("kurisu://episode-updated", entry);
    } catch (e) {
      // revert so the UI reflects what actually saved
      pending = saved;
      console.error("set progress failed", e);
    } finally {
      saving = false;
    }
  }

  // Flush a pending edit if the row scrolls away / the app re-renders.
  $effect(() => {
    return () => {
      if (timer) {
        clearTimeout(timer);
        timer = null;
        void commit();
      }
    };
  });

  // Follow external progress changes (sync / episode-updated reload). When we're
  // not mid-edit, mirror the prop; if a reload lands while we ARE editing, just
  // adopt the new saved baseline without clobbering the user's pending value.
  $effect(() => {
    const p = progress;
    untrack(() => {
      if (pending === saved) {
        pending = p;
        saved = p;
      } else if (p !== saved) {
        saved = p;
      }
    });
  });

  const btnCls =
    "w-6 h-6 grid place-items-center rounded bg-edge/50 hover:bg-edge text-ink-dim hover:text-ink " +
    "disabled:opacity-30 disabled:hover:bg-edge/50 disabled:hover:text-ink-dim text-sm leading-none transition-colors";
</script>

<div class="flex items-center gap-1 select-none">
  <button
    type="button"
    onclick={(e) => { e.stopPropagation(); step(-1); }}
    disabled={atMin || saving}
    aria-label="One less episode"
    class={btnCls}>−</button
  >
  <div
    class="min-w-[2.75rem] text-center text-sm tabular-nums {dirty
      ? 'text-accent'
      : 'text-ink'}"
  >
    {pending}{#if total}<span class="text-ink-dim">/{total}</span>{/if}
  </div>
  <button
    type="button"
    onclick={(e) => { e.stopPropagation(); step(1); }}
    disabled={atMax || saving}
    aria-label="One more episode"
    class={btnCls}>+</button
  >
  {#if dirty}
    <span class="text-[10px] text-accent ml-0.5" title="Saves automatically in 3s">●</span>
  {:else if saving}
    <span class="text-[10px] text-ink-dim ml-0.5">…</span>
  {/if}
</div>
