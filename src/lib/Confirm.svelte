<script lang="ts">
  // Shared confirm for destructive actions. Matches the tracking prompt's
  // modal styling. Render conditionally from the parent and wire the two
  // callbacks. Escape and backdrop cancel, both refused while busy.
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

  let dialog = $state<HTMLDivElement | null>(null);
  $effect(() => dialog?.focus());

  function cancel() {
    if (!busy) oncancel();
  }

  function onWindowKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.stopImmediatePropagation();
      cancel();
    }
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 bg-black/60 grid place-items-center z-[55] backdrop-blur-sm"
  onclick={cancel}
  role="presentation"
>
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    bind:this={dialog}
    class="bg-panel border border-edge rounded-xl p-5 max-w-sm w-full mx-4 shadow-2xl"
    onclick={(e) => e.stopPropagation()}
    role="dialog"
    aria-modal="true"
    tabindex="-1"
  >
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
  </div>
</div>
