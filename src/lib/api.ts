// Typed wrappers over every Tauri command. Centralizes the invoke calls so the UI
// never builds the call strings by hand, and gives us one place to handle errors.
import { invoke } from "@tauri-apps/api/core";
import type {
  AiringItem,
  LibraryFile,
  ListEntry,
  Media,
  MediaDetail,
  Notification,
  TorrentItem,
  TrackingConfig,
  UpdateInfo,
  User,
  UserStats,
} from "./types";

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

  getAppSetting: (key: string) =>
    invoke<string | null>("get_app_setting", { key }),
  setAppSetting: (key: string, value: string) =>
    invoke<void>("set_app_setting", { key, value }),

  isLoggedIn: () => invoke<boolean>("is_logged_in"),
  currentUser: () => invoke<User | null>("current_user"),
  loginWithToken: (token: string) => invoke<User>("login_with_token", { token }),
  loginOauth: () => invoke<User>("login_oauth"),
  logout: () => invoke<void>("logout"),

  searchAnime: (query: string) => invoke<Media[]>("search_anime", { query }),
  getSeason: (season: string, year: number, page: number) =>
    invoke<Media[]>("get_season", { season, year, page }),
  getRecommendations: (mediaId: number) =>
    invoke<Media[]>("get_recommendations", { mediaId }),
  getMedia: (id: number) => invoke<Media>("get_media", { id }),
  getMediaDetail: (id: number) => invoke<MediaDetail>("get_media_detail", { id }),
  getAiringSchedule: (start: number, end: number) =>
    invoke<AiringItem[]>("get_airing_schedule", { start, end }),
  getEntry: (mediaId: number) =>
    invoke<ListEntry | null>("get_entry", { mediaId }),

  syncMyList: () => invoke<ListEntry[]>("sync_my_list"),
  localEntries: () => invoke<ListEntry[]>("local_entries"),
  updateEntry: (
    mediaId: number,
    status: string,
    progress: number,
    score: number | null,
    repeat: number
  ) =>
    invoke<ListEntry>("update_entry", { mediaId, status, progress, score, repeat }),
  incrementEpisode: (mediaId: number) =>
    invoke<ListEntry>("increment_episode", { mediaId }),
  setProgress: (mediaId: number, progress: number, expected?: number) =>
    invoke<ListEntry>("set_progress", { mediaId, progress, expected }),
  deleteEntry: (mediaId: number) =>
    invoke<void>("delete_entry_cmd", { mediaId }),

  getNotifications: () => invoke<Notification[]>("get_notifications"),

  getLibraryFolders: () => invoke<string[]>("get_library_folders"),
  addLibraryFolder: (path: string) =>
    invoke<string[]>("add_library_folder", { path }),
  removeLibraryFolder: (path: string) =>
    invoke<string[]>("remove_library_folder", { path }),
  scanLibrary: () => invoke<LibraryFile[]>("scan_library"),
  bindLibraryPath: (path: string, mediaId: number) =>
    invoke<void>("bind_library_path", { path, mediaId }),
  unbindLibraryMedia: (mediaId: number) =>
    invoke<void>("unbind_library_media", { mediaId }),

  getRssFeeds: () => invoke<string[]>("get_rss_feeds"),
  addRssFeed: (url: string) => invoke<string[]>("add_rss_feed", { url }),
  removeRssFeed: (url: string) => invoke<string[]>("remove_rss_feed", { url }),
  fetchTorrents: () => invoke<TorrentItem[]>("fetch_torrents"),
  markTorrentsSeen: (guids: string[]) =>
    invoke<void>("mark_torrents_seen", { guids }),

  getUserStats: () => invoke<UserStats>("get_user_stats"),

  checkUpdate: () => invoke<UpdateInfo>("check_update"),
  installUpdate: () => invoke<string>("install_update"),
};
