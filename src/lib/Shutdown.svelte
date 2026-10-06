<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { flushPendingEdits } from "$lib/pendingEdits";

  let saving = $state(false);
  let error = $state("");

  $effect(() => {
    let alive = true;
    let unlisten: (() => void) | undefined;
    void listen("kurisu://shutdown-requested", async () => {
      if (saving) return;
      saving = true;
      error = "";
      let success = false;
      try {
        await flushPendingEdits();
        success = true;
      } catch (e) {
        error = `Could not save progress: ${String(e)}. Kurisu is still open.`;
      }
      try {
        await invoke("finish_shutdown", { success });
        if (success) return;
      } catch (e) {
        error = String(e);
      }
      saving = false;
    }).then(async (un) => {
      if (!alive) { un(); return; }
      unlisten = un;
      await invoke("shutdown_ready");
    }).catch((e) => { error = String(e); });
    return () => { alive = false; unlisten?.(); };
  });
</script>

{#if saving}
  <Dialog onclose={() => {}} busy layer={100}>
    <p class="text-ink" role="status">Saving progress before closing…</p>
  </Dialog>
{:else if error}
  <div class="fixed bottom-4 left-4 right-4 z-[100] border border-edge bg-panel p-4 text-red-400" role="alert">
    {error}
    <button class="ml-4 text-ink" onclick={() => { error = ""; }}>Dismiss</button>
  </div>
{/if}
