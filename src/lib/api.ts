import { invoke } from "@tauri-apps/api/core";
import type {
  EntryDetails,
  FuzzyDate,
  SearchPage,
  NotificationPage,
  AiringItem,
  ListEntry,
  Media,
  MediaDetail,
  Notification,
  TorrentItem,
  TorrentFetch,
  ShowTorrents,
  LibraryScan,
  LibraryBinding,
  WatchHistoryItem,
  PendingChange,
  TrackingConfig,
  UpdateInfo,
  User,
  UserStats,
} from "./types";

export const api = {
  getTrackingConfig: () => invoke<TrackingConfig>("get_tracking_config"),
  setTrackingConfig: (
    mode: string,
    promptSeconds: number,
    autoPercent: number,
    autoAsk: boolean,
    mpvIpcSocket: string,
    discordEnabled: boolean
  ) =>
    invoke<TrackingConfig>("set_tracking_config", {
      mode,
      promptSeconds,
      autoPercent,
      autoAsk,
      mpvIpcSocket,
      discordEnabled,
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
  searchAnimePage: (query: string, page = 1) => invoke<SearchPage>("search_anime_page", { query, page }),
  getSeason: (season: string, year: number) =>
    invoke<Media[]>("get_season", { season, year }),
  getRecommendations: (mediaId: number) =>
    invoke<Media[]>("get_recommendations", { mediaId }),
  getMedia: (id: number) => invoke<Media>("get_media", { id }),
  getMediaDetail: (id: number) => invoke<MediaDetail>("get_media_detail", { id }),
  getAiringSchedule: (start: number, end: number) =>
    invoke<AiringItem[]>("get_airing_schedule", { start, end }),
  getEntry: (mediaId: number) =>
    invoke<ListEntry | null>("get_entry", { mediaId }),

  getEntryDetails: (mediaId: number) => invoke<EntryDetails>("get_entry_details", { mediaId }),
  updateEntryDetails: (mediaId: number, notes: string | null, startedAt: FuzzyDate | null, completedAt: FuzzyDate | null, customLists: string[] | null) =>
    invoke<EntryDetails>("update_entry_details", { mediaId, notes, startedAt, completedAt, customLists }),

  syncMyList: () => invoke<ListEntry[]>("sync_my_list"),
  localEntries: () => invoke<ListEntry[]>("local_entries"),
  updateEntry: (
    mediaId: number,
    status: string | null,
    progress: number | null,
    score: number | null,
    repeat: number | null
  ) =>
    invoke<ListEntry>("update_entry", { mediaId, status, progress, score, repeat }),
  setProgress: (mediaId: number, progress: number, expected?: number) =>
    invoke<ListEntry>("set_progress", { mediaId, progress, expected }),
  deleteEntry: (mediaId: number) =>
    invoke<void>("delete_entry_cmd", { mediaId }),

  getNotifications: () => invoke<Notification[]>("get_notifications"),
  getNotificationsPage: (page = 1) => invoke<NotificationPage>("get_notifications_page", { page }),
  markNotificationsRead: () => invoke<void>("mark_notifications_read"),

  getLibraryFolders: () => invoke<string[]>("get_library_folders"),
  addLibraryFolder: (path: string) =>
    invoke<string[]>("add_library_folder", { path }),
  removeLibraryFolder: (path: string) =>
    invoke<string[]>("remove_library_folder", { path }),
  scanLibrary: () => invoke<LibraryScan>("scan_library"),
  bindLibraryPath: (path: string, mediaId: number, episodeOffset = 0) =>
    invoke<void>("bind_library_path", { path, mediaId, episodeOffset }),
  getLibraryBinding: (path: string) =>
    invoke<number | null>("library_binding_for", { path }),
  getLibraryBindingDetails: (path: string) =>
    invoke<LibraryBinding | null>("library_binding_details", { path }),
  getWatchHistory: (limit = 50, before?: number) =>
    invoke<WatchHistoryItem[]>("get_watch_history", { limit, before }),
  getPendingChanges: () => invoke<PendingChange[]>("get_pending_changes"),
  syncPendingChanges: () => invoke<PendingChange[]>("sync_pending_changes"),
  resolvePendingChange: (mediaId: number, keepLocal: boolean) =>
    invoke<void>("resolve_pending_change", { mediaId, keepLocal }),
  unbindLibraryMedia: (mediaId: number) =>
    invoke<void>("unbind_library_media", { mediaId }),

  getRssFeeds: () => invoke<string[]>("get_rss_feeds"),
  addRssFeed: (url: string) => invoke<string[]>("add_rss_feed", { url }),
  removeRssFeed: (url: string) => invoke<string[]>("remove_rss_feed", { url }),
  fetchTorrents: () => invoke<TorrentFetch>("fetch_torrents"),
  findShowTorrents: (mediaId: number, category = "1_0", filter = "0") =>
    invoke<ShowTorrents>("find_show_torrents", { mediaId, category, filter }),
  markTorrentsSeen: (guids: string[]) =>
    invoke<void>("mark_torrents_seen", { guids }),
  searchTorrents: (query: string, category = "1_0", filter = "0") =>
    invoke<TorrentItem[]>("search_torrents", { query, category, filter }),

  getUserStats: () => invoke<UserStats>("get_user_stats"),

  checkUpdate: () => invoke<UpdateInfo>("check_update"),
  installUpdate: () => invoke<string>("install_update"),
  takeUpdateFailed: () => invoke<string | null>("take_update_failed"),
  takePendingUpdate: () => invoke<UpdateInfo | null>("take_pending_update"),
};
