<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
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
  const loggedIn = $derived(auth.isLoggedIn);

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

  let loadId = 0;
  let entryLoadId = 0;
  let addId = 0;
  async function load(mediaId: number) {
    const reqId = ++loadId;
    const entryReq = ++entryLoadId;
    const epoch = auth.epoch;
    const current = () => reqId === loadId && epoch === auth.epoch && auth.isLoggedIn && mediaId === id;
    loading = true;
    error = "";
    try {
      const [d, e] = await Promise.all([api.getMediaDetail(mediaId), api.getEntry(mediaId)]);
      if (!current()) return;
      detail = d;
      if (entryReq === entryLoadId) entry = e ? { ...e, media: e.media ?? d.media } : null;
    } catch (err) {
      if (current()) error = String(err);
    } finally {
      if (current()) loading = false;
    }
    if (!current()) return;
    try {
      const r = await api.getRecommendations(mediaId);
      if (current()) recs = r;
    } catch {
      if (current()) recs = [];
    }
  }

  async function reloadEntry() {
    if (!auth.isLoggedIn) return;
    const reqId = ++entryLoadId;
    const epoch = auth.epoch;
    const mediaId = id;
    try {
      const e = await api.getEntry(mediaId);
      if (reqId !== entryLoadId || epoch !== auth.epoch || !auth.isLoggedIn || mediaId !== id) return;
      entry = e ? { ...e, media: e.media ?? detail?.media ?? null } : null;
    } catch {
    }
  }

  async function add(status: string) {
    if (!auth.isLoggedIn || adding) return;
    const reqId = ++addId;
    const epoch = auth.epoch;
    const mediaId = id;
    const current = () => reqId === addId && epoch === auth.epoch && auth.isLoggedIn && mediaId === id;
    adding = status;
    error = "";
    try {
      const e = await api.updateEntry(mediaId, status, 0, null, 0);
      if (!current()) return;
      entryLoadId++;
      entry = e;
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (current()) adding = null;
    }
  }

  function applyEntry(e: ListEntry) {
    // A stepper may finish saving the previous anime during navigation.
    if (e.media_id !== id) return;
    entryLoadId++;
    entry = { ...e, media: e.media ?? detail?.media ?? null };
  }

  const media = $derived(detail?.media ?? null);
  const desc = $derived(plainDescription(media?.description));
  const air = $derived(airingLabel(media));
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
    auth.epoch;
    loggedIn;
    id;
    detail = null;
    entry = null;
    editing = false;
    adding = null;
    expanded = false;
    recs = [];
    loading = false;
    error = "";
    return () => { loadId++; entryLoadId++; addId++; };
  });

  $effect(() => {
    auth.epoch;
    const mediaId = id;
    if (!auth.isLoggedIn) return;
    if (!Number.isFinite(mediaId)) {
      detail = null;
      entry = null;
      loading = false;
      error = "Invalid anime id.";
      return;
    }
    untrack(() => load(mediaId));
  });

  $effect(() => {
    const epoch = auth.epoch;
    if (auth.isLoggedIn) {
      library.loadFolders().then(() => {
        if (epoch !== auth.epoch || !auth.isLoggedIn) return;
        if (library.folders.length > 0 && !library.hasScan && !library.scanning)
          library.scan().catch((e) => console.error("library scan failed", e));
      });
    }
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else if loading && !detail}
  <div class="page-content">
    <PageHeading index="↗" section="Your collection" title="Anime details" />
    <div class="text-ink-dim py-16 text-center">Loading…</div>
  </div>
{:else if !media}
  <div class="page-content">
    <PageHeading index="↗" section="Your collection" title="Anime details" />
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

  <div class="page-content {media.banner_image ? '-mt-20 relative' : ''}">
    <div class="flex items-start gap-5 mb-4">
      {#if media.cover_large ?? media.cover_medium}
        <Img
          src={media.cover_large ?? media.cover_medium ?? ""}
          class="w-32 h-[11.5rem] object-cover rounded-lg border border-edge shadow-2xl shrink-0"
        />
      {:else}
        <div class="w-32 h-[11.5rem] bg-panel-2 rounded-lg border border-edge shrink-0"></div>
      {/if}
      <div class="flex-1 min-w-0 pb-1">
        <PageHeading index="↗" section="Anime details" title={displayTitle(media)} compact>
          <button
            onclick={() => openUrl(`https://anilist.co/anime/${id}`)}
            title="Open on AniList"
            class="flex items-center gap-2 text-sm text-accent hover:text-ink"
          >
            Open on AniList
            <Icon name="external" size={15} />
          </button>
        </PageHeading>
        {#if media.title_romaji && media.title_romaji !== displayTitle(media)}
          <div class="text-sm text-ink-dim mt-0.5">{media.title_romaji}</div>
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

    <!-- Recreate the stepper so buffered edits flush against the previous anime ID. -->
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
              class="w-28 shrink-0 self-start text-left group"
            >
              {#if r.media.cover_medium}
                <Img src={r.media.cover_medium} class="w-28 h-40 object-cover rounded" />
              {:else}
                <div class="w-28 h-40 bg-panel-2 rounded"></div>
              {/if}
              <span class="block text-xs uppercase tracking-wide text-accent mt-1">
                {RELATION_LABEL[r.relation] ?? r.relation}
              </span>
              <span class="text-xs leading-tight line-clamp-2 text-ink-dim group-hover:text-ink">
                {displayTitle(r.media)}
              </span>
            </button>
          {/each}
        </div>
      </div>
    {/if}

    {#if detail && detail.characters.length > 0}
      <div class="mb-6">
        <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Characters</h2>
        <div class="flex gap-2.5 overflow-x-auto pb-1">
          {#each detail.characters as c, i (`${c.name}-${i}`)}
            <div class="w-28 shrink-0" title={c.va_name ? `${c.name} — CV: ${c.va_name}` : c.name}>
              {#if c.image}
                <Img src={c.image} class="w-28 h-40 object-cover rounded" />
              {:else}
                <div class="w-28 h-40 bg-panel-2 rounded"></div>
              {/if}
              <span class="text-xs leading-tight line-clamp-2 mt-1">{c.name}</span>
              {#if c.va_name}
                <span class="text-xs text-ink-dim line-clamp-1">{c.va_name}</span>
              {/if}
            </div>
          {/each}
        </div>
      </div>
    {/if}

    {#if detail && detail.staff.length > 0}
      <div class="mb-6">
        <h2 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">Staff</h2>
        <div class="flex gap-2.5 overflow-x-auto pb-1">
          {#each detail.staff as s, i (`${s.name}-${i}`)}
            <div class="w-28 shrink-0" title="{s.name}{s.role ? ` — ${s.role}` : ''}">
              {#if s.image}
                <Img src={s.image} class="w-28 h-40 object-cover rounded" />
              {:else}
                <div class="w-28 h-40 bg-panel-2 rounded"></div>
              {/if}
              <span class="text-xs leading-tight line-clamp-2 mt-1">{s.name}</span>
              {#if s.role}
                <span class="text-xs text-ink-dim line-clamp-1">{s.role}</span>
              {/if}
            </div>
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
              class="w-28 shrink-0 self-start text-left group"
            >
              {#if r.cover_medium}
                <Img src={r.cover_medium} class="w-28 h-40 object-cover rounded" />
              {:else}
                <div class="w-28 h-40 bg-panel-2 rounded"></div>
              {/if}
              <span class="text-xs leading-tight line-clamp-2 mt-1 text-ink-dim group-hover:text-ink">
                {displayTitle(r)}
              </span>
            </button>
          {/each}
        </div>
      </div>
    {/if}
  </div>
{/if}

{#if auth.isLoggedIn && editing && entry}
  <EditEntry
    entry={entry}
    scoreFormat={auth.user?.score_format ?? null}
    onclose={() => {
      editing = false;
      reloadEntry();
    }}
  />
{/if}
