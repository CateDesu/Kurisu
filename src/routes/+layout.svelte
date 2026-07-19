<script lang="ts">
  import "../app.css";
  import { auth } from "$lib/auth.svelte";
  import { page as pageStore } from "$app/stores";
  import { get } from "svelte/store";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import TitleBar from "$lib/TitleBar.svelte";
  import Tracking from "$lib/Tracking.svelte";
  let { children } = $props();

  // Emoji icons render in color on every platform, so they stay legible on the
  // dark sidebar (the old text-glyph icons were nearly invisible).
  const nav = [
    { href: "/", label: "My List", icon: "📋" },
    { href: "/library", label: "Library", icon: "📁" },
    { href: "/seasons", label: "Seasons", icon: "🗓️" },
    { href: "/search", label: "Search", icon: "🔍" },
    { href: "/notifications", label: "Inbox", icon: "✉️" },
    { href: "/settings", label: "Settings", icon: "⚙️" },
  ];

  const appWindow = getCurrentWindow();

  /// Persistent back button: walks the in-app history. At the root the webview has
  /// nothing to go back to, so this is a no-op (the user's stated desired behavior).
  function back() {
    if (history.length > 1) history.back();
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
            class="w-8 h-8 grid place-items-center rounded-md text-ink-dim hover:text-ink hover:bg-panel-2/60 text-lg"
          >
            ←
          </button>
        </div>
        <nav class="flex-1 px-2 py-2 space-y-1 overflow-auto">
          {#each nav as item}
            {@const active = get(pageStore).url.pathname === item.href}
            <a
              href={item.href}
              class="flex items-center gap-3 px-3 py-2.5 rounded-md text-[15px] transition-colors
                {active ? 'bg-panel-2 text-ink' : 'text-ink-dim hover:text-ink hover:bg-panel-2/60'}"
            >
              <span class="text-lg w-6 text-center {active ? 'opacity-100' : 'opacity-90'}">{item.icon}</span>
              <span class="truncate">{item.label}</span>
            </a>
          {/each}
        </nav>
        {#if auth.user}
          <div class="px-4 py-4 border-t border-edge flex items-center gap-3.5">
            {#if auth.user.avatar}
              <img src={auth.user.avatar} alt="" class="w-12 h-12 rounded-full shrink-0 object-cover" />
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
              class="text-ink-dim hover:text-ink text-base px-1"
            >
              ⎋
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
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute right-0 inset-y-0 w-1.5 cursor-e-resize z-50" onpointerdown={() => resize("East")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 left-0 w-3 h-3 cursor-nw-resize z-50" onpointerdown={() => resize("NorthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute top-0 right-0 w-3 h-3 cursor-ne-resize z-50" onpointerdown={() => resize("NorthEast")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 left-0 w-3 h-3 cursor-sw-resize z-50" onpointerdown={() => resize("SouthWest")}></div>
  <!-- svelte-ignore a11y_no_static_element_interactions a11y_click_events_have_key_events -->
  <div class="absolute bottom-0 right-0 w-3 h-3 cursor-se-resize z-50" onpointerdown={() => resize("SouthEast")}></div>
</div>
