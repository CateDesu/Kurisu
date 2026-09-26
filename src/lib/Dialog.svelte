<script module lang="ts">
  const dialogs: { element: HTMLDivElement; layer: number }[] = [];

  function topDialog() {
    return dialogs.reduce<(typeof dialogs)[number] | undefined>((top, current) => {
      if (!top || current.layer > top.layer) return current;
      if (
        current.layer === top.layer &&
        top.element.compareDocumentPosition(current.element) & Node.DOCUMENT_POSITION_FOLLOWING
      ) return current;
      return top;
    }, undefined);
  }
</script>

<script lang="ts">
  import { onMount, type Snippet } from "svelte";

  let { children, onclose, busy = false, size = "sm", layer = 50, closeOnBackdrop = true }: {
    children: Snippet;
    onclose: () => void;
    busy?: boolean;
    size?: "sm" | "md";
    layer?: number;
    closeOnBackdrop?: boolean;
  } = $props();

  let dialog: HTMLDivElement;
  onMount(() => {
    const previous = document.activeElement;
    const entry = { element: dialog, layer };
    dialogs.push(entry);
    if (topDialog() === entry) dialog.focus();
    return () => {
      const wasTop = topDialog() === entry;
      dialogs.splice(dialogs.indexOf(entry), 1);
      if (wasTop) {
        const next = topDialog();
        if (next) next.element.focus();
        else if (previous instanceof HTMLElement && previous.isConnected) previous.focus();
      }
    };
  });

  function dismiss() {
    if (!busy) onclose();
  }

  function onkeydown(event: KeyboardEvent) {
    if (event.key !== "Escape" || event.defaultPrevented || topDialog()?.element !== dialog) return;
    event.stopImmediatePropagation();
    dismiss();
  }
</script>

<svelte:window {onkeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 bg-black/60 grid place-items-center backdrop-blur-sm"
  style:z-index={layer}
  onclick={() => { if (closeOnBackdrop) dismiss(); }}
  role="presentation"
>
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    bind:this={dialog}
    class="bg-panel border border-edge rounded-xl p-5 w-full mx-4 shadow-2xl {size === 'md' ? 'max-w-md' : 'max-w-sm'}"
    onclick={(event) => event.stopPropagation()}
    role="dialog"
    aria-modal="true"
    tabindex="-1"
  >
    {@render children()}
  </div>
</div>
