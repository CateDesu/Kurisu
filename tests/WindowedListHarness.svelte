<script lang="ts">
  import WindowedList from "#lib/WindowedList.svelte";
  import EpisodeStepper from "#lib/EpisodeStepper.svelte";
  import type { ListEntry } from "#lib/types.ts";
  import type { WindowedGeometry } from "#lib/windowedList.ts";

  let { items, stepper = false }: { items: ListEntry[]; stepper?: boolean } = $props();
  export function replace(next: ListEntry[]) { items = next; }
  let shown = $state(true);
  export function show(visible: boolean) { shown = visible; }
  let list = $state.raw<{ capture: () => WindowedGeometry } | undefined>(undefined);
  let geometry = $state.raw<WindowedGeometry | undefined>(undefined);
  export function capture() { return list!.capture(); }
  export function restore(saved: WindowedGeometry) { geometry = saved; }
</script>

<main>
  <button type="button" data-before>Before</button>
  {#if shown}<WindowedList {items} {geometry} bind:this={list}>
    {#snippet children(entry, index, retain)}
      <div tabindex="0" role="button" onkeydown={() => {}} onclick={() => {}} data-id={entry.media_id} data-staged-index={index}>
        <span>{entry.media?.title_english} · {entry.progress}</span>
        <button type="button">Open {entry.media_id}</button>
        {#if stepper}
          <EpisodeStepper mediaId={entry.media_id} progress={entry.progress} total={12} onpendingchange={retain} />
        {/if}
      </div>
    {/snippet}
  </WindowedList>{/if}
  <button type="button" data-after>After</button>
</main>
