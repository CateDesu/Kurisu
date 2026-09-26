<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { installInFlight, runInstallUpdate } from "$lib/update.svelte";
  import type { UpdateInfo } from "$lib/types";

  let update = $state<UpdateInfo | null>(null);
  let err = $state("");
  let installed = $state(false);
  let failedMsg = $state("");
  // The event and startup pull can deliver the same release twice.
  let dismissedTag = "";
  let failedSeen = false;

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
    // Pull stashed results in case startup events fired before the listeners existed.
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
    } catch (e) {
      err = String(e);
    }
  }

  function later() {
    if (installInFlight() || !update) return;
    dismissedTag = update.tag;
    update = null;
  }
</script>

{#if update}
  <Dialog onclose={later} busy={installInFlight()}>
    <h3 class="font-semibold mb-1">Update available</h3>
    {#if installed}
      <p class="text-sm text-ink-dim mb-3">
        Kurisu <b class="text-ink">{update.version}</b> is installed — restart Kurisu to finish.
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
  </Dialog>
{/if}

{#if failedMsg}
  <Dialog onclose={() => (failedMsg = "")} closeOnBackdrop={false}>
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
  </Dialog>
{/if}
