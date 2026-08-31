<script lang="ts">
  // Format aware score editor. Adapts to the user's AniList scoreFormat:
  // POINT_100, POINT_10_DECIMAL, POINT_10, POINT_5 stars, POINT_3 smileys.
  // value is null when unrated. AniList stores it as a number.
  let {
    value = $bindable(),
    format,
    id,
  }: { value: number | null; format?: string | null; id?: string } = $props();

  const smiles = ["😞", "😐", "😊"]; // index 0 → score 1
  const stars = [1, 2, 3, 4, 5];

  const inputCls =
    "w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent";

  // min/max/step on a number input are hints, typed values bind raw. An out
  // of range score made AniList reject the whole mutation, so the user's
  // status and progress edits failed with an error pointing nowhere near the
  // score field. Clamp and round to the format on change instead.
  const NUMERIC_BOUNDS: Record<string, [number, number, number]> = {
    POINT_10: [0, 10, 1],
    POINT_10_DECIMAL: [0, 10, 0.1],
    POINT_100: [0, 100, 1],
  };
  function clampTyped() {
    const [min, max, step] = NUMERIC_BOUNDS[format ?? ""] ?? [0, 100, 1];
    if (value == null || Number.isNaN(value)) return;
    let v = Math.min(max, Math.max(min, value));
    v = Math.round(v / step) * step;
    value = Math.round(v * 10) / 10;
  }
</script>

{#if format === "POINT_3"}
  <div {id} class="flex items-center gap-1">
    {#each [1, 2, 3] as n}
      <button
        type="button"
        onclick={() => (value = value === n ? null : n)}
        aria-label={`Rate ${n}`}
        class="text-2xl leading-none px-1 transition-opacity hover:opacity-100
          {value === n ? 'opacity-100' : 'opacity-30 hover:opacity-60'}"
      >
        {smiles[n - 1]}
      </button>
    {/each}
    {#if value != null}
      <button type="button" onclick={() => (value = null)} class="text-xs text-ink-dim ml-2 hover:text-ink">
        clear
      </button>
    {/if}
  </div>
{:else if format === "POINT_5"}
  <div {id} class="flex items-center gap-0.5">
    {#each stars as n}
      <button
        type="button"
        onclick={() => (value = value === n ? null : n)}
        aria-label={`Rate ${n} stars`}
        class="text-xl leading-none px-0.5 transition-colors
          {value != null && n <= value ? 'text-accent' : 'text-ink-dim/40 hover:text-ink-dim'}"
      >
        ★
      </button>
    {/each}
    {#if value != null}
      <button type="button" onclick={() => (value = null)} class="text-xs text-ink-dim ml-2 hover:text-ink">
        clear
      </button>
    {/if}
  </div>
{:else if format === "POINT_10"}
  <input {id} type="number" bind:value min="0" max="10" step="1" onchange={clampTyped} class={inputCls} />
{:else if format === "POINT_10_DECIMAL"}
  <input {id} type="number" bind:value min="0" max="10" step="0.1" onchange={clampTyped} class={inputCls} />
{:else}
  <!-- POINT_100 or unknown -->
  <input {id} type="number" bind:value min="0" max="100" step="1" onchange={clampTyped} class={inputCls} />
{/if}
