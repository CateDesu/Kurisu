// App-wide reactive state with Svelte 5 runes. Holds the current user (or null) so
// every page can gate on login without each re-fetching.
import { api } from "./api";
import { library } from "./library.svelte";
import type { User } from "./types";

let user = $state<User | null>(null);
let ready = $state(false);
// True when a token is stored but AniList could not be reached. Distinct from
// "signed out": the cached list, library and detail pages are all still usable,
// so pages gate on `isLoggedIn` (which stays true) and surface `offline` instead
// of bouncing the user to the sign-in card.
let offline = $state(false);

async function refresh() {
  try {
    user = await api.currentUser();
    offline = false;
  } catch {
    // A failed `current_user` means the AniList round-trip failed, NOT that the
    // user is signed out: `current_user` returns Ok(null) for that. Ask the
    // backend whether a token exists (a pure local read, no network) before
    // throwing away the session and hiding the entire offline cache.
    try {
      offline = await api.isLoggedIn();
    } catch {
      offline = false;
    }
    if (!offline) user = null;
  }
  ready = true;
}

export const auth = {
  get user() {
    return user;
  },
  get ready() {
    return ready;
  },
  /** A token is stored but AniList is unreachable. Cached data is still valid. */
  get offline() {
    return offline;
  },
  get isLoggedIn() {
    return user !== null || offline;
  },
  refresh,
  async loginOauth() {
    user = await api.loginOauth();
    offline = false;
    return user;
  },
  async loginWithToken(token: string) {
    user = await api.loginWithToken(token);
    offline = false;
    return user;
  },
  async logout() {
    try {
      await api.logout();
    } catch (e) {
      // The backend failed to finish (token scrub / cache clear). The
      // in-memory session is already dead, so still drop local state — but
      // surface the failure instead of reporting a clean logout whose token
      // row may have survived in the DB.
      console.error("logout failed", e);
      user = null;
      offline = false;
      library.reset();
      throw e;
    }
    user = null;
    offline = false;
    // Module-level caches outlive the session, so a second account would
    // otherwise see the previous one's scanned library.
    library.reset();
  },
};

// Kick off the first check on module load.
refresh();
