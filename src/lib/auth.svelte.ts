// App wide reactive state via Svelte 5 runes. Holds the current user, or null,
// so every page can gate on login without refetching.
import { listen } from "@tauri-apps/api/event";
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
// Bumped on every login and logout. Pages caching per session flags, like
// My List's auto sync, watch it so a logout and login inside one app run
// behaves like the fresh session it is instead of the stale one it looks like.
let epoch = $state(0);

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
  /** Increments on every login and logout. Key per session caches off this. */
  get epoch() {
    return epoch;
  },
  refresh,
  async loginOauth() {
    user = await api.loginOauth();
    offline = false;
    epoch++;
    return user;
  },
  async loginWithToken(token: string) {
    user = await api.loginWithToken(token);
    offline = false;
    epoch++;
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
      epoch++;
      library.reset();
      throw e;
    }
    user = null;
    offline = false;
    epoch++;
    // Module level caches outlive the session. A second account would otherwise
    // see the previous one's scanned library.
    library.reset();
  },
};

// The backend clears a rejected session the moment a write exposes it, and
// says so here. Dropping the local identity flips every page to the login
// card immediately, instead of failing each action with a raw 401 until the
// app restarts.
listen("kurisu://auth-expired", () => {
  user = null;
  offline = false;
  epoch++;
  library.reset();
});

// Kick off the first check on module load.
refresh();
