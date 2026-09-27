<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { untrack } from "svelte";
  import { listen, emit } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { openPath } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import { nowPlaying } from "$lib/nowplaying.svelte";
  import { displayTitle, scoreLabel, type ListEntry } from "$lib/types";
  import EpisodeStepper from "$lib/EpisodeStepper.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import Login from "$lib/Login.svelte";

  let entry = $state<ListEntry | null>(null);
  let updating = $state(false);
  let error = $state("");
  let stepError = $state("");

  let current = $state<ListEntry[]>([]);
  let currentError = $state("");
  const loggedIn = $derived(auth.isLoggedIn);

  const np = $derived(nowPlaying());
  const playingId = $derived(np?.media_id ?? null);
  const pct = $derived(
    np && np.length_us > 0
      ? Math.min(100, Math.round((np.position_us / np.length_us) * 100))
      : 0
  );
  const nextFile = $derived(
    np?.media_id != null && entry
      ? library.fileFor(np.media_id, (entry.progress ?? 0) + 1)
      : undefined
  );

  // Share this guard across reads, events and writes so stale responses cannot win.
  let entryLoadId = 0;
  let currentLoadId = 0;
  let updateId = 0;
  async function loadEntry(mediaId: number) {
    const lid = ++entryLoadId;
    const epoch = auth.epoch;
    const active = () => lid === entryLoadId && epoch === auth.epoch && auth.isLoggedIn;
    try {
      const e = await api.getEntry(mediaId);
      if (active()) entry = e;
    } catch {
      if (active()) entry = null;
    }
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    playingId;
    entry = null;
    updating = false;
    return () => { entryLoadId++; updateId++; };
  });

  $effect(() => {
    auth.epoch;
    loggedIn;
    current = [];
    currentError = "";
    error = "";
    stepError = "";
    updating = false;
    return () => { currentLoadId++; updateId++; };
  });

  $effect(() => {
    auth.epoch;
    const id = playingId;
    if (auth.isLoggedIn && id != null) {
      untrack(() => { if (!updating) void loadEntry(id); });
    }
  });

  $effect(() => {
    auth.epoch;
    if (!auth.isLoggedIn) return;
    untrack(() => {
      void loadCurrent();
      void library.ensureScan().catch((e) => console.error("library scan failed", e));
    });
  });

  async function loadCurrent() {
    if (!auth.isLoggedIn) return;
    const id = ++currentLoadId;
    const epoch = auth.epoch;
    const active = () => id === currentLoadId && epoch === auth.epoch && auth.isLoggedIn;
    try {
      const all = await api.localEntries();
      if (!active()) return;
      current = all.filter((e) => e.status === "CURRENT");
      currentError = "";
    } catch (e) {
      if (!active()) return;
      current = [];
      currentError = String(e);
    }
  }

  function applyEntry(entry: ListEntry) {
    current = current
      .map((x) => (x.media_id === entry.media_id ? { ...entry, media: entry.media ?? x.media } : x))
      .filter((x) => x.status === "CURRENT");
  }

  $effect(() => {
    const epoch = auth.epoch;
    if (!auth.isLoggedIn) return;
    let alive = true;
    let un: (() => void) | undefined;
    listen("kurisu://episode-updated", () => {
      if (!alive || epoch !== auth.epoch || !auth.isLoggedIn) return;
      const id = np?.media_id ?? null;
      if (id != null) {
        void loadEntry(id);
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
    if (id == null || updating || !auth.isLoggedIn) return;
    const request = ++updateId;
    const epoch = auth.epoch;
    const active = () => request === updateId && epoch === auth.epoch && auth.isLoggedIn && np?.media_id === id;
    entryLoadId++;
    updating = true;
    error = "";
    let fresh: ListEntry | null = null;
    try {
      fresh = await api.getEntry(id);
      if (!active()) return;
      if (!fresh) {
        error = "This show is no longer on your list.";
        return;
      }
      if (fresh.progress >= episode) {
        entry = fresh;
        return;
      }
      entry = { ...fresh, progress: episode };
      const saved = await api.setProgress(id, episode, fresh.progress);
      if (!active()) return;
      entry = saved;
      await emit("kurisu://episode-updated", saved);
      if (active()) await loadCurrent();
    } catch (e) {
      if (!active()) return;
      error = String(e);
      await loadEntry(id);
    } finally {
      if (active()) updating = false;
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
  <div class="page-content">
    <PageHeading section="Now playing" title="Currently Watching" description="Playback and your current shows" />

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
                class="block max-w-full text-left font-semibold text-lg hover:text-accent transition-colors"
              >
                {np.matched}
              </button>
              {#if entry}
                <div class="text-sm text-ink-dim mb-2">
                  Ep {entry.progress}{entry.media?.episodes ? `/${entry.media.episodes}` : ""} on your list
                </div>
              {/if}
            {:else}
              <div class="font-semibold text-lg mb-1">Not on your list</div>
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

            <div class="text-xs text-ink-dim mb-3">Playing in {np.player}</div>

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

    {#if stepError}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4 flex items-center justify-between gap-2">
        <span>Episode update failed: {stepError}</span>
        <button onclick={() => (stepError = "")} class="text-ink-dim hover:text-ink shrink-0">
          <Icon name="x" size={14} />
        </button>
      </div>
    {/if}

    {#if currentError}
      <p class="text-sm text-amber-400">{currentError}</p>
    {:else if current.length > 0}
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
              <div class="font-medium">{displayTitle(e.media)}</div>
              {#if sc}
                <div class="text-xs text-ink-dim">{sc}</div>
              {/if}
            </div>
            <div class="shrink-0">
              <EpisodeStepper
                mediaId={e.media_id}
                progress={e.progress}
                total={e.media?.episodes ?? null}
                onchange={(entry) => { stepError = ""; applyEntry(entry); }}
                onerror={(msg) => { stepError = msg; }}
              />
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
