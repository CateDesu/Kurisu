<script lang="ts">
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import type { TrackingConfig, UpdateInfo } from "$lib/types";

  let cfg = $state<TrackingConfig>({ mode: "off", prompt_seconds: 120, auto_percent: 80 });
  let trackingLoaded = $state(false);
  let trackingSaving = $state(false);
  let trackingSavedAt = $state(0);

  let closeToTray = $state(false);

  // Auto-update defaults ON: only an explicit "0" turns it off.
  let autoUpdate = $state(true);
  let update = $state<UpdateInfo | null>(null);
  let updateChecking = $state(false);
  let updateInstalling = $state(false);
  let updateError = $state("");

  let signingIn = $state(false);
  let signInErr = $state("");

  const modes: Array<[TrackingConfig["mode"], string]> = [
    ["off", "Off — don't track playback"],
    ["prompt", "Prompt — ask me after a while"],
    ["auto", "Auto — update silently at X% watched"],
  ];

  async function load() {
    cfg = await api.getTrackingConfig();
    closeToTray = (await api.getAppSetting("close_to_tray")) === "1";
    autoUpdate = (await api.getAppSetting("auto_update")) !== "0";
    trackingLoaded = true;
  }
  async function signIn() {
    signingIn = true;
    signInErr = "";
    try {
      await auth.loginOauth();
    } catch (e) {
      signInErr = String(e);
    } finally {
      signingIn = false;
    }
  }
  async function saveTracking() {
    trackingSaving = true;
    try {
      cfg = await api.setTrackingConfig(cfg.mode, cfg.prompt_seconds, cfg.auto_percent);
      trackingSavedAt = Date.now();
    } finally {
      trackingSaving = false;
    }
  }
  async function toggleCloseToTray() {
    await api.setAppSetting("close_to_tray", closeToTray ? "1" : "0");
  }
  async function toggleAutoUpdate() {
    await api.setAppSetting("auto_update", autoUpdate ? "1" : "0");
  }
  async function checkForUpdate() {
    updateChecking = true;
    updateError = "";
    update = null;
    try {
      update = await api.checkUpdate();
    } catch (e) {
      updateError = String(e);
    } finally {
      updateChecking = false;
    }
  }
  async function installUpdate() {
    updateInstalling = true;
    updateError = "";
    try {
      // On success the installer has launched and the app quits itself.
      await api.installUpdate();
    } catch (e) {
      updateError = String(e);
      updateInstalling = false;
    }
  }
  load();
</script>

<div class="p-5 max-w-2xl mx-auto space-y-8">
  <div>
    <h1 class="text-xl font-semibold mb-1">Settings</h1>
  </div>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Account</h2>
    {#if auth.user}
      <p class="text-sm mb-2">Signed in as <b>{auth.user.name}</b>.</p>
      <button
        onclick={() => auth.logout()}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm"
      >
        Log out
      </button>
    {:else}
      <p class="text-sm text-ink-dim mb-3">Not signed in.</p>
      {#if signInErr}
        <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-3">
          {signInErr}
        </div>
      {/if}
      <button
        onclick={signIn}
        disabled={signingIn}
        class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
      >
        {signingIn ? "Connecting…" : "Sign in with AniList"}
      </button>
    {/if}
  </section>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Playback tracking</h2>
    <p class="text-sm text-ink-dim mb-3">
      Detect playback in MPV/VLC/Celluloid (any MPRIS2 player) and update your list.
    </p>
    <div class="space-y-2 mb-4">
      {#each modes as [val, label]}
        <label class="flex items-center gap-2 text-sm cursor-pointer">
          <input type="radio" name="tmode" value={val} bind:group={cfg.mode} class="accent-accent" />
          {label}
        </label>
      {/each}
    </div>
    {#if cfg.mode === "prompt"}
      <div class="mb-3 text-sm flex items-center gap-2">
        Ask after
        <input
          type="number"
          bind:value={cfg.prompt_seconds}
          min="1"
          max="3600"
          class="w-20 bg-panel border border-edge rounded-md px-2 py-1 focus:outline-none focus:border-accent"
        />
        seconds of playback
        <span class="text-ink-dim">({Math.round(cfg.prompt_seconds / 60)} min)</span>
      </div>
    {/if}
    {#if cfg.mode === "auto"}
      <div class="mb-3 text-sm flex items-center gap-2">
        Update progress at
        <input
          type="number"
          bind:value={cfg.auto_percent}
          min="1"
          max="100"
          class="w-20 bg-panel border border-edge rounded-md px-2 py-1 focus:outline-none focus:border-accent"
        />
        % watched
      </div>
    {/if}
    <div>
      <button
        disabled={!trackingLoaded || trackingSaving}
        onclick={saveTracking}
        class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
      >
        {trackingSaving ? "Saving…" : "Save tracking"}
      </button>
      {#if trackingSavedAt}
        <span class="text-xs text-accent ml-2">saved ✓</span>
      {/if}
    </div>
  </section>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Window</h2>
    <label class="flex items-center gap-2 text-sm cursor-pointer">
      <input
        type="checkbox"
        bind:checked={closeToTray}
        onchange={toggleCloseToTray}
        class="accent-accent"
      />
      Hide to system tray when closing the window
    </label>
    <p class="text-xs text-ink-dim mt-1">
      Off by default — the close button quits Kurisu outright. Turn this on to keep
      it running in the tray instead (Quit is always available in the tray menu).
    </p>
  </section>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Updates</h2>
    <label class="flex items-center gap-2 text-sm cursor-pointer">
      <input
        type="checkbox"
        bind:checked={autoUpdate}
        onchange={toggleAutoUpdate}
        class="accent-accent"
      />
      Automatically check for updates on startup
    </label>
    <p class="text-xs text-ink-dim mt-1 mb-3">
      On by default. Checks GitHub for a newer build and offers to install it.
    </p>
    <div class="flex items-center gap-2">
      <button
        onclick={checkForUpdate}
        disabled={updateChecking || updateInstalling}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {updateChecking ? "Checking…" : "Check for updates"}
      </button>
      {#if update?.available && update.can_install}
        <button
          onclick={installUpdate}
          disabled={updateInstalling}
          class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
        >
          {updateInstalling ? "Downloading…" : `Install ${update.version}`}
        </button>
      {/if}
    </div>
    {#if update}
      {#if update.available}
        <p class="text-xs text-accent mt-2">
          Version {update.version} is available (you're on {update.current}).
          {#if !update.can_install}
            <button
              onclick={() => openUrl(update!.html_url)}
              class="underline hover:text-accent-2 cursor-pointer"
            >
              Download it from GitHub
            </button>
          {/if}
        </p>
        {#if update.body}
          <pre class="text-xs text-ink-dim whitespace-pre-wrap max-h-32 overflow-y-auto bg-panel-2 border border-edge rounded-md p-2 mt-2">{update.body}</pre>
        {/if}
      {:else}
        <p class="text-xs text-ink-dim mt-2">Up to date — you're on {update.current}.</p>
      {/if}
    {/if}
    {#if updateError}
      <p class="text-xs text-red-400 mt-2">Update failed: {updateError}</p>
    {/if}
  </section>
</div>
