<script lang="ts">
  import "../app.css";
  import { auth } from "$lib/auth.svelte";
  import { page as pageStore } from "$app/stores";
  import { afterNavigate } from "$app/navigation";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { runClock } from "$lib/now.svelte";
  import { bindNowPlaying } from "$lib/nowplaying.svelte";
  import TitleBar from "$lib/TitleBar.svelte";
  import Tracking from "$lib/Tracking.svelte";
  import Updater from "$lib/Updater.svelte";
  import Confirm from "$lib/Confirm.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  import Gearwork from "$lib/Gearwork.svelte";
  let { children } = $props();

  $effect(() => runClock());

  $effect(() => {
    void bindNowPlaying();
  });

  $effect(() => {
    const onOnline = () => void auth.refresh();
    window.addEventListener("online", onOnline);
    return () => window.removeEventListener("online", onOnline);
  });

  $effect(() => {
    if (!auth.offline) return;
    const retry = setInterval(() => void auth.refresh(), 30_000);
    return () => clearInterval(retry);
  });

  let logoutErr = $state("");
  let confirmingLogout = $state(false);
  let loggingOut = $state(false);

  $effect(() => {
    if (!auth.user) confirmingLogout = false;
  });

  async function doLogout() {
    if (loggingOut) return;
    loggingOut = true;
    logoutErr = "";
    try {
      await auth.logout();
      confirmingLogout = false;
    } catch (e) {
      logoutErr = String(e);
      confirmingLogout = false;
    } finally {
      loggingOut = false;
    }
  }

  const nav = [
    { href: "/", label: "My List", icon: "list" },
    { href: "/now", label: "Currently Watching", icon: "play" },
    { href: "/library", label: "Library", icon: "folder" },
    { href: "/torrents", label: "Torrents", icon: "download" },
    { href: "/seasons", label: "Seasons", icon: "sun" },
    { href: "/calendar", label: "Calendar", icon: "calendar" },
    { href: "/search", label: "Search", icon: "search" },
    { href: "/stats", label: "Stats", icon: "chart" },
    { href: "/notifications", label: "Notifications", icon: "bell" },
    { href: "/settings", label: "Settings", icon: "sliders" },
  ];

  const appWindow = getCurrentWindow();

  /// Track program navigation because browser history includes the whole tab session.
  let navDepth = $state(0);
  afterNavigate(({ type, to, from }) => {
    if (type === "popstate") navDepth = Math.max(0, navDepth - 1);
    else if (type !== "enter" && to?.url.pathname !== from?.url.pathname) navDepth += 1;
  });
  function back() {
    if (navDepth > 0) history.back();
  }

  function openProfile() {
    if (auth.user) void openUrl(`https://anilist.co/user/${encodeURIComponent(auth.user.name)}`);
  }

  function resize(direction: "East" | "North" | "NorthEast" | "NorthWest" | "South" | "SouthEast" | "SouthWest" | "West") {
    void appWindow.startResizeDragging(direction);
  }
</script>

<div class="program-shell relative flex flex-col h-screen border border-edge">
  <TitleBar />

  {#if !auth.ready}
    <div class="grid place-items-center flex-1 text-ink-dim">
      <div class="animate-pulse">Loading Kurisu…</div>
    </div>
  {:else}
    <Tracking />
    <Updater />
    <div class="flex flex-1 overflow-hidden">
      <aside class="sidebar shrink-0 border-r border-edge flex flex-col">
        <div class="brand-block">
          <div class="eyebrow brand-caption">アニメの記録 <span>クリス</span></div>
          <div class="wordmark" aria-label="Kurisu">
            <span class="wordmark-k" aria-hidden="true">K</span><span aria-hidden="true">urisu<span class="wordmark-dot">.</span></span>
          </div>
          <button
            onclick={back}
            title="Back"
            aria-label="Back"
            class="back-button w-7 h-7 grid place-items-center text-ink-dim hover:text-ink hover:bg-panel-2/60"
          >
            <Icon name="back" />
          </button>
        </div>
        <nav class="sidebar-nav flex-1 overflow-auto" aria-label="Main navigation">
          {#each nav as item, index}
            {#if index === 0 || index === 4 || index === 8}
              <div class="nav-heading eyebrow">{index === 0 ? 'Your shelf' : index === 4 ? 'Discover' : 'Program'}</div>
            {/if}
            {@const active = $pageStore.url.pathname === item.href}
            <a
              href={item.href}
              aria-current={active ? 'page' : undefined}
              class="nav-link {active ? 'active' : ''}"
            >
              <span class="w-6 grid place-items-center {active ? 'opacity-100' : 'opacity-90'}">
                <Icon name={item.icon} />
              </span>
              <span class="truncate">{item.label}</span>
            </a>
          {/each}
        </nav>
        <div class="sidebar-drawing" aria-hidden="true"><Gearwork /></div>
        {#if auth.user}
          <div class="profile-block px-4 py-4 border-t border-edge flex items-center gap-3">
            {#if auth.user.avatar}
              <Img src={auth.user.avatar} class="w-9 h-9 border border-edge shrink-0 object-cover" />
            {:else}
              <div class="w-9 h-9 border border-edge bg-panel-2 shrink-0"></div>
            {/if}
            <button
              onclick={openProfile}
              title="Open your AniList profile"
              class="flex-1 min-w-0 text-left transition-colors hover:opacity-90"
            >
              <div class="text-sm font-semibold text-ink truncate">{auth.user.name}</div>
              <div class="text-xs text-ink-dim truncate">AniList profile ↗</div>
            </button>
            <button
              onclick={() => (confirmingLogout = true)}
              title="Log out"
              class="text-ink-dim hover:text-ink px-1 grid place-items-center"
            >
              <Icon name="logout" />
            </button>
          </div>
        {:else}
          <div class="px-4 py-5 border-t border-edge text-sm text-ink-dim">
            Not signed in
            {#if logoutErr}
              <p class="text-red-400 mt-1">Log out failed: {logoutErr}</p>
            {/if}
          </div>
        {/if}
      </aside>
      <main class="flex-1 min-w-0 overflow-auto">
        {@render children?.()}
      </main>
      <!-- Keep the resize grip in flow so it cannot cover the scrollbar. -->
      <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
      <div class="w-1.5 shrink-0 cursor-e-resize" onpointerdown={() => resize("East")}></div>
    </div>
  {/if}

  {#if confirmingLogout}
    <Confirm
      title="Log out?"
      body="Removes your token and clears the cached list. Everything syncs back on your next login."
      confirmLabel="Log out"
      busy={loggingOut}
      onconfirm={doLogout}
      oncancel={() => (confirmingLogout = false)}
    />
  {/if}
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 inset-x-0 h-1 cursor-n-resize z-50" onpointerdown={() => resize("North")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 inset-x-0 h-1.5 cursor-s-resize z-50" onpointerdown={() => resize("South")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute left-0 inset-y-0 w-1.5 cursor-w-resize z-50" onpointerdown={() => resize("West")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 left-0 w-2 h-2 cursor-nw-resize z-50" onpointerdown={() => resize("NorthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 right-0 w-2 h-2 cursor-ne-resize z-50" onpointerdown={() => resize("NorthEast")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 left-0 w-2 h-2 cursor-sw-resize z-50" onpointerdown={() => resize("SouthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 right-0 w-2 h-2 cursor-se-resize z-50" onpointerdown={() => resize("SouthEast")}></div>
</div>

<style>
  .sidebar { width: 244px; background: #040303; }
  .brand-block { position: relative; padding: 25px 22px 23px; border-bottom: 1px solid var(--color-edge); }
  .brand-caption { display: flex; justify-content: space-between; letter-spacing: .14em; font-size: 14px; }
  .brand-caption span { color: var(--color-accent); }
  .wordmark { display: flex; align-items: baseline; margin-top: 15px; font: 46px/.95 var(--font-serif); letter-spacing: -3px; color: var(--color-ink); }
  .wordmark-k { display: inline-block; padding: 0 4px 4px; margin-right: 1px; background: var(--color-ink); color: var(--color-base); transform: rotate(-3deg); }
  .wordmark-dot { color: var(--color-accent); }
  .back-button { position: absolute; right: 14px; bottom: 15px; }
  .sidebar-nav { padding: 7px 12px 12px; }
  .nav-heading { padding: 18px 12px 9px; font-size: 11px; color: var(--color-ink-dim); }
  .nav-link { display: flex; align-items: center; gap: 8px; position: relative; padding: 9px 9px; margin-bottom: 2px; color: var(--color-ink-dim); font-size: 15px; line-height: 1.35; border: 1px solid transparent; }
  .nav-link:hover { color: var(--color-ink); background: var(--color-panel-2); }
  .nav-link.active { color: var(--color-ink); background: #160d08; border-color: #82543d; }
  .nav-link.active::before { content: ''; position: absolute; left: -1px; top: 7px; bottom: 7px; width: 2px; background: var(--color-accent); }
  .nav-link.active :global(svg) { color: var(--color-accent); }
  .sidebar-drawing { position: relative; height: 130px; margin: 0 18px 14px; overflow: hidden; flex-shrink: 1; min-height: 0; }
  .sidebar-drawing :global(svg) { width: 210px; position: absolute; top: -8px; left: -5px; opacity: .27; }
  @media (max-height: 940px) { .sidebar-drawing { display: none; } }
  @media (max-height: 800px) {
    .nav-heading { padding-top: 9px; padding-bottom: 5px; }
    .nav-link { padding-top: 6px; padding-bottom: 6px; }
  }
  @media (max-height: 700px) {
    .brand-block { padding-top: 12px; padding-bottom: 12px; }
    .wordmark { font-size: 36px; margin-top: 8px; }
    .nav-heading { padding-top: 6px; padding-bottom: 3px; }
    .nav-link { padding-top: 2px; padding-bottom: 2px; }
  }
  @media (max-width: 960px) {
    .sidebar { width: 216px; }
    .brand-block { padding-left: 17px; padding-right: 17px; }
    .nav-link { padding-left: 5px; padding-right: 5px; gap: 5px; }
  }
</style>
