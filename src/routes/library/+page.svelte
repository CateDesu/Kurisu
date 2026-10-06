<script module lang="ts">
  const COLLATOR = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });
</script>

<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { open } from "@tauri-apps/plugin-dialog";
  import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
  import { goto } from "$app/navigation";
  import { listen } from "@tauri-apps/api/event";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import { listenForListUpdates } from "$lib/list.svelte";
  import { displayTitle, type LibraryFile, type ListEntry, type WatchHistoryItem } from "$lib/types";
  import Confirm from "$lib/Confirm.svelte";
  import Dialog from "$lib/Dialog.svelte";
  import Icon from "$lib/Icon.svelte";
  import LinkAnime from "$lib/LinkAnime.svelte";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

  let entries = $state<ListEntry[]>([]);
  let entriesLoaded = $state(false);
  let error = $state("");
  let linking = $state<string | null>(null);
  let expanded = $state<Set<number>>(new Set());
  let unlinking = $state<Group | null>(null);
  let unlinkBusy = $state(false);
  let removingFolder = $state<string | null>(null);
  let removing = $state<Set<string>>(new Set());
  let scannedAt = $state(0);
  let scannedCount = $state(0);
  let numbering = $state<{ path: string; mediaId: number; title: string } | null>(null);
  let offsetScope = $state<"file" | "folder">("file");
  let episodeOffset = $state(0);
  let offsetLoading = $state(false);
  let offsetBusy = $state(false);
  let offsetError = $state("");
  let offsetConflict = $state(false);
  let offsetReady = $state(false);
  let offsetAttempt = $state(0);
  const offsetFolder = $derived(numbering?.path.replace(/[\\/][^\\/]+$/, "") ?? "");
  const offsetFolderAllowed = $derived(Boolean(offsetFolder) && !library.folders.some((root) => root.replace(/[\\/]+$/, "") === offsetFolder));
  const offsetTarget = $derived(numbering ? offsetScope === "folder" && offsetFolderAllowed ? offsetFolder : numbering.path : "");
  let historyOpen = $state(false);
  let historyItems = $state<WatchHistoryItem[]>([]);
  let historyLoading = $state(false);
  let historyError = $state("");
  let historyMore = $state(false);
  let historyRequest = 0;

  function toggle(mediaId: number) {
    const next = new Set(expanded);
    if (next.has(mediaId)) next.delete(mediaId);
    else next.add(mediaId);
    expanded = next;
  }

  interface Group {
    mediaId: number;
    title: string;
    entry?: ListEntry;
    files: LibraryFile[];
  }

  const groups = $derived.by(() => {
    const entriesByMedia = new Map(entries.map((entry) => [entry.media_id, entry]));
    const byMedia = new Map<number, Group>();
    const unmatched: LibraryFile[] = [];
    for (const f of library.files) {
      if (f.media_id == null) {
        unmatched.push(f);
        continue;
      }
      let g = byMedia.get(f.media_id);
      if (!g) {
        const entry = entriesByMedia.get(f.media_id);
        g = { mediaId: f.media_id, title: f.matched ?? `#${f.media_id}`, entry, files: [] };
        byMedia.set(f.media_id, g);
      }
      g.files.push(f);
    }
    for (const g of byMedia.values()) {
      g.files.sort((a, b) => (a.episode ?? 9999) - (b.episode ?? 9999));
    }
    const matched = [...byMedia.values()].sort((a, b) =>
      COLLATOR.compare(a.title, b.title)
    );
    return { matched, unmatched };
  });

  let entryLoadId = 0;
  async function refreshEntries() {
    const id = ++entryLoadId;
    const epoch = auth.epoch;
    try {
      const fresh = await api.localEntries();
      if (id !== entryLoadId || epoch !== auth.epoch || !auth.isLoggedIn) return;
      entries = fresh;
      entriesLoaded = true;
    } catch (e) {
      if (id === entryLoadId && epoch === auth.epoch && auth.isLoggedIn) throw e;
    }
  }

  async function load() {
    const epoch = auth.epoch;
    error = "";
    try {
      await refreshEntries();
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      await library.ensureScan();
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    }
  }

  async function pickFolder() {
    const epoch = auth.epoch;
    const chosen = await open({ directory: true, multiple: false });
    if (typeof chosen !== "string" || epoch !== auth.epoch || !auth.isLoggedIn) return;
    error = "";
    try {
      await library.addFolder(chosen);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      await library.scan();
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    }
  }

  async function removeFolder(path: string) {
    if (removing.has(path) || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    error = "";
    removing = new Set(removing).add(path);
    try {
      await library.removeFolder(path);
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (epoch === auth.epoch) {
        const next = new Set(removing);
        next.delete(path);
        removing = next;
        removingFolder = null;
      }
    }
  }

  async function rescan() {
    if (library.scanning) return;
    const epoch = auth.epoch;
    error = "";
    try {
      await Promise.all([library.scan(), refreshEntries()]);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      scannedCount = library.files.length;
      const stamp = Date.now();
      scannedAt = stamp;
      setTimeout(() => {
        if (scannedAt === stamp) scannedAt = 0;
      }, 3000);
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
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

  async function reveal(path: string) {
    error = "";
    try {
      await revealItemInDir(path);
    } catch (e) {
      error = String(e);
    }
  }

  function basename(path: string): string {
    return path.split(/[\\/]/).pop() ?? path;
  }

  function nextFile(g: Group): LibraryFile | undefined {
    const progress = g.entry?.progress ?? 0;
    return g.files.find((f) => f.episode === progress + 1);
  }

  function isWatched(g: Group, f: LibraryFile): boolean {
    return f.episode != null && f.episode <= (g.entry?.progress ?? -1);
  }

  function cover(g: Group): string | null {
    return g.entry?.media?.cover_medium ?? null;
  }

  function hasBound(g: Group): boolean {
    return g.files.some((f) => f.bound);
  }

  function editNumbering(g: Group, file: LibraryFile) {
    offsetScope = "file";
    numbering = { path: file.path, mediaId: g.mediaId, title: g.entry ? displayTitle(g.entry.media) : g.title };
  }

  $effect(() => {
    offsetAttempt;
    const target = offsetTarget;
    const selected = numbering;
    const epoch = auth.epoch;
    if (!target || !selected) return;
    let current = true;
    offsetLoading = true;
    offsetReady = false;
    offsetError = "";
    offsetConflict = false;
    episodeOffset = 0;
    api.getLibraryBindingDetails(target).then((binding) => {
      if (!current || epoch !== auth.epoch) return;
      episodeOffset = binding?.episode_offset ?? 0;
      offsetConflict = binding != null && binding.media_id !== selected.mediaId;
      offsetReady = true;
    }).catch((e) => {
      if (current && epoch === auth.epoch) offsetError = String(e);
    }).finally(() => {
      if (current && epoch === auth.epoch) offsetLoading = false;
    });
    return () => { current = false; };
  });

  async function saveNumbering() {
    if (!numbering || offsetBusy || offsetLoading || !offsetReady || offsetConflict) return;
    offsetError = "";
    if (!Number.isInteger(episodeOffset) || Math.abs(episodeOffset) > 9999) {
      offsetError = "Enter a whole-number offset between -9999 and 9999.";
      return;
    }
    const epoch = auth.epoch;
    offsetBusy = true;
    try {
      await api.bindLibraryPath(offsetTarget, numbering.mediaId, episodeOffset);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      await library.scan();
      if (epoch === auth.epoch) numbering = null;
    } catch (e) {
      if (epoch === auth.epoch) offsetError = String(e);
    } finally {
      if (epoch === auth.epoch) offsetBusy = false;
    }
  }

  async function loadHistory(more = false) {
    if ((more && historyLoading) || !auth.isLoggedIn) return;
    const request = ++historyRequest;
    const epoch = auth.epoch;
    historyLoading = true;
    historyError = "";
    try {
      const rows = await api.getWatchHistory(50, more ? historyItems.at(-1)?.id : undefined);
      if (request !== historyRequest || epoch !== auth.epoch || !auth.isLoggedIn) return;
      historyItems = more ? [...historyItems, ...rows] : rows;
      historyMore = rows.length === 50;
    } catch (e) {
      if (request === historyRequest && epoch === auth.epoch) historyError = String(e);
    } finally {
      if (request === historyRequest && epoch === auth.epoch) historyLoading = false;
    }
  }

  function toggleHistory() {
    historyOpen = !historyOpen;
    if (historyOpen) void loadHistory();
  }

  async function unlink(g: Group) {
    if (unlinkBusy || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    error = "";
    unlinkBusy = true;
    try {
      await api.unbindLibraryMedia(g.mediaId);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      await library.scan();
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (epoch === auth.epoch) {
        unlinkBusy = false;
        unlinking = null;
      }
    }
  }

  $effect(() => {
    const epoch = auth.epoch;
    const loggedIn = auth.isLoggedIn;
    entryLoadId++;
    entries = [];
    entriesLoaded = false;
    linking = null;
    numbering = null;
    offsetBusy = false;
    offsetLoading = false;
    historyRequest++;
    historyItems = [];
    historyOpen = false;
    historyLoading = false;
    historyError = "";
    unlinking = null;
    removingFolder = null;
    removing = new Set();
    unlinkBusy = false;
    expanded = new Set();
    error = "";
    scannedAt = 0;
    if (!loggedIn) return;
    let alive = true;
    const updateEntries = () => {
      if (!alive || epoch !== auth.epoch) return;
      refreshEntries().catch((e) => {
        if (alive && epoch === auth.epoch) error = String(e);
      });
    };
    const stop = listenForListUpdates(
      updateEntries,
      (e) => { if (epoch === auth.epoch) error = String(e); },
      () => { if (epoch === auth.epoch) void load(); },
    );
    return () => {
      alive = false;
      entryLoadId++;
      historyRequest++;
      stop();
    };
  });

  $effect(() => {
    const epoch = auth.epoch;
    if (!auth.isLoggedIn) return;
    let alive = true;
    const refresh = () => {
      if (alive && epoch === auth.epoch && document.visibilityState === "visible" && !library.scanning) {
        void library.ensureScan().catch((e) => { if (alive && epoch === auth.epoch) error = String(e); });
      }
    };
    const timer = setInterval(refresh, 60_000);
    window.addEventListener("focus", refresh);
    let stop: (() => void) | undefined;
    listen("kurisu://watch-history-updated", () => {
      if (alive && epoch === auth.epoch && historyOpen) void loadHistory();
    }).then((unlisten) => alive ? stop = unlisten : unlisten()).catch(console.error);
    return () => { alive = false; clearInterval(timer); window.removeEventListener("focus", refresh); stop?.(); };
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <PageHeading section="Your collection" title="Library" description="Anime on your device">
      {#if scannedAt}
        <span class="text-xs text-accent">
          Scan finished · {scannedCount} file{scannedCount === 1 ? "" : "s"}
        </span>
      {/if}
      <button
        onclick={rescan}
        disabled={library.scanning || library.folders.length === 0}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {#if library.scanning}Scanning…{:else}↻ Rescan{/if}
      </button>
    </PageHeading>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if library.unreadable.length > 0}
      <div class="text-sm text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded-md p-2 mb-4">
        Some paths could not be read. Files may be missing from this scan.
        <ul class="mt-1 space-y-0.5">
          {#each library.unreadable as u}
            <li class="text-xs" title={u.error}>{u.path}: {u.error}</li>
          {/each}
        </ul>
      </div>
    {/if}

    <div class="mb-5">
      <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Folders</h2>
      {#if library.foldersFailed}
        <p class="text-sm text-amber-400 mb-2">
          Could not read your library folders. Check that the drive is mounted and try again.
        </p>
      {:else if library.folders.length === 0}
        <p class="text-sm text-ink-dim mb-2">
          No library folders yet — add the folders where your anime files live.
        </p>
      {:else}
        <div class="space-y-1 mb-2">
          {#each library.folders as folder (folder)}
            <div class="flex items-center gap-2 bg-panel border border-edge rounded-md px-3 py-1.5">
              <span class="text-sm min-w-0 flex-1 font-mono">{folder}</span>
              <button
                onclick={() => (removingFolder = folder)}
                disabled={removing.has(folder)}
                title="Remove this folder"
                class="text-ink-dim hover:text-red-400 px-1 grid place-items-center disabled:opacity-50"
              >
                <Icon name="x" size={14} />
              </button>
            </div>
          {/each}
        </div>
      {/if}
      <button
        onclick={pickFolder}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
      >
        + Add folder
      </button>
    </div>

    {#if library.folders.length > 0 && !library.hasScan && library.scanning}
      <div class="text-ink-dim py-10 text-center">Scanning…</div>
    {:else if library.hasScan && groups.matched.length === 0 && groups.unmatched.length === 0}
      <div class="text-ink-dim py-10 text-center">No video files found in those folders.</div>
    {:else}
      <div class="space-y-4">
        {#each groups.matched as g (g.mediaId)}
          {@const next = nextFile(g)}
          {@const cov = cover(g)}
          {@const isOpen = expanded.has(g.mediaId)}
          <section class="cv-group bg-panel border border-edge rounded-lg overflow-hidden">
            <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
            <div
              class="flex items-center gap-3 p-2.5 border-b border-edge cursor-pointer select-none hover:bg-panel-2/40 transition-colors"
              role="button"
              tabindex="0"
              aria-expanded={isOpen}
              onclick={() => toggle(g.mediaId)}
              onkeydown={(ev) => {
                if (ev.currentTarget !== ev.target) return;
                if (ev.key === "Enter" || ev.key === " ") {
                  ev.preventDefault();
                  toggle(g.mediaId);
                }
              }}
            >
              <span
                class="w-4 shrink-0 text-ink-dim grid place-items-center transition-transform duration-150 {isOpen ? 'rotate-90' : ''}"
                aria-hidden="true"
              >
                <Icon name="chevron" size={14} />
              </span>
              {#if cov}
                <button
                  type="button"
                  onclick={(ev) => {
                    ev.stopPropagation();
                    goto(`/anime/${g.mediaId}`);
                  }}
                  title="Open details"
                  class="shrink-0"
                >
                  <Img src={cov} class="w-10 h-14 object-cover rounded" />
                </button>
              {:else}
                <div class="w-10 h-14 bg-panel-2 rounded shrink-0"></div>
              {/if}
              <div class="flex-1 min-w-0">
                <button
                  type="button"
                  onclick={(ev) => {
                    ev.stopPropagation();
                    goto(`/anime/${g.mediaId}`);
                  }}
                  title="Open details"
                  class="block max-w-full font-medium text-left hover:text-accent transition-colors"
                >
                  {g.entry ? displayTitle(g.entry.media) : g.title}
                </button>
                <div class="text-xs text-ink-dim">
                  {#if g.entry}
                    Ep {g.entry.progress}{g.entry.media?.episodes ? `/${g.entry.media.episodes}` : ""} watched ·
                  {:else if entriesLoaded}
                    Not on your list ·
                  {/if}
                  {g.files.length} file{g.files.length === 1 ? "" : "s"}
                </div>
              </div>
              {#if hasBound(g)}
                <button
                  onclick={(ev) => {
                    ev.stopPropagation();
                    unlinking = g;
                  }}
                  title="Manually linked — click to remove the link"
                  class="text-ink-dim hover:text-red-400 px-1 grid place-items-center shrink-0"
                >
                  <Icon name="unlink" size={15} />
                </button>
              {/if}
              {#if next}
                <button
                  onclick={(ev) => {
                    ev.stopPropagation();
                    play(next.path);
                  }}
                  title={basename(next.path)}
                  class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm shrink-0 flex items-center gap-1.5"
                >
                  <Icon name="play" size={13} /> {g.entry?.media?.format === "MOVIE" ? "Play movie" : `Play Ep ${next.episode}`}
                </button>
              {/if}
            </div>
            {#if isOpen}
              <div class="divide-y divide-edge/60">
                {#each g.files as f (f.path)}
                  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
                  <div
                    class="cv-row flex items-center gap-2 px-3 py-1.5 text-sm cursor-pointer hover:bg-panel-2/40 transition-colors"
                    role="button"
                    tabindex="0"
                    title={`Play ${basename(f.path)}`}
                    onclick={() => play(f.path)}
                    onkeydown={(ev) => {
                      // Let nested buttons handle their own keyboard activation.
                      if (ev.currentTarget !== ev.target) return;
                      if (ev.key === "Enter" || ev.key === " ") {
                        ev.preventDefault();
                        play(f.path);
                      }
                    }}
                  >
                    <span class="w-14 shrink-0 text-ink-dim">
                      {f.episode != null ? `Ep ${f.episode}` : "—"}
                    </span>
                    <span class="flex-1 min-w-0 {isWatched(g, f) ? 'text-ink-dim' : ''}">
                      {basename(f.path)}
                    </span>
                    {#if isWatched(g, f)}
                      <span class="text-accent shrink-0 grid place-items-center" title="Watched (per your list progress)"><Icon name="check" size={14} /></span>
                    {/if}
                    <span class="text-ink-dim shrink-0 grid place-items-center" title="Play"><Icon name="play" size={13} /></span>
                    {#if g.entry}
                      <button onclick={(ev) => { ev.stopPropagation(); editNumbering(g, f); }}
                        title="Adjust episode numbering" class="text-ink-dim hover:text-accent px-1 grid place-items-center">
                        <Icon name="sliders" size={14} />
                      </button>
                    {/if}
                    <button
                      onclick={(ev) => {
                        ev.stopPropagation();
                        reveal(f.path);
                      }}
                      title="Show in file manager"
                      class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                    >
                      <Icon name="folder-open" size={14} />
                    </button>
                  </div>
                {/each}
              </div>
            {/if}
          </section>
        {/each}

        {#if groups.unmatched.length > 0}
          <section class="bg-panel border border-edge rounded-lg overflow-hidden">
            <div class="px-3 py-2 border-b border-edge text-sm font-medium text-ink-dim">
              Unmatched — {groups.unmatched.length} file{groups.unmatched.length === 1 ? "" : "s"}
            </div>
            <div class="divide-y divide-edge/60">
              {#each groups.unmatched as f (f.path)}
                <div class="cv-row flex items-center gap-2 px-3 py-1.5 text-sm">
                  <span class="flex-1 min-w-0 text-ink-dim">{basename(f.path)}</span>
                  <button
                    onclick={() => (linking = f.path)}
                    title="Link to a show on your list"
                    class="text-ink-dim hover:text-accent px-1 grid place-items-center"
                  >
                    <Icon name="link" size={14} />
                  </button>
                  <button
                    onclick={() => play(f.path)}
                    title="Play"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="play" size={13} />
                  </button>
                  <button
                    onclick={() => reveal(f.path)}
                    title="Show in file manager"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="folder-open" size={14} />
                  </button>
                </div>
              {/each}
            </div>
          </section>
        {/if}
      </div>
    {/if}
    <section class="mt-6 border-t border-edge pt-4">
      <div class="flex items-center gap-3">
        <button type="button" onclick={toggleHistory} aria-expanded={historyOpen} aria-controls="library-watch-history"
          class="text-sm font-semibold text-ink-dim hover:text-ink">{historyOpen ? "▾" : "▸"} Watch history</button>
        {#if historyOpen}<button onclick={() => loadHistory()} disabled={historyLoading} class="text-xs text-accent hover:underline disabled:opacity-50">Refresh history</button>{/if}
      </div>
      {#if historyOpen}
        <div id="library-watch-history" class="mt-3">
          <p class="text-xs text-ink-dim mb-3">Playback recorded on this device.</p>
          {#if historyError}<p role="alert" class="text-sm text-red-400 mb-2">{historyError}</p>{/if}
          {#if historyLoading && historyItems.length === 0}
            <p class="text-sm text-ink-dim">Loading watch history…</p>
          {:else if historyItems.length === 0 && !historyError}
            <p class="text-sm text-ink-dim">No watched episodes recorded yet.</p>
          {/if}
          <div class="divide-y divide-edge">
            {#each historyItems as watched (watched.id)}
              <div class="py-2 flex items-start gap-3 text-sm">
                <div class="flex-1 min-w-0">
                  <button onclick={() => goto(`/anime/${watched.media_id}`)} class="text-left hover:text-accent">{watched.title || `Show #${watched.media_id}`} · Episode {watched.episode}</button>
                  <p class="text-xs text-ink-dim mt-1" title={watched.path}>{basename(watched.path)}</p>
                </div>
                <time class="text-xs text-ink-dim shrink-0" datetime={new Date(watched.watched_at * 1000).toISOString()}>{new Date(watched.watched_at * 1000).toLocaleString()}</time>
              </div>
            {/each}
          </div>
          {#if historyMore}<button onclick={() => loadHistory(true)} disabled={historyLoading} class="text-sm text-accent hover:underline mt-3 disabled:opacity-50">{historyLoading ? "Loading…" : "Load older history"}</button>{/if}
        </div>
      {/if}
    </section>
  </div>
{/if}

{#if numbering}
  <Dialog onclose={() => { if (!offsetBusy) numbering = null; }} busy={offsetBusy} size="md">
    <h3 class="font-semibold">Episode numbering · {numbering.title}</h3>
    <p class="text-xs text-ink-dim mt-2 mb-3 break-all">{offsetTarget}</p>
    <div class="flex gap-4 text-sm mb-3">
      <label><input type="radio" bind:group={offsetScope} value="file" disabled={offsetBusy} /> This file</label>
      {#if offsetFolderAllowed}<label><input type="radio" bind:group={offsetScope} value="folder" disabled={offsetBusy} /> Whole folder</label>{/if}
    </div>
    <label for="library-episode-offset" class="block text-sm mb-1">Episode offset</label>
    <input id="library-episode-offset" type="number" min="-9999" max="9999" step="1" bind:value={episodeOffset}
      disabled={offsetLoading || offsetBusy || !offsetReady} class="w-28 bg-panel-2 border border-edge rounded px-2 py-1.5 text-sm" />
    <p class="text-xs text-ink-dim mt-2">Added to detected file episode numbers. Use -12 to map file 13 to episode 1. Choose Whole folder to adjust the season together. Episodes outside the show's total are left unnumbered.</p>
    {#if offsetLoading}<p class="text-xs text-ink-dim mt-2">Reading the current link…</p>{/if}
    {#if offsetConflict}<p role="alert" class="text-sm text-amber-400 mt-2">This location is linked to a different show. Select this file only or relink it first.</p>{/if}
    {#if offsetError}<p role="alert" class="text-sm text-red-400 mt-2">{offsetError} {#if !offsetReady}<button onclick={() => offsetAttempt++} class="underline">Retry</button>{/if}</p>{/if}
    <div class="flex justify-end gap-2 mt-4">
      <button onclick={() => numbering = null} disabled={offsetBusy} class="px-3 py-1.5 text-sm">Cancel</button>
      <button onclick={saveNumbering} disabled={offsetBusy || offsetLoading || !offsetReady || offsetConflict} class="px-3 py-1.5 text-sm bg-accent text-white rounded disabled:opacity-50">{offsetBusy ? "Saving…" : "Save numbering"}</button>
    </div>
  </Dialog>
{/if}

{#if unlinking}
  {@const g = unlinking}
  <Confirm
    title={`Remove manual links for ${g.entry ? displayTitle(g.entry.media) : g.title}?`}
    body="The recognizer will guess these files again on the next scan."
    confirmLabel="Remove links"
    busy={unlinkBusy}
    onconfirm={() => unlink(g)}
    oncancel={() => (unlinking = null)}
  />
{/if}

{#if removingFolder}
  {@const path = removingFolder}
  <Confirm
    title="Remove library folder?"
    body={`${path} will no longer be scanned. The files stay on disk.`}
    confirmLabel="Remove"
    busy={removing.has(path)}
    onconfirm={() => removeFolder(path)}
    oncancel={() => (removingFolder = null)}
  />
{/if}

{#if linking}
  <LinkAnime
    path={linking}
    {entries}
    roots={library.folders}
    onclose={() => (linking = null)}
    onlinked={() => library.scan().catch((e) => { error = String(e); })}
  />
{/if}
