use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::db::Db;

const FEEDS_KEY: &str = "rss_feeds";
const DEFAULT_FEEDS: &[&str] = &["https://nyaa.si/?page=rss&c=1_2&f=0"];
static FEEDS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[derive(Debug, Default, Clone)]
pub struct RawItem {
    pub title: String,
    pub link: String,
    pub guid: String,
    pub info_hash: Option<String>,
    pub size: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub category_id: Option<String>,
    pub category: Option<String>,
    pub trusted: Option<bool>,
    pub remake: Option<bool>,
    pub published: Option<i64>,
}

/// Use defaults only when unset. An explicitly empty list stays empty.
pub fn get_feeds(db: &Db) -> Vec<String> {
    match db.get_setting(FEEDS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("corrupt rss_feeds setting, starting from empty: {e}");
            Vec::new()
        }),
        None => DEFAULT_FEEDS.iter().map(|s| s.to_string()).collect(),
    }
}

fn save_feeds(db: &Db, feeds: &[String]) -> Result<()> {
    db.set_setting(FEEDS_KEY, &serde_json::to_string(feeds)?)
}

pub fn add_feed(db: &Db, url: &str) -> Result<Vec<String>> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow!("feed URL must be a valid http:// or https:// URL"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(anyhow!("feed URL must start with http:// or https://"));
    }
    // Allow plaintext HTTP only for a local feed reader.
    if parsed.scheme() == "http" {
        let host = parsed
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']');
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
        if !loopback {
            return Err(anyhow!(
                "plain http feeds are only allowed for addresses on this machine, use https"
            ));
        }
    }
    let url = parsed.as_str();
    let _guard = FEEDS_LOCK.lock();
    let mut feeds = get_feeds(db);
    if !feeds.iter().any(|f| f == url) {
        feeds.push(url.to_string());
        save_feeds(db, &feeds)?;
    }
    Ok(feeds)
}

pub fn remove_feed(db: &Db, url: &str) -> Result<Vec<String>> {
    let _guard = FEEDS_LOCK.lock();
    let mut feeds = get_feeds(db);
    feeds.retain(|f| f != url);
    save_feeds(db, &feeds)?;
    Ok(feeds)
}

const MAX_FEED_BYTES: u64 = 8 * 1024 * 1024;

pub struct FeedFetch {
    pub items: Vec<RawItem>,
    pub failures: Vec<FeedFailure>,
}

pub struct FeedFailure {
    pub url: String,
    pub error: String,
}

pub async fn fetch_all(feeds: &[String]) -> Result<FeedFetch> {
    let http = reqwest::Client::builder()
        .user_agent("Kurisu")
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    let mut tasks = Vec::with_capacity(feeds.len());
    for feed in feeds {
        let http = http.clone();
        let feed = feed.clone();
        tasks.push(tokio::spawn(async move {
            let fetched: Result<String> = async {
                let mut resp = http.get(&feed).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(anyhow!("{feed}: HTTP {status}"));
                }
                if resp.content_length().unwrap_or(0) > MAX_FEED_BYTES {
                    return Err(anyhow!(
                        "{feed}: response larger than {MAX_FEED_BYTES} bytes"
                    ));
                }
                let mut body: Vec<u8> = Vec::new();
                while let Some(chunk) = resp.chunk().await? {
                    body.extend_from_slice(&chunk);
                    // Enforce the cap even when Content-Length is missing or false.
                    if body.len() as u64 > MAX_FEED_BYTES {
                        return Err(anyhow!("{feed}: response exceeded {MAX_FEED_BYTES} bytes"));
                    }
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            }
            .await;
            (feed, fetched)
        }));
    }
    let mut results = Vec::with_capacity(tasks.len());
    for (i, task) in tasks.into_iter().enumerate() {
        match task.await {
            Ok(pair) => results.push(pair),
            Err(e) => results.push((feeds[i].clone(), Err(anyhow!("feed task failed: {e}")))),
        }
    }

    let mut out: Vec<RawItem> = Vec::new();
    let mut seen_guids = std::collections::HashSet::new();
    let mut failures: Vec<FeedFailure> = Vec::new();
    let mut first_err: Option<anyhow::Error> = None;
    let mut ok = 0usize;
    for (feed, fetched) in results {
        match fetched {
            Ok(xml) => {
                let parsed = parse_rss_checked(&xml);
                let items = parsed.items;
                // Distinguish an empty RSS feed from an error page returned with HTTP 200.
                let lower = xml.to_lowercase();
                if items.is_empty() && !lower.contains("<rss") && !lower.contains("<channel") {
                    let msg = format!("{feed}: response was not an RSS feed");
                    log::warn!("{msg}");
                    failures.push(FeedFailure {
                        url: feed,
                        error: msg,
                    });
                    continue;
                }
                if let Some(err) = parsed.error {
                    let msg = format!(
                        "{feed}: feed broke off mid parse, kept {} items recovered before the error: {err}",
                        items.len()
                    );
                    log::warn!("{msg}");
                    failures.push(FeedFailure {
                        url: feed.clone(),
                        error: msg,
                    });
                }
                ok += 1;
                for mut item in items {
                    // Scope GUIDs by feed so deduplication and seen state cannot cross feeds.
                    item.guid = format!("{feed}\u{1}{}", item.guid);
                    if seen_guids.insert(item.guid.clone()) {
                        out.push(item);
                    }
                }
            }
            Err(e) => {
                log::warn!("RSS fetch failed: {e}");
                failures.push(FeedFailure {
                    url: feed,
                    error: e.to_string(),
                });
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if ok == 0 {
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(FeedFetch {
        items: out,
        failures,
    })
}

fn search_url(query: &str, category: &str, filter: &str) -> Result<String> {
    if !matches!(category, "1_0" | "1_1" | "1_2" | "1_3" | "1_4") {
        return Err(anyhow!("unsupported torrent category"));
    }
    if !matches!(filter, "0" | "1" | "2") {
        return Err(anyhow!("unsupported torrent filter"));
    }
    let mut url = reqwest::Url::parse("https://nyaa.si/")?;
    url.query_pairs_mut()
        .append_pair("page", "rss")
        .append_pair("c", category)
        .append_pair("f", filter)
        .append_pair("q", query);
    Ok(url.into())
}

pub async fn search(query: &str, category: &str, filter: &str) -> Result<Vec<RawItem>> {
    let url = search_url(query, category, filter)?;
    let fetched = fetch_all(&[url]).await?;
    if let Some(failure) = fetched.failures.first() {
        return Err(anyhow!("Could not complete the search: {}", failure.error));
    }
    Ok(fetched.items)
}

pub(crate) struct ParsedFeed {
    pub items: Vec<RawItem>,
    /// Keep recovered items, but report a truncated document.
    pub error: Option<String>,
}

#[cfg(test)]
pub fn parse_rss(xml: &str) -> Vec<RawItem> {
    parse_rss_checked(xml).items
}

pub fn parse_rss_checked(xml: &str) -> ParsedFeed {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut item: Option<RawItem> = None;
    // Only the closing tag at this depth may commit the field.
    let mut field: Option<String> = None;
    let mut field_depth = 0usize;
    let mut depth = 0usize;
    let mut document_depth = 0usize;
    let mut buf = String::new();
    let mut error = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                document_depth += 1;
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "item" {
                    item = Some(RawItem::default());
                    field = None;
                    depth = 0;
                } else if item.is_some() {
                    if field.is_none() {
                        field = Some(name);
                        field_depth = depth;
                        buf.clear();
                    }
                    depth += 1;
                }
            }
            Ok(Event::Text(t)) => {
                if item.is_some() && field.is_some() {
                    buf.push_str(&decode_text(&t));
                }
            }
            Ok(Event::CData(t)) => {
                if item.is_some() && field.is_some() {
                    buf.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::End(e)) => {
                document_depth = document_depth.saturating_sub(1);
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "item" {
                    if let Some(it) = item.take() {
                        if !it.title.is_empty() && !it.link.is_empty() {
                            out.push(finish_item(it));
                        }
                    }
                    field = None;
                } else if item.is_some() {
                    depth = depth.saturating_sub(1);
                    if depth == field_depth {
                        if let (Some(it), Some(f)) = (item.as_mut(), field.take()) {
                            let v = buf.trim();
                            match f.as_str() {
                                "title" => it.title = v.to_string(),
                                "link" => it.link = v.to_string(),
                                "guid" => it.guid = v.to_string(),
                                "pubDate" | "dc:date" => {
                                    it.published = parse_date(v).or(it.published)
                                }
                                "nyaa:infoHash" => it.info_hash = Some(v.to_string()),
                                "nyaa:size" => it.size = Some(v.to_string()),
                                "nyaa:seeders" => it.seeders = v.parse().ok(),
                                "nyaa:leechers" => it.leechers = v.parse().ok(),
                                "nyaa:categoryId" => {
                                    it.category_id = (!v.is_empty()).then(|| v.to_string())
                                }
                                "nyaa:category" => {
                                    it.category = (!v.is_empty()).then(|| v.to_string())
                                }
                                "nyaa:trusted" => it.trusted = parse_flag(v),
                                "nyaa:remake" => it.remake = parse_flag(v),
                                _ => {}
                            }
                            buf.clear();
                        }
                    }
                }
            }
            Ok(Event::Eof) => {
                if document_depth != 0 {
                    error = Some("unexpected end of RSS document".to_string());
                }
                break;
            }
            Err(e) => {
                log::warn!("RSS parse stopped early: {e}");
                error = Some(e.to_string());
                break;
            }
            _ => {}
        }
    }
    ParsedFeed { items: out, error }
}

fn parse_flag(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Preserve unknown entities instead of discarding the entire text node.
fn decode_text(t: &quick_xml::events::BytesText) -> String {
    t.unescape_with(|name| {
        Some(match name {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "nbsp" => "\u{a0}",
            "mdash" => "—",
            "ndash" => "–",
            "hellip" => "…",
            "copy" => "©",
            "middot" => "·",
            "laquo" => "«",
            "raquo" => "»",
            "lsquo" => "‘",
            "rsquo" => "’",
            "ldquo" => "“",
            "rdquo" => "”",
            _ => return None,
        })
    })
    .map(|c| c.into_owned())
    .unwrap_or_else(|_| String::from_utf8_lossy(t.as_ref()).into_owned())
}

/// Dates without a timezone use UTC.
fn parse_date(s: &str) -> Option<i64> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(d.timestamp());
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(d.and_utc().timestamp());
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp());
    }
    None
}

fn finish_item(mut it: RawItem) -> RawItem {
    if it.guid.is_empty() {
        it.guid = it.link.clone();
    }
    it
}

/// Validate the hash before embedding it so feed text cannot inject magnet parameters.
pub fn magnet_for(info_hash: &str, title: &str) -> String {
    let h = info_hash.trim();
    let valid = (h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()))
        || (h.len() == 32
            && h.chars()
                .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '2'..='7')));
    if valid {
        format!(
            "magnet:?xt=urn:btih:{h}&dn={}",
            crate::anilist::urlencoding::encode(title)
        )
    } else {
        log::warn!("feed item carried a malformed info hash: {h:?}");
        format!("magnet:?dn={}", crate::anilist::urlencoding::encode(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn truncated_xml_reports_recovered_items() {
        let first =
            "<rss><channel><item><title>First</title><link>https://example.com/1</link></item>";
        for suffix in ["<item><title>Second", "", "</channel>"] {
            let xml = format!("{first}{suffix}");
            let parsed = parse_rss_checked(&xml);
            assert_eq!(parsed.items.len(), 1);
            assert!(parsed.error.is_some());
            let fetched = fetch_full(serve_body(&xml)).await;
            assert_eq!(fetched.items.len(), 1);
            assert_eq!(fetched.failures.len(), 1);
        }
        for xml in [
            "<rss><channel/></rss>",
            "<rss/>",
            "<rss><channel></channel></rss>",
        ] {
            let parsed = parse_rss_checked(xml);
            assert!(parsed.items.is_empty());
            assert!(parsed.error.is_none());
        }
    }

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:nyaa="https://nyaa.si/xmlns/nyaa">
  <channel>
    <title>Nyaa - Home</title>
    <item>
      <title>[SubsPlease] Some Show - 05 (1080p) [ABC123].mkv</title>
      <link>https://nyaa.si/download/1000001.torrent</link>
      <guid isPermaLink="true">https://nyaa.si/view/1000001</guid>
      <pubDate>Mon, 20 Jul 2026 21:38:00 -0000</pubDate>
      <nyaa:seeders>123</nyaa:seeders>
      <nyaa:leechers>7</nyaa:leechers>
      <nyaa:infoHash>abcdef0123456789abcdef0123456789abcdef01</nyaa:infoHash>
      <nyaa:size>1.4 GiB</nyaa:size>
      <nyaa:categoryId>1_2</nyaa:categoryId>
      <nyaa:category>Anime - English-translated</nyaa:category>
      <nyaa:trusted>Yes</nyaa:trusted>
      <nyaa:remake>No</nyaa:remake>
    </item>
    <item>
      <title><![CDATA[[Group] R&D Show - 02 [720p]]]></title>
      <link>https://nyaa.si/download/1000002.torrent</link>
      <guid>https://nyaa.si/view/1000002</guid>
      <pubDate>not a date</pubDate>
      <nyaa:categoryId>1_3</nyaa:categoryId>
      <nyaa:category>Anime - Non-English-translated</nyaa:category>
      <nyaa:trusted>No</nyaa:trusted>
      <nyaa:remake>Yes</nyaa:remake>
    </item>
    <item>
      <title>Entity &amp; Escapes - 09</title>
      <link>https://nyaa.si/download/1000003.torrent</link>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_nyaa_items() {
        let items = parse_rss(SAMPLE);
        assert_eq!(items.len(), 3);
        let a = &items[0];
        assert_eq!(a.title, "[SubsPlease] Some Show - 05 (1080p) [ABC123].mkv");
        assert_eq!(a.link, "https://nyaa.si/download/1000001.torrent");
        assert_eq!(a.guid, "https://nyaa.si/view/1000001");
        assert_eq!(a.seeders, Some(123));
        assert_eq!(a.leechers, Some(7));
        assert_eq!(a.size.as_deref(), Some("1.4 GiB"));
        assert_eq!(a.category_id.as_deref(), Some("1_2"));
        assert_eq!(a.category.as_deref(), Some("Anime - English-translated"));
        assert_eq!(a.trusted, Some(true));
        assert_eq!(a.remake, Some(false));
        assert!(a.published.is_some());
        assert_eq!(
            a.info_hash.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        let b = &items[1];
        assert_eq!(b.title, "[Group] R&D Show - 02 [720p]");
        assert_eq!(b.published, None);
        assert_eq!(b.category_id.as_deref(), Some("1_3"));
        assert_eq!(b.trusted, Some(false));
        assert_eq!(b.remake, Some(true));
        let c = &items[2];
        assert_eq!(c.title, "Entity & Escapes - 09");
        assert_eq!(c.guid, c.link);
        assert_eq!(c.category_id, None);
        assert_eq!(c.category, None);
        assert_eq!(c.trusted, None);
        assert_eq!(c.remake, None);
    }

    #[test]
    fn malformed_release_flags_stay_unknown() {
        let items = parse_rss(
            r#"<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item>
            <title>Unknown</title><link>https://example.com/1</link>
            <nyaa:trusted>maybe</nyaa:trusted><nyaa:remake>unknown</nyaa:remake>
            </item></channel></rss>"#,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].trusted, None);
        assert_eq!(items[0].remake, None);
    }

    #[test]
    fn search_filters_preserve_query_syntax() {
        let query = r#""進撃の巨人"|"Attack on Titan" -1080p &c=0_0"#;
        for category in ["1_0", "1_1", "1_2", "1_3", "1_4"] {
            for filter in ["0", "1", "2"] {
                let url = reqwest::Url::parse(&search_url(query, category, filter).unwrap()).unwrap();
                let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
                assert_eq!(url.host_str(), Some("nyaa.si"));
                assert_eq!(pairs.len(), 4);
                assert_eq!(pairs["page"], "rss");
                assert_eq!(pairs["c"], category);
                assert_eq!(pairs["f"], filter);
                assert_eq!(pairs["q"], query);
            }
        }
        assert!(search_url(query, "1_2&f=0", "2").is_err());
        assert!(search_url(query, "1_2", "unknown").is_err());
    }

    #[test]
    fn magnet_builds() {
        let hash = "abcdef0123456789abcdef0123456789abcdef01";
        let m = magnet_for(hash, "My Show - 05");
        assert_eq!(
            m,
            format!("magnet:?xt=urn:btih:{hash}&dn=My%20Show%20-%2005")
        );
        assert!(
            magnet_for("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", "t").starts_with("magnet:?xt=urn:btih:")
        );
    }

    #[test]
    fn magnet_rejects_malformed_info_hash() {
        let m = magnet_for("abc&tr=http://evil/announce", "My Show");
        assert!(
            !m.contains("urn:btih"),
            "malformed hash must not reach xt: {m}"
        );
        assert!(
            !m.contains("evil"),
            "no injected parameters may survive: {m}"
        );
        assert!(m.starts_with("magnet:?dn="));
    }

    struct Capture;
    static CAPTURE: Capture = Capture;
    static LOG_LINES: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());
    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            LOG_LINES.lock().push(record.args().to_string());
        }
        fn flush(&self) {}
    }
    fn install_logger() {
        let _ = log::set_logger(&CAPTURE);
        log::set_max_level(log::LevelFilter::Warn);
    }

    #[test]
    fn bad_entity_keeps_text() {
        let xml = r#"<rss version="2.0"><channel>
            <item>
              <title>Show&nbsp;Name &amp; Friends - 01</title>
              <link>https://example.com/1.torrent</link>
            </item>
            <item>
              <title>Odd &bogus; Entity - 02</title>
              <link>https://example.com/2.torrent</link>
            </item>
        </channel></rss>"#;
        let items = parse_rss(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Show\u{a0}Name & Friends - 01");
        assert_eq!(items[1].title, "Odd &bogus; Entity - 02");
    }

    #[test]
    fn nested_element_inside_field() {
        let xml = r#"<rss version="2.0"><channel>
            <item>
              <title>Foo <b>Bar</b> Baz - 03</title>
              <link>https://example.com/3.torrent</link>
            </item>
        </channel></rss>"#;
        let items = parse_rss(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Foo Bar Baz - 03");
        assert_eq!(items[0].link, "https://example.com/3.torrent");
    }

    #[test]
    fn truncated_feed_keeps_items_and_warns() {
        install_logger();
        let xml = r#"<rss version="2.0"><channel>
            <item>
              <title>Good - 01</title>
              <link>https://example.com/1.torrent</link>
            </item>
            <item>
              <title>Broken</oops>
              <link>https://example.com/2.torrent</link>
            </item>
        </channel></rss>"#;
        let items = parse_rss(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Good - 01");
        assert!(
            LOG_LINES.lock().iter().any(|l| l.contains("stopped early")),
            "expected a truncation warning, got: {:?}",
            LOG_LINES.lock()
        );
    }

    #[test]
    fn parses_iso_and_dc_dates() {
        let xml = r#"<rss version="2.0"><channel>
            <item>
              <title>RFC2822 - 01</title>
              <link>https://example.com/1.torrent</link>
              <pubDate>Mon, 20 Jul 2026 21:38:00 -0000</pubDate>
            </item>
            <item>
              <title>ISO pubDate - 02</title>
              <link>https://example.com/2.torrent</link>
              <pubDate>2026-07-20T21:38:00+00:00</pubDate>
            </item>
            <item>
              <title>dc:date - 03</title>
              <link>https://example.com/3.torrent</link>
              <dc:date>2026-07-20T21:38:00Z</dc:date>
            </item>
            <item>
              <title>Naive - 04</title>
              <link>https://example.com/4.torrent</link>
              <pubDate>2026-07-20 21:38:00</pubDate>
            </item>
            <item>
              <title>Date only - 05</title>
              <link>https://example.com/5.torrent</link>
              <dc:date>2026-07-20</dc:date>
            </item>
        </channel></rss>"#;
        let items = parse_rss(xml);
        assert_eq!(items.len(), 5);
        let ts = items[0].published.expect("rfc2822 pubDate");
        assert_eq!(items[1].published, Some(ts));
        assert_eq!(items[2].published, Some(ts));
        assert_eq!(items[3].published, Some(ts));
        assert_eq!(items[4].published, Some(ts - (21 * 3600 + 38 * 60)));
    }

    async fn fetch_with_server(
        respond: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
    ) -> Result<Vec<RawItem>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            respond(&mut stream);
        });
        fetch_all(&[format!("http://127.0.0.1:{port}/rss")])
            .await
            .map(|f| f.items)
    }

    async fn fetch_full(
        respond: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
    ) -> FeedFetch {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            respond(&mut stream);
        });
        fetch_all(&[format!("http://127.0.0.1:{port}/rss")])
            .await
            .expect("one feed failure must not fail the whole fetch")
    }

    fn serve_body(body: &str) -> impl FnOnce(&mut std::net::TcpStream) + Send + 'static {
        let body = body.to_owned();
        move |stream| {
            use std::io::{Read, Write};
            let mut req = [0u8; 1024];
            let _ = stream.read(&mut req);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn empty_but_valid_feed_is_not_a_failure() {
        let f = fetch_full(serve_body(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<rss version=\"2.0\"><channel><title>Nyaa - Search</title></channel></rss>",
        ))
        .await;
        assert!(f.items.is_empty());
        assert!(
            f.failures.is_empty(),
            "an empty valid feed must not be reported failed"
        );
    }

    #[tokio::test]
    async fn non_rss_response_is_still_a_failure() {
        let f = fetch_full(serve_body("<html><body>moved</body></html>")).await;
        assert!(f.items.is_empty());
        assert_eq!(f.failures.len(), 1);
        assert!(f.failures[0].error.contains("not an RSS feed"));
    }

    #[tokio::test]
    async fn oversized_feed_is_rejected() {
        use std::io::{Read, Write};
        let err = fetch_with_server(move |stream| {
            let mut req = [0u8; 1024];
            let _ = stream.read(&mut req);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_FEED_BYTES + 1
            )
            .unwrap();
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("larger than"), "got: {err}");
    }

    #[tokio::test]
    async fn streamed_oversize_is_rejected() {
        use std::io::{Read, Write};
        let err = fetch_with_server(move |stream| {
            let mut req = [0u8; 1024];
            let _ = stream.read(&mut req);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let chunk = vec![b'a'; 4 * 1024 * 1024];
            for _ in 0..4 {
                let head = format!("{:x}\r\n", chunk.len());
                if stream.write_all(head.as_bytes()).is_err()
                    || stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    break;
                }
            }
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("exceeded"), "got: {err}");
    }

    #[test]
    fn plain_http_loopback_check_is_not_a_prefix_match() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        assert!(add_feed(&db, "http://127.0.0.1.evil.com/feed").is_err());
        assert!(add_feed(&db, "http://127.0.0.1@evil.com/feed").is_err());
        assert!(add_feed(&db, "http://localhost@evil.com/").is_err());
        assert!(add_feed(&db, "http://evil.com/").is_err());
        assert!(add_feed(&db, "ftp://127.0.0.1/rss").is_err());
        assert!(add_feed(&db, "not a url").is_err());
        assert!(add_feed(&db, "http://127.0.0.1:8123/rss").is_ok());
        assert!(add_feed(&db, "http://localhost/rss").is_ok());
        assert!(add_feed(&db, "http://[::1]/rss").is_ok());
        assert!(add_feed(&db, "https://nyaa.si/?page=rss&c=1_2&f=0").is_ok());
    }

    #[tokio::test]
    async fn a_feed_that_breaks_mid_document_is_reported() {
        let body = "<rss version=\"2.0\"><channel>
            <item>
              <title>Good - 01</title>
              <link>https://example.com/1.torrent</link>
            </item>
            <item>
              <title>Broken</oops>
              <link>https://example.com/2.torrent</link>
            </item>
        </channel></rss>";
        let f = fetch_full(serve_body(body)).await;
        assert_eq!(f.items.len(), 1, "the recovered item rides along");
        assert_eq!(f.failures.len(), 1);
        assert!(
            f.failures[0].error.contains("broke off mid parse"),
            "got: {}",
            f.failures[0].error
        );
    }

    #[tokio::test]
    async fn guid_collisions_across_feeds_stay_distinct() {
        let body = "<rss version=\"2.0\"><channel>
            <item>
              <title>Feed A - 01</title>
              <link>https://a.example/1.torrent</link>
              <guid>1</guid>
            </item>
        </channel></rss>";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                responder2(&mut stream, body);
            }
        });
        let feeds = vec![
            format!("http://127.0.0.1:{port}/a"),
            format!("http://127.0.0.1:{port}/b"),
        ];
        let f = fetch_all(&feeds).await.unwrap();
        assert_eq!(f.items.len(), 2, "both feeds' items survive");
        assert!(f.failures.is_empty());
        assert_ne!(f.items[0].guid, f.items[1].guid);
    }

    fn responder2(stream: &mut std::net::TcpStream, body: &str) {
        use std::io::{Read, Write};
        let mut req = [0u8; 1024];
        let _ = stream.read(&mut req);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }
}
