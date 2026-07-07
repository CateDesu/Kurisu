// Types mirroring the Rust models in src-tauri/src/models.rs. Kept in sync by hand;
// the invoke wrappers in api.ts are the only call sites.

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
}

export function displayTitle(m: Media | null | undefined): string {
  if (!m) return "";
  return (
    m.title_english || m.title_romaji || m.title_native || `#${m.id}`
  );
}

export interface Notification {
  id: number;
  kind: string;
  context?: string | null;
  created_at?: number | null;
  media_id?: number | null;
  episode?: number | null;
  activity_id?: number | null;
  thread_id?: number | null;
  comment_id?: number | null;
  reason?: string | null;
  deleted_media_title?: string | null;
  user_name?: string | null;
  user_avatar?: string | null;
}

/// Where a notification should link. Anime/activity/thread/user, else the inbox.
export function notificationUrl(n: Notification): string {
  if (n.media_id) return `https://anilist.co/anime/${n.media_id}`;
  if (n.activity_id) return `https://anilist.co/activity/${n.activity_id}`;
  if (n.thread_id) return `https://anilist.co/forum/thread/${n.thread_id}`;
  if (n.user_name) return `https://anilist.co/user/${n.user_name}`;
  return "https://anilist.co/notifications";
}

/// Per-kind emoji for the inbox list.
export function notificationIcon(kind: string): string {
  const k = kind.toUpperCase();
  if (k === "AIRING") return "📺";
  if (k === "FOLLOWING") return "👤";
  if (k.includes("LIKE")) return "❤️";
  if (k.includes("MESSAGE")) return "✉️";
  if (k.includes("MENTION") || k.includes("REPLY") || k.includes("THREAD")) return "💬";
  if (k.includes("MEDIA") || k.includes("RELATED")) return "📺";
  return "🔔";
}

/// Compact relative timestamp.
export function timeAgo(unix: number | null | undefined): string {
  if (!unix) return "";
  const s = Date.now() / 1000 - unix;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  if (s < 604800) return `${Math.floor(s / 86400)}d`;
  return new Date(unix * 1000).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export interface TrackingConfig {
  mode: "off" | "prompt" | "auto";
  prompt_seconds: number;
  auto_percent: number;
}

/// "Now Playing" payload pushed from the MPRIS watcher every tick.
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

/// Prompt-mode request: shown as an in-app modal (no tray notification).
export interface TrackingPrompt {
  media_id: number;
  episode: number;
  title: string;
  raw_title: string;
}

export const STATUS_LABEL: Record<string, string> = {
  CURRENT: "Watching",
  PLANNING: "Plan to Watch",
  COMPLETED: "Completed",
  PAUSED: "Paused",
  DROPPED: "Dropped",
  REPEATING: "Rewatching",
};

/// AniList score formats. The user's chosen format (from Viewer.mediaListOptions)
/// decides how scores are shown and edited.
export type ScoreFormat =
  | "POINT_100"
  | "POINT_10_DECIMAL"
  | "POINT_10"
  | "POINT_5"
  | "POINT_3";

/// Render a score for compact display (list rows). Empty string = no score.
export function scoreLabel(score: number | null | undefined, format?: string | null): string {
  if (score == null || score <= 0) return "";
  switch (format as ScoreFormat) {
    case "POINT_3":
      return ["", "😞", "😐", "😊"][Math.round(score)] ?? `${score}`;
    case "POINT_5":
      return `${"★".repeat(Math.min(5, Math.round(score)))}`;
    default:
      return `★ ${score}`;
  }
}

/// Human-readable "next episode airs" line, or null if none / already aired.
export function airingLabel(m: Media | null | undefined): string | null {
  if (!m?.next_airing_episode || !m?.next_airing_at) return null;
  const now = Date.now() / 1000;
  const diff = m.next_airing_at - now;
  if (diff <= 0) return null;
  const ep = m.next_airing_episode;
  const hours = diff / 3600;
  if (hours < 1) return `Ep ${ep} airs in ${Math.max(1, Math.round(diff / 60))}m`;
  if (hours < 24) return `Ep ${ep} airs in ${Math.ceil(hours)}h`;
  const days = diff / 86400;
  if (days < 7) return `Ep ${ep} airs in ${Math.ceil(days)}d`;
  const d = new Date(m.next_airing_at * 1000);
  return `Ep ${ep} airs ${d.toLocaleDateString(undefined, { month: "short", day: "numeric" })}`;
}
