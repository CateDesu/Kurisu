<script lang="ts">
  import { untrack } from "svelte";
  import { api } from "$lib/api";
  import Select from "$lib/Select.svelte";
  import ScoreInput from "$lib/ScoreInput.svelte";
  import { displayTitle, STATUS_LABEL, type ListEntry } from "$lib/types";

  let {
    entry,
    onclose,
    scoreFormat = null,
  }: { entry: ListEntry; onclose: () => void; scoreFormat?: string | null } = $props();

  // Snapshot the entry's current values once (untrack = read, don't subscribe).
  // The modal edits a copy; the source list is reloaded on close.
  let status = $state(untrack(() => entry.status));
  let progress = $state(untrack(() => entry.progress));
  let score = $state<number | null>(untrack(() => entry.score ?? null));
  let saving = $state(false);
  let removing = $state(false);
  let err = $state("");

  const statusOptions = Object.entries(STATUS_LABEL).map(([value, label]) => ({
    value: value as ListEntry["status"],
    label,
  }));
  const total = $derived(entry.media?.episodes ?? null);
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

  async function save() {
    saving = true;
    err = "";
    try {
      await api.updateEntry(entry.media_id, status, progress, score);
      onclose();
    } catch (e) {
      err = String(e);
    } finally {
      saving = false;
    }
  }

  async function remove() {
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
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div
  class="fixed inset-0 bg-black/60 grid place-items-center z-50 backdrop-blur-sm"
  onclick={onclose}
  role="presentation"
>
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="bg-panel border border-edge rounded-xl p-5 max-w-md w-full mx-4 shadow-2xl"
    onclick={(e) => e.stopPropagation()}
    role="dialog"
    aria-modal="true"
    tabindex="-1"
  >
    <div class="flex items-start gap-3 mb-4">
      {#if entry.media?.cover_medium}
        <img src={entry.media.cover_medium} alt="" class="w-12 h-16 object-cover rounded shrink-0" />
      {/if}
      <div class="min-w-0">
        <h3 class="font-semibold truncate">{displayTitle(entry.media)}</h3>
        {#if total}
          <p class="text-xs text-ink-dim">{entry.progress}/{total} eps watched</p>
        {:else}
          <p class="text-xs text-ink-dim">{entry.progress} eps watched</p>
        {/if}
      </div>
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
  </div>
</div>
