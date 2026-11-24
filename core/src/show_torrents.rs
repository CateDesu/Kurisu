use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::{anyhow, Result};
use regex::Regex;

use crate::models::{ListEntry, Media, ShowTorrents, TorrentItem};
use crate::recognize::{self, Matcher, PACK_WORDS, RELEASE_WORDS};
use crate::rss::{self, RawItem};

static EXTRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:\+|&|\band\b)[\s._]*(?:{})(?:[\s._-]*[0-9]+)?\b",
        recognize::RE_EXTRAS.as_str()
    ))
    .unwrap()
});
static SEASON_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(s\d{1,2})[ ._-]*e\d{1,4}(?:v\d+)?\b").unwrap());
static MARKED_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:[\s._]+[-_·–—][\s._]*|\bep(?:isode)?[\s._-]*|\be[\s._-]*|#)([0-9]{1,4})(?:v[0-9]+)?\b",
    )
    .unwrap()
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
            let season = recognize::catalog_season(&alias)
                .or_else(|| {
                    [
                        media.title_english.as_deref(),
                        media.title_romaji.as_deref(),
                        media.title_native.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .find_map(recognize::catalog_season)
                })
                .unwrap_or(1);
            queries.push(format!(
                "{base} \"{next:02}\"|\"{next}\"|\"E{next:02}\"|\"S{season:02}E{next:02}\""
            ));
        }
    }
    queries
}

pub(crate) async fn find(
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
    fractional_end: bool,
    season: Option<u32>,
}

fn episode_ranges(masked: &str) -> Vec<EpisodeRange> {
    recognize::torrent_episode_ranges(masked)
        .map(|c| EpisodeRange {
            span: c.name("range").unwrap().range(),
            first: c["start"].parse().unwrap(),
            last: c["end"].parse().unwrap(),
            fractional_end: c.name("fraction").is_some(),
            season: c
                .name("season")
                .or_else(|| c.name("end_season"))
                .and_then(|m| m.as_str().parse().ok()),
        })
        .collect()
}

fn title_contains(matched: &Matcher, words: &str) -> bool {
    let words = format!(" {} ", recognize::norm_title(words));
    matched
        .variants
        .iter()
        .any(|variant| format!(" {} ", recognize::catalog_title_norm(variant)).contains(&words))
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

pub(crate) fn match_feed<'a>(
    matchers: &'a [Matcher],
    title: &str,
) -> (Option<&'a Matcher>, Option<i64>) {
    let matched = recognize::match_title(matchers, title, "");
    let episode = matched
        .filter(|matched| !recognize::excluded_playback_release(matched, title))
        .and_then(|matched| recognize::resolve_episode(matched, &[title]));
    (matched, episode)
}

fn matching_title(title: &str, matched: &Matcher, ranges: &[EpisodeRange], extras: bool) -> String {
    let mut title = matched.collapse_known_aliases(title).replace('_', " ");
    let episode = recognize::resolve_episode(matched, &[&title]);
    for range in ranges.iter().rev() {
        let scope = range.season.map_or_else(String::new, |n| format!("S{n}"));
        title.replace_range(range.span.clone(), &scope);
    }
    if extras {
        title = EXTRAS.replace_all(&title, " ").into_owned();
    }
    let title = strip_release_words(&title, &PACK_WORDS, matched);
    let title = strip_release_words(&title, &RELEASE_WORDS, matched);
    let alias = matched.release_title_span(&title);
    MARKED_EPISODE
        .replace_all(&title, |c: &regex::Captures| {
            let number = c.get(1).unwrap();
            let suffix = title[c.get(0).unwrap().end()..].trim_start();
            if !suffix.starts_with("- ")
                && episode.is_some()
                && number.as_str().parse::<i64>().ok() == episode
                && alias
                    .as_ref()
                    .is_none_or(|alias| !alias.contains(&number.start()))
            {
                " ".to_string()
            } else {
                c[0].to_string()
            }
        })
        .into_owned()
}

fn ambiguous_aliases(matchers: &[Matcher], media_id: i64) -> Vec<&Matcher> {
    let Some(target) = matchers.iter().find(|m| m.media_id == media_id) else {
        return vec![];
    };
    let norms: HashSet<String> = target
        .variants
        .iter()
        .map(|v| recognize::catalog_title_norm(v))
        .collect();
    let collisions: Vec<_> = matchers
        .iter()
        .filter(|m| {
            m.variants
                .iter()
                .any(|v| norms.contains(&recognize::catalog_title_norm(v)))
        })
        .collect();
    if collisions.len() < 2 {
        return vec![];
    }
    collisions
}

fn identifies_alias(ambiguous: &[&Matcher], title: &str, media_id: i64) -> bool {
    let mut hits = ambiguous
        .iter()
        .flat_map(|matched| {
            matched
                .release_alias_spans(title)
                .into_iter()
                .map(move |span| {
                    let exact = matched.alias_spelling(title, &span);
                    (matched.media_id, span, exact)
                })
        })
        .collect::<Vec<_>>();
    let mut cut = recognize::release_alias_cut(title, 0);
    while let Some(end) = hits
        .iter()
        .filter(|(_, span, exact)| exact.is_literal() && span.start < cut && span.end > cut)
        .map(|(_, span, _)| span.end)
        .max()
    {
        cut = recognize::release_alias_cut(title, end);
    }
    hits.retain(|(_, span, exact)| recognize::alias_before_episode(span, cut, exact.is_literal()));
    let mut identified = false;
    for (id, alias, exact) in &hits {
        if hits.iter().any(|(_, outer, other_exact)| {
            (outer != alias && outer.start <= alias.start && alias.end <= outer.end)
                || (outer == alias && other_exact > exact)
        }) {
            continue;
        }
        if *id != media_id {
            return false;
        }
        identified = true;
    }
    identified
}

fn strict_match<'a>(
    matchers: &'a [Matcher],
    release: &str,
    media_id: i64,
    ambiguous: &[&Matcher],
    ranges: &[EpisodeRange],
    extras: bool,
) -> Option<&'a Matcher> {
    let target = matchers.iter().find(|m| m.media_id == media_id)?;
    let title = recognize::strip_release_groups(release, matchers);
    let groups = &release[..release.len() - title.len()];
    if !ranges.is_empty()
        && recognize::match_title(matchers, release, "")
            .is_some_and(|owner| owner.media_id != media_id && owner.has_release_alias(release))
    {
        return None;
    }
    let cleaned = matching_title(title, target, ranges, extras);
    let candidates = if ambiguous.is_empty() {
        matchers
    } else {
        if !identifies_alias(ambiguous, title, media_id) {
            return None;
        }
        std::slice::from_ref(target)
    };
    let cleaned_release = format!("{groups}{cleaned}");
    let season = target.release_season(title, groups);
    let marked_title = title.replace('_', " ");
    let marked_prefix = recognize::marked_release_prefix(&marked_title);
    let match_source = if ranges.is_empty()
        && (target.release_season(&cleaned, groups) != season
            || (season.is_some()
                && marked_prefix.is_some()
                && recognize::marked_release_prefix(&cleaned).is_none()))
    {
        release
    } else {
        &cleaned_release
    };
    let matched = recognize::match_title(candidates, match_source, "")?;
    if matched.media_id != media_id {
        return None;
    }
    if matched.has_release_alias(&cleaned_release)
        || (ranges.is_empty() && matched.has_scoped_release_alias(release))
    {
        return Some(matched);
    }
    let without_episode = SEASON_EPISODE.replace_all(&cleaned, "$1");
    let (clean, alternate) = recognize::release_title_norms(&without_episode);
    let norm = recognize::catalog_title_norm(&without_episode);
    let marked = marked_prefix.unwrap_or(&without_episode);
    let prefix = recognize::marked_prefix_norm(marked);
    let season_prefix = season.map(|n| format!("{prefix} {n}"));
    let format_prefixes = recognize::format_prefix_norms(marked).collect::<Vec<_>>();
    matched
        .variants
        .iter()
        .any(|variant| {
            let variant = recognize::catalog_title_norm(variant);
            [&clean, &norm, &prefix]
                .into_iter()
                .chain(alternate.as_ref())
                .chain(season_prefix.as_ref())
                .chain(&format_prefixes)
                .any(|candidate| recognize::release_norm_matches(candidate, &variant))
        })
        .then_some(matched)
}

fn item_key(item: &RawItem) -> String {
    item.info_hash
        .as_deref()
        .and_then(rss::info_hash_aliases)
        .map(|[hex, _]| hex)
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

pub fn torrent_item(raw: RawItem) -> TorrentItem {
    TorrentItem {
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
    }
}

pub fn search_results(items: Vec<RawItem>, seen: &HashSet<String>) -> Vec<TorrentItem> {
    items
        .into_iter()
        .map(|raw| TorrentItem {
            seen: was_seen(&raw, seen),
            ..torrent_item(raw)
        })
        .collect()
}

pub fn stable_guid(item: &RawItem) -> String {
    format!("nyaa-show\u{1}{}", item_key(item))
}

pub(crate) fn seen_guid_aliases(guid: &str) -> Vec<String> {
    match guid
        .strip_prefix("nyaa-show\u{1}")
        .and_then(rss::info_hash_aliases)
    {
        Some(hashes) => {
            let mut aliases: Vec<_> = hashes.map(|hash| format!("nyaa-show\u{1}{hash}")).into();
            if !aliases.iter().any(|alias| alias == guid) {
                aliases.push(guid.to_string());
            }
            aliases
        }
        None => vec![guid.to_string()],
    }
}

fn guid_was_seen(guid: &str, seen: &HashSet<String>) -> bool {
    seen.contains(guid)
        || seen_guid_aliases(guid)
            .iter()
            .any(|alias| seen.contains(alias))
}

pub fn was_seen(item: &RawItem, seen: &HashSet<String>) -> bool {
    seen.contains(&item.guid) || guid_was_seen(&stable_guid(item), seen)
}

pub fn restore_seen(result: &mut ShowTorrents, seen: &HashSet<String>) {
    for item in result
        .batches
        .iter_mut()
        .chain(result.episodes.iter_mut())
        .chain(result.other.iter_mut())
    {
        item.seen = guid_was_seen(&item.guid, seen);
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
    let ambiguous = ambiguous_aliases(matchers, entry.media_id);
    let mut seen = HashSet::new();
    for raw in items {
        let folded = recognize::fold_digits(&raw.title);
        if target.is_season_pack(&folded) {
            continue;
        }
        let title = recognize::strip_release_groups(&folded, matchers);
        let groups = &folded[..folded.len() - title.len()];
        let groups = recognize::mask_release_format(&groups.replace('_', " "), true);
        if recognize::has_cross_season_range(&groups, groups.len())
            || recognize::leading_release_extra(&groups, format)
            || recognize::leading_fractional_release(&groups)
        {
            continue;
        }
        let span = target.release_title_span_in(&folded, title);
        let (masked, invalid_season_range) = recognize::episode_remainder(title, span);
        if recognize::has_cross_season_range(&masked, 0) {
            continue;
        }
        let mut ranges = episode_ranges(&masked);
        let mut conflicting_scope = false;
        for range in &mut ranges {
            if let Some(season) = recognize::preceding_season(&title[..range.span.start]) {
                conflicting_scope |= range.season.is_some_and(|scope| scope != season);
                range.season = Some(season);
            }
        }
        if conflicting_scope {
            continue;
        }
        if ranges
            .iter()
            .any(|r| r.fractional_end && total.is_some_and(|t| r.last >= t))
        {
            continue;
        }
        ranges.retain(|r| !r.fractional_end);
        if ranges.is_empty()
            && (recognize::has_fractional_episode(&masked)
                || recognize::has_conflicting_episode_markers(&masked, title))
        {
            continue;
        }
        let extras = total != Some(1)
            && EXTRAS.is_match(&masked)
            && (ranges
                .iter()
                .any(|r| r.first == 1 && total.is_some_and(|t| r.last == t))
                || (PACK_WORDS.is_match(&masked)
                    && recognize::resolve_episode(target, &[&folded]).is_none()));
        let metadata = if extras {
            EXTRAS.replace_all(&masked, " ").into_owned()
        } else {
            masked
        };
        if recognize::has_release_extra(&metadata, format, total == Some(1)) {
            continue;
        }
        let Some(matched) = strict_match(
            matchers,
            &folded,
            entry.media_id,
            &ambiguous,
            &ranges,
            extras,
        ) else {
            continue;
        };
        if !seen.insert(item_key(&raw)) {
            continue;
        }
        let episode = if ranges.is_empty() {
            recognize::resolve_episode(matched, &[&folded])
        } else {
            None
        };
        let leading_pack = if episode.is_none() && !groups.is_empty() {
            recognize::leading_release_tags(&groups, matched.release_season(title, "")).pack_matches
        } else {
            None
        };
        if leading_pack == Some(false) {
            continue;
        }
        if episode.is_none() {
            ranges.extend(
                episode_ranges(&groups)
                    .into_iter()
                    .filter(|r| recognize::bracketed_range(&groups, r.span.clone())),
            );
            if ranges.is_empty() && recognize::has_conflicting_episode_markers(&groups, &groups) {
                continue;
            }
        }
        if (invalid_season_range && episode.is_none())
            || episode.is_some_and(|ep| ep < 1 || total.is_some_and(|t| ep > t))
        {
            continue;
        }
        let mut seasons = ranges.iter().filter_map(|range| range.season);
        if seasons
            .next()
            .is_some_and(|first| seasons.any(|season| season != first))
            || ranges.iter().any(|r| total.is_some_and(|t| r.last > t))
        {
            continue;
        }
        let batch = episode.is_none()
            && if ranges.is_empty() {
                leading_pack == Some(true)
                    || PACK_WORDS.is_match(&metadata)
                    || (finished && total.is_some_and(|t| t > 1))
            } else {
                ranges.iter().any(|r| {
                    result
                        .next_episode
                        .is_none_or(|next| r.first <= next && next <= r.last)
                })
            };
        let item = TorrentItem {
            guid: stable_guid(&raw),
            media_id: Some(matched.media_id),
            matched: Some(matched.display.clone()),
            episode,
            is_new: false,
            seen: false,
            ..torrent_item(raw)
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
        matchers_for(&[entry])
    }

    fn matchers_for(entries: &[&ListEntry]) -> Vec<Matcher> {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for entry in entries {
            db.upsert_media(entry.media.as_ref().unwrap()).unwrap();
            db.upsert_entry(entry).unwrap();
        }
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

    fn named_entry(id: i64, title: &str) -> ListEntry {
        let mut entry = entry();
        entry.media_id = id;
        let media = entry.media.as_mut().unwrap();
        media.id = id;
        media.title_english = Some(title.into());
        media.title_romaji = None;
        entry
    }

    #[test]
    fn scan_playback_feed_and_torrents_agree_on_episode_boundaries() {
        use crate::library::{resolve_bound_episode, LibraryBinding, ScanCache};
        use std::{collections::HashMap, fs, sync::Arc};

        let base = named_entry(1, "Some Show");
        let second = named_entry(2, "Some Show Season 2");
        let final_season = named_entry(3, "Some Show Final Season");
        let numeric = named_entry(4, "86");
        let entries = [&base, &second, &final_season, &numeric];
        let matchers = Arc::new(matchers_for(&entries));

        for (index, (release, entry, episode, batch)) in [
            (
                "[A] Some_Show_Final_Season_05 [1080p]",
                &final_season,
                Some(5),
                false,
            ),
            (
                "[A] Some Show Season 2-10 [1080p]",
                &second,
                Some(10),
                false,
            ),
            ("[A] Some Show Season 2-05 [1080p]", &second, Some(5), false),
            ("[A] Some Show Season 01-02", &base, None, false),
            ("[A] Some Show Season 1~2", &base, None, false),
            ("[A] Some Show Season 1–2", &base, None, false),
            ("[A] Some Show Season 1-2-3", &base, None, false),
            ("[A] Some Show Seasons 1-2", &base, None, false),
            ("[A] Some Show Season 1 - 2 Batch", &base, None, false),
            (
                "[A] Some Show Season 1 - 2 [1080p] [Batch]",
                &base,
                None,
                false,
            ),
            ("[A] Some Show Seasons 1, 2", &base, None, false),
            ("[A] Some Show Season 1 - 02", &base, Some(2), false),
            ("[A] Some Show S01.E05 S02.E05", &base, None, false),
            ("[A] Some Show S01.E.05 S02.E.05", &base, None, false),
            ("[A] Some Show [S02.E05] [S01.E05]", &second, None, false),
            ("[A] Some Show S01.E05 E05", &base, Some(5), false),
            ("[A] Some Show S02.E05 S2.E05v2", &second, Some(5), false),
            (
                "[A] Some Show [Season 2 BD] - 04 (2)",
                &second,
                Some(4),
                false,
            ),
            (
                "[A] Some Show [Season 2 10-bit] - 04",
                &second,
                Some(4),
                false,
            ),
            (
                "[A] Some Show [Season 2 (10-bit)] - 04 (2)",
                &second,
                Some(4),
                false,
            ),
            ("Some Show 2024-02-29 - E05", &base, Some(5), false),
            ("Some Show 2026-04-05 OVA 01", &base, None, false),
            ("Some Show 2026-04-05 - 01-12", &base, None, true),
            ("Some Show 5.1ch - 01-12", &base, None, true),
            ("Some Show 23.976fps - 05", &base, Some(5), false),
            ("Some Show S01-S02 - 01", &base, None, false),
            ("Some Show S01-02 - 01", &base, None, false),
            ("Some Show Season 1 - Season 2 - 01", &base, None, false),
            ("Some Show Season 1-2 - 01", &base, None, false),
            ("Some Show Seasons 1-2 - 01", &base, None, false),
            ("Some Show S01-S02 - 2024 [BD]", &base, None, false),
            ("Some Show S02-01 E05", &second, Some(5), false),
            ("Some Show [Season 2 - 03]", &second, Some(3), false),
            ("Some Show [Season 2 - 12]", &second, Some(12), false),
            ("[Season 2] Some Show 01-12", &second, None, true),
            ("[S02] Some Show E01-E12", &second, None, true),
            ("[Season 2] Some Show - 05", &second, Some(5), false),
            ("[2nd Season] Some Show - 05", &second, Some(5), false),
            ("[S02] Some Show - 05", &second, Some(5), false),
            ("[Group] 【S02E05】 Some Show", &second, Some(5), false),
            ("[Season 2 E05] Some Show", &second, Some(5), false),
            ("[Season 2 - 05] Some Show", &second, Some(5), false),
            ("[Season 2 - E05] Some Show", &second, Some(5), false),
            ("[E05] Some Show", &base, Some(5), false),
            ("[Ep05] Some Show", &base, Some(5), false),
            ("[#05] Some Show", &base, Some(5), false),
            ("[S02E05] Some Show - 06", &second, Some(6), false),
            ("[Season 2] Some Show S01E05", &base, Some(5), false),
            ("[S02E05] Some Show S01E06", &base, Some(6), false),
            ("[S02E05] Some Show [Season 1] - 06", &base, Some(6), false),
            (
                "[Season 1] Some Show E05 [Season 2]",
                &second,
                Some(5),
                false,
            ),
            (
                "[Season 2] Some Show - 05 - Season 1 Summary",
                &second,
                Some(5),
                false,
            ),
            ("[S02E05 1080p] Some Show", &second, Some(5), false),
            ("[S02E05 (10-bit)] Some Show", &second, Some(5), false),
            ("[S02E05 Dual Audio] Some Show", &second, Some(5), false),
            ("Some Show [S02E05 Dual Audio]", &second, Some(5), false),
            (
                "Some Show [Season 2 - 05 Dual Audio]",
                &second,
                Some(5),
                false,
            ),
            (
                "[Season 2 (10-bit)] Some Show - 05",
                &second,
                Some(5),
                false,
            ),
            (
                "[Season 2 Dual Audio] Some Show - 05",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show - 05 [Season 2 (10-bit)]",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show - 05 [Season 2 Dual Audio]",
                &second,
                Some(5),
                false,
            ),
            ("[Season 2 - 2024] Some Show", &second, None, true),
            ("[Season 2 - 1080] Some Show", &second, None, true),
            ("Some Show [Season 2 - 2024] - 05", &second, Some(5), false),
            ("Some Show [Season 2 – 1080] - 05", &second, Some(5), false),
            ("Some Show [Season 2 - 720] - 05", &second, Some(5), false),
            ("Some Show [Season 2 - 576] - 05", &second, Some(5), false),
            ("Some Show [Batch Season 2] - 05", &second, Some(5), false),
            (
                "Some Show [Season 2 - 1080p 2024] - 05",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show [Season 2 - 10-bit 2024] - 05",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show [Season 2 - 5.1 2024] - 05",
                &second,
                Some(5),
                false,
            ),
            ("Some Show [Season 2 - 1080p - 2024]", &second, None, true),
            (
                "Some Show [Season 2 - 2024] OVA - Behind the Scenes",
                &second,
                None,
                false,
            ),
            ("Some Show [Season 2 — 05v2]", &second, Some(5), false),
            ("[Season 2 OVA] Some Show - 01", &base, None, false),
            ("[S02 OVA] Some Show - 01", &base, None, false),
            ("[Season 2 E05 Preview] Some Show - 05", &base, None, false),
            ("[Season 2 E05.5] Some Show - 06", &base, None, false),
            ("[Season 2 - 05.5] Some Show", &base, None, false),
            ("[S02 E05.5] Some Show", &base, None, false),
            ("[Season 2 01-24] Some Show S02", &second, None, false),
            ("[S02 01-24] Some Show S02", &second, None, false),
            ("[Season 2 01-24] Some Show", &second, None, false),
            ("[S02 01-24] Some Show", &second, None, false),
            ("[Season 2 Batch] Some Show", &second, None, true),
            ("[Batch S02] Some Show", &second, None, true),
            (
                "[Season 2 E05][Season 2 E06] Some Show",
                &second,
                None,
                false,
            ),
            (
                "Some Show [Season 2 E05][Season 3 E05]",
                &second,
                None,
                false,
            ),
            ("[Season 2 - 05][S02E06] Some Show", &second, None, false),
            ("[Season 2 E05][S2E05v2] Some Show", &second, Some(5), false),
            ("Some Show [Season 2 - 05][S02E05]", &second, Some(5), false),
            ("[Season 2 01-12] Some Show", &second, None, true),
            ("[Season 2 01-12] Some Show - 06", &second, None, true),
            ("[S02E01-E12] Some Show", &second, None, true),
            ("Some Show [S02E05][S03E06]", &second, None, false),
            ("Some Show [S02E05][S03E05]", &second, None, false),
            ("Some Show [S02E05][S02E06]", &second, None, false),
            ("Some Show S02E05E06", &second, None, false),
            ("Some Show S02E05E05v2", &second, Some(5), false),
            (
                "Some Show Season 2 E05 + Season 3 E05",
                &second,
                None,
                false,
            ),
            (
                "Some Show Season 2 E05 + 2nd Season E05",
                &second,
                Some(5),
                false,
            ),
            ("Some Show E05 and E06", &base, None, false),
            ("Some Show E05 E06", &base, None, false),
            ("Some Show [E05 and E06] - 05", &base, None, false),
            ("[S02E05][S03E06] Some Show", &second, None, false),
            ("[S02E05][S2E05v2] Some Show", &second, Some(5), false),
            ("Some Show [S02E05][S2E05v2]", &second, Some(5), false),
            ("Some Show [S02E05][E05]", &second, Some(5), false),
            ("Some Show E05 E05v2", &base, Some(5), false),
            ("Some Show E05 and E05", &base, Some(5), false),
            ("86 E05 and E06", &numeric, None, false),
            ("86 E05 - The 86th Sector", &numeric, Some(5), false),
            ("Some Show E05 - A Special Episode 3", &base, Some(5), false),
            ("[Group Season 2 OVA] Some Show - 05", &base, Some(5), false),
            ("[Fansubs 01-24] Some Show - 05", &base, Some(5), false),
            (
                "Some Show S02E05 [NCOP Dual Audio] - Dawn",
                &second,
                None,
                false,
            ),
            ("[S03E05] Some Show S02", &second, None, true),
            ("[S99E05] Some Show S02", &second, None, true),
            ("[S03E05] Some Show [Season 2]", &second, None, true),
            ("[S02] [S03E05] Some Show", &second, None, true),
            ("[S03] [E05] Some Show S02", &second, None, true),
            ("[S99] [E05] Some Show S02", &second, None, true),
            ("[Season 99] [E05] Some Show S02", &second, None, true),
            ("[S02] [S03] [E05] Some Show S02", &second, None, true),
            ("[E05] [S03] Some Show S02", &second, None, true),
            ("[E05] Some Show S02", &second, Some(5), false),
            ("[S03] [E05] Some Show S02 - 06", &second, Some(6), false),
            ("Some Show [Fansubs Season 2] - 05", &base, Some(5), false),
            ("Some Show [Group S02E09] - 05", &base, Some(5), false),
            ("[S02] [E05] Some Show", &second, Some(5), false),
            ("[S02] [S02E05] Some Show", &second, Some(5), false),
            ("[S03E06] Some Show S02 - 05", &second, Some(5), false),
            ("Some.Show.S02E05.The.Target.1080p", &second, Some(5), false),
            ("Some Show Season 2 – 05", &second, Some(5), false),
            ("Some Show Season 2 — 01", &second, Some(1), false),
            ("Some Show Season 1—2", &base, None, false),
            ("Some Show [Season 1 – 2] - 05", &base, None, false),
            (
                "Some Show [Season 2 E05] - 12.5 Seconds",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show [Season 2 - 05] - 12.5 Seconds",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show 【2nd Season E05】 - 12.5 Seconds",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show [2nd Season - 05] - 12.5 Seconds",
                &second,
                Some(5),
                false,
            ),
            (
                "Some Show [Season 2 - E05] - 12.5 Seconds",
                &second,
                Some(5),
                false,
            ),
            ("Some Show [Season 2 E05.5]", &second, None, false),
            ("Some Show [Season 2 - 05.5]", &second, None, false),
            ("Some Show [Season 2 E05] [12.5]", &second, None, false),
            (
                "Some Show [Season 1 - 2 Dual Audio] - 05",
                &base,
                None,
                false,
            ),
            ("Some Show [Season 1 - 2 | 1080p] - 05", &base, None, false),
            ("Some Show [Season 1 - 2 - BD] - 05", &base, None, false),
            ("Some.Show.2026.04.05.05.1080p", &base, Some(5), false),
            ("Some_Show_2026_04_05_-_05", &base, Some(5), false),
            ("Some Show 23.976 fps - 05", &base, Some(5), false),
            ("Some Show 1.5GB OVA 01", &base, None, false),
            ("Some Show 6.1ch - 01-12", &base, None, true),
            ("Some.Show.01-12.2024.1080p", &base, None, true),
            ("Some.Show.S01E01-E12.2024.1080p", &base, None, true),
            ("Some.Show.S01E05.5.2024.1080p", &base, None, false),
            ("Some.Show.01-12.5.1080p", &base, None, false),
            ("Some Show 01-12.5 END", &base, None, false),
            ("Some Show - 05.25", &base, None, false),
            ("Some Show S01E05.75", &base, None, false),
            ("Some Show - 05 [OVAs]", &base, None, false),
            ("Some Show - 05 [CMs]", &base, None, false),
            ("Some Show 01-12 [BD + PVs]", &base, None, true),
            ("[01-12] Some Show", &base, None, true),
            ("[05.5] Some Show - 05", &base, None, false),
            ("[Group 1.5] Some Show - 05", &base, Some(5), false),
            ("Some Show DTS5.1 - 05", &base, Some(5), false),
            ("Some Show TrueHD7.1 - 05", &base, Some(5), false),
            ("Some Show LPCM2.0 - 05", &base, Some(5), false),
            ("Some Show HEVC10bit - 05", &base, Some(5), false),
            ("Some Show 576p - 05", &base, Some(5), false),
            ("Some Show 1280x720p - 05", &base, Some(5), false),
            ("Some Show S02-01", &second, None, false),
            ("Some Show S02-02", &second, None, false),
            ("Some Show Season 2-1", &second, None, false),
            ("Some Show S01-2024 - 05", &base, Some(5), false),
            ("Some Show S02-01 - 05", &second, Some(5), false),
            ("Some Show [Season 2 - 01]", &second, Some(1), false),
            ("Some Show [Season 1 - 2 [BD]] - 05", &base, None, false),
            ("Some Show [Season 1 - 2 Complete]", &base, None, false),
            ("Some.Show.2024.01-12.1080p", &base, None, true),
            ("Some.Show.2024.05.5.1080p", &base, None, false),
            ("Some.Show.2024.05.1080p", &base, Some(5), false),
            ("Some.Show.S01E05-E24.1080", &base, None, false),
            ("Some.Show.S01E05.5.1080", &base, None, false),
            ("[Group S02E07] Some Show - 05", &base, Some(5), false),
            ("[Fansubs S01-S02] Some Show - 05", &base, Some(5), false),
            (
                "[Fansubs S01E01-S02E05] Some Show - 05",
                &base,
                Some(5),
                false,
            ),
            (
                "[Fansubs S01-S02] Some Show S02E05",
                &second,
                Some(5),
                false,
            ),
            (
                "[Fansubs S01-S02] Some Show [Season 1 - 2]",
                &base,
                None,
                false,
            ),
            (
                "Some Show [05] - A Special Episode 3",
                &base,
                Some(5),
                false,
            ),
            ("Some Show [05] - 07.5 Seconds", &base, Some(5), false),
            ("Some Show [05] - Countdown 1-2-3", &base, Some(5), false),
            ("Some Show E05 - Countdown 1-2-3", &base, Some(5), false),
            ("[05] Some Show OVA 01", &base, None, false),
            ("Some Show [05] [01-12] [BD]", &base, None, true),
            ("Some Show [05] [05.5]", &base, None, false),
            ("[S01E01-S02E05] Some Show - 05", &base, None, false),
            ("[S01-S02] Some Show - 05", &base, None, false),
            ("【Group Season 2】 Some Show - 05", &base, Some(5), false),
            (
                "Some Show Uncensored - 05 - A Special Episode 3",
                &base,
                Some(5),
                false,
            ),
            ("Some Show - 05 [1920x1080 NCOP01v2]", &base, None, false),
            ("Some Show - 05 [PVs]", &base, None, false),
            ("Some Show - 05 [Menus]", &base, None, false),
            ("Some Show [Season 2 - E05]", &second, Some(5), false),
            ("Some Show [EP-05]", &base, Some(5), false),
            ("Some Show - 2024 - 05 (2)", &base, Some(5), false),
            (
                "Some Show yuv420p10le H-264 - 05 24bit",
                &base,
                Some(5),
                false,
            ),
            ("Some.Show.2024.E05.5.1080p", &base, None, false),
            ("[A] Some Show 2024 OVA 01", &base, None, false),
            ("[A] Some Show 2024 01-12 [1080p]", &base, None, true),
            ("[A] Some Show 2024 - 05.5 [1080p]", &base, None, false),
            ("[A] Some Show 2024 - 12.5 [1080p]", &base, None, false),
            ("[A] Some Show 2024 - 05 [1080p]", &base, Some(5), false),
            ("[A] Some_Show_S02E05_10bit", &second, Some(5), false),
            ("[A] Some_Show_S02_BD_1080p", &second, None, true),
            ("86_05_1080p", &numeric, Some(5), false),
            ("[A] Some_Show_05_1080p", &base, Some(5), false),
            (
                "[A] Some Show Final Season 05-08 [1080p]",
                &final_season,
                None,
                true,
            ),
            (
                "[A] Some Show [Season 2 E05] [1080p]",
                &second,
                Some(5),
                false,
            ),
            (
                "[A] Some Show [Season 2 Preview] - 05",
                &second,
                None,
                false,
            ),
            ("[A] Some Show [Season 2 OVA] - 01", &second, None, false),
            (
                "[A] Some Show [Season 2 E05 Preview] - 05",
                &second,
                None,
                false,
            ),
            (
                "[A] Some Show [Season 2 E05 [1080p]]",
                &second,
                Some(5),
                false,
            ),
            ("[A] Some Show [BD Season 1 - 2] - 05", &base, None, false),
            ("[A] Some Show - 05 [1080p_Sample]", &base, None, false),
            ("[A] Some Show - 05 [NCOP AAC2.0]", &base, None, false),
            ("[A] Some Show 10-bit - 05", &base, Some(5), false),
            ("[A] Some Show 10-bit OVA 01", &base, None, false),
            ("[A] Some Show 2.0 OVA 01", &base, None, false),
            ("[A] Some Show 5.1 - 01-12", &base, None, true),
            ("[A] Some Show 2024-E01-E12", &base, None, true),
            ("[A] Some Show 2024-01~12", &base, None, true),
            ("[A] Some Show - 2024 - 12.5", &base, None, false),
            ("Some.Show.S01E01-E12.1080p", &base, None, true),
            ("Some.Show.01-12.1080p", &base, None, true),
            ("Some.Show.S01E05-E24.1080p", &base, None, false),
            ("Some.Show.05.5.1080p", &base, None, false),
            ("Some.Show.S01E05.1080p", &base, Some(5), false),
            ("[A] Some Show S01-02", &base, None, false),
            ("[A] Some Show [Season 1 - 2]", &base, None, false),
            ("[A] Some Show [ Season 1 - 2 1080p ]", &base, None, false),
            ("[A] Some Show [S01 E10-S02 E03]", &base, None, false),
            ("[A] Some Show Season 1 - Season 2", &base, None, false),
            ("[A] Some Show S01+S02", &base, None, false),
            ("[A] Some Show S2 - 05", &second, Some(5), false),
            ("[A] Some Show [#05] [1080p]", &base, Some(5), false),
            ("[A] Some Show [S02E05] [1080p]", &second, Some(5), false),
            ("[A] 【Some Show】 - 05 [1080p]", &base, Some(5), false),
            ("Some.Show.05.DDP2.0.1080p", &base, Some(5), false),
            ("Some Show Hi10P - 05 AC3", &base, Some(5), false),
            ("Some Show - 05 8bit yuv420p10", &base, Some(5), false),
            ("[A] Some Show - 05 [Menu]", &base, None, false),
        ]
        .into_iter()
        .enumerate()
        {
            let (matched, feed_episode) = match_feed(&matchers, release);
            assert_eq!(feed_episode, episode, "feed: {release}");
            if episode.is_some() || batch {
                assert_eq!(
                    matched.map(|m| m.media_id),
                    Some(entry.media_id),
                    "feed: {release}"
                );
            }
            if let Some(matched) = matched {
                assert_eq!(
                    resolve_bound_episode(matched, &[release], None),
                    episode,
                    "playback: {release}"
                );
            }

            let folder = std::env::temp_dir().join(format!(
                "kurisu-release-test-{}-{index}",
                std::process::id()
            ));
            fs::create_dir(&folder).unwrap();
            // Windows reserves ASCII pipes in filenames.
            let file_stem = release.replace('|', "·");
            fs::write(folder.join(format!("{file_stem}.mkv")), []).unwrap();
            let root = folder.to_string_lossy().into_owned();
            let mut cache = ScanCache::default();
            let scan = cache.scan(std::slice::from_ref(&root), &matchers, &HashMap::new());
            let binding = LibraryBinding {
                media_id: entry.media_id,
                episode_offset: 0,
            };
            let bound = cache.scan(
                std::slice::from_ref(&root),
                &matchers,
                &HashMap::from([(root.clone(), binding)]),
            );
            fs::remove_dir_all(folder).unwrap();
            assert_eq!(scan.files[0].episode, episode, "scan: {release}");
            assert_eq!(bound.files[0].episode, episode, "bound scan: {release}");
            if episode.is_some() || batch {
                assert_eq!(
                    scan.files[0].media_id,
                    Some(entry.media_id),
                    "scan: {release}"
                );
            }
            assert_eq!(
                bound.files[0].media_id,
                Some(entry.media_id),
                "bound scan: {release}"
            );

            let torrents = classify(entry, &matchers, vec![raw(release, 100)]);
            assert_eq!(
                torrents.batches.len(),
                usize::from(batch),
                "torrents: {release}"
            );
            let listed = torrents
                .episodes
                .iter()
                .chain(&torrents.other)
                .map(|item| item.episode)
                .collect::<Vec<_>>();
            assert_eq!(
                listed,
                episode.map(Some).into_iter().collect::<Vec<_>>(),
                "torrents: {release}"
            );
        }
    }

    #[test]
    fn literal_season_titles_keep_episode_batches_while_airing() {
        for (series, title) in [
            ("Some Show", "Some Show Final Season"),
            ("Monster", "Monster Season"),
        ] {
            let base = named_entry(1, series);
            let mut entry = named_entry(2, title);
            let media = entry.media.as_mut().unwrap();
            media.episodes = Some(60);
            media.status = Some("RELEASING".into());
            let matchers = matchers_for(&[&base, &entry]);
            for suffix in ["01-12", "01-16", "1~12", "1-6", "05-08", "01-60", "01~60"] {
                let release = format!("[A] {title} {suffix} [1080p]");
                let (matched, episode) = match_feed(&matchers, &release);
                assert_eq!(matched.map(|m| m.media_id), Some(2), "{release}");
                assert_eq!(episode, None, "{release}");
                let torrents = classify(&entry, &matchers, vec![raw(&release, 100)]);
                assert_eq!(torrents.batches.len(), 1, "{release}");
            }
            let release = format!("[A] {title} Seasons 1-2 [1080p]");
            assert!(classify(&entry, &matchers, vec![raw(&release, 100)])
                .batches
                .is_empty());
        }
    }

    #[test]
    fn exact_aliases_own_punctuation_and_parenthetical_years() {
        for (base, sequel) in [
            ("Gintama", "Gintama."),
            ("Hunter x Hunter", "Hunter x Hunter (2011)"),
        ] {
            let first = named_entry(1, base);
            let mut second = named_entry(2, sequel);
            second.status = "COMPLETED".into();
            let matchers = matchers_for(&[&first, &second]);
            for (title, owner, other) in [(base, &first, &second), (sequel, &second, &first)] {
                let release = format!("[G] {title} - 05 [1080p]");
                let (matched, episode) = match_feed(&matchers, &release);
                assert_eq!(
                    matched.map(|m| m.media_id),
                    Some(owner.media_id),
                    "{release}"
                );
                assert_eq!(episode, Some(5), "{release}");
                assert_eq!(
                    classify(owner, &matchers, vec![raw(&release, 100)])
                        .episodes
                        .len(),
                    1,
                    "{release}"
                );
                assert!(
                    classify(other, &matchers, vec![raw(&release, 100)])
                        .episodes
                        .is_empty(),
                    "{release}"
                );
            }
        }
    }

    #[test]
    fn alias_ownership_stops_before_the_episode_subtitle() {
        let plain = named_entry(1, "Gintama");
        let punctuated = named_entry(2, "Gintama.");
        let spaced = named_entry(3, "No. 6");
        let mut compact = named_entry(4, "No.6");
        compact.media.as_mut().unwrap().title_romaji = Some("Number Six".into());
        let dash = named_entry(5, "İtem - Zero");
        let colon = named_entry(6, "İtem: Zero");
        let entries = [&plain, &punctuated, &spaced, &compact, &dash, &colon];
        let matchers = matchers_for(&entries);
        let mut cases = vec![
            ("Gintama - 05 - Gintama. Forever".to_owned(), 1),
            ("Gintama. - 05 - Gintama Forever".to_owned(), 2),
            ("Gintama. S01E05 - Gintama Forever".to_owned(), 2),
            ("Gintama. [E05] Gintama Forever".to_owned(), 2),
            ("No.6 - 05 - No. 6".to_owned(), 4),
            ("İtem - Zero - 05".to_owned(), 5),
            ("İtem: Zero - 05".to_owned(), 6),
        ];
        cases.extend(
            ["/", "／", "|", "｜", ":", "："]
                .map(|separator| (format!("No.6{separator}Number Six - 05"), 4)),
        );
        for (title, owner) in cases {
            let release = format!("[Group] {title} [1080p]");
            let (matched, episode) = match_feed(&matchers, &release);
            assert_eq!(matched.map(|m| m.media_id), Some(owner), "{release}");
            assert_eq!(episode, Some(5), "{release}");
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(&release, 100)]);
                assert_eq!(
                    result.episodes.len(),
                    usize::from(entry.media_id == owner),
                    "{release}"
                );
                assert!(
                    result.batches.is_empty() && result.other.is_empty(),
                    "{release}"
                );
            }
        }
        for release in ["Gintama. | Gintama - 05", "No.6／No. 6 - 05"] {
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert!(
                    result.batches.is_empty()
                        && result.episodes.is_empty()
                        && result.other.is_empty()
                );
            }
        }
    }

    #[test]
    fn catalog_episode_like_numbers_belong_to_the_title() {
        for (literal, other) in [
            ("Show - 91", "Show 91"),
            ("Show #1", "Show 1"),
            ("Show Episode 1", "Show"),
            ("Show (2)", "Show"),
            ("Show [2]", "Show"),
        ] {
            let mut owner = named_entry(2, literal);
            owner.media.as_mut().unwrap().title_romaji = Some("Alternate Title".into());
            let other = named_entry(1, other);
            let matchers = matchers_for(&[&other, &owner]);
            for (release, expected) in [
                (format!("{literal}.mkv"), None),
                (format!("[Group] {literal} - 05 [1080p]"), Some(5)),
                (format!("[Group] {literal} E05 [1080p]"), Some(5)),
                (format!("[Group] {literal} - 05 - Dawn [1080p]"), Some(5)),
                (format!("{literal} / Alternate Title - 05"), Some(5)),
                (format!("{}_-_05", literal.replace(' ', "_")), Some(5)),
                (format!("{}.-.05", literal.replace(' ', ".")), Some(5)),
            ] {
                let (matched, episode) = match_feed(&matchers, &release);
                assert_eq!(matched.map(|m| m.media_id), Some(2), "{release}");
                assert_eq!(episode, expected, "{release}");
                if expected.is_some() {
                    assert_eq!(
                        classify(&owner, &matchers, vec![raw(&release, 100)])
                            .episodes
                            .len(),
                        1,
                        "{release}"
                    );
                    assert!(
                        classify(&other, &matchers, vec![raw(&release, 100)])
                            .episodes
                            .is_empty(),
                        "{release}"
                    );
                }
            }
        }
        let base = named_entry(1, "Show - 91");
        let sequel = named_entry(2, "Show - 91 Season 2");
        let matchers = matchers_for(&[&base, &sequel]);
        let release = "Show - 91 - Season 2 - 05 - The Target";
        let (matched, episode) = match_feed(&matchers, release);
        assert_eq!(matched.map(|m| m.media_id), Some(2));
        assert_eq!(episode, Some(5));
        assert_eq!(
            classify(&sequel, &matchers, vec![raw(release, 100)])
                .episodes
                .len(),
            1
        );

        let plain = named_entry(3, "Show 91");
        let matchers = matchers_for(&[&base, &plain]);
        for (release, expected) in [
            ("Show - 91 / Show 91 - 05", 0),
            ("Show - 91 - 05 - Show 91", 1),
        ] {
            assert_eq!(
                classify(&base, &matchers, vec![raw(release, 100)])
                    .episodes
                    .len(),
                expected,
                "{release}"
            );
        }
    }

    #[test]
    fn scoped_episodes_keep_catalog_stems_across_release_styles() {
        for (stem, sequel) in [
            ("Mobile Suit Gundam 00", "Mobile Suit Gundam 00 Season 2"),
            ("Steins;Gate 0", "Steins;Gate 0 Season 2"),
            ("Kaiju No. 8", "Kaiju No. 8 Season 2"),
            ("No. 6", "No. 6 Season 2"),
            ("The Special", "The Special Season 2"),
            (
                "2.5 Dimensional Seduction",
                "2.5 Dimensional Seduction Season 2",
            ),
            ("Mob Psycho 100", "Mob Psycho 100 II"),
            ("Some Show", "Some Show Season 2"),
        ] {
            let base = named_entry(1, stem);
            let sequel = named_entry(2, sequel);
            let matchers = matchers_for(&[&base, &sequel]);
            for (release, expected, batch) in [
                (format!("[Season 2] {stem} - 05 [1080p]"), Some(5), false),
                (format!("[S02E05] {stem} [1080p]"), Some(5), false),
                (
                    format!("[Season 2] {stem} - 05 The Target [1080p]"),
                    Some(5),
                    false,
                ),
                (
                    format!("[Season 2] {stem} - 05 – The Target [1080p]"),
                    Some(5),
                    false,
                ),
                (
                    format!("[S02] {stem} E05 The Target [1080p]"),
                    Some(5),
                    false,
                ),
                (format!("{stem} - 05 The Target [Season 2]"), Some(5), false),
                (
                    format!("{stem} S02E05 - The Target [1080p]"),
                    Some(5),
                    false,
                ),
                (format!("{stem} S02 - 05 - The Target"), Some(5), false),
                (
                    format!("{stem} 2nd Season - 05 - The Target"),
                    Some(5),
                    false,
                ),
                (format!("{stem} Season 2 - 05 - The Target"), Some(5), false),
                (format!("[Season 2] {stem} 01-12 [1080p]"), None, true),
            ] {
                let (matched, episode) = match_feed(&matchers, &release);
                assert_eq!(matched.map(|m| m.media_id), Some(2), "{release}");
                assert_eq!(episode, expected, "{release}");
                let torrents = classify(&sequel, &matchers, vec![raw(&release, 100)]);
                assert_eq!(
                    torrents.episodes.len(),
                    usize::from(expected.is_some()),
                    "{release}"
                );
                assert_eq!(torrents.batches.len(), usize::from(batch), "{release}");
                let unrelated = classify(&base, &matchers, vec![raw(&release, 100)]);
                assert!(
                    unrelated.episodes.is_empty() && unrelated.batches.is_empty(),
                    "{release}"
                );
            }
        }
    }

    #[test]
    fn scene_subtitles_keep_season_stems_and_localized_sequels() {
        let base = named_entry(1, "Watashi no Anime");
        let mut sequel = named_entry(2, "My Anime 2");
        sequel.media.as_mut().unwrap().title_romaji = Some("Watashi no Anime 2".into());
        let special = named_entry(3, "The Special");
        let special_sequel = named_entry(4, "The Special Season 2");
        let standalone = named_entry(5, "Golgo 13");
        let entries = [&base, &sequel, &special, &special_sequel, &standalone];
        let matchers = matchers_for(&entries);
        for (release, owner) in [
            ("Watashi.no.Anime.S02E05.The.Target.1080p", 2),
            ("Watashi no Anime S01E05 - The Target", 1),
            ("My Anime S02E05 - The Target", 2),
            ("The Special S02E05 - The Target", 4),
            ("The Special S02E05 - OVA Title", 4),
            ("Golgo 13 S01E05 - The Target", 5),
        ] {
            let (matched, episode) = match_feed(&matchers, release);
            assert_eq!(matched.map(|m| m.media_id), Some(owner), "{release}");
            assert_eq!(episode, Some(5), "{release}");
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert_eq!(
                    result.episodes.len(),
                    usize::from(entry.media_id == owner),
                    "{release}"
                );
            }
        }
        for release in [
            "Another.The.Special.S02E05.The.Target.1080p",
            "The Special OVA S02E05 - The Target",
            "Golgo 13 S13E05 - The Target",
        ] {
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert!(
                    result.batches.is_empty()
                        && result.episodes.is_empty()
                        && result.other.is_empty()
                );
            }
        }
    }

    #[test]
    fn complete_release_aliases_keep_codec_words_and_remake_years() {
        let base = named_entry(1, "Hunter x Hunter");
        let mut remake = named_entry(2, "Hunter x Hunter (2011)");
        remake.status = "COMPLETED".into();
        let opus = named_entry(3, "Opus.COLORs");
        let colors = named_entry(4, "COLORs");
        let dd = named_entry(5, "DD Hokuto no Ken");
        let hokuto = named_entry(6, "Hokuto no Ken");
        let entries = [&base, &remake, &opus, &colors, &dd, &hokuto];
        let matchers = matchers_for(&entries);
        for (id, release, episode) in [
            (1, "Hunter.x.Hunter.05.1080p", Some(5)),
            (2, "Hunter_x_Hunter_(2011)_-_05", Some(5)),
            (2, "Hunter.x.Hunter.(2011).05.1080p", Some(5)),
            (2, "Hunter.x.Hunter.(2011).01-12.1080p", None),
            (3, "Opus.COLORs.05.1080p", Some(5)),
            (3, "Opus.COLORs Season 1 - 05", Some(5)),
            (3, "Opus.COLORs 2024 - 05", Some(5)),
            (3, "Opus.COLORs Complete", None),
            (4, "COLORs - 05 Opus", Some(5)),
            (5, "DD.Hokuto.no.Ken.05.1080p", Some(5)),
            (5, "DD Hokuto no Ken 2024 - 05", Some(5)),
            (6, "Hokuto no Ken DD - 05", Some(5)),
        ] {
            let (matched, parsed) = match_feed(&matchers, release);
            assert_eq!(matched.map(|m| m.media_id), Some(id), "{release}");
            assert_eq!(parsed, episode, "{release}");
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert_eq!(
                    result.episodes.len() + result.batches.len(),
                    usize::from(entry.media_id == id),
                    "{release} for {}",
                    entry.media_id
                );
                assert!(result.other.is_empty(), "{release} for {}", entry.media_id);
            }
        }
    }

    #[test]
    fn explicit_seasons_disambiguate_numbered_catalog_titles() {
        let standalone = [
            named_entry(1, "Lupin III"),
            named_entry(2, "FF VII"),
            named_entry(3, "Some Show 2"),
            named_entry(4, "Mob Psycho 100 II"),
        ];
        let paired = [
            named_entry(5, "Some Show"),
            named_entry(6, "Mob Psycho 100"),
            named_entry(7, "Lupin III: Part II"),
        ];
        for with_base in [false, true] {
            let entries = standalone
                .iter()
                .chain(paired.iter().take(if with_base { 3 } else { 0 }))
                .collect::<Vec<_>>();
            let matchers = matchers_for(&entries);
            for (release, owner) in [
                ("Lupin III S01E05", Some(1)),
                ("Lupin III S03E05", None),
                ("FF VII S01E05", Some(2)),
                ("FF VII S07E05", None),
                ("Some Show 2 S01E05", (!with_base).then_some(3)),
                ("Some Show 2 S02E05", with_base.then_some(3)),
                ("Some Show S02E05", with_base.then_some(3)),
                ("Some Show 2 - 05", Some(3)),
                ("Mob Psycho 100 II S01E05", (!with_base).then_some(4)),
                ("Mob Psycho 100 II S02E05", with_base.then_some(4)),
                ("Mob Psycho 100 S02E05", Some(4)),
                ("Lupin III Part II S01E05", None),
                ("Lupin III Part II Season 1 - 05", None),
                ("Lupin III S02E05", with_base.then_some(7)),
                ("Lupin III Part II Season 2 - 05", with_base.then_some(7)),
            ] {
                if !with_base && release.starts_with("Lupin III Part II") {
                    continue;
                }
                let (matched, episode) = match_feed(&matchers, release);
                assert_eq!(
                    matched.map(|m| m.media_id),
                    owner,
                    "{release} with base: {with_base}"
                );
                assert_eq!(episode, owner.map(|_| 5), "{release}");
                for entry in &entries {
                    let result = classify(entry, &matchers, vec![raw(release, 100)]);
                    assert_eq!(
                        result.episodes.len(),
                        usize::from(owner == Some(entry.media_id)),
                        "{release} for {}",
                        entry.media_id
                    );
                    assert!(
                        result.other.is_empty() && result.batches.is_empty(),
                        "{release} for {}",
                        entry.media_id
                    );
                }
            }
        }
    }

    #[test]
    fn native_and_spelled_seasons_keep_a_consistent_series() {
        let mut base = named_entry(1, "Sample Show");
        base.media.as_mut().unwrap().title_native = Some("サンプル ～副題～".into());
        let mut sequel = named_entry(2, "Sample Show Season 2");
        sequel.media.as_mut().unwrap().title_native = Some("サンプルⅡ ～副題～".into());
        let matchers = matchers_for(&[&base, &sequel]);
        for (title, id, episode, batch) in [
            ("サンプルⅡ - ０５", Some(2), Some(5), false),
            ("サンプル S02E05", Some(2), Some(5), false),
            ("サンプル ～副題～ Season 2 - 05", Some(2), Some(5), false),
            ("サンプル 第２期 - ０５", Some(2), Some(5), false),
            ("サンプルⅡ Season 1 - 05", None, None, false),
            ("サンプル Season 1 第2期 - 05", None, None, false),
            ("サンプル Season 2 第1期 - 05", None, None, false),
            ("サンプルⅡ", Some(2), None, true),
            ("サンプル - 05", Some(1), Some(5), false),
        ] {
            let (matched, parsed) = match_feed(&matchers, title);
            assert_eq!(matched.map(|m| m.media_id), id, "{title}");
            assert_eq!(parsed, episode, "{title}");
            let result = classify(&sequel, &matchers, vec![raw(title, 100)]);
            assert_eq!(
                result.episodes.len(),
                usize::from(id == Some(sequel.media_id) && episode.is_some()),
                "{title}"
            );
            assert_eq!(result.batches.len(), usize::from(batch), "{title}");
        }
    }

    #[test]
    fn repeated_numbered_titles_after_format_tags_use_the_body_alias() {
        let compact = named_entry(1, "No.6");
        let spaced = named_entry(2, "No. 6");
        let entries = [&compact, &spaced];
        let matchers = matchers_for(&entries);
        for (title, id, episode) in [
            ("[No.6] [1080p] No.6", 1, None),
            ("[No.6] [1080p] No.6 - 05", 1, Some(5)),
            ("[No.6] [1080p] No. 6 - 05", 2, Some(5)),
        ] {
            let (matched, parsed) = match_feed(&matchers, title);
            assert_eq!(matched.map(|m| m.media_id), Some(id), "{title}");
            assert_eq!(parsed, episode, "{title}");
            for entry in entries {
                let result = classify(entry, &matchers, vec![raw(title, 100)]);
                assert_eq!(
                    result.episodes.len(),
                    usize::from(entry.media_id == id && episode.is_some()),
                    "{title}"
                );
                assert_eq!(
                    result.batches.len(),
                    usize::from(entry.media_id == id && episode.is_none()),
                    "{title}"
                );
                assert!(result.other.is_empty(), "{title}");
            }
        }
    }

    #[test]
    fn movie_group_names_do_not_interfere_with_episode_inference() {
        let mut movie = named_entry(1, "The Special");
        let media = movie.media.as_mut().unwrap();
        media.format = Some("MOVIE".into());
        media.episodes = Some(1);
        let matchers = matchers(&movie);
        for (title, main) in [
            ("[The Special Fansubs] The Special", true),
            ("[SP Raws] The Special", true),
            ("[The Special] The Special", true),
            ("[SP] The Special", false),
            ("The Special [SP]", false),
        ] {
            let (matched, feed_episode) = match_feed(&matchers, title);
            assert_eq!(feed_episode, None, "{title}");
            assert_eq!(
                recognize::resolve_playback_episode(matched.unwrap(), &[title], false),
                main.then_some(1),
                "{title}"
            );
            assert_eq!(
                classify(&movie, &matchers, vec![raw(title, 100)])
                    .other
                    .len(),
                usize::from(main),
                "{title}"
            );
        }
    }

    #[test]
    fn explicit_scene_seasons_keep_numeric_catalog_titles() {
        let entries = [
            named_entry(1, "Kaiju No. 8"),
            named_entry(2, "Kaiju No. 8 Season 2"),
            named_entry(3, "Ranma 1/2"),
            named_entry(4, "Golgo 13"),
            named_entry(5, "Lupin III"),
            named_entry(6, "Lupin III: Part II"),
        ];
        let matchers = matchers_for(&entries.iter().collect::<Vec<_>>());
        for (id, release) in [
            (1, "Kaiju.No.8.S01E05.1080p"),
            (1, "Kaiju No. 8 (Season 1) - 05"),
            (2, "Kaiju No. 8 S02E05"),
            (3, "Ranma.1.2.S01E05.1080p"),
            (4, "Golgo 13 S01E05"),
            (5, "Lupin III S01E05"),
            (6, "Lupin III S02E05"),
            (6, "Lupin III Part II - 05"),
        ] {
            let (matched, episode) = match_feed(&matchers, release);
            assert_eq!(matched.map(|m| m.media_id), Some(id), "{release}");
            assert_eq!(episode, Some(5), "{release}");
            let result = classify(
                &entries[id as usize - 1],
                &matchers,
                vec![raw(release, 100)],
            );
            assert_eq!(result.episodes.len(), 1, "{release}");
        }
    }

    #[test]
    fn catalog_numbers_and_season_aliases_use_the_same_evidence() {
        let mut base = named_entry(1, "My Hero Academia");
        let mut second = named_entry(2, "My Hero Academia Season 2");
        base.media.as_mut().unwrap().title_native = Some("僕のヒーローアカデミア".into());
        second.media.as_mut().unwrap().title_native = Some("僕のヒーローアカデミア 第2期".into());
        let mut movie = named_entry(3, "Final Fantasy VII: Advent Children");
        movie.progress = 0;
        movie.media.as_mut().unwrap().episodes = Some(1);
        let entries = [
            base,
            second,
            movie,
            named_entry(4, "Golgo 13"),
            named_entry(5, "Lupin III: Part 5"),
            named_entry(6, "Ranma 1/2"),
            named_entry(7, "Kaiju No. 8"),
            named_entry(8, "No.6"),
        ];
        let matcher = matchers_for(&entries.iter().collect::<Vec<_>>());

        for (index, season) in [(1, 2), (2, 1), (3, 1), (4, 5), (5, 1), (6, 1), (7, 1)] {
            let q = queries(entries[index].media.as_ref().unwrap(), Some(5));
            assert!(
                q.iter().any(|q| q.contains(&format!("S{season:02}E05"))),
                "{q:?}"
            );
        }

        for (index, title, episode) in [
            (1, "僕のヒーローアカデミア Season 2 - 05", 5),
            (1, "僕のヒーローアカデミア 第2期 Season 2 - 05", 5),
            (2, "Final Fantasy VII: Advent Children S01E01", 1),
            (2, "Final Fantasy VII: Advent Children 2024 - E01", 1),
            (3, "Golgo.13.S01E05.The.Target", 5),
            (4, "Lupin III Part 05 - 05", 5),
            (4, "Lupin III: Part 5 S05E05", 5),
        ] {
            let entry = &entries[index];
            let (matched, detected) = match_feed(&matcher, title);
            assert_eq!(matched.map(|m| m.media_id), Some(entry.media_id), "{title}");
            assert_eq!(detected, Some(episode), "{title}");
            assert_eq!(
                classify(entry, &matcher, vec![raw(title, 10)])
                    .episodes
                    .len(),
                1,
                "{title}"
            );
        }
        for title in [
            "Golgo 13 S13E05",
            "Ranma.1.2.S02E05.1080p",
            "Kaiju No. 8 S08E05",
            "No.6 S06E05",
            "Lupin III: Part 5 S03E05",
            "Final Fantasy VII: Advent Children S07E01",
            "僕のヒーローアカデミア 第2期 S01E05",
        ] {
            assert!(match_feed(&matcher, title).0.is_none(), "{title}");
        }
    }

    #[test]
    fn group_names_aliases_and_metadata_keep_their_roles() {
        let mut bilingual = named_entry(1, "Eighty Six");
        bilingual.media.as_mut().unwrap().title_romaji = Some("86".into());
        let entries = [
            bilingual,
            named_entry(2, "The Special"),
            named_entry(3, "No.6"),
            named_entry(4, "Opus"),
            named_entry(5, "DD"),
            named_entry(6, "Opus Color"),
            named_entry(7, "Color"),
        ];
        let matcher = matchers_for(&entries.iter().collect::<Vec<_>>());
        for (index, title) in [
            (0, "Eighty Six/86 - 05"),
            (1, "[The Special Fansubs] The Special - 05"),
            (1, "[The Special] The Special - 05"),
            (2, "[No.6 Fansubs] No. 6/No.6 - 05"),
            (3, "Opus FLAC - 05"),
            (4, "DD 1080p - 05"),
            (5, "Opus Color yuv420p10le - 05"),
            (6, "Color - 05"),
        ] {
            let entry = &entries[index];
            let (matched, episode) = match_feed(&matcher, title);
            assert_eq!(matched.map(|m| m.media_id), Some(entry.media_id), "{title}");
            assert_eq!(episode, Some(5), "{title}");
            assert_eq!(
                classify(entry, &matcher, vec![raw(title, 10)])
                    .episodes
                    .len(),
                1,
                "{title}"
            );
        }
        let batch = "Eighty Six / 86 [BD Batch]";
        assert_eq!(match_feed(&matcher, batch).1, None);
        assert_eq!(
            classify(&entries[0], &matcher, vec![raw(batch, 10)])
                .batches
                .len(),
            1
        );
    }

    #[test]
    fn bracket_episode_markers_keep_absolute_episode_bounds() {
        let mut entry = named_entry(1, "Some Show");
        entry.media.as_mut().unwrap().episodes = Some(1100);
        let matcher = matchers(&entry);
        for (title, number) in [
            ("Some Show [S01E1080]", Some(1080)),
            ("Some Show [#720]", Some(720)),
            ("Some Show [1080]", None),
            ("Some Show [720]", None),
            ("Some Show [S01E1101]", Some(1101)),
        ] {
            let (matched, episode) = match_feed(&matcher, title);
            assert_eq!(matched.map(|m| m.media_id), Some(1), "{title}");
            assert_eq!(episode, number, "{title}");
            let expected = number.filter(|n| *n <= 1100);
            let matched = matched.unwrap();
            assert_eq!(
                crate::library::resolve_bound_episode(matched, &[title], None),
                expected,
                "{title}"
            );
            let result = classify(&entry, &matcher, vec![raw(title, 10)]);
            assert_eq!(
                result.other.first().and_then(|item| item.episode),
                expected,
                "{title}"
            );
            if number.is_some_and(|n| n > 1100) {
                assert!(
                    result.episodes.is_empty()
                        && result.batches.is_empty()
                        && result.other.is_empty()
                );
            }
        }
    }

    #[test]
    fn named_seasons_override_roman_numbers_in_the_series_title() {
        let base = named_entry(1, "Lupin III");
        let second = named_entry(2, "Lupin III Season 2");
        let matchers = matchers_for(&[&base, &second]);
        for (release, id) in [("Lupin III S01E05", 1), ("Lupin III S02E05", 2)] {
            let (matched, episode) = match_feed(&matchers, release);
            assert_eq!(matched.map(|m| m.media_id), Some(id), "{release}");
            assert_eq!(episode, Some(5), "{release}");
        }
        let part = named_entry(2, "Lupin III Part II");
        let matchers = matchers_for(&[&base, &part]);
        assert!(match_feed(&matchers, "Lupin III Part II S01E05")
            .0
            .is_none());
    }

    #[test]
    fn marked_episode_seven_and_bare_season_seven_keep_distinct_owners() {
        let mut first = named_entry(1, "My Hero Academia");
        first.media.as_mut().unwrap().title_romaji = Some("Boku no Hero Academia".into());
        let mut seventh = named_entry(7, "My Hero Academia Season 7");
        seventh.media.as_mut().unwrap().title_romaji =
            Some("Boku no Hero Academia 7th Season".into());
        let matchers = matchers_for(&[&first, &seventh]);
        for (release, owner, number) in [
            ("[G] Boku no Hero Academia - 7 (1080p).mkv", &first, Some(7)),
            ("Boku no Hero Academia - 7 1080p", &first, Some(7)),
            ("Boku no Hero Academia-7 [1080p]", &first, Some(7)),
            ("Boku_no_Hero_Academia_-7_[1080p]", &first, Some(7)),
            ("Boku no Hero Academia 7 (1080p)", &seventh, None),
        ] {
            let (matched, episode) = match_feed(&matchers, release);
            assert_eq!(
                matched.map(|m| m.media_id),
                Some(owner.media_id),
                "{release}"
            );
            assert_eq!(episode, number, "{release}");
            let result = classify(owner, &matchers, vec![raw(release, 100)]);
            if number.is_some() {
                assert_eq!(result.other[0].episode, number, "{release}");
            } else {
                assert_eq!(result.batches.len(), 1, "{release}");
            }
        }
    }

    #[test]
    fn airing_packs_belong_to_the_season_identified_by_the_release() {
        let mut first = named_entry(1, "Some Show");
        let mut second = named_entry(2, "Some Show Season 2");
        for entry in [&mut first, &mut second] {
            entry.media.as_mut().unwrap().status = Some("RELEASING".into());
        }
        let matchers = matchers_for(&[&first, &second]);
        for (release, owner) in [
            ("[A] Some_Show_Batch_1080p", Some(1)),
            ("[Batch S02] Some Show", Some(2)),
            ("[Season 2 Complete] Some Show", Some(2)),
            ("[Season 2 01-12] Some Show", Some(2)),
            ("[Fansubs Batch] Some Show", None),
            ("[S02E01-E12] Some Show S01", None),
            ("[S02] [01-12] Some Show S01", None),
        ] {
            for entry in [&first, &second] {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert_eq!(
                    result.batches.len(),
                    usize::from(owner == Some(entry.media_id)),
                    "{release} for {}",
                    entry.media_id
                );
            }
        }
    }

    #[test]
    fn corner_bracket_aliases_distinguish_both_sides_of_a_collision() {
        let plain = named_entry(1, "Oshi no Ko");
        let bracketed = named_entry(2, "【Oshi no Ko】");
        let matchers = matchers_for(&[&plain, &bracketed]);
        let releases = [
            "[Group]Oshi no Ko - 05 [1080p]",
            "[Group]Oshi no Ko [BD]",
            "[Group]【Oshi no Ko】 - 05 [1080p]",
            "[Group]【Oshi no Ko】 [BD]",
        ];
        for (entry, wanted) in [(&plain, &releases[..2]), (&bracketed, &releases[2..])] {
            let result = classify(
                entry,
                &matchers,
                releases.iter().map(|title| raw(title, 100)).collect(),
            );
            assert_eq!(result.episodes.len(), 1);
            assert_eq!(result.episodes[0].title, wanted[0]);
            assert_eq!(result.episodes[0].episode, Some(5));
            assert_eq!(result.batches.len(), 1);
            assert_eq!(result.batches[0].title, wanted[1]);
            assert!(result.other.is_empty());
        }
    }

    #[test]
    fn compact_season_episode_agrees_with_an_ordinal_catalog_alias() {
        let entry = named_entry(2, "Some Show 2nd Season");
        let result = classify(
            &entry,
            &matchers(&entry),
            vec![
                raw("[A] Some Show Season 2-05 [1080p]", 100),
                raw("[A] Some Show Season 2-10 [1080p]", 100),
                raw("[A] Some Show Season 2 - 01-12 [1080p]", 100),
            ],
        );
        assert_eq!(result.episodes.len(), 1);
        assert_eq!(result.episodes[0].episode, Some(5));
        assert_eq!(result.other.len(), 1);
        assert_eq!(result.other[0].episode, Some(10));
        assert_eq!(result.batches.len(), 1);
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
    fn preview_tags_are_excluded_unless_they_belong_to_the_title() {
        let e = entry();
        let targets = matchers(&e);
        for title in [
            "[A] Some Show [Preview] [1080p]",
            "[A] Some Show [Sample] [1080p]",
            "[A] Some Show - 05 [Preview] [1080p]",
            "Some Show - 05 NCOP",
            "Some Show - 05 Preview",
            "Some Show - 05 Sample",
            "Some Show - 05 - Dawn - NCOP",
            "Some Show - 05 NCOP - 1080p",
            "Some Show - 05 - Next Episode Preview",
            "Some_Show_S01E05V2_NCOP01_1080p",
            "Some Show [05] - NCEDv2 [1080p]",
            "[A] Some Show [Creditless] [1080p]",
            "[A] Some Show [Preview_1080p]",
            "[A] Some Show [1080p_Sample]",
            "[A] Some Show [Creditless_1080p]",
        ] {
            let result = classify(&e, &targets, vec![raw(title, 100)]);
            assert!(result.batches.is_empty(), "{title}: {:?}", result.batches);
            assert!(result.episodes.is_empty(), "{title}");
            assert!(result.other.is_empty(), "{title}");
        }
        for title in [
            "Some Show - 05 - A Preview of Spring [1080p]",
            "Some Show - 05 - The Sample [1080p]",
            "Some Show - 05 - A Next Episode Preview of Spring",
            "Some Show - 05 - The Re-Preview",
            "Some Show S01E05 A New Trailer [1080p]",
        ] {
            let result = classify(&e, &targets, vec![raw(title, 100)]);
            assert_eq!(result.episodes.len(), 1, "{title}");
        }
        let mut e = e;
        e.media.as_mut().unwrap().title_english = Some("The Preview".into());
        let result = classify(&e, &matchers(&e), vec![raw("[A] The_Preview [01-12]", 100)]);
        assert_eq!(result.batches.len(), 1);
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
        for title in [
            "[A] Some Show [01-06] [07-24] [Batch]",
            "[01-24] Some Show [01-12]",
            "Some Show [01-12] [13-24.5]",
        ] {
            let result = classify(&e, &matchers, vec![raw(title, 100)]);
            assert!(
                result.batches.is_empty() && result.other.is_empty() && result.episodes.is_empty(),
                "{title}"
            );
        }
        for title in [
            "[A] Some Show [01-04] [05-12] [BD]",
            "[05-12] Some Show [01-04]",
            "[Group 01-24] Some Show [Batch]",
        ] {
            let result = classify(&e, &matchers, vec![raw(title, 100)]);
            assert_eq!(result.batches.len(), 1, "{title}");
        }
        let already_watched = classify(
            &e,
            &matchers,
            vec![raw("[A] Some Show [01-04] [Batch]", 100)],
        );
        assert!(already_watched.batches.is_empty());
        assert_eq!(already_watched.other.len(), 1);
    }

    #[test]
    fn trailing_episode_ranges_must_share_a_season() {
        let first = named_entry(1, "Some Show");
        let second = named_entry(2, "Some Show Season 2");
        let matchers = matchers_for(&[&first, &second]);
        for (release, owner) in [
            ("[A] Some Show [S01E01-E06] [S02E07-E12]", None),
            ("[A] Some Show S01E01-E06 S02E07-E12", None),
            ("[A] Some Show S01E01-E03 S01E04-E12", Some(1)),
            ("[A] Some Show [S02E01-E06] [S01E07-E12]", None),
            ("[A] Some Show [S01.E01-E06] [E07-S02.E12]", None),
            ("[A] Some Show [Season 1 01-06] [Season 2 07-12]", None),
            ("[A] Some Show [Season 2 01-06] [Season 2 07-12]", Some(2)),
            ("[A] Some Show [S01E01-E06] [S1E07-E12]", Some(1)),
            ("[A] Some Show [S02E01-E06] [E07-S02E12]", Some(2)),
        ] {
            for entry in [&first, &second] {
                let result = classify(entry, &matchers, vec![raw(release, 100)]);
                assert_eq!(
                    result.batches.len(),
                    usize::from(owner == Some(entry.media_id)),
                    "{release}, {}",
                    entry.media_id
                );
                assert!(
                    result.episodes.is_empty() && result.other.is_empty(),
                    "{release}"
                );
            }
        }
    }

    #[test]
    fn underscore_delimited_ranges_keep_episode_bounds() {
        let entry = entry();
        let matchers = matchers(&entry);
        let titles = [
            "[A] Some Show [01-12_1080p]",
            "[B] Some Show [01-04_1080p]",
            "[C] Some Show [01-24_1080p]",
            "[D] Some Show [1080p_01-04]",
            "[E] Some Show [1080p_01-24]",
            "[F] Some Show [S01E01-S01E12_1080p]",
            "[G] Some Show [S01E01v2-S01E12v3_1080p]",
        ];
        let results = titles.map(|title| {
            let result = classify(&entry, &matchers, vec![raw(title, 100)]);
            for item in result.batches.iter().chain(&result.other) {
                assert_eq!(item.title, title);
                assert_eq!(item.media_id, Some(1));
                assert_eq!(item.episode, None);
            }
            (
                result.next_episode,
                result.batches.len(),
                result.episodes.len(),
                result.other.len(),
            )
        });
        assert_eq!(
            results,
            [
                (Some(5), 1, 0, 0),
                (Some(5), 0, 0, 1),
                (Some(5), 0, 0, 0),
                (Some(5), 0, 0, 1),
                (Some(5), 0, 0, 0),
                (Some(5), 1, 0, 0),
                (Some(5), 1, 0, 0),
            ]
        );
    }

    #[test]
    fn cross_season_ranges_are_excluded_with_metadata_and_revisions() {
        let entry = entry();
        let matchers = matchers(&entry);
        for title in [
            "[A] Some Show [S01E01-S02E12_1080p]",
            "[B] Some Show [1080p_S01E01-S02E12]",
            "[C] Some Show [1080p_S01E01-S02E12_BD]",
            "[D] Some Show [S01E01-S01E04_S01E05-S02E12]",
            "[E] Some Show [S01E01-S01E04 S01E05-S02E12]",
            "[F] Some Show [S01E01v2-S02E12]",
            "[G] Some Show [S01E01-S02E12v3]",
            "[H] Some Show [S01E01v2-S02E12v3_1080p]",
        ] {
            let result = classify(&entry, &matchers, vec![raw(title, 100)]);
            assert!(
                result.batches.is_empty() && result.episodes.is_empty() && result.other.is_empty(),
                "{title}: {result:?}"
            );
        }
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
        db.mark_rss_seen(std::slice::from_ref(&guid)).unwrap();
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
            raw("[B] K-On!![BD]", 100),
            raw("[C] K On [BD]", 100),
            raw("[Group]K-On!! - 01 [1080p]", 100),
            raw("(Group)K-On!! - 02 [1080p]", 100),
            raw("【Group】K-On!! - 03 [1080p]", 100),
            raw("[Group] K-On!!【1080p】", 100),
        ];
        let first_result = classify(&first, &matchers, releases.clone());
        assert_eq!(first_result.batches.len(), 1);
        assert_eq!(first_result.batches[0].title, "[A] K-On! [BD]");
        assert!(first_result.episodes.is_empty());
        assert!(first_result.other.is_empty());
        let second_result = classify(&second, &matchers, releases);
        for title in ["[B] K-On!![BD]", "[Group] K-On!!【1080p】"] {
            assert!(second_result.batches.iter().any(|item| item.title == title));
        }
        assert_eq!(second_result.batches.len(), 2);
        assert_eq!(second_result.other.len(), 3);
        for episode in 1..=3 {
            assert!(second_result
                .other
                .iter()
                .any(|item| item.episode == Some(episode)));
        }
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
                raw("[A] Some_Show_2nd_Season_05 [1080p]", 100),
                raw("[A] Some Show S02E01-E12 [1080p]", 100),
                raw("[A] Some Show Season 2 [BD]", 100),
                raw("[A] Some Show S01E05 [1080p]", 100),
            ],
        );
        assert_eq!(result.episodes.len(), 2);
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
    fn equivalent_hex_and_base32_hashes_share_releases_and_seen_marks() {
        let entry = entry();
        let first = RawItem {
            info_hash: Some("abcdef0123456789abcdef0123456789abcdef01".into()),
            ..raw("[Group] Some Show - 05 [1080p]", 10)
        };
        let second = RawItem {
            info_hash: Some("VPG66AJDIVTYTK6N54ASGRLHRGV433YB".into()),
            link: "https://another.example/release".into(),
            guid: "another-feed\u{1}release".into(),
            ..first.clone()
        };
        assert_eq!(stable_guid(&first), stable_guid(&second));
        let result = classify(
            &entry,
            &matchers(&entry),
            vec![first.clone(), second.clone()],
        );
        assert_eq!(result.episodes.len(), 1);
        for item in [&first, &second] {
            for old_key in [
                "nyaa-show\u{1}abcdef0123456789abcdef0123456789abcdef01",
                "nyaa-show\u{1}vpg66ajdivtytk6n54asgrlhrgv433yb",
            ] {
                let db = Db::open(std::path::Path::new(":memory:")).unwrap();
                db.mark_rss_seen(&[old_key.into()]).unwrap();
                let seen = db.rss_seen_set().unwrap();
                assert!(was_seen(item, &seen));
                assert!(search_results(vec![item.clone()], &seen)[0].seen);
                let mut discovery = classify(&entry, &matchers(&entry), vec![item.clone()]);
                restore_seen(&mut discovery, &seen);
                assert!(discovery.episodes[0].seen);
            }
        }
    }
    #[test]
    fn bilingual_release_titles_keep_known_aliases_and_reject_unknown_sequels() {
        let entry = entry();
        let matchers = matchers(&entry);
        let releases = vec![
            raw("[A] Some Show / Watashi no Anime - 05 [1080p]", 100),
            raw("[B] Watashi no Anime / Some Show [01-12] [Batch]", 50),
            raw("[C] Some Show / Unrelated Sequel - 05 [1080p]", 20),
            raw("[E] Some Show 5vSequel - 05", 20),
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
            "[A] Some Show - 5.5 [1080p]",
            "[A] Some Show 5.5 [1080p]",
            "[A] Some Show EP5.5 [1080p]",
            "[A] Some Show S01E5.5 [1080p]",
            "[A] Some Show - 12.5 [1080p]",
            "[A] Some Show [12.5_1080p]",
            "[A] Some Show - 12.5_[1080p]",
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
                result.batches.is_empty() && result.episodes.is_empty() && result.other.is_empty(),
                "{release}"
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
        let first = named_entry(1, "Some Show");
        let mut entry = named_entry(2, "Some Show 2nd Season");
        entry.media.as_mut().unwrap().status = Some("RELEASING".into());
        let matchers = matchers_for(&[&first, &entry]);
        for release in [
            "[A] Some Show [S02E01-E12] [BD]",
            "[A] Some Show 【S02E01】 [02-12] [BD]",
            "[A] [Season 2 01-12] Some Show",
            "[A] 【S02E01-E12】 Some Show [BD]",
        ] {
            let wrong_season = classify(&first, &matchers, vec![raw(release, 100)]);
            assert!(wrong_season.batches.is_empty(), "{release}");
            assert!(wrong_season.episodes.is_empty(), "{release}");
            assert!(wrong_season.other.is_empty(), "{release}");
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

    #[test]
    fn dates_and_decimals_are_not_parsed_as_episode_ranges() {
        assert!(episode_ranges("[Group] Some Show 2026-04-05 [1080p]").is_empty());
        assert!(episode_ranges("[Group] Some Show 2026-04-05 - 03 [1080p]").is_empty());
        assert!(episode_ranges("Some Show 1.0-12").is_empty());
        assert!(episode_ranges("Some Show 01-12.5")[0].fractional_end);
    }
}
