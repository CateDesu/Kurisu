<script lang="ts">
  import PageHeading from "#lib/PageHeading.svelte";
  import { onDestroy } from "svelte";
  import { afterNavigate } from "$app/navigation";
  import { api } from "#lib/api.ts";
  import { auth } from "#lib/auth.svelte.ts";
  import type { ListEntry, Media } from "#lib/types.ts";
  import Login from "#lib/Login.svelte";
  import AnimeCard from "#lib/AnimeCard.svelte";
  import { applyListUpdate, createListActions, listenForListUpdates, readListUpdate } from "#lib/list.svelte.ts";
  import { measuredRows } from "#lib/measuredRows.ts";
  import type { Snapshot } from "./$types";

  let query = $state("");
  let results = $state.raw<Media[]>([]);
  let searching = $state(false);
  let error = $state("");
  const actions = createListActions();
  const loggedIn = $derived(auth.isLoggedIn);

  // Reassign for reactive badge lookups.
  let listByMedia = $state(new Map<number, ListEntry>());
  let listLoadId = 0;
  let listLoaded = false;
  let searchId = 0;
  let alive = true;
  let submittedQuery = $state("");
  let resultPage = $state(0);
  let hasMore = $state(false);
  let searched = $state(false);
  let resume: { term: string; more: boolean } | null = null;

  async function loadList(mediaIds?: readonly number[]) {
    const id = ++listLoadId;
    const epoch = auth.epoch;
    const current = () => alive && id === listLoadId && epoch === auth.epoch && auth.isLoggedIn;
    try {
      const update = await readListUpdate(listLoaded ? mediaIds : undefined);
      if (current()) {
        listByMedia = new Map(applyListUpdate([...listByMedia.values()], update).map((e) => [e.media_id, e]));
        listLoaded = true;
      }
    } catch (e) {
      if (current()) error = String(e);
    }
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    listByMedia = new Map();
    listLoaded = false;
    error = "";
    actions.reset();
    searching = false;
    resume = null;
    return () => { listLoadId++; searchId++; actions.reset(); };
  });

  $effect(() => {
    const epoch = auth.epoch;
    if (!auth.isLoggedIn) return;
    return listenForListUpdates(
      (mediaIds) => { if (epoch === auth.epoch) return loadList(mediaIds); },
      (e) => { if (epoch === auth.epoch) error = String(e); },
      undefined,
      { invalidate: () => { listLoadId++; }, revision: () => listLoadId },
    );
  });

  export const snapshot: Snapshot<{ query: string; results: Media[]; submittedQuery: string; resultPage: number; hasMore: boolean; searched: boolean; pending: { term: string; more: boolean } | null }> = {
    capture: () => ({ query, results, submittedQuery, resultPage, hasMore, searched, pending: searching ? { term: submittedQuery, more: resultPage > 0 } : resume }),
    restore: (v) => {
      searchId++;
      searching = false;
      query = v.query;
      results = v.results;
      submittedQuery = v.submittedQuery;
      resultPage = v.resultPage;
      hasMore = v.hasMore;
      searched = v.searched;
      resume = v.pending;
    },
  };

  function run(e: Event) {
    e.preventDefault();
    void search(query.trim());
  }

  async function search(term: string, more = false) {
    if (!term || !auth.isLoggedIn) return;
    const id = ++searchId;
    const epoch = auth.epoch;
    const current = () => alive && id === searchId && epoch === auth.epoch && auth.isLoggedIn;
    searching = true;
    submittedQuery = term;
    resume = null;
    if (!more) {
      results = [];
      resultPage = 0;
      hasMore = false;
      searched = false;
    }
    error = "";
    try {
      const found = await api.searchAnimePage(term, more ? resultPage + 1 : 1);
      if (current()) {
        const merged = new Map((more ? results : []).map((media) => [media.id, media]));
        for (const media of found.items) merged.set(media.id, media);
        results = [...merged.values()];
        resultPage = found.page;
        hasMore = found.has_next_page;
        searched = true;
      }
    } catch (err) {
      if (current()) error = String(err);
    } finally {
      if (current()) searching = false;
    }
  }

  afterNavigate(() => {
    queueMicrotask(() => {
      if (alive && auth.isLoggedIn && resume) void search(resume.term, resume.more);
    });
  });

  async function add(m: Media, status: string) {
    error = "";
    const entry = await actions.add(m, status);
    if (!entry || !alive) return;
    listLoadId++;
    listByMedia = new Map(listByMedia).set(m.id, entry);
    if (!listLoaded) void loadList();
  }

  onDestroy(() => { alive = false; });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
<div class="page-content">
  <PageHeading section="Discover" title="Search" description="Find your next show">
    <form onsubmit={run} class="flex w-72 max-w-full gap-2">
      <input
        bind:value={query}
        aria-label="Anime title"
        placeholder="Anime title…"
        class="flex-1 min-w-0 bg-panel border border-edge rounded-md px-3 py-2 focus:outline-none focus:border-accent"
      />
      <button class="px-4 py-2 rounded-md bg-accent hover:bg-accent-2 text-white" disabled={searching}>
        {searching ? "…" : "Search"}
      </button>
    </form>
  </PageHeading>

  {#if error || actions.error}
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
      {error || actions.error}
    </div>
  {/if}

  {#if searched && results.length === 0 && !searching && !error}
    <p class="text-ink-dim text-center py-10">No anime found for “{submittedQuery}”. Try another title.</p>
  {/if}
  <div class="media-grid" use:measuredRows>
    {#each results as m (m.id)}
      <AnimeCard media={m} entry={listByMedia.get(m.id)} adding={actions.pending(m.id)} onadd={add} showYear showListedActions />
    {/each}
  </div>
  {#if hasMore}
    <div class="text-center mt-6">
      <button onclick={() => search(submittedQuery, true)} disabled={searching} class="px-4 py-2 rounded-md bg-panel-2 hover:bg-edge disabled:opacity-50">
        {searching ? "Loading…" : "Load more anime"}
      </button>
    </div>
  {/if}
</div>
{/if}
