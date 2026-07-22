<script lang="ts">
  import { untrack } from "svelte";
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { openPath, openUrl } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import {
    airingLabel,
    displayTitle,
    MEDIA_STATUS_LABEL,
    plainDescription,
    RELATION_LABEL,
    scoreLabel,
    sourceLabel,
    STATUS_LABEL,
    type ListEntry,
    type Media,
    type MediaDetail,
  } from "$lib/types";
  import EditEntry from "$lib/EditEntry.svelte";
  import EpisodeStepper from "$lib/EpisodeStepper.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import Login from "$lib/Login.svelte";

  const id = $derived(Number($page.params.id));

  let detail = $state<MediaDetail | null>(null);
  let entry = $state<ListEntry | null>(null);
  let recs = $state<Media[]>([]);
  let loading = $state(true);
  let error = $state("");
  let editing = $state(false);
  let adding = $state<string | null>(null);
  let expanded = $state(false);

  const status_options = [
    { v: "CURRENT", label: "Watching" },
    { v: "PLANNING", label: "Plan to watch" },
    { v: "COMPLETED", label: "Completed" },
  ];

  // Navigating relation → relation reuses this component; latest-wins per id.
  let loadId = 0;
  async function load(mediaId: number) {
    const reqId = ++loadId;
    loading = true;
    error = "";
    editing = false;
    expanded = false;
    recs = [];
    try {
      const [d, e] = await Promise.all([api.getMediaDetail(mediaId), api.getEntry(mediaId)]);
      if (reqId !== loadId) return;
      detail = d;
      entry = e ? { ...e, media: e.media ?? d.media } : null;
    } catch (err) {
      if (reqId === loadId) {
        detail = null;
        entry = null;
        error = String(err);
      }
    } finally {
      if (reqId === loadId) loading = false;
    }
    // Recommendations load after the main content; failures just hide the strip.
    try {
      const r = await api.getRecommendations(mediaId);
      if (reqId === loadId) recs = r;
    } catch {
      if (reqId === loadId) recs = [];
    }
  }

  async function reloadEntry() {
    try {
      const e = await api.getEntry(id);
      entry = e ? { ...e, media: e.media ?? detail?.media ?? null } : null;
    } catch {
      // keep whatever we had
    }
  }

  async function add(status: string) {
    adding = status;
    error = "";
    try {
      entry = await api.updateEntry(id, status, 0, null, 0);
    } catch (e) {
      error = String(e);
    } finally {
      adding = null;
    }
  }

  function applyEntry(e: ListEntry) {
    // A stepper flushing its buffered edit during relation→relation navigation
    // reports the PREVIOUS anime — don't let it clobber the new page's entry.
    if (e.media_id !== id) return;
    entry = { ...e, media: e.media ?? detail?.media ?? null };
  }

  const media = $derived(detail?.media ?? null);
  const desc = $derived(plainDescription(media?.description));
  const air = $derived(airingLabel(media));
  // First on-disk file for the next unwatched episode (from the last scan).
  const nextFile = $derived(entry ? library.fileFor(id, entry.progress + 1) : undefined);
  const meta = $derived.by(() => {
    if (!media) return "";
    const parts: string[] = [];
    if (media.format) parts.push(media.format);
    if (media.episodes) parts.push(`${media.episodes} eps`);
    if (media.duration) parts.push(`${media.duration} min`);
    if (media.season && media.season_year) {
      const s = media.season[0] + media.season.slice(1).toLowerCase();
      parts.push(`${s} ${media.season_year}`);
    } else if (media.season_year) {
      parts.push(`${media.season_year}`);
    }
    if (media.status && MEDIA_STATUS_LABEL[media.status]) parts.push(MEDIA_STATUS_LABEL[media.status]);
    if (media.average_score) parts.push(`★ ${media.average_score}`);
    if (media.source) parts.push(`Source: ${sourceLabel(media.source)}`);
    if (media.studios?.length) parts.push(media.studios.join(", "));
    return parts.join(" · ");
  });

  $effect(() => {
    const mediaId = id;
    if (auth.isLoggedIn && Number.isFinite(mediaId)) untrack(() => load(mediaId));
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else if loading && !detail}
  <div class="text-ink-dim py-16 text-center">Loading…</div>
{:else if !media}
  <div class="p-5 max-w-3xl mx-auto">
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-3 mt-6">
      {error || "This anime could not be loaded."}
    </div>
  </div>
{:else}
  {#if media.banner_image}
    <div class="relative h-40 md:h-48 overflow-hidden">
      <Img src={media.banner_image} class="w-full h-full object-cover opacity-50" />
      <div class="absolute inset-0 bg-gradient-to-t from-base via-base/40 to-transparent"></div>
    </div>
  {/if}

  <div class="max-w-4xl mx-auto px-5 pb-8 {media.banner_image ? '-mt-20 relative' : 'pt-5'}">
    <div class="flex items-end gap-5 mb-4">
      {#if media.cover_large ?? media.cover_medium}
        <Img
          src={media.cover_large ?? media.cover_medium ?? ""}
          class="w-32 h-[11.5rem] object-cover rounded-lg border border-edge shadow-2xl shrink-0"
        />
      {:else}
        <div class="w-32 h-[11.5rem] bg-panel-2 rounded-lg border border-edge shrink-0"></div>
      {/if}
      <div class="flex-1 min-w-0 pb-1">
        <div class="flex items-start gap-2">
          <h1 class="text-2xl font-semibold leading-tight flex-1">{displayTitle(media)}</h1>
          <button
            onclick={() => openUrl(`https://anilist.co/anime/${id}`)}
            title="Open on AniList"
            class="shrink-0 w-8 h-8 grid place-items-center rounded-md text-ink-dim hover:text-ink hover:bg-panel-2/60 mt-0.5"
          >
            <Icon name="external" size={15} />
          </button>
        </div>
        {#if media.title_romaji && media.title_romaji !== displayTitle(media)}
          <div class="text-sm text-ink-dim truncate mt-0.5">{media.title_romaji}</div>
        {/if}
        {#if meta}
          <div class="text-sm text-ink-dim mt-2">{meta}</div>
        {/if}
        {#if air}
          <div class="text-sm text-accent mt-1">{air}</div>
        {/if}
      </div>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if media.genres?.length}
      <div class="flex flex-wrap gap-1.5 mb-4">
        {#each media.genres as g (g)}
          <span class="text-xs px-2 py-0.5 rounded-full bg-panel-2 text-ink-dim">{g}</span>
        {/each}
      </div>
    {/if}

    <!-- Your list entry (or quick-add). Keyed on the anime id: navigating
         relation → relation must destroy the stepper so its buffered edit
         flushes against the OLD media id instead of leaking onto the new one. -->
    {#key id}
    <div class="bg-panel border border-edge rounded-lg p-3 mb-5 flex items-center gap-3 flex-wrap">
      {#if entry}
        <span class="text-xs px-2 py-1 rounded bg-panel-2 text-accent shrink-0">
          ✓ {STATUS_LABEL[entry.status] ?? entry.status}
        </span>
        <EpisodeStepper
          mediaId={id}
          progress={entry.progress}
          total={media.episodes ?? null}
          onchange={applyEntry}
        />
        {#if scoreLabel(entry.score, auth.user?.score_format)}
          <span class="text-sm text-ink-dim shrink-0">{scoreLabel(entry.score, auth.user?.score_format)}</span>
        {/if}
        <div class="flex-1"></div>
        {#if nextFile}
          <button
            onclick={() => openPath(nextFile.path)}
            title={nextFile.path}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm shrink-0 flex items-center gap-1.5"
          >
            <Icon name="play" size={13} /> Play Ep {nextFile.episode}
          </button>
        {/if}
        <button
          onclick={() => (editing = true)}
          class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm shrink-0 flex items-center gap-1.5"
        >
          <Icon name="edit" size={13} /> Edit
        </button>
      {:else}
        <span class="text-sm text-ink-dim shrink-0">Add to list:</span>
        {#each status_options as o (o.v)}
          <button
            onclick={() => add(o.v)}
            disabled={adding !== null}
            class="text-sm px-2.5 py-1 rounded bg-panel-2 hover:bg-edge disabled:opacity-50"
          >
            {adding === o.v ? "Adding…" : o.label}
          </button>
        {/each}
      {/if}
    </div>
    {/key}

    {#if desc}
      <div class="mb-6">
        <p class="whitespace-pre-line text-sm leading-relaxed text-ink-dim {expanded ? '' : 'line-clamp-6'}">
          {desc}
        </p>
        {#if desc.length > 420}
          <button
            onclick={() => (expanded = !expanded)}
            class="text-xs text-accent hover:underline mt-1"
          >
            {expanded ? "Show less" : "Show more"}
          </button>
        {/if}
      </div>
    {/if}

    {#if detail && detail.relations.length > 0}
      <div class="mb-6">
        <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Related</h2>
        <div class="flex gap-2.5 overflow-x-auto pb-1">
          {#each detail.relations as r (`${r.relation}-${r.media.id}`)}
            <button
              onclick={() => goto(`/anime/${r.media.id}`)}
              title={displayTitle(r.media)}
              class="w-24 shrink-0 text-left group"
            >
              {#if r.media.cover_medium}
                <Img src={r.media.cover_medium} class="w-24 h-32 object-cover rounded" />
              {:else}
                <div class="w-24 h-32 bg-panel-2 rounded"></div>
              {/if}
              <span class="block text-[10px] uppercase tracking-wide text-accent mt-1">
                {RELATION_LABEL[r.relation] ?? r.relation}
              </span>
              <span class="block text-[11px] leading-tight line-clamp-2 text-ink-dim group-hover:text-ink">
                {displayTitle(r.media)}
              </span>
            </button>
          {/each}
        </div>
      </div>
    {/if}

    {#if recs.length > 0}
      <div class="mb-2">
        <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">
          You might also like
        </h2>
        <div class="flex gap-2.5 overflow-x-auto pb-1">
          {#each recs as r (r.id)}
            <button
              onclick={() => goto(`/anime/${r.id}`)}
              title={displayTitle(r)}
              class="w-24 shrink-0 text-left group"
            >
              {#if r.cover_medium}
                <Img src={r.cover_medium} class="w-24 h-32 object-cover rounded" />
              {:else}
                <div class="w-24 h-32 bg-panel-2 rounded"></div>
              {/if}
              <span class="block text-[11px] leading-tight line-clamp-2 mt-1 text-ink-dim group-hover:text-ink">
                {displayTitle(r)}
              </span>
            </button>
          {/each}
        </div>
      </div>
    {/if}
  </div>
{/if}

{#if editing && entry}
  <EditEntry
    entry={entry}
    scoreFormat={auth.user?.score_format ?? null}
    onclose={() => {
      editing = false;
      reloadEntry();
    }}
  />
{/if}
