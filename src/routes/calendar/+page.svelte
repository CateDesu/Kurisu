<script lang="ts">
  import { untrack } from "svelte";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { nowMs } from "$lib/now.svelte";
  import { displayTitle, STATUS_LABEL, type AiringItem, type ListEntry } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

  const MINE_KEY = "kurisu.cal.mine";
  function readMine(): boolean {
    try {
      return localStorage.getItem(MINE_KEY) !== "0";
    } catch {
      return true;
    }
  }

  let weekOffset = $state(0);
  let mineOnly = $state(readMine());
  let items = $state<AiringItem[]>([]);
  let entries = $state<ListEntry[]>([]);
  let loading = $state(false);
  let error = $state("");

  function setMine(v: boolean) {
    mineOnly = v;
    try {
      localStorage.setItem(MINE_KEY, v ? "1" : "0");
    } catch {
      // storage unavailable — the toggle just won't persist
    }
  }

  /// Rolling 7-day window: local midnight today (+ offset weeks) → +7 days.
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

  // Rapid Prev/Next resolves latest-wins.
  let loadId = 0;
  async function load() {
    const id = ++loadId;
    loading = true;
    error = "";
    try {
      const { start, end } = range(weekOffset);
      const [schedule, myEntries] = await Promise.all([
        api.getAiringSchedule(Math.floor(start.getTime() / 1000), Math.floor(end.getTime() / 1000)),
        api.localEntries(),
      ]);
      if (id !== loadId) return;
      items = schedule;
      entries = myEntries;
    } catch (e) {
      if (id === loadId) error = String(e);
    } finally {
      if (id === loadId) loading = false;
    }
  }

  function shift(delta: number) {
    weekOffset += delta;
    load();
  }

  const onList = $derived(new Map(entries.map((e) => [e.media_id, e.status])));
  const visible = $derived(items.filter((i) => !mineOnly || onList.has(i.media.id)));
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
    if (auth.isLoggedIn) untrack(() => load());
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-3xl mx-auto">
    <div class="flex items-center gap-3 mb-4 flex-wrap">
      <h1 class="text-xl font-semibold">Calendar</h1>
      <span class="text-sm text-ink-dim flex-1">{rangeLabel}</span>
      <div class="flex rounded-md border border-edge overflow-hidden text-sm">
        <button
          onclick={() => setMine(true)}
          class="px-3 py-1.5 {mineOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
        >
          My shows
        </button>
        <button
          onclick={() => setMine(false)}
          class="px-3 py-1.5 {!mineOnly ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink'}"
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
          onclick={() => { weekOffset = 0; load(); }}
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
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if loading && items.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if days.length === 0}
      <div class="text-ink-dim py-10 text-center">
        {mineOnly ? "Nothing on your list airs this week." : "Nothing airing this week."}
      </div>
    {:else}
      <div class="space-y-5">
        {#each days as day (day.date.toDateString())}
          <section>
            <h2 class="text-sm font-semibold text-ink-dim mb-1.5">{dayLabel(day.date)}</h2>
            <div class="bg-panel border border-edge rounded-lg divide-y divide-edge/60 overflow-hidden">
              {#each day.items as it (`${it.media.id}-${it.episode}`)}
                {@const status = onList.get(it.media.id)}
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
                  <span class="flex-1 min-w-0 truncate text-sm">{displayTitle(it.media)}</span>
                  <span class="shrink-0 text-sm text-ink-dim tabular-nums">
                    Ep {it.episode}{it.media.episodes ? `/${it.media.episodes}` : ""}
                  </span>
                  {#if status}
                    <span class="shrink-0 text-xs px-2 py-0.5 rounded bg-panel-2 text-accent">
                      {STATUS_LABEL[status] ?? status}
                    </span>
                  {/if}
                </button>
              {/each}
            </div>
          </section>
        {/each}
      </div>
    {/if}
  </div>
{/if}
