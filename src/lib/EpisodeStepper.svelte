<script lang="ts">
  import { installInFlight } from "$lib/update.svelte";
  import { isPendingEdit, registerPendingEdit, trackPendingSave } from "$lib/pendingEdits";
  import { untrack } from "svelte";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
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
  let failed = $state("");
  let inFlight: Promise<void> | null = null;
  let unregister: (() => void) | undefined;
  let timer: ReturnType<typeof setTimeout> | null = null;
  const targetId = untrack(() => mediaId);
  const epoch = untrack(() => auth.epoch);
  const editKey = `${epoch}:${targetId}`;
  const current = () => epoch === auth.epoch && auth.isLoggedIn;

  const dirty = $derived(pending !== saved);
  const atMin = $derived(pending <= 0);
  const atMax = $derived(pending >= (total ?? 9999));

  function step(delta: number) {
    if (installInFlight()) return;
    let next = pending + delta;
    if (next < 0) next = 0;
    const max = total ?? 9999;
    if (next > max) next = max;
    if (next === pending) return;
    pending = next;
    unregister = registerPendingEdit(flush, editKey);
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => { void commit().catch(() => {}); }, 3000);
  }

  function onKeydown(e: KeyboardEvent) {
    let delta = 0;
    if (e.key === "ArrowLeft" || e.key === "ArrowDown") delta = -1;
    else if (e.key === "ArrowRight" || e.key === "ArrowUp") delta = 1;
    else return;
    e.preventDefault();
    e.stopPropagation();
    if (saving || (delta < 0 && atMin) || (delta > 0 && atMax)) return;
    step(delta);
  }

  function commit(): Promise<void> {
    if (inFlight) return inFlight;
    if (!isPendingEdit(editKey, flush)) return Promise.resolve();
    inFlight = trackPendingSave(save().finally(() => {
      inFlight = null;
      if (!current() || pending === saved) {
        unregister?.();
        unregister = undefined;
      }
    }));
    return inFlight;
  }

  async function flush() {
    if (timer) clearTimeout(timer);
    timer = null;
    await commit();
  }

  async function save() {
    timer = null;
    if (pending === saved || saving || !current()) return;
    saving = true;
    const v = pending;
    failed = "";
    try {
      const entry = await api.setProgress(targetId, v, saved);
      if (!current()) return;
      if (entry.progress !== v) pending = entry.progress;
      saved = entry.progress;
      onchange?.(entry);
    } catch (e) {
      if (!current()) return;
      failed = String(e);
      console.error("set progress failed", e);
      onerror?.(failed);
      throw e;
    } finally {
      saving = false;
    }
  }

  $effect(() => {
    return () => {
      if (timer) {
        clearTimeout(timer);
        timer = null;
        queueMicrotask(() => { void commit().catch(() => {}); });
      }
    };
  });

  $effect(() => {
    const p = progress;
    untrack(() => {
      if (pending === saved) {
        pending = p;
        saved = p;
      } else if (p !== saved) {
        // Keep the original baseline while dirty so stale edits cannot pass the backend check.
        const wasIncrementing = pending > saved;
        if (wasIncrementing && p > pending) pending = p;
      }
    });
  });

  const btnCls =
    "w-6 h-6 grid place-items-center rounded bg-edge/50 hover:bg-edge text-ink-dim hover:text-ink " +
    "disabled:opacity-30 disabled:hover:bg-edge/50 disabled:hover:text-ink-dim text-sm leading-none transition-colors " +
    "focus:outline-none focus:ring-1 focus:ring-accent";
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="flex items-center gap-1 select-none"
  role="presentation"
  onclick={(e) => e.stopPropagation()}
>
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
    onkeydown={onKeydown}
    disabled={atMin || saving || installInFlight()}
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
    onkeydown={onKeydown}
    disabled={atMax || saving || installInFlight()}
    aria-label="One more episode"
    class={btnCls}>+</button
  >
</div>
