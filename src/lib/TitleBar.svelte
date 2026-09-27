<script lang="ts">
  import { getCurrentWindow } from "@tauri-apps/api/window";

  const appWindow = getCurrentWindow();
  let maximized = $state(false);

  async function refresh() {
    try {
      maximized = await appWindow.isMaximized();
    } catch {
    }
  }
  refresh();
  $effect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    appWindow.onResized(() => refresh()).then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
    };
  });
</script>

<div
  data-tauri-drag-region
  class="titlebar relative h-8 shrink-0 flex items-center justify-end border-b border-edge select-none"
>
  <div
    class="absolute left-5 flex items-center gap-2 pointer-events-none eyebrow"
  >
    <span class="text-accent">＋</span>
    <span>Kurisu <span class="titlebar-caption"> / Anime Tracker</span></span>
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
  .titlebar { background: #020202; }
  .titlebar-caption { color: var(--color-ink-dim); text-transform: none; letter-spacing: .04em; }
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
    background: #8e4943;
    color: var(--color-ink);
  }
</style>
