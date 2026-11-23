// Mirror the Rust payloads.

export type ListStatus =
  | "CURRENT"
  | "PLANNING"
  | "COMPLETED"
  | "PAUSED"
  | "DROPPED"
  | "REPEATING";

export interface Media {
  id: number;
  id_mal?: number | null;
  title_romaji?: string | null;
  title_english?: string | null;
  title_native?: string | null;
  cover_medium?: string | null;
  cover_large?: string | null;
  episodes?: number | null;
  format?: string | null;
  status?: string | null;
  average_score?: number | null;
  season?: string | null;
  season_year?: number | null;
  description?: string | null;
  next_airing_episode?: number | null;
  next_airing_at?: number | null;
  banner_image?: string | null;
  genres?: string[] | null;
  duration?: number | null;
  source?: string | null;
  studios?: string[] | null;
}

export interface MediaRelation {
  relation: string;
  media: Media;
}

export interface MediaCharacter {
  role?: string | null;
  name: string;
  image?: string | null;
  va_name?: string | null;
  va_image?: string | null;
}

export interface MediaStaff {
  role?: string | null;
  name: string;
  image?: string | null;
}

export interface MediaDetail {
  media: Media;
  relations: MediaRelation[];
  characters: MediaCharacter[];
  staff: MediaStaff[];
  cached_at?: number | null;
  warning?: string | null;
  unavailable_sections?: string[];
}

export interface SearchPage {
  items: Media[];
  page: number;
  has_next_page: boolean;
}

export interface NotificationPage {
  items: Notification[];
  page: number;
  has_next_page: boolean;
  unread_count: number | null;
}

export interface FuzzyDate {
  year: number | null;
  month: number | null;
  day: number | null;
}

export interface EntryDetails {
  notes: string;
  started_at: FuzzyDate | null;
  completed_at: FuzzyDate | null;
  custom_lists: string[];
  available_custom_lists: string[];
  cached_at?: number | null;
  warning?: string | null;
}

export interface TorrentItem {
  title: string;
  link: string;
  guid: string;
  seen_guid?: string | null;
  details_url?: string | null;
  magnet?: string | null;
  size?: string | null;
  seeders?: number | null;
  leechers?: number | null;
  category_id?: string | null;
  category?: string | null;
  trusted?: boolean | null;
  remake?: boolean | null;
  published?: number | null;
  media_id?: number | null;
  matched?: string | null;
  episode?: number | null;
  is_new: boolean;
  seen: boolean;
}

export interface UnreadableFolder {
  path: string;
  error: string;
}

export interface LibraryScan {
  files: LibraryFile[];
  unreadable: UnreadableFolder[];
}

export interface FeedFailure {
  url: string;
  error: string;
}

export interface TorrentFetch {
  items: TorrentItem[];
  failures: FeedFailure[];
}

export interface ShowTorrents {
  batches: TorrentItem[];
  episodes: TorrentItem[];
  other: TorrentItem[];
  next_episode: number | null;
  warnings: string[];
}

export interface UserStats {
  cached_at?: number | null;
  warning?: string | null;
  count: number;
  episodes_watched: number;
  minutes_watched: number;
  mean_score: number;
  standard_deviation: number;
  scores: { score: number; count: number }[];
  statuses: { status: string; count: number }[];
  formats: { format: string; count: number }[];
  genres: { genre: string; count: number; minutes_watched: number }[];
  release_years: { year: number; count: number }[];
}

export interface AiringItem {
  airing_at: number;
  episode: number;
  media: Media;
}

export interface ListEntry {
  id?: number | null;
  media_id: number;
  status: string;
  progress: number;
  score?: number | null;
  repeat: number;
  updated_at?: number | null;
  media?: Media | null;
}

export interface User {
  id: number;
  name: string;
  avatar?: string | null;
  score_format?: string | null;
  offline?: boolean;
}

export interface Notification {
  id: number;
  kind: string;
  context?: string | null;
  created_at?: number | null;
  media_id?: number | null;
  media_title?: string | null;
  media_cover?: string | null;
  episode?: number | null;
  activity_id?: number | null;
  thread_id?: number | null;
  thread_title?: string | null;
  comment_id?: number | null;
  comment_url?: string | null;
  reason?: string | null;
  deleted_media_title?: string | null;
  user_name?: string | null;
  user_avatar?: string | null;
}

export interface TrackingConfig {
  mode: "off" | "prompt" | "auto";
  prompt_seconds: number;
  auto_percent: number;
  auto_ask: boolean;
  mpv_ipc_socket: string;
  discord_enabled: boolean;
}

export interface NowPlaying {
  active: boolean;
  player: string;
  title: string;
  matched: string | null;
  media_id: number | null;
  episode: number | null;
  length_us: number;
  position_us: number;
}

export interface TrackingPrompt {
  media_id: number;
  episode: number;
  title: string;
  raw_title: string;
  progress: number;
}

export interface TrackingEvent extends TrackingPrompt {
  generation: string;
  account_id: number | null;
  prompt_id: string;
  ask: boolean;
}

export interface UpdateInfo {
  available: boolean;
  can_install: boolean;
  restart_pending: boolean;
  version: string; // latest release
  tag: string;
  html_url: string;
  body: string;
  current: string; // running build
}

/// bound means a manual file or folder link supplied the match.
export interface LibraryFile {
  path: string;
  media_id: number | null;
  matched: string | null;
  episode: number | null;
  bound?: boolean;
}

export type ScoreFormat =
  | "POINT_100"
  | "POINT_10_DECIMAL"
  | "POINT_10"
  | "POINT_5"
  | "POINT_3";

export interface LibraryBinding {
  media_id: number;
  episode_offset: number;
}

export interface WatchHistoryItem {
  id: number;
  path: string;
  media_id: number;
  episode: number;
  watched_at: number;
  title: string;
}

export interface PendingChange {
  media_id: number;
  title: string;
  progress: number | null;
  status: string | null;
  score: number | null;
  repeat: number | null;
  error: string | null;
  conflict: boolean;
  missing_media: boolean;
}
