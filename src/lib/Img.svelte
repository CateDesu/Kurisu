<script lang="ts">
  let {
    src,
    alt = "",
    class: klass = "",
  }: { src: string; alt?: string; class?: string } = $props();

  let failed = $state(false);
  $effect(() => {
    src;
    failed = false;
  });
</script>

{#if failed}
  <div class="{klass} bg-panel-2"></div>
{:else}
  <img {src} {alt} loading="lazy" decoding="async" class={klass} onerror={() => (failed = true)} />
{/if}
