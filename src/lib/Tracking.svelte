<script lang="ts">
  import { listen, emit } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import Icon from "$lib/Icon.svelte";
  import type { NowPlaying, TrackingPrompt } from "$lib/types";

  // `null` = nothing playing / banner hidden.
  let nowPlaying = $state<NowPlaying | null>(null);
  let prompt = $state<TrackingPrompt | null>(null);
  let busy = $state(false);
  let err = $state("");

  const pct = $derived(
    nowPlaying && nowPlaying.length_us > 0
      ? Math.min(100, Math.round((nowPlaying.position_us / nowPlaying.length_us) * 100))
      : 0
  );

  let promptDialog = $state<HTMLDivElement | null>(null);

  $effect(() => {
    let alive = true;
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    listen<NowPlaying>("kurisu://now-playing", (e) => {
      // `active: false` means playback stopped → hide the banner.
      nowPlaying = e.payload?.active ? e.payload : null;
    }).then((u) => (alive ? (un1 = u) : u()));
    listen<TrackingPrompt>("kurisu://tracking-prompt", (e) => {
      prompt = e.payload;
      err = "";
    }).then((u) => (alive ? (un2 = u) : u()));
    return () => {
      alive = false;
      un1?.();
      un2?.();
    };
  });

  // Escape dismisses the prompt. The prompt renders above page modals (z-[60]),
  // so it gets first claim on Escape and stops it reaching modals below.
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
      // The modal can sit open for a while — progress may have moved past the
      // detected episode since the prompt was emitted. Don't rewind it.
      const fresh = await api.getEntry(p.media_id);
      if (fresh && fresh.progress >= p.episode) {
        if (prompt === p) prompt = null;
        return;
      }
      // Set progress to the detected episode (not a blind +1), so mid-cour skips
      // land correctly. The modal only offers this when it's ahead of progress.
      const entry = await api.setProgress(p.media_id, p.episode);
      // Unify the list-refresh signal: auto-increment emits this from the
      // backend, the prompt path emits it here so one listener covers both.
      await emit("kurisu://episode-updated", entry);
      // A NEWER prompt may have replaced ours while the write was in flight —
      // only close the one we actually confirmed.
      if (prompt === p) prompt = null;
    } catch (e) {
      // Keep the prompt open and say why — closing on failure reads as success.
      err = String(e);
    } finally {
      busy = false;
    }
  }

  function skip() {
    prompt = null;
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

{#if nowPlaying}
  <div class="flex items-center gap-3 px-4 py-1.5 border-b border-edge bg-panel text-xs shrink-0">
    <span class="text-accent leading-none grid place-items-center"><Icon name="play" size={12} /></span>
    <span class="truncate max-w-[40%]">
      {#if nowPlaying.matched}
        <span class="font-medium">{nowPlaying.matched}</span>
        {#if nowPlaying.episode != null}
          <span class="text-ink-dim"> · Ep {nowPlaying.episode}</span>
        {/if}
      {:else}
        <span class="text-ink-dim italic">Detected: {nowPlaying.title || "unknown track"}</span>
      {/if}
    </span>
    {#if nowPlaying.length_us > 0}
      <div class="flex-1 h-1 bg-edge rounded overflow-hidden min-w-[40px]">
        <!-- scaleX keeps this animation on the compositor; a width transition
             forces layout every frame -->
        <div class="h-full bg-accent origin-left transition-transform duration-500" style="transform:scaleX({pct / 100})"></div>
      </div>
      <span class="text-ink-dim tabular-nums w-9 text-right">{pct}%</span>
    {:else}
      <div class="flex-1"></div>
    {/if}
    <span class="text-ink-dim shrink-0">{nowPlaying.player}</span>
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
          class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
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
