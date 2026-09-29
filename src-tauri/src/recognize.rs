use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::db::Db;

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(【][^\]\)】]*[\]\)】]").unwrap());
static RE_RES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(1080|720|480|360|2160|1440|4320)p?\b|\b\d{3,4}p\d{2,3}\b|\b[48]k\b|\b(bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc10|hevc|x265|x264|h[\s.]*26[456]|avc|av1|vp9|vp0|vvc|xvid|divx|mpeg-?2|aac|eac3|ddp?\d|opus|flac|10bit|hi10|yuv420)\b").unwrap()
});
// Require a separator before E05 to preserve Steins;Gate. Keep episode zero for specials.
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·]?\s*(?:[sS]\d{1,2}[eE]|ep(?:isode)?\.?|[-_·\s][eE]|#)?\s*0*[1-9]\d{0,3}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
static RE_EP_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:v\d+)?").unwrap());
static RE_NTH_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?P<n>\d{1,2})(?:st|nd|rd|th) season\b").unwrap());
static RE_SEASON_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bseason (?P<n>\d{1,2})\b").unwrap());
static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d\.\d\b").unwrap());
static RE_SEASON_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS](\d{1,2})\s*[eE]\d{1,3}").unwrap());
static RE_SEASON_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS]\d{1,2}\s*([eE])").unwrap());
static RE_SEASON_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS]\d{1,2}\b").unwrap());
static RE_SEASON_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})\b").unwrap());
static RE_REV_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\s+v\d+\s*$").unwrap());
static RE_SEASON_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:season[\s._-]+\d{1,2}|\d{1,2}(?:st|nd|rd|th)[\s._-]+season)\b").unwrap()
});
static RE_EXPLICIT_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\bs\d{1,2}\s*e|\bep(?:isode)?[.\s_-]*|\be[.\s_-]*|#)\s*(\d{1,4})(?:v\d+)?(?:$|[^\w])").unwrap()
});
static RE_LEADING_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[\s._:·-]*(\d{1,4})(?:v\d+)?(?:$|[\s._-])").unwrap());
static RE_BRACKET_EP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[\[(【]\s*(?:(?:ep(?:isode)?\.?|e)\s*)?(\d{1,4})(?:v\d+)?\s*[\])】]").unwrap()
});
static RE_EP_RANGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^\w.])(?P<marker>s[0-9]{1,2}e|ep(?:isode)?[.\s_-]*|e|#)?(?P<start>[0-9]{1,4})(?:v[0-9]+)?\s*[-~–]\s*(?:s[0-9]{1,2}e|ep(?:isode)?[.\s_-]*|e|#)?(?P<end>[0-9]{1,4})(?:v[0-9]+)?(?:$|[^\w.])").unwrap()
});

const NOISE_NUMBERS: [i64; 7] = [360, 480, 720, 1080, 1440, 2160, 4320];

const MAX_SEASON: u32 = 50;

pub(crate) struct Matcher {
    pub media_id: i64,
    pub display: String,
    pub variants: Vec<String>,
    norms: Vec<String>,
    status_rank: u8,
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
    let entries = match db.entries_with_media() {
        Ok(v) => v,
        Err(e) => {
            log::warn!("matcher build could not read the list, matching is off until the next list mutation: {e}");
            Vec::new()
        }
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(m) = e.media else { continue };
        let mut variants = Vec::new();
        let mut norms = Vec::new();
        for v in [
            m.title_english.as_deref(),
            m.title_romaji.as_deref(),
            m.title_native.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !v.trim().is_empty() {
                // List title numbers belong to the name. Only releases lose episode suffixes.
                let n = norm_title(v);
                if !n.is_empty() {
                    variants.push(v.to_string());
                    norms.push(n);
                }
            }
        }
        if norms.is_empty() {
            continue;
        }
        out.push(Matcher {
            media_id: m.id,
            display: m.display_title(),
            variants,
            norms,
            status_rank: status_rank(&e.status),
        });
    }
    out.sort_by_key(|m| (m.status_rank, m.media_id));
    out
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

fn parse_season_marker(raw: &str) -> Option<u32> {
    RE_SEASON_EP
        .captures(raw)
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|&n| (1..=MAX_SEASON).contains(&n))
}

fn parse_season_word(raw: &str) -> Option<u32> {
    let lower = raw.to_lowercase();
    let n = RE_SEASON_N
        .captures(&lower)
        .or_else(|| RE_NTH_SEASON.captures(&lower))?
        .name("n")?;
    n.as_str()
        .parse::<u32>()
        .ok()
        .filter(|&v| (1..=MAX_SEASON).contains(&v))
}

fn parse_season_token(raw: &str) -> Option<u32> {
    RE_SEASON_TOKEN
        .captures(raw)
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|&n| (1..=MAX_SEASON).contains(&n))
}

fn norm_season_ordinal(norm: &str) -> Option<u32> {
    let words: Vec<&str> = norm.split_whitespace().collect();
    for w in &words {
        if let Some(n) = roman_to_u32(w) {
            return Some(n);
        }
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
        total = if v < prev { total - v } else { total + v };
        prev = v;
    }
    (1..=30).contains(&total).then_some(total)
}

type TiebreakKey = (
    bool,
    u8,
    std::cmp::Reverse<u8>,
    usize,
    std::cmp::Reverse<i64>,
);

/// Seasons must agree. Unnumbered releases cannot imply a numbered sequel.
pub(crate) fn match_title<'a>(
    matchers: &'a [Matcher],
    title: &str,
    url: &str,
) -> Option<&'a Matcher> {
    let base = basename(url);
    let candidates = [
        (clean_title(title), None, false),
        (clean_title(&base), None, false),
        (norm_title(title), Some(title), true),
        (norm_title(&base), Some(base.as_str()), true),
        (clean_title_inner(title, false), None, true),
        (clean_title_inner(&base, false), None, true),
    ];
    let cand_season = parse_season_marker(title).or_else(|| parse_season_marker(&basename(url)));
    let mut best: Option<(TiebreakKey, &Matcher)> = None;
    let season_ord = cand_season
        .or_else(|| parse_season_word(title))
        .or_else(|| parse_season_word(&basename(url)))
        .or_else(|| parse_season_token(title))
        .or_else(|| parse_season_token(&basename(url)));
    for (cand, raw_title, exact_only) in candidates {
        if cand.is_empty() {
            continue;
        }
        let cand_ord = season_ord.or_else(|| norm_season_ordinal(&cand));
        for m in matchers {
            if exact_only && !m.norms.iter().any(|n| n == &cand) {
                continue;
            }
            if let Some(raw) = raw_title {
                let text = strip_ext(raw);
                let marked_episode = RE_EP_TAIL.find(&text).is_some_and(|tail| {
                    tail.as_str().trim_start().starts_with(['-', '_', '·'])
                        || RE_EXPLICIT_EP.is_match(tail.as_str())
                });
                if marked_episode
                    && !m
                        .variants
                        .iter()
                        .any(|v| raw.to_lowercase().contains(&v.to_lowercase()))
                {
                    continue;
                }
            }
            let expected_season = cand_ord.unwrap_or(1);
            if !m.norms.iter().any(|x| {
                let ord = norm_season_ordinal(x);
                ord == Some(expected_season) || (expected_season == 1 && ord.is_none())
            }) {
                continue;
            }
            if let Some((tier, nlen)) = m
                .norms
                .iter()
                .filter_map(|n| {
                    if exact_only {
                        (n == &cand).then_some((4, n.len()))
                    } else {
                        norm_match_tier(n, &cand).map(|t| (t, n.len()))
                    }
                })
                .max()
            {
                let season_match = cand_season
                    .is_some_and(|cs| m.norms.iter().any(|n| norm_season_ordinal(n) == Some(cs)));
                let key = (
                    season_match,
                    tier,
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

pub(crate) fn clean_title(s: &str) -> String {
    clean_title_inner(s, true)
}

fn clean_title_inner(s: &str, strip_channels: bool) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    // Strip channel layouts before the episode tail so AAC2.0 cannot leave a false episode zero.
    let s = if strip_channels {
        RE_CHANNEL.replace_all(&s, " ")
    } else {
        s
    };
    let s = RE_RES.replace_all(&s, " ");
    let release =
        s.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-' | '·'));
    let stripped = strip_episode_tail(release);
    // Preserve numeric titles when stripping would leave nothing.
    let normed = normalize(&stripped);
    let out = if normed.is_empty() {
        normalize(&s)
    } else {
        normed
    };
    season_ordinals(&out)
}

/// A number after season is an ordinal. Check outside the regex, which lacks lookbehind.
fn strip_episode_tail(s: &str) -> Cow<'_, str> {
    match RE_EP_TAIL.find(s) {
        Some(m) if preceded_by_season(s, m.start()) => Cow::Borrowed(s),
        _ => RE_EP_TAIL.replace(s, ""),
    }
}

fn preceded_by_season(s: &str, start: usize) -> bool {
    s[..start]
        .split_whitespace()
        .next_back()
        .is_some_and(|w| w.eq_ignore_ascii_case("season"))
}

pub(crate) fn norm_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let res_stripped = RE_RES.replace_all(&s, " ");
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
    // Collapse S02E03 first, before the bare S02 rule.
    let out = RE_SEASON_EP.replace_all(normed, |c: &regex::Captures| {
        canon_ordinal(c.get(1).map_or("", |m| m.as_str())).to_string()
    });
    let out = RE_NTH_SEASON.replace_all(&out, |c: &regex::Captures| {
        canon_ordinal(c.name("n").map_or("", |m| m.as_str())).to_string()
    });
    let out = RE_SEASON_N.replace_all(&out, |c: &regex::Captures| {
        canon_ordinal(c.name("n").map_or("", |m| m.as_str())).to_string()
    });
    RE_SEASON_TOKEN
        .replace_all(&out, |c: &regex::Captures| {
            canon_ordinal(c.get(1).map_or("", |m| m.as_str())).to_string()
        })
        .into_owned()
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

/// Outer None permits guessing. Inner None means the title matched but no episode followed.
pub(crate) fn parse_episode_after(playing: &str, variants: &[String]) -> Option<Option<i64>> {
    let lp = playing.to_lowercase();
    for v in variants {
        let lv = v.to_lowercase();
        if lv.is_empty() {
            continue;
        }
        if lp.contains(&lv) {
            let remainder = lp.replace(&lv, " ");
            return Some(parse_episode_number(&remainder, true));
        }
    }
    None
}

pub(crate) fn parse_episode_guess(s: &str) -> Option<i64> {
    parse_last_episode_number(s)
}

/// Guess only if no raw title variant matched. A title without an episode may be a batch.
pub(crate) fn resolve_episode(matched: &Matcher, candidates: &[&str]) -> Option<i64> {
    let mut variant_hit = false;
    for cand in candidates {
        match parse_episode_after(cand, &matched.variants) {
            Some(Some(n)) => return Some(n),
            Some(None) => variant_hit = true,
            None => {
                if matched.norms.contains(&norm_title(cand)) {
                    if RE_SEASON_EP.is_match(cand) {
                        if let Some(episode) = parse_episode_guess(cand) {
                            return Some(episode);
                        }
                    }
                    if let Some(episode) = bracket_episode(cand) {
                        return Some(episode);
                    }
                    variant_hit = true;
                }
            }
        }
    }
    if variant_hit {
        None
    } else {
        candidates.iter().find_map(|c| parse_episode_guess(c))
    }
}

fn looks_like_year(n: i64) -> bool {
    use chrono::Datelike;
    (1900..=chrono::Utc::now().year() as i64 + 1).contains(&n)
}

fn bracket_episode(s: &str) -> Option<i64> {
    RE_BRACKET_EP
        .captures_iter(s)
        .filter_map(|c| c.get(1)?.as_str().parse::<i64>().ok())
        .find(|n| !NOISE_NUMBERS.contains(n) && !looks_like_year(*n))
}

fn parse_last_episode_number(s: &str) -> Option<i64> {
    parse_episode_number(s, false)
}

fn parse_episode_number(s: &str, title_removed: bool) -> Option<i64> {
    let s = strip_ext(s);
    let s = RE_SEASON_WORDS.replace_all(&s, " ");
    let explicit = RE_EXPLICIT_EP.captures(&s);
    let leading = title_removed.then(|| RE_LEADING_EP.captures(&s)).flatten();
    // A pack has no single episode to track or offer as the next file.
    if RE_EP_RANGE.captures_iter(&s).any(|c| {
        let start = c.name("start").unwrap();
        let first = start.as_str().parse::<i64>().unwrap();
        let last = c.name("end").unwrap().as_str().parse::<i64>().unwrap();
        let episode = explicit.as_ref().or(leading.as_ref()).and_then(|c| c.get(1));
        episode.is_none_or(|episode| episode.start() == start.start())
            && (title_removed || c.name("marker").is_some() || start.len() >= 2)
            && first < last
            && !looks_like_year(first)
            && !looks_like_year(last)
    }) {
        return None;
    }
    let bracketed = bracket_episode(&s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_CHANNEL.replace_all(&s, " ");
    if let Some(episode) = RE_EXPLICIT_EP
        .captures(&s)
        .and_then(|c| c.get(1)?.as_str().parse::<i64>().ok())
    {
        return Some(episode);
    }
    let s = RE_SEASON_PREFIX.replace_all(&s, "$1");
    let s = RE_SEASON_BARE.replace_all(&s, " ");
    let s = RE_REV_TAIL.replace(&s, " ");
    if title_removed {
        if let Some(episode) = RE_LEADING_EP
            .captures(&s)
            .and_then(|c| c.get(1)?.as_str().parse::<i64>().ok())
            .filter(|n| !looks_like_year(*n))
        {
            return Some(episode);
        }
    }
    if bracketed.is_some() {
        return bracketed;
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

    #[test]
    fn episode_after_title_removal_avoids_title_numbers() {
        let variants = vec!["91 Days".to_string()];
        assert_eq!(
            parse_episode_after("[Group] 91 Days - 05 [1080p]", &variants),
            Some(Some(5))
        );
        assert_eq!(
            parse_episode_after("91 Days [BD 1080p]", &variants),
            Some(None)
        );
        assert_eq!(parse_episode_after("Something Else - 03", &variants), None);
    }

    #[test]
    fn resolve_episode_batch_alias_and_crc() {
        let days = Matcher {
            media_id: 1,
            display: "91 Days".into(),
            variants: vec!["91 Days".into()],
            norms: vec!["91 days".into()],
            status_rank: 0,
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
            status_rank: 0,
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
            status_rank: 0,
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
            status_rank: 0,
        }
    }

    fn mk_status(media_id: i64, title: &str, status: &str) -> Matcher {
        Matcher {
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
            match_title(&matchers, "Boku no Hero Academia S02E05.mkv", "")
                .map(|m| m.media_id),
            Some(2)
        );
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
                status_rank: status_rank(status),
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
                status_rank: status_rank(status),
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
                status_rank: status_rank("CURRENT"),
            },
            Matcher {
                media_id: 178789,
                display: "Mushoku Tensei: Jobless Reincarnation Season 3".into(),
                variants: vec!["Mushoku Tensei: Jobless Reincarnation Season 3".into()],
                norms: vec![norm_title("Mushoku Tensei: Jobless Reincarnation Season 3")],
                status_rank: status_rank("PAUSED"),
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
        let variants = vec!["Some Show".to_string()];
        assert_eq!(
            parse_episode_after("Some Show S02 [1080p]", &variants),
            Some(None)
        );
        assert_eq!(
            parse_episode_after("Some Show S02E05 [1080p]", &variants),
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
}
