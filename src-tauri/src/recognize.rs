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
static RE_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());

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
                let n = clean_title(v);
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

/// Match a now-playing string against the cached list. Tries the raw title first,
/// then the file basename. Returns the first matcher that matches exactly, then
/// falls back to substring containment.
pub(crate) fn match_title<'a>(matchers: &'a [Matcher], title: &str, url: &str) -> Option<&'a Matcher> {
    let candidates = [clean_title(title), clean_title(&basename(url))];
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        // exact normalized match
        if let Some(m) = matchers.iter().find(|m| m.norms.iter().any(|n| n == &cand)) {
            return Some(m);
        }
        // containment (one inside the other)
        if let Some(m) = matchers
            .iter()
            .find(|m| m.norms.iter().any(|n| n.contains(&cand) || cand.contains(n)))
        {
            return Some(m);
        }
    }
    None
}

// ─────────────────────────── parsing helpers ───────────────────────────

/// Normalize a title for fuzzy comparison: lowercase, split on non-alphanumeric,
/// drop the noise tokens, collapse spaces.
pub(crate) fn clean_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    let s = RE_EP_TAIL.replace(&s, "");
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

/// Strip a trailing file extension, if any.
fn strip_ext(s: &str) -> String {
    match s.rfind('.') {
        Some(i) if i > 0 && !s[i..].contains('/') && !s[i..].contains('\\') => s[..i].to_string(),
        _ => s.to_string(),
    }
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
pub(crate) fn parse_episode_after(playing: &str, variants: &[String]) -> Option<i64> {
    let lp = playing.to_lowercase();
    for v in variants {
        let lv = v.to_lowercase();
        if lv.is_empty() {
            continue;
        }
        if lp.contains(&lv) {
            let remainder = lp.replace(&lv, " ");
            if let Some(n) = parse_last_episode_number(&remainder) {
                return Some(n);
            }
        }
    }
    None
}

/// Fallback: pick the last plausible episode number from a raw string.
pub(crate) fn parse_episode_guess(s: &str) -> Option<i64> {
    parse_last_episode_number(s)
}

/// Years read as "year, not episode": 1930 through next year. The upper bound
/// tracks the current year instead of a hardcoded 2099.
fn looks_like_year(n: i64) -> bool {
    use chrono::Datelike;
    (1930..=chrono::Utc::now().year() as i64 + 1).contains(&n)
}

/// Last integer that looks like an episode (excludes resolutions and 4-digit years).
fn parse_last_episode_number(s: &str) -> Option<i64> {
    RE_NUM
        .find_iter(s)
        .filter_map(|m| m.as_str().parse::<i64>().ok())
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
    fn episode_after_title_removal_avoids_title_numbers() {
        // The "91 Days" trap: the number in the title must not become the episode.
        let variants = vec!["91 Days".to_string()];
        assert_eq!(
            parse_episode_after("[Group] 91 Days - 05 [1080p]", &variants),
            Some(5)
        );
        assert_eq!(parse_episode_after("91 Days [BD]", &variants), None);
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
}
