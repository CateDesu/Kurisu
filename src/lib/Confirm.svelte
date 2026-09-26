<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
  let {
    title,
    body = "",
    confirmLabel = "Confirm",
    busy = false,
    onconfirm,
    oncancel,
  }: {
    title: string;
    body?: string;
    confirmLabel?: string;
    busy?: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  function cancel() {
    if (!busy) oncancel();
  }
</script>

<Dialog onclose={cancel} {busy} layer={55}>
  <h3 class="font-semibold mb-1">{title}</h3>
  {#if body}
    <p class="text-sm text-ink-dim mb-4">{body}</p>
  {/if}
  <div class="flex justify-end items-center gap-2 mt-4">
    <button
      onclick={cancel}
      disabled={busy}
      class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
    >
      Cancel
    </button>
    <button
      onclick={onconfirm}
      disabled={busy}
      class="px-3 py-1.5 rounded-md bg-red-600 hover:bg-red-500 text-white text-sm disabled:opacity-50"
    >
      {busy ? "Working…" : confirmLabel}
    </button>
  </div>
</Dialog>
