<script lang="ts">
  import { open } from "@tauri-apps/plugin-dialog";
  import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import { displayTitle, type LibraryFile, type ListEntry } from "$lib/types";
  import Icon from "$lib/Icon.svelte";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

  let entries = $state<ListEntry[]>([]);
  let error = $state("");

  interface Group {
    mediaId: number;
    title: string;
    entry?: ListEntry;
    files: LibraryFile[];
  }

  const groups = $derived.by(() => {
    const byMedia = new Map<number, Group>();
    const unmatched: LibraryFile[] = [];
    for (const f of library.files) {
      if (f.media_id == null) {
        unmatched.push(f);
        continue;
      }
      let g = byMedia.get(f.media_id);
      if (!g) {
        const entry = entries.find((e) => e.media_id === f.media_id);
        g = { mediaId: f.media_id, title: f.matched ?? `#${f.media_id}`, entry, files: [] };
        byMedia.set(f.media_id, g);
      }
      g.files.push(f);
    }
    for (const g of byMedia.values()) {
      g.files.sort((a, b) => (a.episode ?? 9999) - (b.episode ?? 9999));
    }
    const matched = [...byMedia.values()].sort((a, b) =>
      a.title.localeCompare(b.title, undefined, { sensitivity: "base", numeric: true })
    );
    return { matched, unmatched };
  });

  async function load() {
    error = "";
    try {
      entries = await api.localEntries();
      await library.loadFolders();
      if (library.folders.length > 0 && !library.hasScan) await library.scan();
    } catch (e) {
      error = String(e);
    }
  }

  async function pickFolder() {
    const chosen = await open({ directory: true, multiple: false });
    if (typeof chosen !== "string") return;
    error = "";
    try {
      await library.addFolder(chosen);
      await library.scan();
    } catch (e) {
      error = String(e);
    }
  }

  async function removeFolder(path: string) {
    error = "";
    try {
      await library.removeFolder(path);
    } catch (e) {
      error = String(e);
    }
  }

  async function rescan() {
    error = "";
    try {
      await library.scan();
    } catch (e) {
      error = String(e);
    }
  }

  function basename(path: string): string {
    return path.split(/[\\/]/).pop() ?? path;
  }

  /// First file exactly at the next unwatched episode (progress + 1).
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

  $effect(() => {
    if (auth.isLoggedIn) load();
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-5xl mx-auto">
    <div class="flex items-center gap-3 mb-4">
      <h1 class="text-xl font-semibold flex-1">Library</h1>
      <button
        onclick={rescan}
        disabled={library.scanning || library.folders.length === 0}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {#if library.scanning}Scanning…{:else}↻ Rescan{/if}
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    <!-- Folders being scanned. -->
    <div class="mb-5">
      <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Folders</h2>
      {#if library.folders.length === 0}
        <p class="text-sm text-ink-dim mb-2">
          No library folders yet — add the folders where your anime files live.
        </p>
      {:else}
        <div class="space-y-1 mb-2">
          {#each library.folders as folder (folder)}
            <div class="flex items-center gap-2 bg-panel border border-edge rounded-md px-3 py-1.5">
              <span class="text-sm truncate flex-1 font-mono">{folder}</span>
              <button
                onclick={() => removeFolder(folder)}
                title="Remove this folder"
                class="text-ink-dim hover:text-red-400 px-1 grid place-items-center"
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
          <section class="cv-card bg-panel border border-edge rounded-lg overflow-hidden">
            <div class="flex items-center gap-3 p-2.5 border-b border-edge">
              {#if cov}
                <Img src={cov} class="w-10 h-14 object-cover rounded shrink-0" />
              {:else}
                <div class="w-10 h-14 bg-panel-2 rounded shrink-0"></div>
              {/if}
              <div class="flex-1 min-w-0">
                <div class="truncate font-medium">{g.entry ? displayTitle(g.entry.media) : g.title}</div>
                <div class="text-xs text-ink-dim">
                  {#if g.entry}
                    Ep {g.entry.progress}{g.entry.media?.episodes ? `/${g.entry.media.episodes}` : ""} watched
                  {:else}
                    On your list
                  {/if}
                  · {g.files.length} file{g.files.length === 1 ? "" : "s"}
                </div>
              </div>
              {#if next}
                <button
                  onclick={() => openPath(next.path)}
                  title={basename(next.path)}
                  class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm shrink-0 flex items-center gap-1.5"
                >
                  <Icon name="play" size={13} /> Play Ep {next.episode}
                </button>
              {/if}
            </div>
            <div class="divide-y divide-edge/60">
              {#each g.files as f (f.path)}
                <div class="cv-row flex items-center gap-2 px-3 py-1.5 text-sm">
                  <span class="w-14 shrink-0 text-ink-dim">
                    {f.episode != null ? `Ep ${f.episode}` : "—"}
                  </span>
                  <span class="flex-1 min-w-0 truncate {isWatched(g, f) ? 'text-ink-dim' : ''}">
                    {basename(f.path)}
                  </span>
                  {#if isWatched(g, f)}
                    <span class="text-accent shrink-0 grid place-items-center" title="Watched (per your list progress)"><Icon name="check" size={14} /></span>
                  {/if}
                  <button
                    onclick={() => openPath(f.path)}
                    title="Play"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="play" size={13} />
                  </button>
                  <button
                    onclick={() => revealItemInDir(f.path)}
                    title="Show in file manager"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="folder-open" size={14} />
                  </button>
                </div>
              {/each}
            </div>
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
                  <span class="flex-1 min-w-0 truncate text-ink-dim">{basename(f.path)}</span>
                  <button
                    onclick={() => openPath(f.path)}
                    title="Play"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="play" size={13} />
                  </button>
                  <button
                    onclick={() => revealItemInDir(f.path)}
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
  </div>
{/if}
