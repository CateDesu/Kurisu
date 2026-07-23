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
    // Detail-only fields (fetched by `media_detail`, not the lean list queries).
    // The DB upsert COALESCEs them so a lean re-fetch never wipes cached values.
    pub banner_image: Option<String>,
    pub genres: Option<Vec<String>>,
    /// Episode length in minutes.
    pub duration: Option<i64>,
    /// Adaptation source (MANGA / LIGHT_NOVEL / ORIGINAL / …).
    pub source: Option<String>,
    /// Main studio names.
    pub studios: Option<Vec<String>>,
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

/// One anime related to another (AniList `relations` edge), shown on the detail
/// page. `relation` is the raw edge type (SEQUEL / PREQUEL / SIDE_STORY / …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRelation {
    pub relation: String,
    pub media: Media,
}

/// One character on the detail page, with their (Japanese) voice actor.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaCharacter {
    /// MAIN / SUPPORTING / BACKGROUND.
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
    pub va_name: Option<String>,
    pub va_image: Option<String>,
}

/// One staff credit on the detail page (`role` is free text: "Director", …).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaStaff {
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
}

/// Full detail-page payload: the (rich) media plus its anime relations and
/// credits. Characters/staff are not cached — offline fallback serves them empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDetail {
    pub media: Media,
    pub relations: Vec<MediaRelation>,
    #[serde(default)]
    pub characters: Vec<MediaCharacter>,
    #[serde(default)]
    pub staff: Vec<MediaStaff>,
}

/// One scheduled episode airing (the calendar view).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiringItem {
    pub airing_at: i64,
    pub episode: i64,
    pub media: Media,
}

/// One RSS feed entry, matched against the local list. `is_new` = matched, the
/// parsed episode is past the entry's progress, and the item hasn't been marked
/// seen. Unmatched items ride along with `media_id: None`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentItem {
    pub title: String,
    /// The feed's `<link>` — for nyaa-style feeds, the .torrent download URL.
    pub link: String,
    /// Stable identity for seen-state (feed `<guid>`, falling back to the link).
    pub guid: String,
    /// magnet: URI built from the feed's info hash, when it publishes one.
    pub magnet: Option<String>,
    pub size: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    /// Unix seconds from `<pubDate>`.
    pub published: Option<i64>,
    pub media_id: Option<i64>,
    pub matched: Option<String>,
    pub episode: Option<i64>,
    pub is_new: bool,
    pub seen: bool,
}

/// AniList-computed profile statistics (`User.statistics.anime`). Server-side
/// aggregates, so they cover the whole list regardless of what's cached locally.
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

/// One video file found by the library scan. `media_id`/`matched`/`episode` are
/// None when the filename didn't match anything on the user's list. `bound` marks
/// a match that came from a manual file/folder link rather than the recognizer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LibraryFile {
    pub path: String,
    pub media_id: Option<i64>,
    /// Display title of the matched list entry.
    pub matched: Option<String>,
    pub episode: Option<i64>,
    #[serde(default)]
    pub bound: bool,
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
    /// Media title (user-preferred language) + cover for media-type notifications,
    /// so the row reads like the anilist.co/notifications entry.
    pub media_title: Option<String>,
    pub media_cover: Option<String>,
    pub episode: Option<i64>,
    pub activity_id: Option<i64>,
    pub thread_id: Option<i64>,
    pub thread_title: Option<String>,
    pub comment_id: Option<i64>,
    pub reason: Option<String>,
    pub deleted_media_title: Option<String>,
    pub user_name: Option<String>,
    pub user_avatar: Option<String>,
}

/// Drift guard for the hand-maintained TS mirror: assert every field name
/// `value` (a serialized command/event payload) carries is declared somewhere
/// in src/lib/types.ts. A Rust rename or addition that would land as
/// `undefined` in the UI fails here instead. Shared with playback.rs for its
/// event payload structs.
#[cfg(test)]
pub(crate) fn assert_ts_declares(name: &str, value: &serde_json::Value) {
    let ts = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/lib/types.ts"))
        .expect("read src/lib/types.ts");
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{name} must serialize to a JSON object"));
    for key in obj.keys() {
        assert!(
            ts.contains(&format!("{key}:")) || ts.contains(&format!("{key}?:")),
            "{name}.{key} is serialized to the frontend but not declared in src/lib/types.ts"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every model a command returns, serialized with all keys present (serde
    /// keeps `None` fields as null), checked against the TS mirror.
    #[test]
    fn serialized_field_names_exist_in_types_ts() {
        let models: Vec<(&str, serde_json::Value)> = vec![
            ("Media", serde_json::to_value(Media::default()).unwrap()),
            ("ListEntry", serde_json::to_value(ListEntry::default()).unwrap()),
            (
                "MediaRelation",
                serde_json::to_value(MediaRelation { relation: String::new(), media: Media::default() }).unwrap(),
            ),
            ("MediaCharacter", serde_json::to_value(MediaCharacter::default()).unwrap()),
            ("MediaStaff", serde_json::to_value(MediaStaff::default()).unwrap()),
            (
                "MediaDetail",
                serde_json::to_value(MediaDetail {
                    media: Media::default(),
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                })
                .unwrap(),
            ),
            (
                "AiringItem",
                serde_json::to_value(AiringItem { airing_at: 0, episode: 0, media: Media::default() }).unwrap(),
            ),
            ("TorrentItem", serde_json::to_value(TorrentItem::default()).unwrap()),
            ("UserStats", serde_json::to_value(UserStats::default()).unwrap()),
            ("ScoreBucket", serde_json::to_value(ScoreBucket::default()).unwrap()),
            ("StatusCount", serde_json::to_value(StatusCount::default()).unwrap()),
            ("FormatCount", serde_json::to_value(FormatCount::default()).unwrap()),
            ("GenreStat", serde_json::to_value(GenreStat::default()).unwrap()),
            ("YearCount", serde_json::to_value(YearCount::default()).unwrap()),
            ("LibraryFile", serde_json::to_value(LibraryFile::default()).unwrap()),
            ("User", serde_json::to_value(User::default()).unwrap()),
            ("Notification", serde_json::to_value(Notification::default()).unwrap()),
            (
                "TrackingConfig",
                serde_json::to_value(crate::commands::TrackingConfig::default()).unwrap(),
            ),
        ];
        for (name, value) in &models {
            assert_ts_declares(name, value);
        }
    }
}
