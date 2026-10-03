<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { goto } from "$app/navigation";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import {
    notificationIcon,
    notificationText,
    notificationUrl,
    timeAgo,
    type Notification,
  } from "$lib/types";
  import Login from "$lib/Login.svelte";
  import Img from "$lib/Img.svelte";

  let items = $state<Notification[]>([]);
  let loading = $state(true);
  let error = $state("");
  let unread = $state<number | null>(null);
  let resultPage = $state(0);
  let hasMore = $state(false);
  let marking = $state(false);

  let loadId = 0;
  let lastEpoch = -1;
  async function load(more = false) {
    if (!auth.isLoggedIn) return;
    const id = ++loadId;
    const epoch = auth.epoch;
    const current = () => id === loadId && epoch === auth.epoch && auth.isLoggedIn;
    loading = true;
    error = "";
    try {
      const page = await api.getNotificationsPage(more ? resultPage + 1 : 1);
      if (!current()) return;
      const merged = new Map((more ? items : []).map((item) => [item.id, item]));
      for (const item of page.items) merged.set(item.id, item);
      items = [...merged.values()];
      resultPage = page.page;
      hasMore = page.has_next_page;
      unread = page.unread_count;
    } catch (e) {
      if (current()) error = String(e);
    } finally {
      if (current()) loading = false;
    }
  }

  async function markRead() {
    if (marking || loading || !auth.isLoggedIn) return;
    const epoch = auth.epoch;
    marking = true;
    error = "";
    try {
      await api.markNotificationsRead();
      if (epoch === auth.epoch && auth.isLoggedIn) unread = 0;
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    } finally {
      if (epoch === auth.epoch) marking = false;
    }
  }

  async function open(n: Notification) {
    const epoch = auth.epoch;
    // Activity, thread and profile pages need the browser's AniList session cookie.
    if (n.media_id) {
      goto(`/anime/${n.media_id}`);
      return;
    }
    try {
      await openUrl(notificationUrl(n));
    } catch (e) {
      if (epoch === auth.epoch && auth.isLoggedIn) error = String(e);
    }
  }

  $effect(() => {
    const epoch = auth.epoch;
    if (epoch !== lastEpoch || !auth.isLoggedIn) {
      items = [];
      error = "";
      loading = false;
      lastEpoch = epoch;
      unread = null;
      resultPage = 0;
      hasMore = false;
      marking = false;
    }
    if (auth.isLoggedIn) load();
    return () => { loadId++; };
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="page-content">
    <PageHeading section="AniList updates" title="Notifications" description="The latest from your account">
      <button
        onclick={() => load()}
        disabled={loading || marking}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {loading ? "Loading…" : "↻ Refresh"}
      </button>
      <button onclick={markRead} disabled={loading || marking || unread === 0} class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50">
        {marking ? "Marking…" : "Mark all read"}
      </button>
    </PageHeading>
    {#if unread !== null}
      <p class="text-sm text-ink-dim mb-4">{unread} unread on AniList</p>
    {/if}

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if loading && items.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if items.length === 0 && !error}
      <div class="text-ink-dim py-10 text-center">No notifications.</div>
    {:else}
      <div class="grid grid-cols-1 gap-1.5">
        {#each items as n (n.id)}
          <button
            onclick={() => open(n)}
            class="cv-row w-full text-left flex items-start gap-3 bg-panel border border-edge rounded-lg p-3 hover:bg-panel-2/60 transition-colors"
          >
            <span class="text-lg leading-none shrink-0 mt-0.5">{notificationIcon(n.kind)}</span>
            {#if n.user_avatar}
              <Img src={n.user_avatar} class="w-8 h-8 rounded-full shrink-0 object-cover" />
            {:else if n.media_cover}
              <Img src={n.media_cover} class="w-8 h-11 rounded shrink-0 object-cover" />
            {/if}
            <div class="flex-1 min-w-0">
              <div class="text-sm leading-snug">{notificationText(n)}</div>
              {#if n.reason}
                <div class="text-xs text-ink-dim mt-0.5">{n.reason}</div>
              {/if}
              <div class="text-xs text-ink-dim/70 mt-1">{timeAgo(n.created_at)}</div>
            </div>
          </button>
        {/each}
      </div>
    {/if}
    {#if hasMore}
      <div class="text-center mt-5">
        <button onclick={() => load(true)} disabled={loading || marking} class="px-4 py-2 rounded-md bg-panel-2 hover:bg-edge disabled:opacity-50">{loading ? "Loading…" : "Load older notifications"}</button>
      </div>
    {/if}
  </div>
{/if}
