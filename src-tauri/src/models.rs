//! Data types shared between the AniList client, the local DB, and the frontend.
//! serde and Tauri serialize every command return.

use serde::{Deserialize, Serialize};

/// AniList media list status. Matches the API enum values exactly, in PascalCase.
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

/// A cached anime entry. Fields we actually show in the UI. AniList returns far
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
    /// Next episode that hasn't aired yet. AniList calls this nextAiringEpisode.
    pub next_airing_episode: Option<i64>,
    /// When that next episode airs, in Unix seconds. None means unknown or finished.
    pub next_airing_at: Option<i64>,
    // Detail-only fields. Fetched by media_detail, not the lean list queries.
    // The DB upsert COALESCEs them so a lean re-fetch never wipes cached values.
    pub banner_image: Option<String>,
    pub genres: Option<Vec<String>>,
    /// Episode length in minutes.
    pub duration: Option<i64>,
    /// Adaptation source. MANGA, LIGHT_NOVEL, ORIGINAL, and so on.
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

/// One row of the user's AniList anime list. Only the bits we track locally.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListEntry {
    pub id: Option<i64>,          // AniList list-entry id. The row, not the media.
    pub media_id: i64,            // the anime
    pub status: String,           // ListStatus as a string for the frontend
    pub progress: i64,
    pub score: Option<f64>,
    pub repeat: i64,
    pub updated_at: Option<i64>,
    pub media: Option<Media>,     // joined when served to the UI
}

/// One anime related to another, an AniList relations edge, shown on the detail
/// page. relation is the raw edge type. SEQUEL, PREQUEL, SIDE_STORY, and so on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaRelation {
    pub relation: String,
    pub media: Media,
}

/// One character on the detail page, with their Japanese voice actor.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaCharacter {
    /// MAIN, SUPPORTING, or BACKGROUND.
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
    pub va_name: Option<String>,
    pub va_image: Option<String>,
}

/// One staff credit on the detail page. role is free text, like Director.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MediaStaff {
    pub role: Option<String>,
    pub name: String,
    pub image: Option<String>,
}

/// Full detail-page payload. The rich media plus its anime relations and
/// credits. Characters and staff are not cached. Offline fallback serves them empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaDetail {
    pub media: Media,
    pub relations: Vec<MediaRelation>,
    #[serde(default)]
    pub characters: Vec<MediaCharacter>,
    #[serde(default)]
    pub staff: Vec<MediaStaff>,
}

/// One scheduled episode airing, for the calendar view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiringItem {
    pub airing_at: i64,
    pub episode: i64,
    pub media: Media,
}

/// One RSS feed entry, matched against the local list. is_new means matched,
/// the parsed episode is past the entry's progress, and the item hasn't been
/// marked seen. Unmatched items ride along with media_id None.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentItem {
    pub title: String,
    /// The feed link. For nyaa-style feeds this is the .torrent download URL.
    pub link: String,
    /// Stable identity for seen-state. feed guid, falling back to the link.
    pub guid: String,
    /// magnet URI built from the feed's info hash, when it publishes one.
    pub magnet: Option<String>,
    pub size: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    /// Unix seconds from pubDate.
    pub published: Option<i64>,
    pub media_id: Option<i64>,
    pub matched: Option<String>,
    pub episode: Option<i64>,
    pub is_new: bool,
    pub seen: bool,
}

/// A torrent refresh. The merged items plus whichever feeds did not answer. A
/// dead feed used to be invisible as long as one other feed worked, so a
/// mistyped or moved feed URL looked like "nothing new today" forever.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TorrentFetch {
    pub items: Vec<TorrentItem>,
    pub failures: Vec<FeedFailure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeedFailure {
    pub url: String,
    pub error: String,
}

/// AniList-computed profile statistics, from User.statistics.anime. Server-side
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

/// One library scan: the recognized files plus any configured root that could
/// not be read. An unmounted drive used to just contribute zero files.
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
    /// The user's preferred score format. POINT_100, POINT_10_DECIMAL,
    /// POINT_10, POINT_5, or POINT_3 smiley. Drives the score UI.
    pub score_format: Option<String>,
}

/// A flattened AniList notification. The API returns a union of about 14 concrete
/// types. We capture the fields we care about and leave the rest None. kind is
/// the type enum. AIRING, FOLLOWING, ACTIVITY_LIKE, and so on.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub context: Option<String>,
    pub created_at: Option<i64>,
    pub media_id: Option<i64>,
    /// Media title in user-preferred language, plus cover for media-type
    /// notifications, so the row reads like the anilist.co/notifications entry.
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

/// Drift guard for the hand-maintained TS mirror. Assert every field name
/// value, a serialized command or event payload, is declared on the matching
/// interface in src/lib/types.ts, and that every field of that interface is
/// still serialized. A Rust rename, addition, or removal that would land as
/// undefined in the UI fails here instead. Shared with playback.rs for its
/// event payload structs.
#[cfg(test)]
pub(crate) fn assert_ts_declares(name: &str, value: &serde_json::Value) {
    let ts = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/lib/types.ts"))
        .expect("read src/lib/types.ts");
    assert_ts_declares_in(&ts, name, value);
}

/// The check itself, split from the file read so tests can run it on scratch TS.
#[cfg(test)]
fn assert_ts_declares_in(ts: &str, name: &str, value: &serde_json::Value) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{name} must serialize to a JSON object"));
    let Some(body) = ts_interface_body(ts, name) else {
        // No interface of that name. Some structs are mirrored as inline
        // object types, like ScoreBucket inside UserStats. File-wide check.
        for key in obj.keys() {
            assert!(
                ts.contains(&format!("{key}:")) || ts.contains(&format!("{key}?:")),
                "{name}.{key} is serialized to the frontend but not declared in src/lib/types.ts"
            );
        }
        return;
    };
    let ts_fields = ts_top_level_fields(&body);
    for key in obj.keys() {
        assert!(
            ts_fields.contains(key),
            "{name}.{key} is serialized to the frontend but not declared on interface {name} in src/lib/types.ts"
        );
    }
    for field in &ts_fields {
        assert!(
            obj.contains_key(field),
            "interface {name}.{field} is declared in src/lib/types.ts but no longer serialized by the Rust struct"
        );
    }
}

/// The body of `interface <name> { ... }`. Brace-depth counted so an inline
/// object type like scores: { score: number }[] doesn't end it early. None when
/// no such interface exists.
#[cfg(test)]
fn ts_interface_body(ts: &str, name: &str) -> Option<String> {
    let marker = format!("interface {name} {{");
    let open = ts.find(&marker)? + marker.len() - 1; // byte index of the opening brace
    let mut depth = 0;
    for (i, c) in ts[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(ts[open + 1..open + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Field names declared at the top level of a TS interface body. The brace
/// depth keeps fields of inline object types out. // comments are skipped so
/// a word: inside one can't read as a field.
#[cfg(test)]
fn ts_top_level_fields(body: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut depth = 0;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            '/' if depth == 0 && chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            _ if depth == 0 && (c.is_alphabetic() || c == '_') => {
                let mut name = String::from(c);
                while let Some(&nc) = chars.peek() {
                    if nc.is_alphanumeric() || nc == '_' {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&'?') {
                    chars.next();
                }
                if chars.peek() == Some(&':') {
                    fields.push(name);
                }
            }
            _ => {}
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every model a command returns, serialized with all keys present. serde
    /// keeps None fields as null. Checked against the TS mirror.
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

    /// C28 regression. A renamed-away field still matches some other interface
    /// file-wide. The interface-scoped check must fail anyway.
    #[test]
    #[should_panic(expected = "not declared on interface Widget")]
    fn drift_guard_catches_rename() {
        let ts = "export interface Widget {\n  id: number;\n}\n\nexport interface Other {\n  name: string;\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1, "name": "x" }));
    }

    /// The reverse direction. A field the Rust struct stopped serializing.
    #[test]
    #[should_panic(expected = "no longer serialized")]
    fn drift_guard_catches_removal() {
        let ts = "export interface Widget {\n  id: number;\n  gone?: string;\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1 }));
    }

    /// Inline object types neither end the interface early nor leak their inner
    /// fields into the comparison.
    #[test]
    fn drift_guard_handles_inline_object_types() {
        let ts = "export interface Widget {\n  id: number;\n  buckets: { score: number; count: number }[];\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1, "buckets": [] }));
    }
}
