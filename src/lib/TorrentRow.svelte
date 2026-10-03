<script lang="ts">
  import Icon from "$lib/Icon.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { timeAgo, type TorrentItem } from "$lib/types";

  let { torrent: t, onopen }: { torrent: TorrentItem; onopen: (url: string) => void } = $props();

  const categories: Record<string, string> = {
    "1_1": "Anime music video",
    "1_2": "English-translated",
    "1_3": "Non-English-translated",
    "1_4": "Raw",
  };
  const category = $derived(categories[t.category_id ?? ""] ?? t.category ?? "Category unknown");
  let detailsError = $state("");
  async function openDetails() {
    detailsError = "";
    try {
      const url = new URL(t.details_url ?? "");
      if (!['https:', 'http:'].includes(url.protocol)) throw new Error("Unsupported release page address");
      await openUrl(url.href);
    } catch (e) {
      detailsError = `Could not open release page: ${String(e)}`;
    }
  }
</script>

<div
  class="cv-row flex items-start gap-3 border-l-2 px-3 py-2.5 text-sm
    {t.remake ? 'border-red-400/70 bg-red-400/5' : t.trusted ? 'border-accent/70 bg-accent/5' : 'border-transparent'}
    {t.seen ? 'opacity-60' : ''}"
>
  <div class="flex-1 min-w-0">
    <div class="leading-relaxed whitespace-normal [overflow-wrap:anywhere]" title={t.title}>{t.title}</div>
    {#if detailsError}<p class="text-xs text-red-400 mt-1" role="alert">{detailsError}</p>{/if}
    <div class="flex flex-wrap items-center gap-x-3 gap-y-1 mt-1.5 text-xs text-ink-dim">
      {#if t.is_new}
        <span class="font-semibold uppercase text-accent">New</span>
      {/if}
      {#if t.episode != null}
        <span class="tabular-nums">Ep {t.episode}</span>
      {/if}
      <span
        class="rounded px-1.5 py-0.5 bg-panel-2 {t.category_id === '1_2' ? 'text-sky-300' : t.category_id === '1_3' ? 'text-violet-300' : ''}"
        title={category === "Category unknown" ? "The feed does not provide a category" : `Nyaa category: ${t.category ?? category}`}
      >{category}</span>
      {#if t.trusted}
        <span class="rounded px-1.5 py-0.5 bg-accent/15 text-accent" title="Uploaded by a trusted Nyaa user">Trusted</span>
      {/if}
      {#if t.remake}
        <span class="rounded px-1.5 py-0.5 bg-red-400/10 text-red-400" title="Marked as a remake on Nyaa, such as a re-encode or altered reupload">Remake</span>
      {/if}
      {#if t.size}
        <span>{t.size}</span>
      {/if}
      {#if t.seeders != null}
        <span class="text-accent tabular-nums" title="Seeders">↑{t.seeders}</span>
      {/if}
      {#if t.leechers != null}
        <span class="tabular-nums" title="Leechers">↓{t.leechers}</span>
      {/if}
      {#if t.published}
        <span class="whitespace-nowrap" title={new Date(t.published * 1000).toLocaleString()}>{timeAgo(t.published)}</span>
      {/if}
    </div>
  </div>
  <div class="flex items-center gap-1 shrink-0">
    {#if t.details_url}
      <button onclick={openDetails} title="Open release page" class="text-ink-dim hover:text-accent p-1.5 grid place-items-center">
        <Icon name="external" size={16} />
      </button>
    {/if}
    {#if t.magnet}
      <button
        onclick={() => onopen(t.magnet ?? t.link)}
        title="Open magnet in your torrent client"
        class="text-ink-dim hover:text-accent p-1.5 grid place-items-center"
      >
        <Icon name="magnet" size={16} />
      </button>
    {/if}
    <button
      onclick={() => onopen(t.link)}
      title="Download .torrent"
      class="text-ink-dim hover:text-ink p-1.5 grid place-items-center"
    >
      <Icon name="download" size={16} />
    </button>
  </div>
</div>
