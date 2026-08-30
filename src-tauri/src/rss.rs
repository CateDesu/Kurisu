//! M6 torrent feed awareness, the other half of the Taiga style flow. Fetches
//! the user's RSS feeds, nyaa style, parses the items and hands them to the
//! command layer which matches titles against the list with the shared
//! recognizer.
//!
//! Feed list lives in the settings table as a JSON array under rss_feeds, same
//! pattern as the library folders. Seen state lives in the rss_seen table.

use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::db::Db;

const FEEDS_KEY: &str = "rss_feeds";
/// nyaa.si Anime English translated, trusted or normal filter. The Taiga
/// default, editable on the Torrents page.
const DEFAULT_FEEDS: &[&str] = &["https://nyaa.si/?page=rss&c=1_2&f=0"];
/// Read modify write JSON in the settings table. Serialize mutations.
static FEEDS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// One raw feed item, before any list matching.
#[derive(Debug, Default, Clone)]
pub struct RawItem {
    pub title: String,
    pub link: String,
    pub guid: String,
    pub info_hash: Option<String>,
    pub size: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub published: Option<i64>,
}

// ─────────────────────────── feed settings ───────────────────────────

/// Configured feeds. The built in default only applies while the setting has
/// never been written, so an emptied list stays empty.
pub fn get_feeds(db: &Db) -> Vec<String> {
    match db.get_setting(FEEDS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            // A corrupt row used to look exactly like "no feeds configured",
            // silently erasing the user's list. Log it so the real state is
            // at least discoverable.
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
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(anyhow!("feed URL must start with http:// or https://"));
    }
    // Plain http can be tampered with in transit. Feed items carry magnet
    // links, so a tampered feed hands the user attacker chosen torrents
    // dressed up as new episodes of shows they watch. A reader self hosted
    // on this machine is the one legitimate plain http case.
    if let Some(rest) = url.strip_prefix("http://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = if let Some(inner) = authority.strip_prefix('[') {
            inner.split(']').next().unwrap_or(inner)
        } else {
            authority.split(':').next().unwrap_or("")
        };
        let loopback = host == "localhost" || host.starts_with("127.") || host == "::1";
        if !loopback {
            return Err(anyhow!(
                "plain http feeds are only allowed for addresses on this machine, use https"
            ));
        }
    }
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

// ─────────────────────────── fetch + parse ───────────────────────────

/// Hard ceiling on one feed body. Real feeds are tens of kilobytes. This
/// bounds how much memory a hostile or pathological endpoint can make us
/// buffer. Feed URLs are arbitrary user input over plaintext http.
const MAX_FEED_BYTES: u64 = 8 * 1024 * 1024;

/// Items from every feed plus a per feed report. Lets the UI say WHICH feed
/// failed instead of showing a silently short list.
pub struct FeedFetch {
    pub items: Vec<RawItem>,
    /// One entry per configured feed, in the configured order.
    pub failures: Vec<FeedFailure>,
}

pub struct FeedFailure {
    pub url: String,
    pub error: String,
}

/// Fetch every feed and merge the items, deduped by guid. One dead feed doesn't
/// fail the refresh. If ALL feeds fail the first error is returned. Feeds are
/// fetched concurrently. Run serially with a 20s timeout each and a few dead
/// feeds stalled the Torrents page for a minute or more before showing anything.
pub async fn fetch_all(feeds: &[String]) -> Result<FeedFetch> {
    let http = reqwest::Client::builder()
        .user_agent("Kurisu")
        .timeout(std::time::Duration::from_secs(20))
        // Limit redirects rather than fully disabling them. HTTP to HTTPS
        // upgrades and domain canonicalisation are common, and blocking them
        // entirely breaks legitimate feeds. Capping at 3 hops bounds the attack
        // surface. The SSRF vector via redirect to loopback or metadata still
        // exists per hop, but feeds are user added not attacker controlled and
        // the response is parsed as RSS, never executed.
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;

    // tokio::spawn rather than a futures combinator. No new dependency, and the
    // handles are awaited in order so the merged list stays deterministic.
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
                    return Err(anyhow!("{feed}: response larger than {MAX_FEED_BYTES} bytes"));
                }
                let mut body: Vec<u8> = Vec::new();
                while let Some(chunk) = resp.chunk().await? {
                    body.extend_from_slice(&chunk);
                    // The header can lie or be absent. Cap the stream too.
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
            // A panic in one feed's task must not lose the others. Report
            // the feed as failed too. Dropping the pair hid it from the
            // failure list, and a run where every task died reported
            // success with an empty item list.
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
                let items = parse_rss(&xml);
                // A 200 that isn't RSS, like a captive portal, an error page
                // or a moved feed, yields zero items and used to be
                // indistinguishable from nothing new. An empty but valid
                // feed like a nyaa search with zero hits still has the rss
                // and channel elements, so only their absence is a failure.
                // Case insensitive. XML is case sensitive in general but
                // real world generators have emitted <RSS>.
                let lower = xml.to_lowercase();
                if items.is_empty() && !lower.contains("<rss") && !lower.contains("<channel") {
                    let msg = format!("{feed}: response was not an RSS feed");
                    log::warn!("{msg}");
                    failures.push(FeedFailure { url: feed, error: msg });
                    continue;
                }
                ok += 1;
                for item in items {
                    if seen_guids.insert(item.guid.clone()) {
                        out.push(item);
                    }
                }
            }
            Err(e) => {
                log::warn!("RSS fetch failed: {e}");
                failures.push(FeedFailure { url: feed, error: e.to_string() });
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
    Ok(FeedFetch { items: out, failures })
}

/// Pull <item>s out of an RSS 2.0 document. Namespaced nyaa extras like
/// nyaa:seeders and nyaa:infoHash are matched on their qualified name. Unknown
/// elements are ignored so non nyaa feeds still yield the basics.
pub fn parse_rss(xml: &str) -> Vec<RawItem> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut item: Option<RawItem> = None;
    // Field being accumulated plus the element depth at which it opened, so a
    // nested child element can't clobber it. Only the End that closes the
    // element which opened the field commits the buffer.
    let mut field: Option<String> = None;
    let mut field_depth = 0usize;
    let mut depth = 0usize;
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
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
                                _ => {}
                            }
                            buf.clear();
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            // Ill formed markup like a mismatched closing tag stops the parse
            // here. Keep what was accumulated but say so. Silent truncation
            // looks identical to a clean short feed otherwise.
            Err(e) => {
                log::warn!("RSS parse stopped early: {e}");
                break;
            }
            _ => {}
        }
    }
    out
}

/// Decode a text node without letting one bad entity blank the whole run.
/// quick-xml resolves only the five predefined XML entities, so map the common
/// HTML ones too. Fall back to the raw text for anything unknown instead of
/// dropping it. An emptied title or link silently deletes the item.
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

/// RSS 2.0 dates are RFC 2822, but plenty of feeds ship ISO 8601. RSS 1.0's
/// dc:date is ISO by spec, and some carry no timezone at all. Try the strict
/// parsers first, then a couple of naive layouts treated as UTC.
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

/// Fill derived fields: guid falls back to the link.
fn finish_item(mut it: RawItem) -> RawItem {
    if it.guid.is_empty() {
        it.guid = it.link.clone();
    }
    it
}

/// magnet URI from an info hash. Clients resolve peers over DHT or trackers.
/// The hash is feed text. Validate the shape, a 40 char hex or 32 char base32
/// value, so a crafted "hash" can not smuggle extra magnet parameters into
/// xt. A bad value degrades to a hashless magnet. The item's torrent page
/// link still works for those.
pub fn magnet_for(info_hash: &str, title: &str) -> String {
    let h = info_hash.trim();
    let valid = (h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()))
        || (h.len() == 32 && h.chars().all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '2'..='7')));
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
    </item>
    <item>
      <title><![CDATA[[Group] R&D Show - 02 [720p]]]></title>
      <link>https://nyaa.si/download/1000002.torrent</link>
      <guid>https://nyaa.si/view/1000002</guid>
      <pubDate>not a date</pubDate>
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
        assert!(a.published.is_some());
        assert_eq!(
            a.info_hash.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
        // CDATA title with raw ampersand survives. Bad pubDate maps to None.
        let b = &items[1];
        assert_eq!(b.title, "[Group] R&D Show - 02 [720p]");
        assert_eq!(b.published, None);
        // No guid means link is the identity. Escaped entity decoded.
        let c = &items[2];
        assert_eq!(c.title, "Entity & Escapes - 09");
        assert_eq!(c.guid, c.link);
    }

    #[test]
    fn magnet_builds() {
        let hash = "abcdef0123456789abcdef0123456789abcdef01";
        let m = magnet_for(hash, "My Show - 05");
        assert_eq!(m, format!("magnet:?xt=urn:btih:{hash}&dn=My%20Show%20-%2005"));
        // base32 hashes are valid too
        assert!(magnet_for("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567", "t").starts_with("magnet:?xt=urn:btih:"));
    }

    /// A crafted "hash" must not smuggle extra magnet parameters into xt.
    #[test]
    fn magnet_rejects_malformed_info_hash() {
        let m = magnet_for("abc&tr=http://evil/announce", "My Show");
        assert!(!m.contains("urn:btih"), "malformed hash must not reach xt: {m}");
        assert!(!m.contains("evil"), "no injected parameters may survive: {m}");
        assert!(m.starts_with("magnet:?dn="));
    }

    // Captures log output so the truncation warning can be asserted on.
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
        // One unknown entity must not blank the whole text node. An emptied
        // title or link silently deletes the item.
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
        // Unknown entity. The raw text is kept verbatim rather than dropped.
        assert_eq!(items[1].title, "Odd &bogus; Entity - 02");
    }

    #[test]
    fn nested_element_inside_field() {
        // A child element inside <title> must not clobber the field state or
        // drop the item. The text around the child is kept.
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
        // A mismatched closing tag mid feed. Keep the items parsed so far and
        // log a warning instead of silently truncating.
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

    /// Serve one canned HTTP response on a loopback port, then run fetch_all
    /// against it.
    async fn fetch_with_server(respond: impl FnOnce(&mut std::net::TcpStream) + Send + 'static) -> Result<Vec<RawItem>> {
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

    /// Same loopback server, but the whole FeedFetch so the per feed failure
    /// list can be asserted on.
    async fn fetch_full(respond: impl FnOnce(&mut std::net::TcpStream) + Send + 'static) -> FeedFetch {
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

    fn serve_body(body: &'static str) -> impl FnOnce(&mut std::net::TcpStream) + Send + 'static {
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

    /// A nyaa search with zero hits returns a well formed rss document with
    /// no items. That is a healthy feed, not a failure.
    #[tokio::test]
    async fn empty_but_valid_feed_is_not_a_failure() {
        let f = fetch_full(serve_body(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<rss version=\"2.0\"><channel><title>Nyaa - Search</title></channel></rss>",
        ))
        .await;
        assert!(f.items.is_empty());
        assert!(f.failures.is_empty(), "an empty valid feed must not be reported failed");
    }

    /// A 200 that is not RSS at all still counts as a feed failure.
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
        // No Content-Length. The running total cap must catch it instead.
        let err = fetch_with_server(move |stream| {
            let mut req = [0u8; 1024];
            let _ = stream.read(&mut req);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                .unwrap();
            let chunk = vec![b'a'; 4 * 1024 * 1024];
            for _ in 0..4 {
                let head = format!("{:x}\r\n", chunk.len());
                if stream.write_all(head.as_bytes()).is_err()
                    || stream.write_all(&chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    break; // client bailed once the cap tripped
                }
            }
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("exceeded"), "got: {err}");
    }
}
