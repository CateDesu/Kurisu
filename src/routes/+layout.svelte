<script lang="ts">
  import "../app.css";
  import { auth } from "$lib/auth.svelte";
  import { page as pageStore } from "$app/stores";
  import { afterNavigate } from "$app/navigation";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { runClock } from "$lib/now.svelte";
  import TitleBar from "$lib/TitleBar.svelte";
  import Tracking from "$lib/Tracking.svelte";
  import Updater from "$lib/Updater.svelte";
  import Icon from "$lib/Icon.svelte";
  import Img from "$lib/Img.svelte";
  let { children } = $props();

  // Ticks the shared clock so relative labels (timeAgo, airingLabel) refresh.
  $effect(() => runClock());

  // Inline SVG icons (Icon.svelte) — stroke style, inherit text color.
  const nav = [
    { href: "/", label: "My List", icon: "list" },
    { href: "/library", label: "Library", icon: "folder" },
    { href: "/seasons", label: "Seasons", icon: "calendar" },
    { href: "/search", label: "Search", icon: "search" },
    { href: "/notifications", label: "Notifications", icon: "bell" },
    { href: "/settings", label: "Settings", icon: "sliders" },
  ];

  const appWindow = getCurrentWindow();

  /// Persistent back button: walks the in-app history. `history.length` counts the
  /// whole tab session (forward entries included), so we track our own navigation
  /// depth instead. At the root there's nothing to go back to → no-op (desired).
  let navDepth = $state(0);
  afterNavigate(({ type, to, from }) => {
    if (type === "popstate") navDepth = Math.max(0, navDepth - 1);
    // Clicking the current page's own link is a navigation that goes nowhere —
    // counting it would make Back return to the identical URL (looks dead).
    else if (type !== "enter" && to?.url.pathname !== from?.url.pathname) navDepth += 1;
  });
  function back() {
    if (navDepth > 0) history.back();
  }

  function openProfile() {
    if (auth.user) void openUrl(`https://anilist.co/user/${encodeURIComponent(auth.user.name)}`);
  }

  /// Begin a compositor resize gesture from a window edge/corner. Only works with
  /// custom decorations (which we use) — native GTK borders would handle it.
  function resize(direction: "East" | "North" | "NorthEast" | "NorthWest" | "South" | "SouthEast" | "SouthWest" | "West") {
    void appWindow.startResizeDragging(direction);
  }
</script>

<div class="relative flex flex-col h-screen border border-edge">
  <TitleBar />

  {#if !auth.ready}
    <div class="grid place-items-center flex-1 text-ink-dim">
      <div class="animate-pulse">Loading Kurisu…</div>
    </div>
  {:else}
    <Tracking />
    <Updater />
    <div class="flex flex-1 overflow-hidden">
      <aside class="w-56 shrink-0 border-r border-edge bg-panel flex flex-col">
        <div class="px-4 py-4 flex items-center gap-2">
          <div class="flex items-center gap-2.5 flex-1 min-w-0">
            <span class="text-accent text-3xl leading-none">ク</span>
            <span class="text-xl font-semibold tracking-wide truncate">Kurisu</span>
          </div>
          <!-- Persistent back; no-op when there's no history to return to. -->
          <button
            onclick={back}
            title="Back"
            aria-label="Back"
            class="w-8 h-8 grid place-items-center rounded-md text-ink-dim hover:text-ink hover:bg-panel-2/60"
          >
            <Icon name="back" />
          </button>
        </div>
        <nav class="flex-1 px-2 py-2 space-y-1 overflow-auto">
          {#each nav as item}
            {@const active = $pageStore.url.pathname === item.href}
            <a
              href={item.href}
              class="flex items-center gap-3 px-3 py-2.5 rounded-md text-[15px] transition-colors
                {active ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink hover:bg-panel-2/60'}"
            >
              <span class="w-6 grid place-items-center {active ? 'opacity-100' : 'opacity-90'}">
                <Icon name={item.icon} />
              </span>
              <span class="truncate">{item.label}</span>
            </a>
          {/each}
        </nav>
        {#if auth.user}
          <div class="px-4 py-4 border-t border-edge flex items-center gap-3.5">
            {#if auth.user.avatar}
              <Img src={auth.user.avatar} class="w-12 h-12 rounded-full shrink-0 object-cover" />
            {:else}
              <div class="w-12 h-12 rounded-full bg-panel-2 shrink-0"></div>
            {/if}
            <button
              onclick={openProfile}
              title="Open your AniList profile"
              class="flex-1 min-w-0 text-left transition-colors hover:opacity-90"
            >
              <div class="text-sm font-semibold text-ink truncate">{auth.user.name}</div>
              <div class="text-xs text-ink-dim truncate">View AniList</div>
            </button>
            <button
              onclick={() => auth.logout()}
              title="Log out"
              class="text-ink-dim hover:text-ink px-1 grid place-items-center"
            >
              <Icon name="logout" />
            </button>
          </div>
        {:else}
          <!-- signed out: keep the bar, just empty -->
          <div class="px-4 py-5 border-t border-edge text-[13px] text-ink-dim/50">Not signed in</div>
        {/if}
      </aside>
      <main class="flex-1 overflow-auto">
        {@render children?.()}
      </main>
      <!-- East resize grip, in-flow: as an absolute overlay it sat on top of
           main's scrollbar (same 6px at the right edge) and won every hit test.
           Taking layout space keeps both the grip and the scrollbar usable. -->
      <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
      <div class="w-1.5 shrink-0 cursor-e-resize" onpointerdown={() => resize("East")}></div>
    </div>
  {/if}

  <!-- Edge/corner resize grips (CSD). Thin overlays that show a resize cursor and
       hand the gesture to the compositor. -->
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 inset-x-0 h-1 cursor-n-resize z-50" onpointerdown={() => resize("North")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 inset-x-0 h-1.5 cursor-s-resize z-50" onpointerdown={() => resize("South")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute left-0 inset-y-0 w-1.5 cursor-w-resize z-50" onpointerdown={() => resize("West")}></div>
  <!-- (East grip lives in-flow after <main> — see above.) -->
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 left-0 w-2 h-2 cursor-nw-resize z-50" onpointerdown={() => resize("NorthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 right-0 w-2 h-2 cursor-ne-resize z-50" onpointerdown={() => resize("NorthEast")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 left-0 w-2 h-2 cursor-sw-resize z-50" onpointerdown={() => resize("SouthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 right-0 w-2 h-2 cursor-se-resize z-50" onpointerdown={() => resize("SouthEast")}></div>
</div>
