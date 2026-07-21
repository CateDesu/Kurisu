<script lang="ts">
  // <img> that swaps to the standard placeholder block when the URL fails to load
  // (404 / CDN hiccup), instead of leaving the browser's broken-image icon behind.
  // Takes the same sizing classes the placeholder divs use.
  let {
    src,
    alt = "",
    class: klass = "",
  }: { src: string; alt?: string; class?: string } = $props();

  let failed = $state(false);
  // A new URL gets a fresh chance to load.
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
