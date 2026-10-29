<script lang="ts">
  import StagedList from "#lib/StagedList.svelte";
  import EpisodeStepper from "#lib/EpisodeStepper.svelte";
  import type { ListEntry } from "#lib/types.ts";

  let { items, stepper = false, onOpen }: {
    items: ListEntry[]; stepper?: boolean; onOpen?: (entry: ListEntry) => void;
  } = $props();
  export function replace(next: ListEntry[]) { items = next; }
</script>

<main>
  <input aria-label="Query" />
  <button type="button" data-before>Before</button>
  <StagedList {items}>
    {#snippet children(entry, index)}
      <div data-id={entry.media_id} data-staged-index={index} tabindex="0" role="button" onkeydown={() => {}} onclick={() => {}}>
        <span>{index + 1}. {entry.media?.title_english}</span>
        <button type="button" onclick={() => onOpen?.(entry)}>Open {entry.media_id}</button>
        {#if stepper}
          <EpisodeStepper mediaId={entry.media_id} progress={entry.progress} total={12} />
        {/if}
      </div>
    {/snippet}
  </StagedList>
  <button type="button" data-after>After</button>
</main>
