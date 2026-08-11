<script lang="ts">
  import { listen, emit } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { nowPlaying } from "$lib/nowplaying.svelte";
  import Icon from "$lib/Icon.svelte";
  import type { TrackingPrompt } from "$lib/types";

  // Reads the shared now-playing store instead of attaching its own listener.
  let prompt = $state<TrackingPrompt | null>(null);
  // Prompts queued while another was open. Close-to-tray keeps the webview
  // alive but hidden, so a night of playback used to stack prompts and only
  // the last survived.
  let queued = $state<TrackingPrompt[]>([]);
  let busy = $state(false);
  let err = $state("");

  const np = $derived(nowPlaying());
  const pct = $derived(
    np && np.length_us > 0
      ? Math.min(100, Math.round((np.position_us / np.length_us) * 100))
      : 0
  );

  let promptDialog = $state<HTMLDivElement | null>(null);

  function presentPrompt(p: TrackingPrompt) {
    if (prompt) {
      // Don't drop a prompt. De-dupe so repeats don't stack.
      const dup =
        (prompt.media_id === p.media_id && prompt.episode === p.episode) ||
        queued.some((q) => q.media_id === p.media_id && q.episode === p.episode);
      if (!dup) queued = [...queued, p];
      return;
    }
    prompt = p;
    err = "";
  }

  $effect(() => {
    let alive = true;
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    // 120s prompt mode. No navigation.
    listen<TrackingPrompt>("kurisu://tracking-prompt", (e) => {
      if (!alive) return;
      const p = e.payload;
      if (!p) return;
      presentPrompt(p);
    }).then((u) => (alive ? (un1 = u) : u()));
    // 15s auto-ask. Switch to Currently Watching first so the show is
    // visible when the prompt appears.
    listen<TrackingPrompt>("kurisu://tracking-ask", (e) => {
      if (!alive) return;
      const p = e.payload;
      if (!p) return;
      goto("/now");
      presentPrompt(p);
    }).then((u) => (alive ? (un2 = u) : u()));
    return () => {
      alive = false;
      un1?.();
      un2?.();
    };
  });

  // Escape dismisses. The prompt sits above page modals so it claims Escape
  // first.
  function onWindowKeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && prompt) {
      e.stopImmediatePropagation();
      skip();
    }
  }
  $effect(() => {
    if (prompt) promptDialog?.focus();
  });

  async function confirm() {
    if (!prompt || busy) return;
    const p = prompt;
    busy = true;
    err = "";
    try {
      // The modal can sit open for a while. Progress may have moved past
      // the detected episode. Don't rewind.
      const fresh = await api.getEntry(p.media_id);
      // No row means the show was removed from the list while the prompt
      // was open. Don't recreate it. Dismiss.
      if (!fresh) {
        if (prompt === p) dismiss();
        return;
      }
      if (fresh.progress >= p.episode) {
        if (prompt === p) dismiss();
        return;
      }
      // Set to the detected episode, not a blind +1, so skips land right.
      // fresh.progress is the CAS baseline. Backend declines if something
      // else moved it between read and write.
      const entry = await api.setProgress(p.media_id, p.episode, fresh.progress);
      // Unify the refresh signal. Auto-increment emits from the backend,
      // prompt emits here. One listener covers both.
      await emit("kurisu://episode-updated", entry);
      // A newer prompt may have replaced ours mid-write. Only close the
      // one we confirmed.
      if (prompt === p) dismiss();
    } catch (e) {
      // Only show the error on the prompt we confirmed. A skip may have
      // swapped in another prompt mid-write and it must not inherit this.
      if (prompt === p) err = String(e);
    } finally {
      busy = false;
    }
  }

  /// Close current prompt, show next queued if any.
  function dismiss() {
    if (queued.length > 0) {
      prompt = queued[0];
      queued = queued.slice(1);
      err = "";
    } else {
      prompt = null;
    }
  }

  function skip() {
    dismiss();
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

{#if np}
  <div class="flex items-center gap-3 px-4 py-1.5 border-b border-edge bg-panel text-xs shrink-0">
    <span class="text-accent leading-none grid place-items-center"><Icon name="play" size={12} /></span>
    <span class="truncate max-w-[40%]">
      {#if np.matched}
        <span class="font-medium">{np.matched}</span>
        {#if np.episode != null}
          <span class="text-ink-dim"> · Ep {np.episode}</span>
        {/if}
      {:else}
        <span class="text-ink-dim italic">Detected: {np.title || "unknown track"}</span>
      {/if}
    </span>
    {#if np.length_us > 0}
      <div class="flex-1 h-1 bg-edge rounded overflow-hidden min-w-[40px]">
        <!-- scaleX stays on the compositor. Width transition would force layout per frame -->
        <div class="h-full bg-accent origin-left transition-transform duration-500" style="transform:scaleX({pct / 100})"></div>
      </div>
      <span class="text-ink-dim tabular-nums w-9 text-right">{pct}%</span>
    {:else}
      <div class="flex-1"></div>
    {/if}
    <span class="text-ink-dim shrink-0">{np.player}</span>
  </div>
{/if}

{#if prompt}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="fixed inset-0 bg-black/60 grid place-items-center z-[60] backdrop-blur-sm"
    onclick={skip}
    role="presentation"
  >
    <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
    <div
      bind:this={promptDialog}
      class="bg-panel border border-edge rounded-xl p-5 max-w-sm w-full mx-4 shadow-2xl"
      onclick={(e) => e.stopPropagation()}
      role="dialog"
      aria-modal="true"
      tabindex="-1"
    >
      <h3 class="font-semibold mb-1">Update your list?</h3>
      <p class="text-sm text-ink-dim mb-3">Detected playback:</p>
      <p class="text-sm font-medium mb-1">{prompt.title}</p>
      <p class="text-sm text-ink-dim mb-4">
        Episode {prompt.episode}
        {#if prompt.raw_title && prompt.raw_title !== prompt.title}
          <span class="block text-xs mt-1 opacity-70">{prompt.raw_title}</span>
        {/if}
      </p>
      <div class="flex justify-end items-center gap-2">
        {#if prompt.episode <= prompt.progress}
          <span class="text-xs text-ink-dim mr-auto">Already past Ep {prompt.episode} (rewatch)</span>
        {/if}
        <button
          onclick={skip}
          disabled={busy}
          class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
        >
          Skip
        </button>
        {#if prompt.episode > prompt.progress}
          <button
            onclick={confirm}
            disabled={busy}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
          >
            {busy ? "Updating…" : `Set progress to Ep ${prompt.episode}`}
          </button>
        {/if}
      </div>
      {#if err}
        <p class="text-xs text-red-400 mt-2">Update failed: {err}</p>
      {/if}
    </div>
  </div>
{/if}
