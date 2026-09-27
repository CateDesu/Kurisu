<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { untrack } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { preferences } from "$lib/preferences";
  import { auth } from "$lib/auth.svelte";
  import { nowMs } from "$lib/now.svelte";
  import { displayTitle, STATUS_LABEL, type AiringItem, type ListEntry } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";
  import type { Snapshot } from "./$types";

  const MINE_KEY = "kurisu.cal.mine";
  let weekOffset = $state(0);
  let watchingOnly = $state(preferences.get(MINE_KEY) !== "0");
  let items = $state<AiringItem[]>([]);
  let entries = $state<ListEntry[]>([]);
  let entriesLoading = $state(false);
  let entriesLoaded = $state(false);
  let entriesError = $state("");
  let loading = $state(false);
  let error = $state("");

  let entriesLoadId = 0;
  async function refreshEntries() {
    const id = ++entriesLoadId;
    const epoch = auth.epoch;
    entriesLoading = true;
    entriesLoaded = false;
    entriesError = "";
    entries = [];
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

  function setWatchingOnly(v: boolean) {
    watchingOnly = v;
    preferences.set(MINE_KEY, v ? "1" : "0");
  }

  function range(offset: number): { start: Date; end: Date } {
    const start = new Date();
    start.setHours(0, 0, 0, 0);
    start.setDate(start.getDate() + offset * 7);
    const end = new Date(start);
    end.setDate(end.getDate() + 7);
    return { start, end };
  }

  const rangeLabel = $derived.by(() => {
    const { start, end } = range(weekOffset);
    const last = new Date(end);
    last.setDate(last.getDate() - 1);
    const fmt = (d: Date) => d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
    return `${fmt(start)} – ${fmt(last)}`;
  });

  let loadId = 0;
  // Prevent a settled empty week from retriggering the load effect.
  let loadedOnce = false;
  async function load() {
    const id = ++loadId;
    const epoch = auth.epoch;
    loading = true;
    items = [];
    error = "";
    try {
      const { start, end } = range(weekOffset);
      const schedule = await api.getAiringSchedule(Math.floor(start.getTime() / 1000), Math.floor(end.getTime() / 1000));
      if (id !== loadId || epoch !== auth.epoch || !auth.isLoggedIn) return;
      items = schedule;
    } catch (e) {
      if (id === loadId && epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (id === loadId && epoch === auth.epoch) {
        loading = false;
        loadedOnce = true;
      }
    }
  }

  function shift(delta: number) {
    weekOffset += delta;
    void load();
    void refreshEntries();
  }

  // Restore invalidates the mount fetch. Captures during loading must refetch the selected week.
  export const snapshot: Snapshot<{ weekOffset: number; items: AiringItem[] }> = {
    capture: () => ({ weekOffset, items: loading ? [] : items }),
    restore: (v) => {
      weekOffset = v.weekOffset;
      items = v.items;
      loadId++;
      loadedOnce = true;
      loading = false;
      if (items.length === 0) void load();
      void refreshEntries();
    },
  };

  const onList = $derived(new Map(entries.map((e) => [e.media_id, e.status])));
  const progressByMedia = $derived(new Map(entries.map((e) => [e.media_id, e.progress])));
  const visible = $derived(items.filter((i) => !watchingOnly || onList.get(i.media.id) === "CURRENT"));
  const days = $derived.by(() => {
    const map = new Map<string, { date: Date; items: AiringItem[] }>();
    for (const it of [...visible].sort((a, b) => a.airing_at - b.airing_at)) {
      const d = new Date(it.airing_at * 1000);
      const key = d.toDateString();
      let g = map.get(key);
      if (!g) {
        g = { date: d, items: [] };
        map.set(key, g);
      }
      g.items.push(it);
    }
    return [...map.values()];
  });

  function dayLabel(d: Date): string {
    const today = new Date(nowMs());
    today.setHours(0, 0, 0, 0);
    const that = new Date(d);
    that.setHours(0, 0, 0, 0);
    const diff = Math.round((that.getTime() - today.getTime()) / 86_400_000);
    const base = d.toLocaleDateString(undefined, { weekday: "long", month: "short", day: "numeric" });
    if (diff === 0) return `Today · ${base}`;
    if (diff === 1) return `Tomorrow · ${base}`;
    return base;
  }

  function timeLabel(unix: number): string {
    return new Date(unix * 1000).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  }

  const aired = (unix: number) => unix * 1000 < nowMs();

  $effect(() => {
    const epoch = auth.epoch;
    const loggedIn = auth.isLoggedIn;
    loadId++;
    entriesLoadId++;
    items = [];
    entries = [];
    loadedOnce = false;
    loading = false;
    entriesLoaded = false;
    entriesLoading = false;
    entriesError = "";
    error = "";
    if (!loggedIn) return;
    let alive = true;
    let unlisten: (() => void) | undefined;
    listen("kurisu://episode-updated", () => {
      if (alive && epoch === auth.epoch) void refreshEntries();
    }).then((stop) => {
      if (alive && epoch === auth.epoch) {
        unlisten = stop;
        void refreshEntries();
      } else stop();
    }).catch((e) => {
      if (alive && epoch === auth.epoch) {
        error = `Could not listen for list updates: ${String(e)}`;
        void refreshEntries();
      }
    });
    return () => {
      alive = false;
      unlisten?.();
      loadId++;
      entriesLoadId++;
    };
  });

  $effect(() => {
    auth.epoch;
    if (!auth.isLoggedIn) return;
    untrack(() => { if (!loadedOnce) void load(); });
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <PageHeading section="On the air" title="Calendar" description={rangeLabel}>
      <div class="flex rounded-md border border-edge overflow-hidden text-sm">
        <button
          onclick={() => setWatchingOnly(true)}
          aria-pressed={watchingOnly}
          class="px-3 py-1.5 {watchingOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
        >
          Watching
        </button>
        <button
          onclick={() => setWatchingOnly(false)}
          aria-pressed={!watchingOnly}
          class="px-3 py-1.5 {!watchingOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
        >
          All
        </button>
      </div>
      <div class="flex gap-1">
        <button
          onclick={() => shift(-1)}
          disabled={loading}
          class="px-2.5 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
          title="Previous week"
        >
          ←
        </button>
        <button
          onclick={() => { weekOffset = 0; void load(); void refreshEntries(); }}
          disabled={loading || weekOffset === 0}
          class="px-2.5 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
        >
          Today
        </button>
        <button
          onclick={() => shift(1)}
          disabled={loading}
          class="px-2.5 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
          title="Next week"
        >
          →
        </button>
      </div>
    </PageHeading>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if entriesError}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4 flex items-center gap-3">
        <span class="flex-1">Could not read your list: {entriesError}</span>
        <button onclick={() => refreshEntries()} disabled={entriesLoading} class="shrink-0 underline disabled:opacity-50">Retry list</button>
      </div>
    {/if}

    {#if watchingOnly && !entriesLoaded}
      {#if !entriesError}
        <div class="text-ink-dim py-10 text-center">Loading your list…</div>
      {/if}
    {:else if loading && items.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if days.length === 0 && !error}
      <div class="text-ink-dim py-10 text-center">
        {watchingOnly ? "None of your Watching shows air this week." : "Nothing airing this week."}
      </div>
    {:else}
      <div class="space-y-5">
        {#each days as day (day.date.toDateString())}
          <section>
            <h2 class="text-sm font-semibold text-ink-dim mb-1.5">{dayLabel(day.date)}</h2>
            <div class="bg-panel border border-edge rounded-lg divide-y divide-edge/60 overflow-hidden">
              {#each day.items as it (`${it.media.id}-${it.episode}`)}
                {@const status = onList.get(it.media.id)}
                {@const progress = progressByMedia.get(it.media.id)}
                <button
                  onclick={() => goto(`/anime/${it.media.id}`)}
                  class="cv-row w-full text-left flex items-center gap-3 px-3 py-2 hover:bg-panel-2/60 transition-colors {aired(it.airing_at) ? 'opacity-55' : ''}"
                >
                  <span class="w-12 shrink-0 text-sm text-ink-dim tabular-nums">{timeLabel(it.airing_at)}</span>
                  {#if it.media.cover_medium}
                    <Img src={it.media.cover_medium} class="w-8 h-11 object-cover rounded shrink-0" />
                  {:else}
                    <div class="w-8 h-11 bg-panel-2 rounded shrink-0"></div>
                  {/if}
                  <span class="flex-1 min-w-0">
                    <span class="block text-sm">{displayTitle(it.media)}</span>
                    <span class="flex flex-wrap items-center gap-x-3 gap-y-1 mt-1">
                      <span class="text-sm text-ink-dim tabular-nums">
                        Ep {it.episode}{it.media.episodes ? `/${it.media.episodes}` : ""}
                      </span>
                      {#if progress !== undefined}
                        <span
                          class="text-xs px-2 py-0.5 rounded bg-panel-2 tabular-nums {progress < it.episode - 1
                            ? 'text-amber-400'
                            : 'text-ink-dim'}"
                        >
                          you: {progress}/{it.episode}
                        </span>
                      {/if}
                      {#if status}
                        <span class="text-xs px-2 py-0.5 rounded bg-panel-2 text-accent">
                          {STATUS_LABEL[status] ?? status}
                        </span>
                      {/if}
                    </span>
                  </span>
                </button>
              {/each}
            </div>
          </section>
        {/each}
      </div>
    {/if}
  </div>
{/if}
