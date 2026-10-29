<script lang="ts">
  import type { Snippet } from "svelte";
  import { measuredRows } from "#lib/measuredRows.ts";

  let { children }: { children: Snippet } = $props();
</script>

<div use:measuredRows>
  {@render children()}
</div>
