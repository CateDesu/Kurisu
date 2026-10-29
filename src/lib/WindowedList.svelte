<script lang="ts">
  import { onDestroy, tick, untrack, type Snippet } from "svelte";
  import StagedList from "#lib/StagedList.svelte";
  import { estimatedRowHeight, listLayout, rowAt, type WindowedGeometry } from "#lib/windowedList.ts";
  import type { ListEntry } from "#lib/types.ts";

  let { items, children: renderRow, geometry }: {
    items: ListEntry[];
    children: Snippet<[ListEntry, number, (pending: boolean) => void]>;
    geometry?: WindowedGeometry;
  } = $props();
  const supported = typeof ResizeObserver !== "undefined";
  const virtual = supported;
  let root: HTMLDivElement;
  let heights = $state.raw(new Map<number, number>());
  let pinned = $state.raw(new Set<number>());
  let focused = $state<number | null>(null);
  let restoringFocus = $state<number | null>(null);
  let requested = $state<number | null>(null);
  let viewport = $state.raw({ top: 0, height: 600 });
  let frame = 0;
  let alive = true;
  let generation = 0;
  let navigationId = 0;
  let endRequested = false;
  let scroller: HTMLElement | undefined;
  let sizes: ResizeObserver | undefined;
  let pendingAnchor: WindowedGeometry["anchor"];
  const measured = new Map<number, number>();
  const rowSizes = supported ? new ResizeObserver((entries) => {
    if (!alive) return;
    for (const entry of entries) {
      const node = entry.target as HTMLElement;
      if (!node.isConnected || !root?.contains(node)) continue;
      const id = Number(node.dataset.windowedId);
      const height = entry.borderBoxSize?.[0]?.blockSize || node.getBoundingClientRect().height;
      if (height > 0 && Math.abs((measured.get(id) ?? heights.get(id) ?? estimatedRowHeight) - height) > .5) {
        measured.set(id, height);
        schedule();
      }
    }
  }) : undefined;
  const layout = $derived(listLayout(items, heights));
  const total = $derived(layout.offsets.at(-1) ?? 0);
  const rows = $derived.by(() => {
    if (!virtual || !items.length) return [];
    const first = items.length <= 250 ? 0 : rowAt(layout.offsets, Math.max(0, viewport.top - 800));
    const last = items.length <= 250 ? items.length - 1
      : Math.min(items.length - 1, rowAt(layout.offsets, viewport.top + viewport.height + 800));
    const indices = new Set<number>();
    for (let index = first; index <= last; index++) indices.add(index);
    for (const id of [...pinned, focused, requested, restoringFocus]) {
      if (id == null) continue;
      const index = layout.indices.get(id);
      if (index !== undefined) indices.add(index);
    }
    let previous = 0;
    return [...indices].sort((a, b) => a - b).map((index) => {
      const gap = layout.offsets[index] - previous;
      previous = layout.offsets[index + 1];
      return { entry: items[index], index, gap };
    });
  });
  const tail = $derived(rows.length ? total - layout.offsets[rows.at(-1)!.index + 1] : total);

  function retain(id: number, pending: boolean) {
    if (pinned.has(id) === pending) return;
    const next = new Set(pinned);
    if (pending) next.add(id);
    else next.delete(id);
    pinned = next;
  }

  function readViewport() {
    if (!scroller || !root) return;
    const top = Math.max(0, scroller.getBoundingClientRect().top - root.getBoundingClientRect().top);
    const height = scroller.clientHeight || 600;
    if (top !== viewport.top || height !== viewport.height) viewport = { top, height };
  }

  function anchor() {
    if (scroller && root.getBoundingClientRect().top > scroller.getBoundingClientRect().top) return;
    const index = rowAt(layout.offsets, viewport.top);
    const entry = items[index];
    return entry ? { id: entry.media_id, offset: viewport.top - layout.offsets[index] } : undefined;
  }

  export function capture(): WindowedGeometry {
    readViewport();
    return { heights: [...heights], width: root?.getBoundingClientRect().width ?? 0, anchor: anchor() };
  }

  async function update() {
    frame = 0;
    if (!alive || !scroller || !virtual) return;
    readViewport();
    if (!measured.size && !pendingAnchor) return;
    const position = pendingAnchor ?? anchor();
    const navigation = navigationId;
    pendingAnchor = undefined;
    const next = new Map(heights);
    for (const [id, height] of measured) {
      if (layout.indices.has(id)) next.set(id, height);
    }
    measured.clear();
    heights = next;
    await tick();
    if (!alive || !scroller || navigation !== navigationId) return;
    if (endRequested) scroller.scrollTop = scroller.scrollHeight;
    else if (position) {
      const current = layout.indices.get(position.id);
      if (current !== undefined) {
        const origin = scroller.scrollTop + root.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
        scroller.scrollTop = origin + layout.offsets[current] + position.offset;
      }
    }
    readViewport();
  }

  function schedule() {
    if (!frame && alive) frame = requestAnimationFrame(() => { void update(); });
  }

  function observeRow(node: HTMLElement, id: number) {
    rowSizes?.observe(node);
    return { destroy() { rowSizes?.unobserve(node); measured.delete(id); } };
  }

  $effect.pre(() => {
    const next = items;
    untrack(() => {
      const current = ++generation;
      requested = null;
      endRequested = false;
      const active = root?.contains(document.activeElement) ? document.activeElement as HTMLElement : null;
      const id = active?.closest<HTMLElement>("[data-windowed-id]")?.dataset.windowedId;
      restoringFocus = id ? Number(id) : null;
      const ids = new Set(next.map((entry) => entry.media_id));
      pinned = new Set([...pinned].filter((id) => ids.has(id)));
      heights = new Map([...heights].filter(([id]) => ids.has(id)));
      void tick().then(() => {
        if (!alive || current !== generation) return;
        if (active?.isConnected && document.activeElement === document.body) active.focus({ preventScroll: true });
        restoringFocus = null;
      });
    });
  });

  $effect(() => {
    const saved = geometry;
    if (!saved || !virtual || !root) return;
    untrack(() => {
      endRequested = false;
      if (Math.abs(saved.width - root.getBoundingClientRect().width) < 1) {
        heights = new Map(saved.heights.filter(([id]) => layout.indices.has(id)));
      }
      pendingAnchor = saved.anchor;
      schedule();
    });
  });

  function focusChanged(event: FocusEvent) {
    const target = event.type === "focusout" ? event.relatedTarget : event.target;
    const row = target instanceof HTMLElement ? target.closest<HTMLElement>("[data-windowed-id]") : null;
    focused = row && root.contains(row) ? Number(row.dataset.windowedId) : null;
  }

  function controls(node: HTMLElement) {
    return [...node.querySelectorAll<HTMLElement>("button:not(:disabled), a[href], input:not(:disabled), [tabindex='0']")];
  }

  function cancelNavigation() {
    navigationId++;
    endRequested = false;
    pendingAnchor = undefined;
    requested = null;
  }

  async function keydown(event: KeyboardEvent) {
    if (event.defaultPrevented) return;
    const target = event.target;
    if (!(target instanceof HTMLElement)) return;
    if ((event.key === "Home" || event.key === "End") && scroller &&
        !target.closest("input, textarea, select, [contenteditable='true']")) {
      event.preventDefault();
      endRequested = event.key === "End";
      scroller.scrollTop = event.key === "Home" ? 0 : scroller.scrollHeight;
      schedule();
      return;
    }
    if (event.key !== "Tab") return;
    const row = target.closest<HTMLElement>("[data-windowed-id]");
    if (!row || !root.contains(row)) return;
    const all = controls(row);
    if (target !== (event.shiftKey ? all[0] : all.at(-1))) return;
    const index = layout.indices.get(Number(row.dataset.windowedId));
    if (index === undefined) return;
    const next = items[index + (event.shiftKey ? -1 : 1)];
    if (!next || root.querySelector(`[data-windowed-id="${next.media_id}"]`)) return;
    event.preventDefault();
    const current = generation;
    const navigation = navigationId;
    requested = next.media_id;
    await tick();
    if (!alive || current !== generation || navigation !== navigationId || document.activeElement !== target || requested !== next.media_id) {
      if (current === generation && navigation === navigationId && requested === next.media_id) requested = null;
      return;
    }
    const destination = root.querySelector<HTMLElement>(`[data-windowed-id="${next.media_id}"]`);
    const control = destination && (event.shiftKey ? controls(destination).at(-1) : controls(destination)[0]);
    control?.focus({ preventScroll: true });
    control?.scrollIntoView({ block: "nearest" });
    requested = null;
    schedule();
  }

  $effect(() => {
    if (!virtual) return;
    scroller = root.closest("main") ?? root;
    let width = 0;
    sizes = new ResizeObserver((entries) => {
      if (!alive) return;
      const nextWidth = entries[0]?.contentRect.width ?? 0;
      if (width && nextWidth && nextWidth !== width) {
        readViewport();
        pendingAnchor = anchor();
        heights = new Map();
        measured.clear();
        for (const row of root.querySelectorAll<HTMLElement>("[data-windowed-id]")) {
          rowSizes?.unobserve(row);
          rowSizes?.observe(row);
        }
      }
      if (nextWidth) width = nextWidth;
      schedule();
    });
    sizes.observe(scroller);
    scroller.addEventListener("scroll", schedule, { passive: true });
    scroller.addEventListener("wheel", cancelNavigation, { passive: true });
    scroller.addEventListener("pointerdown", cancelNavigation, { passive: true });
    scroller.addEventListener("touchstart", cancelNavigation, { passive: true });
    scroller.addEventListener("keydown", cancelNavigation, true);
    root.addEventListener("focusin", focusChanged);
    root.addEventListener("focusout", focusChanged);
    root.addEventListener("keydown", keydown);
    untrack(readViewport);
    return () => {
      scroller?.removeEventListener("scroll", schedule);
      scroller?.removeEventListener("wheel", cancelNavigation);
      scroller?.removeEventListener("pointerdown", cancelNavigation);
      scroller?.removeEventListener("touchstart", cancelNavigation);
      scroller?.removeEventListener("keydown", cancelNavigation, true);
      root.removeEventListener("focusin", focusChanged);
      root.removeEventListener("focusout", focusChanged);
      root.removeEventListener("keydown", keydown);
      sizes?.disconnect();
      cancelAnimationFrame(frame);
      frame = 0;
      scroller = undefined;
    };
  });

  onDestroy(() => { alive = false; generation++; cancelAnimationFrame(frame); rowSizes?.disconnect(); });
</script>

<div bind:this={root} data-windowed-list={virtual ? "" : undefined} style:overflow-anchor={virtual ? "none" : undefined}>
  {#if virtual}
    {#each rows as row (row.entry.media_id)}
      <div aria-hidden="true" style:height="{row.gap}px"></div>
      <div data-windowed-id={row.entry.media_id} use:observeRow={row.entry.media_id}>
        {@render renderRow(row.entry, row.index, (pending) => retain(row.entry.media_id, pending))}
      </div>
    {/each}
    <div aria-hidden="true" style:height="{tail}px"></div>
  {:else}
    <StagedList {items}>
      {#snippet children(entry, index)}
        {@render renderRow(entry, index, () => {})}
      {/snippet}
    </StagedList>
  {/if}
</div>
