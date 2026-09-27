<script lang="ts">
  import type { Snippet } from "svelte";
  import Gearwork from "$lib/Gearwork.svelte";

  let { section, title, description = "", compact = false, children }: {
    section: string;
    title: string;
    description?: string;
    compact?: boolean;
    children?: Snippet;
  } = $props();
</script>

<header class="page-heading" class:compact>
  <div class="heading-main">
    <div class="heading-copy">
      <div class="eyebrow">{section}</div>
      <h1>{title}<span class="heading-dot" aria-hidden="true">.</span></h1>
      {#if description}<p>{description}</p>{/if}
    </div>
  </div>
  {#if children}<div class="heading-actions">{@render children()}</div>{/if}
  <div class="heading-art" aria-hidden="true"><Gearwork /></div>
  <span class="heading-registration" aria-hidden="true">＋</span>
</header>

<style>
  .page-heading { position: relative; display: flex; flex-wrap: wrap; align-items: center; gap: 8px 20px; border-bottom: 1px solid var(--color-edge); margin-bottom: 18px; padding: 0 155px 12px 0; }
  .heading-main { position: relative; display: flex; align-items: center; flex: 1 1 205px; min-width: 0; min-height: 82px; overflow: hidden; }
  .heading-copy { position: relative; z-index: 1; width: 100%; padding: 2px 0; }
  .eyebrow { font-size: 13px; letter-spacing: .14em; }
  h1 { font: 30px/1.15 var(--font-serif); letter-spacing: -.5px; margin: 5px 0; overflow-wrap: anywhere; }
  .heading-dot { color: var(--color-accent); }
  p { font-size: 13px; line-height: 1.4; color: var(--color-ink-dim); }
  .heading-art { position: absolute; width: 145px; right: 0; top: 0; opacity: .85; pointer-events: none; }
  .heading-art :global(.gearwork) { color: var(--color-accent); }
  .heading-registration { position: absolute; right: 0; bottom: 0; font: 14px var(--font-mono); color: var(--color-rust); }
  .heading-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding-right: 16px; }
  .page-heading.compact { padding-right: 0; }
  .compact .heading-main { flex-basis: 320px; min-height: 62px; }
  .compact h1 { font-size: 28px; }
  .compact .heading-art { width: 120px; opacity: .5; }
  @media (max-width: 960px) {
    .page-heading { padding-right: 135px; }
    .heading-art { width: 125px; top: 4px; opacity: .75; }
  }
</style>
