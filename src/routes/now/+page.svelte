<script lang="ts">
  import { listen, emit } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { openPath } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import { nowPlaying } from "$lib/nowplaying.svelte";
  import { displayTitle, scoreLabel, type ListEntry } from "$lib/types";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import Login from "$lib/Login.svelte";

  // Detected show's list entry. Cover and progress come from the joined media.
  let entry = $state<ListEntry | null>(null);
  let updating = $state(false);
  let error = $state("");

  // Idle state continue watching. The user's CURRENT entries.
  let current = $state<ListEntry[]>([]);

  const np = $derived(nowPlaying());
  const pct = $derived(
    np && np.length_us > 0
      ? Math.min(100, Math.round((np.position_us / np.length_us) * 100))
      : 0
  );
  // Next unwatched episode file for the detected show.
  const nextFile = $derived(
    np?.media_id != null && entry
      ? library.fileFor(np.media_id, (entry.progress ?? 0) + 1)
      : undefined
  );

  // Tracks can skip faster than getEntry resolves. Shared latest-wins
  // guard with the event listener and updateTo below, so an older read
  // never overwrites a newer one.
  let entryLoadId = 0;
  async function loadEntry(mediaId: number) {
    const lid = ++entryLoadId;
    try {
      const e = await api.getEntry(mediaId);
      if (lid === entryLoadId) entry = e;
    } catch {
      if (lid === entryLoadId) entry = null;
    }
  }

  $effect(() => {
    const id = np?.media_id ?? null;
    if (id != null) loadEntry(id);
    else entry = null;
  });

  $effect(() => {
    if (auth.isLoggedIn) {
      loadCurrent();
      // Make sure library files are loaded for the Play buttons.
      library.loadFolders().then(() => {
        if (library.folders.length > 0 && !library.hasScan) library.scan();
      });
    }
  });

  async function loadCurrent() {
    try {
      const all = await api.localEntries();
      current = all.filter((e) => e.status === "CURRENT");
    } catch {
      current = [];
    }
  }

  // Refresh when progress is written anywhere. entryLoadId guards
  // against burst races. N events spawn N concurrent loads, only the
  // last result is applied.
  $effect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    listen("kurisu://episode-updated", () => {
      if (!alive) return;
      const id = np?.media_id ?? null;
      if (id != null) {
        const lid = ++entryLoadId;
        api.getEntry(id).then((e) => {
          if (alive && lid === entryLoadId) entry = e;
        }).catch(() => {});
      }
      loadCurrent();
    }).then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
    };
  });

  async function updateTo(episode: number) {
    const id = np?.media_id;
    if (id == null || updating) return;
    updating = true;
    error = "";
    let fresh: ListEntry | null = null;
    try {
      fresh = await api.getEntry(id);
    } catch (e) {
      error = String(e);
      updating = false;
      return;
    }
    if (!fresh) {
      error = "This show is no longer on your list.";
      updating = false;
      return;
    }
    if (fresh.progress >= episode) {
      entry = fresh;
      updating = false;
      return;
    }
    // Optimistic. Reflect the new progress immediately so the click feels
    // instant while the AniList round-trip runs.
    entry = { ...fresh, progress: episode };
    try {
      const saved = await api.setProgress(id, episode, fresh.progress);
      await emit("kurisu://episode-updated", saved);
      entry = saved;
      await loadCurrent();
    } catch (e) {
      error = String(e);
      // Re-fetch instead of reverting to a stale snapshot. The listener
      // may have already updated entry past the pre-click value. Guarded
      // by entryLoadId so a concurrent load isn't clobbered.
      const lid = ++entryLoadId;
      try { const e2 = await api.getEntry(id); if (lid === entryLoadId) entry = e2; } catch { /* keep what we have */ }
    } finally {
      updating = false;
    }
  }

  async function play(path: string) {
    error = "";
    try {
      await openPath(path);
    } catch (e) {
      error = String(e);
    }
  }

  function fmtTime(us: number): string {
    const s = Math.max(0, Math.round(us / 1_000_000));
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${r.toString().padStart(2, "0")}`;
  }
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-3xl mx-auto">
    <h1 class="text-xl font-semibold mb-4">Currently Watching</h1>

    {#if np && np.active}
      {@const detectedEp = np.episode}
      {@const canUpdate = detectedEp != null && entry != null && detectedEp > (entry.progress ?? 0)}
      {@const cover = entry?.media?.cover_large ?? entry?.media?.cover_medium ?? null}
      <section class="bg-panel border border-edge rounded-xl p-4 mb-6">
        <div class="flex items-start gap-4">
          {#if cover}
            <button
              type="button"
              onclick={() => np.media_id && goto(`/anime/${np.media_id}`)}
              title="Open details"
              class="shrink-0"
            >
              <Img src={cover} class="w-24 h-36 object-cover rounded-lg" />
            </button>
          {:else}
            <div class="w-24 h-36 bg-panel-2 rounded-lg shrink-0 grid place-items-center text-ink-dim text-xs">
              No cover
            </div>
          {/if}
          <div class="flex-1 min-w-0">
            {#if np.matched}
              <button
                type="button"
                onclick={() => np.media_id && goto(`/anime/${np.media_id}`)}
                class="block max-w-full text-left font-semibold text-lg truncate hover:text-accent transition-colors"
              >
                {np.matched}
              </button>
              {#if entry}
                <div class="text-sm text-ink-dim mb-2">
                  Ep {entry.progress}{entry.media?.episodes ? `/${entry.media.episodes}` : ""} on your list
                </div>
              {/if}
            {:else}
              <div class="font-semibold text-lg truncate mb-1">Not on your list</div>
            {/if}

            {#if detectedEp != null}
              <div class="text-sm mb-2">
                Detected <span class="font-medium text-accent">Episode {detectedEp}</span>
              </div>
            {/if}

            {#if np.length_us > 0}
              <div class="flex items-center gap-2 mb-1">
                <div class="flex-1 h-1.5 bg-edge rounded overflow-hidden min-w-[60px]">
                  <div class="h-full bg-accent origin-left transition-transform duration-500" style="transform:scaleX({pct / 100})"></div>
                </div>
                <span class="text-xs text-ink-dim tabular-nums">
                  {fmtTime(np.position_us)} / {fmtTime(np.length_us)}
                </span>
              </div>
            {/if}

            <div class="text-xs text-ink-dim mb-3 truncate">Playing in {np.player}</div>

            <div class="flex items-center gap-2 flex-wrap">
              {#if nextFile}
                <button
                  onclick={() => play(nextFile.path)}
                  title="Play the next downloaded episode"
                  class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm flex items-center gap-1.5"
                >
                  <Icon name="play" size={13} /> Play Ep {nextFile.episode}
                </button>
              {/if}
              {#if np.matched && detectedEp != null}
                {#if canUpdate}
                  <button
                    onclick={() => updateTo(detectedEp)}
                    disabled={updating}
                    class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
                  >
                    {updating ? "Updating…" : `Update to Ep ${detectedEp}`}
                  </button>
                {:else if entry}
                  <span class="text-sm text-ink-dim">
                    {#if detectedEp <= (entry.progress ?? 0)}
                      Already watched Ep {detectedEp}
                    {:else}
                      Up to date
                    {/if}
                  </span>
                {/if}
              {/if}
            </div>
            {#if error}
              <p class="text-xs text-red-400 mt-2">Update failed: {error}</p>
            {/if}
          </div>
        </div>
      </section>
    {:else}
      <div class="text-center text-ink-dim py-8 mb-2 border border-dashed border-edge rounded-xl">
        <div class="text-sm">Nothing playing right now.</div>
        <div class="text-xs mt-1 opacity-70">
          Play an anime in MPV, VLC, or Haruna and it'll show up here.
        </div>
      </div>
    {/if}

    {#if current.length > 0}
      <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Continue watching</h2>
      <div class="grid grid-cols-1 gap-2">
        {#each current as e (e.media_id)}
          {@const sc = scoreLabel(e.score, auth.user?.score_format)}
          {@const nf = library.fileFor(e.media_id, (e.progress ?? 0) + 1)}
          <div class="cv-row flex items-center gap-3 bg-panel border border-edge rounded-lg p-2.5">
            {#if e.media?.cover_medium}
              <button
                type="button"
                onclick={() => goto(`/anime/${e.media_id}`)}
                title="Open details"
                class="shrink-0"
              >
                <Img src={e.media.cover_medium} class="w-10 h-14 object-cover rounded" />
              </button>
            {:else}
              <div class="w-10 h-14 bg-panel-2 rounded shrink-0"></div>
            {/if}
            <div
              class="flex-1 min-w-0 cursor-pointer hover:text-accent transition-colors"
              role="button"
              tabindex="0"
              onclick={() => goto(`/anime/${e.media_id}`)}
              onkeydown={(ev) => {
                if (ev.key === "Enter" || ev.key === " ") {
                  ev.preventDefault();
                  goto(`/anime/${e.media_id}`);
                }
              }}
            >
              <div class="truncate font-medium">{displayTitle(e.media)}</div>
              <div class="text-xs text-ink-dim">
                Ep {e.progress}{e.media?.episodes ? `/${e.media.episodes}` : ""}
                {#if sc}<span class="opacity-40">·</span> {sc}{/if}
              </div>
            </div>
            {#if nf}
              <button
                onclick={() => play(nf.path)}
                title={`Play Ep ${nf.episode}`}
                class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm shrink-0 flex items-center gap-1.5"
              >
                <Icon name="play" size={13} /> Ep {nf.episode}
              </button>
            {/if}
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/if}
