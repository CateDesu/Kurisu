<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
  import { listen, emit } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { nowPlaying } from "$lib/nowplaying.svelte";
  import Icon from "$lib/Icon.svelte";
  import type { TrackingPrompt } from "$lib/types";

  let prompt = $state<TrackingPrompt | null>(null);
  // Queue prompts while the window is hidden or another prompt is open.
  let queued = $state<TrackingPrompt[]>([]);
  let busy = $state(false);
  let err = $state("");

  const np = $derived(nowPlaying());
  const pct = $derived(
    np && np.length_us > 0
      ? Math.min(100, Math.round((np.position_us / np.length_us) * 100))
      : 0
  );

  function presentPrompt(p: TrackingPrompt) {
    if (prompt) {
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
    const epoch = auth.epoch;
    prompt = null;
    queued = [];
    busy = false;
    err = "";
    if (!auth.isLoggedIn) return;
    let alive = true;
    let un1: (() => void) | undefined;
    let un2: (() => void) | undefined;
    listen<TrackingPrompt>("kurisu://tracking-prompt", (e) => {
      if (!alive || epoch !== auth.epoch || !auth.isLoggedIn) return;
      const p = e.payload;
      if (!p) return;
      presentPrompt(p);
    }).then((u) => (alive ? (un1 = u) : u()));
    listen<TrackingPrompt>("kurisu://tracking-ask", (e) => {
      if (!alive || epoch !== auth.epoch || !auth.isLoggedIn) return;
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

  async function confirm() {
    if (!prompt || busy || !auth.isLoggedIn) return;
    const p = prompt;
    const epoch = auth.epoch;
    const current = () => epoch === auth.epoch && auth.isLoggedIn && prompt === p;
    busy = true;
    err = "";
    try {
      // Progress may have moved while the prompt was open. Never rewind.
      const fresh = await api.getEntry(p.media_id);
      if (!current()) return;
      if (!fresh) {
        if (prompt === p) dismiss();
        return;
      }
      if (fresh.progress >= p.episode) {
        if (prompt === p) dismiss();
        return;
      }
      // Pass the fresh baseline so a concurrent edit cannot be overwritten.
      const entry = await api.setProgress(p.media_id, p.episode, fresh.progress);
      if (!current()) return;
      await emit("kurisu://episode-updated", entry);
      if (prompt === p) dismiss();
    } catch (e) {
      if (current()) err = String(e);
    } finally {
      if (epoch === auth.epoch) busy = false;
    }
  }

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
    if (busy) return;
    dismiss();
  }
</script>

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
  {#key prompt}
    <Dialog onclose={skip} {busy} layer={60}>
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
    </Dialog>
  {/key}
{/if}
