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
    Regex::new(r"(?i)\b(1080|720|480|360|2160|1440|4320)p?\b|\b(bd|bdrip|blu-?ray|blueray|webrip|web-?dl|dvdrip|hevc10|hevc|x265|x264|h[\s.]*26[45]|avc|aac|eac3|ddp?\d|opus|flac|10bit|hi10|yuv420)\b").unwrap()
});
// Trailing episode marker. The bare `E05` form needs a separator or a season
// prefix before the `e` — otherwise the title's own final `e` is eaten
// ("Steins;Gate 01" → "steins gat"). `\d{1,4}` covers 1000+ episode runs.
static RE_EP_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\s*[-_·]?\s*(?:[sS]\d{1,2}[eE]|ep(?:isode)?\.?|[-_·\s][eE]|#)?\s*0*\d{1,4}(?:v\d+)?\s*(?:end|final)?\s*$").unwrap()
});
/// One episode-number candidate: digits with an optional `vN` revision suffix
/// ("04v2" is episode 4 — not episodes 4 and 2).
static RE_EP_NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+(?:v\d+)?").unwrap());
/// Season markers on an ALREADY-NORMALIZED string (lowercase, space-separated),
/// collapsed to the bare ordinal AniList uses in romaji sequel titles.
/// "7th season" / "2nd season" -> "7" / "2".
static RE_NTH_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?P<n>\d{1,2})(?:st|nd|rd|th) season\b").unwrap());
/// "season 7" -> "7" (also covers the English titles AniList stores).
static RE_SEASON_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bseason (?P<n>\d{1,2})\b").unwrap());
/// Audio channel layouts ("2.0", "5.1"). Stripped before the codec names so
/// their digits cannot outrank the episode number ("Show - 12 Opus2.0" is
/// episode 12, not 2 — and "… DDP5.1" is not episode 1). No leading `\b`:
/// the layout is usually glued to its codec ("AAC2.0"); the trailing `\b`
/// alone keeps dot-separated tags like "S01E01.1080p" intact.
static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d\.\d\b").unwrap());

/// Resolutions / common bitrates to discard when picking the episode number.
const NOISE_NUMBERS: [i64; 7] = [360, 480, 720, 1080, 1440, 2160, 4320];

// ─────────────────────────── list matchers ───────────────────────────

pub(crate) struct Matcher {
    pub media_id: i64,
    pub display: String,
    pub variants: Vec<String>, // raw english / romaji / native titles
    norms: Vec<String>,        // normalized variants for comparison
    /// Rank of the entry's list status, used only to break an otherwise exact
    /// tie. Lower is preferred.
    status_rank: u8,
}

/// Two list entries can normalize to the SAME title (a duplicate, or a special
/// that shares its parent's name). The winner used to be whichever row the
/// unordered DB scan happened to yield last, so the same file could resolve to a
/// different show between runs. Prefer what the user is actually watching, then
/// fall back to the lowest media_id so the choice is at least stable.
fn status_rank(status: &str) -> u8 {
    match status {
        "CURRENT" => 0,
        "REPEATING" => 1,
        "PAUSED" => 2,
        "PLANNING" => 3,
        "COMPLETED" => 4,
        _ => 5, // DROPPED and anything AniList adds later
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
        out.push(Matcher {
            media_id: m.id,
            display: m.display_title(),
            variants,
            norms,
            status_rank: status_rank(&e.status),
        });
    }
    // Stable order so the tie-break below is deterministic regardless of the
    // order SQLite happened to return the rows in.
    out.sort_by_key(|m| (m.status_rank, m.media_id));
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
    // Interior / suffix occurrence, both edges on token boundaries. Searching for
    // `short` directly and checking the byte BEFORE it avoids allocating a padded
    // copy: this is the recognizer's innermost loop (every list norm against
    // every candidate), so a String per comparison is thousands of allocations
    // per scanned file.
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
        // Advance one CHARACTER, not one byte: norms keep unicode (Japanese
        // titles), and slicing mid-codepoint would panic.
        from = start + long[start..].chars().next().map_or(1, char::len_utf8);
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
            // On an exact tie prefer the better status, then the lower media_id.
            // `max_by_key` keeps the LAST maximum, so the ranks are negated to
            // turn "smaller is better" into "larger wins".
            .max_by_key(|((tier, len), m)| {
                (*tier, *len, std::cmp::Reverse(m.status_rank), std::cmp::Reverse(m.media_id))
            });
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
    let stripped = RE_EP_TAIL.replace(&s, "");
    // The tail strip must never eat the WHOLE title. A bare-number show ("86",
    // "91 Days" as "91") is all episode-tail as far as the regex is concerned,
    // and an empty candidate matches nothing at all — so keep the unstripped
    // form when stripping leaves us with nothing to match on.
    let normed = normalize(&stripped);
    let out = if normed.is_empty() { normalize(&s) } else { normed };
    season_ordinals(&out)
}

/// Normalize a LIST TITLE for comparison. Same cleaning as `clean_title` minus
/// the episode-tail strip — a trailing number in a list title is part of the
/// name ("No.6", "86", "Mob Psycho 100", "Steins;Gate 0"), not an episode.
pub(crate) fn norm_title(s: &str) -> String {
    let s = strip_ext(s);
    let s = RE_BRACKETS.replace_all(&s, " ");
    let s = RE_RES.replace_all(&s, " ");
    season_ordinals(&normalize(&s))
}

/// Collapse season markers to a bare ordinal so both sides of a comparison agree:
/// `"… 7th season"` and `"… season 7"` both become `"… 7"`, which is how AniList
/// writes the romaji sequel title (`"Boku no Hero Academia 7"`). Without this a
/// release named MAL-style lost its own sequel entry to the base series: the
/// sequel norm failed the token-boundary check against "…academia 7th season"
/// while the base "…academia" matched as a clean prefix, so progress was written
/// to season 1. Applied to list titles and release names alike.
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
        // Both digits must be ASCII hex. The old code passed non-UTF-8 pairs
        // through `unwrap_or("00")`, which parsed as 0 — so a literal '%' before
        // a multi-byte character (a real filename like "50% オフ") emitted a NUL
        // byte AND swallowed the next two bytes, corrupting the title it was
        // supposed to be decoding.
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            if let Ok(b) = u8::from_str_radix(
                // Both bytes are ASCII hex, so this slice is always valid UTF-8.
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
/// codec tags), audio channel layouts ("2.0", "5.1"), and resolution/codec
/// noise are stripped FIRST — their digits would otherwise beat the real
/// episode number: "... - 28 (1080p) [AB12CD34]" is episode 28, not 34, and
/// "... - 05 AAC2.0 x265" is episode 5, not 265. Excludes resolutions,
/// 4-digit years, and anything outside 1–9999. A `vN` revision suffix belongs
/// to the number it follows.
fn parse_last_episode_number(s: &str) -> Option<i64> {
    let s = RE_BRACKETS.replace_all(s, " ");
    let s = RE_CHANNEL.replace_all(&s, " ");
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

    /// No separator before the episode must not eat the title's final "e",
    /// and 1000+ episode runs must not leave a stray digit behind.
    #[test]
    fn clean_keeps_title_final_e_and_4_digit_episodes() {
        assert_eq!(clean_title("Steins;Gate 01"), "steins gate");
        assert_eq!(clean_title("Fate 01"), "fate");
        // the dash-separated form already worked
        assert_eq!(clean_title("Steins;Gate - 01"), "steins gate");
        assert_eq!(clean_title("One Piece 1015"), "one piece");
        // E05 / S02E05 marker forms still strip, season prefix included
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

    /// Digits in trailing release tags (x265, H 264, AAC2.0, Opus2.0, DDP5.1)
    /// must not beat the real episode number.
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
            status_rank: 0,
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
        // containment: playing string contains the normalized title
        assert!(match_title(&matchers, "", "file:///x/[G] Sousou no Frieren - 28.mkv").is_some());
        assert!(match_title(&matchers, "Totally Different Show", "").is_none());
    }

    /// End to end for the separatorless-episode fix: "Steins;Gate 01" (no
    /// dash) must clean to the list norm and match, not lose its final "e".
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

    /// A sequel that IS on the list must win over its own base series. AniList
    /// writes romaji sequels as a bare ordinal ("Boku no Hero Academia 7") while
    /// release groups write "7th Season" / "Season 7"; without collapsing those
    /// the sequel norm failed the token-boundary check and the base matched as a
    /// clean prefix, so the episode landed on season 1.
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
        // The base series alone still resolves to the base series.
        assert_eq!(
            match_title(&matchers, "Boku no Hero Academia - 05.mkv", "").map(|m| m.media_id),
            Some(1)
        );
    }

    /// The episode-tail strip must never consume the entire title: a bare-number
    /// show is all "episode tail" to the regex, and an empty candidate matches
    /// nothing at all.
    #[test]
    fn a_bare_number_title_survives_the_tail_strip() {
        assert_eq!(clean_title("86.mkv"), "86");
        assert_eq!(clean_title("91.mkv"), "91");
        // ...while a real title plus an episode still strips normally.
        assert_eq!(clean_title("86 - 05.mkv"), "86");
    }

    /// Identical normalized titles used to resolve to whichever row SQLite
    /// happened to return last. Prefer what the user is watching, then the
    /// lowest media_id, so the answer is stable across runs.
    #[test]
    fn duplicate_titles_break_ties_deterministically() {
        let watching = vec![
            mk_status(10, "Some Show", "COMPLETED"),
            mk_status(20, "Some Show", "CURRENT"),
        ];
        assert_eq!(match_title(&watching, "Some Show - 03", "").map(|m| m.media_id), Some(20));
        // Same status on both: the lower id wins, and it wins in either order.
        let a = vec![mk_status(20, "Some Show", "CURRENT"), mk_status(10, "Some Show", "CURRENT")];
        let b = vec![mk_status(10, "Some Show", "CURRENT"), mk_status(20, "Some Show", "CURRENT")];
        assert_eq!(match_title(&a, "Some Show - 03", "").map(|m| m.media_id), Some(10));
        assert_eq!(match_title(&b, "Some Show - 03", "").map(|m| m.media_id), Some(10));
    }

    /// A literal '%' in a filename must not be decoded. The old code parsed a
    /// non-hex pair as 0, emitting a NUL byte and swallowing two more bytes.
    #[test]
    fn percent_decode_leaves_stray_percent_signs_alone() {
        assert_eq!(basename("file:///x/50%20off.mkv"), "50 off");
        // '%' followed by a multi-byte char: previously produced "50\0フ".
        assert_eq!(basename("file:///x/50% オフ.mkv"), "50% オフ");
        assert_eq!(basename("file:///x/100%.mkv"), "100%");
        assert_eq!(basename("file:///x/%zz.mkv"), "%zz");
        // Real escapes still decode, including multi-byte sequences.
        assert_eq!(basename("file:///x/%E3%82%AF%E3%83%AA%E3%82%B9.mkv"), "クリス");
    }

    /// Unicode norms must not panic the token scan (byte indices vs codepoints).
    #[test]
    fn token_containment_handles_multibyte_titles() {
        let matchers = vec![mk(1, "四月は君の嘘"), mk(2, "Shigatsu wa Kimi no Uso")];
        assert!(match_title(&matchers, "[G] 四月は君の嘘 - 05 [1080p].mkv", "").is_some());
        assert!(match_title(&matchers, "ぜんぜん違う番組", "").is_none());
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
