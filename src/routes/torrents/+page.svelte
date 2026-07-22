<script lang="ts">
  import { goto } from "$app/navigation";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { timeAgo, type ListEntry, type TorrentItem } from "$lib/types";
  import Icon from "$lib/Icon.svelte";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

  const NEW_KEY = "kurisu.torrents.new";
  function readNewOnly(): boolean {
    try {
      return localStorage.getItem(NEW_KEY) === "1";
    } catch {
      return false;
    }
  }

  let items = $state<TorrentItem[]>([]);
  let entries = $state<ListEntry[]>([]);
  let feeds = $state<string[]>([]);
  let feedInput = $state("");
  let q = $state("");
  let newOnly = $state(readNewOnly());
  let loading = $state(false);
  let marking = $state(false);
  let error = $state("");
  let loaded = $state(false);

  function setNewOnly(v: boolean) {
    newOnly = v;
    try {
      localStorage.setItem(NEW_KEY, v ? "1" : "0");
    } catch {
      // storage unavailable — the toggle just won't persist
    }
  }

  // Overlapping refreshes (login flip + manual) resolve latest-wins.
  let loadId = 0;
  async function load() {
    const id = ++loadId;
    loading = true;
    error = "";
    try {
      const [torrents, myEntries, myFeeds] = await Promise.all([
        api.fetchTorrents(),
        api.localEntries(),
        api.getRssFeeds(),
      ]);
      if (id !== loadId) return;
      items = torrents;
      entries = myEntries;
      feeds = myFeeds;
      loaded = true;
    } catch (e) {
      if (id === loadId) error = String(e);
    } finally {
      if (id === loadId) loading = false;
    }
  }

  async function addFeed() {
    const url = feedInput.trim();
    if (!url) return;
    error = "";
    try {
      feeds = await api.addRssFeed(url);
      feedInput = "";
      await load();
    } catch (e) {
      error = String(e);
    }
  }

  async function removeFeed(url: string) {
    error = "";
    try {
      feeds = await api.removeRssFeed(url);
      await load();
    } catch (e) {
      error = String(e);
    }
  }

  function markLocal(guids: Set<string>) {
    items = items.map((t) => (guids.has(t.guid) ? { ...t, seen: true, is_new: false } : t));
  }

  async function openItem(t: TorrentItem, url: string) {
    try {
      await openUrl(url);
    } catch (e) {
      error = String(e);
      return;
    }
    // Opening a torrent counts as acting on it — clear its NEW state.
    try {
      await api.markTorrentsSeen([t.guid]);
      markLocal(new Set([t.guid]));
    } catch {
      // seen-state is best-effort
    }
  }

  async function markAllSeen() {
    const guids = items.filter((t) => t.media_id != null && !t.seen).map((t) => t.guid);
    if (guids.length === 0) return;
    marking = true;
    error = "";
    try {
      await api.markTorrentsSeen(guids);
      markLocal(new Set(guids));
    } catch (e) {
      error = String(e);
    } finally {
      marking = false;
    }
  }

  interface Group {
    mediaId: number;
    title: string;
    cover: string | null;
    newest: number;
    hasNew: boolean;
    items: TorrentItem[];
  }

  const newCount = $derived(items.filter((t) => t.is_new).length);
  const unmatchedCount = $derived(items.filter((t) => t.media_id == null).length);

  const groups = $derived.by(() => {
    const needle = q.trim().toLowerCase();
    const byMedia = new Map<number, Group>();
    for (const t of items) {
      if (t.media_id == null) continue;
      if (newOnly && !t.is_new) continue;
      if (needle && !t.title.toLowerCase().includes(needle) && !t.matched?.toLowerCase().includes(needle)) continue;
      let g = byMedia.get(t.media_id);
      if (!g) {
        const entry = entries.find((e) => e.media_id === t.media_id);
        g = {
          mediaId: t.media_id,
          title: t.matched ?? `#${t.media_id}`,
          cover: entry?.media?.cover_medium ?? null,
          newest: 0,
          hasNew: false,
          items: [],
        };
        byMedia.set(t.media_id, g);
      }
      g.items.push(t);
      g.newest = Math.max(g.newest, t.published ?? 0);
      g.hasNew = g.hasNew || t.is_new;
    }
    // Groups with something NEW float to the top, then most recently active.
    return [...byMedia.values()].sort((a, b) => {
      if (a.hasNew !== b.hasNew) return a.hasNew ? -1 : 1;
      return b.newest - a.newest;
    });
  });

  $effect(() => {
    if (auth.isLoggedIn) load();
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-4xl mx-auto">
    <div class="flex items-center gap-2 mb-4 flex-wrap">
      <h1 class="text-xl font-semibold">Torrents</h1>
      {#if newCount > 0}
        <span class="text-xs px-2 py-0.5 rounded-full bg-accent/15 text-accent">{newCount} new</span>
      {/if}
      <div class="flex-1"></div>
      <input
        bind:value={q}
        placeholder="Filter…"
        class="w-40 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
      />
      <div class="flex rounded-md border border-edge overflow-hidden text-sm">
        <button
          onclick={() => setNewOnly(true)}
          class="px-3 py-1.5 {newOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
        >
          New
        </button>
        <button
          onclick={() => setNewOnly(false)}
          class="px-3 py-1.5 {!newOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
        >
          All
        </button>
      </div>
      <button
        onclick={markAllSeen}
        disabled={marking || newCount === 0}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
        title="Mark every listed item as seen"
      >
        {marking ? "Marking…" : "Mark all seen"}
      </button>
      <button
        onclick={load}
        disabled={loading}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50 flex items-center gap-1.5"
      >
        {#if loading}Refreshing…{:else}<Icon name="refresh" size={14} /> Refresh{/if}
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    <!-- Feeds being watched. -->
    <div class="mb-5">
      <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Feeds</h2>
      {#if feeds.length === 0 && loaded}
        <p class="text-sm text-ink-dim mb-2">No feeds configured — add an RSS feed URL (nyaa-style feeds work best).</p>
      {:else}
        <div class="space-y-1 mb-2">
          {#each feeds as feed (feed)}
            <div class="flex items-center gap-2 bg-panel border border-edge rounded-md px-3 py-1.5">
              <span class="text-sm truncate flex-1 font-mono">{feed}</span>
              <button
                onclick={() => removeFeed(feed)}
                title="Remove this feed"
                class="text-ink-dim hover:text-red-400 px-1 grid place-items-center"
              >
                <Icon name="x" size={14} />
              </button>
            </div>
          {/each}
        </div>
      {/if}
      <form
        onsubmit={(e) => {
          e.preventDefault();
          addFeed();
        }}
        class="flex gap-2"
      >
        <input
          bind:value={feedInput}
          placeholder="https://nyaa.si/?page=rss&q=…"
          class="flex-1 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
        />
        <button class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm" type="submit">
          + Add feed
        </button>
      </form>
    </div>

    {#if loading && !loaded}
      <div class="text-ink-dim py-10 text-center">Checking feeds…</div>
    {:else if groups.length === 0 && loaded}
      <div class="text-ink-dim py-10 text-center">
        {#if newOnly && items.some((t) => t.media_id != null)}
          Nothing new for your list.
        {:else}
          Nothing in the feeds matches your list.
        {/if}
        {#if items.length > 0}
          <div class="text-xs mt-1 opacity-70">{items.length} feed items checked.</div>
        {/if}
      </div>
    {:else}
      <div class="space-y-4">
        {#each groups as g (g.mediaId)}
          <section class="cv-card bg-panel border border-edge rounded-lg overflow-hidden">
            <div class="flex items-center gap-3 p-2.5 border-b border-edge">
              {#if g.cover}
                <button type="button" onclick={() => goto(`/anime/${g.mediaId}`)} title="Open details" class="shrink-0">
                  <Img src={g.cover} class="w-10 h-14 object-cover rounded" />
                </button>
              {:else}
                <div class="w-10 h-14 bg-panel-2 rounded shrink-0"></div>
              {/if}
              <div class="flex-1 min-w-0">
                <button
                  type="button"
                  onclick={() => goto(`/anime/${g.mediaId}`)}
                  title="Open details"
                  class="block max-w-full truncate font-medium text-left hover:text-accent transition-colors"
                >
                  {g.title}
                </button>
                <div class="text-xs text-ink-dim">
                  {g.items.length} release{g.items.length === 1 ? "" : "s"}
                </div>
              </div>
            </div>
            <div class="divide-y divide-edge/60">
              {#each g.items as t (t.guid)}
                <div class="cv-row flex items-center gap-2 px-3 py-1.5 text-sm {t.seen ? 'opacity-60' : ''}">
                  {#if t.is_new}
                    <span class="shrink-0 text-[10px] font-semibold uppercase px-1.5 py-0.5 rounded bg-accent/15 text-accent">New</span>
                  {/if}
                  <span class="w-12 shrink-0 text-ink-dim tabular-nums">
                    {t.episode != null ? `Ep ${t.episode}` : "—"}
                  </span>
                  <span class="flex-1 min-w-0 truncate" title={t.title}>{t.title}</span>
                  {#if t.size}
                    <span class="shrink-0 text-xs text-ink-dim">{t.size}</span>
                  {/if}
                  {#if t.seeders != null}
                    <span class="shrink-0 text-xs text-accent/80 tabular-nums" title="Seeders">↑{t.seeders}</span>
                  {/if}
                  {#if t.published}
                    <span class="shrink-0 text-xs text-ink-dim/70 w-10 text-right">{timeAgo(t.published)}</span>
                  {/if}
                  {#if t.magnet}
                    <button
                      onclick={() => openItem(t, t.magnet ?? t.link)}
                      title="Open magnet link"
                      class="text-ink-dim hover:text-accent px-1 grid place-items-center"
                    >
                      <Icon name="magnet" size={14} />
                    </button>
                  {/if}
                  <button
                    onclick={() => openItem(t, t.link)}
                    title="Download .torrent"
                    class="text-ink-dim hover:text-ink px-1 grid place-items-center"
                  >
                    <Icon name="download" size={14} />
                  </button>
                </div>
              {/each}
            </div>
          </section>
        {/each}
        {#if unmatchedCount > 0}
          <div class="text-xs text-ink-dim/70 text-center pb-2">
            {unmatchedCount} feed item{unmatchedCount === 1 ? "" : "s"} didn't match your list and {unmatchedCount === 1 ? "is" : "are"} hidden.
          </div>
        {/if}
      </div>
    {/if}
  </div>
{/if}
