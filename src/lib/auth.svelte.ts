// App-wide reactive state with Svelte 5 runes. Holds the current user (or null) so
// every page can gate on login without each re-fetching.
import { api } from "./api";
import type { User } from "./types";

let user = $state<User | null>(null);
let ready = $state(false);

async function refresh() {
  try {
    user = await api.currentUser();
  } catch {
    user = null;
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
  get isLoggedIn() {
    return user !== null;
  },
  refresh,
  async loginOauth() {
    user = await api.loginOauth();
    return user;
  },
  async loginWithToken(token: string) {
    user = await api.loginWithToken(token);
    return user;
  },
  async logout() {
    await api.logout();
    user = null;
  },
};

// Kick off the first check on module load.
refresh();
