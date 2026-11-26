use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, LazyLock, OnceLock};

use regex::Regex;

use crate::db::Db;

static RE_BRACKETS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\[(?:[^\[\]]|\[[^\[\]]*\])*\]|\((?:[^()]|\([^()]*\))*\)|【(?:[^【】]|【[^【】]*】)*】",
    )
    .unwrap()
});
static RE_EMPTY_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\s*\]|\(\s*\)|【\s*】").unwrap());
static RE_FORMAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:(?:360|480|576|720|1080|1440|2160|4320)p|\d{3,4}p\d{2,3}|[48]k|bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc(?:10(?:bit)?)?|x265|x264|h[\s.-]*26[456]|avc|av1|vp9|vp0|vvc|xvid|divx|uncensored|mpeg-?2|(?:aac|e-?ac-?3|ac-?3|ddp?|opus|flac|dts(?:-?hd)?|truehd|l?pcm)(?:[ ._-]?[0-9]\.[0-9])?|ddp?\d|(?:8|10|12|16|24|32)[ .-]?bit|hi10p?|yuv(?:420|422|444)(?:p(?:\d{1,2}(?:le|be)?)?)?|\d{3,4}[x×]\d{3,4}p?|\d{1,3}(?:\.\d{1,4})?[ ._-]*fps|[1-9]\.\d[ ._-]*ch|\d{1,4}(?:\.\d{1,3})?[ ._-]*(?:[kmgt]i?b))\b").unwrap()
});
static RE_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^\p{L}\p{N}])(?P<date>([12][0-9]{3})([-._])([0-9]{2})([-._])([0-9]{2}))(?:$|[^\p{L}\p{N}])").unwrap()
});
pub(crate) static PACK_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:batch|pack|complete(?:d)?(?:[ .-]+series)?|collection)\b").unwrap()
});
static RE_RES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(1080|720|576|480|360|2160|1440|4320)\b").unwrap());
// Require a separator before E05 to preserve Steins;Gate.
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·–—]?\s*(?:s\d{1,2}[.\s_]*e|ep(?:isode)?\.?|[-_·–—.\s]e|#)?\s*0*[1-9]\d{0,3}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
static RE_EP_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:v\d+)?").unwrap());
static RE_NTH_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?P<n>\d{1,2})(?:st|nd|rd|th)(?:[\s._]+|-)season\b").unwrap());
static RE_SEASON_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bseason(?:[\s._]+|-)(?P<n>\d{1,2})\b").unwrap());
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:2\.0|5\.1|7\.1)\b").unwrap());
static RE_NATIVE_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"第(?P<number>[0-9]{1,2})期|(?P<roman>[ⅡⅢⅣⅤⅥⅦⅧⅨⅩⅪⅫⅱⅲⅳⅴⅵⅶⅷⅸⅹⅺⅻ])").unwrap()
});
static RE_SEASON_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})[.\s_]*(?:e\d{1,4}(?:v\d+)?)+\b").unwrap());
static RE_SEASON_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)s\d{1,2}[.\s_]*(e)").unwrap());
static RE_SEASON_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS]\d{1,2}\b").unwrap());
static RE_SEASON_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})\b").unwrap());
static RE_REV_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+v\d+\s*$").unwrap());
static RE_SUBTITLE_SEPARATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+[-–—|]\s+").unwrap());
pub(crate) static RELEASE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:dual[ ._-]+audio|multi[ ._-]+(?:audio|subs?)|english[ ._-]+sub(?:bed|s)?|end|final)\b").unwrap()
});
static RE_SEASON_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\p{L}\p{N}])(?P<words>season(?:[\s._]+|-)(?P<number>[0-9]{1,2})|[0-9]{1,2}(?:st|nd|rd|th)(?:[\s._]+|-)season)(?:$|[^\p{L}\p{N}])").unwrap()
});
static RE_SEASON_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\p{L}\p{N}])(?P<range>(?:season(?P<plural>s)?(?:[\s._]+|-)|(?P<short>s))(?P<first>[0-9]{1,2})(?P<separator>[\s_]*[-~–—+&,][\s_]*)(?:(?P<second_season>s|seasons?[\s._-]+))?(?P<last>[0-9]{1,4})(?:v[0-9]+)?(?P<more>(?:[\s_]*[-~–—,][\s_]*[0-9]{1,4})*))(?:$|[^\p{L}\p{N}])").unwrap()
});
static RE_EXPLICIT_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bs(?P<season>\d{1,2})[.\s_]*e|\bep(?:isode)?[.\s_-]*|\be[.\s_-]*|#)\s*(?P<episode>\d{1,4})(?:v\d+)?(?P<more>(?:e\d{1,4}(?:v\d+)?)+)?(?:$|[^\w])").unwrap()
});
static RE_LEADING_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[\s._:·–—-]*(\d{1,4})(?:v\d+)?(?:$|[\s._–—-])").unwrap());
static RE_DASHED_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[\s._])(?P<dash>[-·–—])[\s._]*(\d{1,4})(?:v\d+)?\b").unwrap()
});
static RE_BRACKET_SEASON_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[(【]\s*(?P<marker>(?:season[\s._-]+[0-9]{1,2}|[0-9]{1,2}(?:st|nd|rd|th)[\s._-]+season)[\s_]+[-–—])[\s_]+(?P<number>[0-9]{2,4})(?:v[0-9]+)?\s*[\])】]").unwrap()
});
static RE_BRACKET_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[(【]\s*(?P<marker>(?:(?:season[\s._-]+[0-9]{1,2}|[0-9]{1,2}(?:st|nd|rd|th)[\s._-]+season)[\s._]+(?:[-–—][\s._]+)?)?(?:ep(?:isode)?[.\s_-]*|e[.\s_-]*|#)|s[0-9]{1,2}[.\s_]*e)?\s*(?P<number>\d{1,4})(?:v\d+)?(?P<more>(?:e\d{1,4}(?:v\d+)?)+)?\s*[\])】]").unwrap()
});
static RE_EP_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\p{L}\p{N}])(?P<range>(?P<marker>(?:s(?P<season>[0-9]{1,2})[.\s_]*)?e[.\s_-]*|ep(?:isode)?[.\s_-]*|#)?(?P<start>[0-9]{1,4})(?:v[0-9]+)?\s*[-~–—]\s*(?P<end_marker>s(?P<end_season>[0-9]{1,2})[.\s_]*e|ep(?:isode)?[.\s_-]*|e[.\s_-]*|#)?(?P<end>[0-9]{1,4})(?P<fraction>\.(?:5|25|75))?(?:v[0-9]+)?)(?:$|[^\p{L}\p{N}])").unwrap()
});
static RE_CROSS_SEASON_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs([0-9]{1,2})[.\s_]*e[0-9]{1,4}(?:v[0-9]+)?\s*[-~–—]\s*s([0-9]{1,2})[.\s_]*e[0-9]{1,4}(?:v[0-9]+)?\b").unwrap()
});
static RE_FRACTIONAL_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\p{L}\p{N}])(?P<fraction>(?:s[0-9]{1,2}[.\s_]*e|ep(?:isode)?[.\s_-]*|e[.\s_-]*|#)?(?P<number>[0-9]{1,4})\.(?:5|25|75)(?:v[0-9]+)?)(?:$|[^\p{L}\p{N}])").unwrap()
});
pub(crate) static RE_EXTRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?P<marker>trailer|preview|sample|pvs?|ovas?|oads?|sp|op|ed|ost|ncops?|nceds?|creditless|extras?|specials?|cms?|menus?)[0-9]*(?:\s*v[0-9]+)?\b").unwrap()
});

const NOISE_NUMBERS: [i64; 8] = [360, 480, 576, 720, 1080, 1440, 2160, 4320];

const MAX_SEASON: u32 = 50;

#[cfg(test)]
thread_local! {
    static MATCHER_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static ORDINAL_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn matcher_work_counts() -> (usize, usize) {
    MATCHER_WORK.get()
}

pub(crate) struct Matcher {
    pub media_id: i64,
    pub display: String,
    pub variants: Vec<String>,
    pub episodes: Option<i64>,
    format: Option<String>,
    norms: Vec<String>,
    norm_seasons: Vec<Option<u32>>,
    explicit_season: Option<u32>,
    status_rank: u8,
    title_patterns: OnceLock<Vec<Regex>>,
    season_patterns: OnceLock<Vec<(u32, String, Regex)>>,
    // Include rows whose titles cannot produce a matcher.
    source: Option<Arc<Vec<MatcherInput>>>,
}

#[derive(PartialEq, Eq)]
struct MatcherInput {
    media_id: i64,
    titles: [Option<String>; 3],
    episodes: Option<i64>,
    format: Option<String>,
    status_rank: u8,
}

impl Matcher {
    fn title_patterns(&self) -> &[Regex] {
        self.title_patterns.get_or_init(|| {
            let mut variants = self.variants.iter().collect::<Vec<_>>();
            variants.sort_by_key(|variant| std::cmp::Reverse(variant.len()));
            variants
                .into_iter()
                .filter_map(|variant| title_pattern(&fold_digits(variant)))
                .collect()
        })
    }

    pub(crate) fn release_title_span(&self, release: &str) -> Option<Range<usize>> {
        self.release_title_chain(release).map(|(span, _)| span)
    }

    pub(crate) fn release_title_span_in(&self, release: &str, body: &str) -> Option<Range<usize>> {
        let offset = release.len() - body.len();
        self.release_title_span(release)
            .and_then(|span| Some(span.start.checked_sub(offset)?..span.end - offset))
    }

    pub(crate) fn collapse_known_aliases(&self, release: &str) -> String {
        let mut title = release.to_owned();
        if let Some((span, first_end)) = self.release_title_chain(release) {
            title.replace_range(first_end..span.end, &" ".repeat(span.end - first_end));
        }
        title
    }

    fn release_title_chain(&self, release: &str) -> Option<(Range<usize>, usize)> {
        let body = strip_release_groups(release, std::slice::from_ref(self));
        let offset = release.len() - body.len();
        let mut cut = release_alias_cut(body, 0);
        let mut spans = self.release_alias_spans(body).into_iter();
        let mut title = spans
            .find(|span| {
                alias_before_episode(span, cut, self.alias_spelling(body, span).is_literal())
            })
            .or_else(|| {
                let title_end = self.literal_release_title_end(body, cut);
                let cut = cut.max(title_end);
                self.season_alias_spans(body, release_season(body, &release[..offset], title_end))
                    .into_iter()
                    .find(|span| span.end <= cut && season_alias_suffix(body, span))
            })?;
        let first_end = title.end + offset;
        if title.end > cut {
            cut = release_alias_cut(body, title.end);
        }
        for next in spans {
            if next.start < title.end {
                continue;
            }
            if next.start >= cut {
                break;
            }
            if !alias_before_episode(&next, cut, self.alias_spelling(body, &next).is_literal()) {
                continue;
            }
            if !matches!(
                body[title.end..next.start].trim(),
                "/" | "|" | "｜" | "／" | ":" | "："
            ) {
                break;
            }
            title.end = next.end;
            if title.end > cut {
                cut = release_alias_cut(body, title.end);
            }
        }
        Some((title.start + offset..title.end + offset, first_end))
    }

    pub(crate) fn release_alias_spans(&self, release: &str) -> Vec<Range<usize>> {
        Self::pattern_spans(self.title_patterns(), release)
    }

    fn could_contain_alias(&self, normalized: &str) -> bool {
        self.norms.iter().any(|norm| {
            let word = norm.split(' ').next().unwrap_or_default();
            normalized.starts_with(word)
                || (!word.is_ascii()
                    && native_season(word).is_some_and(|(marker, _)| {
                        marker.start > 0 && normalized.starts_with(&word[..marker.start])
                    }))
        })
    }

    fn release_alias_evidence(&self, raw: &str, cut: usize, season: Option<u32>) -> AliasEvidence {
        let mut exact = 0;
        let mut direct = 0;
        for span in self.release_alias_spans(raw) {
            let spelling = self.alias_spelling(raw, &span);
            if !alias_before_episode(&span, cut, spelling.is_literal()) {
                continue;
            }
            if spelling == AliasSpelling::Exact {
                exact = exact.max(span.len());
            }
            if direct == 0
                && (spelling.is_literal()
                    || (release_alias_cut(&raw[span.clone()], 0) == span.len()
                        && !has_marked_episode_tail(&raw[span.clone()])))
                && release_alias_suffix(raw, &span)
            {
                direct = span.len();
            }
        }
        let stem = self
            .season_alias_spans(raw, season)
            .into_iter()
            .find(|span| span.end <= cut && season_alias_suffix(raw, span))
            .map_or(0, |span| span.len());
        AliasEvidence {
            direct,
            stem,
            exact,
        }
    }

    pub(crate) fn has_release_alias(&self, release: &str) -> bool {
        let body = strip_release_groups(release, std::slice::from_ref(self));
        self.release_title_span_in(release, body)
            .is_some_and(|span| release_alias_suffix(body, &span))
    }

    pub(crate) fn has_scoped_release_alias(&self, release: &str) -> bool {
        let body = strip_release_groups(release, std::slice::from_ref(self));
        self.release_title_span_in(release, body)
            .is_some_and(|span| season_alias_suffix(body, &span))
    }

    fn literal_release_title_end(&self, release: &str, cut: usize) -> usize {
        if cut == release.len()
            || !self.variants.iter().any(|variant| {
                let variant = fold_digits(variant);
                release_alias_cut(&variant, 0) < variant.len()
            })
        {
            return 0;
        }
        self.release_alias_spans(release)
            .into_iter()
            .filter(|span| self.alias_spelling(release, span).is_literal())
            .chain(
                self.season_patterns()
                    .iter()
                    .flat_map(|(_, alias, pattern)| {
                        Self::pattern_spans(std::iter::once(pattern), release)
                            .into_iter()
                            .filter(move |span| alias_spelling(release, span, alias).is_literal())
                    }),
            )
            .filter(|span| {
                span.start < cut && span.end > cut && starts_release_alias(release, span)
            })
            .map(|span| span.end)
            .max()
            .unwrap_or_default()
    }

    pub(crate) fn release_season(&self, body: &str, groups: &str) -> Option<u32> {
        let end = self.literal_release_title_end(body, release_alias_cut(body, 0));
        release_season(body, groups, end)
    }

    fn season_alias_spans(&self, release: &str, season: Option<u32>) -> Vec<Range<usize>> {
        let Some(season) = season else {
            return Vec::new();
        };
        if self
            .explicit_season
            .map_or(!self.norm_seasons.contains(&Some(season)), |n| n != season)
        {
            return Vec::new();
        }
        Self::pattern_spans(
            self.season_patterns()
                .iter()
                .filter_map(|(number, _, pattern)| (*number == season).then_some(pattern)),
            release,
        )
    }

    fn season_patterns(&self) -> &[(u32, String, Regex)] {
        self.season_patterns.get_or_init(|| {
            self.variants
                .iter()
                .zip(&self.norm_seasons)
                .filter_map(|(variant, ordinal)| Some((variant, (*ordinal)?)))
                .flat_map(|(variant, ordinal)| {
                    season_aliases(&fold_digits(variant))
                        .into_iter()
                        .filter_map(move |alias| {
                            Some((ordinal, alias.trim().to_owned(), title_pattern(&alias)?))
                        })
                })
                .collect()
        })
    }

    fn pattern_spans<'a>(
        patterns: impl IntoIterator<Item = &'a Regex>,
        release: &str,
    ) -> Vec<Range<usize>> {
        let mut spans = patterns
            .into_iter()
            .flat_map(|pattern| {
                let mut from = 0;
                std::iter::from_fn(move || {
                    let alias = pattern.captures_at(release, from)?.name("title")?;
                    from = alias.end();
                    Some(alias.range())
                })
            })
            .collect::<Vec<_>>();
        spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
        spans
    }

    pub(crate) fn alias_spelling(&self, raw: &str, span: &Range<usize>) -> AliasSpelling {
        self.variants
            .iter()
            .map(|variant| alias_spelling(raw, span, &fold_digits(variant.trim())))
            .max()
            .unwrap_or(AliasSpelling::Normalized)
    }

    pub(crate) fn is_season_pack(&self, release: &str) -> bool {
        season_pack_range(release, || {
            let body = strip_release_groups(release, std::slice::from_ref(self));
            (self.release_title_span(release), release.len() - body.len())
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AliasSpelling {
    Normalized,
    Scene,
    Exact,
}

impl AliasSpelling {
    pub(crate) fn is_literal(self) -> bool {
        self != Self::Normalized
    }
}

fn alias_spelling(raw: &str, span: &Range<usize>, alias: &str) -> AliasSpelling {
    let boundary = |c: char| release_separator(c) || "[]()【】/|｜／:：#".contains(c);
    if !raw[..span.start].chars().next_back().is_none_or(boundary)
        || !raw[span.end..].chars().next().is_none_or(boundary)
    {
        return AliasSpelling::Normalized;
    }
    let text = &raw[span.clone()];
    if text.eq_ignore_ascii_case(alias)
        || (!alias.is_ascii() && text.to_lowercase() == alias.to_lowercase())
    {
        return AliasSpelling::Exact;
    }
    if text.contains(['.', '_']) && alias.contains(char::is_whitespace) {
        let mut release = text.chars().flat_map(char::to_lowercase);
        if alias.chars().flat_map(char::to_lowercase).all(|ch| {
            release.next().is_some_and(|actual| {
                actual == ch || ch.is_whitespace() && matches!(actual, '.' | '_')
            })
        }) && release.next().is_none()
            && release_alias_cut(alias, 0) < alias.len()
        {
            return AliasSpelling::Scene;
        }
    }
    AliasSpelling::Normalized
}

fn status_rank(status: &str) -> u8 {
    match status {
        "CURRENT" => 0,
        "REPEATING" => 1,
        "PAUSED" => 2,
        "PLANNING" => 3,
        "COMPLETED" => 4,
        _ => 5,
    }
}

pub(crate) fn build_matchers(db: &Db) -> Vec<Matcher> {
    build_matchers_from_inputs(read_matcher_inputs(db, None).unwrap_or_else(|e| {
        log_matcher_read_error(&e);
        Vec::new()
    }))
}

fn log_matcher_read_error(e: &anyhow::Error) {
    log::warn!(
        "matcher build could not read the list, matching is off until the next list mutation: {e}"
    );
}

fn read_matcher_inputs(db: &Db, media_ids: Option<&[i64]>) -> anyhow::Result<Vec<MatcherInput>> {
    let c = db.0.lock();
    let query =
        "SELECT m.id,m.title_english,m.title_romaji,m.title_native,m.episodes,m.format,e.status
                 FROM list_entry e JOIN media m ON m.id = e.media_id";
    let query = if media_ids.is_some() {
        format!("{query} WHERE e.media_id IN (SELECT value FROM json_each(?1))")
    } else {
        format!("{query} WHERE ?1 IS NULL")
    };
    let mut statement = c.prepare_cached(&query)?;
    let ids = media_ids.map(serde_json::to_string).transpose()?;
    let mut inputs = statement
        .query_map([ids], |row| {
            #[cfg(test)]
            MATCHER_WORK.set((MATCHER_WORK.get().0 + 1, MATCHER_WORK.get().1));
            Ok(MatcherInput {
                media_id: row.get(0)?,
                titles: [row.get(1)?, row.get(2)?, row.get(3)?],
                episodes: row.get(4)?,
                format: row.get(5)?,
                status_rank: status_rank(&row.get::<_, String>(6)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    drop(c);
    inputs.sort_by_key(|input| (input.status_rank, input.media_id));
    Ok(inputs)
}

/// media_ids limits checks to media upserts. List changes require None.
pub(crate) fn refresh_matchers(
    db: &Db,
    previous: &Arc<Vec<Matcher>>,
    media_ids: Option<&[i64]>,
) -> Arc<Vec<Matcher>> {
    if media_ids.is_some_and(<[i64]>::is_empty) {
        return previous.clone();
    }
    let inputs = match read_matcher_inputs(db, media_ids) {
        Ok(inputs) => inputs,
        Err(e) => {
            log_matcher_read_error(&e);
            return Arc::new(Vec::new());
        }
    };
    if media_ids.is_some() {
        if previous
            .first()
            .and_then(|matcher| matcher.source.as_ref())
            .is_some_and(|source| {
                inputs.iter().all(|input| {
                    source
                        .binary_search_by_key(&(input.status_rank, input.media_id), |old| {
                            (old.status_rank, old.media_id)
                        })
                        .is_ok_and(|index| source[index] == *input)
                })
            })
        {
            return previous.clone();
        }
        return refresh_matchers(db, previous, None);
    }
    if previous
        .first()
        .and_then(|matcher| matcher.source.as_ref())
        .is_some_and(|source| source.as_ref() == &inputs)
    {
        return previous.clone();
    }
    let matchers = build_matchers_from_inputs(inputs);
    if matchers.is_empty() && previous.is_empty() {
        previous.clone()
    } else {
        Arc::new(matchers)
    }
}

fn build_matchers_from_inputs(inputs: Vec<MatcherInput>) -> Vec<Matcher> {
    // Keep construction synchronous with the list write without starving the runtime.
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        return tokio::task::block_in_place(|| normalize_matcher_inputs(inputs));
    }
    normalize_matcher_inputs(inputs)
}

fn normalize_matcher_inputs(inputs: Vec<MatcherInput>) -> Vec<Matcher> {
    #[cfg(test)]
    MATCHER_WORK.set((MATCHER_WORK.get().0, MATCHER_WORK.get().1 + inputs.len()));
    let inputs = Arc::new(inputs);
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs.iter() {
        let mut variants = Vec::new();
        let mut norms = Vec::new();
        for v in input.titles.iter().flatten() {
            if !v.trim().is_empty() {
                // List title numbers belong to the name. Only releases lose episode suffixes.
                let n = catalog_title_norm(v);
                if !n.is_empty() {
                    variants.push(v.to_string());
                    norms.push(n);
                }
            }
        }
        if norms.is_empty() {
            continue;
        }
        let norm_seasons = variants
            .iter()
            .zip(&norms)
            .map(|(variant, norm)| catalog_season_ordinal(variant, norm))
            .collect();
        let explicit_season = variants.iter().find_map(|variant| catalog_season(variant));
        out.push(Matcher {
            media_id: input.media_id,
            display: input
                .titles
                .iter()
                .flatten()
                .find(|title| !title.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| format!("#{}", input.media_id)),
            variants,
            episodes: input.episodes,
            format: input.format.clone(),
            norm_seasons,
            explicit_season,
            norms,
            status_rank: input.status_rank,
            title_patterns: OnceLock::new(),
            season_patterns: OnceLock::new(),
            source: Some(inputs.clone()),
        });
    }
    out
}

fn title_pattern(variant: &str) -> Option<Regex> {
    let mut previous = "";
    let words = variant
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let token = if previous.eq_ignore_ascii_case("part")
                && word
                    .parse::<u32>()
                    .is_ok_and(|n| (1..=MAX_SEASON).contains(&n))
            {
                format!("0*{}", canon_ordinal(word))
            } else {
                regex::escape(word)
            };
            previous = word;
            token
        })
        .collect::<Vec<_>>();
    if words.is_empty() {
        return None;
    }
    Regex::new(&format!(
        r"(?i)(?:^|[^\p{{L}}\p{{N}}])(?P<title>{}|{})(?:$|[^\p{{L}}\p{{N}}])",
        variant
            .trim()
            .split(' ')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(r"[ ._]"),
        words.join(r"[^\p{L}\p{N}]+")
    ))
    .ok()
}

/// Some true means a prefix match. Some false means an interior or suffix match.
fn contains_tokens(long: &str, short: &str) -> Option<bool> {
    if short.is_empty() || short.len() > long.len() {
        return None;
    }
    if long.starts_with(short)
        && (long.len() == short.len() || long.as_bytes()[short.len()] == b' ')
    {
        return Some(true);
    }
    let bytes = long.as_bytes();
    let mut from = 0;
    while let Some(i) = long[from..].find(short) {
        let start = from + i;
        let end = start + short.len();
        let left_ok = start > 0 && bytes[start - 1] == b' ';
        let right_ok = end == long.len() || bytes[end] == b' ';
        if left_ok && right_ok {
            return Some(false);
        }
        // Advance by a character to preserve UTF-8 boundaries.
        from = start + long[start..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn norm_match_tier(norm: &str, cand: &str) -> Option<u8> {
    if norm == cand {
        return Some(3);
    }
    let (short, long) = if norm.len() <= cand.len() {
        (norm, cand)
    } else {
        (cand, norm)
    };
    if short.len() < 4 {
        return None;
    }
    match contains_tokens(long, short) {
        Some(true) => Some(2),
        Some(false) if short.len() >= 8 && short.contains(' ') => Some(1),
        _ => None,
    }
}

pub(crate) fn release_norm_matches(candidate: &str, catalog: &str) -> bool {
    candidate == catalog
        || candidate.strip_suffix(" 1") == Some(catalog)
        || candidate
            .rsplit_once(' ')
            .is_some_and(|(stem, year)| stem == catalog && year.parse().is_ok_and(looks_like_year))
        || catalog.rsplit_once(" part ").is_some_and(|(stem, part)| {
            part_number(part).is_some_and(|part| {
                candidate
                    .rsplit_once(' ')
                    .is_some_and(|(base, suffix)| base == stem && suffix.parse::<u32>() == Ok(part))
            })
        })
        || same_numbered_part(candidate, catalog)
        || native_season(catalog).is_some_and(|(marker, season)| {
            let mut stem = catalog.to_string();
            stem.replace_range(marker, " ");
            let stem = normalize(&stem);
            candidate.rsplit_once(' ').is_some_and(|(base, number)| {
                (base == stem || base == catalog) && number.parse::<u32>() == Ok(season)
            })
        })
}

fn same_numbered_part(candidate: &str, catalog: &str) -> bool {
    candidate
        .rsplit_once(" part ")
        .zip(catalog.rsplit_once(" part "))
        .is_some_and(|((base, first), (stem, second))| {
            base == stem && part_number(first).is_some_and(|n| Some(n) == part_number(second))
        })
}

fn parse_season_marker(raw: &str) -> Option<u32> {
    RE_SEASON_EP
        .captures(&raw.replace('_', " "))
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|&n| (1..=MAX_SEASON).contains(&n))
}

fn parse_season_word(raw: &str) -> Option<u32> {
    let lower = raw.to_lowercase().replace('_', " ");
    let n = RE_NTH_SEASON
        .captures(&lower)
        .or_else(|| RE_SEASON_N.captures(&lower))?
        .name("n")?;
    n.as_str()
        .parse::<u32>()
        .ok()
        .filter(|&v| (1..=MAX_SEASON).contains(&v))
}

fn parse_season_token(raw: &str) -> Option<u32> {
    RE_SEASON_TOKEN
        .captures(&raw.replace('_', " "))
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|&n| (1..=MAX_SEASON).contains(&n))
}

pub(crate) fn catalog_season(title: &str) -> Option<u32> {
    let title = fold_digits(title);
    parse_season_word(&title)
        .or_else(|| parse_season_token(&title))
        .or_else(|| native_season(&title).map(|(_, number)| number))
        .or_else(|| {
            normalize(&title)
                .rsplit_once(" part ")
                .and_then(|(_, part)| part_number(part))
        })
}

fn part_number(part: &str) -> Option<u32> {
    part.parse::<u32>()
        .ok()
        .filter(|n| (1..=MAX_SEASON).contains(n))
        .or_else(|| roman_to_u32(part))
}

fn native_season(title: &str) -> Option<(Range<usize>, u32)> {
    let marker = RE_NATIVE_SEASON.captures(title)?;
    let number = if let Some(number) = marker.name("number") {
        number.as_str().parse().ok()?
    } else {
        let roman = marker.name("roman")?.as_str().to_lowercase();
        "ⅱⅲⅳⅴⅵⅶⅷⅸⅹⅺⅻ".chars().position(|c| roman.starts_with(c))? as u32 + 2
    };
    (1..=MAX_SEASON)
        .contains(&number)
        .then(|| (marker.get(0).unwrap().range(), number))
}

fn season_aliases(title: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    if let Some(words) = RE_SEASON_WORDS
        .captures(title)
        .and_then(|c| c.name("words"))
        .or_else(|| RE_SEASON_TOKEN.find(title))
    {
        let mut alias = title.to_owned();
        alias.replace_range(words.range(), " ");
        aliases.push(alias);
    }
    if let Some((marker, _)) = native_season(title) {
        let mut alias = title.to_owned();
        alias.replace_range(marker.clone(), " ");
        aliases.push(alias);
        // Attached numerals can precede a translated subtitle.
        if !title[marker.clone()].starts_with('第') {
            aliases.push(title[..marker.start].to_owned());
            aliases.push(title[..marker.end].to_owned());
        }
    }
    if aliases.is_empty() {
        if let Some((stem, suffix)) = title.rsplit_once(' ') {
            if part_number(&suffix.to_ascii_lowercase()).is_some() {
                let stem = stem
                    .strip_suffix(" Part")
                    .or_else(|| stem.strip_suffix(" part"))
                    .unwrap_or(stem);
                aliases.push(stem.trim_end_matches([':', '：', ' ']).to_owned());
            }
        }
    }
    aliases
}

fn release_season(body: &str, groups: &str, title_end: usize) -> Option<u32> {
    let (scene, named, token) = body_season_markers(body, title_end);
    scene
        .or(named)
        .or(token)
        .or_else(|| native_release_season(body, title_end))
        .or_else(|| leading_release_tags(groups, None).season)
}

fn body_season_markers(body: &str, title_end: usize) -> (Option<u32>, Option<u32>, Option<u32>) {
    let normalized = body.replace('_', " ");
    let mut markers = Cow::Borrowed(normalized.as_str());
    for tag in RE_BRACKETS.find_iter(&normalized) {
        if tag.start() == 0 || tag.end() <= title_end {
            continue;
        }
        let inner = tag
            .as_str()
            .trim_matches(['[', ']', '(', ')', '【', '】'])
            .trim();
        let first_season = [
            RE_SEASON_WORDS
                .captures(inner)
                .and_then(|c| c.name("words")),
            RE_SEASON_TOKEN.find(inner),
            RE_SEASON_EP.find(inner),
        ]
        .into_iter()
        .flatten()
        .map(|m| m.start())
        .min();
        if let Some(start) = first_season.filter(|start| *start > 0) {
            let prefix = mask_tag_words(&mask_release_format(&inner[..start], true));
            let prefix = PACK_WORDS.replace_all(&prefix, " ");
            if prefix.chars().any(char::is_alphabetic) {
                // A season inside a release group's name does not identify the show.
                markers
                    .to_mut()
                    .replace_range(tag.range(), &" ".repeat(tag.len()));
            }
        }
    }
    let end = marked_episode_bounds(&markers[title_end..])
        .map_or(markers.len(), |(_, end)| title_end + end);
    let (context, tail) = markers.split_at(end);
    let scene = parse_season_marker(context);
    let named = parse_season_word(context);
    let token = parse_season_token(context);
    let trailing_tag = (scene.is_none() && named.is_none() && token.is_none() && !tail.is_empty())
        .then(|| leading_release_tags(tail, None).season)
        .flatten();
    (scene, named.or(trailing_tag), token)
}

fn native_release_season(body: &str, title_end: usize) -> Option<u32> {
    let prefix = marked_release_prefix_after(body, title_end).unwrap_or(body);
    native_season(prefix).map(|(_, number)| number)
}

fn starts_release_alias(body: &str, span: &Range<usize>) -> bool {
    body[..span.start]
        .trim_matches(release_separator)
        .chars()
        .all(|c| matches!(c, '[' | '(' | '【'))
}

fn season_alias_suffix(body: &str, span: &Range<usize>) -> bool {
    release_alias_suffix(body, span)
        || (starts_release_alias(body, span)
            && marked_release_prefix(&body[span.end..].replace('_', " ")).is_some_and(|prefix| {
                if normalize(prefix).starts_with("final season") {
                    return false;
                }
                let prefix = mask_season_words(prefix, None);
                let prefix = RE_NATIVE_SEASON.replace_all(&prefix, " ");
                let prefix = RE_SEASON_TOKEN.replace_all(&prefix, " ");
                let prefix = mask_release_format(&prefix, true);
                let prefix = RELEASE_WORDS.replace_all(&prefix, " ");
                RE_EMPTY_BRACKETS
                    .replace_all(&prefix, " ")
                    .trim_matches(release_separator)
                    .is_empty()
            }))
}

fn release_alias_suffix(body: &str, span: &Range<usize>) -> bool {
    if !starts_release_alias(body, span) {
        return false;
    }
    let tail = mask_dates(&strip_ext(&body[span.end..])).replace('_', " ");
    let tail = mask_release_format(&tail, true);
    let tail = mask_brackets(&tail);
    if normalize(&tail).starts_with("final season ")
        || marked_release_prefix(&tail).is_some_and(|prefix| {
            prefix
                .trim_matches(release_separator)
                .parse::<u32>()
                .is_ok_and(|number| (1..=MAX_SEASON).contains(&number))
        })
    {
        return false;
    }
    let tail = mask_season_words(&tail, None);
    let tail = RE_NATIVE_SEASON.replace_all(&tail, " ");
    let tail = RE_SEASON_BARE.replace_all(&tail, " ");
    let tail = PACK_WORDS.replace_all(&tail, " ");
    let tail = RELEASE_WORDS.replace_all(&tail, " ");
    let tail = RE_EXPLICIT_EP.replace_all(&tail, " ");
    let tail = RE_REV_TAIL.replace(&tail, " ");
    // A remaining numeric field can be an episode, range, revision or year.
    normalize(&tail).split_whitespace().all(|word| {
        let (number, revision) = word.split_once('v').unwrap_or((word, "0"));
        number.parse::<u32>().is_ok() && revision.parse::<u32>().is_ok()
    })
}

fn catalog_season_ordinal(variant: &str, norm: &str) -> Option<u32> {
    let ordinal = norm_season_ordinal(norm);
    catalog_season(variant).or(ordinal)
}

fn norm_season_ordinal(norm: &str) -> Option<u32> {
    #[cfg(test)]
    ORDINAL_PARSES.set(ORDINAL_PARSES.get() + 1);
    let words: Vec<&str> = norm.split_whitespace().collect();
    if let Some(ordinal) = norm
        .rsplit_once(" part ")
        .and_then(|(_, part)| part_number(part))
    {
        return Some(ordinal);
    }
    if let Some(ordinal) = words.last().and_then(|word| roman_to_u32(word)) {
        return Some(ordinal);
    }
    if words.len() >= 2 && words[words.len() - 2] == "no" {
        return None;
    }
    words.last().and_then(|w| {
        if w.len() <= 2 {
            w.parse::<u32>()
                .ok()
                .filter(|&n| (1..=MAX_SEASON).contains(&n))
        } else {
            None
        }
    })
}

/// Require two letters to avoid treating single-letter titles as seasons.
fn roman_to_u32(s: &str) -> Option<u32> {
    if s.len() < 2 {
        return None;
    }
    let valid = ['i', 'v', 'x', 'l', 'c', 'd', 'm'];
    if s.is_empty() || !s.bytes().all(|b| valid.contains(&(b as char))) {
        return None;
    }
    let mut total = 0u32;
    let mut prev = 0u32;
    for c in s.chars().rev() {
        let v = match c {
            'i' => 1,
            'v' => 5,
            'x' => 10,
            'l' => 50,
            'c' => 100,
            'd' => 500,
            'm' => 1000,
            _ => return None,
        };
        total = if v < prev {
            total.checked_sub(v)?
        } else {
            total + v
        };
        prev = v;
    }
    (1..=30).contains(&total).then_some(total)
}

#[derive(Default, PartialEq, Eq, PartialOrd, Ord)]
struct AliasEvidence {
    direct: usize,
    stem: usize,
    exact: usize,
}

impl AliasEvidence {
    fn length(&self) -> usize {
        self.direct.max(self.stem)
    }
}

type TiebreakKey = (
    bool,
    u8,
    usize,
    usize,
    std::cmp::Reverse<u8>,
    usize,
    std::cmp::Reverse<i64>,
);

pub(crate) fn strip_release_groups<'a>(mut title: &'a str, matchers: &[Matcher]) -> &'a str {
    while let Some(tag) = RE_BRACKETS
        .find(title.trim_start())
        .filter(|tag| tag.start() == 0)
    {
        let trimmed = title.trim_start();
        let body = trimmed[tag.end()..].trim_start_matches(|c: char| c.is_whitespace() || c == '_');
        if strip_ext(body).trim().is_empty() {
            break;
        }
        let tag_norm = normalize(tag.as_str());
        let body_norm = normalize(body);
        let prefix = trimmed.to_lowercase();
        let release_suffix = body.starts_with(['-', '·', '[', '(', '#', '【'])
            || marked_release_prefix(body).is_some_and(|prefix| prefix.is_empty())
            || parse_season_word(body).is_some_and(|_| {
                body.get(..6)
                    .is_some_and(|word| word.eq_ignore_ascii_case("season"))
            })
            || leading_episode(body).is_some();
        let mut main = body;
        while let Some(padding) = RE_BRACKETS.find(main).filter(|tag| tag.start() == 0) {
            if !normalize(&mask_release_format(padding.as_str(), true)).is_empty() {
                break;
            }
            main =
                main[padding.end()..].trim_start_matches(|c: char| c.is_whitespace() || c == '_');
        }
        let repeats_title =
            main.len() != body.len() && contains_tokens(&normalize(main), &tag_norm) == Some(true);
        if !repeats_title
            && (release_suffix || contains_tokens(&body_norm, &tag_norm) != Some(true))
            && matchers.iter().any(|matched| {
                matched.variants.iter().any(|variant| {
                    let variant = variant.trim().to_lowercase();
                    let same_bracketed_name = RE_BRACKETS.find(&variant).is_some_and(|bracket| {
                        bracket.start() == 0 && normalize(bracket.as_str()) == tag_norm
                    });
                    (variant.starts_with(['[', '(', '【'])
                        && prefix.strip_prefix(&variant).is_some_and(|tail| {
                            tail.chars().next().is_none_or(|c| !c.is_alphanumeric())
                        }))
                        || ((tag.as_str().starts_with('【') || release_suffix)
                            && matched.norms.iter().any(|norm| norm == &tag_norm))
                        || (release_suffix && same_bracketed_name)
                })
            })
        {
            break;
        }
        title = body;
    }
    title
}

fn marked_episode_bounds(s: &str) -> Option<(usize, usize)> {
    let explicit = RE_EXPLICIT_EP.find(s).map(|m| (m.start(), m.end()));
    let dashed = dashed_episode(s).map(|(dash, number)| (dash, number.end()));
    let bracket = bracket_episode_match(s).map(|tag| (tag.span.start(), tag.span.end()));
    explicit.into_iter().chain(dashed).chain(bracket).min()
}

pub(crate) fn release_alias_cut(s: &str, title_end: usize) -> usize {
    let tail = &s[title_end..];
    let bounds = if tail.contains('_') {
        marked_episode_bounds(&tail.replace('_', " "))
    } else {
        marked_episode_bounds(tail)
    };
    bounds
        .map(|(start, _)| title_end + start)
        .filter(|&start| !s[..start].trim_matches(release_separator).is_empty())
        .unwrap_or(s.len())
}

pub(crate) fn alias_before_episode(span: &Range<usize>, cut: usize, exact: bool) -> bool {
    span.start < cut && (span.end <= cut || exact)
}

pub(crate) fn marked_release_prefix(s: &str) -> Option<&str> {
    marked_release_prefix_after(s, 0)
}

fn marked_release_prefix_after(s: &str, title_end: usize) -> Option<&str> {
    let (start, _) = marked_episode_bounds(&s[title_end..])?;
    Some(s[..title_end + start].trim_end_matches(release_separator))
}

/// Match the release season against any title variant. Unnumbered releases default to season one.
pub(crate) fn match_title<'a>(
    matchers: &'a [Matcher],
    title: &str,
    url: &str,
) -> Option<&'a Matcher> {
    let base = basename(url);
    let folded_title = fold_digits(title);
    let folded_base = fold_digits(&base);
    let title = strip_release_groups(&folded_title, matchers);
    let base = strip_release_groups(&folded_base, matchers);
    let marked_title = has_marked_episode_tail(title);
    let marked_base = has_marked_episode_tail(base);
    let (clean, title_with_season_word) = release_title_norms(title);
    let (clean_base, base_with_season_word) = release_title_norms(base);
    let title_markers = title.replace('_', " ");
    let base_markers = base.replace('_', " ");
    let display_norm = normalize(title);
    let path_norm = normalize(base);
    let literal_title_end = |raw: &str, norm: &str| {
        let cut = release_alias_cut(raw, 0);
        matchers
            .iter()
            .filter(|m| cut < raw.len() && m.could_contain_alias(norm))
            .map(|m| m.literal_release_title_end(raw, cut))
            .max()
            .unwrap_or_default()
    };
    let title_end = literal_title_end(title, &display_norm);
    let base_end = literal_title_end(base, &path_norm);
    let title_prefix = marked_release_prefix_after(&title_markers, title_end).unwrap_or_default();
    let base_prefix = marked_release_prefix_after(&base_markers, base_end).unwrap_or_default();
    let title_alias = catalog_title_norm(title_prefix);
    let base_alias = catalog_title_norm(base_prefix);
    let (title_scene, title_named, title_token) = body_season_markers(title, title_end);
    let (base_scene, base_named, base_token) = body_season_markers(base, base_end);
    let cand_season = title_scene.or(base_scene);
    let season_ord = cand_season
        .or(title_named)
        .or(base_named)
        .or(title_token)
        .or(base_token);
    let native =
        native_release_season(title, title_end).or_else(|| native_release_season(base, base_end));
    if [
        (
            title,
            title_end,
            title_scene.or(title_named).or(title_token),
        ),
        (base, base_end, base_scene.or(base_named).or(base_token)),
    ]
    .into_iter()
    .any(|(release, title_end, season)| {
        native_release_season(release, title_end)
            .zip(season)
            .is_some_and(|(native, season)| native != season)
    }) {
        return None;
    }
    let season_ord = season_ord
        .or(native)
        .or_else(|| {
            leading_release_tags(&folded_title[..folded_title.len() - title.len()], None).season
        })
        .or_else(|| {
            leading_release_tags(&folded_base[..folded_base.len() - base.len()], None).season
        });
    let title_cut = release_alias_cut(title, title_end);
    let base_cut = release_alias_cut(base, base_end);
    let title_norm = norm_title(title);
    let base_norm = norm_title(base);
    let bare_title_norms = [
        (!marked_title).then_some(&title_norm),
        (!marked_base).then_some(&base_norm),
    ];
    let exact_unmarked_title = season_ord.is_none()
        && bare_title_norms
            .iter()
            .flatten()
            .any(|norm| matchers.iter().any(|m| m.norms.contains(norm)));
    let evidence: Vec<_> = matchers
        .iter()
        .map(|m| {
            let mut candidates = [
                (title, &display_norm, title_cut),
                (base, &path_norm, base_cut),
            ]
            .into_iter()
            .filter(|(_, norm, _)| m.could_contain_alias(norm))
            .peekable();
            let literal = season_ord.is_some()
                && candidates.peek().is_some()
                && literal_numbered_title(matchers, m);
            let alias = candidates
                .map(|(raw, _, cut)| m.release_alias_evidence(raw, cut, season_ord))
                .fold(AliasEvidence::default(), |best, candidate| AliasEvidence {
                    exact: best.exact.max(candidate.exact),
                    ..best.max(candidate)
                });
            // A scene marker can corroborate a shortened Roman ordinal title.
            let roman_stem = alias.direct == 0
                && alias.stem > 0
                && m.norms.iter().any(|norm| {
                    norm.rsplit_once(' ')
                        .is_some_and(|(_, suffix)| roman_to_u32(suffix) == season_ord)
                });
            (m, alias, literal && !roman_stem)
        })
        .collect();
    let conflicting_alias = evidence
        .iter()
        .filter_map(|(m, alias, literal)| {
            let season = season_ord?;
            (alias.direct > 0
                && (m.explicit_season.is_some() || m.norm_seasons.iter().any(Option::is_some))
                && !matches_catalog_season(m, season, *literal))
            .then_some(alias.direct)
        })
        .max()
        .unwrap_or_default();
    let bare_owned = |m: &Matcher| {
        bare_title_norms
            .iter()
            .flatten()
            .any(|norm| m.norms.contains(norm))
    };
    let candidates = [
        (title_alias, None, 6, false),
        (base_alias, None, 6, false),
        (marked_prefix_norm(title_prefix), None, 0, false),
        (marked_prefix_norm(base_prefix), None, 0, false),
        (pack_title_norm(title), None, 4, false),
        (plural_season_pack_norm(title), None, 0, false),
        (pack_title_norm(base), None, 4, false),
        (plural_season_pack_norm(base), None, 0, false),
        (clean, None, 0, false),
        (clean_base, None, 0, false),
        (catalog_title_norm(title), Some(title), 5, false),
        (catalog_title_norm(base), Some(base), 5, false),
        (title_norm.clone(), Some(title), 5, false),
        (base_norm.clone(), Some(base), 5, false),
        (clean_title_inner(title, false).0, None, 4, false),
        (clean_title_inner(base, false).0, None, 4, false),
        (
            clean_title_inner(&title.replace(['【', '】'], " "), false).0,
            None,
            if title.trim_start().starts_with('【') {
                5
            } else {
                3
            },
            false,
        ),
        (
            clean_title_inner(&base.replace(['【', '】'], " "), false).0,
            None,
            if base.trim_start().starts_with('【') {
                5
            } else {
                3
            },
            false,
        ),
        (title_with_season_word.unwrap_or_default(), None, 4, true),
        (base_with_season_word.unwrap_or_default(), None, 4, true),
        (literal_season_range_norm(title), None, 4, true),
        (literal_season_range_norm(base), None, 4, true),
    ];
    let mut best: Option<(TiebreakKey, &Matcher)> = None;
    for (m, alias, literal) in &evidence {
        if alias.length() == 0
            || (exact_unmarked_title && !bare_owned(m))
            || alias.length() < conflicting_alias
            || season_ord.is_some_and(|season| !matches_catalog_season(m, season, *literal))
        {
            continue;
        }
        let season_match = cand_season.is_some_and(|cs| m.norm_seasons.contains(&Some(cs)));
        let key = (
            season_match,
            7,
            alias.length(),
            alias.exact,
            std::cmp::Reverse(m.status_rank),
            0,
            std::cmp::Reverse(m.media_id),
        );
        if best.as_ref().is_none_or(|(bk, _)| key > *bk) {
            best = Some((key, m));
        }
    }
    let format_prefixes = [
        if title_prefix.is_empty() && !marked_title {
            &title_markers
        } else {
            title_prefix
        },
        if base_prefix.is_empty() && !marked_base {
            &base_markers
        } else {
            base_prefix
        },
    ]
    .into_iter()
    .flat_map(format_prefix_norms)
    .map(|prefix| (prefix, None, 6, false));
    for (cand, raw_title, exact_tier, season_word_is_literal) in
        candidates.into_iter().chain(format_prefixes)
    {
        let exact_only = exact_tier > 0;
        if cand.is_empty() {
            continue;
        }
        let cand_ord = if season_word_is_literal {
            cand_season
        } else {
            season_ord
        }
        .or_else(|| norm_season_ordinal(&cand));
        for (m, alias, literal) in &evidence {
            if alias.length() < conflicting_alias {
                continue;
            }
            if exact_only && !m.norms.iter().any(|n| n == &cand) {
                continue;
            }
            if let Some(raw) = raw_title {
                let marked_episode = if raw == title {
                    marked_title
                } else {
                    marked_base
                };
                if marked_episode && parse_season_marker(raw).is_none() && alias.exact == 0 {
                    continue;
                }
            }
            let expected_season = cand_ord.unwrap_or(1);
            let exact_alias = m.norms.iter().any(|norm| norm == &cand);
            let matches_season = matches_catalog_season(m, expected_season, *literal)
                || (m.explicit_season.is_some() && season_ord.is_none() && exact_alias);
            if !matches_season {
                continue;
            }
            if let Some((tier, nlen)) = m
                .norms
                .iter()
                .filter_map(|n| {
                    if exact_only {
                        // Preserve title numbers before status breaks ties.
                        (n == &cand).then_some((exact_tier, n.len()))
                    } else {
                        let tier = norm_match_tier(n, &cand)
                            .or_else(|| release_norm_matches(&cand, n).then_some(3));
                        tier.map(|t| (t, n.len()))
                    }
                })
                .max()
            {
                let season_match = cand_season.is_some_and(|cs| m.norm_seasons.contains(&Some(cs)));
                let exact = if tier == 1 && alias.exact == 0 {
                    [(title, title_cut), (base, base_cut)]
                        .into_iter()
                        .flat_map(|(raw, cut)| {
                            m.release_alias_spans(raw).into_iter().filter(move |span| {
                                span.start < cut
                                    && m.alias_spelling(raw, span) == AliasSpelling::Exact
                            })
                        })
                        .map(|span| span.len())
                        .max()
                        .unwrap_or_default()
                } else {
                    alias.exact
                };
                let key = (
                    season_match,
                    tier,
                    exact,
                    0,
                    std::cmp::Reverse(m.status_rank),
                    nlen,
                    std::cmp::Reverse(m.media_id),
                );
                if best.as_ref().is_none_or(|(bk, _)| key > *bk) {
                    best = Some((key, m));
                }
            }
        }
    }
    best.map(|(_, m)| m)
}

fn matches_catalog_season(matched: &Matcher, expected: u32, literal: bool) -> bool {
    if let Some(explicit) = matched.explicit_season {
        return explicit == expected;
    }
    matched.norm_seasons.iter().any(|ord| {
        if literal {
            expected == 1
        } else {
            ord.unwrap_or(1) == expected
        }
    })
}

fn literal_numbered_title(matchers: &[Matcher], matched: &Matcher) -> bool {
    let mut stems = matched
        .norms
        .iter()
        .zip(&matched.variants)
        .zip(&matched.norm_seasons)
        .filter_map(|((norm, variant), ordinal)| {
            let (stem, suffix) = norm.rsplit_once(' ')?;
            let terminal_ordinal = suffix.parse().ok().or_else(|| roman_to_u32(suffix));
            (terminal_ordinal.is_some()
                && terminal_ordinal == *ordinal
                && catalog_season(variant).is_none())
            .then_some(stem)
        })
        .peekable();
    stems.peek().is_some()
        && !stems.any(|stem| {
            matchers.iter().any(|other| {
                matched.media_id != other.media_id && other.norms.iter().any(|n| n == stem)
            })
        })
}

#[cfg(test)]
fn clean_title(s: &str) -> String {
    release_title_norms(s).0
}

/// The alternate needs a catalog title that ends in the literal word Season.
pub(crate) fn release_title_norms(s: &str) -> (String, Option<String>) {
    clean_title_inner(s, true)
}

fn clean_release_text(s: &str, strip_channels: bool) -> String {
    let s = mask_dates(&strip_ext(s)).replace('_', " ");
    let s = RE_BRACKETS.replace_all(&s, |c: &regex::Captures| {
        let inner = &c[0];
        if let Some(words) = RE_SEASON_WORDS
            .captures(inner)
            .and_then(|c| c.name("words"))
        {
            format!(" {} ", words.as_str())
        } else if let Some(season) = RE_SEASON_EP.captures(inner).and_then(|c| c.get(1)) {
            format!(" S{} ", season.as_str())
        } else if let Some(token) = RE_SEASON_TOKEN.find(inner) {
            format!(" {} ", token.as_str())
        } else {
            " ".to_string()
        }
    });
    let s = mask_release_format(&s, strip_channels);
    RE_RES.replace_all(&s, " ").into_owned()
}

fn clean_title_inner(s: &str, strip_channels: bool) -> (String, Option<String>) {
    let s = clean_release_text(s, strip_channels);
    let release = s.trim_end_matches(release_separator);
    let (stripped, alternate) = strip_episode_tail(release);
    // Preserve numeric titles when stripping would leave nothing.
    let normed = normalize(stripped);
    let out = if normed.is_empty() {
        normalize(&s)
    } else {
        normed
    };
    (
        season_ordinals(&out),
        alternate.map(|s| season_ordinals(&normalize(s))),
    )
}

pub(crate) fn marked_prefix_norm(prefix: &str) -> String {
    let norm = season_ordinals(&normalize(&clean_release_text(prefix, true)));
    if let Some((stem, year)) = norm.rsplit_once(' ') {
        if year.parse().is_ok_and(looks_like_year) {
            return stem.to_string();
        }
    }
    norm
}

pub(crate) fn format_prefix_norms(s: &str) -> impl Iterator<Item = String> + '_ {
    RE_FORMAT
        .find_iter(s)
        .filter(|tag| tag.start() > 0 && marked_prefix_norm(&s[tag.start()..]).is_empty())
        .map(|tag| catalog_title_norm(&s[..tag.start()]))
}

fn plural_season_pack_norm(raw: &str) -> String {
    let text = RE_BRACKETS.replace_all(raw, " ").replace('_', " ");
    if let Some(range) = RE_SEASON_RANGE
        .captures(&text)
        .filter(|range| range.name("plural").is_some() && valid_season_range(range, ""))
    {
        let prefix = &text[..range.name("range").unwrap().start()];
        return format!(
            "{} {}",
            catalog_title_norm(prefix),
            canon_ordinal(&range["first"])
        );
    }
    String::new()
}

fn pack_title_norm(raw: &str) -> String {
    let text = strip_ext(raw).replace('_', " ");
    let text = RE_BRACKETS.replace_all(&text, " ");
    if !PACK_WORDS.is_match(&text) {
        return String::new();
    }
    let text = mask_release_format(&text, true);
    let text = PACK_WORDS.replace_all(&text, " ");
    let trimmed = text.trim_end_matches(release_separator);
    season_ordinals(&normalize(trimmed))
}

fn literal_season_range_norm(raw: &str) -> String {
    if !raw
        .as_bytes()
        .windows(6)
        .any(|word| word.eq_ignore_ascii_case(b"season"))
    {
        return String::new();
    }
    let normalized = mask_release_format(&raw.replace('_', " "), true);
    let Some(span) = episode_ranges(&normalized)
        .next()
        .and_then(|range| range.name("range"))
    else {
        return String::new();
    };
    let prefix = normalized[..span.start()].trim_end_matches(release_separator);
    let norm = release_title_norms(prefix).0;
    if norm.ends_with(" season") {
        norm
    } else {
        String::new()
    }
}

fn has_marked_episode_tail(raw: &str) -> bool {
    let text = strip_ext(raw).replace('_', " ");
    let text = RE_BRACKETS.replace_all(&text, " ");
    let text = mask_release_format(&text, false);
    let text = RE_RES.replace_all(&text, " ");
    let text = text.trim_end_matches(release_separator);
    RE_EP_TAIL.find(text).is_some_and(|tail| {
        episode_separator(tail.as_str(), preceded_by_season(&text[..tail.start()]))
            || RE_EXPLICIT_EP.is_match(tail.as_str())
    })
}

fn release_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | '_' | '-' | '·' | '–' | '—')
}

fn preceded_by_season(prefix: &str) -> bool {
    prefix
        .split(release_separator)
        .rfind(|word| !word.is_empty())
        .is_some_and(|word| word.eq_ignore_ascii_case("season"))
}

fn episode_separator(tail: &str, follows_season: bool) -> bool {
    let marker = tail.trim_start();
    marker.starts_with('·')
        || marker.strip_prefix(['-', '–', '—']).is_some_and(|rest| {
            !follows_season || tail != marker || rest.starts_with(char::is_whitespace)
        })
}

fn strip_episode_tail(s: &str) -> (&str, Option<&str>) {
    if let Some((_, fraction)) =
        fractional_episodes(s).find(|(_, fraction)| fraction.end == s.len())
    {
        return (&s[..fraction.start], None);
    }
    let Some(tail) = RE_EP_TAIL.find(s) else {
        return (s, None);
    };
    if RE_SEASON_TOKEN
        .find_iter(s)
        .any(|token| token.end() == s.len())
    {
        return (s, None);
    }
    let prefix = &s[..tail.start()];
    let bare_number = tail
        .as_str()
        .trim_start_matches(release_separator)
        .starts_with(|c: char| c.is_ascii_digit());
    if bare_number && preceded_by_season(prefix) && !episode_separator(tail.as_str(), true) {
        let normalized = normalize(prefix);
        if !RE_NTH_SEASON
            .find_iter(&normalized)
            .any(|m| m.end() == normalized.len())
        {
            return (s, Some(prefix));
        }
    }
    (prefix, None)
}

pub(crate) fn catalog_title_norm(s: &str) -> String {
    let s = strip_ext(&fold_digits(s)).replace(['【', '】', '_'], " ");
    let s = RE_BRACKETS.replace_all(&s, " ");
    season_ordinals(&normalize(&s))
}

pub(crate) fn norm_title(s: &str) -> String {
    let s = strip_ext(&fold_digits(s)).replace(['【', '】', '_'], " ");
    let s = RE_BRACKETS.replace_all(&s, " ");
    let format_stripped = mask_release_format(&s, false);
    let res_stripped = RE_RES.replace_all(&format_stripped, " ");
    // A real title such as Opus can also be a codec name.
    let s = if normalize(&res_stripped).is_empty() {
        s
    } else {
        res_stripped
    };
    season_ordinals(&normalize(&s))
}

fn canon_ordinal(s: &str) -> &str {
    let d = s.trim_start_matches('0');
    if d.is_empty() {
        "0"
    } else {
        d
    }
}

fn season_ordinals(normed: &str) -> String {
    // Collapse S02E03 before the bare S02 rule.
    let out = RE_SEASON_EP.replace_all(normed, |c: &regex::Captures| {
        canon_ordinal(c.get(1).map_or("", |m| m.as_str())).to_string()
    });
    let out = RE_NTH_SEASON.replace_all(&out, |c: &regex::Captures| {
        canon_ordinal(c.name("n").map_or("", |m| m.as_str())).to_string()
    });
    let out = RE_SEASON_N.replace_all(&out, |c: &regex::Captures| {
        canon_ordinal(c.name("n").map_or("", |m| m.as_str())).to_string()
    });
    let out = RE_SEASON_TOKEN
        .replace_all(&out, |c: &regex::Captures| {
            canon_ordinal(c.get(1).map_or("", |m| m.as_str())).to_string()
        })
        .into_owned();
    if let Some((stem, part)) = out.rsplit_once(" part ") {
        if part
            .parse::<u32>()
            .is_ok_and(|number| (1..=MAX_SEASON).contains(&number))
        {
            return format!("{stem} part {}", canon_ordinal(part));
        }
    }
    out
}

pub(crate) fn fold_digits(s: &str) -> Cow<'_, str> {
    if s.contains(|c: char| ('０'..='９').contains(&c)) {
        Cow::Owned(
            s.chars()
                .map(|c| {
                    if ('０'..='９').contains(&c) {
                        (b'0' + (c as u32 - '０' as u32) as u8) as char
                    } else {
                        c
                    }
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(s)
    }
}

fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Strip only known extensions to preserve titles such as No.6 and D.Gray-man.
pub(crate) const VIDEO_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv", "mpg", "mpeg", "m2ts",
    "vob", "ogv", "3gp", "rmvb", "asf", "divx",
];

fn strip_ext(s: &str) -> String {
    if let Some(i) = s.rfind('.') {
        let ext = &s[i + 1..];
        if VIDEO_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

pub(crate) fn basename(url: &str) -> String {
    let parsed = reqwest::Url::parse(url).ok();
    let parsed = parsed
        .as_ref()
        .filter(|u| matches!(u.scheme(), "http" | "https" | "file"));
    let path = parsed.map_or(url, |u| u.path());
    let seg = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let decoded = parsed.map_or_else(|| seg.to_string(), |_| percent_decode(seg));
    strip_ext(&decoded)
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            if let Ok(b) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            ) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A title match suppresses the final numeric fallback. Other candidates can still supply an episode.
pub(crate) fn resolve_episode(matched: &Matcher, candidates: &[&str]) -> Option<i64> {
    let mut variant_hit = false;
    let mut fallback = None;
    for cand in candidates {
        let folded = fold_digits(cand);
        if matched.is_season_pack(&folded) {
            variant_hit = true;
            continue;
        }
        let cand = strip_release_groups(&folded, std::slice::from_ref(matched));
        let offset = folded.len() - cand.len();
        let groups = mask_release_format(&folded[..offset].replace('_', " "), true);
        let title = matched.release_title_span_in(&folded, cand);
        let title_removed = title.is_some();
        let normalized_alias = !title_removed && matched.norms.contains(&norm_title(cand));
        let (remainder, _) = episode_remainder(cand, title);
        let masked = mask_brackets(&remainder);
        let position = episode_position(&masked, title_removed);
        if has_cross_season_range(&groups, groups.len())
            || leading_release_extra(&groups, matched.format.as_deref())
            || leading_fractional_release(&groups)
            || episode_ranges(&groups)
                .any(|range| bracketed_range(&groups, range.name("range").unwrap().range()))
            || has_conflicting_episode_markers(&remainder, cand)
            || !is_single_episode(&remainder, position, title_removed || normalized_alias)
        {
            variant_hit = true;
            continue;
        }
        let episode = if normalized_alias && parse_season_marker(cand).is_none() {
            bracket_episode_match(&remainder).map(|tag| tag.number)
        } else {
            episode_from_remainder(&masked, title_removed)
        };
        if let Some(episode) = episode.or_else(|| {
            if offset == 0 || has_conflicting_episode_markers(&groups, &groups) {
                return None;
            }
            let season = matched.release_season(cand, "").or_else(|| {
                candidates.iter().find_map(|candidate| {
                    let folded = fold_digits(candidate);
                    let body = strip_release_groups(&folded, std::slice::from_ref(matched));
                    matched.release_season(body, "")
                })
            });
            leading_release_tags(&folded[..offset], season).episode
        }) {
            if title_removed || normalized_alias {
                return Some(episode);
            }
            fallback = fallback.or(Some(episode));
        }
        variant_hit |= title_removed || normalized_alias;
    }
    if variant_hit {
        None
    } else {
        fallback
    }
}

pub(crate) fn excluded_playback_release(matched: &Matcher, candidate: &str) -> bool {
    let filename = strip_ext(&fold_digits(candidate));
    let candidate = filename.as_str();
    let movie = matched.episodes == Some(1);
    if matched.is_season_pack(candidate) {
        return true;
    }
    let title = matched.release_title_span(candidate);
    let (remainder, _) = episode_remainder(candidate, title.clone());
    let body = strip_release_groups(candidate, std::slice::from_ref(matched));
    let group_end = candidate.len() - body.len();
    let source_body = body;
    let (groups, body) = remainder.split_at(group_end);
    let release_range = |range| is_release_range(candidate, range, group_end);
    if has_cross_season_range(&remainder, group_end) {
        return true;
    }
    let episode = episode_position(&mask_brackets(body), true);
    if starts_with_fractional_episode(body, episode)
        || has_conflicting_episode_markers(body, source_body)
        || (episode.is_none() && has_conflicting_episode_markers(groups, &candidate[..group_end]))
        || leading_fractional_release(groups)
        || has_release_extra(body, matched.format.as_deref(), movie)
        || leading_release_extra(groups, matched.format.as_deref())
    {
        return true;
    }
    if movie {
        return episode_ranges(&remainder).any(|c| release_range(c.name("range").unwrap().range()))
            || (episode.is_none()
                && RE_DATE.captures_iter(candidate).any(|date| {
                    let span = date.name("date").unwrap().range();
                    title
                        .as_ref()
                        .is_none_or(|title| !title.contains(&span.start))
                        && !RE_BRACKETS
                            .find_iter(candidate)
                            .any(|tag| tag.range().contains(&span.start))
                        && valid_date(date)
                }));
    }
    episode_ranges(&remainder).any(|c| release_range(c.name("range").unwrap().range()))
        && resolve_episode(matched, &[candidate]).is_none()
}

pub(crate) fn has_release_extra(s: &str, format: Option<&str>, single_feature: bool) -> bool {
    let s = mask_season_words(s, None);
    let masked = mask_brackets(&s);
    let episode = (!single_feature)
        .then(|| episode_position(&masked, true))
        .flatten();
    let end = episode.unwrap_or(masked.len());
    unrelated_extra(&masked[..end], format)
        || episode.is_some_and(|start| {
            // An episode subtitle can contain extra markers as ordinary words.
            let tail = RELEASE_WORDS.replace_all(&masked[start..], " ");
            whole_extra_tag(&tail, format)
                || RE_SUBTITLE_SEPARATOR
                    .find_iter(tail.trim_end())
                    .last()
                    .is_some_and(|separator| whole_extra_tag(&tail[separator.end()..], format))
        })
        || RE_BRACKETS.find_iter(&s).any(|bracket| {
            whole_extra_tag(bracket.as_str(), format)
                || (single_feature && unrelated_extra(bracket.as_str(), format))
        })
}

fn whole_extra_tag(tag: &str, format: Option<&str>) -> bool {
    let tag = mask_release_format(&tag.replace('_', " "), true);
    let tag = mask_season_words(&tag, None);
    let tag = RE_SEASON_TOKEN.replace_all(&tag, " ");
    let tag = RELEASE_WORDS.replace_all(&tag, " ");
    let metadata = normalize(&RE_EXPLICIT_EP.replace_all(&tag, " "));
    unrelated_extra(&metadata, format)
        && metadata
            .replace("next episode preview", "preview")
            .split_whitespace()
            .all(|word| {
                word.parse::<u32>().is_ok()
                    || RE_EXTRAS.is_match(word)
                    || word.split_once('v').is_some_and(|(number, revision)| {
                        (number.is_empty() || number.parse::<u32>().is_ok())
                            && revision.parse::<u32>().is_ok()
                    })
            })
}

pub(crate) fn leading_release_extra(groups: &str, format: Option<&str>) -> bool {
    RE_BRACKETS
        .find_iter(groups)
        .any(|tag| whole_extra_tag(tag.as_str(), format))
}

pub(crate) fn leading_fractional_release(groups: &str) -> bool {
    fractional_episodes(groups).any(|(_, span)| bracketed_range(groups, span))
        || episode_ranges(groups).any(|range| {
            range.name("fraction").is_some()
                && bracketed_range(groups, range.name("range").unwrap().range())
        })
}

fn unrelated_extra(s: &str, format: Option<&str>) -> bool {
    RE_EXTRAS.captures_iter(&normalize(s)).any(|capture| {
        !matches!(
            (format, &capture["marker"]),
            (Some("OVA"), "ova" | "ovas" | "oad" | "oads")
                | (Some("SPECIAL"), "sp" | "special" | "specials")
        )
    })
}

pub(crate) fn resolve_playback_episode(
    matched: &Matcher,
    candidates: &[&str],
    bound: bool,
) -> Option<i64> {
    if candidates
        .iter()
        .any(|candidate| excluded_playback_release(matched, candidate))
    {
        return None;
    }
    if matched.episodes != Some(1) {
        return resolve_episode(matched, candidates);
    }
    if let Some(episode) = resolve_episode(matched, candidates) {
        return Some(episode);
    }
    (bound
        || candidates.iter().any(|candidate| {
            let folded = fold_digits(candidate);
            matched.has_release_alias(&folded)
                || matched.norms.contains(&norm_title(strip_release_groups(
                    candidate,
                    std::slice::from_ref(matched),
                )))
                || matched
                    .norms
                    .contains(&norm_title(&RE_BRACKETS.replace_all(candidate, " ")))
                || matched.norms.contains(&release_title_norms(candidate).0)
        }))
    .then_some(1)
}

fn looks_like_year(n: i64) -> bool {
    use chrono::Datelike;
    (1900..=chrono::Utc::now().year() as i64 + 1).contains(&n)
}

#[derive(Default)]
struct LeadingTag {
    season: Option<u32>,
    episode: Option<i64>,
    pack: bool,
}

#[derive(Default)]
pub(crate) struct LeadingTags {
    season: Option<u32>,
    episode: Option<i64>,
    // A recognized pack must agree with the target season.
    pub(crate) pack_matches: Option<bool>,
}

pub(crate) fn leading_release_tags(groups: &str, body_season: Option<u32>) -> LeadingTags {
    if groups.is_empty() {
        return LeadingTags::default();
    }
    let groups = mask_release_format(&groups.replace('_', " "), true);
    let groups = mask_tag_words(&groups);
    let mut uncertain_season = false;
    let tags: Vec<_> = RE_BRACKETS
        .find_iter(&groups)
        .map(|tag| {
            let stripped = PACK_WORDS.replace_all(tag.as_str(), " ");
            let mut pack = matches!(stripped, Cow::Owned(_));
            let tag = stripped.as_ref();
            let inner = tag.trim_matches(['[', ']', '(', ')', '【', '】']).trim();
            if let Some(episode) =
                bracket_episode_match(tag).filter(|ep| ep.marked && ep.span.len() == tag.len())
            {
                let season = parse_season_marker(inner).or_else(|| parse_season_word(inner));
                if season.is_none()
                    && (RE_SEASON_EP.is_match(inner) || RE_SEASON_WORDS.is_match(inner))
                {
                    uncertain_season = true;
                    return LeadingTag::default();
                }
                return LeadingTag {
                    season,
                    episode: Some(episode.number),
                    pack,
                };
            }
            let season = if RE_SEASON_WORDS.captures(inner).is_some_and(|c| {
                c.name("words").is_some_and(|words| {
                    let tail = inner[words.end()..].trim_matches(release_separator);
                    words.start() == 0 && (tail.is_empty() || tail.parse().is_ok_and(episode_noise))
                })
            }) {
                parse_season_word(inner)
            } else if RE_SEASON_TOKEN
                .find(inner)
                .is_some_and(|token| token.len() == inner.len())
            {
                parse_season_token(inner)
            } else if episode_ranges(tag).any(|range| {
                range.name("fraction").is_none()
                    && bracketed_range(tag, range.name("range").unwrap().range())
            }) && !has_cross_season_range(tag, tag.len())
            {
                pack = true;
                parse_season_marker(inner)
                    .or_else(|| parse_season_word(inner))
                    .or_else(|| parse_season_token(inner))
            } else if pack && inner.is_empty() {
                None
            } else {
                return LeadingTag::default();
            };
            uncertain_season |= season.is_none()
                && (RE_SEASON_TOKEN.is_match(inner)
                    || RE_SEASON_EP.is_match(inner)
                    || RE_SEASON_WORDS.is_match(inner));
            LeadingTag {
                season,
                episode: None,
                pack,
            }
        })
        .collect();
    let season = tags.iter().find_map(|tag| tag.season);
    let target = body_season.or(season);
    let unscoped_matches = !uncertain_season
        && tags
            .iter()
            .filter_map(|tag| tag.season)
            .all(|scope| Some(scope) == target);
    let matches = |tag: &LeadingTag| {
        tag.season
            .map_or(unscoped_matches, |scope| Some(scope) == target)
    };
    let episode = tags
        .iter()
        .filter(|tag| matches(tag))
        .find_map(|tag| tag.episode);
    let pack_matches = tags
        .iter()
        .filter(|tag| tag.pack)
        .map(matches)
        .reduce(|a, b| a && b);
    LeadingTags {
        season,
        episode,
        pack_matches,
    }
}

struct BracketEpisode<'a> {
    number: i64,
    span: regex::Match<'a>,
    marked: bool,
}

fn bracket_episode_match(s: &str) -> Option<BracketEpisode<'_>> {
    RE_BRACKET_EP
        .captures_iter(s)
        .map(|c| (c, false))
        .chain(RE_BRACKET_SEASON_EP.captures_iter(s).map(|c| (c, true)))
        .filter_map(|(c, season_dash)| {
            let number = c.name("number")?;
            let number = number.as_str().parse::<i64>().ok()?;
            let marked = c.name("marker").is_some();
            if !marked && c.name("more").is_some() {
                return None;
            }
            ((marked && !season_dash) || !episode_noise(number)).then_some(BracketEpisode {
                number,
                span: c.get(0)?,
                marked,
            })
        })
        .min_by_key(|episode| episode.span.start())
}

fn episode_noise(number: i64) -> bool {
    NOISE_NUMBERS.contains(&number) || looks_like_year(number)
}

fn mask_tag_words(tag: &str) -> String {
    let tag = RELEASE_WORDS.replace_all(tag, |c: &regex::Captures| " ".repeat(c[0].len()));
    RE_EMPTY_BRACKETS
        .replace_all(&tag, |c: &regex::Captures| " ".repeat(c[0].len()))
        .into_owned()
}

fn mask_brackets(s: &str) -> String {
    RE_BRACKETS
        .replace_all(s, |c: &regex::Captures| {
            let bracket = &c[0];
            if bracket_episode_match(bracket).is_some() {
                bracket.to_owned()
            } else {
                " ".repeat(bracket.len())
            }
        })
        .into_owned()
}

fn valid_season_range(c: &regex::Captures<'_>, more: &str) -> bool {
    let numbers = std::iter::once(&c["first"])
        .chain(std::iter::once(&c["last"]))
        .chain(
            more.split(|ch: char| !ch.is_ascii_digit())
                .filter(|n| !n.is_empty()),
        );
    let mut last = 0;
    for n in numbers.map(|n| n.parse::<u32>().unwrap()) {
        if n <= last || n > MAX_SEASON {
            return false;
        }
        last = n;
    }
    true
}

fn season_pack_range(
    s: &str,
    title_context: impl FnOnce() -> (Option<Range<usize>>, usize),
) -> bool {
    let s = mask_dates(s);
    let mut packs = RE_SEASON_RANGE.captures_iter(&s).filter_map(|c| {
        let range = c.name("range").unwrap();
        let more = c.name("more").unwrap();
        let detached = more
            .as_str()
            .find(|ch: char| ch.is_whitespace() || ch == '_');
        let (end, chain) = detached.map_or((range.end(), more.as_str()), |start| {
            (more.start() + start, &more.as_str()[..start])
        });
        let span = range.start()..end;
        (valid_season_range(&c, chain)
            && (explicit_season_range(&c)
                || compact_season_range(&c, chain)
                || (c["last"].len() == 1
                    && (bracketed_range(&s, span.clone()) || follows_with_pack_label(&s[end..])))))
        .then_some((c, span))
    });
    let Some(first) = packs.next() else {
        return false;
    };
    let (title, group_end) = title_context();
    std::iter::once(first).chain(packs).any(|(c, span)| {
        is_release_range(&s, span.clone(), group_end)
            && title.as_ref().is_none_or(|title| {
                !(literal_season_range(&c, title)
                    || (title.start <= span.start && title.end >= span.end))
            })
    })
}

fn follows_with_pack_label(suffix: &str) -> bool {
    let Some(label) = PACK_WORDS.find(suffix) else {
        return false;
    };
    let padding = mask_release_format(&suffix[..label.start()].replace('_', " "), true);
    RELEASE_WORDS
        .replace_all(&padding, " ")
        .chars()
        .all(|ch| release_separator(ch) || "[]()【】|".contains(ch))
}

pub(crate) fn preceding_season(prefix: &str) -> Option<u32> {
    let prefix = prefix.trim_end_matches(release_separator);
    RE_SEASON_WORDS
        .captures_iter(prefix)
        .last()
        .and_then(|c| c.name("words"))
        .filter(|words| words.end() == prefix.len())
        .and_then(|words| parse_season_word(words.as_str()))
        .or_else(|| {
            RE_SEASON_TOKEN
                .find_iter(prefix)
                .last()
                .filter(|token| token.end() == prefix.len())
                .and_then(|token| parse_season_token(token.as_str()))
        })
}

fn literal_season_range(c: &regex::Captures<'_>, title: &Range<usize>) -> bool {
    let start = c.name("range").unwrap().start();
    c.name("short").is_none()
        && c.name("plural").is_none()
        && c.name("second_season").is_none()
        && title.contains(&start)
        && title.end <= c.name("first").unwrap().start()
}

fn explicit_season_range(c: &regex::Captures<'_>) -> bool {
    c.name("plural").is_some()
        || c.name("second_season").is_some()
        || !is_dash(c["separator"].trim_matches(|ch: char| ch.is_whitespace() || ch == '_'))
}

fn is_dash(s: &str) -> bool {
    matches!(s, "-" | "–" | "—")
}

fn compact_season_range(c: &regex::Captures<'_>, more: &str) -> bool {
    is_dash(&c["separator"])
        && (c.name("short").is_some()
            || !more.is_empty()
            || c["first"].starts_with('0')
            || c["last"].len() == 1)
}

fn is_release_range(s: &str, range: Range<usize>, group_end: usize) -> bool {
    range.end > group_end || bracketed_range(s, range)
}

pub(crate) fn bracketed_range(s: &str, range: Range<usize>) -> bool {
    RE_BRACKETS.find_iter(s).any(|bracket| {
        if bracket.start() >= range.start || bracket.end() <= range.end {
            return false;
        }
        let padding = |text: &str| {
            let text = text.replace(['_', '[', ']', '(', ')', '【', '】'], " ");
            let text = mask_release_format(&text, true);
            let text = mask_season_words(&text, None);
            let text = RE_SEASON_TOKEN.replace_all(&text, " ");
            let text = RELEASE_WORDS.replace_all(&text, " ");
            PACK_WORDS
                .replace_all(&text, " ")
                .chars()
                .all(|c| c.is_whitespace() || matches!(c, '|' | '-' | '·' | '–' | '—'))
        };
        let delimiter = bracket.as_str().chars().next().unwrap().len_utf8();
        padding(&s[bracket.start() + delimiter..range.start])
            && padding(&s[range.end..bracket.end() - delimiter])
    })
}

fn mask_season_words(s: &str, title: Option<&Range<usize>>) -> String {
    let mut masked = s.to_owned();
    let mut start = 0;
    while let Some(phrase) = RE_SEASON_WORDS.captures_at(s, start) {
        let words = phrase.name("words").unwrap();
        let literal_season = title.is_some_and(|t| {
            phrase.name("number").is_some_and(|number| {
                t.start <= words.start() && words.start() < t.end && t.end <= number.start()
            })
        });
        if !literal_season {
            masked.replace_range(words.range(), &" ".repeat(words.len()));
            let after_words = s[words.end()..].trim_start();
            if s[..words.start()].trim_end().ends_with(['[', '(', '【']) {
                if let Some(dash) = after_words
                    .chars()
                    .next()
                    .filter(|c| matches!(c, '-' | '–' | '—'))
                {
                    let start = s.len() - after_words.len();
                    let tail = after_words[dash.len_utf8()..].trim_start();
                    // Retain the explicit episode marker and original byte offsets.
                    let marker = if starts_with_unmarked_episode(tail) {
                        "e  "
                    } else {
                        "   "
                    };
                    masked
                        .replace_range(start..start + dash.len_utf8(), &marker[..dash.len_utf8()]);
                }
            }
        }
        start = words.end();
    }
    masked
}

fn starts_with_unmarked_episode(s: &str) -> bool {
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    (1..=4).contains(&end)
        && s[..end].parse().is_ok_and(|number| !episode_noise(number))
        && RE_FORMAT.find(s).is_none_or(|tag| tag.start() != 0)
        && RE_CHANNEL.find(s).is_none_or(|tag| tag.start() != 0)
        && (s[end..].chars().next().is_none_or(|c| !c.is_alphabetic())
            || s[end..]
                .strip_prefix(['v', 'V'])
                .is_some_and(|revision| revision.starts_with(|c: char| c.is_ascii_digit())))
}

pub(crate) fn mask_release_format(s: &str, strip_channels: bool) -> String {
    let mut masked = RE_FORMAT
        .replace_all(s, |c: &regex::Captures| " ".repeat(c[0].len()))
        .into_owned();
    if strip_channels {
        masked = RE_CHANNEL
            .replace_all(&masked, |c: &regex::Captures| " ".repeat(c[0].len()))
            .into_owned();
    }
    mask_dates(&masked)
}

fn valid_date(c: regex::Captures<'_>) -> bool {
    c[3] == c[5]
        && chrono::NaiveDate::from_ymd_opt(
            c[2].parse().unwrap(),
            c[4].parse().unwrap(),
            c[6].parse().unwrap(),
        )
        .is_some()
}

pub(crate) fn mask_dates(s: &str) -> String {
    let mut masked = s.to_string();
    for date in RE_DATE.captures_iter(s) {
        let span = date.name("date").unwrap();
        if valid_date(date) {
            masked.replace_range(span.range(), &" ".repeat(span.len()));
        }
    }
    masked
}

fn mask_release_title(s: &str, masked: &mut String, title: Range<usize>) {
    let tag_opener = RE_BRACKETS
        .find_iter(s)
        .find(|tag| title.start <= tag.start() && tag.start() < title.end && title.end < tag.end());
    let padding = " ".repeat(title.len());
    masked.replace_range(title, &padding);
    if let Some(tag) = tag_opener {
        // Keep the bracket around the episode or extra remaining after the title.
        let end = tag.start() + tag.as_str().chars().next().unwrap().len_utf8();
        masked.replace_range(tag.start()..end, &s[tag.start()..end]);
    }
}

fn mask_invalid_season_ranges(s: &str, masked: &mut String, title: Option<&Range<usize>>) -> bool {
    let mut found = false;
    for c in RE_SEASON_RANGE.captures_iter(s) {
        if !(compact_season_range(&c, &c["more"]) || explicit_season_range(&c))
            || title.is_some_and(|title| literal_season_range(&c, title))
        {
            continue;
        }
        let span = c.name("range").unwrap();
        let last = c.name("last").unwrap().end();
        let more = &s[last..span.end()];
        let has_episode_separator = more.starts_with(|ch: char| ch.is_whitespace() || ch == '_');
        let end = if has_episode_separator {
            last
        } else {
            span.end()
        };
        if !valid_season_range(&c, if has_episode_separator { "" } else { more }) {
            // A detached episode may follow a malformed compact season label.
            masked.replace_range(span.start()..end, &" ".repeat(end - span.start()));
            found = true;
        }
    }
    found
}

pub(crate) fn episode_remainder(s: &str, title: Option<Range<usize>>) -> (String, bool) {
    let s = mask_dates(s).replace('_', " ");
    let mut masked = mask_season_words(&s, title.as_ref());
    let invalid_season_range = mask_invalid_season_ranges(&s, &mut masked, title.as_ref());
    if let Some(title) = title {
        mask_release_title(&s, &mut masked, title);
    }
    if let Some((marker, _)) = native_season(&masked) {
        masked.replace_range(marker.clone(), &" ".repeat(marker.len()));
    }
    let masked = mask_release_format(&strip_ext(&masked), true);
    let masked = RE_BRACKETS
        .replace_all(&masked, |c: &regex::Captures| mask_tag_words(&c[0]))
        .into_owned();
    (masked, invalid_season_range)
}

fn leading_episode(s: &str) -> Option<(i64, usize)> {
    let first = RE_LEADING_EP.captures(s)?.get(1)?;
    let first_number = first.as_str().parse::<i64>().ok()?;
    if !looks_like_year(first_number) {
        return Some((first_number, first.start()));
    }
    let next = RE_LEADING_EP.captures(&s[first.end()..])?.get(1)?;
    let number = next.as_str().parse::<i64>().ok()?;
    (!looks_like_year(number)).then_some((number, first.end() + next.start()))
}

fn dotted_numeric_prefix(s: &str) -> bool {
    let Some(prefix) = s.strip_suffix('.') else {
        return false;
    };
    let number = prefix
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    !number.is_empty() && !number.parse().is_ok_and(looks_like_year)
}

fn dotted_numeric_suffix(s: &str) -> bool {
    let Some(suffix) = s.strip_prefix('.') else {
        return false;
    };
    let number = suffix
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or_default();
    !number.is_empty()
        && !number
            .parse()
            .is_ok_and(|n| NOISE_NUMBERS.contains(&n) || looks_like_year(n))
}

fn fractional_episodes(s: &str) -> impl Iterator<Item = (usize, Range<usize>)> + '_ {
    let mut from = 0;
    std::iter::from_fn(move || loop {
        let capture = RE_FRACTIONAL_EPISODE.captures_at(s, from)?;
        let fraction = capture.name("fraction").unwrap();
        let number = capture.name("number").unwrap();
        from = number.end();
        if (fraction.start() != number.start()
            || (!number.as_str().parse().is_ok_and(looks_like_year)
                && !dotted_numeric_prefix(&s[..fraction.start()])))
            && !dotted_numeric_suffix(&s[fraction.end()..])
        {
            return Some((number.start(), fraction.range()));
        }
    })
}

fn starts_with_fractional_episode(s: &str, episode_start: Option<usize>) -> bool {
    fractional_episodes(s).any(|(number, range)| {
        episode_start.is_none_or(|start| start >= number) || bracketed_range(s, range)
    })
}

pub(crate) fn has_fractional_episode(s: &str) -> bool {
    let s = strip_ext(s);
    let episode = episode_position(&mask_brackets(&s), true);
    starts_with_fractional_episode(&s, episode)
        || release_episode_ranges(&s, episode).any(|range| range.name("fraction").is_some())
}

pub(crate) fn has_cross_season_range(s: &str, group_end: usize) -> bool {
    RE_CROSS_SEASON_RANGE.captures_iter(s).any(|c| {
        canon_ordinal(&c[1]) != canon_ordinal(&c[2])
            && is_release_range(s, c.get(0).unwrap().range(), group_end)
    })
}

pub(crate) fn episode_ranges(s: &str) -> impl Iterator<Item = regex::Captures<'_>> {
    let mut from = 0;
    std::iter::from_fn(move || loop {
        let capture = RE_EP_RANGE.captures_at(s, from)?;
        let span = capture.name("range").unwrap();
        let start = capture.name("start").unwrap();
        let end = capture.name("end").unwrap();
        let first: i64 = start.as_str().parse().unwrap();
        let last: i64 = end.as_str().parse().unwrap();
        let before = &s[..span.start()];
        let unmarked = capture.name("marker").is_none();
        let follows_date = unmarked
            && RE_DATE.captures_iter(&s[..end.end()]).any(|date| {
                date.name("date").unwrap().range().contains(&start.start()) && valid_date(date)
            });
        let trailing_fraction = capture.name("fraction").is_none_or(|_| {
            let suffix = RE_BRACKETS.replace_all(&s[span.end()..], " ");
            let suffix = RELEASE_WORDS.replace_all(&suffix, " ");
            let tail = suffix.trim_matches(release_separator);
            tail.is_empty()
                || tail
                    .parse::<i64>()
                    .is_ok_and(|n| looks_like_year(n) || NOISE_NUMBERS.contains(&n))
                || tail.starts_with([']', ')', '】'])
        });
        if first < last
            && trailing_fraction
            && !looks_like_year(first)
            && !looks_like_year(last)
            && (!unmarked || !dotted_numeric_prefix(before))
            && !dotted_numeric_suffix(&s[span.end()..])
            && !follows_date
        {
            from = span.end();
            return Some(capture);
        }
        // Retry after the first number so a year prefix cannot hide a following range.
        from = start.end();
    })
}

pub(crate) fn torrent_episode_ranges(s: &str) -> impl Iterator<Item = regex::Captures<'_>> {
    let episode = episode_position(&mask_brackets(s), true);
    release_episode_ranges(s, episode)
}

fn release_episode_ranges(
    s: &str,
    episode: Option<usize>,
) -> impl Iterator<Item = regex::Captures<'_>> {
    let mut previous_end = None;
    episode_ranges(s).filter(move |c| {
        let range = c.name("range").unwrap();
        let consecutive = previous_end.is_some_and(|end| {
            s[end..range.start()]
                .chars()
                .all(|ch| release_separator(ch) || matches!(ch, ',' | '+' | '&' | '|'))
        });
        let accepted = consecutive
            || episode.is_none_or(|position| position >= c.name("start").unwrap().start())
            || bracketed_range(s, range.range());
        if accepted {
            previous_end = Some(range.end());
        }
        accepted
    })
}

fn dashed_episode(s: &str) -> Option<(usize, regex::Match<'_>)> {
    RE_DASHED_EP.captures_iter(s).find_map(|c| {
        let number = c.get(2)?;
        (!number.as_str().parse().is_ok_and(looks_like_year))
            .then(|| (c.name("dash").unwrap().start(), number))
    })
}

fn episode_position(s: &str, title_removed: bool) -> Option<usize> {
    let explicit = RE_EXPLICIT_EP
        .captures(s)
        .and_then(|c| c.name("episode"))
        .map(|m| m.start());
    let dashed = dashed_episode(s).map(|(_, m)| m.start());
    let leading = title_removed
        .then(|| leading_episode(s))
        .flatten()
        .map(|(_, start)| start);
    let bracket = bracket_episode_match(s).map(|tag| tag.span.start());
    explicit
        .into_iter()
        .chain(dashed)
        .chain(leading)
        .chain(bracket)
        .min()
}

fn is_single_episode(s: &str, episode_start: Option<usize>, strict_ranges: bool) -> bool {
    // A pack has no single episode to track or offer as the next file.
    !has_cross_season_range(s, 0)
        && !starts_with_fractional_episode(s, episode_start)
        && !release_episode_ranges(s, episode_start)
            .any(|c| strict_ranges || c.name("marker").is_some() || c["start"].len() >= 2)
}

pub(crate) fn has_conflicting_episode_markers(s: &str, source: &str) -> bool {
    let masked = RE_BRACKETS.replace_all(s, |c: &regex::Captures| {
        let span = c.get(0).unwrap();
        let tag = mask_tag_words(span.as_str());
        if let Some(episode) = bracket_episode_match(&tag)
            .filter(|episode| episode.marked && episode.span.len() == tag.len())
        {
            // The episode remainder masks named seasons but keeps the source byte offsets.
            let raw = &source[span.range()];
            if let Some(season) = parse_season_word(raw) {
                let more = RE_EXPLICIT_EP.captures(&tag).and_then(|c| c.name("more"));
                let scoped = format!(
                    "[S{season}E{}{}]",
                    episode.number,
                    more.map_or("", |m| m.as_str())
                );
                return format!("{scoped:<width$}", width = span.len());
            }
        }
        let inner = tag.trim_start_matches(|c: char| c.is_whitespace() || "[(【".contains(c));
        if RE_EXPLICIT_EP
            .find(inner)
            .is_some_and(|marker| marker.start() == 0)
        {
            tag
        } else {
            " ".repeat(tag.len())
        }
    });
    let mut from = 0;
    let mut previous_end = None;
    let mut episode = None;
    let mut season = None;
    while let Some(marker) = RE_EXPLICIT_EP.captures_at(&masked, from) {
        let number = marker.name("episode").unwrap();
        // Start before the consumed delimiter so adjacent markers are visible.
        from = number.end();
        let Ok(number) = number.as_str().parse::<i64>() else {
            continue;
        };
        let start = marker.get(0).unwrap().start();
        if previous_end.is_some_and(|end| {
            let between = &masked[end..start];
            RE_SUBTITLE_SEPARATOR
                .find(between)
                .is_some_and(|separator| {
                    let prefix = between[separator.end()..].trim_end();
                    !prefix.ends_with(['[', '(', '【']) && prefix.chars().any(char::is_alphabetic)
                })
        }) {
            continue;
        }
        if marker.name("more").is_some_and(|more| {
            more.as_str()[1..].split(['e', 'E']).any(|ep| {
                ep.split(['v', 'V']).next().and_then(|ep| ep.parse().ok()) != Some(number)
            })
        }) {
            return true;
        }
        let scope = marker
            .name("season")
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .or_else(|| {
                let mut prefix_start = previous_end.unwrap_or(0);
                while prefix_start < start && !source.is_char_boundary(prefix_start) {
                    prefix_start += 1;
                }
                preceding_season(source.get(prefix_start..start)?)
            });
        previous_end = Some(from);
        if episode.replace(number).is_some_and(|last| last != number)
            || scope.is_some_and(|n| season.replace(n).is_some_and(|last| last != n))
        {
            return true;
        }
    }
    false
}

fn episode_from_remainder(masked: &str, title_removed: bool) -> Option<i64> {
    let bracketed = bracket_episode_match(masked);
    let dashed = dashed_episode(masked).map(|(_, m)| m);
    if let Some(episode) = RE_EXPLICIT_EP
        .captures(masked)
        .and_then(|c| c.name("episode"))
        .filter(|m| {
            dashed.is_none_or(|dash| m.start() < dash.start())
                && bracketed
                    .as_ref()
                    .is_none_or(|tag| m.start() < tag.span.end())
        })
        .and_then(|m| m.as_str().parse().ok())
    {
        return Some(episode);
    }
    let s = RE_SEASON_PREFIX.replace_all(masked, "$1");
    let s = RE_SEASON_BARE.replace_all(&s, " ");
    let s = RE_REV_TAIL.replace(&s, " ");
    if title_removed {
        if let Some((episode, _)) = leading_episode(&s) {
            return Some(episode);
        }
    }
    if let Some(episode) = dashed
        .filter(|m| {
            bracketed
                .as_ref()
                .is_none_or(|tag| m.start() < tag.span.start())
        })
        .and_then(|m| m.as_str().parse().ok())
        .or_else(|| bracketed.map(|tag| tag.number))
    {
        return Some(episode);
    }
    // An episode immediately after the title can resemble a resolution, such as One Piece 1080.
    let after_title = RE_EP_NUM.find(&s).and_then(|m| {
        let lone = s[..m.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
            && s[m.end()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric());
        if lone {
            m.as_str().split('v').next()?.parse::<i64>().ok()
        } else {
            None
        }
    });
    let s = RE_RES.replace_all(&s, " ");
    RE_EP_NUM
        .find_iter(&s)
        .filter_map(|m| {
            // Detached v2 is a revision, never episode two.
            if m.start() > 0 && matches!(s[..m.start()].chars().next_back(), Some('v' | 'V')) {
                return None;
            }
            m.as_str().split('v').next()?.parse::<i64>().ok()
        })
        .filter(|n| !NOISE_NUMBERS.contains(n) && !looks_like_year(*n))
        .filter(|n| *n >= 0 && *n <= 9999)
        .last()
        .or(after_title.filter(|n| !looks_like_year(*n) && *n >= 0 && *n <= 9999))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_display_skips_blank_preferred_titles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        for title in ["", "   "] {
            db.upsert_media(&crate::models::Media {
                id: 1,
                title_english: Some(title.into()),
                title_romaji: Some("Some Show".into()),
                episodes: Some(12),
                ..Default::default()
            })
            .unwrap();
            let matchers = build_matchers(&db);
            let matched = match_title(&matchers, "Some Show - 01", "").unwrap();
            assert_eq!(matched.display, "Some Show");
            assert_eq!(resolve_episode(matched, &["Some Show - 01"]), Some(1));
        }
    }

    fn seed_refresh_inputs(db: &Db) {
        for (id, title) in [(1, "Some Show"), (2, "[Group]")] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                title_romaji: (id == 1).then(|| "Some Show Romaji".into()),
                title_native: (id == 1).then(|| "!!!".into()),
                episodes: Some(12),
                format: Some("TV".into()),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
    }

    #[test]
    fn matcher_refresh_reuses_raw_sources_including_unmatchable_titles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        let before = Arc::new(build_matchers(&db));
        assert_eq!(before.len(), 1);
        before[0].title_patterns();
        let work = matcher_work_counts();
        let parses = ORDINAL_PARSES.get();
        let after = refresh_matchers(&db, &before, None);
        let after = refresh_matchers(&db, &after, Some(&[1, 2]));
        assert!(Arc::ptr_eq(&before, &after));
        assert!(after[0].title_patterns.get().is_some());
        assert_eq!(matcher_work_counts(), (work.0 + 4, work.1));
        assert_eq!(ORDINAL_PARSES.get(), parses);
    }

    #[test]
    fn matcher_refresh_invalidates_each_recognition_input() {
        for sql in [
            "UPDATE media SET title_english = 'Some_Show' WHERE id = 1",
            "UPDATE media SET title_english = NULL WHERE id = 1",
            "UPDATE media SET title_romaji = 'New Romaji' WHERE id = 1",
            "UPDATE media SET title_native = '???' WHERE id = 1",
            "UPDATE media SET title_english = '[Other Group]' WHERE id = 2",
            "UPDATE media SET episodes = 1 WHERE id = 1",
            "UPDATE media SET episodes = NULL WHERE id = 1",
            "UPDATE media SET format = 'MOVIE' WHERE id = 1",
            "UPDATE list_entry SET status = 'PAUSED' WHERE media_id = 1",
            "DELETE FROM list_entry WHERE media_id = 1",
            "DELETE FROM list_entry WHERE media_id = 2",
            "DELETE FROM media WHERE id = 1",
            "INSERT INTO media(id,title_english,cached_at) VALUES(3,'New Show',0);
             INSERT INTO list_entry(media_id,status) VALUES(3,'CURRENT')",
        ] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            seed_refresh_inputs(&db);
            let before = Arc::new(build_matchers(&db));
            db.0.lock().execute_batch(sql).unwrap();
            let after = refresh_matchers(&db, &before, None);
            assert!(!Arc::ptr_eq(&before, &after), "{sql}");
            let fresh = build_matchers(&db);
            let snapshot = |matchers: &[Matcher]| {
                matchers
                    .iter()
                    .map(|matcher| {
                        (
                            matcher.media_id,
                            matcher.display.clone(),
                            matcher.variants.clone(),
                            matcher.norms.clone(),
                            matcher.norm_seasons.clone(),
                            matcher.episodes,
                            matcher.format.clone(),
                            matcher.status_rank,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(snapshot(&after), snapshot(&fresh), "{sql}");
            assert!(
                Arc::ptr_eq(&after, &refresh_matchers(&db, &after, None)),
                "{sql}"
            );
        }
    }

    #[test]
    fn targeted_media_refresh_invalidates_changed_titles_and_preserves_empty_results() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        let before = Arc::new(build_matchers(&db));
        db.0.lock()
            .execute(
                "UPDATE media SET title_english = 'Other Show' WHERE id = 1",
                [],
            )
            .unwrap();
        let after = refresh_matchers(&db, &before, Some(&[1]));
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after[0].display, "Other Show");
        db.clear_entries().unwrap();
        let empty = refresh_matchers(&db, &after, None);
        assert!(empty.is_empty());
        assert!(Arc::ptr_eq(&empty, &refresh_matchers(&db, &empty, None)));
    }

    #[test]
    fn matcher_refresh_limits_upsert_checks_and_requires_full_checks_for_membership() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        let before = Arc::new(build_matchers(&db));
        db.0.lock()
            .execute_batch("DELETE FROM list_entry WHERE media_id = 1")
            .unwrap();
        let work = matcher_work_counts();
        let scoped = refresh_matchers(&db, &before, Some(&[2]));
        assert!(Arc::ptr_eq(&before, &scoped));
        assert_eq!(matcher_work_counts(), (work.0 + 1, work.1));
        let full = refresh_matchers(&db, &scoped, None);
        assert!(!Arc::ptr_eq(&scoped, &full));
        assert!(full.is_empty());
    }

    #[test]
    fn matcher_read_failures_disable_matching_and_full_refresh_recovers() {
        for ids in [None, Some(&[1][..])] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            seed_refresh_inputs(&db);
            let before = Arc::new(build_matchers(&db));
            db.0.lock()
                .execute_batch("ALTER TABLE list_entry RENAME TO unavailable_list")
                .unwrap();
            let failed = refresh_matchers(&db, &before, ids);
            assert!(failed.is_empty());
            assert!(!Arc::ptr_eq(&before, &failed));
            assert!(build_matchers(&db).is_empty());
            db.0.lock()
                .execute_batch("ALTER TABLE unavailable_list RENAME TO list_entry")
                .unwrap();
            let recovered = refresh_matchers(&db, &failed, None);
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].media_id, 1);
        }
    }

    #[test]
    fn malformed_unmatchable_titles_disable_the_snapshot_and_recover() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        let before = Arc::new(build_matchers(&db));
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].source.as_ref().unwrap().len(), 2);
        db.0.lock()
            .execute("UPDATE media SET title_english = X'FF' WHERE id = 2", [])
            .unwrap();
        let parses = ORDINAL_PARSES.get();
        let failed = refresh_matchers(&db, &before, Some(&[2]));
        assert!(failed.is_empty());
        assert_eq!(ORDINAL_PARSES.get(), parses);
        assert!(match_title(&failed, "Some Show - 03", "").is_none());
        db.0.lock()
            .execute(
                "UPDATE media SET title_english = 'Other Show II' WHERE id = 2",
                [],
            )
            .unwrap();
        let recovered = refresh_matchers(&db, &failed, None);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[1].norm_seasons, [Some(2)]);
        assert_eq!(
            match_title(&recovered, "Other Show S02E04", "").map(|matcher| matcher.media_id),
            Some(2)
        );
    }

    #[test]
    fn cached_season_ordinals_preserve_cold_scan_and_playback_matches() {
        let fixtures = [
            (10, "CURRENT", "Example Saga", Some("Rei Monogatari"), None),
            (
                20,
                "COMPLETED",
                "Example Saga Season 2",
                Some("Rei Monogatari II"),
                Some("例の物語 II"),
            ),
            (
                30,
                "PAUSED",
                "Example Saga Season 3",
                Some("Rei Monogatari III"),
                None,
            ),
            (40, "CURRENT", "86", Some("Eighty Six"), None),
            (50, "CURRENT", "91 Days", None, None),
            (60, "PAUSED", "Tie Show", None, None),
            (55, "CURRENT", "Tie Show", None, None),
            (54, "CURRENT", "Tie Show", None, None),
            (70, "CURRENT", "Some Movie", None, None),
            (80, "CURRENT", "[Group]", Some("!!!"), Some(" ")),
            (81, "CURRENT", "I", None, None),
        ];
        let cases = [
            ("Example Saga S01E03", "", Some(10), Some(3)),
            ("EXAMPLE SAGA S02E04", "", Some(20), Some(4)),
            ("Rei Monogatari II - 04", "", Some(20), Some(4)),
            ("例の物語 II - 04", "", Some(20), Some(4)),
            (
                "",
                "file:///shows/Example.Saga.S03E05.mkv",
                Some(30),
                Some(5),
            ),
            (
                "[S03E05] Example Saga",
                "file:///shows/Example.Saga.S02E06.mkv",
                Some(20),
                Some(6),
            ),
            (
                "[S03E05] Example Saga",
                "file:///shows/Example.Saga.S02.mkv",
                Some(20),
                None,
            ),
            (
                "[S03E05] Example Saga",
                "file:///shows/Example.Saga.S01E06.mkv",
                Some(10),
                Some(6),
            ),
            ("Example Saga S04E03", "", None, None),
            ("Example Saga - 03", "", Some(10), Some(3)),
            ("Tie Show - 02", "", Some(54), Some(2)),
            ("86 - 03", "", Some(40), Some(3)),
            ("91 Days - 05", "", Some(50), Some(5)),
            ("I - 02", "", Some(81), Some(2)),
            ("Some Movie.mkv", "", Some(70), Some(1)),
            ("No Such Show - 03", "", None, None),
            (
                "",
                "file:///shows/Example%20Saga%20S02E04.mkv?S03E99",
                Some(20),
                Some(4),
            ),
        ];
        for filler_count in [0, 100, 1000] {
            let mut inputs = fixtures
                .iter()
                .map(|(id, status, english, romaji, native)| MatcherInput {
                    media_id: *id,
                    titles: [
                        Some((*english).into()),
                        romaji.map(String::from),
                        native.map(String::from),
                    ],
                    episodes: Some(if *id == 70 { 1 } else { 12 }),
                    format: Some(if *id == 70 { "MOVIE" } else { "TV" }.into()),
                    status_rank: status_rank(status),
                })
                .collect::<Vec<_>>();
            inputs.extend((0..filler_count).map(|id| MatcherInput {
                media_id: 1000 + id,
                titles: [Some(format!("Unrelated serial {}", 100 + id)), None, None],
                episodes: Some(12),
                format: Some("TV".into()),
                status_rank: 0,
            }));
            let parses = ORDINAL_PARSES.get();
            let matchers = build_matchers_from_inputs(inputs);
            let norms = matchers
                .iter()
                .map(|matcher| matcher.norms.len())
                .sum::<usize>();
            assert_eq!(ORDINAL_PARSES.get() - parses, norms);
            assert_eq!(matchers.len(), fixtures.len() - 1 + filler_count as usize);
            assert_eq!(
                matchers[0].source.as_ref().unwrap().len(),
                fixtures.len() + filler_count as usize
            );
            for matcher in &matchers {
                assert_eq!(
                    matcher.norm_seasons,
                    matcher
                        .norms
                        .iter()
                        .zip(&matcher.variants)
                        .map(|(norm, variant)| catalog_season_ordinal(variant, norm))
                        .collect::<Vec<_>>()
                );
            }
            for (title, url, expected_id, expected_episode) in cases {
                let parses = ORDINAL_PARSES.get();
                assert!(match_title(&[], title, url).is_none());
                let input_parses = ORDINAL_PARSES.get() - parses;
                for _ in 0..8 {
                    let parses = ORDINAL_PARSES.get();
                    let matched = match_title(&matchers, title, url);
                    assert_eq!(
                        matched.map(|matcher| matcher.media_id),
                        expected_id,
                        "{title} {url}"
                    );
                    assert_eq!(
                        ORDINAL_PARSES.get() - parses,
                        input_parses,
                        "matcher parsing for {title} {url}"
                    );
                    let base = basename(url);
                    assert_eq!(
                        matched.and_then(|matcher| resolve_playback_episode(
                            matcher,
                            &[title, &base],
                            false
                        )),
                        expected_episode,
                        "{title} {url}"
                    );
                    let cold_episode =
                        matched.and_then(|matcher| resolve_episode(matcher, &[title, &base]));
                    assert_eq!(
                        cold_episode,
                        if expected_id == Some(70) {
                            None
                        } else {
                            expected_episode
                        },
                        "cold scan {title} {url}"
                    );
                }
            }
            println!("{} matchers, {norms} construction ordinal parses: 0 extra matcher parses across 112 scan/playback matches", matchers.len());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn matcher_construction_supports_the_multithread_runtime() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        let before = Arc::new(build_matchers(&db));
        assert!(Arc::ptr_eq(&before, &refresh_matchers(&db, &before, None)));
    }

    #[test]
    fn television_bonus_clips_are_not_regular_episodes() {
        let series = Matcher {
            episodes: Some(12),
            ..mk(1, "Some Show")
        };
        for filename in [
            "Some Show NCOP01.mkv",
            "Some Show NCED01.mkv",
            "Some Show OP1.mkv",
            "Some Show ED1.mkv",
            "Some Show Trailer01.mkv",
            "Some Show Preview01.mkv",
            "Some_Show_SP01.mkv",
            "Some Show OVA01.mkv",
            "Some Show - 03 [NCOP].mkv",
            "Some Show - 03 [NCOP01 1080p x265].mkv",
        ] {
            for bound in [false, true] {
                assert_eq!(
                    resolve_playback_episode(&series, &[filename, "Some Show E01"], bound),
                    None,
                    "{filename}, bound: {bound}"
                );
            }
        }
    }

    #[test]
    fn television_bonus_exclusions_preserve_titles_and_episode_names() {
        for title in ["The Special", "Sample", "OP", "ED", "Preview"] {
            let series = Matcher {
                episodes: Some(12),
                ..mk(1, title)
            };
            let filename = format!("[Group] {title} - 03.mkv");
            assert_eq!(
                resolve_playback_episode(&series, &[&filename], false),
                Some(3),
                "{filename}"
            );
        }
        let series = Matcher {
            episodes: Some(12),
            ..mk(1, "Some Show")
        };
        for filename in [
            "Some Show E03 - A Special Day.mkv",
            "Some Show - 03 - Preview of Tomorrow.mkv",
            "Some Show E03 - The OP Is Great.mkv",
            "[Group] Some Show - 03 - [Special Day].mkv",
            "[Group] Some Show E03 - [The OP Is Great].mkv",
            "[Group] Some Show E03 - [OP Is Great].mkv",
        ] {
            assert_eq!(
                resolve_playback_episode(&series, &[filename], false),
                Some(3),
                "{filename}"
            );
        }
    }

    #[test]
    fn format_markers_preserve_real_ova_and_special_episodes() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, format, total) in [(1, "OVA", 1), (2, "SPECIAL", 6)] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some("Some Show".into()),
                episodes: Some(total),
                format: Some(format.into()),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let matchers = build_matchers(&db);
        for (id, filename, episode) in [
            (1, "Some Show OVA01.mkv", 1),
            (1, "Some Show [OAD].mkv", 1),
            (2, "Some Show SP03.mkv", 3),
            (2, "Some Show [Special] - 03.mkv", 3),
        ] {
            let matched = matchers.iter().find(|m| m.media_id == id).unwrap();
            assert_eq!(
                resolve_playback_episode(matched, &[filename], false),
                Some(episode),
                "{filename}"
            );
            assert_eq!(
                resolve_playback_episode(matched, &["Some Show Trailer01.mkv"], true),
                None
            );
        }
    }

    #[test]
    fn adversarial_fractional_movie_releases_are_not_the_main_feature() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "Some Movie")
        };
        for filename in ["Some Movie - 12.5.mkv", "Some Movie [12.5].mkv"] {
            assert_eq!(
                resolve_playback_episode(&movie, &[filename], true),
                None,
                "{filename} was recognized as the movie"
            );
        }
    }

    #[test]
    fn adversarial_playback_exclusions_preserve_real_titles_audio_and_episode_names() {
        for title in [
            "Some Movie",
            "12.5",
            "Evangelion: 1.0 You Are (Not) Alone",
            "The Special",
        ] {
            let movie = Matcher {
                episodes: Some(1),
                ..mk(1, title)
            };
            let filename = format!("[G] {title} [1080p] [AAC5.1].mkv");
            assert_eq!(
                resolve_playback_episode(&movie, &[&filename], false),
                Some(1),
                "{filename}"
            );
        }
        let series = Matcher {
            episodes: Some(12),
            ..mk(1, "Some Show")
        };
        for filename in [
            "Some Show - 12.5.mkv",
            "Some Show [12.5].mkv",
            "Some Show - EP5.5.mkv",
        ] {
            assert_eq!(
                resolve_playback_episode(&series, &[filename], false),
                None,
                "{filename}"
            );
        }
        assert_eq!(
            resolve_playback_episode(&series, &["Some Show E03 - 1-2-3 Go!.mkv"], false),
            Some(3)
        );
        for filename in [
            "Some Show E03 - 12.5 Seconds.mkv",
            "Some Show - 03 - 12.5 Seconds.mkv",
        ] {
            assert_eq!(
                resolve_playback_episode(&series, &[filename], false),
                Some(3),
                "{filename}"
            );
        }
        assert_eq!(
            resolve_playback_episode(&series, &["Some Show [12.5_1080p].mkv"], false),
            None
        );
    }

    #[test]
    fn single_episode_playback_uses_known_total_without_guessing_series_or_previews() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "Some Movie")
        };
        assert_eq!(
            resolve_playback_episode(&movie, &["[G] Some Movie [BD 1080p].mkv"], false),
            Some(1)
        );
        assert_eq!(
            resolve_playback_episode(&movie, &["feature.mkv"], true),
            Some(1)
        );
        assert_eq!(
            resolve_playback_episode(&movie, &["Unrelated Movie.mkv"], false),
            None
        );
        for release in [
            "Some Movie [Trailer].mkv",
            "Some Movie [01-02].mkv",
            "Some Movie Preview.mkv",
            "sample.mkv",
            "[Trailer] Some Movie - 01.mkv",
        ] {
            assert_eq!(
                resolve_playback_episode(&movie, &[release], true),
                None,
                "{release}"
            );
        }
        assert_eq!(
            resolve_playback_episode(&movie, &["Some Movie - 02.mkv"], false),
            Some(2)
        );
        let series = Matcher {
            episodes: Some(12),
            ..mk(1, "Some Movie")
        };
        assert_eq!(
            resolve_playback_episode(&series, &["Some Movie.mkv"], true),
            None
        );
        assert_eq!(resolve_episode(&movie, &["Some Movie.mkv"]), None);
        for title in [
            "Evangelion: 1.0 You Are (Not) Alone",
            "5 Centimeters per Second",
            "1.0",
        ] {
            let movie = Matcher {
                episodes: Some(1),
                ..mk(1, title)
            };
            let filename = format!("[G] {title} [1080p].mkv");
            assert_eq!(
                resolve_playback_episode(&movie, &[&filename], false),
                Some(1),
                "{filename}"
            );
        }
    }

    #[test]
    fn adversarial_movie_bonus_videos_cannot_complete_the_feature() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "Some Movie")
        };
        for release in [
            "Some Movie [OVA].mkv",
            "Some Movie_OAD1.mkv",
            "Some Movie [SP01].mkv",
            "OP1.mkv",
            "Some Movie [ED1].mkv",
            "Some Movie [OST].mkv",
        ] {
            for bound in [false, true] {
                assert_eq!(
                    resolve_playback_episode(&movie, &[release, "Some Movie"], bound),
                    None,
                    "{release}, bound: {bound}"
                );
            }
        }
    }

    #[test]
    fn adversarial_single_episode_ova_titles_remain_watchable() {
        for title in ["Some Show OVA", "Some Show OAD", "OP", "ED", "SP"] {
            let movie = Matcher {
                episodes: Some(1),
                ..mk(1, title)
            };
            let filename = format!("[G] {title} [BD 1080p].mkv");
            for bound in [false, true] {
                assert_eq!(
                    resolve_playback_episode(&movie, &[&filename], bound),
                    Some(1),
                    "{filename}, bound: {bound}"
                );
            }
        }
    }

    #[test]
    fn movie_extras_with_release_separators_are_not_watchable_episodes() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "Some Movie")
        };
        for release in [
            "Some_Movie_Sample.mkv",
            "Some Movie [PV01].mkv",
            "Some Movie [NCOP01].mkv",
            "Some Movie Trailer01.mkv",
            "sample01.mkv",
        ] {
            assert_eq!(
                resolve_playback_episode(&movie, &[release], true),
                None,
                "{release}"
            );
        }
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "The Special")
        };
        assert_eq!(
            resolve_playback_episode(&movie, &["The.Special.mkv"], false),
            Some(1)
        );
        for title in ["X", "1"] {
            let movie = Matcher {
                episodes: Some(1),
                ..mk(1, title)
            };
            let extra = format!("[G] {title} Extras01.mkv");
            assert_eq!(
                resolve_playback_episode(&movie, &[&extra], true),
                None,
                "{extra}"
            );
            let range = format!("[G] {title} [11-12].mkv");
            assert_eq!(
                resolve_playback_episode(&movie, &[&range], true),
                None,
                "{range}"
            );
        }
    }

    #[test]
    fn clean_strips_group_resolution_and_episode_tail() {
        assert_eq!(
            clean_title("[SubsPlease] Frieren - 28 (1080p) [AB12CD34].mkv"),
            "frieren"
        );
        assert_eq!(
            clean_title("[Erai-raws] Kusuriya no Hitorigoto - 05 [720p].mkv"),
            "kusuriya no hitorigoto"
        );
    }

    #[test]
    fn clean_handles_v2_and_ep_prefix() {
        assert_eq!(clean_title("Some Show - 04v2 [BD 1080p].mkv"), "some show");
        assert_eq!(clean_title("Another Show EP11.mkv"), "another show");
    }

    #[test]
    fn clean_keeps_title_final_e_and_4_digit_episodes() {
        assert_eq!(clean_title("Steins;Gate 01"), "steins gate");
        assert_eq!(clean_title("Fate 01"), "fate");
        assert_eq!(clean_title("Steins;Gate - 01"), "steins gate");
        assert_eq!(clean_title("One Piece 1015"), "one piece");
        assert_eq!(clean_title("Show E05"), "show");
        assert_eq!(clean_title("Show - E05"), "show");
        assert_eq!(clean_title("Show S02E05"), "show");
    }

    #[test]
    fn basename_decodes_and_strips() {
        assert_eq!(
            basename("file:///media/anime/My%20Show%20-%2003.mkv"),
            "My Show - 03"
        );
        assert_eq!(
            basename("file:///x/%E3%82%AF%E3%83%AA%E3%82%B9.mkv"),
            "クリス"
        );
    }

    #[test]
    fn episode_guess_ignores_resolutions_and_years() {
        assert_eq!(parse_episode_guess("Show - 07 [1080p]"), Some(7));
        assert_eq!(parse_episode_guess("Movie 2016 [BD]"), None);
        assert_eq!(parse_episode_guess("no numbers here"), None);
    }

    #[test]
    fn episode_guess_takes_real_episode_over_detached_revision() {
        assert_eq!(parse_episode_guess("Show - 05 v2 1080p"), Some(5));
        assert_eq!(parse_episode_guess("[Group] Show - 05 v2 x264"), Some(5));
        assert_eq!(parse_episode_guess("Show - 05 v2"), Some(5));
        assert_eq!(parse_episode_guess("Show - 05v2"), Some(5));
    }

    #[test]
    fn guessing_keeps_episode_digits_separate_from_release_tags() {
        for release in [
            "Unknown - 05 DDP2.0",
            "Unknown.05.DDP2.0",
            "Unknown - 05 Hi10P AC3",
            "Unknown - 05 8-bit yuv420p10",
        ] {
            assert_eq!(parse_episode_guess(release), Some(5), "{release}");
        }
        for release in [
            "Unknown - 2024 - 12.5 [1080p]",
            "Unknown 2024 - 12.5 [1080p]",
            "Unknown 2024-E01-E12",
            "Unknown 2024-01~12",
        ] {
            assert_eq!(parse_episode_guess(release), None, "{release}");
        }
    }

    #[test]
    fn four_digit_scene_episodes_survive_title_normalization() {
        let matchers = [mk(1, "Some Show"), mk(2, "Some Show Season 2")];
        for release in [
            "Some.Show.S02E1015.1080p.mkv",
            "Some Show [S02E1015] [1080p].mkv",
        ] {
            let matched = match_title(&matchers, release, "").unwrap();
            assert_eq!(matched.media_id, 2, "{release}");
            assert_eq!(
                resolve_episode(matched, &[release]),
                Some(1015),
                "{release}"
            );
        }
    }

    #[test]
    fn movie_tags_keep_bonus_markers_after_metadata_is_removed() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk(1, "Some Movie Season 2")
        };
        for tag in [
            "[Season 2 Preview]",
            "[Season 2 NCOP01]",
            "[Season 2] [1080p_Sample]",
            "[Season 2] [NCOP AAC2.0]",
            "[Season 2] [Menu]",
            "[Season 2] [CM01]",
            "[Season 2] Extras 01",
            "[Season 2] [S01_E02-S02_E01]",
            "[Season 2] 2024-01-02",
        ] {
            let release = format!("[G] Some Movie {tag}.mkv");
            for bound in [false, true] {
                assert_eq!(
                    resolve_playback_episode(&movie, &[&release], bound),
                    None,
                    "{release}"
                );
            }
        }
        let release = "[G] Some Movie [Season 2 (10-bit)] [1080p].mkv";
        assert_eq!(
            resolve_playback_episode(&movie, &[release], false),
            Some(1),
            "{release}"
        );
    }

    #[test]
    fn episode_zero_parses() {
        assert_eq!(parse_episode_guess("Show - 00"), Some(0));
        assert_eq!(parse_episode_guess("Show 0"), Some(0));
        assert_eq!(parse_episode_guess("Show - 00 (Prologue) [1080p]"), Some(0));
    }

    #[test]
    fn episode_guess_ignores_crc_and_codec_digits() {
        assert_eq!(
            parse_episode_guess("[SubsPlease] Sousou no Frieren - 28 (1080p) [AB12CD34].mkv"),
            Some(28)
        );
        assert_eq!(
            parse_episode_guess("[Group] Show - 07 [1080p x264-10bit].mkv"),
            Some(7)
        );
        assert_eq!(
            parse_episode_guess("Some Show - 04v2 [BD 1080p].mkv"),
            Some(4)
        );
        assert_eq!(
            parse_episode_guess("[GJM] 86 - 11 (1080p) [DEADBEEF].mkv"),
            Some(11)
        );
    }

    #[test]
    fn episode_guess_ignores_trailing_release_tags() {
        assert_eq!(
            parse_episode_guess("[Group] Show - 05 [1080p] AAC2.0 x265"),
            Some(5)
        );
        assert_eq!(parse_episode_guess("Show - 05 DDP2.0 H 264"), Some(5));
        assert_eq!(parse_episode_guess("Show S02E05 AAC2.0"), Some(5));
        assert_eq!(parse_episode_guess("Show - 12 Opus2.0"), Some(12));
        assert_eq!(
            parse_episode_guess("Show - 05 [Multi-Subs] DDP5.1"),
            Some(5)
        );
        assert_eq!(
            parse_episode_guess("[SubsPlease] Frieren - 28 (1080p) [AB12CD34]"),
            Some(28)
        );
    }

    #[test]
    fn resolution_valued_episode_numbers_parse() {
        assert_eq!(
            parse_episode_guess("One Piece - 1080 (1080p) [ABC123]"),
            Some(1080)
        );
        assert_eq!(parse_episode_guess("One Piece - 360 [1080p]"), Some(360));
        assert_eq!(parse_episode_guess("One Piece - 480 (720p)"), Some(480));
        assert_eq!(parse_episode_guess("One Piece - 720"), Some(720));
        assert_eq!(parse_episode_guess("Show - 05 1080"), Some(5));
        assert_eq!(parse_episode_guess("Show 1080p"), None);
        assert_eq!(
            parse_episode_guess("[GJM] 86 - 11 (1080p) [DEADBEEF].mkv"),
            Some(11)
        );
        assert_eq!(parse_episode_guess("Movie 2016 [BD]"), None);
    }

    #[test]
    fn episode_guess_ignores_fps_4k_and_detached_revisions() {
        assert_eq!(
            parse_episode_guess("Show.S02E05.1080p60.WEB-DL.H.264-GROUP"),
            Some(5)
        );
        assert_eq!(parse_episode_guess("Show - 05 4K"), Some(5));
        assert_eq!(parse_episode_guess("Show - 05 (1080p) v2"), Some(5));
    }

    #[test]
    fn trailing_season_n_survives_the_tail_strip() {
        assert_eq!(
            clean_title("[Judas] Kusuriya no Hitorigoto Season 2 [1080p]"),
            "kusuriya no hitorigoto 2"
        );
        assert_eq!(
            clean_title("[Judas] Kusuriya no Hitorigoto 2nd Season [1080p]"),
            "kusuriya no hitorigoto 2"
        );
        assert_eq!(
            clean_title("Kusuriya no Hitorigoto Season 2 - 05 [1080p]"),
            "kusuriya no hitorigoto 2"
        );
        let matchers = vec![
            mk(1, "Kusuriya no Hitorigoto"),
            mk(2, "Kusuriya no Hitorigoto 2nd Season"),
        ];
        assert_eq!(
            match_title(
                &matchers,
                "[Judas] Kusuriya no Hitorigoto Season 2 [1080p]",
                ""
            )
            .map(|m| m.media_id),
            Some(2)
        );
    }

    fn parse_episode_guess(release: &str) -> Option<i64> {
        resolve_episode(&mk(1, "Unmatched fixture title"), &[release])
    }

    fn parse_episode_after(release: &str, title: &str) -> Option<Option<i64>> {
        let matcher = mk(1, title);
        matcher.release_title_span(release)?;
        Some(resolve_episode(&matcher, &[release]))
    }

    #[test]
    fn title_removal_prefers_titles_over_metadata_and_preserves_fallback() {
        for (title, input, expected) in [
            ("86", "[86ABCDEF] [86] - 03", Some(Some(3))),
            ("86", "[86 Fansubs] 【86】 - 03", Some(Some(3))),
            ("91 Days", "[91 Days Fansubs] 91 Days - 05", Some(Some(5))),
            (
                "Some Show",
                "[Some Show Fansubs] Some Show - 03",
                Some(Some(3)),
            ),
            ("Some Show", "[Some Show - 03]", Some(None)),
            ("86", "[86ABCDEF] 86 [1080p]", Some(None)),
            ("86", "[86] - 086", Some(Some(86))),
            ("86", "[86] - 186", Some(Some(186))),
            ("86", "[86] - 86", Some(Some(86))),
            ("86", "[86] - 86 (1080p)", Some(Some(86))),
            ("86", "Other Show - 03", None),
        ] {
            assert_eq!(parse_episode_after(input, title), expected, "{input}");
        }
    }

    #[test]
    fn episode_after_title_removal_avoids_title_numbers() {
        let title = "91 Days";
        assert_eq!(
            parse_episode_after("[Group] 91 Days - 05 [1080p]", title),
            Some(Some(5))
        );
        assert_eq!(parse_episode_after("91 Days [BD 1080p]", title), Some(None));
        assert_eq!(parse_episode_after("Something Else - 03", title), None);
    }

    #[test]
    fn resolve_episode_batch_alias_and_crc() {
        let days = Matcher {
            media_id: 1,
            display: "91 Days".into(),
            variants: vec!["91 Days".into()],
            norms: vec!["91 days".into()],
            explicit_season: None,
            norm_seasons: vec![None],
            episodes: None,
            format: None,
            status_rank: 0,
            title_patterns: OnceLock::new(),
            season_patterns: OnceLock::new(),
            source: None,
        };
        assert_eq!(
            resolve_episode(&days, &["91 Days", "91 Days [BD 1080p]"]),
            None
        );
        assert_eq!(
            resolve_episode(&days, &["91 Days", "91 Days - 05 [BD]"]),
            Some(5)
        );
        let rezero = Matcher {
            media_id: 2,
            display: "Re:Zero".into(),
            variants: vec!["Re:Zero kara Hajimeru Isekai Seikatsu".into()],
            norms: vec!["re zero kara hajimeru isekai seikatsu".into()],
            explicit_season: None,
            norm_seasons: vec![None],
            episodes: None,
            format: None,
            status_rank: 0,
            title_patterns: OnceLock::new(),
            season_patterns: OnceLock::new(),
            source: None,
        };
        assert_eq!(
            resolve_episode(&rezero, &["Re Zero - 05", "Re Zero - 05"]),
            Some(5)
        );
    }

    #[test]
    fn match_title_exact_then_containment() {
        let m = Matcher {
            media_id: 1,
            display: "Frieren".into(),
            variants: vec!["Sousou no Frieren".into()],
            norms: vec!["sousou no frieren".into()],
            explicit_season: None,
            norm_seasons: vec![None],
            episodes: None,
            format: None,
            status_rank: 0,
            title_patterns: OnceLock::new(),
            season_patterns: OnceLock::new(),
            source: None,
        };
        let matchers = vec![m];
        assert!(match_title(&matchers, "Sousou no Frieren - 28", "").is_some());
        assert!(match_title(&matchers, "", "file:///x/[G] Sousou no Frieren - 28.mkv").is_some());
        assert!(match_title(&matchers, "Totally Different Show", "").is_none());
    }

    #[test]
    fn match_title_without_separator_before_episode() {
        let matchers = vec![mk(1, "Steins;Gate"), mk(2, "Fate"), mk(3, "One Piece")];
        assert_eq!(
            match_title(&matchers, "Steins;Gate 01", "").map(|m| m.media_id),
            Some(1)
        );
        assert_eq!(
            match_title(&matchers, "Fate 01", "").map(|m| m.media_id),
            Some(2)
        );
        assert_eq!(
            match_title(&matchers, "One Piece 1015", "").map(|m| m.media_id),
            Some(3)
        );
    }

    fn mk(media_id: i64, title: &str) -> Matcher {
        Matcher {
            media_id,
            display: title.into(),
            variants: vec![title.into()],
            norms: vec![norm_title(title)],
            norm_seasons: vec![catalog_season_ordinal(title, &norm_title(title))],
            explicit_season: catalog_season(title),
            episodes: None,
            format: None,
            status_rank: 0,
            title_patterns: OnceLock::new(),
            season_patterns: OnceLock::new(),
            source: None,
        }
    }

    fn mk_status(media_id: i64, title: &str, status: &str) -> Matcher {
        Matcher {
            episodes: None,
            status_rank: status_rank(status),
            ..mk(media_id, title)
        }
    }

    #[test]
    fn sequels_beat_the_base_series() {
        let matchers = vec![
            mk(1, "Boku no Hero Academia"),
            mk(2, "Boku no Hero Academia 7"),
        ];
        for release in [
            "[Erai-raws] Boku no Hero Academia 7th Season - 05 [1080p].mkv",
            "Boku no Hero Academia Season 7 - 05.mkv",
            "Boku no Hero Academia 7 - 05.mkv",
        ] {
            assert_eq!(
                match_title(&matchers, release, "").map(|m| m.media_id),
                Some(2),
                "{release} should resolve to the sequel"
            );
        }
        assert_eq!(
            match_title(&matchers, "Boku no Hero Academia - 05.mkv", "").map(|m| m.media_id),
            Some(1)
        );
    }

    #[test]
    fn an_unnumbered_release_does_not_fall_forward_to_a_sequel() {
        let matchers = vec![mk(2, "Boku no Hero Academia 2nd Season")];
        assert!(match_title(&matchers, "Boku no Hero Academia - 05.mkv", "").is_none());
        assert_eq!(
            match_title(&matchers, "Boku no Hero Academia S02E05.mkv", "").map(|m| m.media_id),
            Some(2)
        );
    }

    #[test]
    fn bracketed_episode_precedes_numeric_episode_titles() {
        let show = mk(1, "Some Show");
        for title in ["Countdown 1-2-3", "Only 12.5 Seconds", "1-2-3 Go"] {
            let release = format!("[Group] Some Show [E03] - {title} [1080p].mkv");
            assert_eq!(
                resolve_playback_episode(&show, &[&release], false),
                Some(3),
                "{release}"
            );
        }
    }

    #[test]
    fn episode_fallback_ignores_group_numbers_and_preserves_whole_pack_tags() {
        let show = mk(1, "Some Show");
        for group in ["[Fansubs S01-S02]", "[Fansubs 01-24]", "[Group 1.5]"] {
            let release = format!("{group} 05.mkv");
            assert_eq!(resolve_episode(&show, &[&release]), Some(5), "{release}");
            assert_eq!(
                resolve_playback_episode(&show, &[&release], true),
                Some(5),
                "{release}"
            );
        }
        for release in [
            "[S01-S02] 05.mkv",
            "[01-12] 05.mkv",
            "[S01E01-S02E12] 05.mkv",
            "[05.5] 05.mkv",
        ] {
            assert_eq!(resolve_episode(&show, &[release]), None, "{release}");
            assert_eq!(
                resolve_playback_episode(&show, &[release], true),
                None,
                "{release}"
            );
        }
    }

    #[test]
    fn normalized_alias_bracket_episodes_respect_ranges_and_fractions() {
        let show = mk(1, "Some Show (2024)");
        for (suffix, expected) in [
            ("[05] [1080p]", Some(5)),
            ("[05] - 12.5 Seconds", Some(5)),
            ("[05] [01-12] [BD]", None),
            ("[01-12] (2)", None),
            ("[05] [05.5] [BD]", None),
            ("[05.5] [05] [BD]", None),
            ("[05] [S01E01-S02E12] [BD]", None),
        ] {
            let release = format!("Some Show {suffix}.mkv");
            assert_eq!(resolve_episode(&show, &[&release]), expected, "{release}");
        }
    }

    #[test]
    fn batch_ranges_do_not_resolve_to_a_single_episode() {
        let show = mk(1, "Some Show");
        for release in [
            "Some Show - 01-12 [1080p]",
            "Some Show - 01 ~ 12 [1080p]",
            "Some Show E01-E12 [1080p]",
            "Some Show S01E01-E12 [1080p]",
            "Some Show [01-12] [1080p]",
            "Some Show - 01-12 [E03]",
            "Some Show - ٠١-١٢",
        ] {
            assert_eq!(resolve_episode(&show, &[release]), None, "{release}");
        }
        assert_eq!(
            resolve_episode(&show, &["Some Show E03 - 1-2-3 Go!"]),
            Some(3)
        );
        assert_eq!(
            resolve_episode(&show, &["Some Show - 03 - 1-2-3 Go!"]),
            Some(3)
        );
        assert_eq!(parse_episode_guess("Show Season 2 - 05"), Some(5));
        assert_eq!(parse_episode_guess("Show 2 - 05"), Some(5));
    }

    #[test]
    fn season_ordinal_absent_from_list_stays_unmatched() {
        let matchers = vec![
            mk_status(101, "Re:Zero kara Hajimeru Isekai Seikatsu", "COMPLETED"),
            mk_status(
                102,
                "Re:Zero kara Hajimeru Isekai Seikatsu -Memory Snow-",
                "COMPLETED",
            ),
            mk_status(
                103,
                "Re:Zero kara Hajimeru Isekai Seikatsu -Hyouketsu no Kizuna-",
                "COMPLETED",
            ),
            mk_status(
                104,
                "Re:Zero kara Hajimeru Isekai Seikatsu 2nd Season",
                "COMPLETED",
            ),
        ];
        for release in [
            "[SubsPlease] Re:Zero kara Hajimeru Isekai Seikatsu Season 4 - 01 [1080p].mkv",
            "[SubsPlease] Re:Zero kara Hajimeru Isekai Seikatsu 4 - 01 [1080p].mkv",
        ] {
            assert_eq!(
                match_title(&matchers, release, "").map(|m| m.media_id),
                None,
                "{release} names a season that is not on the list, no sibling may take it"
            );
        }
    }

    #[test]
    fn season_ordinal_present_on_list_resolves() {
        let matchers = vec![
            mk_status(101, "Re:Zero kara Hajimeru Isekai Seikatsu", "COMPLETED"),
            mk_status(
                105,
                "Re:Zero kara Hajimeru Isekai Seikatsu 4th Season",
                "CURRENT",
            ),
        ];
        for release in [
            "[SubsPlease] Re:Zero kara Hajimeru Isekai Seikatsu Season 4 - 01 [1080p].mkv",
            "[SubsPlease] Re:Zero kara Hajimeru Isekai Seikatsu 4 - 01 [1080p].mkv",
        ] {
            assert_eq!(
                match_title(&matchers, release, "").map(|m| m.media_id),
                Some(105),
                "{release} must resolve to the on-list season 4"
            );
        }
    }

    #[test]
    fn season_one_markers_match_the_base_entry() {
        let matchers = vec![
            mk(1, "Boku no Hero Academia"),
            mk(2, "Boku no Hero Academia 7"),
        ];
        for release in [
            "[Judas] Boku no Hero Academia - S01E05.mkv",
            "[G] Boku no Hero Academia Season 1 - 05.mkv",
            "[G] Boku no Hero Academia S1 - 05.mkv",
            "[G] Boku no Hero Academia 1 - 05.mkv",
        ] {
            assert_eq!(
                match_title(&matchers, release, "").map(|m| m.media_id),
                Some(1),
                "{release} names season 1, which only the ordinal-less base entry can be"
            );
        }
        let s2_only = vec![mk(2, "Boku no Hero Academia 2nd Season")];
        assert_eq!(
            match_title(&s2_only, "[Judas] Boku no Hero Academia - S01E05.mkv", "")
                .map(|m| m.media_id),
            None
        );
    }

    #[test]
    fn bare_season_token_routes_to_the_matching_season() {
        let matchers = vec![mk(1, "Show"), mk(2, "Show 3rd Season")];
        assert_eq!(
            match_title(&matchers, "[G] Show S3 - 05.mkv", "").map(|m| m.media_id),
            Some(2)
        );
        let base_only = vec![mk(1, "Show")];
        assert_eq!(
            match_title(&base_only, "[G] Show S3 - 05.mkv", "").map(|m| m.media_id),
            None
        );
        assert_eq!(
            match_title(&base_only, "[G] Show - 05 [AB12S2].mkv", "").map(|m| m.media_id),
            Some(1)
        );
    }

    #[test]
    fn ordinal_cap_agrees_across_extractors() {
        let matchers = vec![mk(1, "Show"), mk(2, "Show Season 40")];
        assert_eq!(
            match_title(&matchers, "[G] Show - S40E05.mkv", "").map(|m| m.media_id),
            Some(2)
        );
    }

    #[test]
    fn a_bare_number_title_survives_the_tail_strip() {
        assert_eq!(clean_title("86.mkv"), "86");
        assert_eq!(clean_title("91.mkv"), "91");
        assert_eq!(clean_title("86 - 05.mkv"), "86");
    }

    #[test]
    fn duplicate_titles_break_ties_deterministically() {
        let watching = vec![
            mk_status(10, "Some Show", "COMPLETED"),
            mk_status(20, "Some Show", "CURRENT"),
        ];
        assert_eq!(
            match_title(&watching, "Some Show - 03", "").map(|m| m.media_id),
            Some(20)
        );
        let a = vec![
            mk_status(20, "Some Show", "CURRENT"),
            mk_status(10, "Some Show", "CURRENT"),
        ];
        let b = vec![
            mk_status(10, "Some Show", "CURRENT"),
            mk_status(20, "Some Show", "CURRENT"),
        ];
        assert_eq!(
            match_title(&a, "Some Show - 03", "").map(|m| m.media_id),
            Some(10)
        );
        assert_eq!(
            match_title(&b, "Some Show - 03", "").map(|m| m.media_id),
            Some(10)
        );
    }

    #[test]
    fn percent_decode_leaves_stray_percent_signs_alone() {
        assert_eq!(basename("file:///x/50%20off.mkv"), "50 off");
        assert_eq!(basename("file:///x/50% オフ.mkv"), "50% オフ");
        assert_eq!(basename("file:///x/100%.mkv"), "100%");
        assert_eq!(basename("file:///x/%zz.mkv"), "%zz");
        assert_eq!(
            basename("file:///x/%E3%82%AF%E3%83%AA%E3%82%B9.mkv"),
            "クリス"
        );
    }

    #[test]
    fn token_containment_handles_multibyte_titles() {
        let matchers = vec![mk(1, "四月は君の嘘"), mk(2, "Shigatsu wa Kimi no Uso")];
        assert!(match_title(&matchers, "[G] 四月は君の嘘 - 05 [1080p].mkv", "").is_some());
        assert!(match_title(&matchers, "ぜんぜん違う番組", "").is_none());
    }

    #[test]
    fn strip_ext_and_norms_keep_real_titles() {
        assert_eq!(norm_title("No.6"), "no 6");
        assert_eq!(norm_title("D.Gray-man"), "d gray man");
        assert_eq!(norm_title("Dr. STONE"), "dr stone");
        assert_eq!(norm_title("86"), "86");
        assert_eq!(norm_title("Steins;Gate 0"), "steins gate 0");
        assert_eq!(
            norm_title("Ghost in the Shell 2.0"),
            "ghost in the shell 2 0"
        );
        assert_eq!(clean_title("Show Name - 03.mkv"), "show name");
        assert_eq!(clean_title("Show Name - 03.MKV"), "show name");
        assert_eq!(
            clean_title("Clevatess S02E03 CR WEB-DL DUAL AAC2.0 H.264 (Clevatess: Majuu no Ou)"),
            "clevatess 2 cr dual"
        );
    }

    #[test]
    fn short_titles_do_not_mismatch() {
        let matchers = vec![
            mk(1, "Another"),
            mk(2, "86"),
            mk(3, "Dr. STONE"),
            mk(4, "No.6"),
            mk(5, "K"),
        ];
        assert!(match_title(
            &matchers,
            "[G] Re:Zero Starting Life in Another World - 05",
            ""
        )
        .is_none());
        assert!(
            match_title(&matchers, "[ToonsHub] Grand Blue Dreaming S03E03 1080p", "").is_none()
        );
        assert!(match_title(&matchers, "[G] Sora wa Akai Kawa no Hotori - 03", "").is_none());
        assert!(match_title(&matchers, "Walking the Way All Alone S01E16 1080p", "").is_none());
        assert_eq!(
            match_title(&matchers, "K - 05", "").map(|m| m.media_id),
            Some(5)
        );
        assert_eq!(
            match_title(&matchers, "[SubsPlease] Another - 05 (1080p)", "").map(|m| m.media_id),
            Some(1)
        );
        assert_eq!(
            match_title(&matchers, "[SubsPlease] 86 - 11 (1080p)", "").map(|m| m.media_id),
            Some(2)
        );
        assert!(match_title(&matchers, "[SubsPlease] Dr. Stone S3 - 05 (1080p)", "").is_none());
        assert_eq!(
            match_title(&matchers, "[G] No.6 - 03 [720p]", "").map(|m| m.media_id),
            Some(4)
        );
    }

    #[test]
    fn token_boundary_prefix_and_interior() {
        let matchers = vec![mk(1, "Ghost in the Shell"), mk(2, "Shiro")];
        assert_eq!(
            match_title(&matchers, "[Kotobuki] Koukaku Kidoutai (2026) 03 [1080p HEVC Multisub] | The Ghost in the Shell", "")
                .map(|m| m.media_id),
            Some(1)
        );
        assert!(match_title(&matchers, "[G] Shirobako - 05", "").is_none());
        let rezero = vec![mk(1, "Re:Zero kara Hajimeru Isekai Seikatsu")];
        assert_eq!(
            match_title(&rezero, "Re Zero - 05", "").map(|m| m.media_id),
            Some(1)
        );
        let yama = vec![mk(1, "Yama no Susume")];
        assert_eq!(
            match_title(&yama, "[G] Yama no Susume Next Summit - 03", "").map(|m| m.media_id),
            Some(1)
        );
        let pair = vec![mk(1, "Toradora"), mk(2, "Toradora SOS")];
        assert_eq!(
            match_title(&pair, "[G] Toradora SOS - 02", "").map(|m| m.media_id),
            Some(2)
        );
    }

    #[test]
    fn generic_match_prefers_current_over_longest_title() {
        let titles: &[(i64, &str, &str)] = &[
            (108465, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation"),
            (
                127720,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Cour 2",
            ),
            (
                141534,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer",
            ),
            (
                146065,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Season 2",
            ),
            (
                166873,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Season 2 Part 2",
            ),
            (
                178789,
                "CURRENT",
                "Mushoku Tensei: Jobless Reincarnation Season 3",
            ),
        ];
        let rom_by_id: std::collections::HashMap<i64, &str> = [
            (108465, "Mushoku Tensei: Isekai Ittara Honki Dasu"),
            (127720, "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2"),
            (
                141534,
                "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2 - Eris no Goblin Toubatsu",
            ),
            (146065, "Mushoku Tensei II: Isekai Ittara Honki Dasu"),
            (166873, "Mushoku Tensei II: Isekai Ittara Honki Dasu Part 2"),
            (178789, "Mushoku Tensei III: Isekai Ittara Honki Dasu"),
        ]
        .into_iter()
        .collect();
        let matchers: Vec<Matcher> = titles
            .iter()
            .map(|(id, status, en)| Matcher {
                media_id: *id,
                display: (*en).into(),
                variants: vec![(*en).into(), rom_by_id[id].into()],
                norms: vec![norm_title(en), norm_title(rom_by_id[id])],
                explicit_season: None,
                norm_seasons: vec![
                    norm_season_ordinal(&norm_title(en)),
                    norm_season_ordinal(&norm_title(rom_by_id[id])),
                ],
                episodes: None,
                format: None,
                status_rank: status_rank(status),
                title_patterns: OnceLock::new(),
                season_patterns: OnceLock::new(),
                source: None,
            })
            .collect();
        for raw in [
            "[Judas] Mushoku Tensei - S03E04.mkv",
            "[Judas] Mushoku Tensei - S03E05.mkv",
        ] {
            let url = format!("file:///home/cate/Videos/Torrents/{raw}");
            let m = match_title(&matchers, raw, &url).expect("should match a franchise entry");
            assert_eq!(
                m.media_id, 178789,
                "S03 release must resolve to Season 3 (CURRENT)"
            );
            let ep = resolve_episode(m, &[raw, basename(&url).as_str()]);
            assert_eq!(ep, raw.contains("E04").then_some(4).or(Some(5)));
        }
    }

    #[test]
    fn season_ordinal_beats_same_status_siblings() {
        let titles: &[(i64, &str, &str)] = &[
            (108465, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation"),
            (
                141534,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer",
            ),
            (
                146065,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Season 2",
            ),
            (
                178789,
                "COMPLETED",
                "Mushoku Tensei: Jobless Reincarnation Season 3",
            ),
        ];
        let rom_by_id: std::collections::HashMap<i64, &str> = [
            (108465, "Mushoku Tensei: Isekai Ittara Honki Dasu"),
            (
                141534,
                "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2 - Eris no Goblin Toubatsu",
            ),
            (146065, "Mushoku Tensei II: Isekai Ittara Honki Dasu"),
            (178789, "Mushoku Tensei III: Isekai Ittara Honki Dasu"),
        ]
        .into_iter()
        .collect();
        let matchers: Vec<Matcher> = titles
            .iter()
            .map(|(id, status, en)| Matcher {
                media_id: *id,
                display: (*en).into(),
                variants: vec![(*en).into(), rom_by_id[id].into()],
                norms: vec![norm_title(en), norm_title(rom_by_id[id])],
                explicit_season: None,
                norm_seasons: vec![
                    norm_season_ordinal(&norm_title(en)),
                    norm_season_ordinal(&norm_title(rom_by_id[id])),
                ],
                episodes: None,
                format: None,
                status_rank: status_rank(status),
                title_patterns: OnceLock::new(),
                season_patterns: OnceLock::new(),
                source: None,
            })
            .collect();
        let m = match_title(&matchers, "[Judas] Mushoku Tensei - S03E05.mkv", "")
            .expect("should match");
        assert_eq!(
            m.media_id, 178789,
            "S03 must resolve to Season 3 even when all are COMPLETED"
        );
    }

    #[test]
    fn season_ordinal_beats_status_rank() {
        let matchers = vec![
            Matcher {
                media_id: 146065,
                display: "Mushoku Tensei: Jobless Reincarnation Season 2".into(),
                variants: vec!["Mushoku Tensei: Jobless Reincarnation Season 2".into()],
                norms: vec![norm_title("Mushoku Tensei: Jobless Reincarnation Season 2")],
                explicit_season: None,
                norm_seasons: vec![Some(2)],
                episodes: None,
                format: None,
                status_rank: status_rank("CURRENT"),
                title_patterns: OnceLock::new(),
                season_patterns: OnceLock::new(),
                source: None,
            },
            Matcher {
                media_id: 178789,
                display: "Mushoku Tensei: Jobless Reincarnation Season 3".into(),
                variants: vec!["Mushoku Tensei: Jobless Reincarnation Season 3".into()],
                norms: vec![norm_title("Mushoku Tensei: Jobless Reincarnation Season 3")],
                explicit_season: None,
                norm_seasons: vec![Some(3)],
                episodes: None,
                format: None,
                status_rank: status_rank("PAUSED"),
                title_patterns: OnceLock::new(),
                season_patterns: OnceLock::new(),
                source: None,
            },
        ];
        let m = match_title(&matchers, "[Judas] Mushoku Tensei - S03E05.mkv", "")
            .expect("should match");
        assert_eq!(
            m.media_id, 178789,
            "S03 release must resolve to Season 3 (PAUSED) not S2 (CURRENT)"
        );
    }

    #[test]
    fn season_pack_does_not_parse_season_as_episode() {
        let title = "Some Show";
        assert_eq!(
            parse_episode_after("Some Show S02 [1080p]", title),
            Some(None)
        );
        assert_eq!(
            parse_episode_after("Some Show S02E05 [1080p]", title),
            Some(Some(5))
        );
    }

    #[test]
    fn no_season_marker_falls_back_to_status() {
        let matchers = vec![
            mk_status(1, "Some Show", "COMPLETED"),
            mk_status(2, "Some Show", "CURRENT"),
        ];
        assert_eq!(
            match_title(&matchers, "Some Show - 03", "").map(|m| m.media_id),
            Some(2)
        );
    }

    #[test]
    fn dotted_scene_marker_routes_to_the_matching_season() {
        let matchers = vec![
            mk_status(21355, "Re:Zero kara Hajimeru Isekai Seikatsu", "COMPLETED"),
            mk_status(
                189046,
                "Re:Zero kara Hajimeru Isekai Seikatsu 4th Season",
                "CURRENT",
            ),
        ];
        let release = "Re.Zero.kara.Hajimeru.Isekai.Seikatsu.S04E05.1080p.WEB-DL.mkv";
        let m = match_title(&matchers, release, "").expect("dot scene form must match season 4");
        assert_eq!(m.media_id, 189046);
        assert_eq!(resolve_episode(m, &[release]), Some(5));
    }

    #[test]
    fn playback_urls_ignore_query_numbers_and_season_markers() {
        let matchers = vec![mk(1, "Frieren")];
        let url = "https://example.org/Frieren%20-%2005.mkv?token=S03E9876#part2";
        let base = basename(url);
        assert_eq!(base, "Frieren - 05");
        let matched = match_title(&matchers, "", url).unwrap();
        assert_eq!(resolve_episode(matched, &[&base]), Some(5));
        assert_eq!(
            basename("file:///anime/Frieren%20-%2005%2Emkv"),
            "Frieren - 05"
        );
        assert_eq!(basename("/anime/100%20Real - 05.mkv"), "100%20Real - 05");
    }

    #[test]
    fn video_extension_digits_do_not_override_episodes() {
        let show = mk(1, "Some Show");
        for extension in ["m2ts", "3gp", "m4v", "divx"] {
            let title = format!("Some Show - 05.{extension}");
            assert_eq!(resolve_episode(&show, &[&title]), Some(5), "{title}");
            assert_eq!(clean_title(&title), "some show");
        }
    }

    #[test]
    fn season_alias_without_an_episode_does_not_guess_progress() {
        let show = mk(1, "Some Show 2nd Season");
        for title in ["Some Show Season 2", "Some.Show.2nd.Season.mkv"] {
            assert_eq!(resolve_episode(&show, &[title]), None, "{title}");
        }
    }

    #[test]
    fn adjacent_season_words_and_compact_ranges_are_not_episodes() {
        let show = mk(1, "Some Show");
        for title in [
            "Some Show Season 2/2nd Season",
            "Some Show Season 2 2nd Season",
            "Some Show Season 1-2",
            "Some Show Season 01-02",
            "Some Show Season 1~2",
            "Some Show Season 1–2",
            "Some Show Season 1-2-3",
            "Some Show Seasons 1-2",
            "Some_Show_Seasons_1_-_2",
            "Seasons 1-2",
        ] {
            assert_eq!(parse_episode_guess(title), None, "{title}");
            assert_eq!(
                resolve_playback_episode(&show, &[title], true),
                None,
                "{title}"
            );
        }
        let first_season = mk(1, "Some Show Season 1");
        assert_eq!(
            resolve_episode(&first_season, &["Some Show Season 1-2"]),
            None
        );
        for title in [
            "Some Show [Season 2/2nd Season][E05]",
            "Some Show Season 2 2nd Season S02E05",
            "Some Show Season 2 - 05",
            "Some Show Season 2-05",
        ] {
            assert_eq!(parse_episode_guess(title), Some(5), "{title}");
            assert_eq!(
                resolve_playback_episode(&show, &[title], true),
                Some(5),
                "{title}"
            );
        }
        assert_eq!(parse_episode_guess("Some Show Season 2-10"), Some(10));
    }

    #[test]
    fn title_owns_season_words_and_keeps_official_numbered_editions() {
        let matchers = [
            mk(1, "Some Show"),
            mk(2, "Some Show Final Season"),
            mk(3, "Some Show Season 5"),
            mk(4, "Some Show Final Season 2"),
            mk(5, "Monster"),
            mk(6, "Monster Season"),
            mk(7, "Attack on Titan: Final Season"),
        ];
        for (release, id, episode) in [
            ("Some_Show_Final_Season_05.mkv", 2, Some(5)),
            ("[A] Some_Show_Final_Season_05 [1080p]", 2, Some(5)),
            ("Some_Show_Season_05.mkv", 3, None),
            ("Some Show Final Season 2 [BD]", 4, None),
            ("Some_Show_Final_Season_2_1080p.mkv", 4, None),
            ("Some Show Final Season 2 - 06.mkv", 4, Some(6)),
            ("Monster_Season_05.mkv", 6, Some(5)),
            ("Monster Season - 05.mkv", 6, Some(5)),
            ("Attack on Titan Final Season - 05.mkv", 7, Some(5)),
            ("Some Show Season 1 - 05.mkv", 1, Some(5)),
        ] {
            let matched = match_title(&matchers, release, "").expect(release);
            assert_eq!(matched.media_id, id, "{release}");
            assert_eq!(resolve_episode(matched, &[release]), episode, "{release}");
        }
        assert_eq!(parse_episode_guess("Some_Show_Season_05.mkv"), None);
        assert!(match_title(&matchers[..1], "Some_Show_Season_05.mkv", "").is_none());
    }

    #[test]
    fn year_prefix_does_not_supply_an_episode_to_release_filters() {
        let show = mk(1, "Some Show");
        for release in [
            "Some Show 2024 OVA 01.mkv",
            "Some Show 2024 01-12 [1080p].mkv",
            "Some Show 2024 - 01-12 [1080p].mkv",
            "Some Show 2024 - 05.5 [1080p].mkv",
            "Some Show 2024 - 12.5 [1080p].mkv",
            "Some_Show_2024_-_EP05.5_1080p.mkv",
        ] {
            assert!(excluded_playback_release(&show, release), "{release}");
            assert_eq!(
                resolve_playback_episode(&show, &[release], true),
                None,
                "{release}"
            );
        }
        for release in ["Some Show 2024 - 05.5.mkv", "Some Show 2024 - 12.5.mkv"] {
            assert_eq!(parse_episode_guess(release), None, "{release}");
            assert_eq!(resolve_episode(&show, &[release]), None, "{release}");
        }
        for title in ["Some Show", "Some Show (2024)"] {
            let show = mk(1, title);
            for release in ["Some Show 2024 - 05.mkv", "Some_Show_(2024)_-_05.mkv"] {
                assert_eq!(
                    resolve_playback_episode(&show, &[release], false),
                    Some(5),
                    "{release}"
                );
            }
        }
    }

    #[test]
    fn episode_markers_beat_numbers_in_episode_titles() {
        let show = mk(1, "Some Show");
        for title in [
            "Some Show S01E05 - Part 2",
            "Some Show Episode 05 - Part 2",
            "Some Show - 05 - Episode Title 2",
        ] {
            assert_eq!(resolve_episode(&show, &[title]), Some(5), "{title}");
        }
    }

    #[test]
    fn exact_titles_keep_their_numbers_and_decimal_versions() {
        let matchers = vec![
            mk(1, "Ghost in the Shell"),
            mk(2, "Ghost in the Shell 2.0"),
            mk(3, "Evangelion: 1.0 You Are (Not) Alone"),
            mk(4, "Patlabor"),
            mk(5, "Patlabor 2"),
        ];
        for (title, expected) in [
            ("Ghost in the Shell 2.0.mkv", 2),
            ("Evangelion 1.0 You Are (Not) Alone.mkv", 3),
            ("Patlabor 2.mkv", 5),
        ] {
            let matched = match_title(&matchers, title, "").unwrap();
            assert_eq!(matched.media_id, expected, "{title}");
        }
        let matched = match_title(&matchers, "Patlabor 2.mkv", "").unwrap();
        assert_eq!(resolve_episode(matched, &["Patlabor 2.mkv"]), None);
        let title = "Ghost in the Shell 2.0 - 01.mkv";
        let matched = match_title(&matchers, title, "").unwrap();
        assert_eq!(matched.media_id, 2);
        assert_eq!(resolve_episode(matched, &[title]), Some(1));
        let title = "Patlabor.2.mkv";
        let matched = match_title(&matchers, title, "").unwrap();
        assert_eq!(matched.media_id, 5);
        assert_eq!(resolve_episode(matched, &[title]), None);
    }

    #[test]
    fn explicit_episode_numbers_do_not_become_numeric_sequels() {
        let matchers = vec![mk(1, "Some Show"), mk(2, "Some Show 5")];
        for title in [
            "Some Show - 5.mkv",
            "Some Show - 05.mkv",
            "Some Show E5.mkv",
        ] {
            let matched = match_title(&matchers, title, "").unwrap();
            assert_eq!(matched.media_id, 1, "{title}");
            assert_eq!(resolve_episode(matched, &[title]), Some(5));
        }
    }

    #[test]
    fn interior_alias_exactness_breaks_normalized_ties() {
        let matchers = [mk(1, "Re - Zero Kara"), mk(2, "Re: Zero Kara")];
        for (release, id) in [
            ("Watashi no Anime / Re: Zero Kara - 05", 2),
            ("Watashi no Anime / Re - Zero Kara - 05", 1),
        ] {
            assert_eq!(
                match_title(&matchers, release, "").map(|m| m.media_id),
                Some(id)
            );
        }
    }

    #[test]
    fn clean_trims_dashes_after_the_episode() {
        for dash in ['-', '–', '—'] {
            assert_eq!(
                clean_title(&format!("Some Show 05 {dash} [1080p]")),
                "some show"
            );
        }
    }

    #[test]
    fn dotted_numeric_titles_keep_episode_suffixes_after_resolution_cleanup() {
        let matchers = vec![
            mk(1, "86"),
            mk(2, "Patlabor 2"),
            mk(3, "Ghost in the Shell"),
            mk(4, "Ghost in the Shell 2.0"),
        ];
        for (title, id) in [
            ("86.01.1080p.mkv", 1),
            ("86.S01E01.1080p.mkv", 1),
            ("Patlabor.2.01.1080p.mkv", 2),
            ("Ghost.in.the.Shell.2.0.01.1080p.mkv", 4),
        ] {
            let matched = match_title(&matchers, title, "").unwrap();
            assert_eq!(matched.media_id, id, "{title}");
            assert_eq!(resolve_episode(matched, &[title]), Some(1), "{title}");
        }
    }

    #[test]
    fn episode_brackets_survive_metadata_cleanup() {
        let matchers = vec![mk(1, "Some Show")];
        for title in [
            "[G] Some Show [05] [1080p].mkv",
            "[G] Some Show (05v2) [720p].mkv",
            "[G] Some Show [Episode 05] [AB123456].mkv",
            "[G] Some.Show.[05].[1080p].mkv",
        ] {
            let matched = match_title(&matchers, title, "").unwrap();
            assert_eq!(resolve_episode(matched, &[title]), Some(5), "{title}");
        }
        for title in [
            "Some Show [1080]",
            "Some Show [2026]",
            "Some Show [00000005]",
        ] {
            assert_eq!(resolve_episode(&matchers[0], &[title]), None, "{title}");
        }
    }
    #[test]
    fn catalog_corner_brackets_preserve_titles_and_release_groups() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        seed_refresh_inputs(&db);
        db.upsert_media(&crate::models::Media {
            id: 1,
            title_native: Some("【推しの子】".into()),
            episodes: Some(11),
            ..Default::default()
        })
        .unwrap();
        let matchers = build_matchers(&db);
        for title in [
            "[Group] 推しの子 - 03 [1080p].mkv",
            "[Group] 【推しの子】 - 03 [1080p].mkv",
            "【Group】 推しの子 - 03 [1080p].mkv",
            "推しの子 S01E03.mkv",
        ] {
            let matched = match_title(&matchers, title, "").expect(title);
            assert_eq!(matched.media_id, 1);
            assert_eq!(resolve_episode(matched, &[title]), Some(3), "{title}");
        }
        assert!(match_title(&matchers, "[推しの子] Other Show - 03", "").is_none());
        let siblings = vec![mk(1, "【推しの子】"), mk(3, "【推しの子】 2")];
        for title in ["【推しの子】 S02E03", "推しの子 S02E03"] {
            let matched = match_title(&siblings, title, "").expect(title);
            assert_eq!(matched.media_id, 3);
            assert_eq!(resolve_episode(matched, &[title]), Some(3));
        }
    }

    #[test]
    fn exact_movie_titles_beat_current_cleaned_titles_on_real_paths() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, title, status) in [
            (1, "Ghost in the Shell", "CURRENT"),
            (2, "Ghost in the Shell 2.0", "COMPLETED"),
            (3, "Some Movie", "COMPLETED"),
            (4, "Patlabor", "CURRENT"),
            (5, "Patlabor 2", "COMPLETED"),
        ] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(1),
                format: Some("MOVIE".into()),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: status.into(),
                ..Default::default()
            })
            .unwrap();
        }
        let matchers = build_matchers(&db);
        for (path, expected) in [
            ("file:///movies/Ghost%20in%20the%20Shell%202.0.mkv", 2),
            ("/movies/[Group] Ghost in the Shell 2.0 - 01.mkv", 2),
            ("/movies/Ghost in the Shell.mkv", 1),
            ("/movies/Patlabor 2.mkv", 5),
            ("/movies/[Group] Patlabor 2 - 01.mkv", 5),
        ] {
            let matched = match_title(&matchers, "", path).unwrap();
            assert_eq!(matched.media_id, expected, "{path}");
            let filename = basename(path);
            assert_eq!(
                match_title(&matchers, &filename, path).unwrap().media_id,
                expected,
                "{path}"
            );
            assert_eq!(
                crate::library::resolve_bound_episode(matched, &[&filename], None),
                Some(1)
            );
        }
    }

    #[test]
    fn corner_release_groups_allow_unbound_movie_inference() {
        let movie = Matcher {
            episodes: Some(1),
            ..mk_status(1, "Some Movie", "COMPLETED")
        };
        for path in [
            "/movies/【Group】 Some Movie.mkv",
            "file:///movies/%E3%80%90Group%E3%80%91%20Some%20Movie.mkv",
            "/movies/[Group] Some Movie.mkv",
        ] {
            let filename = basename(path);
            assert_eq!(
                match_title(std::slice::from_ref(&movie), "", path)
                    .unwrap()
                    .media_id,
                movie.media_id
            );
            assert_eq!(
                crate::library::resolve_bound_episode(&movie, &[&filename], None),
                Some(1),
                "{path}"
            );
        }
        for filename in [
            "【Group】 Other Movie.mkv",
            "【Group】 Some Movie Trailer.mkv",
            "【Some Movie】 Other Movie.mkv",
        ] {
            assert_eq!(
                crate::library::resolve_bound_episode(&movie, &[filename], None),
                None,
                "{filename}"
            );
        }
        let bracketed = Matcher {
            episodes: Some(1),
            ..mk(2, "【推しの子】")
        };
        assert_eq!(
            resolve_playback_episode(&bracketed, &["【推しの子】.mkv"], false),
            Some(1)
        );
    }

    #[test]
    fn numbered_no_title_is_not_a_season() {
        let matchers = vec![mk(1, "No.6"), mk(2, "Some Show"), mk(3, "Some Show 6")];
        for title in ["No.6 S01E03.mkv", "[Group] No.6 - 03 [1080p].mkv", "No.6"] {
            let matched = match_title(&matchers, title, "").expect(title);
            assert_eq!(matched.media_id, 1, "{title}");
            assert_eq!(
                resolve_episode(matched, &[title]),
                if title == "No.6" { None } else { Some(3) }
            );
        }
        assert!(match_title(&matchers, "No.6 S02E03", "").is_none());
        assert_eq!(
            match_title(&matchers, "Some Show S06E03", "")
                .unwrap()
                .media_id,
            3
        );
    }
    #[test]
    fn corner_groups_do_not_become_competing_catalog_titles() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (id, title, episodes) in [
            (1, "Some Movie", 1),
            (2, "Group Some Movie", 1),
            (3, "【推しの子】", 11),
            (4, "Group 推しの子", 11),
            (5, "Ghost in the Shell 2.0", 1),
            (6, "Group Ghost in the Shell 2.0", 1),
            (7, "86", 1),
            (8, "Group 86", 1),
            (9, "【映画】", 1),
            (10, "Group 映画", 1),
            (11, "【映画】 Another Movie", 1),
        ] {
            db.upsert_media(&crate::models::Media {
                id,
                title_english: Some(title.into()),
                episodes: Some(episodes),
                ..Default::default()
            })
            .unwrap();
            db.upsert_entry(&crate::models::ListEntry {
                media_id: id,
                status: "CURRENT".into(),
                ..Default::default()
            })
            .unwrap();
        }
        let matchers = build_matchers(&db);
        for (filename, id, episode) in [
            ("【Group】 【推しの子】 - 03.mkv", 3, 3),
            ("【Group】 Some Movie.mkv", 1, 1),
            ("[Group] Some Movie.mkv", 1, 1),
            ("【Group】 推しの子 - 03.mkv", 3, 3),
            ("【推しの子】 - 03.mkv", 3, 3),
            ("【Group】 Ghost in the Shell 2.0.mkv", 5, 1),
            ("【Group】 86.mkv", 7, 1),
            ("Group Some Movie.mkv", 2, 1),
            ("【Group】 【映画】.mkv", 9, 1),
            ("【映画】.mkv", 9, 1),
            ("【映画】 Another Movie.mkv", 11, 1),
            ("【Other】【Group】 Some Movie.mkv", 1, 1),
        ] {
            let path = format!("file:///movies/{filename}")
                .replace('【', "%E3%80%90")
                .replace('】', "%E3%80%91")
                .replace(' ', "%20");
            for title in ["", filename] {
                let matched = match_title(&matchers, title, &path).expect(filename);
                assert_eq!(matched.media_id, id, "{filename}");
                assert_eq!(
                    crate::library::resolve_bound_episode(matched, &[filename], None),
                    Some(episode),
                    "{filename}"
                );
            }
        }
        for filename in [
            "【Group】 Some Movie Trailer.mkv",
            "【Group】 Some Movie - 02.mkv",
            "【Group】 Some Movie - 00.mkv",
            "【Group】 Some Movie - 01.5.mkv",
            "【Group】 Some Movie - 01-02.mkv",
            "【Group】 【推しの子】 - 12.mkv",
        ] {
            let matched = match_title(&matchers, filename, "").expect(filename);
            assert_eq!(
                crate::library::resolve_bound_episode(matched, &[filename], None),
                None,
                "{filename}"
            );
        }
    }
    #[test]
    fn corner_catalog_competition_on_playback_and_scan_paths() {
        let mut failures = Vec::new();
        for (catalog, filename, expected, episode) in [
            (
                vec!["Some Movie", "Group Some Movie"],
                "[Other]【Group】 Some Movie.mkv",
                1,
                1,
            ),
            (
                vec!["Some Movie", "Group Some Movie"],
                "[Other](More)【Group】 Some Movie.mkv",
                1,
                1,
            ),
            (
                vec!["Some Movie", "Group Some Movie"],
                "【Other】[More]【Group】 Some Movie.mkv",
                1,
                1,
            ),
            (
                vec!["Some Movie", "Group Some Movie", "【Group】 Another Movie"],
                "【Group】 Some Movie.mkv",
                1,
                1,
            ),
            (
                vec!["Another Movie", "【映画】 Another Movie"],
                "【映画】 Another Movie - 01.mkv",
                2,
                1,
            ),
            (
                vec!["【Some Show】", "Other Show"],
                "【some show】 - 03.mkv",
                1,
                3,
            ),
            (
                vec!["Ghost in the Shell", "【Ghost in the Shell 2.0】"],
                "【Ghost in the Shell 2.0】 - 01.mkv",
                2,
                1,
            ),
        ] {
            let db = Db::open(std::path::Path::new(":memory:")).unwrap();
            for (index, title) in catalog.iter().enumerate() {
                let id = index as i64 + 1;
                db.upsert_media(&crate::models::Media {
                    id,
                    title_english: Some((*title).into()),
                    episodes: Some(if episode == 3 { 12 } else { 1 }),
                    ..Default::default()
                })
                .unwrap();
                db.upsert_entry(&crate::models::ListEntry {
                    media_id: id,
                    status: if id == 1 { "CURRENT" } else { "COMPLETED" }.into(),
                    ..Default::default()
                })
                .unwrap();
            }
            let matchers = Arc::new(build_matchers(&db));
            let path = format!("file:///movies/{filename}")
                .replace('【', "%E3%80%90")
                .replace('】', "%E3%80%91")
                .replace(' ', "%20");
            for title in ["", filename] {
                let matched = match_title(&matchers, title, &path);
                if matched.map(|m| m.media_id) != Some(expected) {
                    failures.push(format!(
                        "{filename}: got {:?}, expected {expected}",
                        matched.map(|m| m.media_id)
                    ));
                    continue;
                }
                let matched = matched.unwrap();
                assert_eq!(
                    resolve_playback_episode(matched, &[filename], false),
                    Some(episode),
                    "{filename}"
                );
                assert_eq!(
                    crate::library::resolve_bound_episode(matched, &[filename], None),
                    Some(episode),
                    "{filename}"
                );
            }
            let folder = std::env::temp_dir().join(format!(
                "kurisu-corner-review-{}-{expected}-{episode}",
                std::process::id()
            ));
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(filename), []).unwrap();
            let mut cache = crate::library::ScanCache::default();
            for _ in 0..2 {
                let scan = cache.scan(
                    &[folder.to_string_lossy().into_owned()],
                    &matchers,
                    &std::collections::HashMap::new(),
                );
                assert_eq!(scan.files.len(), 1);
                assert_eq!(scan.files[0].media_id, Some(expected), "{filename}");
                assert_eq!(scan.files[0].episode, Some(episode), "{filename}");
            }
            std::fs::remove_dir_all(folder).unwrap();
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn bracketed_and_dot_scene_titles_parse_episodes_and_seasons() {
        let show = Matcher {
            episodes: Some(12),
            ..mk(1, "Show")
        };
        assert_eq!(
            resolve_episode(&show, &["[Group] Show - 03 - 1-2-3 Go! [1080p].mkv"]),
            Some(3)
        );
        assert_eq!(
            resolve_episode(&show, &["[Group] Show - 03 - 12.5 Seconds [1080p].mkv"]),
            Some(3)
        );
        let matchers = vec![
            Matcher {
                episodes: Some(12),
                ..mk(1, "Some Show")
            },
            Matcher {
                episodes: Some(12),
                ..mk(2, "Some Show Season 2")
            },
        ];
        for file in [
            "Some.Show.Season.2.E04.1080p.mkv",
            "Some_Show_2nd_Season_04.mkv",
        ] {
            let matched = match_title(&matchers, file, "").unwrap();
            assert_eq!(matched.media_id, 2, "{file}");
            assert_eq!(resolve_episode(matched, &[file]), Some(4), "{file}");
        }
        for file in [
            "[Group] Show [BD Season 2] - 04 [1080p].mkv",
            "[Group] Show (2024 Season 2) - E04 [1080p].mkv",
            "[Group] Show [Season 2 BD] - 04 (2).mkv",
        ] {
            assert_eq!(parse_episode_guess(file), Some(4), "{file}");
            assert_eq!(
                resolve_playback_episode(&show, &[file], false),
                Some(4),
                "{file}"
            );
        }
        for file in [
            "K_2nd_Season_04.mkv",
            "K.2nd.Season.E04.mkv",
            "K.Season.2.mkv",
        ] {
            assert_eq!(clean_title(file), "k 2", "{file}");
        }
    }

    #[test]
    fn conflicting_seasons_after_multibyte_bracketed_episode_are_rejected() {
        let show = Matcher {
            episodes: Some(12),
            ..mk(1, "Show")
        };
        for file in [
            "Show 【\u{3000}Season 2 E01】 Season 3 E01.mkv",
            "Show 【第2期 Season 2 E01】 Season 3 E01.mkv",
        ] {
            assert_eq!(resolve_episode(&show, &[file]), None, "{file}");
            assert!(excluded_playback_release(&show, file), "{file}");
        }
        for file in [
            "Show 【\u{3000}Season 2 E01】 Season 2 E01.mkv",
            "Show 【\u{3000}Season 2 E01】 E01.mkv",
        ] {
            assert_eq!(resolve_episode(&show, &[file]), Some(1), "{file}");
            assert!(!excluded_playback_release(&show, file), "{file}");
        }
    }
}
