//! AniList v2 GraphQL client + desktop OAuth2 (implicit flow via a localhost
//! callback server). The browser gets the token in a URL fragment (#access_token),
//! which it never sends to a server, so the callback page runs a few lines of JS
//! that move the fragment into the query string and re-request — at which point our
//! listener captures it. Avoids shipping a client_secret.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::models::{ListEntry, ListStatus, Media, Notification, User};

const GRAPHQL: &str = "https://graphql.anilist.co";
const AUTHORIZE: &str = "https://anilist.co/api/v2/oauth/authorize";
/// Fixed port so the user registers ONE redirect_uri in their AniList client once.
pub const OAUTH_PORT: u16 = 39417;
pub const REDIRECT_URI: &str = "http://127.0.0.1:39417/callback";

/// `nextAiringEpisode { episode airingAt }` — shared by the search + list queries.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextAiring {
    episode: Option<i64>,
    airing_at: Option<i64>,
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
        let body: serde_json::Value = resp.json().await?;
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
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AniMedia {
            id: i64,
            id_mal: Option<i64>,
            title: Title,
            cover_image: Cover,
            episodes: Option<i64>,
            format: Option<String>,
            status: Option<String>,
            average_score: Option<i64>,
            season: Option<String>,
            season_year: Option<i64>,
            description: Option<String>,
            next_airing_episode: Option<NextAiring>,
        }
        #[derive(Deserialize)]
        struct Title {
            romaji: Option<String>,
            english: Option<String>,
            native: Option<String>,
        }
        #[derive(Deserialize)]
        struct Cover {
            medium: Option<String>,
            large: Option<String>,
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
        Ok(r.page.media.into_iter().map(|m| Media {
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
        })
        .collect())
    }

    /// Pull the full list (every status group) for a user and flatten to entries.
    pub async fn user_list(&self, user_name: &str) -> Result<Vec<ListEntry>> {
        #[derive(Deserialize)]
        struct R {
            #[serde(rename = "MediaListCollection")]
            collection: Collection,
        }
        #[derive(Deserialize)]
        struct Collection {
            lists: Option<Vec<AniList>>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AniList {
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
        // reuse a local copy of AniMedia (search's struct is private to that fn)
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct AniMedia {
            id: i64,
            id_mal: Option<i64>,
            title: Title,
            cover_image: Cover,
            episodes: Option<i64>,
            format: Option<String>,
            status: Option<String>,
            average_score: Option<i64>,
            season: Option<String>,
            season_year: Option<i64>,
            description: Option<String>,
            next_airing_episode: Option<NextAiring>,
        }
        #[derive(Deserialize)]
        struct Title {
            romaji: Option<String>,
            english: Option<String>,
            native: Option<String>,
        }
        #[derive(Deserialize)]
        struct Cover {
            medium: Option<String>,
            large: Option<String>,
        }
        let q = "query ($userName: String!) {
            MediaListCollection(userName: $userName, type: ANIME) {
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
        let r: R = self
            .gql(q, serde_json::json!({ "userName": user_name }))
            .await?;
        let mut out = Vec::new();
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
                    media: Some(Media {
                        id: e.media.id,
                        id_mal: e.media.id_mal,
                        title_romaji: e.media.title.romaji,
                        title_english: e.media.title.english,
                        title_native: e.media.title.native,
                        cover_medium: e.media.cover_image.medium,
                        cover_large: e.media.cover_image.large,
                        episodes: e.media.episodes,
                        format: e.media.format,
                        status: e.media.status,
                        average_score: e.media.average_score,
                        season: e.media.season,
                        season_year: e.media.season_year,
                        description: e.media.description,
                        next_airing_episode: e.media.next_airing_episode.as_ref().and_then(|n| n.episode),
                        next_airing_at: e.media.next_airing_episode.as_ref().and_then(|n| n.airing_at),
                    }),
                });
            }
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
        let q = "mutation ($mediaId: Int!, $status: MediaListStatus!, $progress: Int!, $score: Float) {
            SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, score: $score) { id }
        }";
        let r: R = self
            .gql(
                q,
                serde_json::json!({
                    "mediaId": media_id,
                    "status": status.as_str(),
                    "progress": progress,
                    "score": score,
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
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ThreadRef {
            id: i64,
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
            ... on AiringNotification { id type createdAt media { id } episode contexts }
            ... on FollowingNotification { id type createdAt context user { id name avatar { large } } }
            ... on ActivityLikeNotification { id type createdAt context activityId }
            ... on ActivityMentionNotification { id type createdAt context activityId }
            ... on ActivityReplyNotification { id type createdAt context activityId }
            ... on ActivityReplyLikeNotification { id type createdAt context activityId }
            ... on ActivityMessageNotification { id type createdAt context activityId }
            ... on ThreadCommentMentionNotification { id type createdAt context commentId thread { id } }
            ... on ThreadCommentReplyNotification { id type createdAt context commentId thread { id } }
            ... on ThreadLikeNotification { id type createdAt context thread { id } }
            ... on RelatedMediaAdditionNotification { id type createdAt context media { id } }
            ... on MediaDataChangeNotification { id type createdAt context media { id } reason }
            ... on MediaMergeNotification { id type createdAt context media { id } reason }
            ... on MediaDeletionNotification { id type createdAt context deletedMediaTitle }
        } } }";
        let r: R = self.gql(q, serde_json::json!({})).await?;
        Ok(r.page
            .notifications
            .unwrap_or_default()
            .into_iter()
            .map(|n| Notification {
                id: n.id,
                kind: n.kind,
                context: n
                    .context
                    .or_else(|| n.contexts.as_ref().filter(|v| !v.is_empty()).map(|v| v.join(" "))),
                created_at: n.created_at,
                media_id: n.media.map(|m| m.id),
                episode: n.episode,
                activity_id: n.activity_id,
                thread_id: n.thread.map(|t| t.id),
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

/// Build the authorize URL the user's browser should visit (response_type=token).
pub fn authorize_url(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "{AUTHORIZE}?client_id={cid}&response_type=token&redirect_uri={redir}",
        cid = urlencoding::encode(client_id),
        redir = urlencoding::encode(redirect_uri),
    )
}

/// The HTML shim served on the first callback hit. It moves the URL fragment
/// (which the server can't see) into a `/__capture__?<fragment>` request that the
/// server CAN read. On a bare probe (no fragment) it just shows a "connecting" page.
const SHIM_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><style>body{font-family:sans-serif;text-align:center;padding:3em;color:#9aa3b2;background:#0f1115;margin:0}h2{color:#3ba55d;font-weight:600}</style></head><body><h2>Connecting to Kurisu…</h2><p>You can close this tab once the app opens.</p><script>(function(){var h=location.hash.charCodeAt(0)===35?location.hash.slice(1):location.hash;if(h.indexOf('access_token=')!==-1){location.replace('/__capture__?'+h);}})();</script></body></html>";

const OK_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#3ba55d'>Connected to Kurisu.</h2><p>You can close this tab and return to the app.</p></body>";
const ERR_HTML: &str = "<!doctype html><body style='font-family:sans-serif;text-align:center;padding:3em;background:#0f1115;color:#9aa3b2'><h2 style='color:#e74c3c'>Authorization failed.</h2><p>Return to Kurisu for details.</p></body>";

/// Start a localhost HTTP listener that captures the access token AniList sends
/// back in the implicit flow. Yields Ok(token) on success or Err(msg) if AniList
/// reports an error / the user denies.
pub fn start_callback_server() -> Result<oneshot::Receiver<Result<String, String>>> {
    let (tx, rx) = oneshot::channel::<Result<String, String>>();
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
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
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
                        (key == k).then(|| val.to_string())
                    })
                };

                // 1) AniList denied → error comes in the query (?error=…).
                // 2) The shim re-requested with the fragment as a query
                //    (?access_token=…), or a code-style redirect slipped through.
                // 3) Otherwise (initial implicit redirect with the token still in the
                //    fragment, or a probe) → serve the shim, don't resolve yet.
                let (result, body): (Result<String, String>, &str) =
                    if let Some(err) = param("error") {
                        let msg = param("error_description").unwrap_or(err);
                        (Err(msg), ERR_HTML)
                    } else if let Some(token) = param("access_token").or_else(|| param("code")) {
                        (Ok(token), OK_HTML)
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
                let _ = tx.send(result);
                break;
            }
        });
    });
    Ok(rx)
}

// minimal URL-encode helper (avoids pulling urlencoding as a dep)
mod urlencoding {
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
