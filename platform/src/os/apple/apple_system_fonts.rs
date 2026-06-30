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

    // A representative sample string per script, used to coax CoreText into
    // picking the right fallback font for CJK / emoji.
    let sample: Option<&str> = match query.role {
        SystemFontRole::Cjk => Some(match query.lang.as_str() {
            "ja" => "\u{3042}", // あ
            "ko" => "\u{ac00}", // 가
            _ => "\u{4e2d}",    // 中 (default zh)
        }),
        SystemFontRole::Emoji => Some("\u{1f600}"), // 😀
        _ => None,
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

    let bytes = font_url_bytes(font);
    CFRelease(font);
    bytes
}

/// Read the on-disk file for a CTFont via its `kCTFontURLAttribute`.
unsafe fn font_url_bytes(font: CTFontRef) -> Option<Vec<u8>> {
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
    std::fs::read(&path).ok()
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
