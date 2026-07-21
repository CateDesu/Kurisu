<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import type { UpdateInfo } from "$lib/types";

  // Shown when the backend's startup check finds a newer release (the
  // `kurisu://update-available` event). `null` = hidden.
  let update = $state<UpdateInfo | null>(null);
  let busy = $state(false);
  let err = $state("");
  // Linux swaps the binary in place and needs a manual restart; Windows
  // quits by itself once the installer launches.
  let installed = $state(false);
  // One-shot notice after a doubly-failed swap (see the backend marker).
  let failedMsg = $state("");

  let updateDialog = $state<HTMLDivElement | null>(null);
  let failedDialog = $state<HTMLDivElement | null>(null);

  $effect(() => {
    let alive = true;
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    listen<UpdateInfo>("kurisu://update-available", (e) => {
      update = e.payload;
      err = "";
      installed = false;
    }).then((u) => (alive ? (un1 = u) : u()));
    listen<{ message: string }>("kurisu://update-failed", (e) => {
      failedMsg = e.payload.message;
    }).then((u) => (alive ? (un2 = u) : u()));
    return () => {
      alive = false;
      un1?.();
      un2?.();
    };
  });

  async function install() {
    if (!update || busy) return;
    busy = true;
    err = "";
    try {
      const result = await api.installUpdate();
      if (result === "installed") installed = true;
      // "restarting": the installer launched and the app quits itself.
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  function later() {
    update = null;
  }

  // Modal behavior: Escape dismisses, dialog takes focus on open.
  function onWindowKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    if (failedMsg) {
      e.stopImmediatePropagation();
      failedMsg = "";
    } else if (update) {
      e.stopImmediatePropagation();
      later();
    }
  }
  $effect(() => {
    if (update) updateDialog?.focus();
  });
  $effect(() => {
    if (failedMsg) failedDialog?.focus();
  });
</script>

<svelte:window onkeydown={onWindowKeydown} />

{#if update}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
    onclick={later}
    role="presentation"
  >
    <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
    <div
      bind:this={updateDialog}
      class="bg-panel border border-edge rounded-xl p-5 max-w-sm w-full mx-4 shadow-2xl"
      onclick={(e) => e.stopPropagation()}
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <h3 class="font-semibold mb-1">Update available</h3>
      {#if installed}
        <p class="text-sm text-ink-dim mb-3">
          Kurisu <b class="text-ink">{update.version}</b> is installed — restart the app to finish.
        </p>
        <div class="flex justify-end">
          <button
            onclick={later}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm"
          >
            Got it
          </button>
        </div>
      {:else}
        <p class="text-sm text-ink-dim mb-3">
          Kurisu <b class="text-ink">{update.version}</b> is out — you're on {update.current}.
        </p>
        {#if update.body}
          <pre class="text-xs text-ink-dim whitespace-pre-wrap max-h-32 overflow-y-auto bg-panel-2 border border-edge rounded-md p-2 mb-4">{update.body}</pre>
        {/if}
        {#if err}
          <p class="text-xs text-red-400 mb-3">Update failed: {err}</p>
        {/if}
        <div class="flex justify-end items-center gap-2">
          <button
            onclick={later}
            class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
          >
            Later
          </button>
          <button
            onclick={install}
            disabled={busy}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
          >
            {busy ? "Downloading…" : "Download & install"}
          </button>
        </div>
        <p class="text-xs text-ink-dim mt-3">
          Downloads the update, then closes Kurisu so it can finish.
        </p>
      {/if}
    </div>
  </div>
{/if}

{#if failedMsg}
  <div
    class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
    role="presentation"
  >
    <div
      bind:this={failedDialog}
      class="bg-panel border border-edge rounded-xl p-5 max-w-sm w-full mx-4 shadow-2xl"
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <h3 class="font-semibold mb-1">Update failed</h3>
      <p class="text-sm text-ink-dim mb-4">{failedMsg}</p>
      <div class="flex justify-end">
        <button
          onclick={() => (failedMsg = "")}
          class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm"
        >
          Got it
        </button>
      </div>
    </div>
  </div>
{/if}
