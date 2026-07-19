<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
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

  let items = $state<Notification[]>([]);
  let loading = $state(true);
  let error = $state("");

  async function load() {
    loading = true;
    error = "";
    try {
      items = await api.getNotifications();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  async function open(n: Notification) {
    try {
      await openUrl(notificationUrl(n));
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    if (auth.isLoggedIn) load();
  });
</script>

{#if !auth.isLoggedIn}
  <div class="grid place-items-center min-h-full p-6">
    <Login />
  </div>
{:else}
  <div class="p-5 max-w-2xl mx-auto">
    <div class="flex items-center gap-3 mb-4">
      <h1 class="text-xl font-semibold flex-1">Notifications</h1>
      <button
        onclick={load}
        disabled={loading}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {loading ? "Loading…" : "↻ Refresh"}
      </button>
    </div>

    {#if error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
        {error}
      </div>
    {/if}

    {#if loading && items.length === 0}
      <div class="text-ink-dim py-10 text-center">Loading…</div>
    {:else if items.length === 0}
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
              <img src={n.user_avatar} alt="" loading="lazy" decoding="async" class="w-8 h-8 rounded-full shrink-0 object-cover" />
            {:else if n.media_cover}
              <img src={n.media_cover} alt="" loading="lazy" decoding="async" class="w-8 h-11 rounded shrink-0 object-cover" />
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
  </div>
{/if}
