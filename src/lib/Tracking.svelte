<script lang="ts">
  import { listen, emit } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import type { NowPlaying, TrackingPrompt } from "$lib/types";

  // `null` = nothing playing / banner hidden.
  let nowPlaying = $state<NowPlaying | null>(null);
  let prompt = $state<TrackingPrompt | null>(null);
  let busy = $state(false);

  const pct = $derived(
    nowPlaying && nowPlaying.length_us > 0
      ? Math.min(100, Math.round((nowPlaying.position_us / nowPlaying.length_us) * 100))
      : 0
  );

  $effect(() => {
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    listen<NowPlaying>("kurisu://now-playing", (e) => {
      // `active: false` means playback stopped → hide the banner.
      nowPlaying = e.payload?.active ? e.payload : null;
    }).then((u) => (un1 = u));
    listen<TrackingPrompt>("kurisu://tracking-prompt", (e) => {
      prompt = e.payload;
    }).then((u) => (un2 = u));
    return () => {
      un1?.();
      un2?.();
    };
  });

  async function confirm() {
    if (!prompt || busy) return;
    busy = true;
    try {
      const entry = await api.incrementEpisode(prompt.media_id);
      // Unify the list-refresh signal: auto-increment emits this from the
      // backend, the prompt path emits it here so one listener covers both.
      await emit("kurisu://episode-updated", entry);
    } catch (e) {
      console.error("tracking prompt update failed", e);
    } finally {
      prompt = null;
      busy = false;
    }
  }

  function skip() {
    prompt = null;
  }
</script>

{#if nowPlaying}
  <div class="flex items-center gap-3 px-4 py-1.5 border-b border-edge bg-panel text-xs shrink-0">
    <span class="text-accent leading-none">▶</span>
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
        <div class="h-full bg-accent transition-[width] duration-500" style="width:{pct}%"></div>
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
    class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
    onclick={skip}
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
      <h3 class="font-semibold mb-1">Update your list?</h3>
      <p class="text-sm text-ink-dim mb-3">Detected playback:</p>
      <p class="text-sm font-medium mb-1">{prompt.title}</p>
      <p class="text-sm text-ink-dim mb-4">
        Episode {prompt.episode}
        {#if prompt.raw_title && prompt.raw_title !== prompt.title}
          <span class="block text-xs mt-1 opacity-70">{prompt.raw_title}</span>
        {/if}
      </p>
      <div class="flex justify-end gap-2">
        <button
          onclick={skip}
          class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
        >
          Skip
        </button>
        <button
          onclick={confirm}
          disabled={busy}
          class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
        >
          {busy ? "Updating…" : "Update (+1)"}
        </button>
      </div>
    </div>
  </div>
{/if}
