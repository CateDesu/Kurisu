<script module lang="ts">
  import type { Media as MediaType } from "$lib/types";
  const RECS_CACHE = new Map<number, MediaType[]>();
</script>

<script lang="ts">
  import Dialog from "$lib/Dialog.svelte";
  import { onDestroy, untrack } from "svelte";
  import { goto } from "$app/navigation";
  import { openPath } from "@tauri-apps/plugin-opener";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { library } from "$lib/library.svelte";
  import Select from "$lib/Select.svelte";
  import ScoreInput from "$lib/ScoreInput.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import { displayTitle, STATUS_LABEL, type EntryDetails, type FuzzyDate, type ListEntry, type Media } from "$lib/types";

  let {
    entry,
    onclose,
    scoreFormat = null,
  }: { entry: ListEntry; onclose: () => void; scoreFormat?: string | null } = $props();

  // Edit a snapshot so list refreshes cannot overwrite pending inputs.
  const snap = untrack(() => ({
    status: entry.status,
    progress: entry.progress,
    score: entry.score ?? null,
    repeat: entry.repeat,
  }));
  const epoch = untrack(() => auth.epoch);
  let alive = true;
  const current = () => alive && epoch === auth.epoch && auth.isLoggedIn;
  onDestroy(() => { alive = false; });
  const scoreAvailable = $derived(!auth.offline || Boolean(scoreFormat));
  let status = $state(untrack(() => entry.status));
  let progress = $state(untrack(() => entry.progress));
  let score = $state<number | null>(untrack(() => entry.score ?? null));
  let repeat = $state(untrack(() => entry.repeat));
  let saving = $state(false);
  let removing = $state(false);
  let err = $state("");
  let confirmingRemove = $state(false);
  let removeTimer: ReturnType<typeof setTimeout> | null = null;

  let recs = $state<Media[]>([]);
  let addingRecs = $state(new Set<number>());
  let addedRecs = $state<number[]>([]);
  let recErr = $state("");

  let detailsOpen = $state(false);
  let details = $state<EntryDetails | null>(null);
  let detailsLoading = $state(false);
  let detailsError = $state("");
  let notes = $state("");
  let started = $state<FuzzyDate>({ year: null, month: null, day: null });
  let completed = $state<FuzzyDate>({ year: null, month: null, day: null });
  let customLists = $state<string[]>([]);
  const emptyDate = (): FuzzyDate => ({ year: null, month: null, day: null });
  const cleanDate = (date: FuzzyDate): FuzzyDate => ({ year: date.year ?? null, month: date.month ?? null, day: date.day ?? null });
  const sameDate = (left: FuzzyDate, right: FuzzyDate | null) => JSON.stringify(cleanDate(left)) === JSON.stringify(cleanDate(right ?? emptyDate()));

  function applyDetails(found: EntryDetails) {
    details = found;
    notes = found.notes;
    started = cleanDate(found.started_at ?? emptyDate());
    completed = cleanDate(found.completed_at ?? emptyDate());
    customLists = [...found.custom_lists];
  }

  async function loadDetails() {
    if (detailsLoading || !current()) return;
    detailsLoading = true;
    detailsError = "";
    try {
      const found = await api.getEntryDetails(entry.media_id);
      if (!current()) return;
      applyDetails(found);
    } catch (e) {
      if (current()) detailsError = String(e);
    } finally {
      if (current()) detailsLoading = false;
    }
  }

  const statusOptions = Object.entries(STATUS_LABEL).map(([value, label]) => ({
    value: value as ListEntry["status"],
    label,
  }));
  const total = $derived(entry.media?.episodes ?? null);
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
    const id = entry.media_id;
    const cached = RECS_CACHE.get(id);
    if (cached) {
      recs = cached;
      return;
    }
    try {
      const r = await api.getRecommendations(id);
      RECS_CACHE.set(id, r);
      if (entry.media_id === id) recs = r;
    } catch {
      if (entry.media_id === id) recs = [];
    }
  }
  loadRecs();

  async function addRec(m: Media) {
    if (!current() || addingRecs.has(m.id) || addedRecs.includes(m.id)) return;
    addingRecs = new Set(addingRecs).add(m.id);
    recErr = "";
    try {
      if (!(await api.getEntry(m.id))) {
        if (!current()) return;
        await api.updateEntry(m.id, "PLANNING", null, null, null);
      }
      if (!current()) return;
      addedRecs.push(m.id);
    } catch (e) {
      if (current()) recErr = String(e);
    } finally {
      const next = new Set(addingRecs);
      next.delete(m.id);
      addingRecs = next;
    }
  }

  function clampInputs() {
    if (progress != null) {
      progress = Math.max(0, total != null ? Math.min(progress, total) : progress);
    }
    if (repeat != null) repeat = Math.max(0, repeat);
  }

  async function save() {
    if (saving || removing || !current()) return;
    // Detect edits before clamping so untouched fields stay untouched.
    const progressTouched = progress !== snap.progress;
    const repeatTouched = repeat !== snap.repeat;
    clampInputs();
    const detailsTouched = details && (notes !== details.notes || !sameDate(started, details.started_at) || !sameDate(completed, details.completed_at) || JSON.stringify([...customLists].sort()) !== JSON.stringify([...details.custom_lists].sort()));
    const basicTouched = status !== snap.status || progressTouched || score !== snap.score || repeatTouched;
    if (!basicTouched && !detailsTouched) {
      if (current()) onclose();
      return;
    }
    saving = true;
    err = "";
    try {
      if (details && detailsTouched) {
        const saved = await api.updateEntryDetails(entry.media_id, notes !== details.notes ? notes : null, !sameDate(started, details.started_at) ? cleanDate(started) : null, !sameDate(completed, details.completed_at) ? cleanDate(completed) : null, JSON.stringify([...customLists].sort()) !== JSON.stringify([...details.custom_lists].sort()) ? [...customLists] : null);
        if (!current()) return;
        applyDetails(saved);
      }
      // Send only edited fields to preserve newer changes from tracking or other clients.
      if (basicTouched) await api.updateEntry(
        entry.media_id,
        status !== snap.status ? status : null,
        progressTouched ? (progress ?? snap.progress) : null,
        // Zero clears a score. Null leaves the remote score unchanged.
        scoreAvailable && score !== snap.score ? (score ?? 0) : null,
        repeatTouched ? (repeat ?? snap.repeat) : null
      );
      if (current()) onclose();
    } catch (e) {
      if (current()) err = String(e);
    } finally {
      saving = false;
    }
  }

  async function remove() {
    if (removing || saving || !current()) return;
    if (!confirmingRemove) {
      confirmingRemove = true;
      removeTimer = setTimeout(() => (confirmingRemove = false), 4000);
      return;
    }
    if (removeTimer) clearTimeout(removeTimer);
    confirmingRemove = false;
    removing = true;
    err = "";
    try {
      await api.deleteEntry(entry.media_id);
      if (current()) onclose();
    } catch (e) {
      if (current()) err = String(e);
    } finally {
      removing = false;
    }
  }

  $effect(() => () => {
    if (removeTimer) clearTimeout(removeTimer);
  });
</script>

<Dialog {onclose} busy={saving || removing} size="md">
  <div class="flex items-start gap-3 mb-4">
    {#if entry.media?.cover_medium}
      <Img src={entry.media.cover_medium} class="w-12 h-16 object-cover rounded shrink-0" />
    {/if}
    <div class="min-w-0 flex-1">
      <button
        type="button"
        disabled={saving || removing}
        onclick={() => {
          onclose();
          goto(`/anime/${entry.media_id}`);
        }}
        title="Open details"
        class="entry-title block max-w-full truncate text-left hover:text-accent transition-colors"
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
  >
    <fieldset disabled={saving || removing} class="space-y-3">
      <div>
        <label class="block text-sm mb-1" for="ed-status">Status</label>
        <Select
          id="ed-status"
          bind:value={status}
          options={statusOptions}
          onchange={(value) => { if (value === "COMPLETED" && total != null) progress = total; }}
        />
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
            onblur={clampInputs}
            class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent"
          />
        </div>
        <div class="flex-1">
          <label class="block text-sm mb-1" for="ed-score">Score <span class="text-ink-dim">{scoreUnit}</span></label>
          {#if scoreAvailable}
            <ScoreInput id="ed-score" bind:value={score} format={scoreFormat} />
          {:else}
            <p class="text-xs text-ink-dim">Reconnect to AniList to load your score format.</p>
          {/if}
        </div>
        <div class="w-24 shrink-0">
          <label class="block text-sm mb-1" for="ed-repeat" title="How many times you've finished this show">Rewatches</label>
          <input
            id="ed-repeat"
            type="number"
            min="0"
            max="1000"
            bind:value={repeat}
            onblur={clampInputs}
            class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent"
          />
        </div>
      </div>

      <div class="border-t border-edge pt-3">
        <button type="button" class="text-sm text-accent" aria-expanded={detailsOpen} onclick={() => { detailsOpen = !detailsOpen; if (detailsOpen && !details) void loadDetails(); }}>Notes, viewing dates and custom lists {detailsOpen ? "▴" : "▾"}</button>
        {#if detailsOpen}
          {#if detailsLoading}<p class="text-xs text-ink-dim mt-2">Loading entry details…</p>{/if}
          {#if detailsError}
            <p class="text-xs text-red-400 mt-2">Entry details are unavailable. {detailsError} <button type="button" onclick={loadDetails} disabled={detailsLoading} class="text-accent">Retry entry details</button></p>
          {/if}
          {#if details}
            {#if details.warning}<p class="text-xs text-ink-dim mt-2">Showing saved entry details. Reconnect before saving changes. {details.warning}</p>{/if}
            <label for="ed-notes" class="block text-sm mt-3 mb-1">Notes</label>
            <textarea id="ed-notes" bind:value={notes} maxlength="6000" rows="3" class="w-full bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm"></textarea>
            <p class="text-xs text-ink-dim mb-2">Notes follow your AniList list privacy settings.</p>
            {#each [{ label: "Started", date: started }, { label: "Completed", date: completed }] as item (item.label)}
              <div class="mt-2">
                <span class="block text-sm mb-1">{item.label} viewing</span>
                <div class="flex gap-2">
                  <input type="number" min="1" max="9999" placeholder="Year" aria-label={`${item.label} year`} bind:value={item.date.year} class="w-1/3 bg-panel-2 border border-edge rounded-md p-2 text-sm" />
                  <input type="number" min="1" max="12" placeholder="Month" aria-label={`${item.label} month`} bind:value={item.date.month} class="w-1/3 bg-panel-2 border border-edge rounded-md p-2 text-sm" />
                  <input type="number" min="1" max="31" placeholder="Day" aria-label={`${item.label} day`} bind:value={item.date.day} class="w-1/3 bg-panel-2 border border-edge rounded-md p-2 text-sm" />
                </div>
              </div>
            {/each}
            <p class="text-xs text-ink-dim mt-1">Leave unknown date parts blank.</p>
            {#if details.available_custom_lists.length}
              <div class="text-sm mt-3 mb-1">Custom lists</div>
              <div class="flex flex-wrap gap-x-4 gap-y-2">
                {#each details.available_custom_lists as name (name)}
                  <label class="flex gap-2 items-center text-sm"><input type="checkbox" value={name} bind:group={customLists} />{name}</label>
                {/each}
              </div>
            {:else}
              <p class="text-xs text-ink-dim mt-3">Create custom list names in your AniList list settings to assign them here.</p>
            {/if}
          {/if}
        {/if}
      </div>

      <div class="flex items-center justify-between gap-2 pt-1">
        <button
          type="button"
          onclick={remove}
          disabled={removing || saving}
          class={confirmingRemove
            ? "px-3 py-1.5 rounded-md text-sm bg-red-500/20 text-red-300 hover:bg-red-500/30 disabled:opacity-40"
            : "px-3 py-1.5 rounded-md text-sm text-red-400/80 hover:text-red-400 hover:bg-red-500/10 disabled:opacity-40"}
          title="Remove this series from your list"
        >
          {removing ? "Removing…" : confirmingRemove ? "Confirm remove" : "Remove from list"}
        </button>
        <div class="flex gap-2">
          <button
            type="button"
            onclick={onclose}
            disabled={saving || removing}
            class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
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
    </fieldset>
  </form>

  {#if recs.length > 0}
    <div class="mt-5 pt-4 border-t border-edge">
      <h4 class="text-xs font-semibold uppercase tracking-wide text-ink-dim mb-2">
        You might also like
      </h4>
      {#if recErr}
        <div class="text-xs text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-2">
          {recErr}
        </div>
      {/if}
      <div class="flex gap-2 overflow-x-auto pb-1">
        {#each recs as r (r.id)}
          <button
            type="button"
            onclick={() => addRec(r)}
            disabled={addingRecs.has(r.id) || addedRecs.includes(r.id)}
            title="{displayTitle(r)} — add to Plan to Watch"
            class="w-24 shrink-0 self-start text-left group disabled:opacity-60"
          >
            {#if r.cover_medium}
              <Img src={r.cover_medium} class="w-24 h-32 object-cover rounded" />
            {:else}
              <div class="w-24 h-32 bg-panel-2 rounded"></div>
            {/if}
            <span class="text-xs leading-tight line-clamp-2 mt-1 text-ink-dim group-hover:text-ink">
              {addedRecs.includes(r.id) ? "✓ Added" : displayTitle(r)}
            </span>
          </button>
        {/each}
      </div>
    </div>
  {/if}
</Dialog>
