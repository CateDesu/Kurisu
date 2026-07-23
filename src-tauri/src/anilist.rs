//! AniList v2 GraphQL client + desktop OAuth2 (implicit flow via a localhost
//! callback server). The browser gets the token in a URL fragment (#access_token),
//! which it never sends to a server, so the callback page runs a few lines of JS
//! that move the fragment into the query string and re-request — at which point our
//! listener captures it. Avoids shipping a client_secret.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::models::{
    AiringItem, FormatCount, GenreStat, ListEntry, ListStatus, Media, MediaCharacter,
    MediaRelation, MediaStaff, Notification, ScoreBucket, StatusCount, User, UserStats, YearCount,
};

const GRAPHQL: &str = "https://graphql.anilist.co";
const AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
/// Fixed port so the user registers ONE redirect_uri in their AniList client once.
pub const OAUTH_PORT: u16 = 39417;

/// `nextAiringEpisode { episode airingAt }` — shared by the search + list queries.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextAiring {
    episode: Option<i64>,
    airing_at: Option<i64>,
}

/// The media field set every list-ish query fetches (search / user_list / season /
/// recommendations) — one deserializer + conversion shared by all of them. The
/// detail-only fields (banner/genres/duration/source/studios) are Options the lean
/// queries simply never populate.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AniMedia {
    id: i64,
    id_mal: Option<i64>,
    title: AniTitle,
    cover_image: AniCover,
    episodes: Option<i64>,
    format: Option<String>,
    status: Option<String>,
    average_score: Option<i64>,
    season: Option<String>,
    season_year: Option<i64>,
    description: Option<String>,
    next_airing_episode: Option<NextAiring>,
    banner_image: Option<String>,
    // AniList types the list's ITEMS as nullable too — a stray null must not
    // fail the whole query.
    genres: Option<Vec<Option<String>>>,
    duration: Option<i64>,
    source: Option<String>,
    studios: Option<AniStudios>,
}
#[derive(Deserialize, Default)]
struct AniTitle {
    romaji: Option<String>,
    english: Option<String>,
    native: Option<String>,
}
#[derive(Deserialize, Default)]
struct AniCover {
    medium: Option<String>,
    large: Option<String>,
}
#[derive(Deserialize)]
struct AniStudios {
    nodes: Option<Vec<AniStudioNode>>,
}
#[derive(Deserialize)]
struct AniStudioNode {
    name: String,
}

impl From<AniMedia> for Media {
    fn from(m: AniMedia) -> Media {
        Media {
            id: m.id,
            id_mal: m.id_mal,
            title_romaji: m.title.romaji,
            title_english: m.title.english,
            title_native: m.title.native,
            cover_medium: m.cover_image.medium,
            cover_large: m.cover_image.large,
            episodes: m.episodes,
            format: m.format,
            status: m.status,
            average_score: m.average_score,
            season: m.season,
            season_year: m.season_year,
            description: m.description,
            next_airing_episode: m.next_airing_episode.as_ref().and_then(|n| n.episode),
            next_airing_at: m.next_airing_episode.as_ref().and_then(|n| n.airing_at),
            banner_image: m.banner_image,
            genres: m
                .genres
                .map(|g| g.into_iter().flatten().collect::<Vec<_>>())
                .filter(|g: &Vec<String>| !g.is_empty()),
            duration: m.duration,
            source: m.source,
            studios: m
                .studios
                .and_then(|s| s.nodes)
                .map(|n| n.into_iter().map(|s| s.name).collect::<Vec<_>>())
                .filter(|s: &Vec<String>| !s.is_empty()),
        }
    }
}

/// reqwest::Client is cheap to clone (Arc-backed), so cloning AniList lets us drop
/// the DB-style lock before any `.await` (Tauri futures must be Send).
#[derive(Clone)]
pub struct AniList {
    http: reqwest::Client,
    token: Option<String>,
}

impl AniList {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("Kurisu")
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        AniList { http, token: None }
    }
    pub fn set_token(&mut self, t: Option<String>) {
        self.token = t;
    }
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    async fn gql<T: for<'de> serde::Deserialize<'de>>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T> {
        let token = self
            .token
            .as_ref()
            .ok_or_else(|| anyhow!("not authenticated"))?;
        let resp = self
            .http
            .post(GRAPHQL)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "query": query, "variables": vars }))
            .send()
            .await?;
        // AniList error envelope: { "errors": [ { "message": "..." } ] }
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("AniList ({}): {}", status, e))?;
        if let Some(errs) = body.get("errors") {
            let msg = errs
                .get(0)
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown AniList error");
            return Err(anyhow!("AniList ({}): {}", status, msg));
        }
        // unwrap the "data" object before deserializing into T
        let data = body
            .get("data")
            .ok_or_else(|| anyhow!("AniList: no data field"))?
            .clone();
        Ok(serde_json::from_value(data)?)
    }

    /// `Viewer` = the authenticated user. Used to verify the token + fetch the name.
    pub async fn viewer(&self) -> Result<User> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Viewer")]
            viewer: Inner,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Inner {
            id: i64,
            name: String,
            avatar: Option<Avatar>,
            media_list_options: Option<MediaListOptions>,
        }
        #[derive(Deserialize)]
        struct Avatar {
            large: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MediaListOptions {
            score_format: Option<String>,
        }
        let r: R = self
            .gql(
                "query { Viewer { id name avatar { large } mediaListOptions { scoreFormat } } }",
                serde_json::json!({}),
            )
            .await?;
        Ok(User {
            id: r.viewer.id,
            name: r.viewer.name,
            avatar: r.viewer.avatar.and_then(|a| a.large),
            score_format: r.viewer.media_list_options.and_then(|o| o.score_format),
        })
    }

    pub async fn search(&self, query: &str, per_page: i64) -> Result<Vec<Media>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        struct Page {
            media: Vec<AniMedia>,
        }
        let q = "query ($search: String!, $perPage: Int!) {
            Page(perPage: $perPage) {
                media(search: $search, type: ANIME, sort: SEARCH_MATCH) {
                    id idMal title { romaji english native }
                    coverImage { medium large }
                    episodes format status averageScore season seasonYear description
                    nextAiringEpisode { episode airingAt }
                }
            }
        }";
        let r: R = self
            .gql(
                q,
                serde_json::json!({ "search": query, "perPage": per_page }),
            )
            .await?;
        Ok(r.page.media.into_iter().map(Media::from).collect())
    }

    /// One anime season (WINTER/SPRING/SUMMER/FALL + year), most popular first.
    pub async fn season(&self, season: &str, year: i64, page: i64) -> Result<Vec<Media>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        struct Page {
            media: Vec<AniMedia>,
        }
        let q = "query ($season: MediaSeason!, $year: Int!, $page: Int!) {
            Page(page: $page, perPage: 50) {
                media(season: $season, seasonYear: $year, type: ANIME, isAdult: false, sort: POPULARITY_DESC) {
                    id idMal title { romaji english native }
                    coverImage { medium large }
                    episodes format status averageScore season seasonYear description
                    nextAiringEpisode { episode airingAt }
                }
            }
        }";
        let r: R = self
            .gql(
                q,
                serde_json::json!({ "season": season, "year": year, "page": page }),
            )
            .await?;
        Ok(r.page.media.into_iter().map(Media::from).collect())
    }

    /// AniList's community recommendations for one title, best-rated first.
    pub async fn recommendations(&self, media_id: i64) -> Result<Vec<Media>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        struct Page {
            recommendations: Option<Vec<Rec>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Rec {
            media_recommendation: Option<AniMedia>,
        }
        let q = "query ($mediaId: Int!) {
            Page(perPage: 10) {
                recommendations(mediaId: $mediaId, sort: RATING_DESC) {
                    mediaRecommendation {
                        id idMal title { romaji english native }
                        coverImage { medium large }
                        episodes format status averageScore season seasonYear description
                        nextAiringEpisode { episode airingAt }
                    }
                }
            }
        }";
        let r: R = self.gql(q, serde_json::json!({ "mediaId": media_id })).await?;
        Ok(r.page
            .recommendations
            .unwrap_or_default()
            .into_iter()
            .filter_map(|rec| rec.media_recommendation)
            .map(Media::from)
            .collect())
    }

    /// One anime by AniList id (the `get_media` cache-miss fallback — full-text
    /// `search` can't look up by id).
    pub async fn media_by_id(&self, id: i64) -> Result<Media> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Media")]
            media: AniMedia,
        }
        let q = "query ($id: Int!) {
            Media(id: $id, type: ANIME) {
                id idMal title { romaji english native }
                coverImage { medium large }
                episodes format status averageScore season seasonYear description
                nextAiringEpisode { episode airingAt }
            }
        }";
        let r: R = self.gql(q, serde_json::json!({ "id": id })).await?;
        Ok(Media::from(r.media))
    }

    /// One anime with the full detail-page field set plus its anime relations
    /// (sequels/prequels/side stories — manga nodes are dropped, the app is
    /// anime-only and can't open them), main characters (with their Japanese
    /// voice actors), and key staff credits.
    pub async fn media_detail(
        &self,
        id: i64,
    ) -> Result<(Media, Vec<MediaRelation>, Vec<MediaCharacter>, Vec<MediaStaff>)> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Media")]
            media: DetailMedia,
        }
        #[derive(Deserialize)]
        struct DetailMedia {
            relations: Option<Relations>,
            characters: Option<CharConn>,
            staff: Option<StaffConn>,
            #[serde(flatten)]
            media: AniMedia,
        }
        #[derive(Deserialize)]
        struct Relations {
            edges: Option<Vec<RelEdge>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RelEdge {
            relation_type: Option<String>,
            node: Option<RelNode>,
        }
        #[derive(Deserialize)]
        struct RelNode {
            #[serde(rename = "type")]
            kind: Option<String>,
            #[serde(flatten)]
            media: AniMedia,
        }
        #[derive(Deserialize)]
        struct CharConn {
            edges: Option<Vec<CharEdge>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CharEdge {
            role: Option<String>,
            node: Option<NamedNode>,
            voice_actors: Option<Vec<NamedNode>>,
        }
        #[derive(Deserialize)]
        struct StaffConn {
            edges: Option<Vec<StaffEdge>>,
        }
        #[derive(Deserialize)]
        struct StaffEdge {
            role: Option<String>,
            node: Option<NamedNode>,
        }
        #[derive(Deserialize)]
        struct NamedNode {
            name: Option<NodeName>,
            image: Option<NodeImage>,
        }
        #[derive(Deserialize)]
        struct NodeName {
            full: Option<String>,
        }
        #[derive(Deserialize)]
        struct NodeImage {
            medium: Option<String>,
        }
        let q = "query ($id: Int!) {
            Media(id: $id, type: ANIME) {
                id idMal title { romaji english native }
                coverImage { medium large } bannerImage
                episodes duration format status source averageScore season seasonYear description
                genres studios(isMain: true) { nodes { name } }
                nextAiringEpisode { episode airingAt }
                relations {
                    edges {
                        relationType
                        node {
                            type
                            id idMal title { romaji english native }
                            coverImage { medium large }
                            episodes format status averageScore season seasonYear description
                            nextAiringEpisode { episode airingAt }
                        }
                    }
                }
                characters(sort: [ROLE, RELEVANCE, ID], perPage: 12) {
                    edges {
                        role
                        node { name { full } image { medium } }
                        voiceActors(language: JAPANESE, sort: [RELEVANCE, ID]) { name { full } image { medium } }
                    }
                }
                staff(sort: [RELEVANCE, ID], perPage: 8) {
                    edges { role node { name { full } image { medium } } }
                }
            }
        }";
        let r: R = self.gql(q, serde_json::json!({ "id": id })).await?;
        let relations = r
            .media
            .relations
            .and_then(|rel| rel.edges)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let node = e.node?;
                if node.kind.as_deref() != Some("ANIME") {
                    return None;
                }
                Some(MediaRelation {
                    relation: e.relation_type.unwrap_or_else(|| "OTHER".to_string()),
                    media: node.media.into(),
                })
            })
            .collect();
        let characters = r
            .media
            .characters
            .and_then(|c| c.edges)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let node = e.node?;
                let name = node.name.and_then(|n| n.full)?;
                let va = e.voice_actors.unwrap_or_default().into_iter().next();
                Some(MediaCharacter {
                    role: e.role,
                    name,
                    image: node.image.and_then(|i| i.medium),
                    va_name: va.as_ref().and_then(|v| v.name.as_ref()).and_then(|n| n.full.clone()),
                    va_image: va.and_then(|v| v.image).and_then(|i| i.medium),
                })
            })
            .collect();
        let staff = r
            .media
            .staff
            .and_then(|s| s.edges)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                let node = e.node?;
                let name = node.name.and_then(|n| n.full)?;
                Some(MediaStaff {
                    role: e.role,
                    name,
                    image: node.image.and_then(|i| i.medium),
                })
            })
            .collect();
        Ok((r.media.media.into(), relations, characters, staff))
    }

    /// AniList's server-side profile statistics for a user. Whole-list
    /// aggregates (counts, time watched, score/status/format/genre/year
    /// breakdowns) — nothing to compute locally.
    pub async fn user_statistics(&self, user_name: &str) -> Result<UserStats> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "User")]
            user: UserNode,
        }
        #[derive(Deserialize)]
        struct UserNode {
            statistics: Option<Statistics>,
        }
        #[derive(Deserialize)]
        struct Statistics {
            anime: Option<Anime>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Anime {
            count: i64,
            episodes_watched: i64,
            minutes_watched: i64,
            mean_score: f64,
            standard_deviation: f64,
            scores: Option<Vec<Score>>,
            statuses: Option<Vec<Status>>,
            formats: Option<Vec<Format>>,
            genres: Option<Vec<Genre>>,
            release_years: Option<Vec<Year>>,
        }
        #[derive(Deserialize)]
        struct Score {
            score: i64,
            count: i64,
        }
        #[derive(Deserialize)]
        struct Status {
            status: Option<String>,
            count: i64,
        }
        #[derive(Deserialize)]
        struct Format {
            format: Option<String>,
            count: i64,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Genre {
            genre: Option<String>,
            count: i64,
            minutes_watched: Option<i64>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Year {
            release_year: Option<i64>,
            count: i64,
        }
        let q = "query ($name: String!) {
            User(name: $name) {
                statistics {
                    anime {
                        count episodesWatched minutesWatched meanScore standardDeviation
                        scores { score count }
                        statuses { status count }
                        formats { format count }
                        genres(limit: 12, sort: COUNT_DESC) { genre count minutesWatched }
                        releaseYears { releaseYear count }
                    }
                }
            }
        }";
        let r: R = self.gql(q, serde_json::json!({ "name": user_name })).await?;
        let a = r
            .user
            .statistics
            .and_then(|s| s.anime)
            .unwrap_or_default();
        let mut release_years: Vec<YearCount> = a
            .release_years
            .unwrap_or_default()
            .into_iter()
            .filter_map(|y| Some(YearCount { year: y.release_year?, count: y.count }))
            .collect();
        release_years.sort_by_key(|y| y.year);
        Ok(UserStats {
            count: a.count,
            episodes_watched: a.episodes_watched,
            minutes_watched: a.minutes_watched,
            mean_score: a.mean_score,
            standard_deviation: a.standard_deviation,
            scores: a
                .scores
                .unwrap_or_default()
                .into_iter()
                .map(|s| ScoreBucket { score: s.score, count: s.count })
                .collect(),
            statuses: a
                .statuses
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| Some(StatusCount { status: s.status?, count: s.count }))
                .collect(),
            formats: a
                .formats
                .unwrap_or_default()
                .into_iter()
                .filter_map(|f| Some(FormatCount { format: f.format?, count: f.count }))
                .collect(),
            genres: a
                .genres
                .unwrap_or_default()
                .into_iter()
                .filter_map(|g| {
                    Some(GenreStat {
                        genre: g.genre?,
                        count: g.count,
                        minutes_watched: g.minutes_watched.unwrap_or(0),
                    })
                })
                .collect(),
            release_years,
        })
    }

    /// Every episode airing in [start, end) (Unix seconds), in airing order.
    /// Pages through AniList's 50-per-page chunks; capped at 12 pages (600
    /// entries — more than any real week) so a bad range can't loop forever.
    /// Adult titles are dropped.
    pub async fn airing_schedule(&self, start: i64, end: i64) -> Result<Vec<AiringItem>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            page_info: Option<PageInfo>,
            airing_schedules: Option<Vec<Sched>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: Option<bool>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Sched {
            airing_at: i64,
            episode: i64,
            media: Option<SchedMedia>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct SchedMedia {
            is_adult: Option<bool>,
            #[serde(flatten)]
            media: AniMedia,
        }
        let q = "query ($start: Int!, $end: Int!, $page: Int!) {
            Page(page: $page, perPage: 50) {
                pageInfo { hasNextPage }
                airingSchedules(airingAt_greater: $start, airingAt_lesser: $end, sort: TIME) {
                    airingAt episode
                    media {
                        isAdult
                        id idMal title { romaji english native }
                        coverImage { medium large }
                        episodes format status averageScore season seasonYear description
                        nextAiringEpisode { episode airingAt }
                    }
                }
            }
        }";
        let mut out = Vec::new();
        for page in 1..=12 {
            let r: R = self
                .gql(q, serde_json::json!({ "start": start, "end": end, "page": page }))
                .await?;
            let scheds = r.page.airing_schedules.unwrap_or_default();
            for s in scheds {
                let Some(m) = s.media else { continue };
                if m.is_adult == Some(true) {
                    continue;
                }
                out.push(AiringItem {
                    airing_at: s.airing_at,
                    episode: s.episode,
                    media: m.media.into(),
                });
            }
            if !r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .unwrap_or(false)
            {
                break;
            }
        }
        Ok(out)
    }

    /// Pull the full list (every status group) for a user and flatten to entries.
    /// AniList chunks big lists at 500 entries per status group — walk the chunks
    /// via `hasNextChunk` or large accounts sync an incomplete list.
    pub async fn user_list(&self, user_name: &str) -> Result<Vec<ListEntry>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "MediaListCollection")]
            collection: Collection,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Collection {
            lists: Option<Vec<AniList>>,
            has_next_chunk: Option<bool>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AniList {
            #[allow(dead_code)]
            status: Option<String>,
            entries: Vec<Entry>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            id: i64,
            status: String,
            progress: i64,
            score: Option<f64>,
            repeat: Option<i64>,
            updated_at: Option<i64>,
            media: AniMedia,
        }
        let q = "query ($userName: String!, $chunk: Int!) {
            MediaListCollection(userName: $userName, type: ANIME, chunk: $chunk) {
                hasNextChunk
                lists {
                    status
                    entries {
                        id status progress score repeat updatedAt
                        media {
                            id idMal title { romaji english native }
                            coverImage { medium large }
                            episodes format status averageScore season seasonYear description
                            nextAiringEpisode { episode airingAt }
                        }
                    }
                }
            }
        }";
        let mut out = Vec::new();
        let mut chunk = 1;
        loop {
            let r: R = self
                .gql(q, serde_json::json!({ "userName": user_name, "chunk": chunk }))
                .await?;
            for list in r.collection.lists.unwrap_or_default() {
                for e in list.entries {
                    out.push(ListEntry {
                        id: Some(e.id),
                        media_id: e.media.id,
                        status: e.status,
                        progress: e.progress,
                        score: e.score,
                        repeat: e.repeat.unwrap_or(0),
                        updated_at: e.updated_at,
                        media: Some(e.media.into()),
                    });
                }
            }
            if !r.collection.has_next_chunk.unwrap_or(false) {
                break;
            }
            chunk += 1;
        }
        Ok(out)
    }

    /// Create or update an entry. Returns the (entry_id) AniList assigned.
    pub async fn save_entry(
        &self,
        media_id: i64,
        status: ListStatus,
        progress: i64,
        score: Option<f64>,
        repeat: i64,
    ) -> Result<i64> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "SaveMediaListEntry")]
            entry: Entry,
        }
        #[derive(Deserialize)]
        struct Entry {
            id: i64,
        }
        let q = "mutation ($mediaId: Int!, $status: MediaListStatus!, $progress: Int!, $score: Float, $repeat: Int) {
            SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, score: $score, repeat: $repeat) { id }
        }";
        let r: R = self
            .gql(
                q,
                serde_json::json!({
                    "mediaId": media_id,
                    "status": status.as_str(),
                    "progress": progress,
                    "score": score,
                    "repeat": repeat,
                }),
            )
            .await?;
        Ok(r.entry.id)
    }

    pub async fn delete_entry(&self, entry_id: i64) -> Result<()> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "DeleteMediaListEntry")]
            entry: Entry,
        }
        #[derive(Deserialize)]
        struct Entry {
            deleted: bool,
        }
        let q = "mutation ($id: Int!) { DeleteMediaListEntry(id: $id) { deleted } }";
        let r: R = self.gql(q, serde_json::json!({ "id": entry_id })).await?;
        if !r.entry.deleted {
            return Err(anyhow!("AniList declined the delete"));
        }
        Ok(())
    }

    /// The user's recent notifications (union of Airing / Following / Activity /
    /// Thread / Media… types). Flattened into one struct per type with the rest
    /// left None. `reset_notification_count=false` so opening the inbox here
    /// doesn't silently clear AniList's own unread badge.
    pub async fn notifications(&self) -> Result<Vec<Notification>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            notifications: Option<Vec<Raw>>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Raw {
            id: i64,
            #[serde(rename = "type")]
            kind: String,
            context: Option<String>,
            // AiringNotification has `contexts: [String]` (plural) instead of `context`.
            contexts: Option<Vec<String>>,
            created_at: Option<i64>,
            // AniList notifications expose the anime as `media { id }`, not `mediaId`.
            media: Option<MediaRef>,
            episode: Option<i64>,
            activity_id: Option<i64>,
            thread: Option<ThreadRef>,
            comment_id: Option<i64>,
            reason: Option<String>,
            deleted_media_title: Option<String>,
            user: Option<UserRef>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MediaRef {
            id: i64,
            title: Option<MediaTitle>,
            cover_image: Option<MediaCover>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MediaTitle {
            user_preferred: Option<String>,
        }
        #[derive(Deserialize)]
        struct MediaCover {
            medium: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ThreadRef {
            id: i64,
            title: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct UserRef {
            name: String,
            avatar: Option<AvatarRef>,
        }
        #[derive(Deserialize)]
        struct AvatarRef {
            large: Option<String>,
        }
        let q = "query { Page(page: 1, perPage: 50) { notifications(resetNotificationCount: false) {
            ... on AiringNotification { id type createdAt media { id title { userPreferred } coverImage { medium } } episode contexts }
            ... on FollowingNotification { id type createdAt context user { id name avatar { large } } }
            ... on ActivityLikeNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityMentionNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplyNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplySubscribedNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplyLikeNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityMessageNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ThreadCommentMentionNotification { id type createdAt context commentId thread { id title } user { name avatar { large } } }
            ... on ThreadCommentReplyNotification { id type createdAt context commentId thread { id title } user { name avatar { large } } }
            ... on ThreadCommentSubscribedNotification { id type createdAt context commentId thread { id title } user { name avatar { large } } }
            ... on ThreadCommentLikeNotification { id type createdAt context commentId thread { id title } user { name avatar { large } } }
            ... on ThreadLikeNotification { id type createdAt context thread { id title } user { name avatar { large } } }
            ... on RelatedMediaAdditionNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } }
            ... on MediaDataChangeNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } reason }
            ... on MediaMergeNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } reason }
            ... on MediaDeletionNotification { id type createdAt context deletedMediaTitle reason }
        } } }";
        let r: R = self.gql(q, serde_json::json!({})).await?;
        Ok(r.page
            .notifications
            .unwrap_or_default()
            .into_iter()
            // A notification type this query has no fragment for comes back as
            // `{}` → kind "" and id 0. Drop it: rendering a ghost row (with a
            // duplicate `id` for the frontend's keyed each) is worse than
            // hiding it until the type is added.
            .filter(|n| !n.kind.is_empty())
            .map(|n| Notification {
                id: n.id,
                kind: n.kind,
                context: n
                    .context
                    .or_else(|| n.contexts.as_ref().filter(|v| !v.is_empty()).map(|v| v.join(" "))),
                created_at: n.created_at,
                media_id: n.media.as_ref().map(|m| m.id),
                media_title: n
                    .media
                    .as_ref()
                    .and_then(|m| m.title.as_ref())
                    .and_then(|t| t.user_preferred.clone()),
                media_cover: n
                    .media
                    .as_ref()
                    .and_then(|m| m.cover_image.as_ref())
                    .and_then(|c| c.medium.clone()),
                episode: n.episode,
                activity_id: n.activity_id,
                thread_id: n.thread.as_ref().map(|t| t.id),
                thread_title: n.thread.as_ref().and_then(|t| t.title.clone()),
                comment_id: n.comment_id,
                reason: n.reason,
                deleted_media_title: n.deleted_media_title,
                user_name: n.user.as_ref().map(|u| u.name.clone()),
                user_avatar: n.user.as_ref().and_then(|u| u.avatar.as_ref().and_then(|a| a.large.clone())),
            })
            .collect())
    }
}

// ───────────────────────── OAuth2 desktop flow (implicit) ─────────────────────────
//
// Implicit grant: no client_secret is needed (a desktop app can't keep one
// private anyway). The token arrives in the redirect URL *fragment*
// (#access_token=…), which the browser never sends to a server — so the callback
// serves a tiny HTML page whose JS lifts the fragment into a query string the
// server can read on a second request.

/// 32 random bytes from the OS CSPRNG, hex-encoded. Used as the OAuth `state` so
/// the callback can reject a token AniList didn't actually issue for THIS login
/// attempt (CSRF / token-injection via a malicious site hitting 127.0.0.1:39417).
/// No software fallback: an OAuth flow that can't randomize its state must not start.
fn random_state() -> Result<String> {
    use ring::rand::SecureRandom;
    use std::fmt::Write as _;
    let mut buf = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow!("OS random source unavailable"))?;
    let mut out = String::with_capacity(64);
    for b in &buf {
        let _ = write!(out, "{:02x}", b);
    }
    Ok(out)
}

/// Build the authorize URL the user's browser should visit (response_type=token).
/// `state` is echoed back by AniList and checked by the callback server.
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE}?client_id={cid}&response_type=token&redirect_uri={redir}&state={state}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(redirect_uri),
        state = urlencoding::encode(state),
    )
}

/// The HTML shim served on the first callback hit. It moves the URL fragment
/// (which the server can't see) into a `/__capture__?<fragment>` request that the
/// server CAN read. On a bare probe (no fragment) it just shows a "connecting" page.
const SHIM_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>body{font-family:sans-serif;text-align:center;padding:3em;color:#9aa3b2;background:#0f1115;margin:0}h2{color:#3ba55d;font-weight:600}</style></head><body><h2>Connecting to Kurisu…</h2><p>You can close this tab once the app opens.</p><script>(function(){var h=location.hash.charCodeAt(0)===35?location.hash.slice(1):location.hash;if(h.indexOf('access_token=')!==-1){location.replace('/__capture__?'+h);}})();</script></body></html>";

const OK_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#3ba55d'>Connected to Kurisu.</h2><p>You can close this tab and return to the app.</p></body>";
const ERR_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#e74c3c'>Authorization failed.</h2><p>Return to Kurisu for details.</p></body>";

/// Minimal query-value decoder: %XX escapes plus '+' as space. Today's values
/// (base64url token, hex state) contain neither, so this is a no-op — it only
/// starts mattering if AniList ever changes the token alphabet. Malformed
/// escapes pass through literally.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Start a localhost HTTP listener that captures the access token AniList sends
/// back in the implicit flow. Returns `(state, receiver)`: the caller embeds
/// `state` in the authorize URL, and the receiver resolves with the token once a
/// request passes the CSRF `state` check. AniList errors, state mismatches, and
/// stray probes are answered and LOGGED but never resolve the receiver or stop
/// the listener — any web page can fire `http://127.0.0.1:39417/?error=x`, and
/// that must not kill a login in flight. The listener shuts down when the caller
/// drops the receiver (timeout / cancel), freeing the port for a retry.
pub fn start_callback_server() -> Result<(String, oneshot::Receiver<String>)> {
    let state = random_state()?;
    let expected = state.clone();
    let (tx, rx) = oneshot::channel::<String>();
    let addr = format!("127.0.0.1:{}", OAUTH_PORT);
    let listener = std::net::TcpListener::bind(&addr)?;
    listener.set_nonblocking(true)?;
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async move {
            // `closed()` takes &mut self; rebind so the select can poll it.
            let mut tx = tx;
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                // Stop waiting when the caller dropped the receiver (login
                // timed out or was abandoned): the listener drops with this
                // task, freeing the port so a retry can bind it. Without this
                // the port stayed bound for the process's lifetime and a
                // second "Sign in" click always failed.
                let (mut sock, _) = match tokio::select! {
                    _ = tx.closed() => return,
                    acc = listener.accept() => acc,
                } {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read until the request headers are complete (or the buffer
                // fills, the peer closes, or 10s passes). One read usually
                // delivers the whole GET over loopback, but that's a TCP
                // accident, not a guarantee — and a speculative browser
                // preconnect that never sends a byte must not park the accept
                // loop until the browser gives up on the socket.
                let mut buf = [0u8; 8192];
                let mut n = 0;
                let read_headers = async {
                    loop {
                        match sock.read(&mut buf[n..]).await {
                            Ok(0) | Err(_) => break,
                            Ok(r) => {
                                n += r;
                                if n == buf.len()
                                    || buf[..n].windows(4).any(|w| w == b"\r\n\r\n")
                                {
                                    break;
                                }
                            }
                        }
                    }
                };
                let _ = tokio::time::timeout(Duration::from_secs(10), read_headers).await;
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("");
                let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
                let param = |k: &str| -> Option<String> {
                    query.split('&').find_map(|kv| {
                        let (key, val) = kv.split_once('=')?;
                        (key == k).then(|| percent_decode(val))
                    })
                };

                // 1) AniList denied → error comes in the query (?error=…).
                // 2) The shim re-requested with the fragment as a query
                //    (?access_token=…). The flow is response_type=token, so
                //    that's the ONLY parameter that can carry credentials — a
                //    `code` would need an exchange step this app doesn't have.
                // 3) Otherwise (initial implicit redirect with the token still in the
                //    fragment, or a probe) → serve the shim, don't resolve yet.
                // Failures (error param, state mismatch) answer the browser but do
                // NOT resolve the login or stop the listener: any web page can hit
                // 127.0.0.1:39417 with ?error=…, and that must not kill a login in
                // flight before the real AniList redirect arrives.
                let (token, body): (Option<String>, &str) =
                    if let Some(err) = param("error") {
                        let msg = param("error_description").unwrap_or(err);
                        log::warn!("OAuth callback: AniList denied access: {msg}");
                        (None, ERR_HTML)
                    } else if let Some(token) = param("access_token") {
                        // CSRF check: the state AniList echoes back must equal the one
                        // we sent. A mismatch / missing state means this token wasn't
                        // for our request — reject it and keep listening.
                        match param("state") {
                            Some(s) if s == expected => (Some(token), OK_HTML),
                            _ => {
                                log::warn!("OAuth callback: state mismatch — rejected a token not issued for this login");
                                (None, ERR_HTML)
                            }
                        }
                    } else {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            SHIM_HTML.len(),
                            SHIM_HTML
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                        continue;
                    };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
                // Only a state-verified token resolves the login and frees the port.
                if let Some(token) = token {
                    let _ = tx.send(token);
                    break;
                }
            }
        });
    });
    Ok((state, rx))
}

// minimal URL-encode helper (avoids pulling urlencoding as a dep); also used by
// rss.rs to build magnet display names
pub(crate) mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn percent_decode_handles_escapes_plus_and_junk() {
        assert_eq!(super::percent_decode("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(super::percent_decode("a%20b+c"), "a b c");
        assert_eq!(super::percent_decode("%41%6eiList"), "AniList");
        // Malformed / truncated escapes pass through literally.
        assert_eq!(super::percent_decode("100%"), "100%");
        assert_eq!(super::percent_decode("%zz%4"), "%zz%4");
    }

    /// C3 regression: a hostile probe (`?error=…`), a token with the WRONG
    /// state, and a bare hit must all be answered WITHOUT resolving the login
    /// or killing the listener — only a state-verified token resolves it.
    #[test]
    fn oauth_callback_survives_probes_and_accepts_verified_token() {
        let (state, rx) = super::start_callback_server().expect("bind callback listener");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async move {
            let base = format!("http://127.0.0.1:{}", super::OAUTH_PORT);
            let http = reqwest::Client::new();
            // Bare probe → shim, still listening.
            let r = http.get(&base).send().await.unwrap();
            assert!(r.status().is_success());
            // The one-shot DoS from the review: ?error= from any web page.
            let r = http
                .get(format!("{base}/?error=access_denied"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            // Token with a wrong state → rejected, still listening.
            let r = http
                .get(format!("{base}/__capture__?access_token=bad&state=nope"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            // Token with the RIGHT state → the receiver resolves.
            let r = http
                .get(format!("{base}/__capture__?access_token=good-token&state={state}"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            let token = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("listener must still be alive after the probes")
                .unwrap();
            assert_eq!(token, "good-token");
        });
    }
}
