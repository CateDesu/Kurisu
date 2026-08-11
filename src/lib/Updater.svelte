<script lang="ts">
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { installInFlight, runInstallUpdate } from "$lib/update.svelte";
  import type { UpdateInfo } from "$lib/types";

  // Shown when the backend's startup check finds a newer release. Comes from
  // the `kurisu://update-available` event or the stashed payload pulled on
  // mount below. null means hidden.
  let update = $state<UpdateInfo | null>(null);
  let err = $state("");
  // Linux swaps the binary in place and needs a manual restart. Windows
  // quits by itself once the installer launches.
  let installed = $state(false);
  // One shot notice after a doubly failed swap. See the backend marker.
  let failedMsg = $state("");
  // What the user already dismissed or was already shown this session. The
  // emit and the pull on mount can deliver the same payload twice.
  let dismissedTag = "";
  let failedSeen = false;

  let updateDialog = $state<HTMLDivElement | null>(null);
  let failedDialog = $state<HTMLDivElement | null>(null);

  function showUpdate(info: UpdateInfo) {
    if (!info?.available || info.tag === dismissedTag) return;
    update = info;
    err = "";
    installed = false;
  }

  function showFailed(message: string) {
    if (failedSeen) return;
    failedSeen = true;
    failedMsg = message;
  }

  $effect(() => {
    let alive = true;
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    listen<UpdateInfo>("kurisu://update-available", (e) => showUpdate(e.payload)).then(
      (u) => (alive ? (un1 = u) : u())
    );
    listen<{ message: string }>("kurisu://update-failed", (e) => showFailed(e.payload.message)).then(
      (u) => (alive ? (un2 = u) : u())
    );
    // The emits above are one shot and can fire before this listener exists
    // on a slow cold boot. The backend also stashes both payloads until
    // the UI pulls them. This pull is the reliable path. The listeners are
    // the fast one.
    api
      .takePendingUpdate()
      .then((info) => {
        if (alive && info) showUpdate(info);
      })
      .catch(() => {});
    api
      .takeUpdateFailed()
      .then((msg) => {
        if (alive && msg) showFailed(msg);
      })
      .catch(() => {});
    return () => {
      alive = false;
      un1?.();
      un2?.();
    };
  });

  async function install() {
    if (!update || installInFlight()) return;
    err = "";
    try {
      const result = await runInstallUpdate();
      if (result === "installed") installed = true;
      // "restarting" means the installer launched and the app quits itself.
    } catch (e) {
      err = String(e);
    }
  }

  // Dismissal is refused while an install runs. Hiding the modal would leave
  // the outcome, success or failure, invisible.
  function later() {
    if (installInFlight() || !update) return;
    dismissedTag = update.tag;
    update = null;
  }

  // Escape dismisses. Dialog takes focus on open.
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
            disabled={installInFlight()}
            class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
          >
            Later
          </button>
          <button
            onclick={install}
            disabled={installInFlight()}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
          >
            {installInFlight() ? "Downloading…" : "Download & install"}
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
