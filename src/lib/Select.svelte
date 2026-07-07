<script lang="ts" generics="T extends string">
  // Custom dropdown. WebKit2GTK renders native <select> popups with the platform
  // theme (white, ignores color-scheme), so we draw our own to keep everything
  // dark and on-theme. Keyboard: Enter/Space/↓ open, Esc closes, ↑/↓ move,
  // Enter picks.
  let {
    value = $bindable(),
    options,
    id,
    class: klass = "",
    onchange,
  }: {
    value: T;
    options: Array<{ value: T; label: string }>;
    id?: string;
    class?: string;
    onchange?: (v: T) => void;
  } = $props();

  let open = $state(false);
  let highlight = $state(-1);
  let root: HTMLDivElement;

  const selected = $derived(options.find((o) => o.value === value));

  function openMenu() {
    highlight = options.findIndex((o) => o.value === value);
    open = true;
  }
  function pick(v: T) {
    value = v;
    open = false;
    onchange?.(v);
  }
  function onKeydown(e: KeyboardEvent) {
    if (!open && (e.key === "Enter" || e.key === " " || e.key === "ArrowDown")) {
      e.preventDefault();
      openMenu();
      return;
    }
    if (!open) return;
    if (e.key === "Escape") {
      e.preventDefault();
      open = false;
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      highlight = (highlight + 1) % options.length;
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      highlight = (highlight - 1 + options.length) % options.length;
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (highlight >= 0) pick(options[highlight].value);
    }
  }
  function onWindowPointerDown(e: PointerEvent) {
    if (root && !root.contains(e.target as Node)) open = false;
  }
  $effect(() => {
    if (open) {
      window.addEventListener("pointerdown", onWindowPointerDown);
      return () => window.removeEventListener("pointerdown", onWindowPointerDown);
    }
  });
</script>

<div class="relative {klass}" bind:this={root}>
  <button
    {id}
    type="button"
    aria-haspopup="listbox"
    aria-expanded={open}
    onclick={() => (open ? (open = false) : openMenu())}
    onkeydown={onKeydown}
    class="w-full flex items-center justify-between bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm text-left focus:outline-none focus:border-accent"
  >
    <span class={selected ? "" : "text-ink-dim"}>{selected?.label ?? "—"}</span>
    <span class="text-ink-dim text-xs ml-2">▾</span>
  </button>

  {#if open}
    <ul
      class="absolute z-50 mt-1 w-full max-h-60 overflow-auto bg-base border border-edge rounded-md shadow-xl py-1"
      role="listbox"
    >
      {#each options as opt, i (opt.value)}
        <li>
          <button
            type="button"
            role="option"
            aria-selected={opt.value === value}
            onclick={() => pick(opt.value)}
            onmousemove={() => (highlight = i)}
            class="w-full text-left px-3 py-1.5 text-sm transition-colors
              {opt.value === value
                ? 'text-accent bg-accent/10'
                : highlight === i
                  ? 'bg-edge text-ink'
                  : 'text-ink hover:bg-edge'}"
          >
            {opt.label}
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</div>
