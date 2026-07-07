// Typed wrappers over every Tauri command. Centralizes the invoke calls so the UI
// never builds the call strings by hand, and gives us one place to handle errors.
import { invoke } from "@tauri-apps/api/core";
import type { ListEntry, Media, Notification, TrackingConfig, User } from "./types";

export const api = {
  getClientId: () => invoke<string | null>("get_client_id"),
  setClientId: (id: string) => invoke<void>("set_client_id", { id }),
  getRedirectUri: () => invoke<string | null>("get_redirect_uri"),
  setRedirectUri: (uri: string) => invoke<void>("set_redirect_uri", { uri }),

  getTrackingConfig: () => invoke<TrackingConfig>("get_tracking_config"),
  setTrackingConfig: (
    mode: string,
    promptSeconds: number,
    autoPercent: number
  ) =>
    invoke<TrackingConfig>("set_tracking_config", {
      mode,
      promptSeconds,
      autoPercent,
    }),

  isLoggedIn: () => invoke<boolean>("is_logged_in"),
  currentUser: () => invoke<User | null>("current_user"),
  loginWithToken: (token: string) => invoke<User>("login_with_token", { token }),
  loginOauth: () => invoke<User>("login_oauth"),
  logout: () => invoke<void>("logout"),

  searchAnime: (query: string) => invoke<Media[]>("search_anime", { query }),
  getMedia: (id: number) => invoke<Media>("get_media", { id }),
  getEntry: (mediaId: number) =>
    invoke<ListEntry | null>("get_entry", { mediaId }),

  syncMyList: () => invoke<ListEntry[]>("sync_my_list"),
  localEntries: () => invoke<ListEntry[]>("local_entries"),
  updateEntry: (
    mediaId: number,
    status: string,
    progress: number,
    score: number | null
  ) =>
    invoke<ListEntry>("update_entry", { mediaId, status, progress, score }),
  incrementEpisode: (mediaId: number) =>
    invoke<ListEntry>("increment_episode", { mediaId }),
  setProgress: (mediaId: number, progress: number) =>
    invoke<ListEntry>("set_progress", { mediaId, progress }),
  deleteEntry: (mediaId: number) =>
    invoke<void>("delete_entry_cmd", { mediaId }),

  getNotifications: () => invoke<Notification[]>("get_notifications"),
};
