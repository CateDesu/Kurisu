<script lang="ts">
  import { auth } from "$lib/auth.svelte";

  let token = $state("");
  let busy = $state(false);
  let error = $state("");

  async function oauth() {
    busy = true;
    error = "";
    try {
      await auth.loginOauth();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
  async function pasteToken(e: Event) {
    e.preventDefault();
    if (!token.trim()) return;
    busy = true;
    error = "";
    try {
      await auth.loginWithToken(token.trim());
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="w-[420px] max-w-[90vw] bg-panel border border-edge rounded-xl p-7">
  <div class="flex items-center gap-3 mb-1">
    <span class="text-accent text-3xl leading-none">ク</span>
    <h1 class="text-xl font-semibold">Kurisu</h1>
  </div>
  <p class="text-ink-dim text-sm mb-6">Connect your AniList account to start tracking.</p>

  {#if error}
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
      {error}
    </div>
  {/if}

  <button
    onclick={oauth}
    disabled={busy}
    class="w-full py-2.5 rounded-md bg-accent hover:bg-accent-2 disabled:opacity-50 text-white font-medium transition-colors"
  >
    {#if busy}Connecting…{:else}Connect via AniList{/if}
  </button>

  <details class="mt-5 text-sm">
    <summary class="text-ink-dim cursor-pointer hover:text-ink">Paste a token instead</summary>
    <form onsubmit={pasteToken} class="mt-3 flex gap-2">
      <input
        bind:value={token}
        type="password"
        placeholder="AniList access token"
        class="flex-1 bg-panel-2 border border-edge rounded-md px-3 py-2 text-sm focus:outline-none focus:border-accent"
      />
      <button class="px-3 py-2 rounded-md bg-panel-2 hover:bg-edge text-sm">Use</button>
    </form>
    <p class="text-xs text-ink-dim mt-2">
      An AniList access token — only needed if the browser sign-in above doesn't
      open for you.
    </p>
  </details>
</div>
