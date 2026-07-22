//! M6: torrent-feed awareness — Taiga's other half. Fetches the user's RSS
//! feeds (nyaa-style), parses the items, and hands them to the command layer,
//! which matches titles against the list with the shared recognizer.
//!
//! Feed list lives in the `settings` table as a JSON array (`rss_feeds`), same
//! pattern as the library folders. Seen-state is the `rss_seen` table.

use anyhow::{anyhow, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::db::Db;

const FEEDS_KEY: &str = "rss_feeds";
/// nyaa.si "Anime - English-translated", trusted-or-normal filter. The Taiga
/// default, editable on the Torrents page.
const DEFAULT_FEEDS: &[&str] = &["https://nyaa.si/?page=rss&c=1_2&f=0"];
/// Read-modify-write JSON in the settings table — serialize mutations.
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

/// Configured feeds; the built-in default only applies while the setting has
/// never been written, so an emptied list stays empty.
pub fn get_feeds(db: &Db) -> Vec<String> {
    match db.get_setting(FEEDS_KEY).ok().flatten() {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
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

/// Fetch every feed and merge the items (deduped by guid). One dead feed
/// doesn't fail the refresh; if ALL feeds fail, the first error is returned.
pub async fn fetch_all(feeds: &[String]) -> Result<Vec<RawItem>> {
    let http = reqwest::Client::builder()
        .user_agent("Kurisu")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut out: Vec<RawItem> = Vec::new();
    let mut seen_guids = std::collections::HashSet::new();
    let mut first_err: Option<anyhow::Error> = None;
    let mut ok = 0usize;
    for feed in feeds {
        let fetched: Result<String> = async {
            let resp = http.get(feed).send().await?;
            let status = resp.status();
            if !status.is_success() {
                return Err(anyhow!("{feed}: HTTP {status}"));
            }
            Ok(resp.text().await?)
        }
        .await;
        match fetched {
            Ok(xml) => {
                ok += 1;
                for item in parse_rss(&xml) {
                    if seen_guids.insert(item.guid.clone()) {
                        out.push(item);
                    }
                }
            }
            Err(e) => {
                log::warn!("RSS fetch failed: {e}");
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
    Ok(out)
}

/// Pull `<item>`s out of an RSS 2.0 document. Namespaced nyaa extras
/// (`nyaa:seeders`, `nyaa:infoHash`, …) are matched on their qualified name;
/// unknown elements are ignored, so non-nyaa feeds still yield the basics.
pub fn parse_rss(xml: &str) -> Vec<RawItem> {
    let mut reader = Reader::from_str(xml);
    let mut out = Vec::new();
    let mut item: Option<RawItem> = None;
    let mut field: Option<String> = None;
    let mut buf = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "item" {
                    item = Some(RawItem::default());
                } else if item.is_some() {
                    field = Some(name);
                    buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if item.is_some() && field.is_some() {
                    buf.push_str(&t.unescape().unwrap_or_default());
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
                } else if let (Some(it), Some(f)) = (item.as_mut(), field.take()) {
                    if f == name {
                        let v = buf.trim();
                        match f.as_str() {
                            "title" => it.title = v.to_string(),
                            "link" => it.link = v.to_string(),
                            "guid" => it.guid = v.to_string(),
                            "pubDate" => {
                                it.published = chrono::DateTime::parse_from_rfc2822(v)
                                    .ok()
                                    .map(|d| d.timestamp())
                            }
                            "nyaa:infoHash" => it.info_hash = Some(v.to_string()),
                            "nyaa:size" => it.size = Some(v.to_string()),
                            "nyaa:seeders" => it.seeders = v.parse().ok(),
                            "nyaa:leechers" => it.leechers = v.parse().ok(),
                            _ => {}
                        }
                    }
                    buf.clear();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Fill derived fields: guid falls back to the link.
fn finish_item(mut it: RawItem) -> RawItem {
    if it.guid.is_empty() {
        it.guid = it.link.clone();
    }
    it
}

/// magnet: URI from an info hash (clients resolve peers over DHT/trackers).
pub fn magnet_for(info_hash: &str, title: &str) -> String {
    format!(
        "magnet:?xt=urn:btih:{}&dn={}",
        info_hash,
        crate::anilist::urlencoding::encode(title)
    )
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
        // CDATA title with raw ampersand survives; bad pubDate → None.
        let b = &items[1];
        assert_eq!(b.title, "[Group] R&D Show - 02 [720p]");
        assert_eq!(b.published, None);
        // No guid → link is the identity. Escaped entity decoded.
        let c = &items[2];
        assert_eq!(c.title, "Entity & Escapes - 09");
        assert_eq!(c.guid, c.link);
    }

    #[test]
    fn magnet_builds() {
        let m = magnet_for("abc123", "My Show - 05");
        assert_eq!(m, "magnet:?xt=urn:btih:abc123&dn=My%20Show%20-%2005");
    }
}
