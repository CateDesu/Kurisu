import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import { library } from "./library.svelte";
import type { User } from "./types";

let user = $state<User | null>(null);
let ready = $state(false);
// A stored token keeps cached pages accessible while AniList is unreachable.
let offline = $state(false);
let epoch = $state(0);

let refreshId = 0;
let authIntent = 0;

async function login(request: () => Promise<User>) {
  const intent = ++authIntent;
  refreshId++;
  const nextUser = await request();
  if (intent !== authIntent) throw new Error("Sign-in was superseded.");
  user = nextUser;
  offline = false;
  epoch++;
  library.reset();
  ready = true;
  return nextUser;
}

async function refresh() {
  const request = ++refreshId;
  const session = epoch;
  const current = () => request === refreshId && session === epoch;
  try {
    const nextUser = await api.currentUser();
    if (!current()) return;
    user = nextUser;
    offline = nextUser?.offline ?? false;
  } catch {
    let storedToken = false;
    try {
      storedToken = await api.isLoggedIn();
    } catch {
    }
    if (!current()) return;
    offline = storedToken;
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
  get offline() {
    return offline;
  },
  get isLoggedIn() {
    return user !== null || offline;
  },
  /* Use this to invalidate caches on login or logout. */
  get epoch() {
    return epoch;
  },
  refresh,
  loginOauth() {
    return login(() => api.loginOauth());
  },
  loginWithToken(token: string) {
    return login(() => api.loginWithToken(token));
  },
  async logout() {
    const intent = ++authIntent;
    refreshId++;
    try {
      await api.logout();
    } catch (e) {
      if (intent !== authIntent) throw e;
      console.error("logout failed", e);
      user = null;
      offline = false;
      epoch++;
      ready = true;
      library.reset();
      throw e;
    }
    if (intent !== authIntent) return;
    user = null;
    offline = false;
    epoch++;
    ready = true;
    library.reset();
  },
};

listen("kurisu://auth-expired", () => {
  refreshId++;
  ready = true;
  user = null;
  offline = false;
  epoch++;
  library.reset();
});

refresh();
