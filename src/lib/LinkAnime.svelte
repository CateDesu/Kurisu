<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
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
    /// Never bind a library root to one show.
    roots: string[];
    onclose: () => void;
    onlinked: () => void;
  } = $props();

  const sepIdx = $derived(Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\")));
  const fileName = $derived(sepIdx >= 0 ? path.slice(sepIdx + 1) : path);
  const dir = $derived(sepIdx > 0 ? path.slice(0, sepIdx) : "");
  const dirName = $derived(dir ? (dir.split(/[\\/]/).pop() ?? dir) : "");
  const folderAllowed = $derived(
    dir !== "" && !roots.some((r) => r.replace(/[\\/]+$/, "") === dir)
  );

  let scopeChoice = $state<"folder" | "file" | null>(null);
  const scope = $derived(
    scopeChoice === "folder" && !folderAllowed
      ? "file"
      : (scopeChoice ?? (folderAllowed ? "folder" : "file"))
  );
  const bindTarget = $derived(scope === "folder" ? dir : path);

  let q = $state("");
  let busy = $state<number | null>(null);
  let err = $state("");
  // Wait for the existing binding before allowing a pick that might replace it.
  let currentBinding = $state<number | null>(null);
  let currentTitle = $state("");
  let pendingRelink = $state<number | null>(null);
  let bindingChecked = $state(false);

  const STATUS_ORDER: Record<string, number> = {
    CURRENT: 0,
    REPEATING: 1,
    PLANNING: 2,
    PAUSED: 3,
    DROPPED: 4,
    COMPLETED: 5,
  };
  const COLLATOR = new Intl.Collator(undefined, { sensitivity: "base", numeric: true });
  const MAX_ROWS = 100;
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
        return COLLATOR.compare(displayTitle(a.media), displayTitle(b.media));
      })
      .slice(0, MAX_ROWS);
  });
  const truncated = $derived(entries.length > candidates.length && candidates.length === MAX_ROWS);

  $effect(() => {
    const target = bindTarget;
    currentBinding = null;
    currentTitle = "";
    pendingRelink = null;
    bindingChecked = false;
    let stale = false;
    api
      .getLibraryBinding(target)
      .then(async (id) => {
        if (stale || id === null) return;
        currentBinding = id;
        try {
          const media = await api.getMedia(id);
          if (!stale) currentTitle = displayTitle(media);
        } catch {
        }
      })
      .catch(() => {})
      .finally(() => {
        if (!stale) bindingChecked = true;
      });
    return () => {
      stale = true;
    };
  });

  async function pick(e: ListEntry) {
    if (busy !== null || !bindingChecked || e.media_id === currentBinding) return;
    if (currentBinding !== null && pendingRelink !== e.media_id) {
      pendingRelink = e.media_id;
      return;
    }
    busy = e.media_id;
    err = "";
    try {
      await api.bindLibraryPath(bindTarget, e.media_id);
      onlinked();
      onclose();
    } catch (ex) {
      err = String(ex);
    } finally {
      busy = null;
    }
  }

  function tryClose() {
    if (busy !== null) return;
    onclose();
  }
</script>

<Dialog onclose={tryClose} busy={busy !== null} size="md">
  <h3 class="font-semibold mb-1">Link to a show on your list</h3>
  <p class="text-xs text-ink-dim font-mono truncate mb-3" title={path}>{fileName}</p>

  {#if currentBinding !== null}
    <div
      class="text-sm text-amber-400 bg-amber-500/10 border border-amber-500/30 rounded-md p-2 mb-3"
    >
      {scope === "folder" ? "This folder" : "This file"} is linked to
      <span class="font-medium">{currentTitle || `show #${currentBinding}`}</span>. Picking a
      different show relinks it.
    </div>
  {/if}

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
          disabled={busy !== null || !bindingChecked || e.media_id === currentBinding}
          class="cv-row w-full text-left flex items-center gap-2.5 rounded-md p-1.5 hover:bg-panel-2/60 disabled:opacity-50"
        >
          {#if e.media?.cover_medium}
            <Img src={e.media.cover_medium} class="w-8 h-11 object-cover rounded shrink-0" />
          {:else}
            <div class="w-8 h-11 bg-panel-2 rounded shrink-0"></div>
          {/if}
          <span class="flex-1 min-w-0 truncate text-sm">
            {busy === e.media_id
              ? "Linking…"
              : pendingRelink === e.media_id
                ? `Relink to ${displayTitle(e.media)}`
                : displayTitle(e.media)}
          </span>
          <span class="shrink-0 text-xs text-ink-dim">
            {e.media_id === currentBinding ? "Linked" : (STATUS_LABEL[e.status] ?? e.status)}
          </span>
        </button>
      {/each}
      {#if truncated}
        <div class="text-xs text-ink-dim py-2 text-center">
          Showing the first {MAX_ROWS}. Type to narrow it down.
        </div>
      {/if}
    {/if}
  </div>

  <div class="flex justify-end pt-3">
    <button
      type="button"
      onclick={tryClose}
      class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
    >
      Cancel
    </button>
  </div>
</Dialog>
