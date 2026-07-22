//! Filename / now-playing title recognition — the seed of the M3 library scanner.
//! Shared by the MPRIS watcher (`playback.rs`) and the library scanner
//! (`library.rs`): both need to clean a raw title/filename, match it against the
//! cached list, and pull an episode number out of the remainder.

use std::sync::LazyLock;

use regex::Regex;

use crate::db::Db;

// ─────────────────────────── regex toolkit ───────────────────────────

static RE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(【][^\]\)】]*[\]\)】]").unwrap());
static RE_RES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(1080|720|480|360|2160|1440|4320)p?\b|\b(bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc|x264|h\.?264|avc|aac|flac|10bit|hi10|yuv420)\b").unwrap()
});
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·]?\s*(?:ep(?:isode)?\.?|e|#)?\s*0*\d{1,3}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
/// One episode-number candidate: digits with an optional `vN` revision suffix
/// ("04v2" is episode 4 — not episodes 4 and 2).
static RE_EP_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:v\d+)?").unwrap());

/// Resolutions / common bitrates to discard when picking the episode number.
const NOISE_NUMBERS: [i64; 7] = [360, 480, 720, 1080, 1440, 2160, 4320];

// ─────────────────────────── list matchers ───────────────────────────

pub(crate) struct Matcher {
    pub media_id: i64,
    pub display: String,
    pub variants: Vec<String>, // raw english / romaji / native titles
    norms: Vec<String>,        // normalized variants for comparison
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
                // norm_title, NOT clean_title: the episode-tail strip is for
                // release names — on list titles it ate numeric suffixes
                // ("No.6" → "no", "86" → "", "Steins;Gate 0" → "steins gate").
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
        out.push(Matcher { media_id: m.id, display: m.display_title(), variants, norms });
    }
    out
}

/// Word-boundary containment: does `long` contain `short` as a contiguous run
/// of whole TOKENS? Both sides are normalized (lowercase, single-space-joined),
/// so a boundary check is a space check. Some(true) = at the very start (the
/// title position in release names), Some(false) = later in the string. Raw
/// char containment is NOT enough: "dr" lives inside "dreaming", "ost" inside
/// "lost" — short norms char-matched half the feed.
fn contains_tokens(long: &str, short: &str) -> Option<bool> {
    if short.is_empty() || short.len() > long.len() {
        return None;
    }
    if long.starts_with(short)
        && (long.len() == short.len() || long.as_bytes()[short.len()] == b' ')
    {
        return Some(true);
    }
    // interior / suffix occurrence, both edges on token boundaries
    let padded = format!(" {short}");
    let mut from = 0;
    while let Some(i) = long[from..].find(&padded) {
        let start = from + i + 1;
        let end = start + short.len();
        if end == long.len() || long.as_bytes()[end] == b' ' {
            return Some(false);
        }
        from = start;
    }
    None
}

/// Match quality of one norm against one candidate. None = no match.
/// 3 = exact; 2 = one is a whole-token PREFIX of the other (the title leads in
/// release names, or the release uses a shortened title); 1 = a long multi-word
/// norm phrase sits mid-string (secondary titles: "… | The Ghost in the Shell").
/// Short strings only match exactly — single common words ("another", "dr") and
/// tiny norms appear inside unrelated titles far too often to trust.
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

/// Match a now-playing string against the cached list. Tries the raw title first,
/// then the file basename. Best (tier, norm length) wins, so an exact hit beats
/// a prefix, a prefix beats an interior phrase, and a prefix pair ("Toradora"
/// vs "Toradora SOS") resolves to the more specific show.
pub(crate) fn match_title<'a>(matchers: &'a [Matcher], title: &str, url: &str) -> Option<&'a Matcher> {
    let candidates = [clean_title(title), clean_title(&basename(url))];
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        let best = matchers
            .iter()
            .filter_map(|m| {
                m.norms
                    .iter()
                    .filter_map(|n| norm_match_tier(n, &cand).map(|t| (t, n.len())))
                    .max()
                    .map(|score| (score, m))
            })
            .max_by_key(|(score, _)| *score);
        if let Some((_, m)) = best {
            return Some(m);
        }
    }
    None
}

// ─────────────────────────── parsing helpers ───────────────────────────

/// Normalize a RELEASE name / now-playing string for comparison: lowercase,
/// split on non-alphanumeric, drop bracket groups + resolution noise, and strip
/// the trailing episode marker.
pub(crate) fn clean_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    let s = RE_EP_TAIL.replace(&s, "");
    normalize(&s)
}

/// Normalize a LIST TITLE for comparison. Same cleaning as `clean_title` minus
/// the episode-tail strip — a trailing number in a list title is part of the
/// name ("No.6", "86", "Mob Psycho 100", "Steins;Gate 0"), not an episode.
pub(crate) fn norm_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    normalize(&s)
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

/// File extensions worth stripping from a title/basename. ONLY these — the old
/// naive last-dot strip mutilated titles like "No.6" (→ "No") and "D.Gray-man"
/// (→ "D"), and truncated torrent names at codec tags ("… AAC2.0 H.264 (…)" →
/// "… AAC2"), feeding the matcher degenerate norms and candidates that then
/// substring-matched everything.
const STRIP_EXTS: &[&str] = &[
    "mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "ogm", "wmv", "flv", "mpg", "mpeg", "m2ts",
    "ogv",
];

/// Strip a trailing KNOWN media extension, if any.
fn strip_ext(s: &str) -> String {
    if let Some(i) = s.rfind('.') {
        let ext = &s[i + 1..];
        if STRIP_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

/// Last path segment of a `file://` URL (or any path-ish string), extension
/// stripped and percent-decoded.
pub(crate) fn basename(url: &str) -> String {
    let seg = url
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(url);
    let seg = strip_ext(seg);
    percent_decode(&seg)
}

/// Minimal percent-decoding for `%20` etc. Decoded bytes are accumulated and then
/// interpreted as UTF-8 (so multi-byte sequences like `%E3%82%AF` → ク survive,
/// instead of being pushed byte-by-byte as garbage chars).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
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

/// Once we know which media it is, parse the episode from the *remainder* after the
/// matched title variant is removed — avoids misreading a number in the title
/// itself (e.g. "91 Days").
///
/// Tri-state: `Some(Some(n))` = a variant matched and the remainder yielded
/// episode n. `Some(None)` = a variant matched but there IS no episode in the
/// string (a batch file) — callers must not fall back to guessing, or the
/// title's own number comes back as the "episode". `None` = no variant in the
/// string at all (the normalized match used an alias) — guessing is fair game.
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

/// Fallback: pick the last plausible episode number from a raw string. Only for
/// strings where NO title variant matched (see parse_episode_after).
pub(crate) fn parse_episode_guess(s: &str) -> Option<i64> {
    parse_last_episode_number(s)
}

/// Resolve the episode for a matched title from candidate strings (player title,
/// then file basename): a matched variant with digits wins; a matched variant
/// with no digits in ANY candidate means a batch file (no episode, no guessing);
/// no raw variant anywhere means the normalized match used an alias — guess.
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

/// Years read as "year, not episode": 1900 through next year. The upper bound
/// tracks the current year instead of a hardcoded 2099; the lower bound covers
/// the handful of pre-1930 shorts AniList lists.
fn looks_like_year(n: i64) -> bool {
    use chrono::Datelike;
    (1900..=chrono::Utc::now().year() as i64 + 1).contains(&n)
}

/// Last integer that looks like an episode. Bracketed groups (CRC32 hashes,
/// codec tags) and resolution/codec noise are stripped FIRST — their digits
/// would otherwise beat the real episode number: "... - 28 (1080p) [AB12CD34]"
/// is episode 28, not 34. Excludes resolutions, 4-digit years, and anything
/// outside 1–9999. A `vN` revision suffix belongs to the number it follows.
fn parse_last_episode_number(s: &str) -> Option<i64> {
    let s = RE_BRACKETS.replace_all(s, " ");
    let s = RE_RES.replace_all(&s, " ");
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
        // The trailing CRC32 must not beat the real episode number.
        assert_eq!(
            parse_episode_guess("[SubsPlease] Sousou no Frieren - 28 (1080p) [AB12CD34].mkv"),
            Some(28)
        );
        // Codec digits in a bracket group are noise too (264, 10).
        assert_eq!(
            parse_episode_guess("[Group] Show - 07 [1080p x264-10bit].mkv"),
            Some(7)
        );
        // A v2 revision suffix belongs to the episode it follows.
        assert_eq!(parse_episode_guess("Some Show - 04v2 [BD 1080p].mkv"), Some(4));
        assert_eq!(parse_episode_guess("[GJM] 86 - 11 (1080p) [DEADBEEF].mkv"), Some(11));
    }

    #[test]
    fn episode_after_title_removal_avoids_title_numbers() {
        // The "91 Days" trap: the number in the title must not become the episode.
        let variants = vec!["91 Days".to_string()];
        assert_eq!(
            parse_episode_after("[Group] 91 Days - 05 [1080p]", &variants),
            Some(Some(5))
        );
        // A batch file: the title matched but there IS no episode — Some(None),
        // which must stop callers from guessing (guessing would return 91).
        assert_eq!(parse_episode_after("91 Days [BD 1080p]", &variants), Some(None));
        // No variant in the string at all → None → guessing is allowed.
        assert_eq!(parse_episode_after("Something Else - 03", &variants), None);
    }

    #[test]
    fn resolve_episode_batch_alias_and_crc() {
        let days = Matcher {
            media_id: 1,
            display: "91 Days".into(),
            variants: vec!["91 Days".into()],
            norms: vec!["91 days".into()],
        };
        // Batch file: matched, no episode → None (NOT 91).
        assert_eq!(resolve_episode(&days, &["91 Days", "91 Days [BD 1080p]"]), None);
        // Player title cleaned, filename carries the episode → read it there.
        assert_eq!(resolve_episode(&days, &["91 Days", "91 Days - 05 [BD]"]), Some(5));
        // Alias case: the raw variant never appears (colon dropped), so the
        // normalized match falls back to guessing.
        let rezero = Matcher {
            media_id: 2,
            display: "Re:Zero".into(),
            variants: vec!["Re:Zero kara Hajimeru Isekai Seikatsu".into()],
            norms: vec!["re zero kara hajimeru isekai seikatsu".into()],
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
        };
        let matchers = vec![m];
        assert!(match_title(&matchers, "Sousou no Frieren - 28", "").is_some());
        // containment: playing string contains the normalized title
        assert!(match_title(&matchers, "", "file:///x/[G] Sousou no Frieren - 28.mkv").is_some());
        assert!(match_title(&matchers, "Totally Different Show", "").is_none());
    }

    fn mk(media_id: i64, title: &str) -> Matcher {
        Matcher {
            media_id,
            display: title.into(),
            variants: vec![title.into()],
            norms: vec![norm_title(title)],
        }
    }

    /// Only KNOWN media extensions are stripped, and list-title norms keep
    /// their trailing numbers: the naive last-dot strip turned "No.6" into "no"
    /// and "D.Gray-man" into "d", the episode-tail strip then ate what was
    /// left — matcher norms that substring-matched everything.
    #[test]
    fn strip_ext_and_norms_keep_real_titles() {
        assert_eq!(norm_title("No.6"), "no 6");
        assert_eq!(norm_title("D.Gray-man"), "d gray man");
        assert_eq!(norm_title("Dr. STONE"), "dr stone");
        assert_eq!(norm_title("86"), "86");
        assert_eq!(norm_title("Steins;Gate 0"), "steins gate 0");
        assert_eq!(norm_title("Ghost in the Shell 2.0"), "ghost in the shell 2 0");
        // release names still get the episode tail + extension stripped
        assert_eq!(clean_title("Show Name - 03.mkv"), "show name");
        assert_eq!(clean_title("Show Name - 03.MKV"), "show name");
        // a torrent title that is not a filename is no longer truncated at the
        // last dot (only the codec/episode noise goes)
        assert_eq!(
            clean_title("Clevatess S02E03 CR WEB-DL DUAL AAC2.0 H.264 (Clevatess: Majuu no Ou)"),
            "clevatess s02e03 cr dual aac2"
        );
    }

    /// The mismatch epidemic: short/single-word list titles must not match
    /// unrelated releases via raw substring containment.
    #[test]
    fn short_titles_do_not_mismatch() {
        let matchers = vec![
            mk(1, "Another"),
            mk(2, "86"),
            mk(3, "Dr. STONE"),
            mk(4, "No.6"),
            mk(5, "K"),
        ];
        // "another" mid-string in an unrelated title: single word → no match
        assert!(match_title(&matchers, "[G] Re:Zero Starting Life in Another World - 05", "").is_none());
        // "dr" must not live inside "dreaming", "no 6" needs whole tokens
        assert!(match_title(&matchers, "[ToonsHub] Grand Blue Dreaming S03E03 1080p", "").is_none());
        assert!(match_title(&matchers, "[G] Sora wa Akai Kawa no Hotori - 03", "").is_none());
        // 1-char norm ("K") matches nothing but itself
        assert!(match_title(&matchers, "Walking the Way All Alone S01E16 1080p", "").is_none());
        assert_eq!(match_title(&matchers, "K - 05", "").map(|m| m.media_id), Some(5));
        // the real shows still match
        assert_eq!(match_title(&matchers, "[SubsPlease] Another - 05 (1080p)", "").map(|m| m.media_id), Some(1));
        assert_eq!(match_title(&matchers, "[SubsPlease] 86 - 11 (1080p)", "").map(|m| m.media_id), Some(2));
        assert_eq!(match_title(&matchers, "[SubsPlease] Dr. Stone S3 - 05 (1080p)", "").map(|m| m.media_id), Some(3));
        assert_eq!(match_title(&matchers, "[G] No.6 - 03 [720p]", "").map(|m| m.media_id), Some(4));
    }

    /// Prefix (either direction) and long interior phrases still match; token
    /// boundaries are respected.
    #[test]
    fn token_boundary_prefix_and_interior() {
        let matchers = vec![mk(1, "Ghost in the Shell"), mk(2, "Shiro")];
        // secondary title after a pipe: long multi-word phrase mid-string
        assert_eq!(
            match_title(&matchers, "[Kotobuki] Koukaku Kidoutai (2026) 03 [1080p HEVC Multisub] | The Ghost in the Shell", "")
                .map(|m| m.media_id),
            Some(1)
        );
        // "shiro" is not a token inside "shirobako"
        assert!(match_title(&matchers, "[G] Shirobako - 05", "").is_none());
        // shortened release title = prefix of the full list title
        let rezero = vec![mk(1, "Re:Zero kara Hajimeru Isekai Seikatsu")];
        assert_eq!(match_title(&rezero, "Re Zero - 05", "").map(|m| m.media_id), Some(1));
        // list title = prefix of a longer release string
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
}
