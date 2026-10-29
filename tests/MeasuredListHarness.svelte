<script lang="ts">
  import MeasuredList from "#lib/MeasuredList.svelte";
  import Stepper from "#lib/EpisodeStepper.svelte";

  let { items, stepper = false }: {
    items: Array<{ media_id: number; title: string }>;
    stepper?: boolean;
  } = $props();

  export function replace(next: typeof items) {
    items = next;
  }
</script>

<main style="overflow: auto; height: 300px">
  <button type="button" data-before>Before</button>
  <MeasuredList>
    {#each items as item, index (item.media_id)}
      <div data-id={item.media_id} data-index={index}>
        <button type="button">{item.title}</button>
        {#if stepper}
          <Stepper mediaId={item.media_id} progress={3} total={12} />
        {/if}
      </div>
    {/each}
  </MeasuredList>
  <button type="button" data-after>After</button>
</main>
