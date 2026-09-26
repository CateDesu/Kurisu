<script lang="ts">
  import { goto } from "$app/navigation";
  import { displayTitle, STATUS_LABEL, type ListEntry, type Media } from "$lib/types";
  import Img from "$lib/Img.svelte";

  let { media, entry, adding, onadd, showYear = false, showListedActions = false }: {
    media: Media;
    entry?: ListEntry;
    adding: boolean;
    onadd: (media: Media, status: string) => void;
    showYear?: boolean;
    showListedActions?: boolean;
  } = $props();

  const statuses = [
    { value: "CURRENT", label: "Watching" },
    { value: "PLANNING", label: "Plan to watch" },
    { value: "COMPLETED", label: "Completed" },
  ];
  const badge = $derived.by(() => {
    if (!entry) return "";
    const label = STATUS_LABEL[entry.status] ?? entry.status;
    if (entry.progress <= 0) return label;
    return media.episodes
      ? `${label} · ${entry.progress}/${media.episodes}`
      : `${label} · ${entry.progress}`;
  });
</script>

<div class="cv-card bg-panel border border-edge rounded-lg overflow-hidden flex flex-col">
  <button type="button" onclick={() => goto(`/anime/${media.id}`)} title="Open details" class="block">
    {#if media.cover_large}
      <Img src={media.cover_large} class="w-full h-44 object-cover" />
    {:else}
      <div class="w-full h-44 bg-panel-2"></div>
    {/if}
  </button>
  <div class="p-2.5 flex-1 flex flex-col">
    <button
      type="button"
      onclick={() => goto(`/anime/${media.id}`)}
      title="Open details"
      class="text-sm font-medium leading-tight [overflow-wrap:anywhere] mb-1 text-left hover:text-accent transition-colors"
    >
      {displayTitle(media)}
    </button>
    <div class="text-xs text-ink-dim mb-2">
      {#if media.format}{media.format}{/if}
      {#if showYear && media.season_year}· {media.season_year}{/if}
      {#if media.episodes}· {media.episodes} eps{/if}
      {#if media.average_score}· ★ {media.average_score}{/if}
    </div>
    {#if entry && showListedActions}
      <div class="mb-2">
        <span class="inline-block text-xs px-2 py-0.5 rounded bg-accent/15 text-accent">{badge}</span>
      </div>
    {/if}
    <div class="mt-auto">
      {#if entry && !showListedActions}
        <span class="text-xs px-2 py-1 rounded bg-panel-2 text-accent">✓ {badge}</span>
      {:else}
        <div class="flex gap-1 flex-wrap">
          {#each statuses as status}
            <button
              onclick={() => onadd(media, status.value)}
              disabled={adding}
              class="text-xs px-2 py-1 rounded disabled:opacity-50 {entry?.status === status.value
                ? 'bg-accent text-white'
                : 'bg-panel-2 hover:bg-edge'}"
            >
              {status.label}
            </button>
          {/each}
        </div>
      {/if}
    </div>
  </div>
</div>
