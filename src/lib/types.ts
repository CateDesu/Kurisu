// Types mirroring the Rust models in src-tauri/src/models.rs. Kept in sync by hand;
// the invoke wrappers in api.ts are the only call sites.

import { nowMs } from "./now.svelte";

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

/// One anime related to another (detail page strip). `relation` is the raw
/// AniList edge type (SEQUEL / PREQUEL / SIDE_STORY / …).
export interface MediaRelation {
  relation: string;
  media: Media;
}

export interface MediaDetail {
  media: Media;
  relations: MediaRelation[];
}

/// One scheduled episode airing (calendar view).
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
  media_title?: string | null;
  media_cover?: string | null;
  episode?: number | null;
  activity_id?: number | null;
  thread_id?: number | null;
  thread_title?: string | null;
  comment_id?: number | null;
  reason?: string | null;
  deleted_media_title?: string | null;
  user_name?: string | null;
  user_avatar?: string | null;
}

/// One-line text for a notification, mirroring what anilist.co/notifications
/// shows for the same entry.
export function notificationText(n: Notification): string {
  const user = n.user_name ?? "Someone";
  const media = n.media_title ?? "A media";
  const thread = n.thread_title ? `"${n.thread_title}"` : "a thread";
  switch (n.kind.toUpperCase()) {
    case "AIRING":
      return `Episode ${n.episode ?? "?"} of ${media} aired.`;
    case "FOLLOWING":
      return `${user} started following you.`;
    case "ACTIVITY_MESSAGE":
      return `${user} sent you a message.`;
    case "ACTIVITY_MENTION":
      return `${user} mentioned you in an activity.`;
    case "ACTIVITY_REPLY":
      return `${user} replied to your activity.`;
    case "ACTIVITY_REPLY_SUBSCRIBED":
      return `${user} replied in an activity you're following.`;
    case "ACTIVITY_LIKE":
      return `${user} liked your activity.`;
    case "ACTIVITY_REPLY_LIKE":
      return `${user} liked your reply.`;
    case "THREAD_COMMENT_MENTION":
      return `${user} mentioned you in ${thread}.`;
    case "THREAD_COMMENT_REPLY":
      return `${user} replied to you in ${thread}.`;
    case "THREAD_COMMENT_SUBSCRIBED":
      return `${user} commented in ${thread}.`;
    case "THREAD_COMMENT_LIKE":
      return `${user} liked your comment in ${thread}.`;
    case "THREAD_LIKE":
      return `${user} liked your thread${n.thread_title ? ` ${thread}` : ""}.`;
    case "RELATED_MEDIA_ADDITION":
      return `${media} was added to AniList.`;
    case "MEDIA_DATA_CHANGE":
      return `${media} data was changed.`;
    case "MEDIA_MERGE":
      return `${media} was merged.`;
    case "MEDIA_DELETION":
      return `${n.deleted_media_title ?? "A media"} was deleted.`;
    default:
      return n.context ?? n.kind.replace(/_/g, " ").toLowerCase();
  }
}

/// Where a notification should link. Anime/activity/thread/user, else the inbox.
/// `encodeURIComponent` on the username — it's AniList-controlled and could
/// otherwise break out of the path.
export function notificationUrl(n: Notification): string {
  if (n.media_id) return `https://anilist.co/anime/${n.media_id}`;
  if (n.activity_id) return `https://anilist.co/activity/${n.activity_id}`;
  if (n.thread_id) return `https://anilist.co/forum/thread/${n.thread_id}`;
  if (n.user_name) return `https://anilist.co/user/${encodeURIComponent(n.user_name)}`;
  return "https://anilist.co/notifications";
}

/// Per-kind emoji for the inbox list. Exact match on the AniList kind — substring
/// matching made the result depend on arm order (THREAD_LIKE hit "LIKE" first).
export function notificationIcon(kind: string): string {
  switch (kind.toUpperCase()) {
    case "AIRING":
      return "📺";
    case "FOLLOWING":
      return "👤";
    case "ACTIVITY_LIKE":
    case "ACTIVITY_REPLY_LIKE":
    case "THREAD_LIKE":
    case "THREAD_COMMENT_LIKE":
      return "❤️";
    case "ACTIVITY_MESSAGE":
      return "✉️";
    case "ACTIVITY_MENTION":
    case "ACTIVITY_REPLY":
    case "ACTIVITY_REPLY_SUBSCRIBED":
    case "THREAD_COMMENT_MENTION":
    case "THREAD_COMMENT_REPLY":
    case "THREAD_COMMENT_SUBSCRIBED":
      return "💬";
    case "RELATED_MEDIA_ADDITION":
    case "MEDIA_DATA_CHANGE":
    case "MEDIA_MERGE":
    case "MEDIA_DELETION":
      return "📺";
    default:
      return "🔔";
  }
}

/// Compact relative timestamp.
export function timeAgo(unix: number | null | undefined): string {
  if (!unix) return "";
  const s = nowMs() / 1000 - unix;
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
/// `progress` is the entry's current progress, so the modal only offers
/// "set to Ep N" when that's actually ahead.
export interface TrackingPrompt {
  media_id: number;
  episode: number;
  title: string;
  raw_title: string;
  progress: number;
}

/// Self-update check result (settings page + the startup prompt).
export interface UpdateInfo {
  available: boolean; // a newer release exists on GitHub
  can_install: boolean; // this build can update in place (Windows + installer asset)
  version: string; // latest release version
  tag: string;
  html_url: string;
  body: string; // release notes
  current: string; // this build's version
}

/// One video file from the library scan (M3). `bound` = matched via a manual
/// file/folder link rather than the recognizer.
export interface LibraryFile {
  path: string;
  media_id: number | null;
  matched: string | null;
  episode: number | null;
  bound?: boolean;
}

export const STATUS_LABEL: Record<string, string> = {
  CURRENT: "Watching",
  PLANNING: "Plan to Watch",
  COMPLETED: "Completed",
  PAUSED: "Paused",
  DROPPED: "Dropped",
  REPEATING: "Rewatching",
};

/// AniList media (airing) status → display label.
export const MEDIA_STATUS_LABEL: Record<string, string> = {
  RELEASING: "Airing",
  FINISHED: "Finished",
  NOT_YET_RELEASED: "Not yet aired",
  CANCELLED: "Cancelled",
  HIATUS: "On hiatus",
};

/// AniList relation edge type → display label (detail page strips).
export const RELATION_LABEL: Record<string, string> = {
  PREQUEL: "Prequel",
  SEQUEL: "Sequel",
  PARENT: "Parent story",
  SIDE_STORY: "Side story",
  SPIN_OFF: "Spin-off",
  ALTERNATIVE: "Alternative",
  SUMMARY: "Summary",
  CHARACTER: "Shared cast",
  ADAPTATION: "Adaptation",
  SOURCE: "Source",
  COMPILATION: "Compilation",
  CONTAINS: "Contains",
  OTHER: "Related",
};

/// AniList `source` enum → display label ("LIGHT_NOVEL" → "Light Novel").
export function sourceLabel(source: string | null | undefined): string {
  if (!source) return "";
  return source
    .toLowerCase()
    .split("_")
    .map((w) => (w ? w[0].toUpperCase() + w.slice(1) : w))
    .join(" ");
}

/// AniList descriptions arrive as limited HTML (<br>, <i>, <b>, entities).
/// Render them as plain text — strip tags, decode the common entities — instead
/// of trusting remote HTML into {@html}.
export function plainDescription(d: string | null | undefined): string {
  if (!d) return "";
  return d
    .replace(/<br\s*\/?>/gi, "\n")
    .replace(/<\/(p|div)>/gi, "\n")
    .replace(/<[^>]+>/g, "")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#0?39;/g, "'")
    .replace(/&nbsp;/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

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
  const diff = m.next_airing_at - nowMs() / 1000;
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
