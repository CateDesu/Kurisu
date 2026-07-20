<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import type { UpdateInfo } from "$lib/types";

  // Shown when the backend's startup check finds a newer release (the
  // `kurisu://update-available` event). Windows-only in practice — the backend
  // never emits elsewhere. `null` = hidden.
  let update = $state<UpdateInfo | null>(null);
  let busy = $state(false);
  let err = $state("");

  $effect(() => {
    let un: (() => void) | undefined;
    listen<UpdateInfo>("kurisu://update-available", (e) => {
      update = e.payload;
      err = "";
    }).then((u) => (un = u));
    return () => un?.();
  });

  async function install() {
    if (!update || busy) return;
    busy = true;
    err = "";
    try {
      // On success the installer has launched and the app quits itself, so
      // there is nothing more to render here.
      await api.installUpdate();
    } catch (e) {
      err = String(e);
      busy = false;
    }
  }

  function later() {
    update = null;
  }
</script>

{#if update}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
    onclick={later}
    role="presentation"
  >
    <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
    <div
      class="bg-panel border border-edge rounded-xl p-5 max-w-sm w-full mx-4 shadow-2xl"
      onclick={(e) => e.stopPropagation()}
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <h3 class="font-semibold mb-1">Update available</h3>
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
        Downloads the installer, then closes Kurisu so it can finish.
      </p>
    </div>
  </div>
{/if}
