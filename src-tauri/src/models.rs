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
    pub fn display_title(&self) -> String {
        self.title_english
            .clone()
            .or_else(|| self.title_romaji.clone())
            .or_else(|| self.title_native.clone())
            .unwrap_or_else(|| format!("#{}", self.id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListEntry {
    pub id: Option<i64>, // AniList list entry ID
    pub media_id: i64,
    pub status: String,
    pub progress: i64,
    pub score: Option<f64>,
    pub repeat: i64,
    pub updated_at: Option<i64>,
    pub media: Option<Media>,
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
    /// Feed GUID, falling back to the link.
    pub guid: String,
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
pub(crate) fn assert_ts_declares(name: &str, value: &serde_json::Value) {
    let ts = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/lib/types.ts"))
        .expect("read src/lib/types.ts");
    assert_ts_declares_in(&ts, name, value);
}

#[cfg(test)]
fn assert_ts_declares_in(ts: &str, name: &str, value: &serde_json::Value) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{name} must serialize to a JSON object"));
    let Some(body) = ts_interface_body(ts, name) else {
        // Some models use inline types rather than named interfaces.
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

#[cfg(test)]
fn strip_ts_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            q @ ('"' | '\'' | '`') => {
                out.push(q);
                let mut escaped = false;
                for c in chars.by_ref() {
                    out.push(c);
                    if escaped {
                        escaped = false;
                    } else if c == '\\' {
                        escaped = true;
                    } else if c == q {
                        break;
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
fn ts_interface_body(ts: &str, name: &str) -> Option<String> {
    let ts = strip_ts_comments(ts);
    let marker = format!("interface {name} {{");
    let open = ts.find(&marker)? + marker.len() - 1;
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

#[cfg(test)]
fn ts_top_level_fields(body: &str) -> Vec<String> {
    let body = strip_ts_comments(body);
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

    #[test]
    fn serialized_field_names_exist_in_types_ts() {
        let models: Vec<(&str, serde_json::Value)> = vec![
            ("Media", serde_json::to_value(Media::default()).unwrap()),
            (
                "ListEntry",
                serde_json::to_value(ListEntry::default()).unwrap(),
            ),
            (
                "MediaRelation",
                serde_json::to_value(MediaRelation {
                    relation: String::new(),
                    media: Media::default(),
                })
                .unwrap(),
            ),
            (
                "MediaCharacter",
                serde_json::to_value(MediaCharacter::default()).unwrap(),
            ),
            (
                "MediaStaff",
                serde_json::to_value(MediaStaff::default()).unwrap(),
            ),
            (
                "MediaDetail",
                serde_json::to_value(MediaDetail {
                    media: Media::default(),
                    relations: vec![],
                    characters: vec![],
                    staff: vec![],
                    cached_at: None,
                    warning: None,
                    unavailable_sections: vec![],
                })
                .unwrap(),
            ),
            (
                "AiringItem",
                serde_json::to_value(AiringItem {
                    airing_at: 0,
                    episode: 0,
                    media: Media::default(),
                })
                .unwrap(),
            ),
            (
                "TorrentItem",
                serde_json::to_value(TorrentItem::default()).unwrap(),
            ),
            (
                "UserStats",
                serde_json::to_value(UserStats::default()).unwrap(),
            ),
            (
                "ScoreBucket",
                serde_json::to_value(ScoreBucket::default()).unwrap(),
            ),
            (
                "StatusCount",
                serde_json::to_value(StatusCount::default()).unwrap(),
            ),
            (
                "FormatCount",
                serde_json::to_value(FormatCount::default()).unwrap(),
            ),
            (
                "GenreStat",
                serde_json::to_value(GenreStat::default()).unwrap(),
            ),
            (
                "YearCount",
                serde_json::to_value(YearCount::default()).unwrap(),
            ),
            (
                "LibraryFile",
                serde_json::to_value(LibraryFile::default()).unwrap(),
            ),
            ("User", serde_json::to_value(User::default()).unwrap()),
            (
                "Notification",
                serde_json::to_value(Notification::default()).unwrap(),
            ),
            (
                "TrackingConfig",
                serde_json::to_value(crate::commands::TrackingConfig::default()).unwrap(),
            ),
            (
                "TorrentFetch",
                serde_json::to_value(TorrentFetch::default()).unwrap(),
            ),
            (
                "ShowTorrents",
                serde_json::to_value(ShowTorrents::default()).unwrap(),
            ),
            (
                "FeedFailure",
                serde_json::to_value(FeedFailure::default()).unwrap(),
            ),
            (
                "LibraryScan",
                serde_json::to_value(LibraryScan::default()).unwrap(),
            ),
            (
                "UnreadableFolder",
                serde_json::to_value(UnreadableFolder::default()).unwrap(),
            ),
        ];
        for (name, value) in &models {
            assert_ts_declares(name, value);
        }
    }

    #[test]
    #[should_panic(expected = "not declared on interface Widget")]
    fn drift_guard_catches_rename() {
        let ts = "export interface Widget {\n  id: number;\n}\n\nexport interface Other {\n  name: string;\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1, "name": "x" }));
    }

    #[test]
    #[should_panic(expected = "no longer serialized")]
    fn drift_guard_catches_removal() {
        let ts = "export interface Widget {\n  id: number;\n  gone?: string;\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1 }));
    }

    #[test]
    fn drift_guard_handles_inline_object_types() {
        let ts = "export interface Widget {\n  id: number;\n  buckets: { score: number; count: number }[];\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1, "buckets": [] }));
    }

    #[test]
    fn drift_guard_ignores_block_comments() {
        let ts = "export interface Widget {\n  /* gone: string; */\n  id: number;\n  /* a } hiding in a comment */\n  name: string;\n}\n";
        assert_ts_declares_in(ts, "Widget", &serde_json::json!({ "id": 1, "name": "x" }));
    }

    #[test]
    fn strip_ts_comments_leaves_strings_alone() {
        let stripped = strip_ts_comments("let x = \"/* nope */\"; /* real\ncomment */ let y = 1;");
        assert!(stripped.contains("\"/* nope */\""));
        assert!(!stripped.contains("real"));
        assert!(stripped.contains("let y = 1;"));
    }
}
