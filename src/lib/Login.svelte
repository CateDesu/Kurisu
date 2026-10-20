<script lang="ts">
  import { auth } from "$lib/auth.svelte";
  import Gearwork from "$lib/Gearwork.svelte";

  let token = $state("");
  let busy = $state<"oauth" | "token" | null>(null);
  let error = $state("");
  let attempt = 0;

  async function signIn(method: "oauth" | "token", request: () => Promise<unknown>) {
    const current = ++attempt;
    busy = method;
    error = "";
    try {
      await request();
    } catch (e) {
      if (current === attempt) error = String(e);
    } finally {
      if (current === attempt) busy = null;
    }
  }
  async function oauth() {
    if (busy) return;
    await signIn("oauth", () => auth.loginOauth());
  }
  async function pasteToken(e: Event) {
    e.preventDefault();
    if (busy === "token" || !token.trim()) return;
    await signIn("token", () => auth.loginWithToken(token.trim()));
  }
</script>

<div class="login-sheet">
  <div class="login-art"><Gearwork /></div>
  <div class="eyebrow">Your own little anime archive</div>
  <div class="mb-4 mt-4">
    <h1>Welcome to<br />Kurisu<span>.</span></h1>
  </div>
  <p class="text-ink-dim text-sm mb-6">Connect your AniList account to start tracking.</p>

  {#if error}
    <div class="text-sm text-red-400 bg-red-500/10 border border-red-500/30 rounded-md p-2 mb-4">
      {error}
    </div>
  {/if}

  <button
    onclick={oauth}
    disabled={busy !== null}
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
      <button disabled={busy === "token"} class="px-3 py-2 rounded-md bg-panel-2 hover:bg-edge text-sm disabled:opacity-50">
        Use
      </button>
    </form>
    <p class="text-xs text-ink-dim mt-2">
      If browser sign-in fails, paste an AniList access token here. This replaces
      the pending browser sign-in. Use the token itself, not a client ID or secret.
    </p>
  </details>
</div>

<style>
  .login-sheet { position: relative; width: 460px; max-width: 100%; border: 1px solid var(--color-edge); border-top: 2px solid var(--color-rust); background: var(--color-panel); padding: 35px; margin-top: 50px; }
  .login-art { position: absolute; width: 215px; top: -112px; right: -12px; opacity: .48; pointer-events: none; }
  h1 { font: 32px/1.1 var(--font-serif); letter-spacing: -.5px; }
  h1 span { color: var(--color-accent); }
  .login-art :global(.gearwork) { color: var(--color-rust); }
</style>
