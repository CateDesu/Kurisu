<script lang="ts">
  // Modal for the Library's unmatched files: pick a show on your list and bind
  // this file (or its whole folder) to it. The binding is stored backend-side
  // and wins over the recognizer on every future scan.
  import { api } from "$lib/api";
  import { displayTitle, STATUS_LABEL, type ListEntry } from "$lib/types";
  import Img from "$lib/Img.svelte";

  let {
    path,
    entries,
    roots,
    onclose,
    onlinked,
  }: {
    path: string;
    entries: ListEntry[];
    /// Configured library root folders — a root is never offered for folder
    /// binding (it would swallow every unmatched file in the library).
    roots: string[];
    onclose: () => void;
    onlinked: () => void;
  } = $props();

  const sepIdx = $derived(Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\")));
  const fileName = $derived(sepIdx >= 0 ? path.slice(sepIdx + 1) : path);
  const dir = $derived(sepIdx > 0 ? path.slice(0, sepIdx) : "");
  const dirName = $derived(dir ? (dir.split(/[\\/]/).pop() ?? dir) : "");
  // Trailing separators on configured roots must not defeat the root check.
  const folderAllowed = $derived(
    dir !== "" && !roots.some((r) => r.replace(/[\\/]+$/, "") === dir)
  );

  // The user's radio pick, if any; until then follow what the path allows.
  let scopeChoice = $state<"folder" | "file" | null>(null);
  const scope = $derived(
    scopeChoice === "folder" && !folderAllowed
      ? "file"
      : (scopeChoice ?? (folderAllowed ? "folder" : "file"))
  );
  let q = $state("");
  let busy = $state<number | null>(null);
  let err = $state("");

  // Watching first — those are the likely targets — then the rest, title order.
  const STATUS_ORDER: Record<string, number> = {
    CURRENT: 0,
    REPEATING: 1,
    PLANNING: 2,
    PAUSED: 3,
    DROPPED: 4,
    COMPLETED: 5,
  };
  const candidates = $derived.by(() => {
    const needle = q.trim().toLowerCase();
    return entries
      .filter((e) => {
        if (!needle) return true;
        const m = e.media;
        return [m?.title_english, m?.title_romaji, m?.title_native].some((t) =>
          t?.toLowerCase().includes(needle)
        );
      })
      .sort((a, b) => {
        const s = (STATUS_ORDER[a.status] ?? 9) - (STATUS_ORDER[b.status] ?? 9);
        if (s !== 0) return s;
        return displayTitle(a.media).localeCompare(displayTitle(b.media), undefined, {
          sensitivity: "base",
          numeric: true,
        });
      });
  });

  async function pick(e: ListEntry) {
    if (busy !== null) return;
    busy = e.media_id;
    err = "";
    try {
      await api.bindLibraryPath(scope === "folder" ? dir : path, e.media_id);
      onlinked();
      onclose();
    } catch (ex) {
      err = String(ex);
    } finally {
      busy = null;
    }
  }

  let dialog = $state<HTMLDivElement | null>(null);
  function onWindowKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onclose();
  }
  $effect(() => dialog?.focus());
</script>

<svelte:window onkeydown={onWindowKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
  onclick={onclose}
  role="presentation"
>
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    bind:this={dialog}
    class="bg-panel border border-edge rounded-xl p-5 max-w-md w-full mx-4 shadow-2xl"
    onclick={(e) => e.stopPropagation()}
    role="dialog"
    aria-modal="true"
    tabindex="-1"
  >
    <h3 class="font-semibold mb-1">Link to a show on your list</h3>
    <p class="text-xs text-ink-dim font-mono truncate mb-3" title={path}>{fileName}</p>

    {#if err}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-3">
        {err}
      </div>
    {/if}

    <div class="flex gap-4 mb-3 text-sm">
      {#if folderAllowed}
        <label class="flex items-center gap-1.5 cursor-pointer min-w-0">
          <input
            type="radio"
            name="link-scope"
            checked={scope === "folder"}
            onchange={() => (scopeChoice = "folder")}
            class="accent-accent"
          />
          <span class="truncate" title={dir}>Whole folder <span class="text-ink-dim">({dirName})</span></span>
        </label>
      {/if}
      <label class="flex items-center gap-1.5 cursor-pointer shrink-0">
        <input
          type="radio"
          name="link-scope"
          checked={scope === "file"}
          onchange={() => (scopeChoice = "file")}
          class="accent-accent"
        />
        <span>This file only</span>
      </label>
    </div>

    <input
      bind:value={q}
      placeholder="Search your list…"
      class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent mb-2"
    />

    <div class="max-h-72 overflow-y-auto space-y-1 -mx-1 px-1">
      {#if candidates.length === 0}
        <div class="text-sm text-ink-dim py-6 text-center">No matches on your list.</div>
      {:else}
        {#each candidates as e (e.media_id)}
          <button
            type="button"
            onclick={() => pick(e)}
            disabled={busy !== null}
            class="w-full text-left flex items-center gap-2.5 rounded-md p-1.5 hover:bg-panel-2/60 disabled:opacity-50"
          >
            {#if e.media?.cover_medium}
              <Img src={e.media.cover_medium} class="w-8 h-11 object-cover rounded shrink-0" />
            {:else}
              <div class="w-8 h-11 bg-panel-2 rounded shrink-0"></div>
            {/if}
            <span class="flex-1 min-w-0 truncate text-sm">
              {busy === e.media_id ? "Linking…" : displayTitle(e.media)}
            </span>
            <span class="shrink-0 text-xs text-ink-dim">{STATUS_LABEL[e.status] ?? e.status}</span>
          </button>
        {/each}
      {/if}
    </div>

    <div class="flex justify-end pt-3">
      <button
        type="button"
        onclick={onclose}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
      >
        Cancel
      </button>
    </div>
  </div>
</div>
