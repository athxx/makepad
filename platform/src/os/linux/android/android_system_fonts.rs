//! Resolve Android's default fonts at runtime by parsing the system font
//! config (`/system/etc/fonts.xml`, with a fallback to the legacy
//! `/system/etc/system_fonts.xml`) and reading the matched file from
//! `/system/fonts/`.
//!
//! These files are world-readable on Android, so plain `std::fs` works — no
//! AssetManager / JNI needed. We never bundle text fonts: glyph coverage is
//! delegated to whatever the device ships.

use crate::cx_api::{SystemFontQuery, SystemFontRole};

const FONTS_DIR: &str = "/system/fonts";
const FONTS_XML: &str = "/system/etc/fonts.xml";
const LEGACY_FONTS_XML: &str = "/system/etc/system_fonts.xml";

/// Resolve `query` to font file bytes by reading the Android font config.
pub fn load_system_font(query: &SystemFontQuery) -> Option<Vec<u8>> {
    let xml = std::fs::read_to_string(FONTS_XML)
        .or_else(|_| std::fs::read_to_string(LEGACY_FONTS_XML))
        .ok()?;
    let families = parse_families(&xml);

    let bold = query.weight >= 600;
    let want_weight = if bold { 700 } else { 400 };

    // Pick candidate families in priority order for the requested role.
    let candidates = role_family_candidates(query.role, &query.lang);

    // First try named-family matches in priority order; then, for CJK / emoji,
    // fall back to any family whose lang attribute matches.
    for name in &candidates {
        if let Some(file) = pick_font_in_family_named(&families, name, want_weight, query.italic) {
            if let Some(bytes) = read_font_file(&file) {
                return Some(bytes);
            }
        }
    }

    if let Some(langs) = role_lang_candidates(query.role, &query.lang) {
        for lang in langs {
            if let Some(file) = pick_font_in_family_by_lang(&families, lang, want_weight, query.italic)
            {
                if let Some(bytes) = read_font_file(&file) {
                    return Some(bytes);
                }
            }
        }
    }

    None
}

fn read_font_file(file: &str) -> Option<Vec<u8>> {
    // fonts.xml entries are bare filenames relative to /system/fonts.
    let path = if file.starts_with('/') {
        file.to_string()
    } else {
        format!("{}/{}", FONTS_DIR, file)
    };
    std::fs::read(&path).ok()
}

fn role_family_candidates(role: SystemFontRole, _lang: &str) -> Vec<&'static str> {
    match role {
        SystemFontRole::Ui => vec!["sans-serif", "roboto"],
        SystemFontRole::Serif => vec!["serif", "noto-serif"],
        SystemFontRole::Mono => vec!["monospace", "sans-serif-monospace"],
        // CJK / emoji are resolved via lang attributes below; named families
        // here are best-effort common names.
        SystemFontRole::Cjk => vec![],
        SystemFontRole::Emoji => vec![],
    }
}

fn role_lang_candidates(role: SystemFontRole, lang: &str) -> Option<Vec<&'static str>> {
    match role {
        SystemFontRole::Cjk => Some(match lang {
            "ja" => vec!["ja"],
            "ko" => vec!["ko"],
            // zh-Hans first, then generic CJK fallbacks.
            _ => vec!["zh-Hans", "zh-Hant", "zh"],
        }),
        // Android tags color emoji with the und-Zsye (Symbols, Emoji) script.
        SystemFontRole::Emoji => Some(vec!["und-Zsye", "und-Zsym"]),
        _ => None,
    }
}

#[derive(Default)]
struct FontEntry {
    file: String,
    weight: u32,
    italic: bool,
}

#[derive(Default)]
struct Family {
    name: Option<String>,
    lang: Option<String>,
    fonts: Vec<FontEntry>,
}

/// A deliberately tiny, dependency-free parser for the subset of the Android
/// fonts.xml schema we need: `<family [name=..] [lang=..]> <font weight=..
/// style=..>file</font>* </family>`.
fn parse_families(xml: &str) -> Vec<Family> {
    let mut families = Vec::new();
    let mut rest = xml;

    while let Some(start) = rest.find("<family") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else { break };
        let open_tag = &after[..open_end];
        let name = attr_value(open_tag, "name");
        let lang = attr_value(open_tag, "lang");

        // Body up to the closing </family>.
        let body_start = start + open_end + 1;
        let body_and_rest = &rest[body_start..];
        let (body, next) = match body_and_rest.find("</family>") {
            Some(close) => (
                &body_and_rest[..close],
                &body_and_rest[close + "</family>".len()..],
            ),
            None => (body_and_rest, ""),
        };

        let mut family = Family {
            name,
            lang,
            fonts: Vec::new(),
        };
        parse_fonts_into(body, &mut family.fonts);
        families.push(family);
        rest = next;
    }

    families
}

fn parse_fonts_into(body: &str, out: &mut Vec<FontEntry>) {
    let mut rest = body;
    while let Some(start) = rest.find("<font") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else { break };
        let open_tag = &after[..open_end];

        let weight = attr_value(open_tag, "weight")
            .and_then(|w| w.trim().parse::<u32>().ok())
            .unwrap_or(400);
        let italic = attr_value(open_tag, "style")
            .map(|s| s.trim().eq_ignore_ascii_case("italic"))
            .unwrap_or(false);

        let content_start = start + open_end + 1;
        let content_and_rest = &rest[content_start..];
        let (content, next) = match content_and_rest.find("</font>") {
            Some(close) => (
                &content_and_rest[..close],
                &content_and_rest[close + "</font>".len()..],
            ),
            None => (content_and_rest, ""),
        };

        let file = content.trim();
        if !file.is_empty() {
            out.push(FontEntry {
                file: file.to_string(),
                weight,
                italic,
            });
        }
        rest = next;
    }
}

/// Read the value of `attr="..."` (or `attr='...'`) from an opening-tag slice.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let key = format!("{}=", attr);
    let mut search = tag;
    while let Some(pos) = search.find(&key) {
        // Ensure the char before the key is a boundary (space or tag-open), so
        // `lang=` doesn't match inside e.g. `xlang=`.
        let prev_ok = pos == 0
            || search.as_bytes()[pos - 1].is_ascii_whitespace()
            || search.as_bytes()[pos - 1] == b'<';
        let after = &search[pos + key.len()..];
        if !prev_ok {
            search = after;
            continue;
        }
        let bytes = after.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let quote = bytes[0];
        if quote == b'"' || quote == b'\'' {
            let inner = &after[1..];
            if let Some(end) = inner.find(quote as char) {
                return Some(inner[..end].to_string());
            }
        }
        return None;
    }
    None
}

fn pick_font_in_family_named(
    families: &[Family],
    name: &str,
    want_weight: u32,
    want_italic: bool,
) -> Option<String> {
    families
        .iter()
        .filter(|f| {
            f.name
                .as_deref()
                .map(|n| n.eq_ignore_ascii_case(name))
                .unwrap_or(false)
        })
        .find_map(|f| pick_best(&f.fonts, want_weight, want_italic))
}

fn pick_font_in_family_by_lang(
    families: &[Family],
    lang: &str,
    want_weight: u32,
    want_italic: bool,
) -> Option<String> {
    families
        .iter()
        .filter(|f| {
            f.lang
                .as_deref()
                .map(|l| {
                    // lang attr may list several tags space-separated, and may
                    // be more specific (zh-Hans-CN) than our query (zh-Hans).
                    l.split([',', ' ']).any(|t| {
                        let t = t.trim();
                        !t.is_empty()
                            && (t.eq_ignore_ascii_case(lang)
                                || t.to_ascii_lowercase()
                                    .starts_with(&lang.to_ascii_lowercase()))
                    })
                })
                .unwrap_or(false)
        })
        .find_map(|f| pick_best(&f.fonts, want_weight, want_italic))
}

/// Choose the font entry closest to the requested weight, preferring a style
/// (italic/roman) match.
fn pick_best(fonts: &[FontEntry], want_weight: u32, want_italic: bool) -> Option<String> {
    fonts
        .iter()
        .min_by_key(|e| {
            let style_penalty = if e.italic == want_italic { 0 } else { 10_000 };
            let weight_penalty = (e.weight as i64 - want_weight as i64).unsigned_abs() as u64;
            style_penalty as u64 + weight_penalty
        })
        .map(|e| e.file.clone())
}
