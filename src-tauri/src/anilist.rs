use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::models::{
    AiringItem, EntryDetails, FormatCount, FuzzyDate, GenreStat, ListEntry, ListStatus, Media,
    MediaCharacter, MediaDetail, MediaRelation, MediaStaff, Notification, NotificationPage,
    ScoreBucket, SearchPage, StatusCount, User, UserStats, YearCount,
};

const GRAPHQL: &str = "https://graphql.anilist.co";
const AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
/// Must match the registered OAuth redirect port.
pub const OAUTH_PORT: u16 = 39417;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextAiring {
    episode: Option<i64>,
    airing_at: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct AniMedia {
    id: i64,
    id_mal: Option<i64>,
    title: AniTitle,
    // AniList can return explicit nulls even for merged or stub entries.
    cover_image: Option<AniCover>,
    episodes: Option<i64>,
    format: Option<String>,
    status: Option<String>,
    average_score: Option<i64>,
    season: Option<String>,
    season_year: Option<i64>,
    description: Option<String>,
    next_airing_episode: Option<NextAiring>,
    banner_image: Option<String>,
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
            cover_medium: m.cover_image.as_ref().and_then(|c| c.medium.clone()),
            cover_large: m.cover_image.and_then(|c| c.large),
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

#[derive(Deserialize)]
pub struct SavedEntry {
    pub id: i64,
    pub status: Option<String>,
    pub progress: Option<i64>,
    pub score: Option<f64>,
    pub repeat: Option<i64>,
}

/// Clones share the rate budget, including limits reported by failed requests.
#[derive(Default)]
struct RateBudget {
    remaining: Option<u64>,
    reset_at: Option<i64>,
}

/// Clone before awaiting so no mutex guard crosses an await.
#[derive(Clone)]
pub struct AniList {
    http: reqwest::Client,
    token: Option<String>,
    rate: Arc<Mutex<RateBudget>>,
    #[cfg(test)]
    endpoint: Option<String>,
}

impl AniList {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("Kurisu")
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        AniList {
            http,
            token: None,
            rate: Arc::new(Mutex::new(RateBudget::default())),
            #[cfg(test)]
            endpoint: None,
        }
    }
    pub fn set_token(&mut self, t: Option<String>) {
        self.token = t;
    }
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
    pub fn token(&self) -> Option<String> {
        self.token.clone()
    }

    fn note_rate_headers(&self, h: &reqwest::header::HeaderMap) {
        let remaining = h
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let reset_at = h
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());
        if remaining.is_none() && reset_at.is_none() {
            return;
        }
        let mut b = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = remaining {
            b.remaining = Some(r);
        }
        if let Some(t) = reset_at {
            b.reset_at = Some(t);
        }
    }

    async fn wait_for_budget(&self) {
        let (remaining, reset_at) = {
            let b = self.rate.lock().unwrap_or_else(|e| e.into_inner());
            (b.remaining, b.reset_at)
        };
        if remaining != Some(0) {
            return;
        }
        let Some(reset) = reset_at else { return };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let wait = reset - now + 1;
        if wait <= 0 || wait > 65 {
            return;
        }
        log::info!("AniList rate window exhausted, waiting {wait}s for the reset");
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
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
        self.wait_for_budget().await;
        let (status, body) = loop {
            let resp = self
                .http
                .post(self.endpoint())
                .header("Authorization", format!("Bearer {}", token))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&payload)
                .send()
                .await?;
            let status = resp.status();
            self.note_rate_headers(resp.headers());
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1);
                if retries < 2 && wait <= 65 {
                    retries += 1;
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                return Err(ApiError {
                    status,
                    from_json: true,
                    message: format!("rate limit reached; retry after {wait}s"),
                }
                .into());
            }
            let body: serde_json::Value = resp.json().await.map_err(|e| ApiError {
                status,
                from_json: false,
                message: e.to_string(),
            })?;
            break (status, body);
        };
        if let Some(errs) = body.get("errors") {
            let error = errs.get(0);
            let reported_status = error
                .and_then(|e| e.get("status"))
                .and_then(|s| s.as_u64())
                .and_then(|s| u16::try_from(s).ok())
                .and_then(|s| reqwest::StatusCode::from_u16(s).ok())
                .filter(|s| s.is_client_error() || s.is_server_error());
            let status = if status.is_success() {
                reported_status.unwrap_or(status)
            } else {
                status
            };
            let msg = error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown AniList error");
            return Err(ApiError {
                status,
                from_json: true,
                message: msg.to_string(),
            }
            .into());
        }
        if !status.is_success() {
            return Err(ApiError {
                status,
                from_json: false,
                message: body
                    .get("message")
                    .and_then(|value| value.as_str())
                    .or_else(|| status.canonical_reason())
                    .unwrap_or("request failed")
                    .to_string(),
            }
            .into());
        }
        let data = body
            .get("data")
            .ok_or_else(|| anyhow!("AniList: no data field"))?
            .clone();
        Ok(serde_json::from_value(data)?)
    }

    fn endpoint(&self) -> &str {
        #[cfg(test)]
        if let Some(endpoint) = &self.endpoint {
            return endpoint;
        }
        GRAPHQL
    }

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
            offline: false,
        })
    }

    pub async fn search(&self, query: &str, per_page: i64) -> Result<Vec<Media>> {
        Ok(self.search_page(query, 1, per_page).await?.items)
    }

    pub async fn search_page(&self, query: &str, page: i64, per_page: i64) -> Result<SearchPage> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            media: Vec<Option<AniMedia>>,
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
        }
        let q = "query ($search: String!, $page: Int!, $perPage: Int!) {
            Page(page: $page, perPage: $perPage) {
                pageInfo { hasNextPage }
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
                serde_json::json!({ "search": query, "page": page, "perPage": per_page }),
            )
            .await?;
        Ok(SearchPage {
            items: r
                .page
                .media
                .into_iter()
                .flatten()
                .map(Media::from)
                .collect(),
            page,
            has_next_page: r.page.page_info.has_next_page,
        })
    }

    pub async fn season_all(&self, season: &str, year: i64) -> Result<Vec<Media>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            media: Option<Vec<Option<AniMedia>>>,
            page_info: Option<PageInfo>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
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
        for page in 1..=20 {
            if page > 1 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "season": season, "year": year, "page": page }),
                )
                .await?;
            let media = r.page.media.unwrap_or_default();
            let got = media.len();
            for m in media.into_iter().flatten() {
                out.push(Media::from(m));
            }
            // An empty page after hasNextPage must not pass as a complete season.
            if page > 1 && got == 0 {
                anyhow::bail!(
                    "AniList stopped returning entries partway through this season while claiming more pages; its API is degraded right now, try again later"
                );
            }
            has_next = r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            if !has_next {
                break;
            }
        }
        if has_next {
            anyhow::bail!("AniList season walk ran past 20 pages without hasNextPage clearing");
        }
        Ok(out)
    }

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
        let r: R = self
            .gql(q, serde_json::json!({ "mediaId": media_id }))
            .await?;
        Ok(r.page
            .recommendations
            .unwrap_or_default()
            .into_iter()
            .filter_map(|rec| rec.media_recommendation)
            .map(Media::from)
            .collect())
    }

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

    pub async fn media_detail(&self, id: i64) -> Result<MediaDetail> {
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
                relations(perPage: 50) {
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
        let mut unavailable_sections = Vec::new();
        if r.media
            .relations
            .as_ref()
            .and_then(|v| v.edges.as_ref())
            .is_none()
        {
            unavailable_sections.push("relations".to_string());
        }
        if r.media
            .characters
            .as_ref()
            .and_then(|v| v.edges.as_ref())
            .is_none()
        {
            unavailable_sections.push("characters".to_string());
        }
        if r.media
            .staff
            .as_ref()
            .and_then(|v| v.edges.as_ref())
            .is_none()
        {
            unavailable_sections.push("staff".to_string());
        }
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
                    va_name: va
                        .as_ref()
                        .and_then(|v| v.name.as_ref())
                        .and_then(|n| n.full.clone()),
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
        Ok(MediaDetail {
            media: r.media.media.into(),
            relations,
            characters,
            staff,
            cached_at: None,
            warning: None,
            unavailable_sections,
        })
    }

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
        let r: R = self
            .gql(q, serde_json::json!({ "name": user_name }))
            .await?;
        let a = r
            .user
            .statistics
            .and_then(|s| s.anime)
            .ok_or_else(|| anyhow!("AniList statistics are unavailable for this account"))?;
        let mut release_years: Vec<YearCount> = a
            .release_years
            .unwrap_or_default()
            .into_iter()
            .filter_map(|y| {
                Some(YearCount {
                    year: y.release_year?,
                    count: y.count,
                })
            })
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
                .map(|s| ScoreBucket {
                    score: s.score,
                    count: s.count,
                })
                .collect(),
            statuses: a
                .statuses
                .unwrap_or_default()
                .into_iter()
                .filter_map(|s| {
                    Some(StatusCount {
                        status: s.status?,
                        count: s.count,
                    })
                })
                .collect(),
            formats: a
                .formats
                .unwrap_or_default()
                .into_iter()
                .filter_map(|f| {
                    Some(FormatCount {
                        format: f.format?,
                        count: f.count,
                    })
                })
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
            cached_at: None,
            warning: None,
        })
    }

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
        // AniList uses an exclusive lower bound. Subtract one second for an inclusive start.
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
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "startExclusive": start_exclusive, "end": end, "page": page }),
                )
                .await?;
            let scheds = r.page.airing_schedules.unwrap_or_default();
            let got = scheds.len();
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
            if page > 1 && got == 0 {
                anyhow::bail!(
                    "AniList stopped returning entries partway through this week while claiming more pages; its API is degraded right now, try again later"
                );
            }
            has_next = r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            if !has_next {
                break;
            }
        }
        if has_next {
            anyhow::bail!("AniList calendar walk ran past 12 pages without hasNextPage clearing");
        }
        Ok(out)
    }

    /// Return only a complete list. Sync deletes local entries absent from this result.
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
        for chunk in 1..=200 {
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "userName": user_name, "chunk": chunk }),
                )
                .await?;
            let before = out.len();
            let lists = r.collection.lists.ok_or_else(|| {
                anyhow!("AniList returned null lists, refusing to sync a partial list")
            })?;
            for list in lists {
                let entries = list.entries.ok_or_else(|| {
                    anyhow!("AniList returned null entries, refusing to sync a partial list")
                })?;
                for e in entries {
                    let e = e.ok_or_else(|| {
                        anyhow!("AniList returned a null entry, refusing to sync a partial list")
                    })?;
                    // Keep entries with null media so sync does not delete them during an AniList merge.
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
            let Some(has_next) = r.collection.has_next_chunk else {
                return Err(anyhow!(
                    "AniList returned a null hasNextChunk, refusing to sync a partial list"
                ));
            };
            if out.len() == before && (chunk > 1 || has_next) {
                return Err(anyhow!(
                    "AniList returned an empty chunk while promising more entries, refusing to sync a partial list"
                ));
            }
            if !has_next {
                return Ok(out);
            }
        }
        Err(anyhow!(
            "AniList list walk ran past 200 chunks without hasNextChunk clearing"
        ))
    }

    pub async fn entry_by_media_id(&self, media_id: i64) -> Result<Option<SavedEntry>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Media")]
            media: EntryMedia,
        }
        #[derive(Deserialize)]
        struct EntryMedia {
            #[serde(rename = "mediaListEntry")]
            entry: Option<SavedEntry>,
        }
        let q = "query ($mediaId: Int!) {
            Media(id: $mediaId, type: ANIME) {
                mediaListEntry { id status progress score repeat }
            }
        }";
        let r: R = self
            .gql(q, serde_json::json!({ "mediaId": media_id }))
            .await?;
        Ok(r.media.entry)
    }

    /// Omitted fields stay unchanged on AniList.
    pub async fn save_entry(
        &self,
        media_id: i64,
        status: Option<ListStatus>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
    ) -> Result<SavedEntry> {
        self.save_entry_target(
            serde_json::json!({"mediaId": media_id}),
            status,
            progress,
            score,
            repeat,
        )
        .await
    }

    pub(crate) async fn save_entry_by_id(
        &self,
        entry_id: i64,
        status: Option<ListStatus>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
    ) -> Result<SavedEntry> {
        self.save_entry_target(
            serde_json::json!({"id": entry_id}),
            status,
            progress,
            score,
            repeat,
        )
        .await
    }

    async fn save_entry_target(
        &self,
        mut vars: serde_json::Value,
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
        let q = "mutation ($id: Int, $mediaId: Int, $status: MediaListStatus, $progress: Int, $score: Float, $repeat: Int) {
            SaveMediaListEntry(id: $id, mediaId: $mediaId, status: $status, progress: $progress, score: $score, repeat: $repeat) { id status progress score repeat }
        }";
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

    /// False means the entry was already absent remotely.
    pub async fn delete_entry(&self, entry_id: i64) -> Result<bool> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "DeleteMediaListEntry")]
            entry: Option<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
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

    pub async fn entry_details(&self, media_id: i64) -> Result<EntryDetails> {
        self.entry_details_with_id(media_id)
            .await
            .map(|(_, details)| details)
    }

    pub(crate) async fn entry_details_with_id(&self, media_id: i64) -> Result<(i64, EntryDetails)> {
        let q = "query ($id: Int!) {
            Media(id: $id, type: ANIME) { mediaListEntry { id notes startedAt { year month day } completedAt { year month day } customLists } }
            Viewer { mediaListOptions { animeList { customLists } } }
        }";
        let r: serde_json::Value = self.gql(q, serde_json::json!({"id": media_id})).await?;
        let raw = r
            .pointer("/Media/mediaListEntry")
            .filter(|v| !v.is_null())
            .ok_or_else(|| anyhow!("This show has no saved AniList entry yet"))?;
        let entry_id = raw
            .get("id")
            .and_then(|id| id.as_i64())
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("AniList did not return the saved entry ID"))?;
        let mut details = parse_entry_details(raw)?;
        details.available_custom_lists = r
            .pointer("/Viewer/mediaListOptions/animeList/customLists")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        for name in &details.custom_lists {
            if !details.available_custom_lists.contains(name) {
                details.available_custom_lists.push(name.clone());
            }
        }
        Ok((entry_id, details))
    }

    pub async fn save_entry_details(
        &self,
        entry_id: i64,
        notes: Option<&str>,
        started_at: Option<&FuzzyDate>,
        completed_at: Option<&FuzzyDate>,
        custom_lists: Option<&[String]>,
    ) -> Result<EntryDetails> {
        let q = "mutation ($id: Int!, $notes: String, $started: FuzzyDateInput, $completed: FuzzyDateInput, $lists: [String]) {
            SaveMediaListEntry(id: $id, notes: $notes, startedAt: $started, completedAt: $completed, customLists: $lists) {
                notes startedAt { year month day } completedAt { year month day } customLists
            }
        }";
        let mut vars = serde_json::json!({"id": entry_id});
        if let Some(v) = notes {
            vars["notes"] = serde_json::json!(v);
        }
        if let Some(v) = started_at {
            vars["started"] = serde_json::to_value(v)?;
        }
        if let Some(v) = completed_at {
            vars["completed"] = serde_json::to_value(v)?;
        }
        if let Some(v) = custom_lists {
            vars["lists"] = serde_json::json!(v);
        }
        let r: serde_json::Value = self.gql(q, vars).await?;
        parse_entry_details(
            r.get("SaveMediaListEntry")
                .filter(|v| !v.is_null())
                .ok_or_else(|| anyhow!("AniList did not return the saved entry"))?,
        )
    }

    /// Leave AniList's unread count unchanged when opening the inbox.
    pub async fn notifications(&self) -> Result<Vec<Notification>> {
        Ok(self.notifications_page(1).await?.items)
    }

    pub async fn mark_notifications_read(&self) -> Result<()> {
        let _: serde_json::Value = self.gql("query { Page(page: 1, perPage: 1) { notifications(resetNotificationCount: true) { ... on AiringNotification { id } } } }", serde_json::json!({})).await?;
        Ok(())
    }

    pub async fn notifications_page(&self, page: i64) -> Result<NotificationPage> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Page")]
            page: Page,
            #[serde(rename = "Viewer")]
            viewer: Viewer,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Viewer {
            unread_notification_count: Option<i64>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            notifications: Vec<Raw>,
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Comment {
            site_url: Option<String>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Raw {
            id: i64,
            #[serde(rename = "type")]
            kind: String,
            context: Option<String>,
            // Airing notifications use a list of contexts.
            contexts: Option<Vec<String>>,
            created_at: Option<i64>,
            media: Option<MediaRef>,
            episode: Option<i64>,
            activity_id: Option<i64>,
            thread: Option<ThreadRef>,
            comment_id: Option<i64>,
            comment: Option<Comment>,
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
        let q = "query ($page: Int!) { Viewer { unreadNotificationCount } Page(page: $page, perPage: 50) { pageInfo { hasNextPage } notifications(resetNotificationCount: false) {
            ... on AiringNotification { id type createdAt media { id title { userPreferred } coverImage { medium } } episode contexts }
            ... on FollowingNotification { id type createdAt context user { id name avatar { large } } }
            ... on ActivityLikeNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityMentionNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplyNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplySubscribedNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityReplyLikeNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ActivityMessageNotification { id type createdAt context activityId user { name avatar { large } } }
            ... on ThreadCommentMentionNotification { id type createdAt context commentId comment { siteUrl } thread { id title } user { name avatar { large } } }
            ... on ThreadCommentReplyNotification { id type createdAt context commentId comment { siteUrl } thread { id title } user { name avatar { large } } }
            ... on ThreadCommentSubscribedNotification { id type createdAt context commentId comment { siteUrl } thread { id title } user { name avatar { large } } }
            ... on ThreadCommentLikeNotification { id type createdAt context commentId comment { siteUrl } thread { id title } user { name avatar { large } } }
            ... on ThreadLikeNotification { id type createdAt context thread { id title } user { name avatar { large } } }
            ... on RelatedMediaAdditionNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } }
            ... on MediaDataChangeNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } reason }
            ... on MediaMergeNotification { id type createdAt context media { id title { userPreferred } coverImage { medium } } reason }
            ... on MediaDeletionNotification { id type createdAt context deletedMediaTitle reason }
        } } }";
        let r: R = self.gql(q, serde_json::json!({ "page": page })).await?;
        let items = r
            .page
            .notifications
            .into_iter()
            .filter(|n| !n.kind.is_empty())
            .map(|n| Notification {
                id: n.id,
                kind: n.kind,
                context: n.context.or_else(|| {
                    n.contexts
                        .as_ref()
                        .filter(|v| !v.is_empty())
                        .map(|v| v.join(" "))
                }),
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
                comment_url: n.comment.and_then(|c| c.site_url),
                reason: n.reason,
                deleted_media_title: n.deleted_media_title,
                user_name: n.user.as_ref().map(|u| u.name.clone()),
                user_avatar: n
                    .user
                    .as_ref()
                    .and_then(|u| u.avatar.as_ref().and_then(|a| a.large.clone())),
            })
            .collect();
        Ok(NotificationPage {
            items,
            page,
            has_next_page: r.page.page_info.has_next_page,
            unread_count: r.viewer.unread_notification_count,
        })
    }
}

fn parse_entry_details(raw: &serde_json::Value) -> Result<EntryDetails> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Details {
        notes: Option<String>,
        started_at: Option<FuzzyDate>,
        completed_at: Option<FuzzyDate>,
        custom_lists: Option<std::collections::BTreeMap<String, bool>>,
    }
    let entry: Details = serde_json::from_value(raw.clone())?;
    Ok(EntryDetails {
        notes: entry.notes.unwrap_or_default(),
        started_at: entry.started_at,
        completed_at: entry.completed_at,
        custom_lists: entry
            .custom_lists
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(name, enabled)| enabled.then_some(name))
            .collect(),
        ..Default::default()
    })
}

/// Keep transport errors distinct from AniList errors.
#[derive(Debug)]
struct ApiError {
    status: reqwest::StatusCode,
    from_json: bool,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AniList ({}): {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// AniList can report Not Found inside an HTTP 200 response.
fn is_not_found(e: &anyhow::Error) -> bool {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return false;
    };
    api.from_json && (api.status == reqwest::StatusCode::NOT_FOUND || api.message == "Not Found")
}

pub fn is_retryable(error: &anyhow::Error) -> bool {
    if let Some(api) = error.downcast_ref::<ApiError>() {
        if api.from_json && (is_not_found(error) || is_auth_rejection(error)) {
            return false;
        }
        return !api.from_json
            || api.status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || api.status == reqwest::StatusCode::REQUEST_TIMEOUT
            || (api.status == reqwest::StatusCode::FORBIDDEN
                && api
                    .message
                    .to_ascii_lowercase()
                    .starts_with("the anilist api has been temporarily disabled"))
            || api.status.is_server_error();
    }
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_body() || e.is_request())
}

pub fn media_not_found(e: &anyhow::Error) -> bool {
    is_not_found(e)
}

/// Only a definitive token rejection may clear the session.
pub fn is_auth_rejection(e: &anyhow::Error) -> bool {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return false;
    };
    api.from_json
        && (api.status == reqwest::StatusCode::UNAUTHORIZED
            || api.message.eq_ignore_ascii_case("Invalid Token"))
}

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

pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE}?client_id={cid}&response_type=token&redirect_uri={redir}&state={state}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(redirect_uri),
        state = urlencoding::encode(state),
    )
}

/// Forward the URL fragment as a query because browsers do not send fragments to servers.
const SHIM_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>body{font-family:sans-serif;text-align:center;padding:3em;color:#9aa3b2;background:#0f1115;margin:0}h2{color:#3ba55d;font-weight:600}</style></head><body><h2>Connecting to Kurisu…</h2><p>You can close this tab once the app opens.</p><script>(function(){var h=location.hash.charCodeAt(0)===35?location.hash.slice(1):location.hash;if(h.indexOf('access_token=')!==-1||h.indexOf('error=')!==-1){location.replace('/__capture__?'+h);}})();</script></body></html>";

const OK_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#3ba55d'>Connected to Kurisu.</h2><p>You can close this tab and return to the app.</p></body>";
const ERR_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#e74c3c'>Authorization failed.</h2><p>Return to Kurisu for details.</p></body>";

fn clip_for_log(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

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

pub type OAuthReceiver = oneshot::Receiver<Result<String, String>>;

/// Tests use port zero to avoid the live OAuth listener.
pub fn start_callback_server_on(port: u16) -> Result<(String, u16, OAuthReceiver)> {
    let state = random_state()?;
    let expected = state.clone();
    let (tx, rx) = oneshot::channel::<Result<String, String>>();
    let addr = format!("127.0.0.1:{port}");
    let listener = std::net::TcpListener::bind(&addr).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::Error::new(e).context(format!(
                "a sign-in is already in progress (port {port} is busy) — finish it in your \
                 browser, or wait a moment and try again"
            ))
        } else {
            anyhow::anyhow!(e)
        }
    })?;
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
            let mut tx = tx;
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    log::warn!("OAuth callback: could not register the listener with the runtime: {e}");
                    return;
                }
            };
            loop {
                let (mut sock, _) = match tokio::select! {
                    _ = tx.closed() => return,
                    acc = listener.accept() => acc,
                } {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Bound the whole read so browser preconnects cannot stall the listener.
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
                tokio::select! {
                    _ = tx.closed() => return,
                    _ = tokio::time::timeout(Duration::from_secs(10), read_headers) => {}
                }
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

                let (outcome, body) =
                    if (param("error").is_some() || param("access_token").is_some())
                        && param("state").as_deref() != Some(expected.as_str())
                    {
                        (None, ERR_HTML)
                    } else if let Some(err) = param("error") {
                        let msg = param("error_description").unwrap_or(err);
                        log::warn!("OAuth callback: AniList denied access: {}", clip_for_log(&msg));
                        (Some(Err(format!("AniList authorization failed: {}", clip_for_log(&msg)))), ERR_HTML)
                    } else if let Some(token) = param("access_token") {
                        if token.trim().is_empty() {
                            (Some(Err("AniList returned an empty access token.".into())), ERR_HTML)
                        } else {
                            (Some(Ok(token)), OK_HTML)
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
                if let Some(outcome) = outcome {
                    let _ = tx.send(outcome);
                    break;
                }
            }
        });
    });
    Ok((state, bound_port, rx))
}

pub(crate) mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
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
        assert_eq!(super::percent_decode("100%"), "100%");
        assert_eq!(super::percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn is_not_found_matches_status_and_exact_message_only() {
        let real_404: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::NOT_FOUND,
            from_json: true,
            message: "whatever".into(),
        }
        .into();
        assert!(super::is_not_found(&real_404));
        let gql_not_found: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::OK,
            from_json: true,
            message: "Not Found".into(),
        }
        .into();
        assert!(super::is_not_found(&gql_not_found));
        let quoted_id: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            from_json: true,
            message: "validation failed for entry 40413".into(),
        }
        .into();
        assert!(!super::is_not_found(&quoted_id));
        let near_miss: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::OK,
            from_json: true,
            message: "Not Found.".into(),
        }
        .into();
        assert!(!super::is_not_found(&near_miss));
        let plain = anyhow::anyhow!("AniList (404): boom");
        assert!(!super::is_not_found(&plain));
    }

    #[test]
    fn is_auth_rejection_matches_invalid_token_and_auth_statuses_only() {
        let invalid_token: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            from_json: true,
            message: "Invalid Token".into(),
        }
        .into();
        assert!(super::is_auth_rejection(&invalid_token));
        let unauthorized: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            from_json: true,
            message: "anything".into(),
        }
        .into();
        assert!(super::is_auth_rejection(&unauthorized));
        let validation: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::BAD_REQUEST,
            from_json: true,
            message: "validation failed".into(),
        }
        .into();
        assert!(!super::is_auth_rejection(&validation));
        let transport = anyhow::anyhow!("dns error");
        assert!(!super::is_auth_rejection(&transport));
    }

    #[test]
    fn forbidden_with_a_non_json_body_is_not_an_auth_rejection() {
        let html_403: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            from_json: false,
            message: "error decoding response body: expected value at line 1 column 1".into(),
        }
        .into();
        assert!(!super::is_auth_rejection(&html_403));
        let json_403: anyhow::Error = super::ApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            from_json: true,
            message: "Forbidden".into(),
        }
        .into();
        assert!(!super::is_auth_rejection(&json_403));
    }

    #[test]
    fn oauth_callback_survives_probes_and_accepts_verified_token() {
        let (state, port, rx) = super::start_callback_server_on(0).expect("bind callback listener");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async move {
            let base = format!("http://127.0.0.1:{port}");
            let http = reqwest::Client::new();
            let r = http.get(&base).send().await.unwrap();
            assert!(r.status().is_success());
            let r = http
                .get(format!("{base}/?error=access_denied"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            let r = http
                .get(format!("{base}/__capture__?access_token=bad&state=nope"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            let r = http
                .get(format!(
                    "{base}/__capture__?access_token=good-token&state={state}"
                ))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            let token = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("listener must still be alive after the probes")
                .unwrap();
            assert_eq!(token.unwrap(), "good-token");
        });
    }
}

#[cfg(test)]
pub(crate) async fn mock_api(
    responses: Vec<(u16, serde_json::Value)>,
) -> (
    AniList,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let body_start;
            loop {
                let mut buffer = [0u8; 4096];
                let size = socket.read(&mut buffer).await.unwrap();
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..size]);
                if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    body_start = index + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while request.len() < body_start + length {
                let mut buffer = [0u8; 4096];
                let size = socket.read(&mut buffer).await.unwrap();
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..size]);
            }
            let request =
                serde_json::from_slice(&request[body_start..body_start + length]).unwrap();
            let _ = requests.send(request);
            let body = body.to_string();
            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let mut api = AniList::new();
    api.http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    api.endpoint = Some(endpoint);
    api.set_token(Some("test-session".into()));
    (api, received)
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_verified_oauth_denial_finishes_the_login_attempt() {
        let (state, port, receiver) = start_callback_server_on(0).unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{port}/__capture__?error=access_denied&state={state}"
            ))
            .send()
            .await
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.unwrap_err().contains("access_denied"));
    }

    #[tokio::test]
    async fn entry_lookup_is_scoped_to_the_authenticated_user() {
        let (api, mut requests) = mock_api(vec![(200, json!({"data": {
            "Media": {"mediaListEntry": {"id": 22, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}},
            "MediaList": {"id": 999, "status": "COMPLETED", "progress": 12, "score": 10.0, "repeat": 0}
        }}))]).await;
        let entry = api.entry_by_media_id(1).await.unwrap().unwrap();
        assert_eq!(entry.id, 22);
        let request = requests.recv().await.unwrap();
        let query = request["query"].as_str().unwrap();
        assert!(query.contains("mediaListEntry"));
        assert!(!query.contains("MediaList(mediaId:"));
    }

    #[tokio::test]
    async fn an_unlisted_anime_is_not_a_missing_anime() {
        let (api, _) = mock_api(vec![(
            200,
            json!({"data": {"Media": {"mediaListEntry": null}}}),
        )])
        .await;
        assert!(api.entry_by_media_id(1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn incomplete_list_payloads_are_rejected() {
        for lists in [
            json!(null),
            json!([{"entries": null}]),
            json!([{"entries": [null]}]),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data": {"MediaListCollection": {"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            assert!(api.user_list("Tester").await.is_err());
        }
    }

    #[tokio::test]
    async fn a_missing_later_chunk_is_not_a_complete_list() {
        let (api, _) = mock_api(vec![
            (200, json!({"data": {"MediaListCollection": {"lists": [{"entries": [{"id": 22, "mediaId": 1}]}], "hasNextChunk": true}}})),
            (200, json!({"data": {"MediaListCollection": {"lists": [], "hasNextChunk": false}}})),
        ]).await;
        assert!(api.user_list("Tester").await.is_err());
    }

    #[tokio::test]
    async fn complete_lists_allow_empty_arrays_and_null_media() {
        for (lists, count) in [
            (json!([]), 0),
            (json!([{"entries": []}]), 0),
            (
                json!([{"entries": [{"id": 22, "mediaId": 1, "media": null}]}]),
                1,
            ),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data": {"MediaListCollection": {"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            assert_eq!(api.user_list("Tester").await.unwrap().len(), count);
        }
    }
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn graphql_error_status_controls_retries_without_broadening_permanent_failures() {
        for (transport, reported, message, retry, auth, missing) in [
            (200, json!(429), "Too Many Requests.", true, false, false),
            (200, json!(503), "Service unavailable", true, false, false),
            (200, json!(400), "validation", false, false, false),
            (200, json!(401), "Unauthenticated", false, true, false),
            (200, json!(404), "Anime unavailable", false, false, true),
            (200, json!(403), "Access denied", false, false, false),
            (
                403,
                json!(403),
                "This IP address has been blocked",
                false,
                false,
                false,
            ),
            (200, json!(999), "Invalid status", false, false, false),
            (
                200,
                json!("429"),
                "Invalid status type",
                false,
                false,
                false,
            ),
            (503, json!(400), "Request failed", true, false, false),
        ] {
            let (api, _) = mock_api(vec![(
                transport,
                json!({"errors":[{"status":reported,"message":message}],"data":null}),
            )])
            .await;
            let error = api.entry_by_media_id(1).await.err().unwrap();
            assert_eq!(is_retryable(&error), retry, "{transport}: {message}");
            assert_eq!(is_auth_rejection(&error), auth, "{transport}: {message}");
            assert_eq!(media_not_found(&error), missing, "{transport}: {message}");
        }
    }

    #[tokio::test]
    async fn json_gateway_errors_preserve_status_without_invalidating_account_or_media() {
        for status in [401, 404, 502, 503] {
            let (api, _) =
                mock_api(vec![(status, json!({"message": "Gateway unavailable"}))]).await;
            let error = api.entry_by_media_id(1).await.err().unwrap();
            assert_eq!(
                error.downcast_ref::<ApiError>().unwrap().status.as_u16(),
                status
            );
            assert!(is_retryable(&error), "{status}");
            assert!(!is_auth_rejection(&error), "{status}");
            assert!(!media_not_found(&error), "{status}");
        }
    }

    #[test]
    fn retry_classification_keeps_permanent_failures_out_of_the_queue() {
        for (status, from_json, message, expected) in [
            (429, true, "slow down", true),
            (408, true, "timeout", true),
            (503, true, "unavailable", true),
            (404, false, "edge page", true),
            (400, true, "validation failed", false),
            (401, true, "Invalid Token", false),
            (200, true, "Not Found", false),
            (200, true, "Invalid Token", false),
        ] {
            let error: anyhow::Error = ApiError {
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                from_json,
                message: message.into(),
            }
            .into();
            assert_eq!(is_retryable(&error), expected, "{status}: {message}");
            if !from_json {
                assert!(!media_not_found(&error));
            }
        }
    }

    #[tokio::test]
    async fn search_returns_page_boundaries_from_anilist() {
        let (api, mut requests) = mock_api(vec![(
            200,
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":true},"media":[{"id":26}]}}}),
        )])
        .await;
        let page = api.search_page("Show", 2, 25).await.unwrap();
        assert_eq!(page.page, 2);
        assert!(page.has_next_page);
        assert_eq!(page.items[0].id, 26);
        assert_eq!(requests.recv().await.unwrap()["variables"]["page"], 2);
    }

    #[tokio::test]
    async fn notifications_keep_the_unread_count_and_comment_permalink() {
        let (api, mut requests) = mock_api(vec![(200, json!({"data":{"Viewer":{"unreadNotificationCount":4},"Page":{"pageInfo":{"hasNextPage":true},"notifications":[{"id":8,"type":"THREAD_COMMENT_REPLY","commentId":22,"comment":{"siteUrl":"https://anilist.co/forum/thread/1/comment/22"}}]}}}))]).await;
        let page = api.notifications_page(2).await.unwrap();
        assert_eq!(page.unread_count, Some(4));
        assert!(page.has_next_page);
        assert_eq!(
            page.items[0].comment_url.as_deref(),
            Some("https://anilist.co/forum/thread/1/comment/22")
        );
        let request = requests.recv().await.unwrap();
        assert_eq!(request["variables"]["page"], 2);
        assert!(request["query"]
            .as_str()
            .unwrap()
            .contains("resetNotificationCount: false"));
    }

    #[tokio::test]
    async fn statistics_null_is_unavailable_instead_of_an_empty_account() {
        let (api, _) = mock_api(vec![(200, json!({"data":{"User":{"statistics":null}}}))]).await;
        assert!(api
            .user_statistics("Tester")
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
    }

    #[tokio::test]
    async fn details_distinguish_missing_sections_from_empty_sections() {
        let (api, _) = mock_api(vec![(200, json!({"data":{"Media":{"id":1,"relations":null,"characters":{"edges":[]},"staff":null}}}))]).await;
        let details = api.media_detail(1).await.unwrap();
        assert_eq!(details.unavailable_sections, vec!["relations", "staff"]);
    }

    #[tokio::test]
    async fn notes_mutation_omits_untouched_dates_lists_and_progress() {
        let (api, mut requests) = mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":{"notes":"New note","startedAt":{"year":2024,"month":null,"day":null},"customLists":{"Favorites":true,"Other":false}}}}))]).await;
        let result = api
            .save_entry_details(1, Some("New note"), None, None, None)
            .await
            .unwrap();
        assert_eq!(result.custom_lists, vec!["Favorites"]);
        assert_eq!(result.started_at.unwrap().year, Some(2024));
        let request = requests.recv().await.unwrap();
        assert_eq!(request["variables"], json!({"id":1,"notes":"New note"}));
        assert!(!request["query"].as_str().unwrap().contains("progress:"));
    }
}
