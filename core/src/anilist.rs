use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

#[cfg(feature = "test-support")]
use std::net::IpAddr;

use crate::models::{
    AiringItem, EntryDetails, FormatCount, FuzzyDate, GenreStat, ListEntry, ListStatus, Media,
    MediaCharacter, MediaDetail, MediaRelation, MediaStaff, Notification, NotificationPage,
    ScoreBucket, SearchPage, StatusCount, User, UserStats, YearCount,
};

const GRAPHQL: &str = "https://graphql.anilist.co";
const AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
const OAUTH_EXCHANGE: &str = "https://kurisu-anilist-exchange.vercel.app/api/exchange";
/// Must match the registered OAuth redirect port.
pub const OAUTH_PORT: u16 = 39417;

pub fn valid_account_id(id: i64) -> bool {
    (1..=i64::from(i32::MAX)).contains(&id)
}

#[cfg(feature = "test-support")]
fn fixture_graphql_endpoint(value: &str) -> Result<String> {
    Ok(fixture_loopback_http_url(value)?.to_string())
}

#[cfg(feature = "test-support")]
pub(crate) fn fixture_loopback_http_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).map_err(|_| anyhow!("invalid fixture URL"))?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(anyhow!("fixture URL must be a plain HTTP loopback IP URL"));
    }
    let authority = value
        .strip_prefix("http://")
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .ok_or_else(|| anyhow!("fixture URL must use a loopback IP literal"))?;
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed
            .split_once(']')
            .map(|(host, _)| host)
            .ok_or_else(|| anyhow!("invalid fixture IP literal"))?
    } else {
        authority
            .split_once(':')
            .map_or(authority, |(host, _)| host)
    };
    let ip: IpAddr = host
        .parse()
        .map_err(|_| anyhow!("fixture host must be an IP literal"))?;
    if !ip.is_loopback() {
        return Err(anyhow!("fixture IP must be loopback"));
    }
    let parsed_ip = url.host_str().and_then(|host| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok()
    });
    if parsed_ip != Some(ip) {
        return Err(anyhow!("fixture host changed during URL parsing"));
    }
    Ok(url)
}

#[cfg(feature = "test-support")]
fn configured_fixture_graphql_endpoint() -> Option<String> {
    fixture_graphql_from_env_value(std::env::var("KURISU_FIXTURE_GRAPHQL"))
}

#[cfg(feature = "test-support")]
fn fixture_graphql_from_env_value(
    value: std::result::Result<String, std::env::VarError>,
) -> Option<String> {
    match value {
        Ok(value) => Some(
            fixture_graphql_endpoint(&value)
                .unwrap_or_else(|error| panic!("invalid KURISU_FIXTURE_GRAPHQL: {error}")),
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid KURISU_FIXTURE_GRAPHQL: {error}"),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextAiring {
    #[serde(deserialize_with = "Option::deserialize")]
    episode: Option<i64>,
    #[serde(deserialize_with = "Option::deserialize")]
    airing_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AniMedia {
    id: i64,
    #[serde(deserialize_with = "Option::deserialize")]
    id_mal: Option<i64>,
    title: AniTitle,
    // Selected fields may be null for stubs, but omitted fields are incomplete.
    #[serde(deserialize_with = "Option::deserialize")]
    cover_image: Option<AniCover>,
    #[serde(deserialize_with = "Option::deserialize")]
    episodes: Option<i64>,
    #[serde(deserialize_with = "Option::deserialize")]
    format: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    status: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    average_score: Option<i64>,
    #[serde(deserialize_with = "Option::deserialize")]
    season: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    season_year: Option<i64>,
    #[serde(deserialize_with = "Option::deserialize")]
    description: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    next_airing_episode: Option<NextAiring>,
}
#[derive(Deserialize, Default)]
struct AniTitle {
    #[serde(deserialize_with = "Option::deserialize")]
    romaji: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    english: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    native: Option<String>,
}
#[derive(Deserialize, Default)]
struct AniCover {
    #[serde(deserialize_with = "Option::deserialize")]
    medium: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    large: Option<String>,
}
#[derive(Deserialize)]
struct AniStudios {
    #[serde(deserialize_with = "Option::deserialize")]
    nodes: Option<Vec<Option<AniStudioNode>>>,
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
            episodes: m.episodes.filter(|total| *total > 0),
            format: m.format,
            status: m.status,
            average_score: m.average_score,
            season: m.season,
            season_year: m.season_year,
            description: m.description,
            next_airing_episode: m.next_airing_episode.as_ref().and_then(|n| n.episode),
            next_airing_at: m.next_airing_episode.as_ref().and_then(|n| n.airing_at),
            ..Default::default()
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct SavedEntry {
    pub id: i64,
    #[serde(rename = "mediaId")]
    media_id: i64,
    pub status: String,
    pub progress: i64,
    // Null is valid, but an omitted score makes the reply incomplete.
    #[serde(deserialize_with = "Option::deserialize")]
    pub score: Option<f64>,
    pub repeat: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<i64>,
}

// Omitted totals leave the cache alone. An explicit null clears it.
pub(crate) type ReportedEpisodes = Option<Option<i64>>;

fn reported_episodes<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<ReportedEpisodes, D::Error> {
    Option::<i64>::deserialize(deserializer).map(|total| Some(total.filter(|total| *total > 0)))
}

impl SavedEntry {
    fn check_identity(&self, media_id: i64, expected_id: Option<i64>) -> Result<()> {
        require_media_id(media_id, self.media_id)?;
        require_entry_id(self.id, expected_id)
    }

    fn retain_score_in_format(&mut self, score_format: Option<&str>) {
        self.score = score_in_format(self.score, score_format);
    }
}

fn score_in_format(score: Option<f64>, score_format: Option<&str>) -> Option<f64> {
    // Fresh null means unrated. Local null means the score is unknown.
    let score = score.unwrap_or(0.0);
    (score == 0.0 || score_format.is_some()).then_some(score)
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
    verified_account: Option<i64>,
    rate: Arc<Mutex<RateBudget>>,
    auth_intent: Option<(tokio::sync::watch::Receiver<u64>, u64)>,
    #[cfg(any(test, feature = "test-support"))]
    endpoint: Option<String>,
}

impl AniList {
    pub fn new() -> Self {
        #[cfg(feature = "test-support")]
        let endpoint = configured_fixture_graphql_endpoint();
        #[cfg(all(test, not(feature = "test-support")))]
        let endpoint = None;
        let http = reqwest::Client::builder()
            .user_agent("Kurisu")
            .timeout(Duration::from_secs(20));
        #[cfg(feature = "test-support")]
        let http = if endpoint.is_some() {
            http.no_proxy().redirect(reqwest::redirect::Policy::none())
        } else {
            http
        };
        let http = http.build().expect("reqwest client");
        AniList {
            http,
            token: None,
            verified_account: None,
            rate: Arc::new(Mutex::new(RateBudget::default())),
            auth_intent: None,
            #[cfg(any(test, feature = "test-support"))]
            endpoint,
        }
    }
    pub fn set_token(&mut self, t: Option<String>) {
        self.token = t;
        self.verified_account = None;
    }
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }
    pub fn token(&self) -> Option<String> {
        self.token.clone()
    }

    pub(crate) fn verified_account(&self) -> Option<i64> {
        self.verified_account
    }

    pub(crate) fn record_viewer(&mut self, account: i64) {
        self.verified_account = Some(account);
    }

    pub async fn exchange_code(&self, code: &str) -> Result<String> {
        let endpoint = OAUTH_EXCHANGE;
        #[cfg(test)]
        let endpoint = self.endpoint.as_deref().unwrap_or(endpoint);
        let http = reqwest::Client::builder()
            .user_agent("Kurisu")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(25));
        #[cfg(test)]
        let http = http.no_proxy();
        const MAX_EXCHANGE_BYTES: usize = 64 * 1024;
        const INVALID_RESPONSE: &str = "Kurisu's sign-in service returned an invalid response.";
        let mut response = http
            .build()?
            .post(endpoint)
            .header("Accept", "application/json")
            .json(&serde_json::json!({ "code": code }))
            .send()
            .await
            .map_err(|_| anyhow!("Could not reach Kurisu's sign-in service. Try again later."))?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(match status.as_u16() {
                400 => "AniList rejected this authorization code. Start sign-in again.",
                429 => "AniList is busy. Try signing in again later.",
                _ => "Kurisu's sign-in service could not complete authorization. Try again later.",
            }));
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_EXCHANGE_BYTES as u64)
        {
            return Err(anyhow!(INVALID_RESPONSE));
        }
        let mut buf = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!(INVALID_RESPONSE))?
        {
            if buf.len().saturating_add(chunk.len()) > MAX_EXCHANGE_BYTES {
                return Err(anyhow!(INVALID_RESPONSE));
            }
            buf.extend_from_slice(&chunk);
        }
        let token: TokenResponse =
            serde_json::from_slice(&buf).map_err(|_| anyhow!(INVALID_RESPONSE))?;
        if token.access_token.trim().is_empty() || token.access_token.len() > 16384 {
            return Err(anyhow!(
                "Kurisu's sign-in service returned an invalid access token."
            ));
        }
        Ok(token.access_token)
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
        let wait = reset.saturating_sub(now).saturating_add(1);
        if wait <= 0 || wait > 65 {
            return;
        }
        log::info!("AniList rate window exhausted, waiting {wait}s for the reset");
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
    }

    pub(crate) fn for_intent(
        &self,
        intent: &tokio::sync::watch::Sender<u64>,
        generation: u64,
    ) -> Self {
        Self {
            auth_intent: Some((intent.subscribe(), generation)),
            ..self.clone()
        }
    }

    async fn until_replaced<T>(
        &self,
        operation: impl std::future::Future<Output = T>,
    ) -> Result<T> {
        if let Some((intent, generation)) = &self.auth_intent {
            let mut intent = intent.clone();
            tokio::select! {
                biased;
                _ = intent.wait_for(|current| current != generation) => Err(UnsentRequest.into()),
                result = operation => Ok(result),
            }
        } else {
            Ok(operation.await)
        }
    }

    async fn gql<T: for<'de> serde::Deserialize<'de>>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T> {
        self.until_replaced(self.gql_request(query, vars)).await?
    }

    // Let dispatched writes finish so callers can distinguish rejection from uncertainty.
    async fn gql_request<T: for<'de> serde::Deserialize<'de>>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T> {
        anyhow::ensure!(self.has_token(), "not authenticated");
        self.gql_response(query, vars).await
    }

    async fn gql_response<T: for<'de> serde::Deserialize<'de>>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T> {
        self.gql_inner(query, vars).await.map_err(|mut error| {
            if let Some(token) = self.token.as_deref().filter(|token| !token.is_empty()) {
                if let Some(api_error) = error.downcast_mut::<ApiError>() {
                    api_error.message = api_error.message.replace(token, "[redacted]");
                }
            }
            error
        })
    }

    async fn gql_inner<T: for<'de> serde::Deserialize<'de>>(
        &self,
        query: &str,
        vars: serde_json::Value,
    ) -> Result<T> {
        let payload = serde_json::json!({ "query": query, "variables": vars });
        let mut retries = 0;
        let (status, body) = loop {
            self.until_replaced(self.wait_for_budget()).await?;
            let request = self
                .http
                .post(self.endpoint())
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&payload);
            let request = match &self.token {
                Some(token) => request.bearer_auth(token),
                None => request,
            };
            let resp = request.send().await?;
            let status = resp.status();
            self.note_rate_headers(resp.headers());
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| {
                        let reset = resp
                            .headers()
                            .get("x-ratelimit-reset")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<i64>().ok())?;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        Some(reset.saturating_sub(now).saturating_add(1).max(1) as u64)
                    })
                    .unwrap_or(1);
                if retries < 2 && wait <= 65 {
                    retries += 1;
                    self.until_replaced(tokio::time::sleep(Duration::from_secs(wait)))
                        .await?;
                    continue;
                }
                return Err(ApiError::new(
                    status,
                    true,
                    format!("rate limit reached; retry after {wait}s"),
                )
                .into());
            }
            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ApiError::new(status, false, e.to_string()))?;
            break (status, body);
        };
        if let Some(errs) = body.get("errors") {
            let error = errs.get(0);
            let Some(msg) = error
                .and_then(|e| e.get("message"))
                .and_then(|value| value.as_str())
                .filter(|message| !message.trim().is_empty())
            else {
                return Err(ApiError::new(
                    status,
                    false,
                    "AniList returned an unusable error response".into(),
                )
                .into());
            };
            let transport_success = status.is_success();
            let reported_status = error.and_then(graphql_error_status);
            // A transport 404 can carry a more specific account or service error.
            let status = if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                reported_status.unwrap_or(status)
            } else {
                status
            };
            let mut failure = ApiError::new(status, true, msg.to_string());
            if failure.not_found()
                && (errs.as_array().is_some_and(|errors| errors.len() > 1)
                    || error
                        .and_then(|error| error.get("path"))
                        .and_then(|path| path.as_array())
                        .is_some_and(|path| path.len() > 1))
            {
                failure.kind = ApiErrorKind::Partial;
            }
            if transport_success
                && graphql_field_errors(
                    errs,
                    |path| matches!(path, [root, _, ..] if root.as_str() == Some("SaveMediaListEntry")),
                )
            {
                failure.kind = ApiErrorKind::PartialEntryWrite;
            }
            if transport_success
                && body
                    .pointer("/data/Media/id")
                    .and_then(|id| id.as_i64())
                    .is_some_and(|id| {
                        id > 0 && vars.get("mediaId").and_then(|value| value.as_i64()) == Some(id)
                    })
                && graphql_field_errors(errs, |path| {
                    matches!(path, [root, field, ..]
                        if root.as_str() == Some("Media") && field.as_str() == Some("mediaListEntry"))
                })
            {
                failure.kind = ApiErrorKind::PartialEntryLookup;
            }
            if status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
                && errs.as_array().is_some_and(|errors| {
                    errors.iter().all(|error| {
                        graphql_error_status(error).is_none_or(|status| {
                            status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
                        }) && error
                            .get("message")
                            .and_then(|value| value.as_str())
                            .is_some_and(|message| {
                                !message.trim().is_empty()
                                    && ApiErrorKind::from_message(message) == ApiErrorKind::Other
                            })
                    })
                })
            {
                failure.kind = ApiErrorKind::EntryServer;
            }
            return Err(failure.into());
        }
        if !status.is_success() {
            return Err(ApiError::new(
                status,
                false,
                body.get("message")
                    .and_then(|value| value.as_str())
                    .or_else(|| status.canonical_reason())
                    .unwrap_or("request failed")
                    .to_string(),
            )
            .into());
        }
        Self::decode_data(body, status)
    }

    fn decode_data<T: for<'de> serde::Deserialize<'de>>(
        mut body: serde_json::Value,
        status: reqwest::StatusCode,
    ) -> Result<T> {
        let data = body
            .get_mut("data")
            .ok_or_else(|| ApiError::new(status, false, "AniList: no data field".into()))?
            .take();
        serde_json::from_value(data).map_err(|error| {
            ApiError::new(
                status,
                false,
                format!("AniList returned unusable data: {error}"),
            )
            .into()
        })
    }

    fn endpoint(&self) -> &str {
        #[cfg(any(test, feature = "test-support"))]
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
        anyhow::ensure!(valid_account_id(r.viewer.id), "Invalid AniList account ID");
        Ok(User {
            id: r.viewer.id,
            name: r.viewer.name,
            avatar: r.viewer.avatar.and_then(|a| a.large),
            score_format: r.viewer.media_list_options.and_then(|o| o.score_format),
            offline: false,
        })
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
        let has_next_page = r.page.page_info.has_next_page;
        let expected_items = has_next_page || !r.page.media.is_empty();
        let items: Vec<_> = r
            .page
            .media
            .into_iter()
            .flatten()
            .filter(|media| media.id > 0)
            .map(Media::from)
            .collect();
        anyhow::ensure!(
            !expected_items || !items.is_empty(),
            "AniList returned an incomplete search page. Try again later."
        );
        Ok(SearchPage {
            items,
            page,
            has_next_page,
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
        let mut seen = HashSet::new();
        let mut has_next = true;
        for page in 1..=20 {
            if page > 1 {
                self.until_replaced(tokio::time::sleep(Duration::from_millis(300)))
                    .await?;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "season": season, "year": year, "page": page }),
                )
                .await?;
            let media = r
                .page
                .media
                .ok_or_else(|| anyhow!("AniList returned unavailable season media"))?;
            has_next = r
                .page
                .page_info
                .and_then(|p| p.has_next_page)
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            let expected_items = !media.is_empty() || page > 1 || has_next;
            let mut media = media.into_iter().flatten().filter(|m| m.id > 0).peekable();
            if media.peek().is_none() && expected_items {
                anyhow::bail!(
                    "AniList stopped returning entries during this season. Try again later."
                );
            }
            for m in media {
                if seen.insert(m.id) {
                    out.push(Media::from(m));
                }
            }
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
            recommendations: Option<Vec<Option<Rec>>>,
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
            .ok_or_else(|| anyhow!("AniList returned unavailable recommendations"))?
            .into_iter()
            .flatten()
            .filter_map(|rec| rec.media_recommendation)
            .filter(|media| media.id > 0)
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
        require_media_id(id, r.media.id)?;
        Ok(Media::from(r.media))
    }

    pub async fn media_detail(&self, id: i64) -> Result<MediaDetail> {
        type Connection = serde_json::Map<String, serde_json::Value>;

        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Media")]
            media: DetailMedia,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct DetailMedia {
            #[serde(deserialize_with = "Option::deserialize")]
            banner_image: Option<String>,
            #[serde(deserialize_with = "Option::deserialize")]
            genres: Option<Vec<Option<String>>>,
            #[serde(deserialize_with = "Option::deserialize")]
            duration: Option<i64>,
            #[serde(deserialize_with = "Option::deserialize")]
            source: Option<String>,
            #[serde(deserialize_with = "Option::deserialize")]
            studios: Option<AniStudios>,
            relations: Option<Connection>,
            characters: Option<Connection>,
            staff: Option<Connection>,
            #[serde(flatten)]
            media: AniMedia,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RelEdge {
            #[serde(deserialize_with = "Option::deserialize")]
            relation_type: Option<String>,
            node: Option<RelNode>,
        }
        #[derive(Deserialize)]
        struct RelNode {
            #[serde(rename = "type")]
            kind: Option<MediaKind>,
            #[serde(flatten)]
            media: AniMedia,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "UPPERCASE")]
        enum MediaKind {
            Anime,
            Manga,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CharEdge {
            #[serde(default, deserialize_with = "selected")]
            role: Option<Option<String>>,
            node: Option<NamedNode>,
            #[serde(default, deserialize_with = "selected")]
            voice_actors: Option<Option<Vec<Option<NamedNode>>>>,
        }
        #[derive(Deserialize)]
        struct StaffEdge {
            #[serde(default, deserialize_with = "selected")]
            role: Option<Option<String>>,
            node: Option<NamedNode>,
        }
        #[derive(Deserialize)]
        struct NamedNode {
            #[serde(default, deserialize_with = "selected")]
            name: Option<Option<NodeName>>,
            #[serde(default, deserialize_with = "selected")]
            image: Option<Option<NodeImage>>,
        }
        impl NamedNode {
            fn fields(self) -> Option<(Option<String>, Option<String>)> {
                let name = match self.name? {
                    Some(name) => name.full?,
                    None => None,
                };
                let image = match self.image? {
                    Some(image) => image.medium?,
                    None => None,
                };
                Some((name, image))
            }
        }
        #[derive(Deserialize)]
        struct NodeName {
            #[serde(default, deserialize_with = "selected")]
            full: Option<Option<String>>,
        }
        #[derive(Deserialize)]
        struct NodeImage {
            #[serde(default, deserialize_with = "selected")]
            medium: Option<Option<String>>,
        }
        fn selected<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
            value: D,
        ) -> Result<Option<Option<T>>, D::Error> {
            Option::deserialize(value).map(Some)
        }
        fn section<T: serde::de::DeserializeOwned, U>(
            connection: Option<Connection>,
            name: &str,
            unavailable: &mut Vec<String>,
            convert: impl Fn(T) -> Option<U>,
        ) -> Result<Vec<U>> {
            let edges = connection
                .and_then(|mut connection| connection.remove("edges"))
                .map(serde_json::from_value::<Option<Vec<Option<T>>>>)
                .transpose()
                .map_err(|error| {
                    ApiError::new(
                        reqwest::StatusCode::OK,
                        false,
                        format!("AniList returned unusable {name}: {error}"),
                    )
                })?
                .flatten();
            Ok(edges
                .and_then(|edges| edges.into_iter().map(|edge| convert(edge?)).collect())
                .unwrap_or_else(|| {
                    unavailable.push(name.to_owned());
                    Vec::new()
                }))
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
        let r: R = self
            .until_replaced(self.gql_response(q, serde_json::json!({ "id": id })))
            .await??;
        require_media_id(id, r.media.media.id)?;
        let mut unavailable_sections = Vec::new();
        let relations = section(
            r.media.relations,
            "relations",
            &mut unavailable_sections,
            |edge: RelEdge| {
                let node = edge.node?;
                let kind = node.kind?;
                if matches!(kind, MediaKind::Anime) && node.media.id <= 0 {
                    return None;
                }
                Some((kind, node.media, edge.relation_type))
            },
        )?
        .into_iter()
        .filter(|(kind, _, _)| matches!(kind, MediaKind::Anime))
        .map(|(_, media, relation)| MediaRelation {
            relation: relation.unwrap_or_else(|| "OTHER".to_string()),
            media: media.into(),
        })
        .collect();
        let characters = section(
            r.media.characters,
            "characters",
            &mut unavailable_sections,
            |edge: CharEdge| {
                let (name, image) = edge.node?.fields()?;
                let actors = edge.voice_actors?.unwrap_or_default();
                if actors.iter().any(Option::is_none) {
                    return None;
                }
                let (va_name, va_image) = match actors.into_iter().next().flatten() {
                    Some(actor) => actor.fields()?,
                    None => (None, None),
                };
                Some(MediaCharacter {
                    role: edge.role?,
                    name: name?,
                    image,
                    va_name,
                    va_image,
                })
            },
        )?;
        let staff = section(
            r.media.staff,
            "staff",
            &mut unavailable_sections,
            |edge: StaffEdge| {
                let (name, image) = edge.node?.fields()?;
                Some(MediaStaff {
                    role: edge.role?,
                    name: name?,
                    image,
                })
            },
        )?;
        let media = Media {
            banner_image: r.media.banner_image,
            genres: r
                .media
                .genres
                .map(|g| g.into_iter().flatten().collect::<Vec<_>>())
                .filter(|g: &Vec<String>| !g.is_empty()),
            duration: r.media.duration,
            source: r.media.source,
            studios: r
                .media
                .studios
                .and_then(|s| s.nodes)
                .map(|n| n.into_iter().flatten().map(|s| s.name).collect::<Vec<_>>())
                .filter(|s: &Vec<String>| !s.is_empty()),
            ..r.media.media.into()
        };
        Ok(MediaDetail {
            media,
            relations,
            characters,
            staff,
            cached_at: None,
            warning: None,
            unavailable_sections,
        })
    }

    pub async fn user_statistics(&self, account_id: i64) -> Result<UserStats> {
        anyhow::ensure!(valid_account_id(account_id), "Invalid AniList account ID");
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "User")]
            user: UserNode,
        }
        #[derive(Deserialize)]
        struct UserNode {
            id: i64,
            statistics: Option<Statistics>,
        }
        #[derive(Deserialize)]
        struct Statistics {
            anime: Option<Anime>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Anime {
            count: i64,
            episodes_watched: i64,
            minutes_watched: i64,
            mean_score: f64,
            standard_deviation: f64,
            #[serde(deserialize_with = "Option::deserialize")]
            scores: Option<Vec<Score>>,
            #[serde(deserialize_with = "Option::deserialize")]
            statuses: Option<Vec<Status>>,
            #[serde(deserialize_with = "Option::deserialize")]
            formats: Option<Vec<Format>>,
            #[serde(deserialize_with = "Option::deserialize")]
            genres: Option<Vec<Genre>>,
            #[serde(deserialize_with = "Option::deserialize")]
            release_years: Option<Vec<Year>>,
        }
        #[derive(Deserialize)]
        struct Score {
            score: i64,
            count: i64,
        }
        #[derive(Deserialize)]
        struct Status {
            #[serde(deserialize_with = "Option::deserialize")]
            status: Option<String>,
            count: i64,
        }
        #[derive(Deserialize)]
        struct Format {
            #[serde(deserialize_with = "Option::deserialize")]
            format: Option<String>,
            count: i64,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Genre {
            #[serde(deserialize_with = "Option::deserialize")]
            genre: Option<String>,
            count: i64,
            #[serde(deserialize_with = "Option::deserialize")]
            minutes_watched: Option<i64>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Year {
            #[serde(deserialize_with = "Option::deserialize")]
            release_year: Option<i64>,
            count: i64,
        }
        let q = "query ($accountId: Int!) {
            User(id: $accountId) {
                id
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
            .gql(q, serde_json::json!({ "accountId": account_id }))
            .await?;
        anyhow::ensure!(
            r.user.id == account_id,
            "AniList returned statistics for a different account"
        );
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
            airing_schedules: Option<Vec<Option<Sched>>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            #[serde(default, deserialize_with = "present_page")]
            current_page: Option<i64>,
            has_next_page: Option<bool>,
        }
        fn present_page<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<i64>, D::Error> {
            i64::deserialize(deserializer).map(Some)
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
            #[serde(deserialize_with = "Option::deserialize")]
            is_adult: Option<bool>,
            #[serde(flatten)]
            media: AniMedia,
        }
        // Include the start time despite AniList's exclusive lower bound.
        let start_exclusive = start.saturating_sub(1);
        let q = "query ($startExclusive: Int!, $end: Int!, $page: Int!) {
            Page(page: $page, perPage: 50) {
                pageInfo { currentPage hasNextPage }
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
        let mut seen = HashSet::new();
        let mut has_next = true;
        for page in 1..=12 {
            if page > 1 {
                self.until_replaced(tokio::time::sleep(Duration::from_millis(300)))
                    .await?;
            }
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "startExclusive": start_exclusive, "end": end, "page": page }),
                )
                .await?;
            let scheds = r
                .page
                .airing_schedules
                .ok_or_else(|| anyhow!("AniList returned unavailable airing schedules"))?;
            let page_info = r
                .page
                .page_info
                .ok_or_else(|| anyhow!("AniList returned unavailable airing schedule page info"))?;
            has_next = page_info
                .has_next_page
                .ok_or_else(|| anyhow!("AniList returned a null pageInfo.hasNextPage"))?;
            if page_info.current_page != Some(page)
                && (page > 1 || has_next || page_info.current_page.is_some())
            {
                anyhow::bail!(
                    "AniList returned an invalid airing schedule page number. Try again later."
                );
            }
            if scheds.is_empty() && (page > 1 || has_next) {
                anyhow::bail!("AniList skipped an airing schedule page. Try again later.");
            }
            let previous_count = seen.len();
            for s in scheds {
                let Some(Sched {
                    airing_at,
                    episode,
                    media: Some(media),
                }) = s
                else {
                    anyhow::bail!("AniList returned an incomplete airing schedule page");
                };
                if !(start..end).contains(&airing_at) || episode < 0 || media.media.id <= 0 {
                    anyhow::bail!("AniList returned an invalid airing schedule row");
                }
                if !seen.insert((media.media.id, episode, airing_at))
                    || media.is_adult == Some(true)
                {
                    continue;
                }
                out.push(AiringItem {
                    airing_at,
                    episode,
                    media: media.media.into(),
                });
            }
            if page > 1 && previous_count == seen.len() {
                anyhow::bail!("AniList repeated an airing schedule page. Try again later.");
            }
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
    pub async fn user_list(
        &self,
        account_id: i64,
        score_format: Option<&str>,
    ) -> Result<Vec<ListEntry>> {
        anyhow::ensure!(valid_account_id(account_id), "Invalid AniList account ID");
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "MediaListCollection")]
            collection: Collection,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Collection {
            user: Owner,
            lists: Option<Vec<AniList>>,
            has_next_chunk: Option<bool>,
        }
        #[derive(Deserialize)]
        struct Owner {
            id: i64,
        }
        #[derive(Deserialize)]
        struct AniList {
            entries: Option<Vec<Option<Entry>>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Entry {
            id: i64,
            status: String,
            progress: i64,
            #[serde(deserialize_with = "Option::deserialize")]
            score: Option<f64>,
            repeat: i64,
            updated_at: Option<i64>,
            media_id: i64,
            media: Option<AniMedia>,
        }
        let q = "query ($accountId: Int!, $chunk: Int!, $scoreFormat: ScoreFormat) {
            MediaListCollection(userId: $accountId, type: ANIME, chunk: $chunk, perChunk: 500) {
                user { id }
                hasNextChunk
                lists {
                    entries {
                        id status progress score(format: $scoreFormat) repeat updatedAt mediaId
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
        let mut out: Vec<ListEntry> = Vec::new();
        let mut positions = HashMap::<i64, (usize, f64)>::new();
        let mut entry_ids = HashSet::new();
        for chunk in 1..=200 {
            let r: R = self
                .gql(
                    q,
                    serde_json::json!({ "accountId": account_id, "chunk": chunk, "scoreFormat": score_format }),
                )
                .await?;
            anyhow::ensure!(
                r.collection.user.id == account_id,
                "AniList returned a list for a different account"
            );
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
                    require_entry_id(e.id, None)?;
                    anyhow::ensure!(
                        e.media_id > 0,
                        "AniList returned an invalid anime ID {}",
                        e.media_id
                    );
                    if let Some(media) = &e.media {
                        require_media_id(e.media_id, media.id)?;
                    }
                    let media = e.media.map(Media::from);
                    let score = e.score.unwrap_or(0.0);
                    // Status and custom groups can share an entry within one chunk.
                    if let Some(&(index, previous_score)) = positions.get(&e.media_id) {
                        anyhow::ensure!(
                            index >= before,
                            "AniList returned an overlapping chunk, refusing to sync a partial list"
                        );
                        let duplicate = &mut out[index];
                        require_entry_id(e.id, duplicate.id)?;
                        anyhow::ensure!(
                            (
                                duplicate.status.as_str(),
                                duplicate.progress,
                                previous_score,
                                duplicate.repeat,
                                duplicate.updated_at
                            ) == (e.status.as_str(), e.progress, score, e.repeat, e.updated_at)
                                && duplicate
                                    .media
                                    .as_ref()
                                    .zip(media.as_ref())
                                    .is_none_or(|(a, b)| a == b),
                            "AniList returned inconsistent copies of entry {}",
                            e.id
                        );
                        if duplicate.media.is_none() {
                            duplicate.media = media;
                        }
                        continue;
                    }
                    anyhow::ensure!(
                        entry_ids.insert(e.id),
                        "AniList returned entry {} for multiple anime",
                        e.id
                    );
                    // Keep null media entries so AniList merges cannot trigger local deletion.
                    positions.insert(e.media_id, (out.len(), score));
                    out.push(ListEntry {
                        id: Some(e.id),
                        media_id: e.media_id,
                        status: e.status,
                        progress: e.progress,
                        score: score_in_format(e.score, score_format),
                        repeat: e.repeat,
                        updated_at: e.updated_at,
                        media,
                        queued_creation: None,
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

    pub async fn entry_by_media_id(
        &self,
        media_id: i64,
        score_format: Option<&str>,
    ) -> Result<Option<SavedEntry>> {
        self.entry_by_media_id_with_episodes(media_id, score_format)
            .await
            .map(|(entry, _)| entry)
    }

    pub(crate) async fn entry_by_media_id_with_episodes(
        &self,
        media_id: i64,
        score_format: Option<&str>,
    ) -> Result<(Option<SavedEntry>, ReportedEpisodes)> {
        let (mut entry, episodes) = self.fetch_entry(media_id, score_format).await?;
        if let Some(entry) = entry.as_mut() {
            entry.retain_score_in_format(score_format);
        }
        Ok((entry, episodes))
    }

    async fn fetch_entry(
        &self,
        media_id: i64,
        score_format: Option<&str>,
    ) -> Result<(Option<SavedEntry>, ReportedEpisodes)> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "Media")]
            media: EntryMedia,
        }
        #[derive(Deserialize)]
        struct EntryMedia {
            id: i64,
            #[serde(default, deserialize_with = "reported_episodes")]
            episodes: ReportedEpisodes,
            #[serde(default, rename = "mediaListEntry", deserialize_with = "present_value")]
            entry: Option<serde_json::Value>,
        }
        fn present_value<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Option<serde_json::Value>, D::Error> {
            serde_json::Value::deserialize(deserializer).map(Some)
        }
        let q = "query ($mediaId: Int!, $scoreFormat: ScoreFormat) {
            Media(id: $mediaId, type: ANIME) {
                id episodes
                mediaListEntry { id mediaId status progress score(format: $scoreFormat) repeat updatedAt }
            }
        }";
        let r: R = self
            .gql(
                q,
                serde_json::json!({ "mediaId": media_id, "scoreFormat": score_format }),
            )
            .await?;
        require_media_id(media_id, r.media.id)?;
        let raw = r
            .media
            .entry
            .ok_or_else(|| IncompleteEntryLookup("missing field `mediaListEntry`".into()))?;
        let entry: Option<SavedEntry> = serde_json::from_value(raw)
            .map_err(|error| IncompleteEntryLookup(error.to_string()))?;
        if let Some(entry) = &entry {
            entry.check_identity(media_id, None)?;
        }
        Ok((entry, r.media.episodes))
    }

    /// Omitted fields stay unchanged on AniList.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn save_entry(
        &self,
        media_id: i64,
        entry_id: Option<i64>,
        status: Option<ListStatus>,
        progress: Option<i64>,
        score: Option<f64>,
        repeat: Option<i64>,
        score_format: Option<&str>,
        allow_adjusted_completion: bool,
    ) -> Result<(SavedEntry, Option<i64>)> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "SaveMediaListEntry")]
            entry: SavedEntry,
        }
        let q = "mutation ($id: Int, $mediaId: Int, $status: MediaListStatus, $progress: Int, $score: Float, $repeat: Int, $scoreFormat: ScoreFormat) {
            SaveMediaListEntry(id: $id, mediaId: $mediaId, status: $status, progress: $progress, score: $score, repeat: $repeat) { id mediaId status progress score(format: $scoreFormat) repeat updatedAt }
        }";
        let mut vars = serde_json::json!({"scoreFormat": score_format});
        if let Some(id) = entry_id {
            vars["id"] = id.into();
        } else {
            vars["mediaId"] = media_id.into();
        }
        if let Some(st) = status {
            vars["status"] = st.as_str().into();
        }
        if let Some(p) = progress {
            vars["progress"] = p.into();
        }
        if let Some(s) = score {
            vars["score"] = s.into();
        }
        if let Some(r) = repeat {
            vars["repeat"] = r.into();
        }
        let r: R = self.gql_request(q, vars).await.map_err(|error| {
            if error
                .downcast_ref::<ApiError>()
                .is_some_and(|api| api.kind == ApiErrorKind::PartialEntryWrite)
            {
                error.context(UnconfirmedEntryWrite(None))
            } else {
                error
            }
        })?;
        let mut entry = r.entry;
        entry.check_identity(media_id, entry_id)?;
        let requested_status = status.map(ListStatus::as_str);
        let status_changed = requested_status.is_some_and(|status| status != entry.status);
        let progress_changed = progress.is_some_and(|progress| progress != entry.progress);
        let mut confirmed_total = None;
        if !status_changed && progress_changed && status == Some(ListStatus::Completed) {
            let (remote, total) = self
                .fetch_entry(media_id, score_format)
                .await
                .map_err(|e| e.context(UnconfirmedEntryWrite(None)))?;
            let Some(remote) = remote.filter(|remote| {
                remote.id == entry.id
                    && remote.status == entry.status
                    && remote.progress == entry.progress
            }) else {
                return Err(UnconfirmedEntryWrite(total).into());
            };
            entry = remote;
            confirmed_total = total;
        }
        let confirmed_adjustment =
            allow_adjusted_completion && confirmed_total.flatten() == Some(entry.progress);
        let status_or_progress_changed =
            status_changed || progress_changed && !confirmed_adjustment;
        let score_or_repeat_changed = score
            .is_some_and(|score| entry.score.unwrap_or(0.0) != score)
            || repeat.is_some_and(|repeat| entry.repeat != repeat);
        if status_or_progress_changed || score_or_repeat_changed {
            return Err(IncompatibleEntryWrite {
                id: entry.id,
                status: entry.status,
                progress: entry.progress,
                requested_status: requested_status.map(str::to_owned),
                requested_progress: progress,
                total: confirmed_total,
                score_or_repeat_only: !status_or_progress_changed,
            }
            .into());
        }
        entry.retain_score_in_format(score_format);
        Ok((entry, confirmed_total.flatten()))
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
        let r: R = match self
            .gql_request(q, serde_json::json!({ "id": entry_id }))
            .await
        {
            Ok(r) => r,
            Err(e) if is_not_found(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        match r.entry.and_then(|e| e.deleted) {
            Some(true) => Ok(true),
            Some(false) => Err(anyhow!("AniList declined the delete")),
            None => Err(ApiError::new(
                reqwest::StatusCode::OK,
                false,
                "AniList did not confirm the delete".into(),
            )
            .into()),
        }
    }

    pub(crate) async fn entry_details_with_id(
        &self,
        media_id: i64,
    ) -> Result<Option<(i64, EntryDetails)>> {
        let q = "query ($id: Int!) {
            Media(id: $id, type: ANIME) { id mediaListEntry { id mediaId notes startedAt { year month day } completedAt { year month day } customLists } }
            Viewer { mediaListOptions { animeList { customLists } } }
        }";
        let r: serde_json::Value = self.gql(q, serde_json::json!({"id": media_id})).await?;
        let media = r
            .get("Media")
            .ok_or_else(|| anyhow!("AniList did not return the requested anime"))?;
        if media.is_null() {
            return Ok(None);
        }
        let returned_media_id = media.get("id").and_then(|value| value.as_i64());
        if let Some(id) = returned_media_id {
            require_media_id(media_id, id)?;
        }
        let raw = media
            .get("mediaListEntry")
            .ok_or_else(|| anyhow!("AniList did not return entry details"))?;
        if raw.is_null() {
            anyhow::ensure!(
                returned_media_id.is_some(),
                "AniList did not confirm the anime ID"
            );
            return Ok(None);
        }
        let (entry_id, mut details) = parse_entry_details(raw, media_id, None)?;
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
        Ok(Some((entry_id, details)))
    }

    pub async fn save_entry_details(
        &self,
        media_id: i64,
        entry_id: i64,
        notes: Option<&str>,
        started_at: Option<&FuzzyDate>,
        completed_at: Option<&FuzzyDate>,
        custom_lists: Option<&[String]>,
    ) -> Result<EntryDetails> {
        let q = "mutation ($id: Int!, $notes: String, $started: FuzzyDateInput, $completed: FuzzyDateInput, $lists: [String]) {
            SaveMediaListEntry(id: $id, notes: $notes, startedAt: $started, completedAt: $completed, customLists: $lists) {
                id mediaId notes startedAt { year month day } completedAt { year month day } customLists
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
        let r: serde_json::Value = self.gql_request(q, vars).await?;
        let (_, details) = parse_entry_details(
            r.get("SaveMediaListEntry")
                .filter(|v| !v.is_null())
                .ok_or(UnconfirmedEntryDetailsWrite)?,
            media_id,
            Some(entry_id),
        )
        .map_err(|e| e.context(UnconfirmedEntryDetailsWrite))?;
        let empty_date = FuzzyDate::default();
        anyhow::ensure!(
            notes.is_none_or(|notes| details.notes == notes)
                && started_at.is_none_or(|date| {
                    details.started_at.as_ref().unwrap_or(&empty_date) == date
                })
                && completed_at.is_none_or(|date| {
                    details.completed_at.as_ref().unwrap_or(&empty_date) == date
                })
                && custom_lists.is_none_or(|lists| {
                    use std::collections::BTreeSet;
                    lists.iter().collect::<BTreeSet<_>>()
                        == details.custom_lists.iter().collect::<BTreeSet<_>>()
                }),
            UnconfirmedEntryDetailsWrite
        );
        Ok(details)
    }

    pub async fn mark_notifications_read(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "Page")]
            _page: ReadPage,
        }
        #[derive(Deserialize)]
        struct ReadPage {
            #[serde(rename = "notifications")]
            _notifications: Vec<Option<ReadNotification>>,
        }
        #[derive(Deserialize)]
        struct ReadNotification {
            #[serde(rename = "id")]
            _id: Option<i32>,
        }
        let _: Response = self.gql_request("query { Page(page: 1, perPage: 1) { notifications(resetNotificationCount: true) { ... on AiringNotification { id } } } }", serde_json::json!({})).await?;
        Ok(())
    }

    /// Leave AniList's unread count unchanged when opening the inbox.
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
            unread_notification_count: Option<i32>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            notifications: Vec<Option<Raw>>,
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            current_page: i64,
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
            #[serde(rename = "__typename")]
            typename: Option<String>,
            id: Option<i32>,
            #[serde(rename = "type")]
            kind: Option<String>,
            context: Option<String>,
            // Airing notifications use a list of contexts.
            contexts: Option<Vec<Option<String>>>,
            created_at: Option<i32>,
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
            id: Option<i64>,
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
            id: Option<i64>,
            title: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct UserRef {
            name: Option<String>,
            avatar: Option<AvatarRef>,
        }
        #[derive(Deserialize)]
        struct AvatarRef {
            large: Option<String>,
        }
        let q = "query ($page: Int!) { Viewer { unreadNotificationCount } Page(page: $page, perPage: 50) { pageInfo { currentPage hasNextPage } notifications(resetNotificationCount: false) {
            __typename
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
        if r.page.page_info.current_page != page {
            anyhow::bail!("AniList returned the wrong notifications page. Try again later.");
        }
        if r.page.page_info.has_next_page && r.page.notifications.is_empty() {
            anyhow::bail!("AniList returned an empty notifications page. Try again later.");
        }
        let unread_count = r.viewer.unread_notification_count.map(i64::from);
        anyhow::ensure!(
            unread_count.is_none_or(|count| count >= 0),
            "AniList returned an invalid unread notification count"
        );
        let had_rows = !r.page.notifications.is_empty();
        let mut has_unrecognized_type = false;
        for row in r.page.notifications.iter().flatten() {
            match (row.kind.as_deref(), row.id) {
                (None, None) => {
                    // New union members return only their GraphQL type name.
                    has_unrecognized_type |=
                        row.typename.as_deref().is_some_and(|name| !name.is_empty());
                }
                (Some(kind), Some(id)) if id > 0 && !kind.trim().is_empty() => {}
                _ => anyhow::bail!(
                    "AniList returned an incomplete notifications page. Try again later."
                ),
            }
        }
        let items: Vec<_> = r
            .page
            .notifications
            .into_iter()
            .flatten()
            .filter_map(|n| {
                let id = i64::from(n.id?);
                let kind = n.kind?;
                Some(Notification {
                    id,
                    kind,
                    context: n.context.or_else(|| {
                        let parts = n
                            .contexts
                            .into_iter()
                            .flatten()
                            .flatten()
                            .collect::<Vec<_>>();
                        (!parts.is_empty()).then(|| parts.join(" "))
                    }),
                    created_at: n.created_at.map(i64::from),
                    media_id: n.media.as_ref().and_then(|m| m.id).filter(|id| *id > 0),
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
                    activity_id: n.activity_id.filter(|id| *id > 0),
                    thread_id: n.thread.as_ref().and_then(|t| t.id).filter(|id| *id > 0),
                    thread_title: n.thread.as_ref().and_then(|t| t.title.clone()),
                    comment_id: n.comment_id.filter(|id| *id > 0),
                    comment_url: n.comment.and_then(|c| c.site_url),
                    reason: n.reason,
                    deleted_media_title: n.deleted_media_title,
                    user_name: n.user.as_ref().and_then(|u| u.name.clone()),
                    user_avatar: n
                        .user
                        .as_ref()
                        .and_then(|u| u.avatar.as_ref().and_then(|a| a.large.clone())),
                })
            })
            .collect();
        if had_rows && items.is_empty() && !has_unrecognized_type {
            anyhow::bail!("AniList returned an incomplete notifications page. Try again later.");
        }
        Ok(NotificationPage {
            items,
            page,
            has_next_page: r.page.page_info.has_next_page,
            unread_count,
        })
    }
}

fn require_media_id(expected: i64, actual: i64) -> Result<()> {
    if actual <= 0 || actual != expected {
        return Err(ApiError::new(
            reqwest::StatusCode::OK,
            false,
            format!("AniList returned anime {actual} instead of {expected}"),
        )
        .into());
    }
    Ok(())
}

fn graphql_error_status(error: &serde_json::Value) -> Option<reqwest::StatusCode> {
    error
        .get("status")
        .and_then(|status| status.as_u64())
        .and_then(|status| u16::try_from(status).ok())
        .and_then(|status| reqwest::StatusCode::from_u16(status).ok())
        .filter(|status| status.is_client_error() || status.is_server_error())
}

fn graphql_field_errors(
    errors: &serde_json::Value,
    matches_path: impl Fn(&[serde_json::Value]) -> bool,
) -> bool {
    errors.as_array().is_some_and(|errors| {
        errors.iter().all(|error| {
            error
                .get("status")
                .is_none_or(|status| matches!(status.as_u64(), Some(200..=299 | 404)))
                && error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .is_some_and(|message| {
                        !message.trim().is_empty()
                            && matches!(
                                ApiErrorKind::from_message(message),
                                ApiErrorKind::Other | ApiErrorKind::Missing
                            )
                    })
                && error
                    .get("path")
                    .and_then(|path| path.as_array())
                    .is_some_and(|path| matches_path(path))
        })
    })
}

fn require_entry_id(id: i64, expected: Option<i64>) -> Result<()> {
    if id <= 0 || expected.is_some_and(|expected| id != expected) {
        return Err(ApiError::new(
            reqwest::StatusCode::OK,
            false,
            expected.map_or_else(
                || format!("AniList returned an invalid list entry ID {id}"),
                |expected| format!("AniList returned entry {id} instead of {expected}"),
            ),
        )
        .into());
    }
    Ok(())
}

fn parse_entry_details(
    raw: &serde_json::Value,
    media_id: i64,
    expected_id: Option<i64>,
) -> Result<(i64, EntryDetails)> {
    #[derive(Deserialize)]
    struct Date {
        #[serde(deserialize_with = "Option::deserialize")]
        year: Option<i64>,
        #[serde(deserialize_with = "Option::deserialize")]
        month: Option<i64>,
        #[serde(deserialize_with = "Option::deserialize")]
        day: Option<i64>,
    }
    impl From<Date> for FuzzyDate {
        fn from(date: Date) -> Self {
            Self {
                year: date.year,
                month: date.month,
                day: date.day,
            }
        }
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Details {
        id: i64,
        media_id: i64,
        // Selected fields may be null, but must be present.
        #[serde(deserialize_with = "Option::deserialize")]
        notes: Option<String>,
        #[serde(deserialize_with = "Option::deserialize")]
        started_at: Option<Date>,
        #[serde(deserialize_with = "Option::deserialize")]
        completed_at: Option<Date>,
        #[serde(deserialize_with = "Option::deserialize")]
        custom_lists: Option<std::collections::BTreeMap<String, bool>>,
    }
    let entry: Details = serde_json::from_value(raw.clone())?;
    require_media_id(media_id, entry.media_id)?;
    require_entry_id(entry.id, expected_id)?;
    Ok((
        entry.id,
        EntryDetails {
            notes: entry.notes.unwrap_or_default(),
            started_at: entry.started_at.map(FuzzyDate::from),
            completed_at: entry.completed_at.map(FuzzyDate::from),
            custom_lists: entry
                .custom_lists
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(name, enabled)| enabled.then_some(name))
                .collect(),
            ..Default::default()
        },
    ))
}

/// Keep transport errors distinct from AniList errors.
#[derive(Debug)]
struct ApiError {
    status: reqwest::StatusCode,
    from_json: bool,
    message: String,
    kind: ApiErrorKind,
}

#[derive(Debug, PartialEq, Eq)]
enum ApiErrorKind {
    Other,
    Missing,
    Partial,
    PartialEntryLookup,
    Rejected,
    Disabled,
    EntryServer,
    PartialEntryWrite,
}

impl ApiErrorKind {
    fn from_message(message: &str) -> Self {
        if message == "Not Found" {
            Self::Missing
        } else if message.eq_ignore_ascii_case("Invalid Token") {
            Self::Rejected
        } else if message
            .to_ascii_lowercase()
            .starts_with("the anilist api has been temporarily disabled")
        {
            Self::Disabled
        } else {
            Self::Other
        }
    }
}

impl ApiError {
    fn not_found(&self) -> bool {
        self.from_json
            && !matches!(
                self.kind,
                ApiErrorKind::Rejected
                    | ApiErrorKind::Partial
                    | ApiErrorKind::PartialEntryLookup
                    | ApiErrorKind::PartialEntryWrite
            )
            && (self.status == reqwest::StatusCode::NOT_FOUND
                || self.status.is_success() && self.kind == ApiErrorKind::Missing)
    }

    fn new(status: reqwest::StatusCode, from_json: bool, message: String) -> Self {
        // Classify before redacting diagnostic text.
        Self {
            status,
            from_json,
            kind: ApiErrorKind::from_message(&message),
            message,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AniList ({}): {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug)]
pub(crate) struct UnsentRequest;

impl std::fmt::Display for UnsentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The account changed before the request could be sent.")
    }
}

impl std::error::Error for UnsentRequest {}

// The mutation may have committed before confirmation failed.
#[derive(Debug)]
pub(crate) struct UnconfirmedEntryWrite(pub ReportedEpisodes);

impl std::fmt::Display for UnconfirmedEntryWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AniList did not confirm the requested status and progress.")
    }
}

impl std::error::Error for UnconfirmedEntryWrite {}

#[derive(Debug)]
struct IncompleteEntryLookup(String);

impl std::fmt::Display for IncompleteEntryLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AniList returned unusable entry data: {}", self.0)
    }
}

impl std::error::Error for IncompleteEntryLookup {}

#[derive(Debug)]
pub(crate) struct UnconfirmedEntryDetailsWrite;

impl std::fmt::Display for UnconfirmedEntryDetailsWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AniList did not confirm the requested entry details. Reload the entry before trying again.")
    }
}

impl std::error::Error for UnconfirmedEntryDetailsWrite {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IncompatibleEntryWrite {
    pub id: i64,
    pub status: String,
    pub progress: i64,
    pub requested_status: Option<String>,
    pub requested_progress: Option<i64>,
    #[serde(
        default,
        deserialize_with = "reported_episodes",
        skip_serializing_if = "Option::is_none"
    )]
    pub total: ReportedEpisodes,
    #[serde(skip)]
    pub(crate) score_or_repeat_only: bool,
}

impl IncompatibleEntryWrite {
    pub(crate) const MESSAGE: &'static str =
        "AniList did not confirm the requested status and progress. Review this saved change.";

    pub(crate) fn matches_entry(&self, entry: &ListEntry) -> bool {
        entry.id == Some(self.id) && entry.status == self.status && entry.progress == self.progress
    }

    pub(crate) fn clamped_active_entry(&self, base: &ListEntry) -> bool {
        self.requested_status.is_none()
            && matches!(base.status.as_str(), "CURRENT" | "REPEATING")
            && self.status == "COMPLETED"
            && self.progress > 0
            && self.requested_progress.is_some_and(|p| p > self.progress)
    }

    pub(crate) fn forced_completion(&self) -> bool {
        self.status == "COMPLETED"
            && self.requested_progress.is_some()
            && matches!(
                self.requested_status.as_deref(),
                Some("CURRENT" | "REPEATING")
            )
    }
}

impl std::fmt::Display for IncompatibleEntryWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.score_or_repeat_only {
            f.write_str("AniList did not confirm the requested score or rewatch count. Review this saved change.")
        } else {
            f.write_str(Self::MESSAGE)
        }
    }
}

impl std::error::Error for IncompatibleEntryWrite {}

/// AniList can report Not Found inside an HTTP 200 response.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(ApiError::not_found)
}

pub fn is_retryable(error: &anyhow::Error) -> bool {
    if error.is::<IncompleteEntryLookup>() {
        return true;
    }
    if error.is::<UnconfirmedEntryWrite>() {
        return !is_auth_rejection(error);
    }
    if let Some(api) = error.downcast_ref::<ApiError>() {
        if api.from_json && (is_not_found(error) || is_auth_rejection(error)) {
            return false;
        }
        return !api.from_json
            || matches!(
                api.kind,
                ApiErrorKind::Partial | ApiErrorKind::PartialEntryLookup
            )
            || api.status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || api.status == reqwest::StatusCode::REQUEST_TIMEOUT
            || (api.status == reqwest::StatusCode::FORBIDDEN
                && api.kind == ApiErrorKind::Disabled)
            || api.status.is_server_error();
    }
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_body() || e.is_request())
}

pub(crate) fn is_entry_retry_error(error: &anyhow::Error) -> bool {
    error.is::<IncompleteEntryLookup>()
        || error.root_cause().is::<UnconfirmedEntryWrite>()
        || error.downcast_ref::<ApiError>().is_some_and(|api| {
            matches!(
                api.kind,
                ApiErrorKind::EntryServer
                    | ApiErrorKind::PartialEntryLookup
                    | ApiErrorKind::PartialEntryWrite
            )
        })
}

pub fn media_not_found(e: &anyhow::Error) -> bool {
    !e.is::<UnconfirmedEntryWrite>() && is_not_found(e)
}

/// Only a definitive token rejection may clear the session.
pub fn is_auth_rejection(e: &anyhow::Error) -> bool {
    let Some(api) = e.downcast_ref::<ApiError>() else {
        return false;
    };
    api.from_json
        && (api.status == reqwest::StatusCode::UNAUTHORIZED || api.kind == ApiErrorKind::Rejected)
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
        "{AUTHORIZE}?client_id={cid}&response_type=code&redirect_uri={redir}&state={state}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(redirect_uri),
        state = urlencoding::encode(state),
    )
}

const WAIT_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2>Waiting for AniList.</h2><p>Finish signing in using the AniList tab opened by Kurisu.</p></body>";

const OK_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#3ba55d'>Authorization received.</h2><p>Close this tab and return to Kurisu to see the connection result.</p><script>history.replaceState(null,'',location.pathname);</script></body>";
const ERR_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#e74c3c'>Authorization failed.</h2><p>Return to Kurisu for details.</p></body>";

fn clip_for_log(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

fn percent_decode(s: &str) -> String {
    crate::recognize::percent_decode(&s.replace('+', " "))
}

pub type OAuthReceiver = oneshot::Receiver<Result<String, String>>;

/// Pass zero to bind an available callback port.
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
            let (outcome_tx, mut outcome_rx) =
                tokio::sync::mpsc::channel::<Result<String, String>>(1);
            let slots = Arc::new(tokio::sync::Semaphore::new(16));
            let expected = Arc::new(expected);
            loop {
                tokio::select! {
                    _ = tx.closed() => return,
                    outcome = outcome_rx.recv() => {
                        if let Some(outcome) = outcome {
                            let _ = tx.send(outcome);
                        }
                        return;
                    }
                    acc = listener.accept() => {
                        let Ok((mut sock, _)) = acc else {
                            continue;
                        };
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            continue;
                        };
                        let expected = expected.clone();
                        let outcome_tx = outcome_tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            use tokio::io::{AsyncReadExt, AsyncWriteExt};
                            // Bound each connection so browser preconnects cannot stall login.
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
                            if n == 0 {
                                return;
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
                                if (param("error").is_some() || param("code").is_some())
                                    && param("state").as_deref() != Some(expected.as_str())
                                {
                                    (None, ERR_HTML)
                                } else if let Some(err) = param("error") {
                                    let msg = param("error_description").unwrap_or(err);
                                    log::warn!("OAuth callback: AniList denied access: {}", clip_for_log(&msg));
                                    (Some(Err(format!("AniList authorization failed: {}", clip_for_log(&msg)))), ERR_HTML)
                                } else if let Some(code) = param("code") {
                                    if code.trim().is_empty() {
                                        (Some(Err("AniList returned an empty authorization code.".into())), ERR_HTML)
                                    } else {
                                        (Some(Ok(code)), OK_HTML)
                                    }
                                } else {
                                    (None, WAIT_HTML)
                                };
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                            let _ = sock.shutdown().await;
                            if let Some(outcome) = outcome {
                                let _ = outcome_tx.try_send(outcome);
                            }
                        });
                    }
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
    #[tokio::test]
    async fn extreme_rate_reset_headers_do_not_panic_or_wait() {
        let api = super::AniList::new();
        for reset in [i64::MIN, i64::MAX] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
            headers.insert("x-ratelimit-reset", reset.to_string().parse().unwrap());
            api.note_rate_headers(&headers);
            tokio::time::timeout(std::time::Duration::from_millis(100), api.wait_for_budget())
                .await
                .expect("out-of-window reset must not delay requests");
        }
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn fixture_graphql_accepts_only_literal_loopback_http_urls() {
        for value in [
            "http://127.0.0.1:39418/graphql",
            "http://127.42.0.1:39418/",
            "http://[::1]:39418/graphql",
        ] {
            assert!(super::fixture_graphql_endpoint(value).is_ok(), "{value}");
        }
        for value in [
            "https://127.0.0.1:39418/graphql",
            "http://localhost:39418/graphql",
            "http://2130706433:39418/graphql",
            "http://0.0.0.0:39418/graphql",
            "http://192.0.2.4:39418/graphql",
            "http://[::2]:39418/graphql",
            "http://user@127.0.0.1:39418/graphql",
            "http://127.0.0.1:39418/graphql#fragment",
            "http://127.0.0.1:39418/graphql?token=synthetic",
            "http://127.0.0.1:invalid/graphql",
        ] {
            assert!(super::fixture_graphql_endpoint(value).is_err(), "{value}");
        }
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn invalid_fixture_setting_fails_closed() {
        use std::env::VarError;

        assert_eq!(
            super::fixture_graphql_from_env_value(Err(VarError::NotPresent)),
            None
        );
        assert_eq!(
            super::fixture_graphql_from_env_value(Ok("http://127.0.0.1:39418/graphql".into())),
            Some("http://127.0.0.1:39418/graphql".into())
        );
        assert!(std::panic::catch_unwind(|| {
            super::fixture_graphql_from_env_value(Ok("http://example.com/graphql".into()))
        })
        .is_err());
    }

    #[test]
    fn decoded_list_reuses_response_strings() {
        #[derive(serde::Deserialize)]
        struct Page {
            media: Vec<super::AniMedia>,
        }
        let mut media = (1..=1000).map(super::mock_media).collect::<Vec<_>>();
        for row in &mut media {
            row["title"]["english"] = serde_json::json!("Example");
            row["description"] = serde_json::json!("Synthetic description. ".repeat(100));
        }
        let body = serde_json::json!({"data":{"media":media}});
        let buffers = body["data"]["media"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let title = row["title"]["english"].as_str().unwrap();
                let description = row["description"].as_str().unwrap();
                (
                    title.as_ptr(),
                    description.as_ptr(),
                    title.len() + description.len(),
                )
            })
            .collect::<Vec<_>>();
        let decoded: Page = super::AniList::decode_data(body, reqwest::StatusCode::OK).unwrap();
        assert_eq!(decoded.media.len(), 1000);
        let mut reused = 0;
        for (index, (row, (title_ptr, description_ptr, bytes))) in
            decoded.media.into_iter().zip(buffers).enumerate()
        {
            let row = super::Media::from(row);
            assert_eq!(row.id, index as i64 + 1);
            assert_eq!(row.title_english.as_deref(), Some("Example"));
            let description = row.description.as_ref().unwrap();
            assert_eq!(description, &"Synthetic description. ".repeat(100));
            if row.title_english.as_ref().unwrap().as_ptr() == title_ptr
                && description.as_ptr() == description_ptr
            {
                reused += bytes;
            }
        }
        println!("decoded 1000 media rows, reused {reused} title and description bytes");
        assert_eq!(reused, 2_307_000);
    }

    #[test]
    fn decoded_data_preserves_missing_null_and_malformed_errors() {
        for body in [
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::Value::Null,
        ] {
            let error =
                super::AniList::decode_data::<serde_json::Value>(body, reqwest::StatusCode::OK)
                    .unwrap_err();
            assert_eq!(
                error.to_string(),
                "AniList (200 OK): AniList: no data field"
            );
            assert!(super::is_retryable(&error));
            assert!(!super::is_auth_rejection(&error));
            assert!(!super::media_not_found(&error));
        }
        let explicit_null: Option<String> =
            super::AniList::decode_data(serde_json::json!({"data":null}), reqwest::StatusCode::OK)
                .unwrap();
        assert_eq!(explicit_null, None);
        for data in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!("wrong type"),
        ] {
            let error = super::AniList::decode_data::<Vec<super::AniMedia>>(
                serde_json::json!({"data":data}),
                reqwest::StatusCode::OK,
            )
            .err()
            .unwrap();
            assert!(error
                .to_string()
                .contains("AniList returned unusable data:"));
            assert!(super::is_retryable(&error));
            assert!(!super::is_auth_rejection(&error));
            assert!(!super::media_not_found(&error));
        }
    }

    #[test]
    fn oauth_authorization_requests_a_code_and_preserves_parameters() {
        let url = reqwest::Url::parse(&super::authorize_url(
            "45266",
            "http://127.0.0.1:39417/",
            "state+with&reserved=characters",
        ))
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "45266");
        assert_eq!(query["redirect_uri"], "http://127.0.0.1:39417/");
        assert_eq!(query["state"], "state+with&reserved=characters");
        assert!(!query.contains_key("client_secret"));
    }

    #[test]
    fn percent_decode_handles_escapes_plus_and_junk() {
        assert_eq!(super::percent_decode("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(super::percent_decode("a%20b+c"), "a b c");
        assert_eq!(super::percent_decode("%2B+%252B"), "+ %2B");
        assert_eq!(super::percent_decode("%E3%81%82%FF"), "あ�");
        assert_eq!(super::percent_decode("%41%6eiList"), "AniList");
        assert_eq!(super::percent_decode("100%"), "100%");
        assert_eq!(super::percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn is_not_found_matches_status_and_exact_message_only() {
        let real_404: anyhow::Error =
            super::ApiError::new(reqwest::StatusCode::NOT_FOUND, true, "whatever".into()).into();
        assert!(super::is_not_found(&real_404));
        let gql_not_found: anyhow::Error =
            super::ApiError::new(reqwest::StatusCode::OK, true, "Not Found".into()).into();
        assert!(super::is_not_found(&gql_not_found));
        let quoted_id: anyhow::Error = super::ApiError::new(
            reqwest::StatusCode::BAD_REQUEST,
            true,
            "validation failed for entry 40413".into(),
        )
        .into();
        assert!(!super::is_not_found(&quoted_id));
        let near_miss: anyhow::Error =
            super::ApiError::new(reqwest::StatusCode::OK, true, "Not Found.".into()).into();
        assert!(!super::is_not_found(&near_miss));
        let plain = anyhow::anyhow!("AniList (404): boom");
        assert!(!super::is_not_found(&plain));
    }

    #[test]
    fn is_auth_rejection_matches_invalid_token_and_auth_statuses_only() {
        let invalid_token: anyhow::Error = super::ApiError::new(
            reqwest::StatusCode::BAD_REQUEST,
            true,
            "Invalid Token".into(),
        )
        .into();
        assert!(super::is_auth_rejection(&invalid_token));
        let unauthorized: anyhow::Error =
            super::ApiError::new(reqwest::StatusCode::UNAUTHORIZED, true, "anything".into()).into();
        assert!(super::is_auth_rejection(&unauthorized));
        let validation: anyhow::Error = super::ApiError::new(
            reqwest::StatusCode::BAD_REQUEST,
            true,
            "validation failed".into(),
        )
        .into();
        assert!(!super::is_auth_rejection(&validation));
        let transport = anyhow::anyhow!("dns error");
        assert!(!super::is_auth_rejection(&transport));
    }

    #[test]
    fn forbidden_with_a_non_json_body_is_not_an_auth_rejection() {
        let html_403: anyhow::Error = super::ApiError::new(
            reqwest::StatusCode::FORBIDDEN,
            false,
            "error decoding response body: expected value at line 1 column 1".into(),
        )
        .into();
        assert!(!super::is_auth_rejection(&html_403));
        let json_403: anyhow::Error =
            super::ApiError::new(reqwest::StatusCode::FORBIDDEN, true, "Forbidden".into()).into();
        assert!(!super::is_auth_rejection(&json_403));
    }

    #[test]
    fn oauth_callback_survives_probes_and_accepts_verified_code() {
        let (state, port, mut rx) =
            super::start_callback_server_on(0).expect("bind callback listener");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio rt");
        rt.block_on(async move {
            let base = format!("http://127.0.0.1:{port}");
            let http = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap();
            for path in ["/", "/?error=access_denied", "/?code=bad&state=nope"] {
                let response = http.get(format!("{base}{path}")).send().await.unwrap();
                assert!(response.status().is_success());
                response.text().await.unwrap();
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(50), &mut rx)
                        .await
                        .is_err(),
                    "unverified callback {path} must leave authorization pending"
                );
            }
            let r = http
                .get(format!("{base}/?code=good-code&state={state}"))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
            assert_eq!(r.headers()["cache-control"], "no-store");
            let page = r.text().await.unwrap();
            assert!(page.contains("Authorization received."));
            assert!(!page.contains("good-code"));
            let code = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("listener must still be alive after the probes")
                .unwrap();
            assert_eq!(code.unwrap(), "good-code");
        });
    }
}

#[cfg(test)]
pub(crate) fn mock_media(id: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id, "idMal": null,
        "title": {"romaji": null, "english": null, "native": null},
        "coverImage": null, "episodes": null, "format": null, "status": null,
        "averageScore": null, "season": null, "seasonYear": null,
        "description": null, "nextAiringEpisode": null,
        "bannerImage": null, "genres": null, "duration": null, "source": null,
        "studios": null, "relations": {"edges": []},
        "characters": {"edges": []}, "staff": {"edges": []}
    })
}

#[cfg(test)]
pub(crate) async fn mock_api(
    responses: Vec<(u16, serde_json::Value)>,
) -> (
    AniList,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    mock_api_gated(responses, None).await
}

#[cfg(test)]
pub(crate) async fn mock_api_gated(
    responses: Vec<(u16, serde_json::Value)>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
) -> (
    AniList,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    mock_api_mapped(responses, gate, |_, _| {}).await
}

#[cfg(test)]
pub(crate) async fn mock_api_mapped(
    responses: Vec<(u16, serde_json::Value)>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
    map_response: impl Fn(&serde_json::Value, &mut serde_json::Value) + Send + 'static,
) -> (
    AniList,
    tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (requests, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut responses = responses.into_iter();
        loop {
            let response = responses.next();
            let (mut socket, _) = tokio::select! {
                _ = requests.closed(), if response.is_none() => return,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let (status, mut body) = response.unwrap_or_else(|| {
                (400, serde_json::json!({"errors":[{"message":"Unexpected AniList test request","status":400}]}))
            });
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
            map_response(&request, &mut body);
            let _ = requests.send(request);
            if let Some(gate) = &gate {
                tokio::select! {
                    permit = gate.acquire() => permit.unwrap().forget(),
                    _ = requests.closed() => return,
                }
            }
            let (content_type, body) = match body {
                serde_json::Value::String(raw) => ("text/html", raw),
                body => ("application/json", body.to_string()),
            };
            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
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
    async fn invalid_account_ids_do_not_send_identity_queries() {
        let (api, mut requests) = mock_api(vec![]).await;
        for id in [-1, 0, i64::from(i32::MAX) + 1, i64::MAX] {
            assert!(api.user_list(id, None).await.is_err());
            assert!(api.user_statistics(id).await.is_err());
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn oauth_exchange_sends_only_the_code_to_the_service() {
        let (api, mut requests) = mock_api(vec![(200, json!({"access_token":"new-token"}))]).await;
        assert_eq!(
            api.exchange_code("one-use-code").await.unwrap(),
            "new-token"
        );
        assert_eq!(
            requests.recv().await.unwrap(),
            json!({"code":"one-use-code"})
        );
    }

    #[tokio::test]
    async fn oauth_exchange_rejects_errors_and_empty_tokens_without_echoing_responses() {
        for (status, body) in [
            (
                400,
                json!({"error":"invalid_grant", "message":"private-code"}),
            ),
            (502, json!({"message":"private-secret"})),
            (200, json!({"access_token":""})),
            (200, json!({"access_token":null})),
            (200, json!({"access_token":" ".repeat(10)})),
            (200, json!({"access_token":"x".repeat(16385)})),
        ] {
            let (api, _requests) = mock_api(vec![(status, body)]).await;
            let error = api
                .exchange_code("one-use-code")
                .await
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private-"));
            assert!(!error.contains("one-use-code"));
        }
    }

    #[tokio::test]
    async fn an_empty_oauth_code_finishes_with_an_error() {
        let (state, port, receiver) = start_callback_server_on(0).unwrap();
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}/?code=&state={state}"))
            .send()
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("empty authorization code"));
    }

    #[tokio::test]
    async fn unexpected_requests_stay_visible_to_negative_assertions() {
        for scripted in [false, true] {
            let response = json!({"data":{"Viewer":{"id":7,"name":"Tester"}}});
            let (api, mut requests) = mock_api(if scripted {
                vec![(200, response)]
            } else {
                vec![]
            })
            .await;
            if scripted {
                api.viewer().await.unwrap();
                requests.recv().await.unwrap();
            }
            assert!(api.viewer().await.is_err());
            let request = requests
                .try_recv()
                .expect("unexpected HTTP requests must remain observable");
            assert!(request["query"].as_str().unwrap().contains("Viewer"));
        }
    }

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
            "Media": {"id":1,"mediaListEntry": {"id": 22,"mediaId":1, "status": "CURRENT", "progress": 4, "score": 8.5, "repeat": 2}},
            "MediaList": {"id": 999, "status": "COMPLETED", "progress": 12, "score": 10.0, "repeat": 0}
        }}))]).await;
        let entry = api.entry_by_media_id(1, None).await.unwrap().unwrap();
        assert_eq!(entry.id, 22);
        let request = requests.recv().await.unwrap();
        let query = request["query"].as_str().unwrap();
        assert!(query.contains("mediaListEntry"));
        assert!(!query.contains("MediaList(mediaId:"));
    }

    #[tokio::test]
    async fn unknown_score_format_drops_only_unscaled_nonzero_scores() {
        for format in [None, Some("POINT_5")] {
            for score in [json!(null), json!(0), json!(4)] {
                let entry = json!({"id":22,"mediaId":1,"status":"CURRENT","progress":4,"score":score,"repeat":0});
                let (api, mut requests) = mock_api(vec![
                (200, json!({"data":{"Media":{"id":1,"mediaListEntry":entry}}})),
                (200, json!({"data":{"SaveMediaListEntry":entry}})),
                (200, json!({"data":{"MediaListCollection":{"user":{"id":7},"lists":[{"entries":[entry]}],"hasNextChunk":false}}})),
            ])
            .await;
                let expected = Some(score.as_f64().unwrap_or(0.0))
                    .filter(|score| format.is_some() || *score == 0.0);
                assert_eq!(
                    api.entry_by_media_id(1, format)
                        .await
                        .unwrap()
                        .unwrap()
                        .score,
                    expected
                );
                assert_eq!(
                    api.save_entry(1, Some(22), None, Some(4), None, None, format, false)
                        .await
                        .unwrap()
                        .0
                        .score,
                    expected
                );
                assert_eq!(api.user_list(7, format).await.unwrap()[0].score, expected);
                for _ in 0..3 {
                    let request = requests.recv().await.unwrap();
                    assert_eq!(request["variables"]["scoreFormat"], json!(format));
                    assert!(request["variables"].get("score").is_none());
                }
                assert!(requests.try_recv().is_err());
            }
        }
    }

    #[tokio::test]
    async fn an_unlisted_anime_is_not_a_missing_anime() {
        let (api, _) = mock_api(vec![(
            200,
            json!({"data": {"Media": {"id":1,"mediaListEntry": null}}}),
        )])
        .await;
        assert!(api.entry_by_media_id(1, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn incomplete_list_payloads_are_rejected() {
        for lists in [
            json!(null),
            json!([{"entries": null}]),
            json!([{"entries": [null]}]),
            json!([{"entries": [{"id":22,"mediaId":1,"status":"CURRENT","progress":0,"score":0,"repeat":0,"media":mock_media(2)}]}]),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data": {"MediaListCollection": {"user":{"id":7},"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            assert!(api.user_list(7, None).await.is_err());
        }
    }

    #[tokio::test]
    async fn a_missing_later_chunk_is_not_a_complete_list() {
        let (api, _) = mock_api(vec![
            (200, json!({"data": {"MediaListCollection": {"user":{"id":7},"lists": [{"entries": [{"id": 22, "mediaId": 1, "status": "CURRENT", "progress": 0, "repeat": 0, "score": 0}]}], "hasNextChunk": true}}})),
            (200, json!({"data": {"MediaListCollection": {"user":{"id":7},"lists": [], "hasNextChunk": false}}})),
        ]).await;
        assert!(api.user_list(7, None).await.is_err());
    }

    #[tokio::test]
    async fn list_chunks_allow_entries_shared_by_groups_in_the_same_chunk() {
        let entry =
            json!({"id":22,"mediaId":1,"status":"CURRENT","progress":0,"repeat":0,"score":0});
        let mut custom_entry = entry.clone();
        custom_entry["media"] = mock_media(1);
        let mut next_entry = entry.clone();
        next_entry["id"] = json!(23);
        next_entry["mediaId"] = json!(2);
        let (api, _) = mock_api(vec![
            (
                200,
                json!({"data":{"MediaListCollection":{
                    "user":{"id":7},"hasNextChunk":true,
                    "lists":[{"entries":[entry]},{"entries":[custom_entry]}]
                }}}),
            ),
            (
                200,
                json!({"data":{"MediaListCollection":{
                    "user":{"id":7},"hasNextChunk":false,"lists":[{"entries":[next_entry]}]
                }}}),
            ),
        ])
        .await;
        let entries = api.user_list(7, None).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.media_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(entries[0].media.as_ref().unwrap().id, 1);
    }

    #[tokio::test]
    async fn inconsistent_list_group_entries_are_rejected_in_either_order() {
        let first = json!({"id":22,"mediaId":1,"status":"CURRENT","progress":4,"repeat":0,"score":70,
            "updatedAt":10,"media":mock_media(1)});
        let mut different_media = mock_media(1);
        different_media["episodes"] = json!(24);
        for (field, value) in [
            ("id", json!(23)),
            ("mediaId", json!(2)),
            ("status", json!("PAUSED")),
            ("progress", json!(3)),
            ("repeat", json!(1)),
            ("score", json!(80)),
            ("updatedAt", json!(11)),
            ("media", different_media),
        ] {
            let mut other = first.clone();
            if field == "mediaId" {
                other["media"] = mock_media(2);
            }
            other[field] = value;
            for entries in [[first.clone(), other.clone()], [other, first.clone()]] {
                let lists = entries.map(|entry| json!({"entries":[entry]}));
                let (api, _) = mock_api(vec![(
                    200,
                    json!({"data":{"MediaListCollection":{
                        "user":{"id":7},"hasNextChunk":false,"lists":lists
                    }}}),
                )])
                .await;
                assert!(
                    api.user_list(7, Some("POINT_100")).await.is_err(),
                    "{field}"
                );
            }
        }
    }

    #[tokio::test]
    async fn null_media_does_not_excuse_an_invalid_list_media_id() {
        let entry = json!({"id":22,"mediaId":0,"status":"CURRENT","progress":4,"repeat":0,"score":0,"media":null});
        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"MediaListCollection":{
                "user":{"id":7},"hasNextChunk":false,"lists":[{"entries":[entry]}]
            }}}),
        )])
        .await;
        assert!(api.user_list(7, None).await.is_err());
    }

    #[tokio::test]
    async fn complete_lists_allow_empty_arrays_and_null_media() {
        for (lists, count) in [
            (json!([]), 0),
            (json!([{"entries": []}]), 0),
            (
                json!([{"entries": [{"id": 22, "mediaId": 1, "status": "CURRENT", "progress": 0, "repeat": 0, "media": null, "score": null}]}]),
                1,
            ),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data": {"MediaListCollection": {"user":{"id":7},"lists": lists, "hasNextChunk": false}}}),
            )])
            .await;
            assert_eq!(api.user_list(7, None).await.unwrap().len(), count);
        }
    }
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn overlapping_credentials_preserve_message_based_classification() {
        for (token, status, message, auth, retry, missing) in [
            ("Token", 400, "Invalid Token", true, false, false),
            ("Found", 200, "Not Found", false, false, true),
            (
                "temporarily",
                403,
                "The AniList API has been temporarily disabled",
                false,
                true,
                false,
            ),
        ] {
            let (mut api, _) =
                mock_api(vec![(status, json!({"errors":[{"message":message}]}))]).await;
            api.set_token(Some(token.into()));
            let error = api.viewer().await.unwrap_err();
            assert!(!error.to_string().contains(token));
            assert!(!format!("{error:?}").contains(token));
            assert_eq!(is_auth_rejection(&error), auth);
            assert_eq!(is_retryable(&error), retry);
            assert_eq!(media_not_found(&error), missing);
        }
    }

    #[tokio::test]
    async fn reflected_credentials_are_redacted_without_changing_error_classification() {
        const TOKEN: &str = "synthetic-secret-canary";
        for (status, body, from_json, auth, retry) in [
            (
                200,
                json!({"errors":[{"status":401,"message":format!("Invalid Token {TOKEN} {TOKEN}")}]}),
                true,
                true,
                false,
            ),
            (
                503,
                json!({"message":format!("Gateway rejected {TOKEN}")}),
                false,
                false,
                true,
            ),
            (
                200,
                json!({"data":{"Viewer":{"id":TOKEN}}}),
                false,
                false,
                true,
            ),
        ] {
            let (mut api, _) = mock_api(vec![(status, body)]).await;
            api.set_token(Some(TOKEN.into()));
            let error = api.viewer().await.unwrap_err();
            assert!(!error.to_string().contains(TOKEN));
            assert!(error.to_string().contains("[redacted]"));
            assert_eq!(
                error.downcast_ref::<ApiError>().unwrap().from_json,
                from_json
            );
            assert_eq!(is_auth_rejection(&error), auth);
            assert_eq!(is_retryable(&error), retry);
        }
        let (mut api, _) = mock_api(vec![(
            400,
            json!({"errors":[{"status":400,"message":"Invalid Token"}]}),
        )])
        .await;
        api.set_token(Some(TOKEN.into()));
        let error = api.viewer().await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().message,
            "Invalid Token"
        );
    }

    #[tokio::test]
    async fn graphql_error_status_controls_retries_without_broadening_permanent_failures() {
        for (transport, reported, message, retry, auth, missing) in [
            (200, json!(429), "Too Many Requests.", true, false, false),
            (200, json!(503), "Service unavailable", true, false, false),
            (503, json!(404), "Not Found", true, false, false),
            (404, json!(503), "Not Found", true, false, false),
            (404, json!(429), "Too Many Requests.", true, false, false),
            (404, json!(401), "Unauthenticated", false, true, false),
            (200, json!(400), "validation", false, false, false),
            (200, json!(401), "Unauthenticated", false, true, false),
            (200, json!(404), "Anime unavailable", false, false, true),
            (404, json!(404), "Invalid Token", false, true, false),
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
            let error = api.entry_by_media_id(1, None).await.err().unwrap();
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
            let error = api.entry_by_media_id(1, None).await.err().unwrap();
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
            let error: anyhow::Error = ApiError::new(
                reqwest::StatusCode::from_u16(status).unwrap(),
                from_json,
                message.into(),
            )
            .into();
            assert_eq!(is_retryable(&error), expected, "{status}: {message}");
            if !from_json {
                assert!(!media_not_found(&error));
            }
        }
    }

    #[tokio::test]
    async fn unavailable_discovery_results_are_not_empty_results() {
        for field in ["media", "recommendations"] {
            for available in [None, Some(json!(null))] {
                let mut page = json!({"pageInfo":{"hasNextPage":false}});
                if let Some(value) = available {
                    page[field] = value;
                }
                let (api, _) = mock_api(vec![(200, json!({"data":{"Page":page}}))]).await;
                if field == "media" {
                    assert!(api.season_all("FALL", 2026).await.is_err());
                } else {
                    assert!(api.recommendations(1).await.is_err());
                }
            }
        }
        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":false},"media":[]
            }}}),
        )])
        .await;
        assert!(api.season_all("FALL", 2026).await.unwrap().is_empty());
        let (api, _) = mock_api(vec![(200, json!({"data":{"Page":{"recommendations":[]}}}))]).await;
        assert!(api.recommendations(1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn season_keeps_unique_entries_across_overlapping_pages() {
        let page = |ids: &[i64], more| {
            let media = ids.iter().copied().map(mock_media).collect::<Vec<_>>();
            (
                200,
                json!({"data":{"Page":{"pageInfo":{"hasNextPage":more},"media":media}}}),
            )
        };
        let (api, mut requests) = mock_api(vec![
            page(&[10, 11], true),
            page(&[11, 12], true),
            page(&[11, 12], true),
            page(&[12, 13], false),
        ])
        .await;
        let items = api.season_all("FALL", 2026).await.unwrap();
        assert_eq!(
            items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [10, 11, 12, 13]
        );
        for expected in 1..=4 {
            assert_eq!(requests.try_recv().unwrap()["variables"]["page"], expected);
        }
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn season_rejects_pages_without_usable_media_during_a_walk() {
        let page = |media, more| {
            (
                200,
                json!({"data":{"Page":{"pageInfo":{"hasNextPage":more},"media":media}}}),
            )
        };
        for pages in [
            vec![page(json!([]), true)],
            vec![
                page(json!([mock_media(10)]), true),
                page(json!([null]), false),
            ],
        ] {
            let (api, _) = mock_api(pages).await;
            assert!(api
                .season_all("FALL", 2026)
                .await
                .unwrap_err()
                .to_string()
                .contains("stopped returning entries"));
        }
    }

    #[tokio::test]
    async fn season_requires_the_walk_to_finish_at_or_before_page_twenty() {
        for complete in [true, false] {
            let pages = (1..=20)
                .map(|id| {
                    let more = id < 20 || !complete;
                    (200, json!({"data":{"Page":{"pageInfo":{"hasNextPage":more},"media":[mock_media(1),mock_media(id)]}}}))
                })
                .collect();
            let (api, mut requests) = mock_api(pages).await;
            let result = api.season_all("FALL", 2026).await;
            if complete {
                let ids = result
                    .unwrap()
                    .iter()
                    .map(|item| item.id)
                    .collect::<Vec<_>>();
                assert_eq!(ids, (1..=20).collect::<Vec<_>>());
            } else {
                assert!(result.unwrap_err().to_string().contains("20 pages"));
            }
            for page in 1..=20 {
                assert_eq!(requests.try_recv().unwrap()["variables"]["page"], page);
            }
            assert!(requests.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn unavailable_calendar_is_not_an_empty_calendar() {
        for page in [
            json!({"pageInfo":{"hasNextPage":false}}),
            json!({"pageInfo":{"hasNextPage":false},"airingSchedules":null}),
        ] {
            let (api, _) = mock_api(vec![(200, json!({"data":{"Page":page}}))]).await;
            assert!(api.airing_schedule(100, 200).await.is_err());
        }
        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":false},"airingSchedules":[]
            }}}),
        )])
        .await;
        assert!(api.airing_schedule(100, 200).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn calendar_rejects_a_missing_first_page_before_a_valid_later_page() {
        let (api, mut requests) = mock_api(vec![
            (
                200,
                json!({"data":{"Page":{
                    "pageInfo":{"currentPage":1,"hasNextPage":true},"airingSchedules":[]
                }}}),
            ),
            (
                200,
                json!({"data":{"Page":{
                    "pageInfo":{"hasNextPage":false},
                    "airingSchedules":[{"airingAt":180,"episode":2,"media":mock_media(26)}]
                }}}),
            ),
        ])
        .await;
        assert!(api.airing_schedule(100, 200).await.is_err());
        assert_eq!(requests.recv().await.unwrap()["variables"]["page"], 1);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn calendar_requires_new_rows_and_matching_page_numbers() {
        let row = |id, airing_at, adult| {
            let mut media = mock_media(id);
            media["isAdult"] = json!(adult);
            json!({"airingAt":airing_at,"episode":1,"media":media})
        };
        let first = row(1, 110, false);
        let second = row(2, 120, true);
        let third = row(3, 130, false);
        let response = |info, rows| {
            (
                200,
                json!({"data":{"Page":{"pageInfo":info,"airingSchedules":rows}}}),
            )
        };
        let first_response = |rows| response(json!({"currentPage":1,"hasNextPage":true}), rows);
        for (info, rows) in [
            (
                json!({"currentPage":2,"hasNextPage":false}),
                vec![second.clone(), first.clone()],
            ),
            (
                json!({"currentPage":1,"hasNextPage":false}),
                vec![third.clone()],
            ),
            (
                json!({"currentPage":null,"hasNextPage":false}),
                vec![third.clone()],
            ),
            (json!({"hasNextPage":false}), vec![third.clone()]),
        ] {
            let (api, mut requests) = mock_api(vec![
                first_response(vec![first.clone(), second.clone()]),
                response(info.clone(), rows),
            ])
            .await;
            assert!(api.airing_schedule(100, 200).await.is_err(), "{info}");
            for page in 1..=2 {
                assert_eq!(requests.try_recv().unwrap()["variables"]["page"], page);
            }
        }
        for info in [
            json!({"currentPage":2,"hasNextPage":false}),
            json!({"currentPage":null,"hasNextPage":false}),
            json!({"hasNextPage":true}),
        ] {
            let (api, mut requests) =
                mock_api(vec![response(info.clone(), vec![first.clone()])]).await;
            assert!(api.airing_schedule(100, 200).await.is_err(), "{info}");
            assert_eq!(requests.try_recv().unwrap()["variables"]["page"], 1);
            assert!(requests.try_recv().is_err());
        }
        let (api, _) = mock_api(vec![
            first_response(vec![first.clone()]),
            response(
                json!({"currentPage":2,"hasNextPage":true}),
                vec![first.clone(), second],
            ),
            response(
                json!({"currentPage":3,"hasNextPage":false}),
                vec![first, third],
            ),
        ])
        .await;
        let items = api.airing_schedule(100, 200).await.unwrap();
        assert_eq!(
            items.iter().map(|item| item.media.id).collect::<Vec<_>>(),
            [1, 3]
        );
    }

    #[tokio::test]
    async fn search_returns_page_boundaries_from_anilist() {
        let (api, mut requests) = mock_api(vec![(
            200,
            json!({"data":{"Page":{"pageInfo":{"hasNextPage":true},"media":[mock_media(26)]}}}),
        )])
        .await;
        let page = api.search_page("Show", 2, 25).await.unwrap();
        assert_eq!(page.page, 2);
        assert!(page.has_next_page);
        assert_eq!(page.items[0].id, 26);
        assert_eq!(requests.recv().await.unwrap()["variables"]["page"], 2);
    }

    #[tokio::test]
    async fn search_rejects_unusable_pages_and_accepts_empty_results() {
        for (media, more, valid) in [
            (json!([null]), false, false),
            (json!([mock_media(0)]), true, false),
            (json!([]), true, false),
            (json!([]), false, true),
            (json!([null, mock_media(1)]), true, true),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data":{"Page":{"pageInfo":{"hasNextPage":more},"media":media}}}),
            )])
            .await;
            assert_eq!(api.search_page("Show", 1, 25).await.is_ok(), valid);
        }
    }

    #[tokio::test]
    async fn notification_pages_require_the_collection_and_page_boundary() {
        for page in [
            json!({"pageInfo":{"currentPage":2,"hasNextPage":false}}),
            json!({"pageInfo":{"currentPage":2,"hasNextPage":false},"notifications":null}),
            json!({"notifications":[]}),
            json!({"notifications":[],"pageInfo":{"currentPage":2}}),
            json!({"notifications":[],"pageInfo":{"currentPage":2,"hasNextPage":null}}),
            json!({"notifications":[],"pageInfo":{"hasNextPage":false}}),
            json!({"notifications":[],"pageInfo":{"currentPage":null,"hasNextPage":false}}),
            json!({"notifications":[],"pageInfo":{"currentPage":2,"hasNextPage":true}}),
            json!({"notifications":[null,{}],"pageInfo":{"currentPage":2,"hasNextPage":false}}),
            json!({"notifications":[{"id":1,"type":"AIRING"},{"id":2}],"pageInfo":{"currentPage":2,"hasNextPage":false}}),
            json!({"notifications":[{"id":0,"type":"AIRING"}],"pageInfo":{"currentPage":2,"hasNextPage":false}}),
            json!({"notifications":[{"id":1,"type":"AIRING","createdAt":i64::MIN}],"pageInfo":{"currentPage":2,"hasNextPage":false}}),
        ] {
            let (api, _) = mock_api(vec![(
                200,
                json!({"data":{"Viewer":{"unreadNotificationCount":4},"Page":page}}),
            )])
            .await;
            assert!(api.notifications_page(2).await.is_err(), "{page}");
        }
        let response = |count| {
            (
                200,
                json!({"data":{"Viewer":{"unreadNotificationCount":count},"Page":{
                    "pageInfo":{"currentPage":2,"hasNextPage":false},"notifications":[]
                }}}),
            )
        };
        let (api, _) = mock_api(vec![response(-1), response(0)]).await;
        assert!(api.notifications_page(2).await.is_err());
        let page = api.notifications_page(2).await.unwrap();
        assert!(page.items.is_empty());
        assert!(!page.has_next_page);
        assert_eq!(page.unread_count, Some(0));
    }

    #[tokio::test]
    async fn notifications_keep_the_unread_count_and_comment_permalink() {
        let response = |current_page| {
            (
                200,
                json!({"data":{"Viewer":{"unreadNotificationCount":4},"Page":{"pageInfo":{"currentPage":current_page,"hasNextPage":true},"notifications":[{"id":8,"type":"THREAD_COMMENT_REPLY","commentId":22,"comment":{"siteUrl":"https://anilist.co/forum/thread/1/comment/22"}}]}}}),
            )
        };
        let (api, mut requests) = mock_api(vec![response(1), response(2)]).await;
        assert!(api
            .notifications_page(2)
            .await
            .unwrap_err()
            .to_string()
            .contains("wrong notifications page"));
        let page = api.notifications_page(2).await.unwrap();
        assert_eq!(page.page, 2);
        assert_eq!(page.unread_count, Some(4));
        assert!(page.has_next_page);
        assert_eq!(
            page.items[0].comment_url.as_deref(),
            Some("https://anilist.co/forum/thread/1/comment/22")
        );
        for _ in 0..2 {
            let request = requests.recv().await.unwrap();
            assert_eq!(request["variables"]["page"], 2);
            let query = request["query"].as_str().unwrap();
            assert!(query.contains("currentPage"));
            assert!(query.contains("resetNotificationCount: false"));
        }
    }

    #[tokio::test]
    async fn statistics_null_is_unavailable_instead_of_an_empty_account() {
        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"User":{"id":7,"statistics":null}}}),
        )])
        .await;
        assert!(api
            .user_statistics(7)
            .await
            .unwrap_err()
            .to_string()
            .contains("unavailable"));
    }

    #[tokio::test]
    async fn incomplete_mark_read_replies_do_not_report_success() {
        for reply in [
            json!({"data":null}),
            json!({"data":{}}),
            json!({"data":{"Page":null}}),
            json!({"data":{"Page":{}}}),
            json!({"data":{"Page":{"notifications":null}}}),
            json!({"data":{"Page":{"notifications":[123]}}}),
        ] {
            let (api, _) = mock_api(vec![(200, reply)]).await;
            let error = api
                .mark_notifications_read()
                .await
                .expect_err("missing acknowledgement");
            assert!(is_retryable(&error));
            assert!(!is_auth_rejection(&error));
        }
        for notifications in [json!([]), json!([{}]), json!([{"id":8}])] {
            let (api, mut requests) = mock_api(vec![(
                200,
                json!({"data":{"Page":{"notifications":notifications}}}),
            )])
            .await;
            api.mark_notifications_read().await.unwrap();
            assert!(requests.recv().await.unwrap()["query"]
                .as_str()
                .unwrap()
                .contains("resetNotificationCount: true"));
        }
    }

    #[tokio::test]
    async fn empty_statistics_still_allow_zero_counts_and_null_buckets() {
        for null_members in [false, true] {
            let mut anime = json!({
                "count":0,"episodesWatched":0,"minutesWatched":0,
                "meanScore":0.0,"standardDeviation":0.0,
                "scores":null,"statuses":null,"formats":null,"genres":null,"releaseYears":null
            });
            if null_members {
                anime["statuses"] = json!([{"status":null,"count":0}]);
                anime["formats"] = json!([{"format":null,"count":0}]);
                anime["genres"] = json!([{"genre":null,"count":0,"minutesWatched":null}]);
                anime["releaseYears"] = json!([{"releaseYear":null,"count":0}]);
            }
            let (api, _) = mock_api(vec![(
                200,
                json!({"data":{"User":{"id":7,"statistics":{"anime":anime}}}}),
            )])
            .await;
            let found = api.user_statistics(7).await.unwrap();
            assert_eq!(found.count, 0);
            assert_eq!(found.episodes_watched, 0);
            assert!(found.scores.is_empty());
            assert!(found.statuses.is_empty());
            assert!(found.formats.is_empty());
            assert!(found.genres.is_empty());
            assert!(found.release_years.is_empty());
        }
    }

    #[tokio::test]
    async fn incomplete_media_cannot_pass_through_other_query_paths() {
        let mut media = mock_media(1);
        media.as_object_mut().unwrap().remove("episodes");
        for path in [
            "search",
            "season",
            "recommendations",
            "calendar",
            "list",
            "relations",
        ] {
            let data = match path {
                "search" | "season" => json!({"Page":{
                    "pageInfo":{"hasNextPage":false},"media":[media]
                }}),
                "recommendations" => {
                    json!({"Page":{"recommendations":[{"mediaRecommendation":media}]}})
                }
                "calendar" => {
                    let mut airing = media.clone();
                    airing["isAdult"] = json!(false);
                    json!({"Page":{"pageInfo":{"hasNextPage":false},"airingSchedules":[{
                        "airingAt":1000,"episode":1,"media":airing
                    }]}})
                }
                "list" => {
                    json!({"MediaListCollection":{"user":{"id":7},"hasNextChunk":false,"lists":[{"entries":[{
                        "id":22,"mediaId":1,"status":"CURRENT","progress":3,"score":0,
                        "repeat":0,"updatedAt":null,"media":media
                    }]}]}})
                }
                "relations" => {
                    let mut parent = mock_media(2);
                    let mut relation = media.clone();
                    relation["type"] = json!("ANIME");
                    parent["relations"] =
                        json!({"edges":[{"relationType":"SEQUEL","node":relation}]});
                    json!({"Media":parent})
                }
                _ => unreachable!(),
            };
            let (api, _) = mock_api(vec![(200, json!({"data":data}))]).await;
            let result = match path {
                "search" => api.search_page("Show", 1, 25).await.map(|_| ()),
                "season" => api.season_all("WINTER", 2026).await.map(|_| ()),
                "recommendations" => api.recommendations(1).await.map(|_| ()),
                "calendar" => api.airing_schedule(1, 2000).await.map(|_| ()),
                "list" => api.user_list(7, None).await.map(|_| ()),
                "relations" => api.media_detail(2).await.map(|_| ()),
                _ => unreachable!(),
            };
            let error = result.expect_err(path);
            assert!(is_retryable(&error), "{path}");
            assert!(!is_auth_rejection(&error), "{path}");
        }
    }

    #[tokio::test]
    async fn missing_nested_media_fields_do_not_become_explicit_nulls() {
        for (parent, field) in [
            ("title", "romaji"),
            ("title", "english"),
            ("title", "native"),
            ("coverImage", "medium"),
            ("coverImage", "large"),
            ("nextAiringEpisode", "episode"),
            ("nextAiringEpisode", "airingAt"),
            ("studios", "nodes"),
        ] {
            let mut media = mock_media(1);
            media["coverImage"] = json!({"medium":null,"large":null});
            media["nextAiringEpisode"] = json!({"episode":1,"airingAt":1000});
            media["studios"] = json!({"nodes":[]});
            media[parent].as_object_mut().unwrap().remove(field);
            let (api, _) = mock_api(vec![(200, json!({"data":{"Media":media}}))]).await;
            let error = api.media_detail(1).await.expect_err(field);
            assert!(is_retryable(&error), "missing {parent}.{field}");
        }
    }

    #[tokio::test]
    async fn queried_media_must_match_the_requested_identity() {
        for details in [false, true] {
            let (api, _) = mock_api(vec![(200, json!({"data":{"Media":mock_media(2)}}))]).await;
            let result = if details {
                api.media_detail(1).await.map(|detail| detail.media)
            } else {
                api.media_by_id(1).await
            };
            let error = result.expect_err("a different anime must not satisfy this request");
            assert!(is_retryable(&error));
            assert!(!media_not_found(&error));
            assert!(!is_auth_rejection(&error));
        }
    }

    #[tokio::test]
    async fn summary_queries_require_selected_fields_but_allow_explicit_nulls() {
        for field in [
            "id",
            "idMal",
            "title",
            "coverImage",
            "episodes",
            "format",
            "status",
            "averageScore",
            "season",
            "seasonYear",
            "description",
            "nextAiringEpisode",
        ] {
            let mut media = mock_media(1);
            media.as_object_mut().unwrap().remove(field);
            let (api, _) = mock_api(vec![(200, json!({"data":{"Media":media}}))]).await;
            let error = api.media_by_id(1).await.expect_err(field);
            assert!(is_retryable(&error), "missing {field}");
        }
        let mut summary = mock_media(1);
        for field in [
            "bannerImage",
            "genres",
            "duration",
            "source",
            "studios",
            "relations",
            "characters",
            "staff",
        ] {
            summary.as_object_mut().unwrap().remove(field);
        }
        let (api, _) = mock_api(vec![(200, json!({"data":{"Media":summary}}))]).await;
        let media = api.media_by_id(1).await.unwrap();
        assert_eq!(media.id, 1);
        assert_eq!(media.episodes, None);
        assert_eq!(media.cover_medium, None);
    }

    #[tokio::test]
    async fn details_distinguish_missing_sections_from_empty_sections() {
        let mut media = mock_media(1);
        media["relations"] = json!(null);
        media["staff"] = json!(null);
        let (api, _) = mock_api(vec![(200, json!({"data":{"Media":media}}))]).await;
        let details = api.media_detail(1).await.unwrap();
        assert_eq!(details.unavailable_sections, vec!["relations", "staff"]);
    }

    #[tokio::test]
    async fn explicit_null_metadata_is_a_valid_empty_entry() {
        let empty = json!({"id":22,"mediaId":1,"notes":null,"startedAt":null,"completedAt":null,"customLists":null});
        let (api, _) = mock_api(vec![
            (200, json!({"data":{"Media":{"mediaListEntry":empty}}})),
            (200, json!({"data":{"SaveMediaListEntry":empty}})),
        ])
        .await;
        let (entry_id, fetched) = api.entry_details_with_id(1).await.unwrap().unwrap();
        assert_eq!(entry_id, 22);
        for details in [
            fetched,
            api.save_entry_details(
                1,
                22,
                Some(""),
                Some(&FuzzyDate::default()),
                Some(&FuzzyDate::default()),
                Some(&[]),
            )
            .await
            .unwrap(),
        ] {
            assert!(details.notes.is_empty());
            assert!(details.started_at.is_none());
            assert!(details.completed_at.is_none());
            assert!(details.custom_lists.is_empty());
        }
    }

    #[tokio::test]
    async fn details_save_requires_confirmation_of_each_requested_field() {
        let reply = json!({"data":{"SaveMediaListEntry":{
            "id":22,"mediaId":1,"notes":"Saved note",
            "startedAt":{"year":2024,"month":null,"day":null},"completedAt":null,
            "customLists":{"Favorites":true,"Rewatch":true,"Other":false}
        }}});
        let (api, _) = mock_api(vec![(200, reply); 5]).await;
        let year = FuzzyDate {
            year: Some(2024),
            ..FuzzyDate::default()
        };
        let other_year = FuzzyDate {
            year: Some(2025),
            ..FuzzyDate::default()
        };
        let lists = ["Rewatch".into(), "Favorites".into()];
        for (notes, started, completed, lists) in [
            (Some("New note"), None, None, None),
            (None, Some(&other_year), None, None),
            (None, None, Some(&year), None),
            (None, None, None, Some(&[][..])),
        ] {
            let result = api
                .save_entry_details(1, 22, notes, started, completed, lists)
                .await;
            assert!(result.unwrap_err().is::<UnconfirmedEntryDetailsWrite>());
        }
        api.save_entry_details(1, 22, Some("Saved note"), Some(&year), None, Some(&lists))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn notes_mutation_omits_untouched_dates_lists_and_progress() {
        let (api, mut requests) = mock_api(vec![(200, json!({"data":{"SaveMediaListEntry":{"id":22,"mediaId":1,"notes":"New note","startedAt":{"year":2024,"month":null,"day":null},"completedAt":null,"customLists":{"Favorites":true,"Other":false}}}}))]).await;
        let result = api
            .save_entry_details(1, 22, Some("New note"), None, None, None)
            .await
            .unwrap();
        assert_eq!(result.custom_lists, vec!["Favorites"]);
        assert_eq!(result.started_at.unwrap().year, Some(2024));
        let request = requests.recv().await.unwrap();
        assert_eq!(request["variables"], json!({"id":22,"notes":"New note"}));
        assert!(!request["query"].as_str().unwrap().contains("progress:"));
    }

    #[tokio::test]
    async fn non_positive_ids_are_filtered_across_discovery_detail_and_notifications() {
        let mut zero_media = mock_media(0);
        zero_media["id"] = json!(0);
        let mut negative_media = mock_media(-5);
        negative_media["id"] = json!(-5);
        let valid_media = mock_media(42);

        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":false},
                "media":[zero_media.clone(), negative_media.clone()]
            }}}),
        )])
        .await;
        assert!(api.season_all("SPRING", 2026).await.is_err());

        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"Page":{
                "pageInfo":{"hasNextPage":false},
                "media":[zero_media.clone(), valid_media.clone(), negative_media.clone()]
            }}}),
        )])
        .await;
        let season = api.season_all("SPRING", 2026).await.unwrap();
        assert_eq!(season.iter().map(|m| m.id).collect::<Vec<_>>(), vec![42]);

        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{"Page":{"recommendations":[
                {"mediaRecommendation": zero_media.clone()},
                {"mediaRecommendation": negative_media.clone()},
                {"mediaRecommendation": valid_media.clone()}
            ]}}}),
        )])
        .await;
        let recs = api.recommendations(1).await.unwrap();
        assert_eq!(recs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![42]);

        let detail_with_relation = |kind: &str, rel_id: i64| {
            let mut rel_media = mock_media(rel_id);
            rel_media["id"] = json!(rel_id);
            rel_media["type"] = json!(kind);
            let mut root = mock_media(7);
            root["bannerImage"] = json!(null);
            root["genres"] = json!([]);
            root["duration"] = json!(24);
            root["source"] = json!("ORIGINAL");
            root["studios"] = json!({"nodes":[]});
            root["relations"] = json!({"edges":[{"relationType":"SEQUEL","node":rel_media}]});
            root["characters"] = json!({"edges":[]});
            root["staff"] = json!({"edges":[]});
            json!({"data":{"Media":root}})
        };
        let (api, _) = mock_api(vec![
            (200, detail_with_relation("ANIME", 0)),
            (200, detail_with_relation("ANIME", -5)),
            (200, detail_with_relation("MANGA", 0)),
        ])
        .await;
        let bad_zero = api.media_detail(7).await.unwrap();
        assert!(bad_zero.relations.is_empty());
        assert!(bad_zero
            .unavailable_sections
            .iter()
            .any(|s| s == "relations"));
        let bad_neg = api.media_detail(7).await.unwrap();
        assert!(bad_neg.relations.is_empty());
        assert!(bad_neg
            .unavailable_sections
            .iter()
            .any(|s| s == "relations"));
        let manga_zero = api.media_detail(7).await.unwrap();
        assert!(manga_zero.relations.is_empty());
        assert!(!manga_zero
            .unavailable_sections
            .iter()
            .any(|s| s == "relations"));

        let (api, _) = mock_api(vec![(
            200,
            json!({"data":{
                "Viewer":{"unreadNotificationCount":1},
                "Page":{
                    "pageInfo":{"currentPage":1,"hasNextPage":false},
                    "notifications":[{
                        "__typename":"ThreadCommentReplyNotification",
                        "id":99,
                        "type":"THREAD_COMMENT_REPLY",
                        "createdAt":1700000000,
                        "context":"replied",
                        "media":{"id":0,"title":{"userPreferred":"Zero"},"coverImage":{"medium":null}},
                        "activityId":-5,
                        "thread":{"id":0,"title":"Thread"},
                        "commentId":-1,
                        "comment":{"siteUrl":null},
                        "user":{"name":"Alice","avatar":{"large":null}}
                    }]
                }
            }}),
        )])
        .await;
        let page = api.notifications_page(1).await.unwrap();
        assert_eq!(page.items.len(), 1);
        let item = &page.items[0];
        assert_eq!(item.id, 99);
        assert_eq!(item.media_id, None);
        assert_eq!(item.activity_id, None);
        assert_eq!(item.thread_id, None);
        assert_eq!(item.comment_id, None);
    }

    #[tokio::test]
    async fn oauth_exchange_rejects_oversized_content_length_and_streamed_bodies() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        async fn spawn_exchange_server(
            response: Vec<u8>,
        ) -> (AniList, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                let _ = reader.get_mut().write_all(&response).await;
                let _ = reader.get_mut().shutdown().await;
            });
            let mut api = AniList::new();
            api.endpoint = Some(format!("http://{addr}/exchange"));
            (api, handle)
        }

        let oversized_len =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 70000\r\nConnection: close\r\n\r\n"
                .to_vec();
        let (api, server) = spawn_exchange_server(oversized_len).await;
        let err = api.exchange_code("code-1").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "Kurisu's sign-in service returned an invalid response."
        );
        server.await.unwrap();

        let chunk = "a".repeat(20 * 1024);
        let chunk_frame = format!("{:x}\r\n{}\r\n", chunk.len(), chunk);
        let mut streamed =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                .to_vec();
        for _ in 0..4 {
            streamed.extend_from_slice(chunk_frame.as_bytes());
        }
        streamed.extend_from_slice(b"0\r\n\r\n");
        let (api, server) = spawn_exchange_server(streamed).await;
        let err = api.exchange_code("code-2").await.unwrap_err().to_string();
        assert_eq!(
            err,
            "Kurisu's sign-in service returned an invalid response."
        );
        server.await.unwrap();
    }
}
