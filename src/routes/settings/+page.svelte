<script lang="ts">
  import PageHeading from "$lib/PageHeading.svelte";
  import { api } from "$lib/api";
  import { auth } from "$lib/auth.svelte";
  import { installInFlight, notePendingRestart, runInstallUpdate, updateNeedsRestart } from "$lib/update.svelte";
  import Confirm from "$lib/Confirm.svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import type { TrackingConfig, UpdateInfo } from "$lib/types";

  let cfg = $state<TrackingConfig>({ mode: "off", prompt_seconds: 120, auto_percent: 80, auto_ask: true, mpv_ipc_socket: "", discord_enabled: true });
  let trackingLoaded = $state(false);
  let trackingSaving = $state(false);
  let trackingSavedAt = $state(0);
  let trackingError = $state("");

  let closeToTray = $state(false);
  const isLinux = typeof navigator !== "undefined" && navigator.userAgent.includes("Linux");
  let hardwareAcceleration = $state(false);
  let renderingSaved = $state(false);

  let autoUpdate = $state(true);
  type ToggleKey = "close_to_tray" | "auto_update" | "hardware_acceleration";
  const toggles = $state({
    close_to_tray: { loaded: false, loading: false, saving: false, error: "" },
    auto_update: { loaded: false, loading: false, saving: false, error: "" },
    hardware_acceleration: { loaded: false, loading: false, saving: false, error: "" },
  });
  let update = $state<UpdateInfo | null>(null);
  let updateChecking = $state(false);
  let updateError = $state("");
  let updateStatus = $state("");
  let loadError = $state("");

  let signingIn = $state(false);
  let signInErr = $state("");
  let confirmingLogout = $state(false);
  let loggingOut = $state(false);

  const modes: Array<[TrackingConfig["mode"], string]> = [
    ["off", "Off — don't track playback"],
    ["prompt", "Prompt — ask me after a while"],
    ["auto", "Auto — update silently at X% watched"],
  ];

  async function load() {
    try {
      cfg = await api.getTrackingConfig();
      trackingLoaded = true;
    } catch (e) {
      loadError = String(e);
    }
  }

  async function loadToggle(key: ToggleKey) {
    const state = toggles[key];
    if (state.loading || state.loaded) return;
    state.loading = true;
    state.error = "";
    try {
      const value = await api.getAppSetting(key);
      if (key === "close_to_tray") closeToTray = value === "1";
      else if (key === "auto_update") autoUpdate = value !== "0";
      else hardwareAcceleration = value === "1";
      state.loaded = true;
    } catch (e) {
      state.error = String(e);
    } finally {
      state.loading = false;
    }
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
  async function doLogout() {
    if (loggingOut) return;
    loggingOut = true;
    signInErr = "";
    try {
      await auth.logout();
      confirmingLogout = false;
    } catch (e) {
      signInErr = String(e);
      confirmingLogout = false;
    } finally {
      loggingOut = false;
    }
  }
  $effect(() => {
    if (!auth.user) confirmingLogout = false;
  });
  async function saveTracking() {
    if (!trackingLoaded || trackingSaving) return;
    trackingSaving = true;
    trackingError = "";
    // Normalize empty and fractional inputs before sending Rust integer parameters.
    const int = (v: unknown, fallback: number, lo: number, hi: number) => {
      if (v == null || v === "") return fallback;
      const n = Math.round(Number(v));
      if (!Number.isFinite(n)) return fallback;
      return Math.min(hi, Math.max(lo, n));
    };
    const snap = {
      mode: cfg.mode,
      prompt_seconds: int(cfg.prompt_seconds, 120, 1, 3_600),
      auto_percent: int(cfg.auto_percent, 80, 1, 100),
      auto_ask: cfg.auto_ask,
      mpv_ipc_socket: cfg.mpv_ipc_socket.trim().slice(0, 512),
      discord_enabled: cfg.discord_enabled,
    };
    cfg.prompt_seconds = snap.prompt_seconds;
    cfg.auto_percent = snap.auto_percent;
    try {
      const saved = await api.setTrackingConfig(snap.mode, snap.prompt_seconds, snap.auto_percent, snap.auto_ask, snap.mpv_ipc_socket, snap.discord_enabled);
      if (
        cfg.mode === snap.mode &&
        cfg.prompt_seconds === snap.prompt_seconds &&
        cfg.auto_percent === snap.auto_percent &&
        cfg.auto_ask === snap.auto_ask &&
        cfg.mpv_ipc_socket.trim() === snap.mpv_ipc_socket &&
        cfg.discord_enabled === snap.discord_enabled
      ) {
        cfg = saved;
        trackingSavedAt = Date.now();
      }
    } catch (e) {
      trackingError = String(e);
    } finally {
      trackingSaving = false;
    }
  }
  async function toggleSetting(key: ToggleKey) {
    const state = toggles[key];
    if (!state.loaded || state.saving) return;
    const value = key === "close_to_tray" ? closeToTray : key === "auto_update" ? autoUpdate : hardwareAcceleration;
    state.saving = true;
    state.error = "";
    if (key === "hardware_acceleration") renderingSaved = false;
    try {
      await api.setAppSetting(key, value ? "1" : "0");
      if (key === "hardware_acceleration") renderingSaved = true;
    } catch (e) {
      if (key === "close_to_tray") closeToTray = !value;
      else if (key === "auto_update") autoUpdate = !value;
      else hardwareAcceleration = !value;
      state.error = String(e);
    } finally {
      state.saving = false;
    }
  }
  async function checkForUpdate() {
    updateChecking = true;
    updateError = "";
    updateStatus = "";
    update = null;
    try {
      update = await api.checkUpdate();
      notePendingRestart(update.restart_pending);
    } catch (e) {
      updateError = String(e);
    } finally {
      updateChecking = false;
    }
  }
  async function installUpdate() {
    updateError = "";
    updateStatus = "";
    try {
      const result = await runInstallUpdate();
      if (result === "installed") updateStatus = "Installed — restart Kurisu to finish.";
    } catch (e) {
      updateError = String(e);
    }
  }
  load();
  void loadToggle("close_to_tray");
  void loadToggle("auto_update");
  if (isLinux) void loadToggle("hardware_acceleration");
</script>

<div class="page-content space-y-5">
  <div>
    <PageHeading section="Program" title="Settings" description="El Psy Congroo" />
    {#if loadError}
      <p class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mt-2">
        Couldn't load settings: {loadError}
      </p>
    {/if}
  </div>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Account</h2>
    {#if auth.user}
      <p class="text-sm mb-2">Signed in as <b>{auth.user.name}</b>.</p>
      <button
        onclick={() => (confirmingLogout = true)}
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
      Bare MPV is detected through its IPC socket.
    </p>
    <fieldset disabled={!trackingLoaded} class="min-w-0">
    <div class="space-y-2 mb-4">
      {#each modes as [val, label]}
        <label class="flex items-center gap-2 text-sm cursor-pointer">
          <input type="radio" name="tmode" value={val} bind:group={cfg.mode} oninput={() => (trackingSavedAt = 0)} class="accent-accent" />
          {label}
        </label>
      {/each}
    </div>
    {#if cfg.mode === "prompt"}
      <div class="mb-3 text-sm flex flex-wrap items-center gap-2">
        Ask after
        <input
          type="number"
          bind:value={cfg.prompt_seconds}
          oninput={() => (trackingSavedAt = 0)}
          min="1"
          max="3600"
          class="w-20 bg-panel border border-edge rounded-md px-2 py-1 focus:outline-none focus:border-accent"
        />
        seconds of playback
        <span class="text-ink-dim">({Math.round(cfg.prompt_seconds / 60)} min)</span>
      </div>
    {/if}
    {#if cfg.mode === "auto"}
      <div class="mb-3 text-sm flex flex-wrap items-center gap-2">
        Update progress at
        <input
          type="number"
          bind:value={cfg.auto_percent}
          oninput={() => (trackingSavedAt = 0)}
          min="1"
          max="100"
          class="w-20 bg-panel border border-edge rounded-md px-2 py-1 focus:outline-none focus:border-accent"
        />
        % watched
      </div>
    {/if}
    <label class="flex items-start gap-2 text-sm cursor-pointer mb-4">
      <input
        type="checkbox"
        bind:checked={cfg.auto_ask}
        onchange={() => (trackingSavedAt = 0)}
        class="accent-accent mt-0.5"
      />
      <span>
        Jump to the <b>Currently Watching</b> tab and ask to update after ~15s of playback
        <span class="block text-xs text-ink-dim mt-0.5">
          On by default. When a show on your list is detected, Kurisu switches to the
          detection tab and prompts you — harder to miss than the banner alone.
        </span>
      </span>
    </label>
    <label class="flex items-start gap-2 text-sm cursor-pointer mb-4">
      <input
        type="checkbox"
        bind:checked={cfg.discord_enabled}
        onchange={() => (trackingSavedAt = 0)}
        class="accent-accent mt-0.5"
      />
      <span>
        Show what I'm watching as my Discord status
        <span class="block text-xs text-ink-dim mt-0.5">
          Rich Presence. Announces the matched show and episode with the time left
          while it plays, and clears when playback stops. Works in every tracking
          mode, including Off, and needs the Discord desktop app running.
        </span>
      </span>
    </label>
    <div class="mb-4">
      <label class="block text-sm mb-1" for="mpv-sock">MPV socket path <span class="text-ink-dim">(optional)</span></label>
      <input
        id="mpv-sock"
        type="text"
        placeholder="/tmp/mpvsocket"
        bind:value={cfg.mpv_ipc_socket}
        oninput={() => (trackingSavedAt = 0)}
        class="w-full bg-panel border border-edge rounded-md px-2 py-1 text-sm font-mono focus:outline-none focus:border-accent"
      />
      <p class="text-xs text-ink-dim mt-1">
        Bare MPV doesn't report playback to the OS, so Kurisu reads mpv's IPC socket
        directly. Turn it on with <code>input-ipc-server=/tmp/mpvsocket</code> in your
        mpv.conf. Leave this blank to try the common default paths.
      </p>
    </div>
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
      {#if trackingError}
        <p class="text-xs text-red-400 mt-2">Save failed: {trackingError}</p>
      {/if}
    </div>
    </fieldset>
  </section>

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Window</h2>
    <label class="flex items-center gap-2 text-sm cursor-pointer">
      <input
        type="checkbox"
        bind:checked={closeToTray}
        disabled={!toggles.close_to_tray.loaded || toggles.close_to_tray.saving}
        onchange={() => toggleSetting("close_to_tray")}
        class="accent-accent"
      />
      Hide to system tray when closing the window
    </label>
    <p class="text-xs text-ink-dim mt-1">
      Off by default — the close button quits Kurisu outright. Turn this on to keep
      it running in the tray instead (Quit is always available in the tray menu).
    </p>
    {#if toggles.close_to_tray.error}
      <p class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mt-2">
        Window setting: {toggles.close_to_tray.error}
        {#if !toggles.close_to_tray.loaded}
          <button onclick={() => loadToggle("close_to_tray")} disabled={toggles.close_to_tray.loading} class="underline ml-2">Retry</button>
        {/if}
      </p>
    {/if}
  </section>

  {#if isLinux}
    <section class="pt-4 border-t border-edge">
      <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Performance</h2>
      <label class="flex items-center gap-2 text-sm cursor-pointer">
        <input
          type="checkbox"
          bind:checked={hardwareAcceleration}
          disabled={!toggles.hardware_acceleration.loaded || toggles.hardware_acceleration.saving}
          onchange={() => toggleSetting("hardware_acceleration")}
          class="accent-accent"
        />
        Use hardware acceleration
      </label>
      <p class="text-xs text-ink-dim mt-1">
        Off by default. Uses your graphics card and may make scrolling smoother.
        Restart Kurisu after changing this setting. Turn it off if you see graphics problems.
      </p>
      {#if renderingSaved}
        <p class="text-xs text-accent mt-2" role="status">Saved. Quit and reopen Kurisu to apply.</p>
      {/if}
      {#if toggles.hardware_acceleration.error}
        <p class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mt-2">
          Rendering setting: {toggles.hardware_acceleration.error}
          {#if !toggles.hardware_acceleration.loaded}
            <button onclick={() => loadToggle("hardware_acceleration")} disabled={toggles.hardware_acceleration.loading} class="underline ml-2">Retry</button>
          {/if}
        </p>
      {/if}
    </section>
  {/if}

  <section class="pt-4 border-t border-edge">
    <h2 class="text-sm font-semibold uppercase tracking-wide text-ink-dim mb-2">Updates</h2>
    <label class="flex items-center gap-2 text-sm cursor-pointer">
      <input
        type="checkbox"
        bind:checked={autoUpdate}
        disabled={!toggles.auto_update.loaded || toggles.auto_update.saving}
        onchange={() => toggleSetting("auto_update")}
        class="accent-accent"
      />
      Automatically check for updates
    </label>
    <p class="text-xs text-ink-dim mt-1 mb-3">
      On by default. Checks on startup and every hour while Kurisu is running. You choose when to install.
    </p>
    {#if toggles.auto_update.error}
      <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-3">
        Update setting: {toggles.auto_update.error}
        {#if !toggles.auto_update.loaded}
          <button onclick={() => loadToggle("auto_update")} disabled={toggles.auto_update.loading} class="underline ml-2">Retry</button>
        {/if}
      </div>
    {/if}
    <div class="flex flex-wrap items-center gap-2">
      <button
        onclick={checkForUpdate}
        disabled={updateChecking || installInFlight()}
        class="px-3 py-1.5 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50"
      >
        {updateChecking ? "Checking…" : "Check for updates"}
      </button>
      {#if update?.available && update.can_install && !updateStatus && !updateNeedsRestart()}
        <button
          onclick={installUpdate}
          disabled={installInFlight()}
          class="px-3 py-1.5 rounded-md bg-accent hover:bg-accent-2 text-white text-sm disabled:opacity-50"
        >
          {installInFlight() ? "Downloading…" : `Install ${update.version}`}
        </button>
      {/if}
    </div>
    {#if update}
      {#if update.available}
        <p class="text-xs text-accent mt-2">
          Version {update.version} is available (you're on {update.current}).
          {#if updateNeedsRestart()}
            — installed, restart Kurisu to finish.
          {:else if !update.can_install}
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
    {#if updateStatus}
      <p class="text-xs text-accent mt-2">{updateStatus}</p>
    {/if}
  </section>
</div>

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
