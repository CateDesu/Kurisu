//! AniList v2 GraphQL client with desktop OAuth2 implicit flow.
//! Browser gets the token in a URL fragment it never sends to a server.
//! Callback page runs JS that moves the fragment into the query string and
//! re-requests. Then our listener picks it up. No client_secret needed.

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
/// Fixed port. User registers ONE redirect_uri in their AniList client.
pub const OAUTH_PORT: u16 = 39417;

/// `nextAiringEpisode { episode airingAt }`. Shared by search and list queries.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextAiring {
    episode: Option<i64>,
    airing_at: Option<i64>,
}

/// Media fields every list-ish query fetches. search, user_list, season,
/// recommendations all share one deserializer and conversion. Detail fields
/// are Options the lean queries never populate.
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
    // AniList types the list items as nullable. A stray null must not
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

/// Entry fields a SaveMediaListEntry mutation or MediaList query returns.
/// AniList's true post-write state. The local cache mirrors THESE values,
/// not what we sent. Arguments Kurisu omits keep their remote values, and
/// only the response knows them.
#[derive(Deserialize)]
pub struct SavedEntry {
    pub id: i64,
    pub status: Option<String>,
    pub progress: Option<i64>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
}

/// reqwest::Client is cheap to clone, Arc backed. Cloning AniList lets us
/// drop the lock before any .await. Tauri futures must be Send.
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
        let payload = serde_json::json!({ "query": query, "variables": vars });
        let mut retries = 0;
        // AniList error envelope: { "errors": [ { "message": "..." } ] }
        let (status, body) = loop {
            let resp = self
                .http
                .post(GRAPHQL)
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await?;
            let status = resp.status();
            // Rate limited. Honor Retry-After and retry, bounded. Do not
            // fail the whole operation on a transient 429.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && retries < 2 {
                retries += 1;
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ApiError { status, message: e.to_string() })?;
            break (status, body);
        };
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow!(
                "AniList is rate-limiting this app (429, still limited after {retries} retries); wait a minute and try again"
            ));
        }
        if let Some(errs) = body.get("errors") {
            let msg = errs
                .get(0)
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown AniList error");
            return Err(ApiError { status, message: msg.to_string() }.into());
        }
        // unwrap the data object before deserializing into T
        let data = body
            .get("data")
            .ok_or_else(|| anyhow!("AniList: no data field"))?
            .clone();
        Ok(serde_json::from_value(data)?)
    }

    /// Viewer is the authenticated user. Used to verify the token and fetch the name.
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
            // The list and its items are both nullable in the schema. A
            // stray null must not fail the whole query.
            media: Option<Vec<Option<AniMedia>>>,
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
        Ok(r.page
            .media
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .map(Media::from)
            .collect())
    }

    /// One anime season. WINTER, SPRING, SUMMER, or FALL plus year. Most popular first.
    /// Walks every page. A single page of 50 on a season carrying several
    /// hundred entries silently hid everything past the popular head, which
    /// looked exactly like a complete listing. Paced like the calendar walk
    /// so the rate budget survives it.
    pub async fn season_all(&self, season: &str, year: i64) -> Result<Vec<Media>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        struct Page {
            // The list and its items are both nullable in the schema. A
            // stray null must not fail the whole query.
            media: Option<Vec<Option<AniMedia>>>,
            page_info: Option<PageInfo>,
        }
        #[derive(Deserialize)]
        struct PageInfo {
            has_next_page: Option<bool>,
        }
        let q = "query ($season: MediaSeason!, $year: Int!, $page: Int!) {
            Page(page: $page, perPage: 50) {
                pageInfo { hasNextPage }
                media(season: $season, seasonYear: $year, type: ANIME, isAdult: false, sort: POPULARITY_DESC) {
                    id idMal title { romaji english native }
                    coverImage { medium large }
                    episodes format status averageScore season seasonYear description
                    nextAiringEpisode { episode airingAt }
                }
            }
        }";
        let mut out = Vec::new();
        let mut has_next = true;
        for page in 1..=10 {
            if page > 1 {
                // Pace the page walk. AniList's rate budget is tight. Do not
                // burn it in a single burst.
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "season": season, "year": year, "page": page }),
                )
                .await?;
            let media = r.page.media.unwrap_or_default();
            for m in media.into_iter().flatten() {
                out.push(Media::from(m));
            }
            // A null hasNextPage is not "walk done". Breaking here would
            // hand back a truncated season as if it were complete. Abort
            // instead so the caller surfaces the failure.
            has_next = r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            if !has_next {
                break;
            }
        }
        // 10 pages of 50 covers every real season with room to spare. Still
        // reporting more means something is wrong, and a partial season must
        // not pass as complete.
        if has_next {
            anyhow::bail!("AniList season walk ran past 10 pages without hasNextPage clearing");
        }
        Ok(out)
    }

    /// Community recommendations for one title. Best rated first.
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

    /// One anime by AniList id. Cache miss fallback for get_media, since
    /// search can not look up by id.
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

    /// One anime with full detail fields plus anime relations, main
    /// characters with Japanese voice actors, and key staff. Manga
    /// relations dropped, app is anime only and can not open them.
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

    /// Profile statistics for a user, computed on the server. Whole list
    /// aggregates like counts, time watched, score, status, format, genre,
    /// year breakdowns. Nothing to compute locally.
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

    /// Every episode airing in [start, end), Unix seconds, in airing order.
    /// Pages through 50-per-page chunks. Capped at 12 pages so 600 entries,
    /// more than any real week. A bad range can not loop forever.
    /// Adult titles dropped.
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
        // airingAt_greater is EXCLUSIVE, so passing the window start dropped
        // an episode airing exactly on the boundary. The calendar tiles a
        // week as [start, end), so that episode fell through the crack
        // between two adjacent weeks and appeared in neither. Shift the
        // lower bound by one second to make it inclusive. airingAt_lesser
        // with end stays exclusive.
        let start_exclusive = start.saturating_sub(1);
        let q = "query ($startExclusive: Int!, $end: Int!, $page: Int!) {
            Page(page: $page, perPage: 50) {
                pageInfo { hasNextPage }
                airingSchedules(airingAt_greater: $startExclusive, airingAt_lesser: $end, sort: TIME) {
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
        let mut has_next = true;
        for page in 1..=12 {
            if page > 1 {
                // Pace the page walk. One calendar view can cost a dozen
                // requests and AniList's rate budget is tight. Do not burn
                // it in a single burst.
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "startExclusive": start_exclusive, "end": end, "page": page }),
                )
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
            // A null hasNextPage is not "walk done". Breaking here would
            // hand back a truncated calendar as if it were complete. Abort
            // instead so the caller surfaces the failure.
            has_next = r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            if !has_next {
                break;
            }
        }
        // Falling out of the loop while pages remain would hand back a
        // partial week as if it were complete, the exact thing the null
        // check above guards against.
        if has_next {
            anyhow::bail!("AniList calendar walk ran past 12 pages without hasNextPage clearing");
        }
        Ok(out)
    }

    /// Pull the full list, every status group, for a user and flatten to entries.
    /// AniList chunks big lists at 500 entries per status group. Walk the
    /// chunks via hasNextChunk or large accounts sync an incomplete list. Ok
    /// is only ever returned after a COMPLETE walk where hasNextChunk is false.
    /// Any error aborts the whole fetch, because the sync caller reconcile
    /// deletes local rows the remote did not return. AniList types every entry
    /// field nullable, so they are Options here. One malformed row costs that
    /// row, not the sync.
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
            // The list and its items are both nullable in the schema. A
            // stray null must not fail the whole query.
            entries: Option<Vec<Option<Entry>>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            id: i64,
            status: Option<String>,
            progress: Option<i64>,
            score: Option<f64>,
            repeat: Option<i64>,
            updated_at: Option<i64>,
            // Non null in the schema, unlike media. Still identifies the
            // entry while AniList merges or removes a title and media
            // comes back null.
            media_id: i64,
            media: Option<AniMedia>,
        }
        let q = "query ($userName: String!, $chunk: Int!) {
            MediaListCollection(userName: $userName, type: ANIME, chunk: $chunk, perChunk: 500) {
                hasNextChunk
                lists {
                    status
                    entries {
                        id status progress score repeat updatedAt mediaId
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
        // Chunks are 500 entries. Cap the walk at 200 chunks, 100k entries,
        // well past any real list. A hasNextChunk stuck true must not spin
        // requests until hard 429s burn the rate budget to stop it.
        for chunk in 1..=200 {
            let r: R = self
                .gql(q, serde_json::json!({ "userName": user_name, "chunk": chunk }))
                .await?;
            for list in r.collection.lists.unwrap_or_default() {
                for e in list.entries.unwrap_or_default().into_iter().flatten() {
                    // media comes back null while AniList merges or removes
                    // a title, but the entry still exists remotely. Keep it
                    // with media None so sync marks it seen instead of
                    // reconcile deleting the local row. media_id equals
                    // media.id whenever media is present.
                    out.push(ListEntry {
                        id: Some(e.id),
                        media_id: e.media_id,
                        status: e.status.unwrap_or_else(|| "CURRENT".into()),
                        progress: e.progress.unwrap_or(0),
                        score: e.score,
                        repeat: e.repeat.unwrap_or(0),
                        updated_at: e.updated_at,
                        media: e.media.map(Media::from),
                    });
                }
            }
            // A null hasNextChunk is not "walk done". Sync reconcile deletes
            // every local row the walk did not return, so a partial list
            // must abort here and never pass for complete.
            let Some(has_next) = r.collection.has_next_chunk else {
                return Err(anyhow!(
                    "AniList returned a null hasNextChunk, refusing to sync a partial list"
                ));
            };
            if !has_next {
                return Ok(out);
            }
        }
        Err(anyhow!(
            "AniList list walk ran past 200 chunks without hasNextChunk clearing"
        ))
    }

    /// The viewer's list entry for one media. None means not on their list.
    /// Used to tell a real add apart from a local cache miss on an entry
    /// that already exists remotely.
    pub async fn entry_by_media_id(&self, media_id: i64) -> Result<Option<SavedEntry>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "MediaList")]
            entry: Option<SavedEntry>,
        }
        let q = "query ($mediaId: Int!) {
            MediaList(mediaId: $mediaId) { id status progress score repeat }
        }";
        let r: R = self.gql(q, serde_json::json!({ "mediaId": media_id })).await?;
        Ok(r.entry)
    }

    /// Create or update an entry. Only the Some fields are sent. AniList
    /// treats omitted arguments as unchanged, so a write that is not meant
    /// to touch score or repeat can not clobber values set elsewhere.
    /// Returns the entry as AniList stored it.
    pub async fn save_entry(
        &self,
        media_id: i64,
        status: Option<ListStatus>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
    ) -> Result<SavedEntry> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "SaveMediaListEntry")]
            entry: SavedEntry,
        }
        let q = "mutation ($mediaId: Int!, $status: MediaListStatus, $progress: Int, $score: Float, $repeat: Int) {
            SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, score: $score, repeat: $repeat) { id status progress score repeat }
        }";
        let mut vars = serde_json::json!({ "mediaId": media_id });
        let obj = vars.as_object_mut().expect("vars object");
        if let Some(st) = status {
            obj.insert("status".into(), st.as_str().into());
        }
        if let Some(p) = progress {
            obj.insert("progress".into(), p.into());
        }
        if let Some(s) = score {
            obj.insert("score".into(), s.into());
        }
        if let Some(r) = repeat {
            obj.insert("repeat".into(), r.into());
        }
        let r: R = self.gql(q, vars).await?;
        Ok(r.entry)
    }

    /// Delete a list entry. Ok(true) means deleted. Ok(false) means the
    /// entry was already absent remotely, deleted on anilist.co or another
    /// client. That is the desired end state. The caller should drop the
    /// local row too instead of hard failing and stranding it.
    pub async fn delete_entry(&self, entry_id: i64) -> Result<bool> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "DeleteMediaListEntry")]
            entry: Option<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            // Nullable in the schema. A null maps to the friendly decline
            // below, not a serde type error.
            deleted: Option<bool>,
        }
        let q = "mutation ($id: Int!) { DeleteMediaListEntry(id: $id) { deleted } }";
        let r: R = match self.gql(q, serde_json::json!({ "id": entry_id })).await {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        if r.entry.and_then(|e| e.deleted) != Some(true) {
            return Err(anyhow!("AniList declined the delete"));
        }
        Ok(true)
    }

    /// The user's recent notifications. Union of Airing, Following, Activity,
    /// Thread, Media, and other types. Flattened into one struct per type
    /// with the rest left None. resetNotificationCount is false so opening
    /// the inbox here does not silently clear AniList's own unread badge.
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
            // AiringNotification has contexts, a Vec, instead of context.
            contexts: Option<Vec<String>>,
            created_at: Option<i64>,
            // AniList notifications expose the anime as media { id }, not mediaId.
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
            // A notification type with no fragment in this query comes back
            // as {} so kind "" and id 0. Drop it. Rendering a ghost row
            // with a duplicate id for the frontend keyed each is worse than
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

// ───────────────────────── OAuth2 flow ─────────────────────────

/// An AniList API failure with the HTTP status and the error message kept as
/// data, so a caller can match on them instead of substring searching a
/// formatted string. Displays in the same shape the old formatted error did.
#[derive(Debug)]
struct ApiError {
    status: reqwest::StatusCode,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AniList ({}): {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// AniList answers a mutation against an already gone entry with
/// {"data":{"DeleteMediaListEntry":null},"errors":[{"message":"Not Found"}]}
/// over HTTP 200, or more rarely a real 404. Both mean no such entry.
fn is_not_found(e: &anyhow::Error) -> bool {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return false;
    };
    api.status == reqwest::StatusCode::NOT_FOUND || api.message == "Not Found"
}

/// Same test, public for the command layer. A media id that answers Not Found
/// was merged or deleted upstream, callers use this to drop the dead cached
/// row and report the merge instead of surfacing the raw status forever.
pub fn media_not_found(e: &anyhow::Error) -> bool {
    is_not_found(e)
}

/// AniList rejected the token itself. Revoked or expired tokens answer
/// 400 "Invalid Token" rather than 401. A transport failure must not be
/// confused with this: offline is not logged out. Only a definitive
/// rejection may clear the session.
pub fn is_auth_rejection(e: &anyhow::Error) -> bool {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return false;
    };
    api.status == reqwest::StatusCode::UNAUTHORIZED
        || api.status == reqwest::StatusCode::FORBIDDEN
        || api.message.eq_ignore_ascii_case("Invalid Token")
}
//
// Implicit grant. No client_secret needed, a desktop app can not keep one
// private anyway. The token arrives in the redirect URL fragment,
// #access_token=..., which the browser never sends to a server. The
// callback serves a tiny HTML page whose JS lifts the fragment into a
// query string the server can read on a second request.

/// 32 random bytes from the OS CSPRNG, hex encoded. Used as OAuth state so
/// the callback can reject a token AniList did not issue for THIS login
/// attempt. Blocks CSRF and token injection via a malicious site hitting
/// 127.0.0.1:39417. No software fallback. An OAuth flow that can not
/// randomize its state must not start.
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

/// Build the authorize URL the user's browser should visit. response_type=token.
/// state is echoed back by AniList and checked by the callback server.
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE}?client_id={cid}&response_type=token&redirect_uri={redir}&state={state}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(redirect_uri),
        state = urlencoding::encode(state),
    )
}

/// HTML shim served on the first callback hit. Moves the URL fragment,
/// which the server can not see, into a /__capture__?<fragment> request
/// the server CAN read. On a bare probe with no fragment, just shows a
/// connecting page.
const SHIM_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>body{font-family:sans-serif;text-align:center;padding:3em;color:#9aa3b2;background:#0f1115;margin:0}h2{color:#3ba55d;font-weight:600}</style></head><body><h2>Connecting to Kurisu…</h2><p>You can close this tab once the app opens.</p><script>(function(){var h=location.hash.charCodeAt(0)===35?location.hash.slice(1):location.hash;if(h.indexOf('access_token=')!==-1){location.replace('/__capture__?'+h);}})();</script></body></html>";

const OK_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#3ba55d'>Connected to Kurisu.</h2><p>You can close this tab and return to the app.</p></body>";
const ERR_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#e74c3c'>Authorization failed.</h2><p>Return to Kurisu for details.</p></body>";

/// Minimal query value decoder. %XX escapes plus + as space. Today's
/// values, base64url token and hex state, contain neither, so this is a
/// no-op. Only matters if AniList ever changes the token alphabet.
/// Malformed escapes pass through literally.
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

/// Start a localhost HTTP listener that captures the access token AniList
/// sends back in the implicit flow. Returns (state, receiver). The caller
/// embeds state in the authorize URL. The receiver resolves with the
/// token once a request passes the CSRF state check. AniList errors,
/// state mismatches, and stray probes are answered and LOGGED but never
/// resolve the receiver or stop the listener. Any web page can fire
/// http://127.0.0.1:39417/?error=x and that must not kill a login in
/// flight. The listener shuts down when the caller drops the receiver,
/// timeout or cancel, freeing the port for a retry.
pub fn start_callback_server() -> Result<(String, oneshot::Receiver<String>)> {
    let (state, _port, rx) = start_callback_server_on(OAUTH_PORT)?;
    Ok((state, rx))
}

/// start_callback_server, with the port injectable. Production always uses
/// OAUTH_PORT, it has to match the registered redirect URI. Tests pass 0
/// so the OS assigns a free port, keeping them off the real singleton and
/// letting them run concurrently with each other and a running Kurisu.
pub fn start_callback_server_on(
    port: u16,
) -> Result<(String, u16, oneshot::Receiver<String>)> {
    let state = random_state()?;
    let expected = state.clone();
    let (tx, rx) = oneshot::channel::<String>();
    let addr = format!("127.0.0.1:{port}");
    let listener = std::net::TcpListener::bind(&addr).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            // Without this the user just saw "Address already in use".
            // The usual cause is their own abandoned sign in still holding
            // the port.
            anyhow::anyhow!(
                "a sign-in is already in progress (port {port} is busy) — finish it in your \
                 browser, or wait a moment and try again"
            )
        } else {
            anyhow::anyhow!(e)
        }
    })?;
    // Read back what the OS actually gave us. Port 0 resolves to the real one.
    let bound_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
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
            // closed() takes &mut self. Rebind so the select can poll it.
            let mut tx = tx;
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                // Reactor registration failed, fd exhaustion is the usual
                // cause. Returning drops tx, closing the channel, so the
                // login fails fast instead of hanging until the timeout.
                Err(e) => {
                    log::warn!("OAuth callback: could not register the listener with the runtime: {e}");
                    return;
                }
            };
            loop {
                // Stop waiting when the caller dropped the receiver. Login
                // timed out or was abandoned. The listener drops with this
                // task, freeing the port so a retry can bind it. Without
                // this the port stayed bound for the process lifetime and
                // a second Sign in click always failed.
                let (mut sock, _) = match tokio::select! {
                    _ = tx.closed() => return,
                    acc = listener.accept() => acc,
                } {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read until the request headers are complete, or the buffer
                // fills, the peer closes, or 10s passes. One read usually
                // delivers the whole GET over loopback, but that is a TCP
                // accident, not a guarantee. A speculative browser preconnect
                // that never sends a byte must not park the accept loop until
                // the browser gives up on the socket.
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

                // 1) AniList denied. The error comes in the query, ?error=...
                // 2) The shim re-requested with the fragment as a query,
                //    ?access_token=.... The flow is response_type=token, so
                //    that is the ONLY parameter that can carry credentials.
                //    A code would need an exchange step this app does not have.
                // 3) Otherwise, initial implicit redirect with the token still
                //    in the fragment, or a probe. Serve the shim, do not
                //    resolve yet.
                // Failures, error param or state mismatch, answer the browser
                // but do NOT resolve the login or stop the listener. Any web
                // page can hit 127.0.0.1:39417 with ?error=... and that must
                // not kill a login in flight before the real AniList redirect
                // arrives.
                let (token, body): (Option<String>, &str) =
                    if let Some(err) = param("error") {
                        let msg = param("error_description").unwrap_or(err);
                        log::warn!("OAuth callback: AniList denied access: {msg}");
                        (None, ERR_HTML)
                    } else if let Some(token) = param("access_token") {
                        // CSRF check. The state AniList echoes back must equal the
                        // one we sent. A mismatch or missing state means this token
                        // was not for our request. Reject it and keep listening.
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
                // Only a token that passed the state check resolves the login and frees the port.
                if let Some(token) = token {
                    let _ = tx.send(token);
                    break;
                }
            }
        });
    });
    Ok((state, bound_port, rx))
}

// minimal URL encode helper. Avoids pulling urlencoding as a dep.
// Also used by rss.rs to build magnet display names.
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
        // Malformed or truncated escapes pass through literally.
        assert_eq!(super::percent_decode("100%"), "100%");
        assert_eq!(super::percent_decode("%zz%4"), "%zz%4");
    }

    /// A4 regression. Only the real HTTP status or the exact Not Found
    /// message mark an entry as already gone. A validation error whose
    /// message happens to carry the digits 404, say a quoted entry id,
    /// must not read as gone or the local row dies while the remote lives.
    #[test]
    fn is_not_found_matches_status_and_exact_message_only() {
        let real_404: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::NOT_FOUND,
            message: "whatever".into(),
        }
        .into();
        assert!(super::is_not_found(&real_404));
        let gql_not_found: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::OK,
            message: "Not Found".into(),
        }
        .into();
        assert!(super::is_not_found(&gql_not_found));
        let quoted_id: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "validation failed for entry 40413".into(),
        }
        .into();
        assert!(!super::is_not_found(&quoted_id));
        let near_miss: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::OK,
            message: "Not Found.".into(),
        }
        .into();
        assert!(!super::is_not_found(&near_miss));
        let plain = anyhow::anyhow!("AniList (404): boom");
        assert!(!super::is_not_found(&plain));
    }

    /// Only a definitive rejection counts. Transport noise and unrelated
    /// 400s must not log the user out.
    #[test]
    fn is_auth_rejection_matches_invalid_token_and_auth_statuses_only() {
        let invalid_token: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "Invalid Token".into(),
        }
        .into();
        assert!(super::is_auth_rejection(&invalid_token));
        let unauthorized: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            message: "anything".into(),
        }
        .into();
        assert!(super::is_auth_rejection(&unauthorized));
        let validation: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "validation failed".into(),
        }
        .into();
        assert!(!super::is_auth_rejection(&validation));
        let transport = anyhow::anyhow!("dns error");
        assert!(!super::is_auth_rejection(&transport));
    }

    /// C3 regression. A hostile probe with ?error=..., a token with the
    /// WRONG state, and a bare hit must all be answered WITHOUT resolving
    /// the login or killing the listener. Only a token that passes the
    /// state check resolves it.
    #[test]
    fn oauth_callback_survives_probes_and_accepts_verified_token() {
        // Port 0. The OS picks a free one. Binding the real OAUTH_PORT
        // made this test fail whenever Kurisu was running or another copy
        // of the test was, and it could steal the port from a sign in
        // in progress.
        let (state, port, rx) =
            super::start_callback_server_on(0).expect("bind callback listener");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async move {
            let base = format!("http://127.0.0.1:{port}");
            let http = reqwest::Client::new();
            // Bare probe. Shim, still listening.
            let r = http.get(&base).send().await.unwrap();
            assert!(r.status().is_success());
            // One shot DoS from the review. ?error= from any web page.
            let r = http
                .get(format!("{base}/?error=access_denied"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            // Token with a wrong state. Rejected, still listening.
            let r = http
                .get(format!("{base}/__capture__?access_token=bad&state=nope"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            // Token with the RIGHT state. The receiver resolves.
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
