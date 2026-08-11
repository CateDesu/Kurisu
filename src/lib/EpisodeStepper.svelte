<script lang="ts">
  // Compact minus plus stepper. Edits buffer 3s then auto commit.
  // Pending changes flush on unmount. Grey only, no white.
  import { untrack } from "svelte";
  import { emit } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import type { ListEntry } from "$lib/types";

  let {
    mediaId,
    progress,
    total = null,
    onchange,
    onerror,
  }: {
    mediaId: number;
    progress: number;
    total?: number | null;
    onchange?: (e: ListEntry) => void;
    onerror?: (message: string) => void;
  } = $props();

  let pending = $state(untrack(() => progress));
  let saved = $state(untrack(() => progress));
  let saving = $state(false);
  // Last commit failure. Show the snapback instead of silently reverting.
  let failed = $state("");
  let timer: ReturnType<typeof setTimeout> | null = null;

  const dirty = $derived(pending !== saved);
  const atMin = $derived(pending <= 0);
  // No episode count known. Cap at a sane ceiling.
  const atMax = $derived(pending >= (total ?? 9999));

  function step(delta: number) {
    let next = pending + delta;
    if (next < 0) next = 0;
    const max = total ?? 9999;
    if (next > max) next = max;
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
    failed = "";
    try {
      // CAS on saved. If something else moved progress while this was in
      // flight, the backend skips our stale write and returns the live entry.
      const entry = await api.setProgress(mediaId, v, saved);
      if (entry.progress === v) {
        saved = v;
        onchange?.(entry);
        await emit("kurisu://episode-updated", entry);
      } else {
        pending = entry.progress;
        saved = entry.progress;
        onchange?.(entry);
      }
    } catch (e) {
      // revert to what actually saved
      pending = saved;
      failed = String(e);
      console.error("set progress failed", e);
      // Surface to the page. A console log alone is invisible to the user.
      onerror?.(failed);
    } finally {
      saving = false;
    }
  }

  // Flush a pending edit if the row scrolls off or re-renders.
  $effect(() => {
    return () => {
      if (timer) {
        clearTimeout(timer);
        timer = null;
        void commit();
      }
    };
  });

  // Follow external progress changes. When idle, mirror the prop. When
  // editing, adopt the new baseline but don't clobber the pending value.
  $effect(() => {
    const p = progress;
    untrack(() => {
      if (pending === saved) {
        pending = p;
        saved = p;
      } else if (p !== saved) {
        // Only adopt a higher value when the user was also incrementing.
        // Don't reverse a decrement silently. CAS sorts it out at commit.
        const wasIncrementing = pending > saved;
        saved = p;
        if (wasIncrementing && p > pending) pending = p;
      }
    });
  });

  const btnCls =
    "w-6 h-6 grid place-items-center rounded bg-edge/50 hover:bg-edge text-ink-dim hover:text-ink " +
    "disabled:opacity-30 disabled:hover:bg-edge/50 disabled:hover:text-ink-dim text-sm leading-none transition-colors";
</script>

<!-- stopPropagation so clicking anywhere in the stepper doesn't fire the parent row's click handler -->
<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="flex items-center gap-1 select-none" role="presentation" onclick={(e) => e.stopPropagation()}>
  <span class="w-3 shrink-0 text-[10px] text-center leading-none">
    {#if saving}
      <span class="text-ink-dim">…</span>
    {:else if failed}
      <span class="text-red-400" title="Not saved: {failed}">!</span>
    {:else if dirty}
      <span class="text-accent" title="Saves automatically in 3s">●</span>
    {/if}
  </span>
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
</div>
