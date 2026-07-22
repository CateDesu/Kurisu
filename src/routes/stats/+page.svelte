<script lang="ts">
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { STATUS_LABEL, type UserStats } from "$lib/types";
  import Login from "$lib/Login.svelte";

  let stats = $state<UserStats | null>(null);
  let loading = $state(true);
  let error = $state("");

  let loadId = 0;
  async function load() {
    const id = ++loadId;
    loading = true;
    error = "";
    try {
      const s = await api.getUserStats();
      if (id !== loadId) return;
      stats = s;
    } catch (e) {
      if (id === loadId) error = String(e);
    } finally {
      if (id === loadId) loading = false;
    }
  }

  const daysWatched = $derived(stats ? stats.minutes_watched / 1440 : 0);
  // Fixed presentation order for the status breakdown.
  const STATUS_ORDER = ["CURRENT", "REPEATING", "COMPLETED", "PAUSED", "DROPPED", "PLANNING"];
  const statuses = $derived.by(() => {
    if (!stats) return [];
    return [...stats.statuses].sort(
      (a, b) => STATUS_ORDER.indexOf(a.status) - STATUS_ORDER.indexOf(b.status)
    );
  });
  const maxStatus = $derived(Math.max(1, ...statuses.map((s) => s.count)));
  const scores = $derived(stats ? [...stats.scores].sort((a, b) => a.score - b.score) : []);
  const maxScore = $derived(Math.max(1, ...scores.map((s) => s.count)));
  const maxGenre = $derived(Math.max(1, ...(stats?.genres.map((g) => g.count) ?? [])));
  const maxYear = $derived(Math.max(1, ...(stats?.release_years.map((y) => y.count) ?? [])));

  function pct(n: number, max: number): string {
    return `${Math.max(2, Math.round((n / max) * 100))}%`;
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
  <div class="p-5 max-w-3xl mx-auto">
    <div class="flex items-center gap-3 mb-4">
      <h1 class="text-xl font-semibold flex-1">Stats</h1>
      <button
        onclick={load}
        disabled={loading}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {loading ? "Loading…" : "↻ Refresh"}
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if loading && !stats}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if stats}
      <!-- Headline numbers. -->
      <div class="grid grid-cols-2 md:grid-cols-4 gap-3 mb-6">
        <div class="bg-panel border border-edge rounded-lg p-3">
          <div class="text-2xl font-semibold tabular-nums">{stats.count}</div>
          <div class="text-xs text-ink-dim mt-0.5">Anime</div>
        </div>
        <div class="bg-panel border border-edge rounded-lg p-3">
          <div class="text-2xl font-semibold tabular-nums">{stats.episodes_watched.toLocaleString()}</div>
          <div class="text-xs text-ink-dim mt-0.5">Episodes</div>
        </div>
        <div class="bg-panel border border-edge rounded-lg p-3">
          <div class="text-2xl font-semibold tabular-nums">{daysWatched.toFixed(1)}</div>
          <div class="text-xs text-ink-dim mt-0.5">Days watched</div>
        </div>
        <div class="bg-panel border border-edge rounded-lg p-3">
          <div class="text-2xl font-semibold tabular-nums">
            {stats.mean_score > 0 ? stats.mean_score : "—"}
          </div>
          <div class="text-xs text-ink-dim mt-0.5">
            Mean score{stats.standard_deviation > 0 ? ` · σ ${stats.standard_deviation}` : ""}
          </div>
        </div>
      </div>

      {#if statuses.length > 0}
        <div class="mb-6">
          <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">By status</h2>
          <div class="bg-panel border border-edge rounded-lg p-3 space-y-2">
            {#each statuses as s (s.status)}
              <div class="flex items-center gap-2 text-sm">
                <span class="w-28 shrink-0 text-ink-dim">{STATUS_LABEL[s.status] ?? s.status}</span>
                <div class="flex-1 h-4 bg-panel-2 rounded overflow-hidden">
                  <div class="h-full bg-accent/70 rounded" style="width: {pct(s.count, maxStatus)}"></div>
                </div>
                <span class="w-10 shrink-0 text-right tabular-nums">{s.count}</span>
              </div>
            {/each}
          </div>
        </div>
      {/if}

      {#if scores.length > 0}
        <div class="mb-6">
          <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Score distribution</h2>
          <div class="bg-panel border border-edge rounded-lg p-3 space-y-1.5">
            {#each scores as s (s.score)}
              <div class="flex items-center gap-2 text-sm">
                <span class="w-10 shrink-0 text-ink-dim text-right tabular-nums">{s.score}</span>
                <div class="flex-1 h-3.5 bg-panel-2 rounded overflow-hidden">
                  <div class="h-full bg-accent/70 rounded" style="width: {pct(s.count, maxScore)}"></div>
                </div>
                <span class="w-10 shrink-0 text-right tabular-nums text-ink-dim">{s.count}</span>
              </div>
            {/each}
          </div>
        </div>
      {/if}

      {#if stats.genres.length > 0}
        <div class="mb-6">
          <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Top genres</h2>
          <div class="bg-panel border border-edge rounded-lg p-3 space-y-2">
            {#each stats.genres as g (g.genre)}
              <div class="flex items-center gap-2 text-sm">
                <span class="w-28 shrink-0 text-ink-dim truncate">{g.genre}</span>
                <div class="flex-1 h-4 bg-panel-2 rounded overflow-hidden">
                  <div class="h-full bg-accent/70 rounded" style="width: {pct(g.count, maxGenre)}"></div>
                </div>
                <span class="w-24 shrink-0 text-right text-xs text-ink-dim tabular-nums">
                  {g.count} · {Math.round(g.minutes_watched / 60)}h
                </span>
              </div>
            {/each}
          </div>
        </div>
      {/if}

      {#if stats.formats.length > 0}
        <div class="mb-6">
          <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Formats</h2>
          <div class="flex flex-wrap gap-1.5">
            {#each stats.formats as f (f.format)}
              <span class="text-xs px-2 py-1 rounded-full bg-panel-2">
                {f.format} <span class="text-ink-dim">{f.count}</span>
              </span>
            {/each}
          </div>
        </div>
      {/if}

      {#if stats.release_years.length > 1}
        <div class="mb-2">
          <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">By release year</h2>
          <div class="bg-panel border border-edge rounded-lg p-3">
            <div class="flex items-end gap-[3px] h-24">
              {#each stats.release_years as y (y.year)}
                <div
                  class="flex-1 min-w-[3px] bg-accent/60 hover:bg-accent rounded-t transition-colors"
                  style="height: {pct(y.count, maxYear)}"
                  title="{y.year}: {y.count}"
                ></div>
              {/each}
            </div>
            <div class="flex justify-between text-[10px] text-ink-dim mt-1">
              <span>{stats.release_years[0].year}</span>
              <span>{stats.release_years[stats.release_years.length - 1].year}</span>
            </div>
          </div>
        </div>
      {/if}
    {/if}
  </div>
{/if}
