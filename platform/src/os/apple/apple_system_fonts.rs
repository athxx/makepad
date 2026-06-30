//! Resolve the platform's default fonts at runtime via CoreText.
//!
//! Shared by the macOS / iOS / tvOS backends. We never ask for a typeface by
//! name; instead we ask CoreText for the system font for a role (and, for CJK /
//! emoji, for a font that can render a representative character) and then read
//! the resolved font's on-disk file via `kCTFontURLAttribute`. This is how we
//! avoid bundling text fonts: glyph coverage is delegated to the OS.

use crate::cx_api::{SystemFontQuery, SystemFontRole};
use crate::os::apple::apple_sys::*;
use crate::os::apple::apple_util::cfstring_ref_to_string;
use std::ffi::c_void;

/// Resolve `query` to font file bytes using CoreText, or `None` on failure.
pub fn load_system_font(query: &SystemFontQuery) -> Option<Vec<u8>> {
    unsafe { load_system_font_inner(query) }
}

unsafe fn load_system_font_inner(query: &SystemFontQuery) -> Option<Vec<u8>> {
    let bold = query.weight >= 600;

    // A representative sample string used to coax CoreText into picking the
    // right covering font via its cascade list. An explicit `query.sample`
    // (per-glyph fallback: the actual uncovered characters) takes precedence;
    // otherwise we fall back to a per-role representative character for CJK /
    // emoji. Roles without a sample resolve the plain UI font.
    let sample: Option<&str> = if !query.sample.is_empty() {
        Some(query.sample.as_str())
    } else {
        match query.role {
            SystemFontRole::Cjk => Some(match query.lang.as_str() {
                "ja" => "\u{3042}", // あ
                "ko" => "\u{ac00}", // 가
                _ => "\u{4e2d}",    // 中 (default zh)
            }),
            SystemFontRole::Emoji => Some("\u{1f600}"), // 😀
            _ => None,
        }
    };

    // Base UI font. CoreText's UI font types give us the system default for the
    // primary script; for serif/mono we still start from the system font and
    // rely on the role mapping below.
    let ui_type = match query.role {
        SystemFontRole::Mono => kCTFontUIFontUserFixedPitch,
        _ if bold => kCTFontUIFontEmphasizedSystem,
        _ => kCTFontUIFontSystem,
    };

    let base_font = CTFontCreateUIFontForLanguage(ui_type, 0.0, std::ptr::null());
    if base_font.is_null() {
        return None;
    }

    // For CJK / emoji, ask CoreText for a font that can render the sample char,
    // starting from the base UI font (this triggers the cascade list).
    let font = if let Some(sample) = sample {
        let cf_sample = cfstring_from_str(sample);
        if cf_sample.is_null() {
            CFRelease(base_font);
            return None;
        }
        let len = CFStringGetLength(cf_sample);
        let range = CFRange {
            location: 0,
            length: len as u64,
        };
        let f = CTFontCreateForString(base_font, cf_sample, range);
        CFRelease(cf_sample as *const c_void);
        CFRelease(base_font);
        if f.is_null() {
            return None;
        }
        f
    } else {
        base_font
    };

    let path = font_url_path(font);
    CFRelease(font);

    // CoreText happily resolves the system UI/CJK fonts to Apple's proprietary
    // variable/format files (SFNS.ttf, PingFangUI.ttc). Those parse, but their
    // glyph outlines live in formats `ttf_parser` (our outline extractor) cannot
    // read, so every glyph would render as a blank box. When CoreText hands us
    // one of those, substitute a known-parseable system font for the same role.
    // (Color emoji uses `sbix` raster images, independent of outline extraction,
    // so it is left untouched.) See `unparseable` / `fallback_paths` below.
    let path = path?;
    if is_unparseable_system_font(&path) {
        if let Some(bytes) = load_parseable_fallback(query) {
            return Some(bytes);
        }
        // No fallback found on disk: fall through and return the original bytes
        // rather than nothing — better a tofu box than no font at all.
    }
    std::fs::read(&path).ok()
}

/// Apple's proprietary system fonts whose outlines `ttf_parser` cannot extract.
/// Matched by file name so this is robust to the exact directory CoreText returns.
fn is_unparseable_system_font(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    // SFNS.ttf / SFNSRounded.ttf etc. (SF Pro family) and PingFangUI.ttc.
    name.starts_with("SFNS") || name.starts_with("PingFang")
}

/// Known-parseable system fonts to use in place of the unparseable defaults,
/// one ordered candidate list per role. The first file that exists on disk and
/// reads successfully wins. All entries are verified to expose `ttf_parser`
/// outlines at face index 0 (the index our `FontFace` loader uses).
fn load_parseable_fallback(query: &SystemFontQuery) -> Option<Vec<u8>> {
    const UI: &[&str] = &[
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/HelveticaNeue.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];
    const SERIF: &[&str] = &[
        "/System/Library/Fonts/Times.ttc",
        "/System/Library/Fonts/Supplemental/Times New Roman.ttf",
    ];
    const MONO: &[&str] = &[
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
        "/System/Library/Fonts/Courier.ttc",
    ];
    // CJK: Hiragino covers Han + Hiragana/Katakana; Korean prefers AppleSDGothicNeo.
    const CJK_JA_ZH: &[&str] = &[
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
    ];
    const CJK_KO: &[&str] = &[
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];

    let candidates: &[&str] = match query.role {
        SystemFontRole::Serif => SERIF,
        SystemFontRole::Mono => MONO,
        SystemFontRole::Cjk => {
            if query.lang == "ko" {
                CJK_KO
            } else {
                CJK_JA_ZH
            }
        }
        // Ui / Emoji and anything else fall back to the sans UI list. (Emoji
        // never reaches here — it isn't flagged as unparseable.)
        _ => UI,
    };

    candidates.iter().find_map(|path| std::fs::read(path).ok())
}

/// Read the on-disk file path for a CTFont via its `kCTFontURLAttribute`.
unsafe fn font_url_path(font: CTFontRef) -> Option<String> {
    let url = CTFontCopyAttribute(font, kCTFontURLAttribute);
    if url.is_null() {
        return None;
    }
    // kCFURLPOSIXPathStyle == 0
    let cf_path = CFURLCopyFileSystemPath(url, 0);
    CFRelease(url);
    if cf_path.is_null() {
        return None;
    }
    let path = cfstring_ref_to_string(cf_path);
    CFRelease(cf_path as *const c_void);
    if path.is_empty() {
        return None;
    }
    Some(path)
}

/// Build a CFString from a Rust &str (caller must CFRelease).
unsafe fn cfstring_from_str(s: &str) -> CFStringRef {
    CFStringCreateWithBytes(
        std::ptr::null(),
        s.as_ptr(),
        s.len() as CFIndex,
        kCFStringEncodingUTF8,
        0,
    )
}
