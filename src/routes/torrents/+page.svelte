<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { afterNavigate, goto } from "$app/navigation";
  import { onDestroy } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { preferences } from "$lib/preferences";
  import { auth } from "$lib/auth.svelte";
  import { displayTitle, STATUS_LABEL, type FeedFailure, type ListEntry, type ListStatus, type ShowTorrents, type TorrentItem } from "$lib/types";
  import Confirm from "$lib/Confirm.svelte";
  import Icon from "$lib/Icon.svelte";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";
  import Select from "$lib/Select.svelte";
  import TorrentRow from "$lib/TorrentRow.svelte";
  import ShowTorrentChoices from "$lib/ShowTorrentChoices.svelte";
  import type { Snapshot } from "./$types";

  const NEW_KEY = "kurisu.torrents.new";
  const STATUSES_KEY = "kurisu.torrents.statuses";
  const FILTERS_KEY = "kurisu.torrents.filters";
  const CATEGORY_OPTIONS = [
    { value: "1_0", label: "All languages" },
    { value: "1_2", label: "English-translated" },
    { value: "1_3", label: "Non-English-translated" },
    { value: "1_4", label: "Raw" },
    { value: "1_1", label: "Anime music videos" },
  ];
  const RELEASE_OPTIONS = [
    { value: "0", label: "All releases" },
    { value: "1", label: "No remakes" },
    { value: "2", label: "Trusted only" },
  ];
  function readFilters(): { category: string; release: string } {
    const saved = preferences.getJson(FILTERS_KEY);
    const values = saved && typeof saved === "object" ? saved : {};
    return {
      category: CATEGORY_OPTIONS.find((o) => "category" in values && o.value === values.category)?.value ?? "1_0",
      release: RELEASE_OPTIONS.find((o) => "release" in values && o.value === values.release)?.value ?? "0",
    };
  }
  const savedFilters = readFilters();
  let categoryFilter = $state(savedFilters.category);
  let releaseFilter = $state(savedFilters.release);

  function matchesFilters(t: TorrentItem): boolean {
    return (categoryFilter === "1_0" || t.category_id === categoryFilter)
      && (releaseFilter === "0" || (releaseFilter === "1" ? t.remake === false : t.trusted === true));
  }

  function filtersChanged() {
    preferences.setJson(FILTERS_KEY, { category: categoryFilter, release: releaseFilter });
    if (submittedQuery) void search(submittedQuery);
  }
  const STATUS_ORDER: ListStatus[] = ["CURRENT", "PLANNING", "REPEATING", "PAUSED", "COMPLETED", "DROPPED"];
  function readStatuses(): Set<ListStatus> {
    const saved = preferences.getJson(STATUSES_KEY);
    return new Set(Array.isArray(saved) ? STATUS_ORDER.filter((status) => saved.includes(status)) : ["CURRENT"]);
  }

  let items = $state<TorrentItem[]>([]);
  let feedFailures = $state<FeedFailure[]>([]);
  let entries = $state<ListEntry[]>([]);
  let entriesLoading = $state(false);
  let entriesLoaded = $state(false);
  let entriesError = $state("");
  let feeds = $state<string[]>([]);
  let feedsLoaded = $state(false);
  let feedsError = $state("");
  let feedInput = $state("");
  let showFeeds = $state(false);
  let showSearchTips = $state(false);
  let showFilters = $state<Partial<Record<ListStatus, string>>>({});
  let newOnly = $state(preferences.get(NEW_KEY) === "1");
  let expandedStatuses = $state(readStatuses());
  let expanded = $state<Set<number>>(new Set());
  let loading = $state(false);
  let refreshedAt = $state(0);
  const showTorrentCache = new Map<string, { savedAt: number; choices: ShowTorrents }>();
  let openedLinks = $state<Set<string>>(new Set());
  let openedIdentities = $state<Set<string>>(new Set());
  const seenLinks = $derived(new Set([...openedLinks, ...items.filter((item) => item.seen).map((item) => item.link)]));
  const seenIdentities = $derived(new Set([...openedIdentities, ...items.filter((item) => item.seen).flatMap(seenGuids)]));

  function seenGuids(item: TorrentItem): string[] {
    return item.seen_guid ? [item.guid, item.seen_guid] : [item.guid];
  }

  function toggle(mediaId: number) {
    const next = new Set(expanded);
    if (next.has(mediaId)) next.delete(mediaId);
    else next.add(mediaId);
    expanded = next;
  }
  let error = $state("");
  let loaded = $state(false);

  function setNewOnly(v: boolean) {
    newOnly = v;
    preferences.set(NEW_KEY, v ? "1" : "0");
  }

  function setStatuses(statuses: Set<ListStatus>) {
    expandedStatuses = statuses;
    preferences.setJson(STATUSES_KEY, [...statuses]);
  }

  function toggleStatus(status: ListStatus) {
    const next = new Set(expandedStatuses);
    if (next.has(status)) next.delete(status);
    else next.add(status);
    setStatuses(next);
  }

  let loadId = 0;
  let feedsLoadId = 0;
  let entriesLoadId = 0;
  async function refreshEntries() {
    const id = ++entriesLoadId;
    const epoch = auth.epoch;
    entriesLoading = true;
    entriesError = "";
    try {
      const fresh = await api.localEntries();
      if (id !== entriesLoadId || epoch !== auth.epoch || !auth.isLoggedIn) return;
      entries = fresh;
      entriesLoaded = true;
    } catch (e) {
      if (id === entriesLoadId && epoch === auth.epoch && auth.isLoggedIn) entriesError = String(e);
    } finally {
      if (id === entriesLoadId && epoch === auth.epoch) entriesLoading = false;
    }
  }

  async function loadFeeds() {
    const id = ++feedsLoadId;
    const epoch = auth.epoch;
    feedsError = "";
    try {
      const saved = await api.getRssFeeds();
      if (id !== feedsLoadId || epoch !== auth.epoch || !auth.isLoggedIn) return;
      feeds = saved;
      feedsLoaded = true;
    } catch (e) {
      if (id === feedsLoadId && epoch === auth.epoch && auth.isLoggedIn) feedsError = String(e);
    }
  }

  async function load(flash = false) {
    showTorrentCache.clear();
    const id = ++loadId;
    const epoch = auth.epoch;
    const configuration = loadFeeds();
    entriesLoaded = false;
    entries = [];
    void refreshEntries();
    loading = true;
    error = "";
    try {
      const torrents = await api.fetchTorrents();
      if (id !== loadId || epoch !== auth.epoch || !auth.isLoggedIn) return;
      const opened = torrents.items.filter((item) => openedLinks.has(item.link) || seenGuids(item).some((guid) => openedIdentities.has(guid)));
      const guids = new Set(opened.flatMap(seenGuids));
      items = torrents.items.map((item) => guids.has(item.guid) ? { ...item, seen: true, is_new: false } : item);
      if (guids.size) void api.markTorrentsSeen([...guids]).catch(() => {});
      feedFailures = torrents.failures;
      loaded = true;
      if (flash) flashRefreshed();
    } catch (e) {
      if (id === loadId && epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      await configuration;
      if (id === loadId && epoch === auth.epoch) loading = false;
    }
  }

  function flashRefreshed() {
    refreshedAt = Date.now();
    const stamp = refreshedAt;
    setTimeout(() => {
      if (refreshedAt === stamp) refreshedAt = 0;
    }, 3000);
  }

  let addingFeed = $state(false);
  async function addFeed() {
    const url = feedInput.trim();
    if (!url || addingFeed || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    addingFeed = true;
    feedsLoadId++;
    error = "";
    try {
      const saved = await api.addRssFeed(url);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      feeds = saved;
      feedsLoaded = true;
      feedInput = "";
      await load();
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (epoch === auth.epoch) addingFeed = false;
    }
  }

  let removingFeed = $state<string | null>(null);
  let removing = $state(false);

  async function removeFeed() {
    const url = removingFeed;
    if (!url || removing || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    removing = true;
    feedsLoadId++;
    error = "";
    try {
      const saved = await api.removeRssFeed(url);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return;
      feeds = saved;
      feedsLoaded = true;
      await load();
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (epoch === auth.epoch) {
        removing = false;
        removingFeed = null;
      }
    }
  }

  function markLocal(guids: Set<string>) {
    items = items.map((t) => (guids.has(t.guid) ? { ...t, seen: true, is_new: false } : t));
  }

  async function openLink(url: string): Promise<boolean> {
    const epoch = auth.epoch;
    const current = () => alive && epoch === auth.epoch && auth.isLoggedIn;
    // Feed links must not invoke arbitrary OS protocols.
    const scheme = url.split(":")[0]?.toLowerCase().trim();
    if (scheme !== "http" && scheme !== "https" && scheme !== "magnet") {
      error = `Refused to open link with scheme "${scheme}:"`;
      return false;
    }
    try {
      await openUrl(url);
      return current();
    } catch (e) {
      if (current()) error = `Could not open ${url.startsWith("magnet:") ? "magnet link" : "link"}: ${String(e)}`;
      return false;
    }
  }

  async function openItem(t: TorrentItem, url: string): Promise<boolean> {
    const epoch = auth.epoch;
    if (!(await openLink(url))) return false;
    if (epoch !== auth.epoch || !auth.isLoggedIn) return false;
    if (t.link) openedLinks = new Set([...openedLinks, t.link]);
    const identities = new Set(seenGuids(t));
    const guids = new Set([...identities, ...items
      .filter((item) => (t.link && item.link === t.link) || seenGuids(item).some((guid) => identities.has(guid)))
      .flatMap(seenGuids)]);
    openedIdentities = new Set([...openedIdentities, ...guids]);
    markLocal(guids);
    try {
      await api.markTorrentsSeen([...guids]);
      if (epoch !== auth.epoch || !auth.isLoggedIn) return false;
    } catch {
    }
    return epoch === auth.epoch && auth.isLoggedIn;
  }

  let searchQ = $state("");
  let searching = $state(false);
  let searched = $state(false);
  let results = $state<TorrentItem[]>([]);
  let submittedQuery = $state("");
  const seenResults = $derived(results.map((item) => seenLinks.has(item.link) || seenGuids(item).some((guid) => seenIdentities.has(guid)) ? { ...item, seen: true } : item));
  const filteredResults = $derived(seenResults.filter(matchesFilters));

  let searchId = 0;
  let alive = true;
  let resumeQuery: string | null = null;
  async function search(query = searchQ.trim()) {
    if (!query || !auth.isLoggedIn) return;
    const id = ++searchId;
    const epoch = auth.epoch;
    const current = () => alive && id === searchId && epoch === auth.epoch && auth.isLoggedIn;
    resumeQuery = null;
    submittedQuery = query;
    searching = true;
    searched = false;
    results = [];
    error = "";
    try {
      const found = await api.searchTorrents(query, categoryFilter, releaseFilter);
      if (!current()) return;
      results = found;
      searched = true;
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (current()) searching = false;
    }
  }

  export const snapshot: Snapshot<{
    searchQ: string;
    submittedQuery: string;
    results: TorrentItem[];
    searched: boolean;
    categoryFilter: string;
    releaseFilter: string;
    pendingQuery: string | null;
    epoch: number;
    showFilters: Partial<Record<ListStatus, string>>;
    expanded: number[];
  }> = {
    capture: () => ({ searchQ, submittedQuery, results: seenResults, searched, categoryFilter, releaseFilter, pendingQuery: searching ? submittedQuery : resumeQuery, epoch: auth.epoch,
      showFilters: { ...showFilters }, expanded: [...expanded] }),
    restore: (v) => {
      searchId++;
      searching = false;
      searchQ = v.searchQ;
      submittedQuery = v.submittedQuery;
      resumeQuery = v.epoch === auth.epoch ? v.pendingQuery : (v.submittedQuery || null);
      results = resumeQuery ? [] : v.results;
      searched = !resumeQuery && v.searched;
      categoryFilter = v.categoryFilter;
      releaseFilter = v.releaseFilter;
      showFilters = v.epoch === auth.epoch ? { ...v.showFilters } : {};
      expanded = new Set(v.epoch === auth.epoch ? v.expanded : []);
    },
  };

  afterNavigate(() => {
    queueMicrotask(() => {
      if (alive && auth.isLoggedIn && resumeQuery) void search(resumeQuery);
    });
  });

  onDestroy(() => { alive = false; searchId++; });

  interface Group {
    entry: ListEntry;
    mediaId: number;
    status: string;
    title: string;
    cover: string | null;
    newest: number;
    hasNew: boolean;
    items: TorrentItem[];
  }

  const entriesByMedia = $derived(new Map(entries.map((e) => [e.media_id, e])));
  const unmatchedCount = $derived(items.filter((t) => t.media_id == null || !entriesByMedia.has(t.media_id)).length);

  const groups = $derived.by(() => {
    const byMedia = new Map<number, Group>();
    function groupFor(entry: ListEntry): Group {
      let group = byMedia.get(entry.media_id);
      if (!group) {
        group = {
          entry,
          mediaId: entry.media_id,
          status: entry.status,
          title: displayTitle(entry.media) || `#${entry.media_id}`,
          cover: entry.media?.cover_medium ?? null,
          newest: 0,
          hasNew: false,
          items: [],
        };
        byMedia.set(entry.media_id, group);
      }
      return group;
    }
    for (const entry of entries) groupFor(entry);
    for (const t of items) {
      if (!matchesFilters(t)) continue;
      if (t.media_id == null) continue;
      const entry = entriesByMedia.get(t.media_id);
      if (!entry) continue;
      const total = entry.media?.episodes;
      const isNew = !t.seen && t.episode != null && t.episode > entry.progress
        && (total == null || t.episode <= total);
      if (newOnly && !isNew) continue;
      const g = groupFor(entry);
      g.items.push({ ...t, is_new: isNew });
      g.newest = Math.max(g.newest, t.published ?? 0);
      g.hasNew = g.hasNew || isNew;
    }
    return [...byMedia.values()].sort((a, b) => {
      if (a.hasNew !== b.hasNew) return a.hasNew ? -1 : 1;
      return b.newest - a.newest || a.title.localeCompare(b.title);
    });
  });

  const categories = $derived(STATUS_ORDER.map((status) => {
    const allShows = groups.filter((g) => g.status === status);
    const needle = (showFilters[status] ?? "").trim().toLowerCase();
    const shows = allShows.filter((g) => !needle || [
      g.title, g.entry.media?.title_romaji, g.entry.media?.title_native,
      ...g.items.map((item) => item.title),
    ].some((title) => title?.toLowerCase().includes(needle)));
    return {
      status,
      label: STATUS_LABEL[status],
      shows,
      total: allShows.length,
      releases: allShows.reduce((total, g) => total + g.items.length, 0),
      newCount: allShows.reduce((total, g) => total + g.items.filter((t) => t.is_new).length, 0),
    };
  }));
  const newCount = $derived(categories.reduce((total, category) =>
    total + (expandedStatuses.has(category.status) ? category.newCount : 0), 0
  ));

  $effect(() => {
    const epoch = auth.epoch;
    const loggedIn = auth.isLoggedIn;
    loadId++;
    feedsLoadId++;
    entriesLoadId++;
    searchId++;
    items = [];
    entries = [];
    entriesLoaded = false;
    entriesLoading = false;
    entriesError = "";
    expanded = new Set();
    showFilters = {};
    showTorrentCache.clear();
    openedLinks = new Set();
    openedIdentities = new Set();
    feeds = [];
    feedsLoaded = false;
    loaded = false;
    loading = false;
    results = [];
    searched = false;
    searching = false;
    submittedQuery = "";
    resumeQuery = null;
    error = "";
    feedFailures = [];
    feedsError = "";
    removingFeed = null;
    removing = false;
    addingFeed = false;
    if (!loggedIn) return;
    let alive = true;
    const stops: (() => void)[] = [];
    const registrations = ["kurisu://episode-updated", "kurisu://pending-changed"].map((event) =>
      listen(event, () => {
        if (alive && epoch === auth.epoch) void refreshEntries();
      }).then((stop) => {
        if (alive && epoch === auth.epoch) stops.push(stop);
        else stop();
      })
    );
    void Promise.allSettled(registrations).then((results) => {
      const failures = results.flatMap((result) => result.status === "rejected" ? [String(result.reason)] : []);
      if (!alive || epoch !== auth.epoch) return;
      void load();
      if (failures.length) error = `Could not listen for list updates: ${failures.join("; ")}`;
    });
    return () => {
      alive = false;
      for (const stop of stops) stop();
      loadId++;
      feedsLoadId++;
      entriesLoadId++;
    };
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <PageHeading section="Releases" title="Torrents" description="Downloads for your list and new releases" />
    <div class="sticky top-0 z-10 mb-4 bg-base pt-4 pb-4 border-b border-edge">
      <div class="flex items-center gap-2 mb-4 flex-wrap">
        {#if newCount > 0}
          <span class="text-xs px-2 py-0.5 rounded-full bg-accent/15 text-accent">{newCount} new</span>
        {/if}
        <div class="flex-1"></div>
        <label class="flex items-center gap-1.5 text-sm text-ink-dim">
          <input
            type="checkbox"
            checked={newOnly}
            onchange={(e) => setNewOnly(e.currentTarget.checked)}
            class="accent-accent"
          />
          New feed releases only
        </label>
        <button
          onclick={() => load(true)}
          disabled={loading}
          class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50 flex items-center gap-1.5"
        >
          {#if loading}Refreshing…{:else}<Icon name="refresh" size={14} /> Refresh{/if}
        </button>
        {#if refreshedAt}
          <span class="text-xs text-accent">Refreshed ✓</span>
        {/if}
      </div>
      <div class="grid grid-cols-[repeat(auto-fit,minmax(250px,1fr))] gap-4">
        <div class="min-w-0">
          <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Search</h2>
          <form
            onsubmit={(e) => {
              e.preventDefault();
              search();
            }}
            class="flex gap-2"
          >
            <input
              bind:value={searchQ}
              aria-label="Search Nyaa"
              maxlength={200}
              placeholder="Search nyaa for any torrent…"
              class="min-w-0 flex-1 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
            />
            <button
              class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
              type="submit"
              disabled={searching}
            >
              {searching ? "Searching…" : "Search"}
            </button>
          </form>
        </div>
        <div class="min-w-0">
          <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">
            <button
              type="button"
              onclick={() => (showFeeds = !showFeeds)}
              aria-expanded={showFeeds}
              aria-controls="saved-torrent-feeds"
              title="Manage saved RSS feeds"
              class="inline-flex items-center gap-1 hover:text-ink uppercase"
            >
              <span class={showFeeds ? "rotate-90" : ""} aria-hidden="true"><Icon name="chevron" size={12} /></span>
              Feeds
            </button>
          </h2>
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
              class="min-w-0 flex-1 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
            />
            <button
              class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
              type="submit"
              disabled={addingFeed}
            >
              {addingFeed ? "Adding…" : "+ Add feed"}
            </button>
          </form>
          {#if feedsError}
            <p class="text-sm text-red-400 mt-2">Could not read saved feeds: {feedsError}</p>
          {/if}
          {#if feeds.length === 0 && feedsLoaded && !feedsError}
            <p class="text-sm text-ink-dim mt-2">No feeds configured. Add an RSS feed URL above.</p>
          {/if}
          {#if showFeeds}
            <section id="saved-torrent-feeds" class="mt-2 max-h-32 overflow-auto space-y-1" aria-label="Saved feeds">
              {#each feeds as feed (feed)}
                <div class="flex items-center gap-2 bg-panel border border-edge rounded-md px-3 py-1.5">
                  <span class="text-sm truncate flex-1 font-mono" title={feed}>{feed}</span>
                  <button
                    onclick={() => (removingFeed = feed)}
                    title="Remove this feed"
                    class="text-ink-dim hover:text-red-400 px-1 grid place-items-center"
                  >
                    <Icon name="x" size={14} />
                  </button>
                </div>
              {/each}
            </section>
          {/if}
        </div>
      </div>
      <div class="flex flex-wrap gap-3 mt-3">
        <div class="flex-1 min-w-[210px] max-w-xs">
          <label for="torrent-category" class="block text-xs text-ink-dim mb-1">Language / category</label>
          <Select id="torrent-category" bind:value={categoryFilter} options={CATEGORY_OPTIONS} onchange={filtersChanged} />
        </div>
        <div class="flex-1 min-w-[150px] max-w-xs">
          <label for="torrent-release" class="block text-xs text-ink-dim mb-1">Release filter</label>
          <Select id="torrent-release" bind:value={releaseFilter} options={RELEASE_OPTIONS} onchange={filtersChanged} />
        </div>
      </div>
    </div>

    <div class="mb-4 text-xs text-ink-dim">
      <button
        type="button"
        onclick={() => (showSearchTips = !showSearchTips)}
        aria-expanded={showSearchTips}
        aria-controls="torrent-search-tips"
        class="inline-flex items-center gap-1 hover:text-ink"
      >
        <span class={showSearchTips ? "rotate-90" : ""} aria-hidden="true"><Icon name="chevron" size={12} /></span>
        Search tips and torrent labels
      </button>
      {#if showSearchTips}
        <div id="torrent-search-tips" class="mt-2 space-y-2 max-w-3xl leading-relaxed">
          <p>Open a show to find batches and your next unwatched episode, including shows that have finished airing. Each release lists its group in the title so you can choose your preferred version.</p>
          <p>Language and release filters apply to show downloads, Nyaa searches and your loaded feeds. Changing a filter repeats open searches. New feed releases only hides seen and watched episodes from the feed results. Your full list and download options stay available.</p>
          <p>Nyaa’s categories describe the release. Check its description for individual subtitle and audio tracks. Feeds without category information show “Category unknown”.</p>
          <p><span class="text-accent">Green / Trusted</span> marks trusted uploaders. <span class="text-red-400">Red / Remake</span> marks re-encodes or altered reuploads.</p>
          <p>In the Nyaa search bar, use <code>"exact phrase"</code> for a phrase, <code>-word</code> to exclude a word, and <code>word|other</code> to match either word.</p>
          <button type="button" onclick={() => openLink("https://nyaa.si/help")} class="text-accent hover:underline">Open Nyaa’s search guide</button>
        </div>
      {/if}
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if feedFailures.length > 0}
      <div class="text-sm text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded-md p-2 mb-4">
        {feedFailures.length === 1 ? "1 feed failed" : `${feedFailures.length} feeds failed`}:
        <ul class="mt-1 space-y-0.5">
          {#each feedFailures as f (f.url)}
            <li class="truncate text-xs" title={f.error}>{f.error}</li>
          {/each}
        </ul>
      </div>
    {/if}

    {#if entriesError}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4 flex items-center gap-3">
        <span class="flex-1">Could not read your list: {entriesError}</span>
        <button onclick={() => refreshEntries()} disabled={entriesLoading} class="shrink-0 underline disabled:opacity-50">Retry list</button>
      </div>
    {/if}

    {#if searched || searching}
      <div class="mb-5" aria-live="polite" aria-busy={searching}>
        <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Search results</h2>
        <p class="text-xs text-ink-dim mb-2 [overflow-wrap:anywhere]">{submittedQuery}</p>
        {#if searching}
          <div class="text-sm text-ink-dim mt-2">Searching Nyaa…</div>
        {:else if filteredResults.length === 0}
          <div class="text-sm text-ink-dim mt-2">No results match this search and the selected filters.</div>
        {:else}
          <div class="mt-2 bg-panel border border-edge rounded-lg divide-y divide-edge/60 overflow-hidden">
            {#each filteredResults as t (t.guid)}
              <TorrentRow torrent={t} onopen={(url) => openItem(t, url)} />
            {/each}
          </div>
        {/if}
      </div>
    {/if}

    {#if !entriesLoaded}
      {#if !entriesError}
        <div class="text-ink-dim py-10 text-center">Loading your list…</div>
      {/if}
    {:else}
      {#if loading && !loaded}
        <p class="text-sm text-ink-dim mb-3">Checking feeds… You can browse downloads for your list below.</p>
      {/if}
      <p class="text-xs text-ink-dim mb-3">Open a show for batches and your next unwatched episode from different release groups.</p>
      <div class="space-y-3 mb-6">
        {#each categories as category (category.status)}
          {@const categoryOpen = expandedStatuses.has(category.status)}
          <section class="border border-edge rounded-lg overflow-hidden">
            <h2>
              <button
                type="button"
                onclick={() => toggleStatus(category.status)}
                aria-expanded={categoryOpen}
                aria-controls={`torrents-${category.status}`}
                class="w-full flex items-center gap-2.5 px-3 py-2.5 bg-panel hover:bg-panel-2 text-left"
              >
                <span class="text-ink-dim transition-transform {categoryOpen ? 'rotate-90' : ''}" aria-hidden="true">
                  <Icon name="chevron" size={14} />
                </span>
                <span class="font-semibold flex-1">{category.label}</span>
                <span class="text-xs text-ink-dim">
                  {category.total} show{category.total === 1 ? "" : "s"}
                  · {category.releases} feed release{category.releases === 1 ? "" : "s"}
                </span>
                {#if category.newCount > 0}
                  <span class="text-xs px-2 py-0.5 rounded-full bg-accent/15 text-accent">{category.newCount} new</span>
                {/if}
              </button>
            </h2>
            <div id={`torrents-${category.status}`} hidden={!categoryOpen}>
              {#if categoryOpen}
                <div class="space-y-3 p-3 border-t border-edge">
                  <div class="flex items-center gap-3">
                    <input
                      value={showFilters[category.status] ?? ""}
                      oninput={(event) => { showFilters[category.status] = event.currentTarget.value; }}
                      aria-label={`Filter ${category.label} shows`}
                      placeholder="Filter shows…"
                      class="min-w-0 w-64 bg-panel border border-edge rounded-md px-3 py-1.5 text-sm focus:outline-none focus:border-accent"
                    />
                    {#if showFilters[category.status]?.trim()}
                      <span class="text-xs text-ink-dim">{category.shows.length} of {category.total} shows</span>
                    {/if}
                  </div>
                  {#if category.shows.length === 0}
                    <p class="text-sm text-ink-dim py-3 text-center">
                      {showFilters[category.status]?.trim() ? "No shows or releases match your filter." : "No shows in this category."}
                    </p>
                  {/if}
                  {#each category.shows as g (g.mediaId)}
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
                        {#if g.cover}
                          <button
                            type="button"
                            onclick={(ev) => {
                              ev.stopPropagation();
                              goto(`/anime/${g.mediaId}`);
                            }}
                            title="Open details"
                            class="shrink-0"
                          >
                            <Img src={g.cover} class="w-10 h-14 object-cover rounded" />
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
                            class="block max-w-full [overflow-wrap:anywhere] font-medium text-left hover:text-accent transition-colors"
                          >
                            {g.title}
                          </button>
                          <div class="text-xs text-ink-dim flex flex-wrap items-center gap-1.5">
                            {#if g.entry.media?.episodes == null || g.entry.progress < g.entry.media.episodes}
                              <span>Find batches · Next episode {g.entry.progress + 1}</span>
                            {:else}
                              <span>Find batches</span>
                            {/if}
                            {#if g.entry.media?.status === "FINISHED"}
                              <span class="opacity-40">·</span>
                              <span>Finished airing</span>
                            {/if}
                            {#if g.items.length > 0}
                              <span class="opacity-40">·</span>
                              <span>{g.items.length} feed release{g.items.length === 1 ? "" : "s"}</span>
                            {/if}
                            {#if g.hasNew}
                              <span class="opacity-40">·</span>
                              <span class="text-accent">new</span>
                            {/if}
                          </div>
                        </div>
                      </div>
                      {#if isOpen}
                        <ShowTorrentChoices entry={g.entry} category={categoryFilter} filter={releaseFilter}
                          feedItems={g.items} seenLinks={seenLinks} seenIdentities={seenIdentities} cache={showTorrentCache} onopen={openItem} />
                      {/if}
                    </section>
                  {/each}
                </div>
              {/if}
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
  {#if removingFeed}
    <Confirm
      title="Remove feed?"
      body={removingFeed}
      confirmLabel="Remove"
      busy={removing}
      onconfirm={removeFeed}
      oncancel={() => (removingFeed = null)}
    />
  {/if}
{/if}
