// App wide reactive state via Svelte 5 runes. Holds the current user, or null,
// so every page can gate on login without refetching.
import { api } from "./api";
import { library } from "./library.svelte";
import type { User } from "./types";

let user = $state<User | null>(null);
let ready = $state(false);
// True when a token is stored but AniList can't be reached. Not the same as
// signed out. The cached list, library and detail pages still work, so pages
// gate on isLoggedIn, which stays true, and surface offline instead of
// bouncing to the sign in card.
let offline = $state(false);

async function refresh() {
  try {
    user = await api.currentUser();
    offline = false;
  } catch {
    // A failed current_user means the AniList round trip failed. Not that the
    // user is signed out. current_user returns Ok(null) for that. Ask the
    // backend if a token exists, a local read with no network, before throwing
    // away the session and hiding the whole offline cache.
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
      // The backend failed to finish, token scrub or cache clear. The in memory
      // session is already dead, so drop local state but surface the failure
      // instead of reporting a clean logout whose token row may have survived
      // in the DB.
      console.error("logout failed", e);
      user = null;
      offline = false;
      library.reset();
      throw e;
    }
    user = null;
    offline = false;
    // Module level caches outlive the session. A second account would otherwise
    // see the previous one's scanned library.
    library.reset();
  },
};

// Kick off the first check on module load.
refresh();
