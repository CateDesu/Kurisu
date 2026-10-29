<script lang="ts">
  import { nowMs, runClock } from "#lib/now.svelte.ts";

  let { intervalMs = 30_000, observe }: { intervalMs?: number; observe: (now: number) => void } = $props();
  $effect(() => runClock(intervalMs));
  $effect(() => observe(nowMs()));
</script>

<p>{nowMs()}</p>
