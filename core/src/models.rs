use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ListStatus {
    Current,
    Planning,
    Completed,
    Paused,
    Dropped,
    Repeating,
}

impl ListStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ListStatus::Current => "CURRENT",
            ListStatus::Planning => "PLANNING",
            ListStatus::Completed => "COMPLETED",
            ListStatus::Paused => "PAUSED",
            ListStatus::Dropped => "DROPPED",
            ListStatus::Repeating => "REPEATING",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Media {
    pub id: i64,
    pub id_mal: Option<i64>,
    pub title_romaji: Option<String>,
    pub title_english: Option<String>,
    pub title_native: Option<String>,
    pub cover_medium: Option<String>,
    pub cover_large: Option<String>,
    pub episodes: Option<i64>,
    pub format: Option<String>,
    pub status: Option<String>,
    pub average_score: Option<i64>,
    pub season: Option<String>,
    pub season_year: Option<i64>,
    pub description: Option<String>,
    pub next_airing_episode: Option<i64>,
    /// Unix seconds.
    pub next_airing_at: Option<i64>,
    pub banner_image: Option<String>,
    pub genres: Option<Vec<String>>,
    /// Minutes.
    pub duration: Option<i64>,
    pub source: Option<String>,
    pub studios: Option<Vec<String>>,
}

impl Media {
    pub(crate) fn with_detail_fields_from(self, details: Self) -> Self {
        Self {
            banner_image: details.banner_image,
            genres: details.genres,
            duration: details.duration,
            source: details.source,
            studios: details.studios,
            ..self
        }
    }

    pub(crate) fn has_metadata(&self) -> bool {
        self.title().is_some()
            || self.id_mal.is_some()
            || self.cover_medium.is_some()
            || self.cover_large.is_some()
            || self.format.is_some()
            || self.status.is_some()
            || self.average_score.is_some()
            || self.season.is_some()
            || self.season_year.is_some()
            || self.description.is_some()
            || self.next_airing_episode.is_some()
            || self.next_airing_at.is_some()
            || self.banner_image.is_some()
            || self.genres.is_some()
            || self.duration.is_some()
            || self.source.is_some()
            || self.studios.is_some()
    }

    pub fn display_title(&self) -> String {
        self.title()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("#{}", self.id))
    }

    pub fn known_episodes(&self) -> Option<i64> {
        self.episodes.filter(|total| *total > 0)
    }

    fn title(&self) -> Option<&str> {
        self.title_english
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                self.title_romaji
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
            })
            .or_else(|| {
                self.title_native
                    .as_deref()
                    .filter(|title| !title.trim().is_empty())
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListEntry {
    pub id: Option<i64>, // AniList list entry ID
    pub media_id: i64,
    pub status: String,
    pub progress: i64,
    // None is unknown. Some(0.0) is known unrated.
    pub score: Option<f64>,
    pub repeat: i64,
    pub updated_at: Option<i64>,
    pub media: Option<Media>,
    #[serde(skip)]
    pub queued_creation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryIdentity {
    Remote(i64),
    Queued(String),
    Unverified,
}

impl EntryIdentity {
    pub fn matches(&self, entry: &ListEntry) -> bool {
        match self {
            Self::Remote(id) => entry.id == Some(*id),
            Self::Queued(key) => entry.queued_creation.as_ref() == Some(key),
            Self::Unverified => false,
        }
    }
}

impl ListEntry {
    pub fn identity(&self) -> EntryIdentity {
        if let Some(id) = self.id {
            EntryIdentity::Remote(id)
        } else if let Some(key) = &self.queued_creation {
            EntryIdentity::Queued(key.clone())
        } else {
            EntryIdentity::Unverified
        }
    }
}

#[derive(Debug, Clone)]
pub struct EntryEditTarget {
    pub account_id: Option<i64>,
    pub identity: EntryIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRelation {
    pub relation: String,
    pub media: Media,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaCharacter {
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
    pub va_name: Option<String>,
    pub va_image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaStaff {
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
}

/// Full detail snapshots retain relations and credits offline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDetail {
    pub media: Media,
    pub relations: Vec<MediaRelation>,
    #[serde(default)]
    pub characters: Vec<MediaCharacter>,
    #[serde(default)]
    pub staff: Vec<MediaStaff>,
    #[serde(default)]
    pub cached_at: Option<i64>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub unavailable_sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchPage {
    pub items: Vec<Media>,
    pub page: i64,
    pub has_next_page: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotificationPage {
    pub items: Vec<Notification>,
    pub page: i64,
    pub has_next_page: bool,
    pub unread_count: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FuzzyDate {
    pub year: Option<i64>,
    pub month: Option<i64>,
    pub day: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntryDetails {
    pub notes: String,
    pub started_at: Option<FuzzyDate>,
    pub completed_at: Option<FuzzyDate>,
    pub custom_lists: Vec<String>,
    pub available_custom_lists: Vec<String>,
    #[serde(default)]
    pub cached_at: Option<i64>,
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiringItem {
    pub airing_at: i64,
    pub episode: i64,
    pub media: Media,
}

/// is_new requires a matched episode beyond progress that has not been marked seen.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentItem {
    pub title: String,
    pub link: String,
    /// Feed scoped GUID or stable search identity.
    pub guid: String,
    /// Shared identity for seen state across feeds and searches.
    pub seen_guid: Option<String>,
    pub details_url: Option<String>,
    pub magnet: Option<String>,
    pub size: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub category_id: Option<String>,
    pub category: Option<String>,
    pub trusted: Option<bool>,
    pub remake: Option<bool>,
    /// Unix seconds.
    pub published: Option<i64>,
    pub media_id: Option<i64>,
    pub matched: Option<String>,
    pub episode: Option<i64>,
    pub is_new: bool,
    pub seen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentFetch {
    pub items: Vec<TorrentItem>,
    pub failures: Vec<FeedFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShowTorrents {
    pub batches: Vec<TorrentItem>,
    pub episodes: Vec<TorrentItem>,
    pub other: Vec<TorrentItem>,
    pub next_episode: Option<i64>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeedFailure {
    pub url: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserStats {
    pub count: i64,
    pub episodes_watched: i64,
    pub minutes_watched: i64,
    pub mean_score: f64,
    pub standard_deviation: f64,
    pub scores: Vec<ScoreBucket>,
    pub statuses: Vec<StatusCount>,
    pub formats: Vec<FormatCount>,
    pub genres: Vec<GenreStat>,
    pub release_years: Vec<YearCount>,
    #[serde(default)]
    pub cached_at: Option<i64>,
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScoreBucket {
    pub score: i64,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FormatCount {
    pub format: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenreStat {
    pub genre: String,
    pub count: i64,
    pub minutes_watched: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct YearCount {
    pub year: i64,
    pub count: i64,
}

/// bound means a manual file or folder link supplied the match.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryFile {
    pub path: String,
    pub media_id: Option<i64>,
    pub matched: Option<String>,
    pub episode: Option<i64>,
    #[serde(default)]
    pub bound: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryScan {
    pub files: Vec<LibraryFile>,
    pub unreadable: Vec<UnreadableFolder>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UnreadableFolder {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub avatar: Option<String>,
    pub score_format: Option<String>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub context: Option<String>,
    pub created_at: Option<i64>,
    pub media_id: Option<i64>,
    pub media_title: Option<String>,
    pub media_cover: Option<String>,
    pub episode: Option<i64>,
    pub activity_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub thread_title: Option<String>,
    pub comment_id: Option<i64>,
    pub comment_url: Option<String>,
    pub reason: Option<String>,
    pub deleted_media_title: Option<String>,
    pub user_name: Option<String>,
    pub user_avatar: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_title_uses_the_first_nonblank_title_or_media_id() {
        for english in [None, Some(""), Some(" \t\n"), Some("\u{3000}")] {
            for romaji in [None, Some(""), Some(" \t\n"), Some("\u{3000}")] {
                for native in [None, Some(""), Some(" \t\n"), Some("\u{3000}")] {
                    let mut media = Media {
                        id: 42,
                        title_english: english.map(str::to_owned),
                        title_romaji: romaji.map(str::to_owned),
                        title_native: native.map(str::to_owned),
                        ..Default::default()
                    };
                    assert_eq!(media.display_title(), "#42");
                    media.title_native = Some("原題".into());
                    assert_eq!(media.display_title(), "原題");
                    media.title_romaji = Some("Romaji".into());
                    assert_eq!(media.display_title(), "Romaji");
                    media.title_english = Some("English".into());
                    assert_eq!(media.display_title(), "English");
                    media.title_english = Some(" ".into());
                    assert_eq!(media.display_title(), "Romaji");
                    media.title_english = Some(" English ".into());
                    assert_eq!(media.display_title(), " English ");
                }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UpdateInfo {
    pub available: bool,
    pub can_install: bool,
    pub restart_pending: bool,
    pub version: String,
    pub tag: String,
    pub html_url: String,
    pub body: String,
    pub current: String,
}

pub use crate::commands::TrackingConfig;
pub use crate::library::LibraryBinding;
pub use crate::playback::{NowPlaying, TrackingPrompt};
pub use crate::sync_queue::{PendingChange, WatchHistoryItem};
