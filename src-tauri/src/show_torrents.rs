use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use regex::Regex;

use crate::models::{ListEntry, Media, ShowTorrents, TorrentItem};
use crate::recognize::{self, Matcher};
use crate::rss::{self, RawItem};

static RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\w.])(?P<range>(?:(?P<season>s[0-9]{1,2})?e|ep(?:isode)?[.\s_-]*|#)?(?P<first>[0-9]{1,4})(?:v[0-9]+)?\s*[-~–]\s*(?:s[0-9]{1,2}e|ep(?:isode)?[.\s_-]*|e|#)?(?P<last>[0-9]{1,4})(?:v[0-9]+)?)(?:$|[^\w.])").unwrap()
});
static CROSS_SEASON_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs([0-9]{1,2})e[0-9]{1,4}\s*[-~–]\s*s([0-9]{1,2})e[0-9]{1,4}\b").unwrap()
});
static SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(ova|oad|sp|specials?|ncop|nced|op|ed|pv|ost|trailer|preview|sample|creditless)(?:\b|[0-9])").unwrap()
});
static HALF_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|[^\p{L}\p{N}.])(?:s[0-9]{1,2}\s*e|ep(?:isode)?[.\s_-]*|e[.\s_-]*|#)?[0-9]{1,4}\.5(?:v[0-9]+)?(?:$|[^\p{L}\p{N}.])",
    )
    .unwrap()
});
static EXTRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\+|&|\band\b)[\s._]*(?:ova|oad|sp|specials?|extras?|ncop|nced|op|ed|pv|trailer)(?:[\s._-]*[0-9]+)?\b").unwrap()
});
static SEASON_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[(]\s*(s[0-9]{1,2}|season[ ._-]+[0-9]{1,2}|[0-9]{1,2}(?:st|nd|rd|th)[ ._-]+season)\s*[\])]").unwrap()
});
static SCENE_EPISODE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[(【]\s*(?P<episode>s[0-9]{1,2}\s*e[0-9]{1,4}(?:v[0-9]+)?)\s*[\])】]")
        .unwrap()
});
static PACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:batch|complete(?:d)?(?:[ ._-]+series)?|collection)\b").unwrap()
});
static SEASON_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(s\d{1,2})[ ._-]*e\d{1,4}(?:v\d+)?\b").unwrap());
static MARKED_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:[\s._]+[-_·][\s._]*|\bep(?:isode)?[\s._-]*|\be[\s._-]*)([0-9]{1,4})(?:v[0-9]+)?\b",
    )
    .unwrap()
});
static RELEASE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:dual[ ._-]+audio|multi[ ._-]+(?:audio|subs?)|english[ ._-]+sub(?:bed|s)?|end|final)\b").unwrap()
});
static NAMED_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:season (?P<number>[0-9]{1,2})|(?P<ordinal>[0-9]{1,2})(?:st|nd|rd|th) season)\b",
    )
    .unwrap()
});

fn next_episode(entry: &ListEntry) -> Option<i64> {
    let next = entry.progress.max(0).checked_add(1)?;
    entry
        .media
        .as_ref()
        .and_then(|m| m.episodes)
        .filter(|total| *total > 0)
        .is_none_or(|total| next <= total)
        .then_some(next)
}

fn search_title(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn season_alias(title: &str) -> (String, Option<u32>) {
    let Some(captures) = NAMED_SEASON.captures(title) else {
        return (title.to_string(), None);
    };
    let season = captures
        .name("number")
        .or_else(|| captures.name("ordinal"))
        .and_then(|m| m.as_str().parse::<u32>().ok())
        .filter(|n| (1..=50).contains(n));
    let base = search_title(&NAMED_SEASON.replace(title, " "));
    if base.is_empty() || season.is_none() {
        (title.to_string(), None)
    } else {
        (base, season)
    }
}

fn season_terms(season: u32) -> String {
    let suffix = if (11..=13).contains(&(season % 100)) {
        "th"
    } else {
        match season % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    };
    format!("\"S{season:02}\"|\"S{season}\"|\"Season {season}\"|\"{season}{suffix} Season\"|\"{season}\"")
}

fn queries(media: &Media, next: Option<i64>) -> Vec<String> {
    let mut aliases = Vec::new();
    for title in [
        media.title_english.as_deref(),
        media.title_romaji.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let title = search_title(title);
        if !title.is_empty()
            && !aliases
                .iter()
                .any(|a: &String| a.eq_ignore_ascii_case(&title))
        {
            aliases.push(title);
        }
    }
    if aliases.is_empty() {
        if let Some(title) = media.title_native.as_deref() {
            let title = search_title(title);
            if !title.is_empty() {
                aliases.push(title);
            }
        }
    }
    let mut queries = Vec::new();
    for alias in aliases {
        let (base, explicit_season) = season_alias(&alias);
        let title_query = explicit_season.map_or_else(
            || base.clone(),
            |season| format!("{base} {}", season_terms(season)),
        );
        queries.push(title_query.clone());
        let mut packs = vec![
            "batch".to_string(),
            "complete".to_string(),
            "BD".to_string(),
        ];
        if let Some(total) = media.episodes.filter(|n| *n > 1) {
            packs.push(format!("01-{total:02}"));
            packs.push(format!("1-{total}"));
        }
        let packs = packs
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join("|");
        queries.push(format!("{title_query} {packs}"));
        if let Some(next) = next {
            let season = explicit_season
                .or_else(|| {
                    recognize::norm_title(&alias)
                        .split_whitespace()
                        .next_back()
                        .and_then(|s| s.parse::<u32>().ok())
                        .filter(|n| (1..=50).contains(n))
                })
                .unwrap_or(1);
            queries.push(format!(
                "{base} \"{next:02}\"|\"{next}\"|\"E{next:02}\"|\"S{season:02}E{next:02}\""
            ));
        }
    }
    queries
}

pub async fn find(
    entry: &ListEntry,
    matchers: &[Matcher],
    category: &str,
    filter: &str,
) -> Result<ShowTorrents> {
    let media = entry.media.as_ref().ok_or_else(|| {
        anyhow!("This show's details are not cached. Sync your list and try again.")
    })?;
    let next = next_episode(entry);
    let queries = queries(media, next);
    if queries.is_empty() {
        return Err(anyhow!("This show has no title to search for"));
    }
    if !matchers.iter().any(|m| m.media_id == entry.media_id) {
        return Err(anyhow!(
            "This show's titles are not available. Sync your list and try again."
        ));
    }
    let fetched = rss::search_variants(&queries, category, filter).await?;
    let mut result = classify(entry, matchers, fetched.items);
    if !fetched.failures.is_empty() {
        result.warnings.push(format!(
            "{} of {} searches could not finish. Some download options may be missing. Try again to retry them.",
            fetched.failures.len(), queries.len()
        ));
    }
    Ok(result)
}

struct EpisodeRange {
    span: std::ops::Range<usize>,
    first: i64,
    last: i64,
    season: String,
}

fn title_patterns(matched: &Matcher) -> Vec<Regex> {
    let mut seen = HashSet::new();
    matched
        .variants
        .iter()
        .filter_map(|variant| {
            let words = variant
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .map(regex::escape)
                .collect::<Vec<_>>();
            if words.is_empty() {
                return None;
            }
            let pattern = format!(
                r"(?i)(?:^|[^\p{{L}}\p{{N}}])(?P<title>{})(?:$|[^\p{{L}}\p{{N}}])",
                words.join(r"[\W_]+")
            );
            if seen.insert(pattern.to_lowercase()) {
                Regex::new(&pattern).ok()
            } else {
                None
            }
        })
        .collect()
}

fn mask_title(title: &str, patterns: &[Regex]) -> String {
    // Keep byte offsets so range cleanup preserves the original title.
    let mut masked = title.to_string();
    for pattern in patterns {
        if let Some(found) = pattern.captures(&masked).and_then(|c| c.name("title")) {
            masked.replace_range(found.start()..found.end(), &" ".repeat(found.len()));
        }
    }
    masked
}

fn episode_ranges(masked: &str) -> Vec<EpisodeRange> {
    let mut ranges: Vec<EpisodeRange> = Vec::new();
    let mut from = 0;
    while let Some(c) = RANGE.captures_at(masked, from) {
        from = c.name("first").unwrap().end();
        let first = c.name("first").unwrap().as_str().parse::<i64>().unwrap();
        let last = c.name("last").unwrap().as_str().parse::<i64>().unwrap();
        let span = c.name("range").unwrap().range();
        if first < last && last < 1900 && ranges.last().is_none_or(|r| r.span.end <= span.start) {
            ranges.push(EpisodeRange {
                span,
                first,
                last,
                season: c.name("season").map_or("", |m| m.as_str()).to_string(),
            });
        }
    }
    ranges
}

fn unrelated_special(masked: &str, format: Option<&str>) -> bool {
    let masked = masked.replace('_', " ");
    SPECIAL.captures_iter(&masked).any(|c| {
        let marker = c[1].to_ascii_lowercase();
        !matches!(
            (format, marker.as_str()),
            (Some("OVA"), "ova" | "oad") | (Some("SPECIAL"), "sp" | "special" | "specials")
        )
    })
}

fn title_contains(matched: &Matcher, words: &str) -> bool {
    let words = format!(" {} ", recognize::norm_title(words));
    matched
        .variants
        .iter()
        .any(|variant| format!(" {} ", recognize::norm_title(variant)).contains(&words))
}

fn strip_release_words(title: &str, pattern: &Regex, matched: &Matcher) -> String {
    pattern
        .replace_all(title, |c: &regex::Captures| {
            if title_contains(matched, &c[0]) {
                c[0].to_string()
            } else {
                " ".to_string()
            }
        })
        .into_owned()
}

fn collapse_known_aliases(title: &str, patterns: &[Regex]) -> String {
    let mut spans = patterns
        .iter()
        .flat_map(|pattern| {
            pattern
                .captures_iter(title)
                .filter_map(|captures| captures.name("title").map(|found| found.range()))
        })
        .collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut distinct: Vec<std::ops::Range<usize>> = Vec::new();
    for span in spans {
        if distinct.last().is_none_or(|last| last.end <= span.start) {
            distinct.push(span);
        }
    }
    let mut collapsed = title.to_string();
    for pair in distinct.windows(2).rev() {
        let separator = title[pair[0].end..pair[1].start].trim();
        if matches!(separator, "/" | "|" | "｜" | "／") {
            let span = pair[0].end..pair[1].end;
            collapsed.replace_range(span.clone(), &" ".repeat(span.len()));
        }
    }
    collapsed
}

fn unbracket_scene_episodes(title: &str) -> String {
    SCENE_EPISODE_TAG
        .replace_all(title, |captures: &regex::Captures| {
            let tag = captures.get(0).unwrap();
            let episode = captures.name("episode").unwrap();
            format!(
                "{}{}{}",
                " ".repeat(episode.start() - tag.start()),
                episode.as_str(),
                " ".repeat(tag.end() - episode.end())
            )
        })
        .into_owned()
}

pub fn match_feed<'a>(matchers: &'a [Matcher], title: &str) -> (Option<&'a Matcher>, Option<i64>) {
    let title = unbracket_scene_episodes(title);
    let matched = recognize::match_title(matchers, &title, "");
    let episode = matched
        .filter(|matched| !recognize::excluded_playback_release(matched, &title))
        .and_then(|matched| recognize::resolve_episode(matched, &[&title]));
    (matched, episode)
}

fn matching_title(
    title: &str,
    matched: &Matcher,
    ranges: &[EpisodeRange],
    extras: bool,
    patterns: &[Regex],
) -> String {
    let mut title = unbracket_scene_episodes(&collapse_known_aliases(title, patterns));
    let episode = recognize::resolve_episode(matched, &[&title]);
    for range in ranges.iter().rev() {
        title.replace_range(range.span.clone(), &range.season);
    }
    if extras {
        title = EXTRAS.replace_all(&title, " ").into_owned();
    }
    let title = SEASON_TAG.replace_all(&title, " $1 ");
    let title = strip_release_words(&title, &PACK, matched);
    let title = strip_release_words(&title, &RELEASE_WORDS, matched);
    MARKED_EPISODE
        .replace_all(&title, |c: &regex::Captures| {
            if episode.is_some() && c[1].parse::<i64>().ok() == episode {
                " ".to_string()
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

fn ambiguous_aliases(matchers: &[Matcher], media_id: i64) -> Vec<(i64, Vec<Regex>)> {
    let Some(target) = matchers.iter().find(|m| m.media_id == media_id) else {
        return vec![];
    };
    let norms: HashSet<String> = target
        .variants
        .iter()
        .map(|v| recognize::norm_title(v))
        .collect();
    let collisions: Vec<_> = matchers
        .iter()
        .filter(|m| {
            m.variants
                .iter()
                .any(|v| norms.contains(&recognize::norm_title(v)))
        })
        .collect();
    if collisions.len() < 2 {
        return vec![];
    }
    collisions
        .into_iter()
        .map(|m| {
            (
                m.media_id,
                m.variants
                    .iter()
                    .map(|variant| {
                        Regex::new(&format!(
                            r"(?i)(?:^|[\s._\-\[\(]){}(?:$|[\s._\-\[\(])",
                            regex::escape(variant)
                        ))
                        .unwrap()
                    })
                    .collect(),
            )
        })
        .collect()
}

fn strict_match<'a>(
    matchers: &'a [Matcher],
    title: &str,
    media_id: i64,
    ambiguous: &[(i64, Vec<Regex>)],
    ranges: &[EpisodeRange],
    extras: bool,
    patterns: &[Regex],
) -> Option<&'a Matcher> {
    let target = matchers.iter().find(|m| m.media_id == media_id)?;
    let cleaned = matching_title(title, target, ranges, extras, patterns);
    let candidates = if ambiguous.is_empty() {
        matchers
    } else {
        let mut identified = ambiguous
            .iter()
            .filter(|(_, variants)| variants.iter().any(|r| r.is_match(title)));
        if identified.next().map(|(id, _)| *id) != Some(media_id) || identified.next().is_some() {
            return None;
        }
        std::slice::from_ref(target)
    };
    let matched = recognize::match_title(candidates, &cleaned, "")?;
    if matched.media_id != media_id {
        return None;
    }
    let without_episode = SEASON_EPISODE.replace_all(&cleaned, "$1");
    let candidates = [
        recognize::clean_title(&without_episode),
        recognize::norm_title(&without_episode),
    ];
    matched
        .variants
        .iter()
        .any(|variant| {
            let norm = recognize::norm_title(variant);
            candidates.iter().any(|candidate| {
                candidate == &norm || candidate.strip_suffix(" 1") == Some(norm.as_str())
            })
        })
        .then_some(matched)
}

fn item_key(item: &RawItem) -> String {
    item.info_hash
        .as_deref()
        .and_then(rss::valid_info_hash)
        .map(|hash| hash.to_ascii_lowercase())
        .unwrap_or_else(|| {
            if !item.link.is_empty() {
                item.link.clone()
            } else {
                item.guid
                    .rsplit('\u{1}')
                    .next()
                    .unwrap_or(&item.guid)
                    .to_string()
            }
        })
}

pub fn search_results(items: Vec<RawItem>, seen: &HashSet<String>) -> Vec<TorrentItem> {
    items
        .into_iter()
        .map(|raw| TorrentItem {
            seen: was_seen(&raw, seen),
            seen_guid: Some(stable_guid(&raw)),
            details_url: rss::details_url(&raw),
            magnet: raw
                .info_hash
                .as_deref()
                .and_then(|hash| rss::magnet_for(hash, &raw.title)),
            title: raw.title,
            link: raw.link,
            guid: raw.guid,
            size: raw.size,
            seeders: raw.seeders,
            leechers: raw.leechers,
            category_id: raw.category_id,
            category: raw.category,
            trusted: raw.trusted,
            remake: raw.remake,
            published: raw.published,
            ..Default::default()
        })
        .collect()
}

pub fn stable_guid(item: &RawItem) -> String {
    format!("nyaa-show\u{1}{}", item_key(item))
}

pub fn was_seen(item: &RawItem, seen: &HashSet<String>) -> bool {
    seen.contains(&item.guid) || seen.contains(&stable_guid(item))
}

pub fn restore_seen(result: &mut ShowTorrents, seen: &HashSet<String>) {
    for item in result
        .batches
        .iter_mut()
        .chain(result.episodes.iter_mut())
        .chain(result.other.iter_mut())
    {
        item.seen = seen.contains(&item.guid);
    }
}

fn classify(entry: &ListEntry, matchers: &[Matcher], items: Vec<RawItem>) -> ShowTorrents {
    let mut result = ShowTorrents {
        next_episode: next_episode(entry),
        ..Default::default()
    };
    let total = entry
        .media
        .as_ref()
        .and_then(|m| m.episodes)
        .filter(|t| *t > 0);
    let finished = entry.media.as_ref().and_then(|m| m.status.as_deref()) == Some("FINISHED");
    let format = entry.media.as_ref().and_then(|m| m.format.as_deref());
    let Some(target) = matchers.iter().find(|m| m.media_id == entry.media_id) else {
        return result;
    };
    let title_patterns = title_patterns(target);
    let ambiguous = ambiguous_aliases(matchers, entry.media_id);
    let mut seen = HashSet::new();
    for raw in items {
        let masked = mask_title(&raw.title, &title_patterns);
        if CROSS_SEASON_RANGE
            .captures_iter(&masked)
            .any(|c| c[1].parse::<u32>().ok() != c[2].parse::<u32>().ok())
        {
            continue;
        }
        let ranges = episode_ranges(&masked);
        if ranges.is_empty() && HALF_EPISODE.is_match(&masked) {
            continue;
        }
        let extras = EXTRAS.is_match(&masked)
            && (ranges
                .iter()
                .any(|r| r.first == 1 && total.is_some_and(|t| r.last == t))
                || PACK.is_match(&masked));
        let metadata = if extras {
            EXTRAS.replace_all(&masked, " ").into_owned()
        } else {
            masked
        };
        if unrelated_special(&metadata, format) {
            continue;
        }
        let Some(matched) = strict_match(
            matchers,
            &raw.title,
            entry.media_id,
            &ambiguous,
            &ranges,
            extras,
            &title_patterns,
        ) else {
            continue;
        };
        if !seen.insert(item_key(&raw)) {
            continue;
        }
        let episode = if ranges.is_empty() {
            let title =
                unbracket_scene_episodes(&collapse_known_aliases(&raw.title, &title_patterns));
            recognize::resolve_episode(matched, &[&title])
        } else {
            None
        };
        if episode.is_some_and(|ep| ep < 1 || total.is_some_and(|t| ep > t)) {
            continue;
        }
        if ranges.iter().any(|r| total.is_some_and(|t| r.last > t)) {
            continue;
        }
        let batch = episode.is_none()
            && if ranges.is_empty() {
                PACK.find_iter(&raw.title)
                    .any(|word| !title_contains(matched, word.as_str()))
                    || (finished && total.is_some_and(|t| t > 1))
            } else {
                ranges.iter().any(|r| {
                    result
                        .next_episode
                        .is_none_or(|next| r.first <= next && next <= r.last)
                })
            };
        let item = TorrentItem {
            seen_guid: Some(stable_guid(&raw)),
            details_url: rss::details_url(&raw),
            magnet: raw
                .info_hash
                .as_deref()
                .and_then(|hash| rss::magnet_for(hash, &raw.title)),
            guid: stable_guid(&raw),
            title: raw.title,
            link: raw.link,
            size: raw.size,
            seeders: raw.seeders,
            leechers: raw.leechers,
            category_id: raw.category_id,
            category: raw.category,
            trusted: raw.trusted,
            remake: raw.remake,
            published: raw.published,
            media_id: Some(matched.media_id),
            matched: Some(matched.display.clone()),
            episode,
            is_new: false,
            seen: false,
        };
        if batch {
            result.batches.push(item);
        } else if episode.is_some() && episode == result.next_episode {
            result.episodes.push(item);
        } else {
            result.other.push(item);
        }
    }
    for items in [&mut result.batches, &mut result.episodes, &mut result.other] {
        rank_alternatives(items);
    }
    result
}

fn group(title: &str) -> String {
    title
        .trim_start()
        .strip_prefix('[')
        .and_then(|t| t.split_once(']'))
        .map(|(g, _)| g.to_lowercase())
        .unwrap_or_default()
}

fn rank_alternatives(items: &mut Vec<TorrentItem>) {
    items.sort_by_key(|item| {
        (
            std::cmp::Reverse(item.seeders.unwrap_or(0)),
            std::cmp::Reverse(item.published.unwrap_or(0)),
        )
    });
    let mut groups = HashSet::new();
    let mut repeats = Vec::new();
    let mut diverse = Vec::new();
    for item in items.drain(..) {
        if groups.insert(group(&item.title)) {
            diverse.push(item);
        } else {
            repeats.push(item);
        }
    }
    diverse.extend(repeats);
    *items = diverse;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn entry() -> ListEntry {
        ListEntry {
            media_id: 1,
            status: "CURRENT".into(),
            progress: 4,
            media: Some(Media {
                id: 1,
                title_english: Some("Some Show".into()),
                title_romaji: Some("Watashi no Anime".into()),
                episodes: Some(12),
                status: Some("FINISHED".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn matchers(entry: &ListEntry) -> Vec<Matcher> {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
        db.upsert_entry(entry).unwrap();
        recognize::build_matchers(&db)
    }

    fn raw(title: &str, seeders: i64) -> RawItem {
        RawItem {
            title: title.into(),
            link: format!("https://nyaa.si/view/{title}"),
            seeders: Some(seeders),
            ..Default::default()
        }
    }

    #[test]
    fn feeds_do_not_label_fractional_releases_as_regular_episodes() {
        let entry = entry();
        let matchers = matchers(&entry);
        for title in [
            "[G] Some Show - 10.5.mkv",
            "[G] Some Show - 12.5 [1080p]",
            "[G] Some Show - E10.5 [1080p]",
        ] {
            let (matched, episode) = match_feed(&matchers, title);
            assert_eq!(matched.map(|m| m.media_id), Some(1), "{title}");
            assert_eq!(episode, None, "{title}");
        }
        for title in [
            "[G] Some Show - 10 [1080p]",
            "[G] Some Show [S01E10] [1080p]",
            "[G] Some Show - 10 [1080p FLAC 5.1]",
        ] {
            assert_eq!(match_feed(&matchers, title).1, Some(10), "{title}");
        }
    }

    #[test]
    fn finished_airing_still_searches_the_next_unwatched_episode() {
        let mut e = entry();
        assert_eq!(next_episode(&e), Some(5));
        let q = queries(e.media.as_ref().unwrap(), next_episode(&e));
        assert_eq!(q.len(), 6);
        assert!(q.iter().any(|q| q.contains("Some Show \"05\"")));
        assert!(q.iter().any(|q| q.contains("Watashi no Anime \"batch\"")));
        assert!(q.iter().any(|q| q.contains("\"01-12\"")));
        e.progress = 12;
        assert_eq!(next_episode(&e), None);
        assert_eq!(
            queries(e.media.as_ref().unwrap(), next_episode(&e)).len(),
            4
        );
    }

    #[test]
    fn adversarial_separator_delimited_previews_are_not_complete_packs() {
        let e = entry();
        let targets = matchers(&e);
        for title in [
            "[A] Some Show [Preview_1080p]",
            "[A] Some Show [1080p_Sample]",
            "[A] Some Show [Creditless_1080p]",
        ] {
            let result = classify(&e, &targets, vec![raw(title, 100)]);
            assert!(result.batches.is_empty(), "{title}: {:?}", result.batches);
            assert!(result.episodes.is_empty(), "{title}");
            assert!(result.other.is_empty(), "{title}");
        }
        let mut e = e;
        e.media.as_mut().unwrap().title_english = Some("The Preview".into());
        let result = classify(&e, &matchers(&e), vec![raw("[A] The_Preview [01-12]", 100)]);
        assert_eq!(result.batches.len(), 1);
    }

    #[test]
    fn adversarial_separator_delimited_half_episodes_are_not_complete_packs() {
        let e = entry();
        let targets = matchers(&e);
        for title in ["[A] Some Show [12.5_1080p]", "[A] Some Show - 12.5_[1080p]"] {
            let result = classify(&e, &targets, vec![raw(title, 100)]);
            assert!(result.batches.is_empty(), "{title}");
            assert!(result.episodes.is_empty(), "{title}");
            assert!(result.other.is_empty(), "{title}");
        }
    }

    #[test]
    fn searches_deduplicate_names_and_remove_query_operators() {
        let mut e = entry();
        let media = e.media.as_mut().unwrap();
        media.title_romaji = Some("SOME SHOW".into());
        assert_eq!(queries(media, Some(5)).len(), 3);
        assert_eq!(search_title("A \"B\" | -1080p"), "A B 1080p");
    }

    #[test]
    fn named_seasons_search_scene_packs_and_the_next_scene_episode() {
        let mut e = entry();
        let media = e.media.as_mut().unwrap();
        for title in ["Some Show 2nd Season", "Some Show Season 2"] {
            media.title_english = Some(title.into());
            media.title_romaji = None;
            let q = queries(media, Some(5));
            assert_eq!(q.len(), 3);
            assert!(q[0].starts_with("Some Show \"S02\"|"));
            assert!(q[0].contains("\"Season 2\""));
            assert!(q[0].contains("\"2nd Season\""));
            assert!(q[1].starts_with("Some Show \"S02\"|"));
            assert!(q[1].contains("\"batch\""));
            assert!(q[2].starts_with("Some Show \"05\"|"));
            assert!(q[2].contains("\"S02E05\""));
        }
        media.title_romaji = Some("Watashi no Anime 2nd Season".into());
        assert_eq!(queries(media, Some(5)).len(), 6);
        assert!(queries(media, Some(5))
            .iter()
            .any(|q| q.starts_with("Watashi no Anime \"05\"|") && q.contains("\"S02E05\"")));
    }

    #[test]
    fn title_numbers_are_not_removed_as_implicit_seasons() {
        for title in [
            "86",
            "Mob Psycho 100",
            "Some Show 2",
            "No 6",
            "Some Show Final Season",
        ] {
            assert_eq!(season_alias(title), (title.to_string(), None));
        }
    }

    #[test]
    fn numeric_title_numbers_never_become_episode_ranges() {
        for (title, release, next) in [
            ("Kaiju No. 8", "[A] Kaiju No. 8 - 09 [1080p]", 9),
            ("No. 6", "[A] No. 6 - 07 [1080p]", 7),
            ("Kaiju No. 8", "[A] Kaiju.No.8 - 09 [1080p]", 9),
            ("怪獣 No. 8", "[A] 怪獣 No. 8 - 09 [1080p]", 9),
        ] {
            let mut e = entry();
            e.progress = next - 1;
            e.media.as_mut().unwrap().title_english = Some(title.into());
            e.media.as_mut().unwrap().title_romaji = None;
            let result = classify(&e, &matchers(&e), vec![raw(release, 100)]);
            assert_eq!(result.episodes.len(), 1, "{release}");
            assert_eq!(result.episodes[0].episode, Some(next));
        }
    }

    #[test]
    fn numeric_titles_keep_their_real_batch_ranges() {
        for title in ["86", "Some Show 2", "Some Show 2nd Season"] {
            let mut e = entry();
            e.media.as_mut().unwrap().title_english = Some(title.into());
            e.media.as_mut().unwrap().title_romaji = None;
            let release = if title == "Some Show 2nd Season" {
                "[A] Some Show Season 2 - 01-12 [BD]".into()
            } else {
                format!("[A] {title} - 01-12 [BD]")
            };
            let result = classify(&e, &matchers(&e), vec![raw(&release, 100)]);
            assert_eq!(result.batches.len(), 1, "{release}");
        }
    }

    #[test]
    fn special_releases_and_cross_season_ranges_are_not_regular_downloads() {
        let e = entry();
        for release in [
            "[A] Some Show [OVA] [BD]",
            "[A] Some Show - 05 [Special] [1080p]",
            "[A] Some Show [NCOP] [1080p]",
            "[A] Some Show [S01E01-S02E12] [Batch]",
            "[A] Some Show S01E01-S02E12 [Batch]",
        ] {
            let result = classify(&e, &matchers(&e), vec![raw(release, 100)]);
            assert!(
                result.batches.is_empty() && result.episodes.is_empty() && result.other.is_empty(),
                "{release}"
            );
        }
    }

    #[test]
    fn adversarial_preview_and_sample_releases_are_not_download_options() {
        let e = entry();
        let matchers = matchers(&e);
        for release in [
            "[A] Some Show [Preview] [1080p]",
            "[A] Some Show [Sample] [1080p]",
            "[A] Some Show - 05 [Preview] [1080p]",
            "[A] Some Show [Creditless] [1080p]",
        ] {
            let result = classify(&e, &matchers, vec![raw(release, 100)]);
            assert!(
                result.batches.is_empty() && result.episodes.is_empty() && result.other.is_empty(),
                "{release} was suggested as a download: {result:?}"
            );
        }
    }

    #[test]
    fn verified_series_packs_can_include_extra_specials() {
        let e = entry();
        for release in [
            "[A] Some Show - 01-12 [BD + OVA + NCOP + NCED]",
            "[A] Some Show - 01-12 + OVA [BD]",
            "[A] Some Show [Complete] + OVA [BD]",
        ] {
            let result = classify(&e, &matchers(&e), vec![raw(release, 100)]);
            assert_eq!(result.batches.len(), 1, "{release}");
        }
        let mut special = entry();
        special.media.as_mut().unwrap().title_english = Some("Some Special Show".into());
        special.media.as_mut().unwrap().title_romaji = None;
        let result = classify(
            &special,
            &matchers(&special),
            vec![raw("[A] Some Special Show - 05 [1080p]", 100)],
        );
        assert_eq!(result.episodes.len(), 1);
        let mut ova = entry();
        ova.media.as_mut().unwrap().format = Some("OVA".into());
        let result = classify(
            &ova,
            &matchers(&ova),
            vec![raw("[A] Some Show - 05 [OVA]", 100)],
        );
        assert_eq!(result.episodes.len(), 1);
    }

    #[test]
    fn every_batch_range_is_checked_and_can_cover_the_next_episode() {
        let e = entry();
        let matchers = matchers(&e);
        let invalid = classify(
            &e,
            &matchers,
            vec![raw("[A] Some Show [01-06] [07-24] [Batch]", 100)],
        );
        assert!(invalid.batches.is_empty() && invalid.other.is_empty());
        let valid = classify(
            &e,
            &matchers,
            vec![raw("[A] Some Show [01-04] [05-12] [BD]", 100)],
        );
        assert_eq!(valid.batches.len(), 1);
        let already_watched = classify(
            &e,
            &matchers,
            vec![raw("[A] Some Show [01-04] [Batch]", 100)],
        );
        assert!(already_watched.batches.is_empty());
        assert_eq!(already_watched.other.len(), 1);
    }

    #[test]
    fn packs_and_next_episodes_keep_group_choices_without_duplicate_search_hits() {
        let e = entry();
        let matchers = matchers(&e);
        let item = raw("[A] Some Show - 05 [1080p]", 100);
        let result = classify(
            &e,
            &matchers,
            vec![
                raw("[A] Some Show - 01-12 [1080p]", 300),
                raw("[B] Watashi no Anime [BD 1080p]", 250),
                raw("[C] Some Show [Complete]", 200),
                item.clone(),
                item,
                raw("[A] Some Show - 05 [720p]", 90),
                raw("[B] Some Show - 05 [1080p]", 20),
                raw("[C] Some Show - 04 [1080p]", 5),
            ],
        );
        assert_eq!(result.batches.len(), 3);
        assert_eq!(result.episodes.len(), 3);
        assert!(result.episodes[0].title.starts_with("[A]"));
        assert!(result.episodes[1].title.starts_with("[B]"));
        assert_eq!(result.other.len(), 1);
    }

    #[test]
    fn saved_discovery_marks_survive_requery_and_apply_to_later_feed_items() {
        let e = entry();
        let matchers = matchers(&e);
        let mut release = raw("[A] Some Show - 05 [1080p]", 100);
        release.info_hash = Some("abcdef0123456789abcdef0123456789abcdef01".into());
        release.guid = "first-search\u{1}item".into();
        let initial = classify(&e, &matchers, vec![release.clone()]);
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        let guid = initial.episodes[0].guid.clone();
        release.guid = "second-search\u{1}item".into();
        let mut refreshed = classify(&e, &matchers, vec![release.clone()]);
        db.mark_rss_seen(&[guid.clone()]).unwrap();
        restore_seen(&mut refreshed, &db.rss_seen_set().unwrap());
        assert_eq!(refreshed.episodes[0].guid, guid);
        assert!(refreshed.episodes[0].seen);
        release.guid = "later-feed\u{1}item".into();
        assert!(was_seen(&release, &db.rss_seen_set().unwrap()));
        assert!(was_seen(&release, &HashSet::from([release.guid.clone()])));
        assert!(!was_seen(
            &raw("[B] Some Show - 05 [1080p]", 100),
            &db.rss_seen_set().unwrap()
        ));
        let reverse_db = Db::open(std::path::Path::new(":memory:")).unwrap();
        reverse_db
            .mark_rss_seen(&[release.guid.clone(), stable_guid(&release)])
            .unwrap();
        let mut suggestions = classify(&e, &matchers, vec![release.clone()]);
        restore_seen(&mut suggestions, &reverse_db.rss_seen_set().unwrap());
        assert!(suggestions.episodes[0].seen);
        assert_eq!(
            suggestions.episodes[0].seen_guid.as_deref(),
            Some(guid.as_str())
        );
    }

    #[test]
    fn unrelated_sequels_specials_and_out_of_total_episodes_are_excluded() {
        let e = entry();
        let result = classify(
            &e,
            &matchers(&e),
            vec![
                raw("[A] Some Show Season 2 - 05 [1080p]", 100),
                raw("[A] Some Show Shippuden - 05 [1080p]", 100),
                raw("[A] Some Show OVA [BD]", 100),
                raw("[A] Some Show - 13 [1080p]", 100),
                raw("[A] Some Show - 01-24 [1080p]", 100),
                raw("[A] Unrelated Show - 05 [1080p]", 100),
            ],
        );
        assert!(result.batches.is_empty());
        assert!(result.episodes.is_empty());
        assert!(result.other.is_empty());
    }

    #[test]
    fn punctuation_distinguishes_colliding_aliases_despite_list_status() {
        let mut first = entry();
        first.media.as_mut().unwrap().title_english = Some("K-On!".into());
        first.media.as_mut().unwrap().title_romaji = None;
        let mut second = first.clone();
        second.media_id = 2;
        second.status = "COMPLETED".into();
        second.media.as_mut().unwrap().id = 2;
        second.media.as_mut().unwrap().title_english = Some("K-On!!".into());
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for e in [&first, &second] {
            db.upsert_media(e.media.as_ref().unwrap()).unwrap();
            db.upsert_entry(e).unwrap();
        }
        let matchers = recognize::build_matchers(&db);
        let releases = vec![
            raw("[A] K-On! [BD]", 100),
            raw("[B] K-On!! [BD]", 100),
            raw("[C] K On [BD]", 100),
        ];
        let first_result = classify(&first, &matchers, releases.clone());
        assert_eq!(first_result.batches.len(), 1);
        assert_eq!(first_result.batches[0].title, "[A] K-On! [BD]");
        let second_result = classify(&second, &matchers, releases);
        assert_eq!(second_result.batches.len(), 1);
        assert_eq!(second_result.batches[0].title, "[B] K-On!! [BD]");
    }

    #[test]
    fn scene_seasons_and_episode_ranges_stay_with_the_correct_season() {
        let mut e = entry();
        let media = e.media.as_mut().unwrap();
        media.title_english = Some("Some Show 2nd Season".into());
        media.title_romaji = None;
        let result = classify(
            &e,
            &matchers(&e),
            vec![
                raw("[A] Some Show S02E05 [1080p]", 100),
                raw("[A] Some Show S02E01-E12 [1080p]", 100),
                raw("[A] Some Show Season 2 [BD]", 100),
                raw("[A] Some Show S01E05 [1080p]", 100),
            ],
        );
        assert_eq!(result.episodes.len(), 1);
        assert_eq!(result.batches.len(), 2);
    }

    #[test]
    fn unlabelled_airing_releases_are_not_assumed_to_be_packs() {
        let mut e = entry();
        e.media.as_mut().unwrap().status = Some("RELEASING".into());
        let result = classify(&e, &matchers(&e), vec![raw("[A] Some Show [1080p]", 100)]);
        assert!(result.batches.is_empty());
        assert_eq!(result.other.len(), 1);
    }

    #[test]
    fn metadata_words_within_the_show_title_are_preserved() {
        let mut e = entry();
        let media = e.media.as_mut().unwrap();
        media.title_english = Some("Some Show Final Season".into());
        media.title_romaji = None;
        let result = classify(
            &e,
            &matchers(&e),
            vec![
                raw("[A] Some Show Final Season - 05 [1080p]", 100),
                raw("[B] Some Show Final Season - 01-12 [Batch]", 100),
                raw("[C] Some Show Final Season [BD]", 100),
            ],
        );
        assert_eq!(result.episodes.len(), 1);
        assert_eq!(result.batches.len(), 2);
    }
    #[test]
    fn invalid_info_hashes_do_not_merge_unrelated_releases_or_seen_marks() {
        let entry = entry();
        let matchers = matchers(&entry);
        for hash in ["", "unknown", "invalid&tr=tracker"] {
            let mut first = raw("[A] Some Show - 05 [1080p]", 100);
            first.info_hash = Some(hash.into());
            let mut second = raw("[B] Some Show - 05 [1080p]", 50);
            second.info_hash = Some(hash.into());
            let result = classify(&entry, &matchers, vec![first.clone(), second.clone()]);
            assert_eq!(
                result.episodes.len(),
                2,
                "invalid hash {hash:?} must not merge releases"
            );
            assert!(!was_seen(&second, &HashSet::from([stable_guid(&first)])));
        }
    }

    #[test]
    fn manual_search_restores_saved_seen_marks_for_new_query_guids() {
        let mut first = raw("[A] Some Show - 05 [1080p]", 100);
        first.info_hash = Some("abcdef0123456789abcdef0123456789abcdef01".into());
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.mark_rss_seen(&[stable_guid(&first)]).unwrap();
        first.guid = "different-query\u{1}release".into();
        let second = raw("[B] Some Show - 05 [1080p]", 50);
        let result = search_results(vec![first, second], &db.rss_seen_set().unwrap());
        assert!(result[0].seen);
        assert_eq!(result[0].guid, "different-query\u{1}release");
        assert!(!result[1].seen);
    }

    #[test]
    fn canonical_hash_identity_ignores_case_and_surrounding_spaces() {
        let mut first = raw("Some Show", 100);
        first.info_hash = Some("ABCDEF0123456789ABCDEF0123456789ABCDEF01".into());
        let mut second = first.clone();
        second.info_hash = Some(" abcdef0123456789abcdef0123456789abcdef01 ".into());
        assert_eq!(stable_guid(&first), stable_guid(&second));
    }
    #[test]
    fn bilingual_release_titles_keep_known_aliases_and_reject_unknown_sequels() {
        let entry = entry();
        let matchers = matchers(&entry);
        let releases = vec![
            raw("[A] Some Show / Watashi no Anime - 05 [1080p]", 100),
            raw("[B] Watashi no Anime / Some Show [01-12] [Batch]", 50),
            raw("[C] Some Show / Unrelated Sequel - 05 [1080p]", 20),
            raw("[D] Some Show / Watashi no Anime Season 2 - 05 [1080p]", 10),
        ];
        let result = classify(&entry, &matchers, releases);
        assert_eq!(result.episodes.len(), 1);
        assert_eq!(result.batches.len(), 1);
        assert!(result.other.is_empty());
    }

    #[test]
    fn bilingual_batches_do_not_read_numeric_aliases_as_episodes() {
        let mut entry = entry();
        entry.media.as_mut().unwrap().title_romaji = Some("100-man no Anime".into());
        let result = classify(
            &entry,
            &matchers(&entry),
            vec![raw("[G] Some Show / 100-man no Anime [BD Batch]", 100)],
        );
        assert_eq!(result.batches.len(), 1);
        assert_eq!(result.batches[0].episode, None);
    }

    #[test]
    fn half_episode_releases_are_not_batches_or_regular_next_episodes() {
        let entry = entry();
        let matchers = matchers(&entry);
        for release in [
            "[A] Some Show - 05.5 [1080p]",
            "[A] Some Show 05.5 [1080p]",
            "[A] Some Show EP05.5 [1080p]",
            "[A] Some Show S01E05.5 [1080p]",
            "[A] Some Show [05.5] [1080p]",
            "[A] Some Show [05.5v2] [1080p]",
            "[A] Some Show (Episode 05.5) [1080p]",
            "[A] Some Show 【05.5】 [1080p]",
            "[A] Some Show [S01E05.5] [1080p]",
        ] {
            let result = classify(&entry, &matchers, vec![raw(release, 100)]);
            assert!(
                result.batches.is_empty() && result.episodes.is_empty(),
                "{release}"
            );
        }
    }

    #[test]
    fn adversarial_unpadded_half_episodes_are_not_regular_downloads() {
        let entry = entry();
        let matchers = matchers(&entry);
        for release in [
            "[A] Some Show - 5.5 [1080p]",
            "[A] Some Show 5.5 [1080p]",
            "[A] Some Show EP5.5 [1080p]",
            "[A] Some Show S01E5.5 [1080p]",
            "[A] Some Show - 12.5 [1080p]",
        ] {
            let result = classify(&entry, &matchers, vec![raw(release, 100)]);
            assert!(
                result.batches.is_empty() && result.episodes.is_empty(),
                "{release} was suggested as a regular download: {result:?}"
            );
        }
    }

    #[test]
    fn half_episode_filter_preserves_audio_layouts_and_decimal_titles() {
        let mut entry = entry();
        let patterns = matchers(&entry);
        for release in [
            "[A] Some Show - 05 [AAC5.1] [DDP2.0]",
            "[A] Some Show - 05 [5.1] [2.0]",
        ] {
            let result = classify(&entry, &patterns, vec![raw(release, 100)]);
            assert_eq!(result.episodes.len(), 1, "{release}");
            assert_eq!(result.episodes[0].episode, Some(5));
        }
        let result = classify(
            &entry,
            &patterns,
            vec![raw("[A] Some Show [BD] [AAC5.1] [DDP2.0] [5.1]", 100)],
        );
        assert_eq!(result.batches.len(), 1);
        let result = classify(
            &entry,
            &patterns,
            vec![raw("[A] Some Show [01-12] [05.5] [BD]", 100)],
        );
        assert_eq!(result.batches.len(), 1);
        for title in ["Ghost in the Shell 2.0", "Some Show (2.5)"] {
            entry.media.as_mut().unwrap().title_english = Some(title.into());
            entry.media.as_mut().unwrap().title_romaji = None;
            let result = classify(
                &entry,
                &matchers(&entry),
                vec![raw(&format!("[A] {title} [BD]"), 100)],
            );
            assert_eq!(result.batches.len(), 1, "{title}");
        }
    }

    #[test]
    fn bracketed_scene_episodes_keep_the_episode_and_season() {
        let first = entry();
        let mut second = first.clone();
        second.media_id = 2;
        second.media.as_mut().unwrap().id = 2;
        second.media.as_mut().unwrap().title_english = Some("Some Show 2nd Season".into());
        second.media.as_mut().unwrap().title_romaji = None;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for entry in [&first, &second] {
            db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
            db.upsert_entry(entry).unwrap();
        }
        let matchers = recognize::build_matchers(&db);
        for (first_tag, second_tag) in [
            ("[S01E05]", "[S02E05]"),
            ("(S01E05v2)", "(S02E05v2)"),
            ("【S01 E05】", "【S02 E05】"),
        ] {
            for entry in [&first, &second] {
                let result = classify(
                    entry,
                    &matchers,
                    vec![
                        raw(&format!("[A] Some Show {first_tag} [1080p]"), 100),
                        raw(&format!("[B] Some Show {second_tag} [1080p]"), 50),
                        raw("[C] Some Show [S03E05] [1080p]", 40),
                        raw("[D] Some Show [S01E13] [1080p]", 30),
                        raw("[E] Some Show [S02E13] [1080p]", 20),
                    ],
                );
                assert!(
                    result.batches.is_empty(),
                    "{} received a single episode as a batch",
                    entry.media_id
                );
                assert_eq!(
                    result.episodes.len(),
                    1,
                    "{} lost its scene episode",
                    entry.media_id
                );
                assert_eq!(result.episodes[0].episode, Some(5));
                assert!(result.episodes[0].title.contains(if entry.media_id == 1 {
                    first_tag
                } else {
                    second_tag
                }));
                assert!(result.other.is_empty());
            }
        }
    }

    #[test]
    fn scene_tags_preserve_batch_ranges_and_their_original_offsets() {
        let mut entry = entry();
        entry.media.as_mut().unwrap().title_english = Some("Some Show 2nd Season".into());
        entry.media.as_mut().unwrap().title_romaji = None;
        let matchers = matchers(&entry);
        for release in [
            "[A] Some Show [S02E01-E12] [BD]",
            "[A] Some Show 【S02E01】 [02-12] [BD]",
        ] {
            let result = classify(&entry, &matchers, vec![raw(release, 100)]);
            assert_eq!(result.batches.len(), 1, "{release}");
            assert_eq!(result.batches[0].title, release);
            assert!(result.episodes.is_empty() && result.other.is_empty());
        }
    }

    #[test]
    fn feeds_keep_bracketed_scene_episode_numbers_with_the_right_season() {
        let first = entry();
        let mut second = first.clone();
        second.media_id = 2;
        second.media.as_mut().unwrap().id = 2;
        second.media.as_mut().unwrap().title_english = Some("Some Show 2nd Season".into());
        second.media.as_mut().unwrap().title_romaji = None;
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for entry in [&first, &second] {
            db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
            db.upsert_entry(entry).unwrap();
        }
        let matchers = recognize::build_matchers(&db);
        for (title, expected_id, expected_episode) in [
            ("[A] Some Show [S01E05] [1080p]", Some(1), Some(5)),
            ("[A] Some Show [S02E05] [1080p]", Some(2), Some(5)),
            ("[A] Some Show [S03E05] [1080p]", None, None),
            ("[A] Some Show - 05 [AAC5.1]", Some(1), Some(5)),
            ("[A] Some Show [S01E05.5] [1080p]", Some(1), None),
            ("[A] Some Show [S02E05.5] [1080p]", Some(2), None),
            ("[A] Some Show [BD Batch]", Some(1), None),
        ] {
            let (matched, episode) = match_feed(&matchers, title);
            assert_eq!(
                matched.map(|matched| matched.media_id),
                expected_id,
                "{title}"
            );
            assert_eq!(episode, expected_episode, "{title}");
        }
    }
}
