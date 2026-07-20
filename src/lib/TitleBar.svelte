<script lang="ts">
  // Custom dark title bar. The native title bar is disabled (decorations: false),
  // so we draw this and make it draggable via `data-tauri-drag-region`. Window
  // controls call into the Tauri window API.
  import { getCurrentWindow } from "@tauri-apps/api/window";

  const appWindow = getCurrentWindow();
  let maximized = $state(false);

  async function refresh() {
    try {
      maximized = await appWindow.isMaximized();
    } catch {
      // window API not ready yet (during loading) — leave default
    }
  }
  refresh();
  // keep the maximize/restore icon in sync when the user resizes via the WM edges
  $effect(() => {
    let un: (() => void) | undefined;
    appWindow.onResized(() => refresh()).then((u) => (un = u));
    return () => un?.();
  });
</script>

<div
  data-tauri-drag-region
  class="relative h-9 shrink-0 flex items-center justify-end bg-base border-b border-edge select-none"
>
  <div
    class="absolute left-1/2 -translate-x-1/2 flex items-center gap-2 text-sm font-medium pointer-events-none"
  >
    <span class="text-accent text-base leading-none">ク</span>
    <span class="tracking-wide">Kurisu</span>
  </div>
  <div class="flex items-center h-full">
    <button class="tb-btn" title="Minimize" onclick={() => appWindow.minimize()}>
      <svg viewBox="0 0 10 10" width="10" height="10"><rect y="4.5" width="10" height="1" fill="currentColor" /></svg>
    </button>
    <button
      class="tb-btn"
      title={maximized ? "Restore" : "Maximize"}
      onclick={() => appWindow.toggleMaximize()}
    >
      {#if maximized}
        <svg viewBox="0 0 10 10" width="10" height="10" fill="none" stroke="currentColor" stroke-width="1">
          <rect x="1.5" y="2.5" width="6" height="6" />
          <rect x="0.5" y="0.5" width="6" height="6" fill="var(--color-base)" />
          <rect x="0.5" y="0.5" width="6" height="6" />
        </svg>
      {:else}
        <svg viewBox="0 0 10 10" width="10" height="10" fill="none" stroke="currentColor" stroke-width="1">
          <rect x="0.5" y="0.5" width="9" height="9" />
        </svg>
      {/if}
    </button>
    <button class="tb-btn tb-close" title="Close" onclick={() => appWindow.close()}>
      <svg viewBox="0 0 10 10" width="10" height="10" stroke="currentColor" stroke-width="1.2">
        <path d="M1 1 L9 9 M9 1 L1 9" />
      </svg>
    </button>
  </div>
</div>

<style>
  .tb-btn {
    width: 42px;
    height: 100%;
    display: grid;
    place-items: center;
    color: var(--color-ink-dim);
    background: transparent;
    border: none;
    cursor: pointer;
    transition: background 0.12s, color 0.12s;
  }
  .tb-btn:hover {
    background: var(--color-edge);
    color: var(--color-ink);
  }
  .tb-close:hover {
    background: #e23c3c;
    color: #fff;
  }
</style>
