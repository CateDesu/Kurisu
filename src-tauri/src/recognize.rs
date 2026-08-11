//! Filename and now-playing title recognition. Core of the M3 library scanner.
//! Used by the MPRIS watcher in playback.rs and the library scanner in
//! library.rs. Both clean a raw title or filename, match it against the cached
//! list, and pull an episode number from what is left.

use std::sync::LazyLock;

use regex::Regex;

use crate::db::Db;

// ─────────────────────────── regex ───────────────────────────

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(【][^\]\)】]*[\]\)】]").unwrap());
static RE_RES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(1080|720|480|360|2160|1440|4320)p?\b|\b(bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc10|hevc|x265|x264|h[\s.]*26[456]|avc|av1|vp9|vp0|vvc|xvid|divx|mpeg-?2|aac|eac3|ddp?\d|opus|flac|10bit|hi10|yuv420)\b").unwrap()
});
// Trailing episode marker. Bare E05 needs a separator or season prefix before
// the e, else the title's own final e gets eaten. Steins;Gate 01 would become
// "steins gat". d{1,4} covers 1000+ episode runs.
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·]?\s*(?:[sS]\d{1,2}[eE]|ep(?:isode)?\.?|[-_·\s][eE]|#)?\s*0*\d{1,4}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
/// Episode number candidate. Optional vN revision suffix. 04v2 is episode 4,
/// not episodes 4 and 2.
static RE_EP_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:v\d+)?").unwrap());
/// Season markers on a normalized string. Collapses to the bare ordinal AniList
/// uses in romaji sequel titles. 7th season becomes 7.
static RE_NTH_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?P<n>\d{1,2})(?:st|nd|rd|th) season\b").unwrap());
/// season 7 becomes 7. Also covers English titles AniList stores.
static RE_SEASON_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bseason (?P<n>\d{1,2})\b").unwrap());
/// Audio channel layouts like 2.0 or 5.1. Stripped before codec names so the
/// digits don't outrank the episode number. No leading b since the layout is
/// usually glued to its codec like AAC2.0. Trailing b keeps dot-separated tags
/// like S01E01.1080p intact.
static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d\.\d\b").unwrap());
/// Season-episode marker in a raw release name. Used to extract the season
/// ordinal before clean_title strips the marker entirely.
static RE_SEASON_EP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS](\d{1,2})\s*[eE]\d{1,3}").unwrap());
/// Season prefix before an episode marker. S02E becomes E. Used in
/// parse_last_episode_number so season digits don't survive as a candidate
/// episode number.
static RE_SEASON_PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS]\d{1,2}\s*([eE])").unwrap());
/// Bare season marker without a following episode. Stripped from the
/// episode-parser remainder so Show S02 doesn't parse as episode 2. Applied
/// after RE_SEASON_PREFIX so S02E05 is safe.
static RE_SEASON_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[sS]\d{1,2}\b").unwrap());

/// Resolutions to discard when picking the episode number.
const NOISE_NUMBERS: [i64; 7] = [360, 480, 720, 1080, 1440, 2160, 4320];

// ─────────────────────────── matchers ───────────────────────────

pub(crate) struct Matcher {
    pub media_id: i64,
    pub display: String,
    pub variants: Vec<String>, // raw english, romaji, native titles
    norms: Vec<String>,        // normalized variants for comparison
    /// Rank of the entry's list status. Used to break ties. Lower wins.
    status_rank: u8,
}

/// Two list entries can normalize to the same title. Prefer what the user is
/// watching, then fall back to the lowest media_id so the choice is stable.
fn status_rank(status: &str) -> u8 {
    match status {
        "CURRENT" => 0,
        "REPEATING" => 1,
        "PAUSED" => 2,
        "PLANNING" => 3,
        "COMPLETED" => 4,
        _ => 5, // DROPPED and future statuses
    }
}

pub(crate) fn build_matchers(db: &Db) -> Vec<Matcher> {
    let entries = db.entries_with_media().unwrap_or_default();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(m) = e.media else { continue };
        let mut variants = Vec::new();
        let mut norms = Vec::new();
        for v in [m.title_english.as_deref(), m.title_romaji.as_deref(), m.title_native.as_deref()].into_iter().flatten() {
            if !v.trim().is_empty() {
                // norm_title, not clean_title. The episode-tail strip is for
                // release names. On list titles it ate numeric suffixes.
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
    // Stable order so the tie-break below is deterministic regardless of row
    // order from SQLite.
    out.sort_by_key(|m| (m.status_rank, m.media_id));
    out
}

/// Does `long` contain `short` as whole tokens? Both sides are normalized so a
/// boundary check is just a space check. Some(true) means at the start, Some(false)
/// means later in the string. Raw char containment isn't enough since "dr" lives
/// inside "dreaming".
fn contains_tokens(long: &str, short: &str) -> Option<bool> {
    if short.is_empty() || short.len() > long.len() {
        return None;
    }
    if long.starts_with(short)
        && (long.len() == short.len() || long.as_bytes()[short.len()] == b' ')
    {
        return Some(true);
    }
    // Interior or suffix occurrence. Both edges must be on token boundaries.
    // Searching for short directly and checking the byte before it avoids
    // allocating a padded copy. This is the innermost loop, thousands of
    // allocations per scanned file otherwise.
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
        // Advance one character, not one byte. Norms keep unicode. Slicing mid
        // codepoint would panic.
        from = start + long[start..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

/// Match quality of one norm against one candidate. None is no match.
/// 3 is exact. 2 is one is a whole-token prefix of the other. 1 is a long
/// multi-word norm phrase sitting mid-string, like a secondary title.
/// Short strings only match exactly. Single common words and tiny norms appear
/// inside unrelated titles too often to trust.
fn norm_match_tier(norm: &str, cand: &str) -> Option<u8> {
    if norm == cand {
        return Some(3);
    }
    let (short, long) = if norm.len() <= cand.len() { (norm, cand) } else { (cand, norm) };
    if short.len() < 4 {
        return None;
    }
    match contains_tokens(long, short) {
        Some(true) => Some(2),
        Some(false) if short.len() >= 8 && short.contains(' ') => Some(1),
        _ => None,
    }
}

/// Extract the season ordinal from a raw release title or URL. Returns None
/// when no S\d{1,2}E\d marker is present. Only trust the explicit pattern, not
/// bare numbers, to avoid false confidence on movies and non-seasonal releases.
fn parse_season_marker(raw: &str) -> Option<u32> {
    RE_SEASON_EP
        .captures(raw)
        .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .filter(|&n| (1..=50).contains(&n))
}

/// Extract the season ordinal from a normalized list-title norm. After
/// season_ordinals collapses, the bare number is already there. Also check for
/// roman-numeral sequels. Returns None when the title has no season marker.
fn norm_season_ordinal(norm: &str) -> Option<u32> {
    let words: Vec<&str> = norm.split_whitespace().collect();
    // Roman-numeral sequel markers in AniList romaji titles.
    for w in &words {
        if let Some(n) = roman_to_u32(w) {
            return Some(n);
        }
    }
    // Bare trailing number from the season_ordinals collapse. Only the last
    // token. A number mid-title is part of the name, not a season ordinal.
    words.last().and_then(|w| {
        if w.len() <= 2 {
            w.parse::<u32>().ok().filter(|&n| (1..=30).contains(&n))
        } else {
            None
        }
    })
}

/// Parse a lowercase roman numeral to its integer value. Requires at least 2
/// characters so single letters like i, v, x common in anime titles don't
/// false-positive into season ordinals.
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

/// Match a now-playing string against the cached list. Tries the raw title first,
/// then the file basename. Scores all candidates globally so a weak hit on the
/// player title doesn't hide a stronger match on the filename.
///
/// Tiebreak order: higher tier wins first, then a season-ordinal match wins,
/// then status, then longer norm, then lower media_id.
pub(crate) fn match_title<'a>(matchers: &'a [Matcher], title: &str, url: &str) -> Option<&'a Matcher> {
    let candidates = [clean_title(title), clean_title(&basename(url))];
    // Season ordinal from the raw inputs before clean_title strips the marker.
    let cand_season = parse_season_marker(title).or_else(|| parse_season_marker(&basename(url)));
    let mut best: Option<((bool, u8, std::cmp::Reverse<u8>, usize, std::cmp::Reverse<i64>), &Matcher)> =
        None;
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        for m in matchers {
            if let Some((tier, nlen)) = m
                .norms
                .iter()
                .filter_map(|n| norm_match_tier(n, &cand).map(|t| (t, n.len())))
                .max()
            {
                // Season match. The release says S03E05 and at least one of
                // this entry's norms encodes the same ordinal. Strongest
                // franchise disambiguator. Picks the right season even when
                // every sibling shares a prefix at the same tier and status.
                let season_match = cand_season.map_or(false, |cs| {
                    m.norms.iter().any(|n| norm_season_ordinal(n) == Some(cs))
                });
                let key = (
                    season_match,
                    tier,
                    std::cmp::Reverse(m.status_rank),
                    nlen,
                    std::cmp::Reverse(m.media_id),
                );
                if best.as_ref().map_or(true, |(bk, _)| key > *bk) {
                    best = Some((key, m));
                }
            }
        }
    }
    best.map(|(_, m)| m)
}

// ─────────────────────────── parsing ───────────────────────────

/// Normalize a release name or now-playing string for comparison. Lowercase,
/// split on non-alphanumeric, drop bracket groups and resolution noise, strip
/// the trailing episode marker.
pub(crate) fn clean_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    let stripped = RE_EP_TAIL.replace(&s, "");
    // The tail strip must never eat the whole title. A bare-number show is all
    // episode-tail to the regex. Keep the unstripped form when stripping leaves
    // nothing to match on.
    let normed = normalize(&stripped);
    let out = if normed.is_empty() { normalize(&s) } else { normed };
    season_ordinals(&out)
}

/// Normalize a list title for comparison. Same as clean_title without the
/// episode-tail strip. A trailing number in a list title is part of the name,
/// not an episode.
pub(crate) fn norm_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let res_stripped = RE_RES.replace_all(&s, " ");
    // Don't let codec-name stripping empty a real title. The film Opus would
    // lose its only word to the opus codec alias.
    let s = if normalize(&res_stripped).is_empty() { s } else { res_stripped };
    season_ordinals(&normalize(&s))
}

/// Collapse season markers to a bare ordinal so both sides of a comparison agree.
/// Without this a MAL-style release lost its sequel entry to the base series
/// because the sequel norm failed the token check while the base matched as a
/// prefix. Applied to list titles and release names.
fn season_ordinals(normed: &str) -> String {
    let out = RE_NTH_SEASON.replace_all(normed, "$n");
    RE_SEASON_N.replace_all(&out, "$n").into_owned()
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

/// File extensions worth stripping from a title or basename. Only these. The
/// old naive last-dot strip mutilated titles like No.6 and D.Gray-man, and
/// truncated torrent names at codec tags.
const STRIP_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv", "mpg", "mpeg", "m2ts",
    "ogv",
];

/// Strip a trailing known media extension, if any.
fn strip_ext(s: &str) -> String {
    if let Some(i) = s.rfind('.') {
        let ext = &s[i + 1..];
        if STRIP_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

/// Last path segment of a file URL or any path-ish string. Extension stripped
/// and percent-decoded.
pub(crate) fn basename(url: &str) -> String {
    let seg = url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(url);
    let seg = strip_ext(seg);
    percent_decode(&seg)
}

/// Minimal percent-decoding for things like %20. Decoded bytes are accumulated
/// and interpreted as UTF-8 so multi-byte sequences survive instead of becoming
/// garbage chars.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Both digits must be ASCII hex. Old code passed non-UTF-8 pairs through
        // unwrap_or which parsed as 0. A literal percent before a multi-byte
        // character emitted a NUL byte and swallowed the next two bytes.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            if let Ok(b) = u8::from_str_radix(
                // Both bytes are ASCII hex, so the slice is always valid UTF-8.
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

/// Once we know which media it is, parse the episode from the remainder after
/// the matched title variant is removed. Avoids misreading a number in the title
/// itself like 91 Days.
///
/// Tri-state. Some(Some(n)) means a variant matched and the remainder yielded
/// episode n. Some(None) means a variant matched but there is no episode, likely
/// a batch file. Callers must not guess or the title's own number comes back as
/// the episode. None means no variant in the string at all, the normalized match
/// used an alias. Guessing is fine.
pub(crate) fn parse_episode_after(playing: &str, variants: &[String]) -> Option<Option<i64>> {
    let lp = playing.to_lowercase();
    for v in variants {
        let lv = v.to_lowercase();
        if lv.is_empty() {
            continue;
        }
        if lp.contains(&lv) {
            let remainder = lp.replace(&lv, " ");
            return Some(parse_last_episode_number(&remainder));
        }
    }
    None
}

/// Fallback. Pick the last plausible episode number from a raw string. Only for
/// strings where no title variant matched.
pub(crate) fn parse_episode_guess(s: &str) -> Option<i64> {
    parse_last_episode_number(s)
}

/// Resolve the episode for a matched title from candidate strings. A matched
/// variant with digits wins. A matched variant with no digits in any candidate
/// is a batch file. No raw variant anywhere means the normalized match used an
/// alias, so guess.
pub(crate) fn resolve_episode(matched: &Matcher, candidates: &[&str]) -> Option<i64> {
    let mut variant_hit = false;
    for cand in candidates {
        match parse_episode_after(cand, &matched.variants) {
            Some(Some(n)) => return Some(n),
            Some(None) => variant_hit = true,
            None => {}
        }
    }
    if variant_hit {
        None
    } else {
        candidates.iter().find_map(|c| parse_episode_guess(c))
    }
}

/// Years read as year not episode. 1900 through next year. Upper bound tracks
/// the current year. Lower bound covers the few pre-1930 shorts AniList lists.
fn looks_like_year(n: i64) -> bool {
    use chrono::Datelike;
    (1900..=chrono::Utc::now().year() as i64 + 1).contains(&n)
}

/// Last integer that looks like an episode. Bracketed groups, audio channel
/// layouts, and resolution and codec noise are stripped first or their digits
/// would beat the real episode number. Excludes resolutions, 4-digit years,
/// and anything outside 1 to 9999. A vN revision suffix belongs to the number
/// it follows.
fn parse_last_episode_number(s: &str) -> Option<i64> {
    let s = RE_BRACKETS.replace_all(s, " ");
    let s = RE_CHANNEL.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    // Strip season markers so a season pack Show S02 doesn't parse its season
    // number as episode 2. Two-step. First peel the season prefix off S02E05,
    // then strip any remaining bare S02.
    let s = RE_SEASON_PREFIX.replace_all(&s, "$1");
    let s = RE_SEASON_BARE.replace_all(&s, " ");
    RE_EP_NUM
        .find_iter(&s)
        .filter_map(|m| m.as_str().split('v').next()?.parse::<i64>().ok())
        .filter(|n| !NOISE_NUMBERS.contains(n) && !looks_like_year(*n))
        .filter(|n| *n >= 1 && *n <= 9999)
        .last()
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

    /// No separator before the episode must not eat the title's final e.
    /// 1000+ episode runs must not leave a stray digit behind.
    #[test]
    fn clean_keeps_title_final_e_and_4_digit_episodes() {
        assert_eq!(clean_title("Steins;Gate 01"), "steins gate");
        assert_eq!(clean_title("Fate 01"), "fate");
        // dash-separated form already worked
        assert_eq!(clean_title("Steins;Gate - 01"), "steins gate");
        assert_eq!(clean_title("One Piece 1015"), "one piece");
        // E05 and S02E05 marker forms still strip, season prefix included
        assert_eq!(clean_title("Show E05"), "show");
        assert_eq!(clean_title("Show - E05"), "show");
        assert_eq!(clean_title("Show S02E05"), "show");
    }

    #[test]
    fn basename_decodes_and_strips() {
        assert_eq!(basename("file:///media/anime/My%20Show%20-%2003.mkv"), "My Show - 03");
        // multi-byte UTF-8 survives decoding
        assert_eq!(basename("file:///x/%E3%82%AF%E3%83%AA%E3%82%B9.mkv"), "クリス");
    }

    #[test]
    fn episode_guess_ignores_resolutions_and_years() {
        assert_eq!(parse_episode_guess("Show - 07 [1080p]"), Some(7));
        assert_eq!(parse_episode_guess("Movie 2016 [BD]"), None);
        assert_eq!(parse_episode_guess("no numbers here"), None);
    }

    #[test]
    fn episode_guess_ignores_crc_and_codec_digits() {
        // trailing CRC32 must not beat the real episode number
        assert_eq!(
            parse_episode_guess("[SubsPlease] Sousou no Frieren - 28 (1080p) [AB12CD34].mkv"),
            Some(28)
        );
        // codec digits in a bracket group are noise too
        assert_eq!(
            parse_episode_guess("[Group] Show - 07 [1080p x264-10bit].mkv"),
            Some(7)
        );
        // v2 revision suffix belongs to the episode it follows
        assert_eq!(parse_episode_guess("Some Show - 04v2 [BD 1080p].mkv"), Some(4));
        assert_eq!(parse_episode_guess("[GJM] 86 - 11 (1080p) [DEADBEEF].mkv"), Some(11));
    }

    /// Digits in trailing release tags must not beat the real episode number.
    #[test]
    fn episode_guess_ignores_trailing_release_tags() {
        assert_eq!(parse_episode_guess("[Group] Show - 05 [1080p] AAC2.0 x265"), Some(5));
        assert_eq!(parse_episode_guess("Show - 05 DDP2.0 H 264"), Some(5));
        assert_eq!(parse_episode_guess("Show S02E05 AAC2.0"), Some(5));
        assert_eq!(parse_episode_guess("Show - 12 Opus2.0"), Some(12));
        assert_eq!(parse_episode_guess("Show - 05 [Multi-Subs] DDP5.1"), Some(5));
        assert_eq!(
            parse_episode_guess("[SubsPlease] Frieren - 28 (1080p) [AB12CD34]"),
            Some(28)
        );
    }

    #[test]
    fn episode_after_title_removal_avoids_title_numbers() {
        // 91 Days trap. Number in the title must not become the episode.
        let variants = vec!["91 Days".to_string()];
        assert_eq!(
            parse_episode_after("[Group] 91 Days - 05 [1080p]", &variants),
            Some(Some(5))
        );
        // Batch file. Title matched but no episode. Some(None) must stop callers
        // from guessing or guessing would return 91.
        assert_eq!(parse_episode_after("91 Days [BD 1080p]", &variants), Some(None));
        // No variant in the string at all. None means guessing is allowed.
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
        // Batch file. Matched, no episode. None, not 91.
        assert_eq!(resolve_episode(&days, &["91 Days", "91 Days [BD 1080p]"]), None);
        // Player title cleaned, filename carries the episode. Read it there.
        assert_eq!(resolve_episode(&days, &["91 Days", "91 Days - 05 [BD]"]), Some(5));
        // Alias case. The raw variant never appears since the colon was dropped.
        // The normalized match falls back to guessing.
        let rezero = Matcher {
            media_id: 2,
            display: "Re:Zero".into(),
            variants: vec!["Re:Zero kara Hajimeru Isekai Seikatsu".into()],
            norms: vec!["re zero kara hajimeru isekai seikatsu".into()],
            status_rank: 0,
        };
        assert_eq!(resolve_episode(&rezero, &["Re Zero - 05", "Re Zero - 05"]), Some(5));
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
        // containment. Playing string contains the normalized title.
        assert!(match_title(&matchers, "", "file:///x/[G] Sousou no Frieren - 28.mkv").is_some());
        assert!(match_title(&matchers, "Totally Different Show", "").is_none());
    }

    /// End to end for the separatorless-episode fix. Steins;Gate 01 with no dash
    /// must clean to the list norm and match, not lose its final e.
    #[test]
    fn match_title_without_separator_before_episode() {
        let matchers = vec![mk(1, "Steins;Gate"), mk(2, "Fate"), mk(3, "One Piece")];
        assert_eq!(match_title(&matchers, "Steins;Gate 01", "").map(|m| m.media_id), Some(1));
        assert_eq!(match_title(&matchers, "Fate 01", "").map(|m| m.media_id), Some(2));
        assert_eq!(match_title(&matchers, "One Piece 1015", "").map(|m| m.media_id), Some(3));
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
        Matcher { status_rank: status_rank(status), ..mk(media_id, title) }
    }

    /// A sequel on the list must win over its own base series. AniList writes
    /// romaji sequels as a bare ordinal while release groups write 7th Season
    /// or Season 7. Without collapsing those the sequel norm failed the token
    /// check and the base matched as a prefix, so the episode landed on season 1.
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
        // base series alone still resolves to the base series
        assert_eq!(
            match_title(&matchers, "Boku no Hero Academia - 05.mkv", "").map(|m| m.media_id),
            Some(1)
        );
    }

    /// The episode-tail strip must never consume the entire title. A bare-number
    /// show is all episode tail to the regex, and an empty candidate matches
    /// nothing.
    #[test]
    fn a_bare_number_title_survives_the_tail_strip() {
        assert_eq!(clean_title("86.mkv"), "86");
        assert_eq!(clean_title("91.mkv"), "91");
        // real title plus an episode still strips normally
        assert_eq!(clean_title("86 - 05.mkv"), "86");
    }

    /// Identical normalized titles used to resolve to whichever row SQLite
    /// returned last. Prefer what the user is watching, then the lowest media_id
    /// so the answer is stable across runs.
    #[test]
    fn duplicate_titles_break_ties_deterministically() {
        let watching = vec![
            mk_status(10, "Some Show", "COMPLETED"),
            mk_status(20, "Some Show", "CURRENT"),
        ];
        assert_eq!(match_title(&watching, "Some Show - 03", "").map(|m| m.media_id), Some(20));
        // same status on both. Lower id wins, and it wins in either order.
        let a = vec![mk_status(20, "Some Show", "CURRENT"), mk_status(10, "Some Show", "CURRENT")];
        let b = vec![mk_status(10, "Some Show", "CURRENT"), mk_status(20, "Some Show", "CURRENT")];
        assert_eq!(match_title(&a, "Some Show - 03", "").map(|m| m.media_id), Some(10));
        assert_eq!(match_title(&b, "Some Show - 03", "").map(|m| m.media_id), Some(10));
    }

    /// A literal percent in a filename must not be decoded. Old code parsed a
    /// non-hex pair as 0, emitting a NUL byte and swallowing two more bytes.
    #[test]
    fn percent_decode_leaves_stray_percent_signs_alone() {
        assert_eq!(basename("file:///x/50%20off.mkv"), "50 off");
        // percent followed by a multi-byte char. Previously produced "50\0フ".
        assert_eq!(basename("file:///x/50% オフ.mkv"), "50% オフ");
        assert_eq!(basename("file:///x/100%.mkv"), "100%");
        assert_eq!(basename("file:///x/%zz.mkv"), "%zz");
        // real escapes still decode, including multi-byte sequences
        assert_eq!(basename("file:///x/%E3%82%AF%E3%83%AA%E3%82%B9.mkv"), "クリス");
    }

    /// Unicode norms must not panic the token scan.
    #[test]
    fn token_containment_handles_multibyte_titles() {
        let matchers = vec![mk(1, "四月は君の嘘"), mk(2, "Shigatsu wa Kimi no Uso")];
        assert!(match_title(&matchers, "[G] 四月は君の嘘 - 05 [1080p].mkv", "").is_some());
        assert!(match_title(&matchers, "ぜんぜん違う番組", "").is_none());
    }

    /// Only known media extensions are stripped, and list-title norms keep their
    /// trailing numbers. The naive last-dot strip turned No.6 into no and
    /// D.Gray-man into d. The episode-tail strip then ate what was left.
    #[test]
    fn strip_ext_and_norms_keep_real_titles() {
        assert_eq!(norm_title("No.6"), "no 6");
        assert_eq!(norm_title("D.Gray-man"), "d gray man");
        assert_eq!(norm_title("Dr. STONE"), "dr stone");
        assert_eq!(norm_title("86"), "86");
        assert_eq!(norm_title("Steins;Gate 0"), "steins gate 0");
        assert_eq!(norm_title("Ghost in the Shell 2.0"), "ghost in the shell 2 0");
        // release names still get the episode tail and extension stripped
        assert_eq!(clean_title("Show Name - 03.mkv"), "show name");
        assert_eq!(clean_title("Show Name - 03.MKV"), "show name");
        // a torrent title that is not a filename is no longer truncated at the
        // last dot. Only the codec and episode noise goes.
        assert_eq!(
            clean_title("Clevatess S02E03 CR WEB-DL DUAL AAC2.0 H.264 (Clevatess: Majuu no Ou)"),
            "clevatess s02e03 cr dual aac2"
        );
    }

    /// Short single-word list titles must not match unrelated releases via raw
    /// substring containment.
    #[test]
    fn short_titles_do_not_mismatch() {
        let matchers = vec![
            mk(1, "Another"),
            mk(2, "86"),
            mk(3, "Dr. STONE"),
            mk(4, "No.6"),
            mk(5, "K"),
        ];
        // another mid-string in an unrelated title. Single word means no match.
        assert!(match_title(&matchers, "[G] Re:Zero Starting Life in Another World - 05", "").is_none());
        // dr must not live inside dreaming. no 6 needs whole tokens.
        assert!(match_title(&matchers, "[ToonsHub] Grand Blue Dreaming S03E03 1080p", "").is_none());
        assert!(match_title(&matchers, "[G] Sora wa Akai Kawa no Hotori - 03", "").is_none());
        // 1-char norm K matches nothing but itself
        assert!(match_title(&matchers, "Walking the Way All Alone S01E16 1080p", "").is_none());
        assert_eq!(match_title(&matchers, "K - 05", "").map(|m| m.media_id), Some(5));
        // the real shows still match
        assert_eq!(match_title(&matchers, "[SubsPlease] Another - 05 (1080p)", "").map(|m| m.media_id), Some(1));
        assert_eq!(match_title(&matchers, "[SubsPlease] 86 - 11 (1080p)", "").map(|m| m.media_id), Some(2));
        assert_eq!(match_title(&matchers, "[SubsPlease] Dr. Stone S3 - 05 (1080p)", "").map(|m| m.media_id), Some(3));
        assert_eq!(match_title(&matchers, "[G] No.6 - 03 [720p]", "").map(|m| m.media_id), Some(4));
    }

    /// Prefix in either direction and long interior phrases still match. Token
    /// boundaries are respected.
    #[test]
    fn token_boundary_prefix_and_interior() {
        let matchers = vec![mk(1, "Ghost in the Shell"), mk(2, "Shiro")];
        // secondary title after a pipe. Long multi-word phrase mid-string.
        assert_eq!(
            match_title(&matchers, "[Kotobuki] Koukaku Kidoutai (2026) 03 [1080p HEVC Multisub] | The Ghost in the Shell", "")
                .map(|m| m.media_id),
            Some(1)
        );
        // shiro is not a token inside shirobako
        assert!(match_title(&matchers, "[G] Shirobako - 05", "").is_none());
        // shortened release title is a prefix of the full list title
        let rezero = vec![mk(1, "Re:Zero kara Hajimeru Isekai Seikatsu")];
        assert_eq!(match_title(&rezero, "Re Zero - 05", "").map(|m| m.media_id), Some(1));
        // list title is a prefix of a longer release string
        let yama = vec![mk(1, "Yama no Susume")];
        assert_eq!(
            match_title(&yama, "[G] Yama no Susume Next Summit - 03", "").map(|m| m.media_id),
            Some(1)
        );
        // prefix pair resolves to the more specific show
        let pair = vec![mk(1, "Toradora"), mk(2, "Toradora SOS")];
        assert_eq!(
            match_title(&pair, "[G] Toradora SOS - 02", "").map(|m| m.media_id),
            Some(2)
        );
    }

    /// A generic candidate matches every entry of a multi-season franchise as a
    /// prefix once the season marker strips away. The matcher must resolve to
    /// the season the user is currently watching, not the entry with the longest
    /// title. Regression for a bug where Mushoku S3 updates were silently routed
    /// to a 1-episode COMPLETED special purely because its title was longer, so
    /// the detected episode got clamped to 1 and progress never moved.
    #[test]
    fn generic_match_prefers_current_over_longest_title() {
        // All six real Mushoku entries. Only Season 3 (178789) is CURRENT.
        let titles: &[(i64, &str, &str)] = &[
            (108465, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation"),
            (127720, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Cour 2"),
            (141534, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer"),
            (146065, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Season 2"),
            (166873, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Season 2 Part 2"),
            (178789, "CURRENT", "Mushoku Tensei: Jobless Reincarnation Season 3"),
        ];
        let rom_by_id: std::collections::HashMap<i64, &str> = [
            (108465, "Mushoku Tensei: Isekai Ittara Honki Dasu"),
            (127720, "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2"),
            (141534, "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2 - Eris no Goblin Toubatsu"),
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
            assert_eq!(m.media_id, 178789, "S03 release must resolve to Season 3 (CURRENT)");
            let ep = resolve_episode(m, &[raw, basename(&url).as_str()]);
            assert_eq!(ep, raw.contains("E04").then_some(4).or(Some(5)));
        }
    }

    /// The season-ordinal fix. When all entries share the same status, the
    /// season marker in the release must still disambiguate to the correct
    /// season. The old status-based tiebreak couldn't handle this since status
    /// ties and longest-title wins, which picks the special.
    #[test]
    fn season_ordinal_beats_same_status_siblings() {
        // Every entry is COMPLETED. Without the season ordinal, the longest
        // title entry (141534, the special) would win.
        let titles: &[(i64, &str, &str)] = &[
            (108465, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation"),
            (141534, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer"),
            (146065, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Season 2"),
            (178789, "COMPLETED", "Mushoku Tensei: Jobless Reincarnation Season 3"),
        ];
        let rom_by_id: std::collections::HashMap<i64, &str> = [
            (108465, "Mushoku Tensei: Isekai Ittara Honki Dasu"),
            (141534, "Mushoku Tensei: Isekai Ittara Honki Dasu Part 2 - Eris no Goblin Toubatsu"),
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
        assert_eq!(m.media_id, 178789, "S03 must resolve to Season 3 even when all are COMPLETED");
    }

    /// The season ordinal wins over status. Even if the user is currently
    /// watching Season 2, an S03E05 release resolves to Season 3 because the
    /// season marker is the strongest signal.
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
        assert_eq!(m.media_id, 178789, "S03 release must resolve to Season 3 (PAUSED) not S2 (CURRENT)");
    }

    /// A season pack with no episode number must not have its season number
    /// parsed as the episode. Show S02 returns Some(None) from
    /// parse_episode_after, not episode 2.
    #[test]
    fn season_pack_does_not_parse_season_as_episode() {
        let variants = vec!["Some Show".to_string()];
        // Some Show S02. The season marker should be stripped, leaving no
        // episode number.
        assert_eq!(parse_episode_after("Some Show S02 [1080p]", &variants), Some(None));
        // Some Show S02E05. The episode number is still read correctly.
        assert_eq!(parse_episode_after("Some Show S02E05 [1080p]", &variants), Some(Some(5)));
    }

    /// A non-seasonal release must not get a season boost. The status tiebreak
    /// still applies as before.
    #[test]
    fn no_season_marker_falls_back_to_status() {
        let matchers = vec![
            mk_status(1, "Some Show", "COMPLETED"),
            mk_status(2, "Some Show", "CURRENT"),
        ];
        // No S\d{1,2}E\d in the release. season_match is false for both.
        // status_rank decides. CURRENT wins.
        assert_eq!(
            match_title(&matchers, "Some Show - 03", "").map(|m| m.media_id),
            Some(2)
        );
    }
}
