<script lang="ts">
  import { untrack } from "svelte";
  import { goto } from "$app/navigation";
  import { openPath } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { library } from "$lib/library.svelte";
  import Select from "$lib/Select.svelte";
  import ScoreInput from "$lib/ScoreInput.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import { displayTitle, STATUS_LABEL, type ListEntry, type Media } from "$lib/types";

  let {
    entry,
    onclose,
    scoreFormat = null,
  }: { entry: ListEntry; onclose: () => void; scoreFormat?: string | null } = $props();

  // Snapshot the entry's current values once (untrack = read, don't subscribe).
  // The modal edits a copy; the source list is reloaded on close.
  const snap = untrack(() => ({
    status: entry.status,
    progress: entry.progress,
    score: entry.score ?? null,
    repeat: entry.repeat,
  }));
  let status = $state(untrack(() => entry.status));
  let progress = $state(untrack(() => entry.progress));
  let score = $state<number | null>(untrack(() => entry.score ?? null));
  let repeat = $state(untrack(() => entry.repeat));
  let saving = $state(false);
  let removing = $state(false);
  let err = $state("");

  // Community recommendations for this title; failures just hide the strip.
  let recs = $state<Media[]>([]);
  let addingRec = $state<number | null>(null);
  let addedRecs = $state<number[]>([]);

  const statusOptions = Object.entries(STATUS_LABEL).map(([value, label]) => ({
    value: value as ListEntry["status"],
    label,
  }));
  const total = $derived(entry.media?.episodes ?? null);
  // Next unwatched episode on disk (from the last library scan, if any).
  const nextFile = $derived(library.fileFor(entry.media_id, entry.progress + 1));
  const scoreUnit = $derived(
    scoreFormat === "POINT_3"
      ? "(smileys)"
      : scoreFormat === "POINT_5"
        ? "(1–5)"
        : scoreFormat === "POINT_10"
          ? "(0–10)"
          : scoreFormat === "POINT_10_DECIMAL"
            ? "(0–10)"
            : "(0–100)"
  );

  async function loadRecs() {
    try {
      recs = await api.getRecommendations(entry.media_id);
    } catch {
      recs = [];
    }
  }
  loadRecs();

  async function addRec(m: Media) {
    addingRec = m.id;
    try {
      // updateEntry writes status/progress unconditionally — only push when the
      // show isn't already on the list, or its entry would be reset.
      if (!(await api.getEntry(m.id))) {
        await api.updateEntry(m.id, "PLANNING", 0, null, 0);
      }
      addedRecs.push(m.id);
    } catch {
      // leave the button enabled so the user can retry
    } finally {
      addingRec = null;
    }
  }

  async function save() {
    if (saving || removing) return; // form still submits on Enter mid-save
    saving = true;
    err = "";
    try {
      // Merge with a FRESH read: fields the user didn't touch in the modal follow
      // the live entry, so saving can't rewind progress that advanced (the
      // auto-tracker, the row stepper) while the modal was open.
      const fresh = await api.getEntry(entry.media_id);
      await api.updateEntry(
        entry.media_id,
        status !== snap.status ? status : (fresh?.status ?? status),
        progress !== snap.progress ? progress : (fresh?.progress ?? progress),
        score !== snap.score ? score : (fresh?.score ?? null),
        repeat !== snap.repeat ? (repeat ?? snap.repeat) : (fresh?.repeat ?? snap.repeat)
      );
      onclose();
    } catch (e) {
      err = String(e);
    } finally {
      saving = false;
    }
  }

  async function remove() {
    if (removing || saving) return;
    removing = true;
    err = "";
    try {
      await api.deleteEntry(entry.media_id);
      onclose();
    } catch (e) {
      err = String(e);
    } finally {
      removing = false;
    }
  }

  // Modal behavior: Escape closes (Select swallows it first when its dropdown is
  // open), and the dialog takes focus so Tab stays inside the modal.
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
    <div class="flex items-start gap-3 mb-4">
      {#if entry.media?.cover_medium}
        <Img src={entry.media.cover_medium} class="w-12 h-16 object-cover rounded shrink-0" />
      {/if}
      <div class="min-w-0 flex-1">
        <button
          type="button"
          onclick={() => {
            onclose();
            goto(`/anime/${entry.media_id}`);
          }}
          title="Open details"
          class="block max-w-full font-semibold truncate text-left hover:text-accent transition-colors"
        >
          {displayTitle(entry.media)}
        </button>
        {#if total}
          <p class="text-xs text-ink-dim">{entry.progress}/{total} eps watched</p>
        {:else}
          <p class="text-xs text-ink-dim">{entry.progress} eps watched</p>
        {/if}
      </div>
      {#if nextFile}
        <button
          type="button"
          onclick={() => openPath(nextFile.path)}
          title={nextFile.path}
          class="px-2.5 py-1 rounded-md bg-accent hover:bg-accent-2 text-white text-xs shrink-0 flex items-center gap-1"
        >
          <Icon name="play" size={11} /> Play Ep {nextFile.episode}
        </button>
      {/if}
    </div>

    {#if err}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-3">
        {err}
      </div>
    {/if}

    <form
      onsubmit={(e) => {
        e.preventDefault();
        save();
      }}
      class="space-y-3"
    >
      <div>
        <label class="block text-sm mb-1" for="ed-status">Status</label>
        <Select id="ed-status" bind:value={status} options={statusOptions} />
      </div>

      <div class="flex gap-3">
        <div class="flex-1">
          <label class="block text-sm mb-1" for="ed-progress">Progress {#if total}<span class="text-ink-dim">/ {total}</span>{/if}</label>
          <input
            id="ed-progress"
            type="number"
            min="0"
            max={total ?? undefined}
            bind:value={progress}
            class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent"
          />
        </div>
        <div class="flex-1">
          <label class="block text-sm mb-1" for="ed-score">Score <span class="text-ink-dim">{scoreUnit}</span></label>
          <ScoreInput id="ed-score" bind:value={score} format={scoreFormat} />
        </div>
        <div class="w-24 shrink-0">
          <label class="block text-sm mb-1" for="ed-repeat" title="How many times you've finished this show">Rewatches</label>
          <input
            id="ed-repeat"
            type="number"
            min="0"
            bind:value={repeat}
            class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent"
          />
        </div>
      </div>

      <div class="flex items-center justify-between gap-2 pt-1">
        <button
          type="button"
          onclick={remove}
          disabled={removing || saving}
          class="px-3 py-1.5 rounded-md text-sm text-red-400/80 hover:text-red-400 hover:bg-red-500/10 disabled:opacity-40"
          title="Remove this series from your list"
        >
          {removing ? "Removing…" : "Remove from list"}
        </button>
        <div class="flex gap-2">
          <button
            type="button"
            onclick={onclose}
            class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={saving}
            class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
          >
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </form>

    {#if recs.length > 0}
      <div class="mt-5 pt-4 border-t border-edge">
        <h4 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">
          You might also like
        </h4>
        <div class="flex gap-2 overflow-x-auto pb-1">
          {#each recs as r (r.id)}
            <button
              type="button"
              onclick={() => addRec(r)}
              disabled={addingRec === r.id || addedRecs.includes(r.id)}
              title="{displayTitle(r)} — add to Plan to Watch"
              class="w-16 shrink-0 text-left group disabled:opacity-60"
            >
              {#if r.cover_medium}
                <Img src={r.cover_medium} class="w-16 h-[5.5rem] object-cover rounded" />
              {:else}
                <div class="w-16 h-[5.5rem] bg-panel-2 rounded"></div>
              {/if}
              <span class="block text-[11px] leading-tight line-clamp-2 mt-1 text-ink-dim group-hover:text-ink">
                {addedRecs.includes(r.id) ? "✓ Added" : displayTitle(r)}
              </span>
            </button>
          {/each}
        </div>
      </div>
    {/if}
  </div>
</div>
