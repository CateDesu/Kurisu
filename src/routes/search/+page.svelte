<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { afterNavigate } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import type { ListEntry, Media } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import AnimeCard from "$lib/AnimeCard.svelte";
  import { addToList } from "$lib/list";
  import type { Snapshot } from "./$types";

  let query = $state("");
  let results = $state<Media[]>([]);
  let searching = $state(false);
  let error = $state("");
  let adding = $state<number | null>(null);
  const loggedIn = $derived(auth.isLoggedIn);

  // Reassign the Map so badge lookups react to changes.
  let listByMedia = $state(new Map<number, ListEntry>());
  let listLoadId = 0;
  let searchId = 0;
  let addId = 0;
  let alive = true;
  let submittedQuery = "";
  let resumeQuery: string | null = null;

  async function loadList() {
    const id = ++listLoadId;
    const epoch = auth.epoch;
    const current = () => alive && id === listLoadId && epoch === auth.epoch && auth.isLoggedIn;
    try {
      const es = await api.localEntries();
      if (current()) listByMedia = new Map(es.map((e) => [e.media_id, e]));
    } catch (e) {
      if (current()) error = String(e);
    }
  }

  $effect(() => {
    auth.epoch;
    loggedIn;
    listByMedia = new Map();
    error = "";
    adding = null;
    searching = false;
    resumeQuery = null;
    return () => { listLoadId++; searchId++; addId++; };
  });

  $effect(() => {
    auth.epoch;
    if (auth.isLoggedIn) untrack(() => loadList());
  });

  export const snapshot: Snapshot<{ query: string; results: Media[]; pendingQuery: string | null }> = {
    capture: () => ({ query, results, pendingQuery: searching ? submittedQuery : resumeQuery }),
    restore: (v) => {
      searchId++;
      searching = false;
      query = v.query;
      resumeQuery = v.pendingQuery;
      results = resumeQuery ? [] : v.results;
    },
  };

  function run(e: Event) {
    e.preventDefault();
    void search(query.trim());
  }

  async function search(term: string) {
    if (!term || !auth.isLoggedIn) return;
    const id = ++searchId;
    const epoch = auth.epoch;
    const current = () => alive && id === searchId && epoch === auth.epoch && auth.isLoggedIn;
    searching = true;
    submittedQuery = term;
    resumeQuery = null;
    results = [];
    error = "";
    try {
      const found = await api.searchAnime(term);
      if (current()) results = found;
    } catch (err) {
      if (current()) error = String(err);
    } finally {
      if (current()) searching = false;
    }
  }

  afterNavigate(() => {
    queueMicrotask(() => {
      if (alive && auth.isLoggedIn && resumeQuery) void search(resumeQuery);
    });
  });

  async function add(m: Media, status: string) {
    const id = ++addId;
    const epoch = auth.epoch;
    const current = () => alive && epoch === auth.epoch && auth.isLoggedIn;
    adding = m.id;
    error = "";
    try {
      const entry = await addToList(m, status);
      if (!current()) return;
      listByMedia = new Map(listByMedia).set(m.id, entry);
      void loadList();
    } catch (err) {
      if (current() && id === addId) error = String(err);
    } finally {
      if (current() && id === addId) adding = null;
    }
  }

  onDestroy(() => { alive = false; });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
<div class="page-content">
  <h1 class="text-xl font-semibold mb-4">Search</h1>

  <form onsubmit={run} class="flex gap-2 mb-5">
    <input
      bind:value={query}
      placeholder="Anime title…"
      class="flex-1 min-w-0 bg-panel border border-edge rounded-md px-3 py-2 focus:outline-none focus:border-accent"
    />
    <button class="px-4 py-2 rounded-md bg-accent hover:bg-accent-2 text-white" disabled={searching}>
      {searching ? "…" : "Search"}
    </button>
  </form>

  {#if error}
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
      {error}
    </div>
  {/if}

  <div class="media-grid">
    {#each results as m (m.id)}
      <AnimeCard media={m} entry={listByMedia.get(m.id)} adding={adding === m.id} onadd={add} showYear showListedActions />
    {/each}
  </div>
</div>
{/if}
