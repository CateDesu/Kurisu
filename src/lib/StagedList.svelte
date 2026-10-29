<script lang="ts">
  import { onDestroy, tick, untrack, type Snippet } from "svelte";
  import MeasuredList from "#lib/MeasuredList.svelte";
  import { appendStagedRows } from "#lib/stagedRows.ts";
  import type { ListEntry } from "#lib/types.ts";

  let { items, children }: { items: ListEntry[]; children: Snippet<[ListEntry, number]> } = $props();
  type Row = { entry: ListEntry; index: number };
  const batchSize = 32;
  let rendered = $state.raw({ rows: [] as Row[] });
  let pending = $state(false);
  let root: HTMLDivElement;
  let mounted = new Set<number>();
  let queue: Row[] = [];
  let cursor = 0;
  let frame = 0;
  let fallback: ReturnType<typeof setTimeout> | undefined;
  let generation = 0;
  let destroyed = false;
  let endRequested = false;
  let focusRequest: { index: number; source: HTMLElement; backward: boolean } | undefined;

  function cancelBatch() {
    if (frame) cancelAnimationFrame(frame);
    frame = 0;
    clearTimeout(fallback);
    fallback = undefined;
  }

  function schedule() {
    if (destroyed || frame || fallback !== undefined || !pending) return;
    if (typeof requestAnimationFrame === "function") frame = requestAnimationFrame(advance);
    fallback = setTimeout(advance, 100);
  }

  function focusedControl() {
    const active = document.activeElement;
    return active instanceof HTMLElement && root?.contains(active) ? active : undefined;
  }

  function restoreFocus(active: HTMLElement | undefined) {
    if (active?.isConnected && document.activeElement === document.body) active.focus({ preventScroll: true });
  }

  function addBatch() {
    const added: Row[] = [];
    const requested = focusRequest && queue.find((row) => row.index === focusRequest!.index);
    if (requested && !mounted.has(requested.entry.media_id)) added.push(requested);
    while (cursor < queue.length && added.length < batchSize) {
      const row = queue[cursor++];
      if (!mounted.has(row.entry.media_id) && row !== requested) added.push(row);
    }
    for (const row of added) mounted.add(row.entry.media_id);
    added.sort((a, b) => a.index - b.index);
    if (added.length) rendered = { rows: appendStagedRows(rendered.rows, added) };
    pending = mounted.size < items.length;
  }

  async function advance() {
    cancelBatch();
    if (destroyed || !pending) return;
    const current = generation;
    const active = focusedControl();
    addBatch();
    await tick();
    if (destroyed || current !== generation) return;
    restoreFocus(active);
    const request = focusRequest;
    if (request && mounted.has(items[request.index]?.media_id)) {
      focusRequest = undefined;
      if (document.activeElement === request.source) {
        const row = root.querySelector<HTMLElement>(`[data-staged-index="${request.index}"]`);
        if (row) (request.backward ? controlsIn(row).at(-1) : row)?.focus();
      }
    }
    if (!pending && endRequested) {
      endRequested = false;
      const scroller = root.closest("main");
      if (scroller) scroller.scrollTop = scroller.scrollHeight;
    }
    schedule();
  }

  $effect.pre(() => {
    const next = items;
    untrack(() => {
      cancelBatch();
      generation++;
      const current = generation;
      const active = focusedControl();
      endRequested = false;
      focusRequest = undefined;
      const retained: Row[] = [];
      queue = [];
      const ids = new Set<number>();
      next.forEach((entry, index) => {
        const row = { entry, index };
        if (mounted.has(entry.media_id)) {
          retained.push(row);
          ids.add(entry.media_id);
        } else queue.push(row);
      });
      mounted = ids;
      cursor = 0;
      rendered = { rows: retained };
      pending = queue.length > 0;
      if (!retained.length || next.length <= batchSize) addBatch();
      void tick().then(() => {
        if (!destroyed && current === generation) restoreFocus(active);
      });
      schedule();
    });
  });

  function controlsIn(row: HTMLElement) {
    return [row, ...row.querySelectorAll<HTMLElement>("button:not(:disabled), a[href], input:not(:disabled), [tabindex='0']")];
  }

  $effect(() => {
    const scroller = root.closest("main") ?? root;
    const cancelNavigation = () => { endRequested = false; focusRequest = undefined; };
    function keydown(event: KeyboardEvent) {
      if (event.defaultPrevented || !pending) return;
      const target = event.target;
      if (!(target instanceof HTMLElement) || target.closest("input, textarea, select, [contenteditable='true']")) return;
      if (event.key === "End") {
        event.preventDefault();
        focusRequest = undefined;
        endRequested = true;
      } else if (event.key === "Tab") {
        const row = target.closest<HTMLElement>("[data-staged-index]");
        if (!row || !root.contains(row)) return;
        const controls = controlsIn(row);
        const index = Number(row.dataset.stagedIndex) + (event.shiftKey ? -1 : 1);
        if (target !== (event.shiftKey ? row : controls.at(-1)) || index < 0 || index >= items.length || mounted.has(items[index].media_id)) return;
        event.preventDefault();
        endRequested = false;
        focusRequest = { index, source: target, backward: event.shiftKey };
      } else if (event.key !== "Tab") cancelNavigation();
    }
    scroller.addEventListener("keydown", keydown);
    scroller.addEventListener("wheel", cancelNavigation, { passive: true });
    scroller.addEventListener("pointerdown", cancelNavigation, { passive: true });
    scroller.addEventListener("touchstart", cancelNavigation, { passive: true });
    return () => {
      scroller.removeEventListener("keydown", keydown);
      scroller.removeEventListener("wheel", cancelNavigation);
      scroller.removeEventListener("pointerdown", cancelNavigation);
      scroller.removeEventListener("touchstart", cancelNavigation);
    };
  });

  onDestroy(() => { destroyed = true; generation++; cancelBatch(); });
</script>

<div bind:this={root} data-staged-rows data-pending-rows={pending ? "" : undefined} aria-busy={pending}>
  <MeasuredList>
    {#each rendered.rows as row (row.entry.media_id)}
      {@render children(row.entry, row.index)}
    {/each}
  </MeasuredList>
  {#if pending}
    <p role="status" class="text-ink-dim py-4 text-center">Loading titles… {rendered.rows.length} / {items.length}</p>
  {/if}
</div>
