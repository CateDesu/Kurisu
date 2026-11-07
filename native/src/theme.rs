pub const BASE: u32 = 0x000000;
pub const PANEL: u32 = 0x070605;
pub const PANEL_2: u32 = 0x100d0b;
pub const EDGE: u32 = 0x44342b;
pub const ACCENT: u32 = 0xc18b6b;
pub const ACCENT_2: u32 = 0xd09b7a;
pub const RUST: u32 = 0x81503b;
pub const INK: u32 = 0xe5ddd2;
pub const DIM: u32 = 0xb6aaa0;
pub const SAGE: u32 = 0x9ba886;
pub const BLUE: u32 = 0x86a5ae;
pub const OCHRE: u32 = 0xbda36b;
pub const ERROR: u32 = 0xd28a82;

pub const FONT_SANS: &str = "Trebuchet MS";
pub const FONT_SERIF: &str = "Baskerville";
pub const FONT_MONO: &str = "Courier New";

pub const FONT_SANS_FALLBACK: &[&str] = &[
    "Trebuchet MS",
    "Segoe UI",
    "Noto Sans",
    "DejaVu Sans",
    "Noto Sans CJK JP",
];
pub const FONT_SERIF_FALLBACK: &[&str] = &[
    "Baskerville",
    "Palatino Linotype",
    "Book Antiqua",
    "Georgia",
    "Noto Serif",
    "Liberation Serif",
    "DejaVu Serif",
    "Noto Sans",
];
pub const FONT_MONO_FALLBACK: &[&str] = &[
    "Courier New",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Liberation Mono",
];

use std::sync::OnceLock;

use gpui::{Font, FontFallbacks, font};

struct ResolvedFonts {
    sans: Font,
    serif: Font,
    mono: Font,
}

static RESOLVED: OnceLock<ResolvedFonts> = OnceLock::new();

pub fn init_fonts(available: &[String]) {
    let resolved = ResolvedFonts {
        sans: resolve_font(FONT_SANS_FALLBACK, available),
        serif: resolve_font(FONT_SERIF_FALLBACK, available),
        mono: resolve_font(FONT_MONO_FALLBACK, available),
    };
    log::info!(
        "GPUI fonts: sans={}, serif={}, mono={}",
        resolved.sans.family,
        resolved.serif.family,
        resolved.mono.family
    );
    let _ = RESOLVED.set(resolved);
}

fn resolve_font(priority: &[&str], available: &[String]) -> Font {
    let matches = |name: &str| {
        available
            .iter()
            .any(|family| family.eq_ignore_ascii_case(name))
    };
    let primary = priority
        .iter()
        .copied()
        .find(|name| matches(name))
        .unwrap_or(".SystemUIFont");
    let mut fallbacks: Vec<String> = priority
        .iter()
        .copied()
        .filter(|name| *name != primary && matches(name))
        .map(str::to_owned)
        .collect();
    for unicode_family in ["Noto Sans CJK JP", "Noto Sans", "DejaVu Sans"] {
        if unicode_family != primary
            && matches(unicode_family)
            && !fallbacks.iter().any(|family| family == unicode_family)
        {
            fallbacks.push(unicode_family.into());
        }
    }
    let mut selected = font(primary);
    selected.fallbacks = Some(FontFallbacks::from_fonts(fallbacks));
    selected
}

pub fn sans_font() -> Font {
    RESOLVED
        .get()
        .map_or_else(|| font(FONT_SANS), |fonts| fonts.sans.clone())
}

pub fn serif_font() -> Font {
    RESOLVED
        .get()
        .map_or_else(|| font(FONT_SERIF), |fonts| fonts.serif.clone())
}

pub fn mono_font() -> Font {
    RESOLVED
        .get()
        .map_or_else(|| font(FONT_MONO), |fonts| fonts.mono.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_priority_picks_installed_family_and_unicode_fallback() {
        let available = [
            "Noto Sans".into(),
            "Noto Sans CJK JP".into(),
            "DejaVu Sans".into(),
        ];
        let selected = resolve_font(FONT_SANS_FALLBACK, &available);
        assert_eq!(selected.family.as_ref(), "Noto Sans");
        assert!(
            selected
                .fallbacks
                .unwrap()
                .fallback_list()
                .contains(&"Noto Sans CJK JP".into())
        );
    }
}
