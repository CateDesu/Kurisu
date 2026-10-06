<script lang="ts" generics="T extends string">
  let {
    value = $bindable(),
    options,
    id,
    label,
    class: klass = "",
    onchange,
  }: {
    value: T;
    options: Array<{ value: T; label: string }>;
    id?: string;
    label?: string;
    class?: string;
    onchange?: (v: T) => void;
  } = $props();
</script>

<div class="relative {klass}">
  <select
    {id}
    aria-label={label}
    bind:value={() => value, (v) => { value = v; onchange?.(v); }}
    class="w-full appearance-none bg-panel-2 border border-edge rounded-md pl-3 pr-8 py-2 text-sm text-left focus:outline-none focus:border-accent"
  >
    {#each options as opt (opt.value)}
      <option value={opt.value}>{opt.label}</option>
    {/each}
  </select>
  <span aria-hidden="true" class="pointer-events-none absolute right-3 top-1/2 -translate-y-1/2 text-ink-dim text-xs">▾</span>
</div>
