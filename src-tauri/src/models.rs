//! Data types shared between the AniList client, the local DB, and the frontend
//! (serde + Tauri serialize every command return).

use serde::{Deserialize, Serialize};

/// AniList media list status. Matches the API enum values exactly (PascalCase).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ListStatus {
    Current,   // "watching"
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
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            ListStatus::Current => "Watching",
            ListStatus::Planning => "Plan to Watch",
            ListStatus::Completed => "Completed",
            ListStatus::Paused => "Paused",
            ListStatus::Dropped => "Dropped",
            ListStatus::Repeating => "Rewatching",
        }
    }
}

/// A cached anime entry. Fields we actually show in the UI; AniList returns far
/// more, we only deserialize what we need.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    /// Next episode that hasn't aired yet (AniList `nextAiringEpisode`).
    pub next_airing_episode: Option<i64>,
    /// When that next episode airs (Unix seconds). None = unknown / finished.
    pub next_airing_at: Option<i64>,
}

impl Media {
    pub fn display_title(&self) -> String {
        self.title_english.clone()
            .or_else(|| self.title_romaji.clone())
            .or_else(|| self.title_native.clone())
            .unwrap_or_else(|| format!("#{}", self.id))
    }
}

/// One row of the user's AniList anime list (the bits we track locally).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListEntry {
    pub id: Option<i64>,          // AniList list-entry id (the row, not the media)
    pub media_id: i64,            // the anime
    pub status: String,           // ListStatus as a string for the frontend
    pub progress: i64,
    pub score: Option<f64>,
    pub repeat: i64,
    pub updated_at: Option<i64>,
    pub media: Option<Media>,     // joined when served to the UI
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub avatar: Option<String>,
    /// The user's preferred score format: POINT_100 / POINT_10_DECIMAL /
    /// POINT_10 / POINT_5 / POINT_3 (smiley). Drives the score UI.
    pub score_format: Option<String>,
}

/// A flattened AniList notification. The API returns a union of ~14 concrete types;
/// we capture the fields we care about and leave the rest None. `kind` is the
/// `type` enum (AIRING / FOLLOWING / ACTIVITY_LIKE / …).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub context: Option<String>,
    pub created_at: Option<i64>,
    pub media_id: Option<i64>,
    pub episode: Option<i64>,
    pub activity_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub comment_id: Option<i64>,
    pub reason: Option<String>,
    pub deleted_media_title: Option<String>,
    pub user_name: Option<String>,
    pub user_avatar: Option<String>,
}
