// Cross-platform high-performance JSON implementation.
//
// Compile-time/runtime dispatch:
// - x86/x86_64: AVX2 -> SSE2 -> native-word SWAR.
// - AArch64/ARM64EC with target_feature="neon": NEON -> SWAR.
// - ARM32: optional nightly NEON when both target_feature="neon" and the
//   Cargo feature "nightly-arm-neon" are enabled; otherwise SWAR.
// - Every other std-capable target: endian-neutral 16/32/64-bit SWAR.
//
// ARM32 nightly NEON also requires this in the crate root:
// #![cfg_attr(
//     all(target_arch = "arm", feature = "nightly-arm-neon"),
//     feature(stdarch_arm_neon_intrinsics)
// )]
// and `[features] nightly-arm-neon = []` in Cargo.toml.

use viso_vid::VID;
use std::collections::HashMap;
use std::hash::Hash;
use std::str::Chars;

use std::borrow::Cow;
use std::fmt::Write as FmtWrite;

const DIGIT_TABLE: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

#[inline(always)]
fn append_utf8_unchecked(out: &mut String, bytes: &[u8]) {
    debug_assert!(std::str::from_utf8(bytes).is_ok());
    // SAFETY: every caller passes either bytes from an existing UTF-8 string or ASCII digits.
    unsafe { out.push_str(std::str::from_utf8_unchecked(bytes)) };
}

#[inline]
fn write_u64(out: &mut String, mut value: u64) {
    let mut buf = [0u8; 20];
    let mut cursor = buf.len();

    while value >= 100 {
        let next = value / 100;
        let pair = ((value - next * 100) as usize) * 2;
        cursor -= 2;
        buf[cursor] = DIGIT_TABLE[pair];
        buf[cursor + 1] = DIGIT_TABLE[pair + 1];
        value = next;
    }

    if value < 10 {
        cursor -= 1;
        buf[cursor] = b'0' + value as u8;
    } else {
        let pair = value as usize * 2;
        cursor -= 2;
        buf[cursor] = DIGIT_TABLE[pair];
        buf[cursor + 1] = DIGIT_TABLE[pair + 1];
    }

    append_utf8_unchecked(out, &buf[cursor..]);
}

#[inline]
fn write_u64_fixed_19(out: &mut String, mut value: u64) {
    debug_assert!(value < 10_000_000_000_000_000_000u64);
    let mut buf = [b'0'; 19];
    let mut cursor = buf.len();
    while cursor > 1 {
        let next = value / 100;
        let pair = ((value - next * 100) as usize) * 2;
        cursor -= 2;
        buf[cursor] = DIGIT_TABLE[pair];
        buf[cursor + 1] = DIGIT_TABLE[pair + 1];
        value = next;
    }
    debug_assert!(value < 10);
    buf[0] = b'0' + value as u8;
    append_utf8_unchecked(out, &buf);
}

#[inline]
fn write_u128(out: &mut String, value: u128) {
    if value <= u64::MAX as u128 {
        write_u64(out, value as u64);
        return;
    }

    // Splitting into base-10^19 limbs reduces the worst case from nineteen
    // 128-bit divisions to at most two; each limb then uses cheap u64 divisions.
    const BASE: u128 = 10_000_000_000_000_000_000u128;
    let upper = value / BASE;
    let low = (value - upper * BASE) as u64;
    if upper <= u64::MAX as u128 {
        write_u64(out, upper as u64);
    } else {
        let high = upper / BASE;
        let middle = (upper - high * BASE) as u64;
        write_u64(out, high as u64);
        write_u64_fixed_19(out, middle);
    }
    write_u64_fixed_19(out, low);
}

#[inline]
fn write_i64(out: &mut String, value: i64) {
    if value < 0 {
        out.push('-');
        write_u64(out, value.wrapping_neg() as u64);
    } else {
        write_u64(out, value as u64);
    }
}

#[inline]
fn write_i128(out: &mut String, value: i128) {
    if value < 0 {
        out.push('-');
        write_u128(out, value.wrapping_neg() as u128);
    } else {
        write_u128(out, value as u128);
    }
}

#[inline]
fn parse_u128_digit_parts(first: Option<u8>, tail: &[u8]) -> Option<u128> {
    let mut saw_digit = false;
    let mut significant_digits = 0usize;
    let mut small = 0u64;
    let mut wide = 0u128;

    macro_rules! push_digit {
        ($digit:expr) => {{
            let digit = $digit;
            debug_assert!(digit.is_ascii_digit());
            saw_digit = true;
            if significant_digits == 0 && digit == b'0' {
                // Ignore arbitrary leading zeroes without making huge but valid
                // compatibility-mode integers overflow solely because of length.
            } else {
                significant_digits += 1;
                match significant_digits {
                    1..=19 => {
                        small = small * 10 + (digit - b'0') as u64;
                    }
                    20 => {
                        wide = small as u128 * 10 + (digit - b'0') as u128;
                    }
                    21..=38 => {
                        wide = wide * 10 + (digit - b'0') as u128;
                    }
                    39 => {
                        wide = wide
                            .checked_mul(10)?
                            .checked_add((digit - b'0') as u128)?;
                    }
                    _ => return None,
                }
            }
        }};
    }

    if let Some(digit) = first {
        push_digit!(digit);
    }
    for &digit in tail {
        push_digit!(digit);
    }

    if !saw_digit {
        None
    } else if significant_digits == 0 {
        Some(0)
    } else if significant_digits <= 19 {
        Some(small as u128)
    } else {
        Some(wide)
    }
}

#[inline]
fn parse_u128_digits(digits: &[u8]) -> Option<u128> {
    parse_u128_digit_parts(None, digits)
}

#[inline(always)]
fn is_string_special(byte: u8) -> bool {
    byte == b'"' || byte == b'\\' || byte < 0x20
}

#[inline]
fn find_string_special_scalar(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&byte| is_string_special(byte))
}

// Portable byte-parallel fallback. This processes one native machine word at a
// time and is used on every architecture that has no active hardware SIMD path.
// It is endian-neutral and only performs unaligned reads inside the slice.
#[inline(always)]
const fn swar_repeat_byte(byte: u8) -> usize {
    (usize::MAX / 0xff) * byte as usize
}

const SWAR_HIGH_BITS: usize = swar_repeat_byte(0x80);
const SWAR_LOW_7_BITS: usize = swar_repeat_byte(0x7f);
const SWAR_HIGH_3_BITS: usize = swar_repeat_byte(0xe0);

/// Returns bit 7 in every byte lane whose input byte is exactly zero.
/// Unlike the subtraction-based `has_zero_byte` trick, this form produces an
/// exact per-lane mask, so it can also be used to locate the first matching byte.
#[inline(always)]
fn swar_zero_byte_high_bits(value: usize) -> usize {
    !(((value & SWAR_LOW_7_BITS).wrapping_add(SWAR_LOW_7_BITS))
        | value
        | SWAR_LOW_7_BITS)
        & SWAR_HIGH_BITS
}

#[inline(always)]
fn swar_byte_eq_high_bits(value: usize, byte: u8) -> usize {
    swar_zero_byte_high_bits(value ^ swar_repeat_byte(byte))
}

#[inline(always)]
fn swar_first_marked_byte(mask: usize) -> usize {
    debug_assert_ne!(mask, 0);
    #[cfg(target_endian = "little")]
    {
        (mask.trailing_zeros() as usize) >> 3
    }
    #[cfg(target_endian = "big")]
    {
        (mask.leading_zeros() as usize) >> 3
    }
}

#[inline]
fn find_string_special_swar(bytes: &[u8]) -> Option<usize> {
    const WIDTH: usize = std::mem::size_of::<usize>();
    let mut offset = 0usize;

    while offset + WIDTH <= bytes.len() {
        // SAFETY: the loop proves a full native word is inside `bytes` and
        // `read_unaligned` imposes no alignment requirement.
        let word = unsafe {
            (bytes.as_ptr().add(offset) as *const usize).read_unaligned()
        };
        let mask = swar_byte_eq_high_bits(word, b'"')
            | swar_byte_eq_high_bits(word, b'\\')
            | swar_zero_byte_high_bits(word & SWAR_HIGH_3_BITS);
        if mask != 0 {
            return Some(offset + swar_first_marked_byte(mask));
        }
        offset += WIDTH;
    }

    find_string_special_scalar(&bytes[offset..]).map(|index| offset + index)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn find_string_special_sse2(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    // SAFETY: the caller guarantees SSE2 support; all loads are explicitly
    // unaligned and guarded by the remaining-length check.
    unsafe {
        let quote = _mm_set1_epi8(b'"' as i8);
        let slash = _mm_set1_epi8(b'\\' as i8);
        let sign = _mm_set1_epi8(i8::MIN);
        let control_limit = _mm_set1_epi8((0x20u8 ^ 0x80) as i8);
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = _mm_loadu_si128(bytes.as_ptr().add(offset) as *const __m128i);
            let quote_mask = _mm_cmpeq_epi8(value, quote);
            let slash_mask = _mm_cmpeq_epi8(value, slash);
            let biased = _mm_xor_si128(value, sign);
            let control_mask = _mm_cmpgt_epi8(control_limit, biased);
            let mask = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(quote_mask, slash_mask),
                control_mask,
            )) as u32;
            if mask != 0 {
                return Some(offset + mask.trailing_zeros() as usize);
            }
            offset += 16;
        }

        find_string_special_swar(&bytes[offset..]).map(|index| offset + index)
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn find_string_special_avx2(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    // SAFETY: the caller guarantees AVX2 support; every vector load is guarded
    // by the chunk-length condition and is explicitly unaligned.
    unsafe {
        let quote = _mm256_set1_epi8(b'"' as i8);
        let slash = _mm256_set1_epi8(b'\\' as i8);
        let sign = _mm256_set1_epi8(i8::MIN);
        let control_limit = _mm256_set1_epi8((0x20u8 ^ 0x80) as i8);
        let mut offset = 0usize;

        while offset + 32 <= bytes.len() {
            let value = _mm256_loadu_si256(bytes.as_ptr().add(offset) as *const __m256i);
            let quote_mask = _mm256_cmpeq_epi8(value, quote);
            let slash_mask = _mm256_cmpeq_epi8(value, slash);
            let biased = _mm256_xor_si256(value, sign);
            let control_mask = _mm256_cmpgt_epi8(control_limit, biased);
            let mask = _mm256_movemask_epi8(_mm256_or_si256(
                _mm256_or_si256(quote_mask, slash_mask),
                control_mask,
            )) as u32;
            if mask != 0 {
                return Some(offset + mask.trailing_zeros() as usize);
            }
            offset += 32;
        }

        find_string_special_sse2(&bytes[offset..]).map(|index| offset + index)
    }
}

// AArch64/Apple Silicon and Apple arm64_32 targets compile this branch only
// when the target ABI advertises NEON. Keeping it behind `target_feature`
// avoids enabling NEON inside soft-float AArch64 ABIs, where doing so is unsound.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "arm64ec"),
    target_feature = "neon"
))]
unsafe fn find_string_special_neon_aarch64(bytes: &[u8]) -> Option<usize> {
    use std::arch::aarch64::*;

    // SAFETY: this function is only compiled for targets with NEON enabled and
    // every vector access has a full 16-byte backing region.
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let control_limit = vdupq_n_u8(0x20);
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = vld1q_u8(bytes.as_ptr().add(offset));
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(value, quote), vceqq_u8(value, slash)),
                vcltq_u8(value, control_limit),
            );
            if vmaxvq_u8(mask) != 0 {
                let mut lanes = [0u8; 16];
                vst1q_u8(lanes.as_mut_ptr(), mask);
                return lanes
                    .iter()
                    .position(|&lane| lane != 0)
                    .map(|index| offset + index);
            }
            offset += 16;
        }

        find_string_special_swar(&bytes[offset..]).map(|index| offset + index)
    }
}

// Rust stable still does not expose ARM32 NEON intrinsics. This optional branch
// is therefore compiled only when the crate explicitly opts into nightly ARM
// stdarch support AND the target itself is built with `+neon`.
#[cfg(all(
    target_arch = "arm",
    target_feature = "neon",
    feature = "nightly-arm-neon"
))]
unsafe fn find_string_special_neon_arm32(bytes: &[u8]) -> Option<usize> {
    use std::arch::arm::*;

    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let control_limit = vdupq_n_u8(0x20);
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = vld1q_u8(bytes.as_ptr().add(offset));
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(value, quote), vceqq_u8(value, slash)),
                vcltq_u8(value, control_limit),
            );
            let mut lanes = [0u8; 16];
            vst1q_u8(lanes.as_mut_ptr(), mask);
            let low = (lanes.as_ptr() as *const u64).read_unaligned();
            let high = (lanes.as_ptr().add(8) as *const u64).read_unaligned();
            if low | high != 0 {
                return lanes
                    .iter()
                    .position(|&lane| lane != 0)
                    .map(|index| offset + index);
            }
            offset += 16;
        }

        find_string_special_swar(&bytes[offset..]).map(|index| offset + index)
    }
}

#[inline]
fn find_string_special(bytes: &[u8]) -> Option<usize> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if bytes.len() >= 64 && std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 support was checked immediately above.
            return unsafe { find_string_special_avx2(bytes) };
        }
        if bytes.len() >= 32 && std::arch::is_x86_feature_detected!("sse2") {
            // SAFETY: SSE2 support was checked immediately above. It is part of
            // the x86_64 baseline and dynamically detected on 32-bit x86.
            return unsafe { find_string_special_sse2(bytes) };
        }
    }

    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "arm64ec"),
        target_feature = "neon"
    ))]
    {
        if bytes.len() >= 32 {
            // SAFETY: this branch exists only in a NEON-enabled target build.
            return unsafe { find_string_special_neon_aarch64(bytes) };
        }
    }

    #[cfg(all(
        target_arch = "arm",
        target_feature = "neon",
        feature = "nightly-arm-neon"
    ))]
    {
        if bytes.len() >= 32 {
            // SAFETY: the Cargo feature and target feature jointly guarantee
            // nightly ARM stdarch support and a NEON-capable deployment target.
            return unsafe { find_string_special_neon_arm32(bytes) };
        }
    }

    find_string_special_swar(bytes)
}

#[inline(always)]
fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\n' | b'\r' | b'\t')
}

#[inline]
fn skip_json_whitespace_scalar(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|&byte| !is_json_whitespace(byte))
        .unwrap_or(bytes.len())
}

#[inline]
fn skip_json_whitespace_swar(bytes: &[u8]) -> usize {
    const WIDTH: usize = std::mem::size_of::<usize>();
    let mut offset = 0usize;

    while offset + WIDTH <= bytes.len() {
        // SAFETY: a complete native word remains in the input slice.
        let word = unsafe {
            (bytes.as_ptr().add(offset) as *const usize).read_unaligned()
        };
        let whitespace_mask = swar_byte_eq_high_bits(word, b' ')
            | swar_byte_eq_high_bits(word, b'\n')
            | swar_byte_eq_high_bits(word, b'\r')
            | swar_byte_eq_high_bits(word, b'\t');
        if whitespace_mask != SWAR_HIGH_BITS {
            let non_whitespace_mask = (!whitespace_mask) & SWAR_HIGH_BITS;
            return offset + swar_first_marked_byte(non_whitespace_mask);
        }
        offset += WIDTH;
    }

    offset + skip_json_whitespace_scalar(&bytes[offset..])
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "sse2")]
unsafe fn skip_json_whitespace_sse2(bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    unsafe {
        let space = _mm_set1_epi8(b' ' as i8);
        let newline = _mm_set1_epi8(b'\n' as i8);
        let carriage = _mm_set1_epi8(b'\r' as i8);
        let tab = _mm_set1_epi8(b'\t' as i8);
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = _mm_loadu_si128(bytes.as_ptr().add(offset) as *const __m128i);
            let mask = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(value, space),
                    _mm_cmpeq_epi8(value, newline),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(value, carriage),
                    _mm_cmpeq_epi8(value, tab),
                ),
            )) as u32;
            if mask != 0xffff {
                return offset + ((!mask) & 0xffff).trailing_zeros() as usize;
            }
            offset += 16;
        }

        offset + skip_json_whitespace_swar(&bytes[offset..])
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn skip_json_whitespace_avx2(bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    unsafe {
        let space = _mm256_set1_epi8(b' ' as i8);
        let newline = _mm256_set1_epi8(b'\n' as i8);
        let carriage = _mm256_set1_epi8(b'\r' as i8);
        let tab = _mm256_set1_epi8(b'\t' as i8);
        let mut offset = 0usize;

        while offset + 32 <= bytes.len() {
            let value = _mm256_loadu_si256(bytes.as_ptr().add(offset) as *const __m256i);
            let mask = _mm256_movemask_epi8(_mm256_or_si256(
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(value, space),
                    _mm256_cmpeq_epi8(value, newline),
                ),
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(value, carriage),
                    _mm256_cmpeq_epi8(value, tab),
                ),
            )) as u32;
            if mask != u32::MAX {
                return offset + (!mask).trailing_zeros() as usize;
            }
            offset += 32;
        }

        offset + skip_json_whitespace_sse2(&bytes[offset..])
    }
}

#[cfg(all(
    any(target_arch = "aarch64", target_arch = "arm64ec"),
    target_feature = "neon"
))]
unsafe fn skip_json_whitespace_neon_aarch64(bytes: &[u8]) -> usize {
    use std::arch::aarch64::*;

    unsafe {
        let space = vdupq_n_u8(b' ');
        let newline = vdupq_n_u8(b'\n');
        let carriage = vdupq_n_u8(b'\r');
        let tab = vdupq_n_u8(b'\t');
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = vld1q_u8(bytes.as_ptr().add(offset));
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(value, space), vceqq_u8(value, newline)),
                vorrq_u8(vceqq_u8(value, carriage), vceqq_u8(value, tab)),
            );
            if vminvq_u8(mask) != u8::MAX {
                let mut lanes = [0u8; 16];
                vst1q_u8(lanes.as_mut_ptr(), mask);
                return offset + lanes.iter().position(|&lane| lane == 0).unwrap();
            }
            offset += 16;
        }

        offset + skip_json_whitespace_swar(&bytes[offset..])
    }
}

#[cfg(all(
    target_arch = "arm",
    target_feature = "neon",
    feature = "nightly-arm-neon"
))]
unsafe fn skip_json_whitespace_neon_arm32(bytes: &[u8]) -> usize {
    use std::arch::arm::*;

    unsafe {
        let space = vdupq_n_u8(b' ');
        let newline = vdupq_n_u8(b'\n');
        let carriage = vdupq_n_u8(b'\r');
        let tab = vdupq_n_u8(b'\t');
        let mut offset = 0usize;

        while offset + 16 <= bytes.len() {
            let value = vld1q_u8(bytes.as_ptr().add(offset));
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(value, space), vceqq_u8(value, newline)),
                vorrq_u8(vceqq_u8(value, carriage), vceqq_u8(value, tab)),
            );
            let mut lanes = [0u8; 16];
            vst1q_u8(lanes.as_mut_ptr(), mask);
            let low = (lanes.as_ptr() as *const u64).read_unaligned();
            let high = (lanes.as_ptr().add(8) as *const u64).read_unaligned();
            if low != u64::MAX || high != u64::MAX {
                return offset + lanes.iter().position(|&lane| lane == 0).unwrap();
            }
            offset += 16;
        }

        offset + skip_json_whitespace_swar(&bytes[offset..])
    }
}

#[inline]
fn skip_json_whitespace(bytes: &[u8]) -> usize {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if bytes.len() >= 64 && std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 support was checked immediately above.
            return unsafe { skip_json_whitespace_avx2(bytes) };
        }
        if bytes.len() >= 32 && std::arch::is_x86_feature_detected!("sse2") {
            // SAFETY: SSE2 support was checked immediately above.
            return unsafe { skip_json_whitespace_sse2(bytes) };
        }
    }

    #[cfg(all(
        any(target_arch = "aarch64", target_arch = "arm64ec"),
        target_feature = "neon"
    ))]
    {
        if bytes.len() >= 32 {
            // SAFETY: this code is only present in a NEON-enabled build.
            return unsafe { skip_json_whitespace_neon_aarch64(bytes) };
        }
    }

    #[cfg(all(
        target_arch = "arm",
        target_feature = "neon",
        feature = "nightly-arm-neon"
    ))]
    {
        if bytes.len() >= 32 {
            // SAFETY: enabled only for a nightly, NEON-targeted ARM32 build.
            return unsafe { skip_json_whitespace_neon_arm32(bytes) };
        }
    }

    skip_json_whitespace_swar(bytes)
}

#[inline]
fn write_json_string(out: &mut String, value: &str) {
    out.reserve(value.len().saturating_add(2));
    out.push('"');
    let bytes = value.as_bytes();
    let mut start = 0usize;

    while let Some(relative) = find_string_special(&bytes[start..]) {
        let special = start + relative;
        append_utf8_unchecked(out, &bytes[start..special]);
        match bytes[special] {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0 => out.push_str("\\u0000"),
            byte => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let escaped = [b'\\', b'u', b'0', b'0', HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]];
                append_utf8_unchecked(out, &escaped);
            }
        }
        start = special + 1;
    }

    append_utf8_unchecked(out, &bytes[start..]);
    out.push('"');
}

/// Resets a `Chars` iterator to a byte suffix without decoding the skipped ASCII run.
#[inline(always)]
fn reset_chars_to_suffix<'a>(i: &mut Chars<'a>, offset: usize) {
    // `Chars::as_str` returns a slice with the original input lifetime, so the
    // iterator can be safely replaced with a suffix of that same slice.
    let rest: &'a str = i.as_str();
    debug_assert!(offset <= rest.len());
    debug_assert!(rest.is_char_boundary(offset));
    // SAFETY: all callers derive `offset` by scanning ASCII bytes or a known
    // UTF-8 boundary in this exact `rest` slice.
    let suffix: &'a str = unsafe { rest.get_unchecked(offset..) };
    *i = suffix.chars();
}


pub struct EnJsonState {
    pub out: String,
}

impl EnJsonState {
    #[inline(always)]
    pub fn indent(&mut self, _d: usize) {
        //for _ in 0..d {
        //    self.out.push_str("    ");
        //}
    }

    #[inline(always)]
    pub fn field(&mut self, d: usize, field: &str) {
        self.indent(d);
        // Derived field names are Rust identifiers and therefore need no escape scan.
        self.out.push('"');
        self.out.push_str(field);
        self.out.push_str("\":");
    }

    #[inline(always)]
    pub fn label(&mut self, label: &str) {
        // Preserve the original fast path for generated enum/variant labels.
        self.out.push('"');
        self.out.push_str(label);
        self.out.push('"');
    }

    #[inline(always)]
    pub fn conl(&mut self) {
        self.out.push(',')
    }

    #[inline(always)]
    pub fn st_pre(&mut self) {
        self.out.push('{');
    }

    #[inline(always)]
    pub fn st_post(&mut self, d: usize) {
        self.indent(d);
        self.out.push('}');
    }
}

pub trait EnJson {
    #[inline]
    fn encode_json(&self) -> String {
        self.encode_json_with_capacity(128)
    }

    #[inline]
    fn encode_json_with_capacity(&self, capacity: usize) -> String {
        let mut s = EnJsonState {
            out: String::with_capacity(capacity),
        };
        self.en_json(0, &mut s);
        s.out
    }

    /// encodes into an existing buffer and reuses its allocation.
    #[inline]
    fn encode_json_to(&self, out: &mut String) {
        let mut s = EnJsonState {
            out: std::mem::take(out),
        };
        s.out.clear();
        self.en_json(0, &mut s);
        *out = s.out;
    }

    fn en_json(&self, d: usize, s: &mut EnJsonState);
}

pub trait DeJson: Sized {
    #[inline]
    fn decode_json(input: &str) -> Result<Self, DeJsonErr> {
        let mut state = DeJsonState::default();
        let mut chars = input.chars();
        state.next(&mut chars);
        state.next_tok(&mut chars)?;
        DeJson::de_json(&mut state, &mut chars)
    }

    #[inline]
    fn decode_json_lenient(input: &str) -> Result<Self, DeJsonErr> {
        let mut state = DeJsonState::default();
        state.lenient = true;
        let mut chars = input.chars();
        state.next(&mut chars);
        state.next_tok(&mut chars)?;
        DeJson::de_json(&mut state, &mut chars)
    }

    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Self, DeJsonErr>;
}

#[derive(PartialEq, Debug, Default)]
pub enum DeJsonTok {
    Str,
    Char(char),
    U64(u64),
    U128(u128),
    I64(i64),
    I128(i128),
    F64(f64),
    Bool(bool),
    BareIdent,
    Null,
    Colon,
    CurlyOpen,
    CurlyClose,
    BlockOpen,
    BlockClose,
    Comma,
    #[default]
    Bof,
    Eof,
}

#[derive(Default)]
pub struct DeJsonState {
    pub cur: char,
    pub tok: DeJsonTok,
    pub strbuf: String,
    pub numbuf: String,
    pub identbuf: String,
    pub line: usize,
    pub col: usize,
    pub lenient: bool,
}

pub struct DeJsonErr {
    pub msg: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Debug for DeJsonErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Json Decode error: {}, line:{} col:{}",
            self.msg,
            self.line + 1,
            self.col + 1
        )
    }
}

impl DeJsonState {
    #[inline]
    pub fn next(&mut self, i: &mut Chars) {
        if let Some(c) = i.next() {
            self.cur = c;
            if self.cur == '\n' {
                self.line += 1;
                self.col = 0;
            } else {
                self.col += 1;
            }
        } else {
            self.cur = '\0';
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_exp(&self, name: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Unexpected key {}", name),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_msg(&self, msg: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("{}", msg),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_nf(&self, name: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Key not found {}", name),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_enum(&self, name: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Enum not defined {}", name),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_token(&self, what: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Unexpected token {:?} expected {} ", self.tok, what),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_range(&self, what: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Value out of range {} ", what),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_type(&self, what: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Token wrong type {} ", what),
            line: self.line,
            col: self.col,
        }
    }

    #[cold]
    #[inline(never)]
    pub fn err_parse(&self, what: &str) -> DeJsonErr {
        DeJsonErr {
            msg: format!("Cannot parse {} ", what),
            line: self.line,
            col: self.col,
        }
    }

    #[inline]
    pub fn eat_comma_block(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        match self.tok {
            DeJsonTok::Comma => {
                self.next_tok(i)?;
                Ok(())
            }
            DeJsonTok::BlockClose => Ok(()),
            _ => Err(self.err_token(", or ]")),
        }
    }

    #[inline]
    pub fn eat_comma_curly(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        match self.tok {
            DeJsonTok::Comma => {
                self.next_tok(i)?;
                Ok(())
            }
            DeJsonTok::CurlyClose => Ok(()),
            _ => Err(self.err_token(", or }")),
        }
    }

    #[inline]
    pub fn colon(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        match self.tok {
            DeJsonTok::Colon => {
                self.next_tok(i)?;
                Ok(())
            }
            _ => Err(self.err_token(":")),
        }
    }

    #[inline]
    pub fn string(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        match &mut self.tok {
            DeJsonTok::Str => {
                self.next_tok(i)?;
                Ok(())
            }
            _ => Err(self.err_token("String")),
        }
    }

    #[inline]
    pub fn next_colon(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        self.next_tok(i)?;
        self.colon(i)?;
        Ok(())
    }

    #[inline]
    pub fn next_str(&mut self) -> Option<()> {
        if let DeJsonTok::Str = &mut self.tok {
            //let mut s = String::new();
            //std::mem::swap(&mut s, name);
            Some(())
        } else {
            None
        }
    }

    #[inline]
    pub fn block_open(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        if self.tok == DeJsonTok::BlockOpen {
            self.next_tok(i)?;
            return Ok(());
        }
        Err(self.err_token("["))
    }

    #[inline]
    pub fn block_close(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        if self.tok == DeJsonTok::BlockClose {
            self.next_tok(i)?;
            return Ok(());
        }
        Err(self.err_token("]"))
    }

    #[inline]
    pub fn curly_open(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        if self.tok == DeJsonTok::CurlyOpen {
            self.next_tok(i)?;
            return Ok(());
        }
        Err(self.err_token("{"))
    }

    #[inline]
    pub fn curly_close(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        if self.tok == DeJsonTok::CurlyClose {
            self.next_tok(i)?;
            return Ok(());
        }
        Err(self.err_token("}"))
    }

    #[inline]
    pub fn u64_range(&mut self, max: u64) -> Result<u64, DeJsonErr> {
        let value = self.u128_range(max as u128)?;
        Ok(value as u64)
    }

    #[inline]
    pub fn u128_range(&mut self, max: u128) -> Result<u128, DeJsonErr> {
        let value = match self.tok {
            DeJsonTok::U64(value) => value as u128,
            DeJsonTok::U128(value) => value,
            _ => return Err(self.err_token("unsigned integer")),
        };
        if value > max {
            return Err(self.err_range(&format!("{}>{}", value, max)));
        }
        Ok(value)
    }

    #[inline]
    pub fn i64_range(&mut self, min: i64, max: i64) -> Result<i64, DeJsonErr> {
        let value = self.i128_range(min as i128, max as i128)?;
        Ok(value as i64)
    }

    #[inline]
    pub fn i128_range(&mut self, min: i128, max: i128) -> Result<i128, DeJsonErr> {
        let value = match self.tok {
            DeJsonTok::I64(value) => value as i128,
            DeJsonTok::I128(value) => value,
            DeJsonTok::U64(value) => value as i128,
            DeJsonTok::U128(value) => {
                if value > i128::MAX as u128 {
                    return Err(self.err_range(&format!("{}>{}", value, max)));
                }
                value as i128
            }
            _ => return Err(self.err_token("signed integer")),
        };
        if value < min {
            return Err(self.err_range(&format!("{}<{}", value, min)));
        }
        if value > max {
            return Err(self.err_range(&format!("{}>{}", value, max)));
        }
        Ok(value)
    }

    #[inline]
    pub fn as_f64(&mut self) -> Result<f64, DeJsonErr> {
        if let DeJsonTok::I128(value) = self.tok {
            return Ok(value as f64);
        }
        if let DeJsonTok::U128(value) = self.tok {
            return Ok(value as f64);
        }
        if let DeJsonTok::I64(value) = self.tok {
            return Ok(value as f64);
        }
        if let DeJsonTok::U64(value) = self.tok {
            return Ok(value as f64);
        }
        if let DeJsonTok::F64(value) = self.tok {
            return Ok(value);
        }
        Err(self.err_token("floating point"))
    }

    #[inline]
    pub fn as_bool(&mut self) -> Result<bool, DeJsonErr> {
        if let DeJsonTok::Bool(value) = self.tok {
            return Ok(value);
        }
        Err(self.err_token("boolean"))
    }

    #[inline]
    pub fn as_string(&mut self) -> Result<String, DeJsonErr> {
        if let DeJsonTok::Str = &mut self.tok {
            let mut val = String::new();
            std::mem::swap(&mut val, &mut self.strbuf);
            return Ok(val);
        }
        Err(self.err_token("string"))
    }

    #[inline]
    pub fn as_ident(&mut self) -> Result<String, DeJsonErr> {
        if let DeJsonTok::BareIdent = &mut self.tok {
            let mut val = String::new();
            std::mem::swap(&mut val, &mut self.identbuf);
            return Ok(val);
        }
        Err(self.err_token("ident"))
    }

    pub fn skip_value(&mut self, i: &mut Chars) -> Result<(), DeJsonErr> {
        match self.tok {
            DeJsonTok::Str
            | DeJsonTok::U64(_)
            | DeJsonTok::U128(_)
            | DeJsonTok::I64(_)
            | DeJsonTok::I128(_)
            | DeJsonTok::F64(_)
            | DeJsonTok::Bool(_)
            | DeJsonTok::BareIdent
            | DeJsonTok::Null => {
                self.next_tok(i)?;
                Ok(())
            }
            DeJsonTok::CurlyOpen => {
                self.curly_open(i)?;
                while self.tok != DeJsonTok::CurlyClose {
                    self.string(i)?;
                    self.colon(i)?;
                    self.skip_value(i)?;
                    self.eat_comma_curly(i)?;
                }
                self.curly_close(i)?;
                Ok(())
            }
            DeJsonTok::BlockOpen => {
                self.block_open(i)?;
                while self.tok != DeJsonTok::BlockClose {
                    self.skip_value(i)?;
                    self.eat_comma_block(i)?;
                }
                self.block_close(i)?;
                Ok(())
            }
            _ => Err(self.err_token("value to skip")),
        }
    }

    #[inline]
    fn advance_position_ascii_bytes(&mut self, bytes: &[u8]) {
        if !bytes.contains(&b'\n') {
            self.col += bytes.len();
            return;
        }
        for &byte in bytes {
            if byte == b'\n' {
                self.line += 1;
                self.col = 0;
            } else {
                self.col += 1;
            }
        }
    }

    #[inline]
    fn advance_position_str(&mut self, value: &str) {
        // String chunks are cut before every control byte, so they cannot contain
        // a newline. Only UTF-8 character counting is needed here.
        self.col += if value.is_ascii() {
            value.len()
        } else {
            value.chars().count()
        };
    }


    #[inline]
    fn parse_number_fast(&mut self, i: &mut Chars<'_>) -> Result<(), DeJsonErr> {
        let first = self.cur as u8;
        let rest = i.as_str();
        let bytes = rest.as_bytes();
        let negative = first == b'-';
        let mut cursor = 0usize;

        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let integer_tail_end = cursor;

        let mut is_float = false;
        if cursor < bytes.len() && bytes[cursor] == b'.' {
            is_float = true;
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
        }

        if cursor < bytes.len() && matches!(bytes[cursor], b'e' | b'E') {
            is_float = true;
            cursor += 1;
            if cursor < bytes.len() && matches!(bytes[cursor], b'+' | b'-') {
                cursor += 1;
            }
            let exponent_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
            }
            if cursor == exponent_start {
                return Err(self.err_parse("number"));
            }
        }

        let token = if is_float {
            const STACK_NUMBER_BYTES: usize = 64;
            let token_len = cursor + 1;
            let parsed = if token_len <= STACK_NUMBER_BYTES {
                let mut stack = [0u8; STACK_NUMBER_BYTES];
                stack[0] = first;
                stack[1..token_len].copy_from_slice(&bytes[..cursor]);
                // SAFETY: the number scanner only accepts ASCII bytes.
                let text = unsafe { std::str::from_utf8_unchecked(&stack[..token_len]) };
                text.parse::<f64>()
            } else {
                self.numbuf.clear();
                self.numbuf.reserve(token_len);
                self.numbuf.push(self.cur);
                self.numbuf.push_str(&rest[..cursor]);
                self.numbuf.parse::<f64>()
            };
            match parsed {
                Ok(value) => DeJsonTok::F64(value),
                Err(_) => return Err(self.err_parse("number")),
            }
        } else {
            let first_digit = if negative { None } else { Some(first) };
            let value = match parse_u128_digit_parts(first_digit, &bytes[..integer_tail_end]) {
                Some(value) => value,
                None => return Err(self.err_parse("number")),
            };

            if negative {
                const I128_MIN_MAGNITUDE: u128 = 1u128 << 127;
                if value > I128_MIN_MAGNITUDE {
                    return Err(self.err_parse("number"));
                }
                let value = if value == I128_MIN_MAGNITUDE {
                    i128::MIN
                } else {
                    -(value as i128)
                };
                if value >= i64::MIN as i128 {
                    DeJsonTok::I64(value as i64)
                } else {
                    DeJsonTok::I128(value)
                }
            } else if value <= u64::MAX as u128 {
                DeJsonTok::U64(value as u64)
            } else {
                DeJsonTok::U128(value)
            }
        };

        self.col += cursor;
        // A JSON number is ASCII and `cursor` was produced by scanning bytes.
        reset_chars_to_suffix(i, cursor);
        self.next(i);
        self.tok = token;
        Ok(())
    }

    #[inline]
    fn parse_ident_fast(&mut self, i: &mut Chars<'_>) -> Result<(), DeJsonErr> {
        let first = self.cur as u8;
        let rest = i.as_str();
        let bytes = rest.as_bytes();
        let mut cursor = 0usize;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphabetic() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        let tail = &bytes[..cursor];

        let token = if first == b't' && tail == b"rue" {
            Some(DeJsonTok::Bool(true))
        } else if first == b'f' && tail == b"alse" {
            Some(DeJsonTok::Bool(false))
        } else if first == b'n' && tail == b"ull" {
            Some(DeJsonTok::Null)
        } else {
            self.identbuf.clear();
            self.identbuf.push(self.cur);
            self.identbuf.push_str(&rest[..cursor]);
            None
        };

        self.col += tail.len();
        // Identifiers are scanned as ASCII bytes only.
        reset_chars_to_suffix(i, cursor);
        self.next(i);

        if let Some(token) = token {
            self.tok = token;
            return Ok(());
        }

        self.tok = DeJsonTok::BareIdent;
        if self.lenient {
            Ok(())
        } else {
            Err(self.err_msg(&format!(
                "Got ##{}## needed true, false, null",
                self.identbuf
            )))
        }
    }

    #[inline]
    fn read_hex4(&mut self, i: &mut Chars<'_>) -> Result<u16, DeJsonErr> {
        let mut value = 0u16;
        for _ in 0..4 {
            self.next(i);
            let digit = match self.cur {
                '0'..='9' => self.cur as u16 - '0' as u16,
                'a'..='f' => self.cur as u16 - 'a' as u16 + 10,
                'A'..='F' => self.cur as u16 - 'A' as u16 + 10,
                _ => return Err(self.err_parse("unicode escape")),
            };
            value = (value << 4) | digit;
        }
        self.next(i);
        Ok(value)
    }

    #[inline]
    fn parse_string_fast(&mut self, i: &mut Chars<'_>) -> Result<(), DeJsonErr> {
        self.strbuf.clear();
        self.next(i);

        loop {
            match self.cur {
                '"' => {
                    self.next(i);
                    self.tok = DeJsonTok::Str;
                    return Ok(());
                }
                '\\' => {
                    self.next(i);
                    match self.cur {
                        'n' => self.strbuf.push('\n'),
                        'r' => self.strbuf.push('\r'),
                        't' => self.strbuf.push('\t'),
                        'b' => self.strbuf.push('\u{0008}'),
                        'f' => self.strbuf.push('\u{000c}'),
                        '0' => self.strbuf.push('\0'),
                        '"' => self.strbuf.push('"'),
                        '\\' => self.strbuf.push('\\'),
                        '/' => self.strbuf.push('/'),
                        '\0' => return Err(self.err_parse("string")),
                        'u' => {
                            let high = self.read_hex4(i)?;
                            let code_point = if (0xd800..=0xdbff).contains(&high) {
                                if self.cur == '\\'
                                    && i.as_str().as_bytes().first() == Some(&b'u')
                                {
                                    self.next(i);
                                    let low = self.read_hex4(i)?;
                                    if (0xdc00..=0xdfff).contains(&low) {
                                        0x1_0000
                                            + (((high as u32 - 0xd800) << 10)
                                                | (low as u32 - 0xdc00))
                                    } else {
                                        0xfffd
                                    }
                                } else {
                                    0xfffd
                                }
                            } else if (0xdc00..=0xdfff).contains(&high) {
                                0xfffd
                            } else {
                                high as u32
                            };
                            self.strbuf
                                .push(std::char::from_u32(code_point).unwrap_or('\u{fffd}'));
                            continue;
                        }
                        value => self.strbuf.push(value),
                    }
                    self.next(i);
                }
                '\0' => return Err(self.err_parse("string")),
                value => {
                    self.strbuf.push(value);
                    let rest = i.as_str();
                    let count = find_string_special(rest.as_bytes()).unwrap_or(rest.len());
                    if count != 0 {
                        let segment = &rest[..count];
                        self.strbuf.push_str(segment);
                        self.advance_position_str(segment);
                    }
                    // `count` ends at an ASCII special byte or the end of a valid str.
                    reset_chars_to_suffix(i, count);
                    self.next(i);
                }
            }
        }
    }

    pub fn next_tok(&mut self, i: &mut Chars<'_>) -> Result<(), DeJsonErr> {
        while matches!(self.cur, '\n' | '\r' | '\t' | ' ') {
            let rest = i.as_str();
            let count = skip_json_whitespace(rest.as_bytes());
            self.advance_position_ascii_bytes(&rest.as_bytes()[..count]);
            // JSON whitespace is ASCII and `count` is therefore a UTF-8 boundary.
            reset_chars_to_suffix(i, count);
            self.next(i);
        }

        if self.cur == '\0' {
            self.tok = DeJsonTok::Eof;
            return Ok(());
        }

        match self.cur {
            ':' => {
                self.next(i);
                self.tok = DeJsonTok::Colon;
                Ok(())
            }
            ',' => {
                self.next(i);
                self.tok = DeJsonTok::Comma;
                Ok(())
            }
            '[' => {
                self.next(i);
                self.tok = DeJsonTok::BlockOpen;
                Ok(())
            }
            ']' => {
                self.next(i);
                self.tok = DeJsonTok::BlockClose;
                Ok(())
            }
            '{' => {
                self.next(i);
                self.tok = DeJsonTok::CurlyOpen;
                Ok(())
            }
            '}' => {
                self.next(i);
                self.tok = DeJsonTok::CurlyClose;
                Ok(())
            }
            '-' | '0'..='9' => self.parse_number_fast(i),
            'a'..='z' | 'A'..='Z' | '_' => self.parse_ident_fast(i),
            '"' => self.parse_string_fast(i),
            _ => Err(self.err_token("tokenizer")),
        }
    }
}

macro_rules! impl_en_de_json_unsigned {
    ($ty:ident, $max:expr, $writer:ident, $wide:ty) => {
        impl EnJson for $ty {
            #[inline]
            fn en_json(&self, _d: usize, s: &mut EnJsonState) {
                $writer(&mut s.out, *self as $wide);
            }
        }

        impl DeJson for $ty {
            #[inline]
            fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<$ty, DeJsonErr> {
                let val = s.u128_range($max as u128)?;
                s.next_tok(i)?;
                Ok(val as $ty)
            }
        }
    };
}

macro_rules! impl_en_de_json_signed {
    ($ty:ident, $min:expr, $max:expr, $writer:ident, $wide:ty) => {
        impl EnJson for $ty {
            #[inline]
            fn en_json(&self, _d: usize, s: &mut EnJsonState) {
                $writer(&mut s.out, *self as $wide);
            }
        }

        impl DeJson for $ty {
            #[inline]
            fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<$ty, DeJsonErr> {
                let val = s.i128_range($min as i128, $max as i128)?;
                s.next_tok(i)?;
                Ok(val as $ty)
            }
        }
    };
}

macro_rules! impl_en_de_json_float {
    ($ty:ident) => {
        impl EnJson for $ty {
            #[inline]
            fn en_json(&self, _d: usize, s: &mut EnJsonState) {
                // Writing directly into the destination avoids a temporary `String` allocation.
                let _ = FmtWrite::write_fmt(&mut s.out, format_args!("{}", self));
            }
        }

        impl DeJson for $ty {
            #[inline]
            fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<$ty, DeJsonErr> {
                let val = s.as_f64()?;
                s.next_tok(i)?;
                Ok(val as $ty)
            }
        }
    };
}

impl_en_de_json_unsigned!(usize, usize::MAX, write_u64, u64);
impl_en_de_json_unsigned!(u64, u64::MAX, write_u64, u64);
impl_en_de_json_unsigned!(u128, u128::MAX, write_u128, u128);
impl_en_de_json_unsigned!(u32, u32::MAX, write_u64, u64);
impl_en_de_json_unsigned!(u16, u16::MAX, write_u64, u64);
impl_en_de_json_unsigned!(u8, u8::MAX, write_u64, u64);
impl_en_de_json_signed!(i64, i64::MIN, i64::MAX, write_i64, i64);
impl_en_de_json_signed!(i128, i128::MIN, i128::MAX, write_i128, i128);
impl_en_de_json_signed!(i32, i32::MIN, i32::MAX, write_i64, i64);
impl_en_de_json_signed!(i16, i16::MIN, i16::MAX, write_i64, i64);
impl_en_de_json_signed!(i8, i8::MIN, i8::MAX, write_i64, i64);
impl_en_de_json_float!(f64);
impl_en_de_json_float!(f32);

impl EnJson for VID {
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        self.0.en_json(d, s);
    }
}

impl DeJson for VID {
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Self, DeJsonErr> {
        Ok(VID(u64::de_json(s, i)?))
    }
}

impl<T> EnJson for Option<T>
where
    T: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        if let Some(v) = self {
            v.en_json(d, s);
        } else {
            s.out.push_str("null");
        }
    }
}

impl<T> DeJson for Option<T>
where
    T: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Self, DeJsonErr> {
        if let DeJsonTok::Null = s.tok {
            s.next_tok(i)?;
            return Ok(None);
        }
        Ok(Some(DeJson::de_json(s, i)?))
    }
}

impl EnJson for bool {
    fn en_json(&self, _d: usize, s: &mut EnJsonState) {
        if *self {
            s.out.push_str("true")
        } else {
            s.out.push_str("false")
        }
    }
}

impl DeJson for bool {
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<bool, DeJsonErr> {
        let val = s.as_bool()?;
        s.next_tok(i)?;
        Ok(val)
    }
}

impl EnJson for str {
    #[inline]
    fn en_json(&self, _d: usize, s: &mut EnJsonState) {
        write_json_string(&mut s.out, self);
    }
}

impl EnJson for &str {
    #[inline]
    fn en_json(&self, _d: usize, s: &mut EnJsonState) {
        write_json_string(&mut s.out, *self);
    }
}

impl EnJson for String {
    #[inline]
    fn en_json(&self, _d: usize, s: &mut EnJsonState) {
        write_json_string(&mut s.out, self.as_str());
    }
}

impl DeJson for String {
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<String, DeJsonErr> {
        let val = s.as_string()?;
        s.next_tok(i)?;
        Ok(val)
    }
}

impl<T> EnJson for Vec<T>
where
    T: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('[');
        let mut values = self.iter();
        if let Some(first) = values.next() {
            s.indent(d + 1);
            first.en_json(d + 1, s);
            for value in values {
                s.out.push(',');
                s.indent(d + 1);
                value.en_json(d + 1, s);
            }
        }
        s.out.push(']');
    }
}

impl<T> DeJson for Vec<T>
where
    T: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Vec<T>, DeJsonErr> {
        s.block_open(i)?;
        if s.tok == DeJsonTok::BlockClose {
            s.block_close(i)?;
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(8);
        while s.tok != DeJsonTok::BlockClose {
            out.push(DeJson::de_json(s, i)?);
            s.eat_comma_block(i)?;
        }
        s.block_close(i)?;
        Ok(out)
    }
}

#[derive(Clone, Debug)]
pub enum JsonValue {
    String(String),
    Char(char),
    U64(u64),
    U128(u128),
    I64(i64),
    I128(i128),
    F64(f64),
    Bool(bool),
    BareIdent(String),
    Null,
    Undefined,
    Object(HashMap<String, JsonValue>),
    Array(Vec<JsonValue>),
}

impl JsonValue {
    pub fn object(&self) -> Option<&HashMap<String, JsonValue>> {
        if let JsonValue::Object(obj) = self {
            return Some(obj);
        }
        None
    }
    pub fn string(&self) -> Option<&String> {
        if let JsonValue::String(obj) = self {
            return Some(obj);
        }
        None
    }
    pub fn key(&self, key: &str) -> Option<&JsonValue> {
        if let JsonValue::Object(obj) = self {
            return obj.get(key);
        }
        None
    }
}

impl EnJson for JsonValue {
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        match self {
            JsonValue::String(v) => v.en_json(d, s),
            JsonValue::Char(v) => {
                let mut buffer = [0u8; 4];
                write_json_string(&mut s.out, (*v).encode_utf8(&mut buffer));
            }
            JsonValue::U64(v) => v.en_json(d, s),
            JsonValue::U128(v) => v.en_json(d, s),
            JsonValue::I64(v) => v.en_json(d, s),
            JsonValue::I128(v) => v.en_json(d, s),
            JsonValue::F64(v) => v.en_json(d, s),
            JsonValue::Bool(v) => v.en_json(d, s),
            JsonValue::BareIdent(v) => v.en_json(d, s),
            JsonValue::Null => s.out.push_str("null"),
            JsonValue::Undefined => s.out.push_str("undefined"),
            JsonValue::Object(v) => v.en_json(d, s),
            JsonValue::Array(v) => v.en_json(d, s),
        }
    }
}

impl DeJson for JsonValue {
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<JsonValue, DeJsonErr> {
        // lets check what tokenm we have
        match s.tok {
            DeJsonTok::Str => {
                let v = s.as_string()?;
                s.next_tok(i)?;
                Ok(JsonValue::String(v))
            }
            DeJsonTok::Char(c) => {
                s.next_tok(i)?;
                Ok(JsonValue::Char(c))
            }
            DeJsonTok::U64(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::U64(v))
            }
            DeJsonTok::U128(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::U128(v))
            }
            DeJsonTok::I64(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::I64(v))
            }
            DeJsonTok::I128(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::I128(v))
            }
            DeJsonTok::F64(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::F64(v))
            }
            DeJsonTok::Bool(v) => {
                s.next_tok(i)?;
                Ok(JsonValue::Bool(v))
            }
            DeJsonTok::BareIdent => {
                let v = s.as_ident()?;
                s.next_tok(i)?;
                Ok(JsonValue::BareIdent(v))
            }
            DeJsonTok::Null => {
                s.next_tok(i)?;
                Ok(JsonValue::Null)
            }
            DeJsonTok::Colon => return Err(s.err_msg("Unexpected :")),
            DeJsonTok::CurlyOpen => {
                s.curly_open(i)?;
                let mut h = HashMap::with_capacity(8);
                while s.tok != DeJsonTok::CurlyClose {
                    let k = String::de_json(s, i)?;
                    s.colon(i)?;
                    let v = JsonValue::de_json(s, i)?;
                    s.eat_comma_curly(i)?;
                    h.insert(k, v);
                }
                s.curly_close(i)?;
                Ok(JsonValue::Object(h))
            }
            DeJsonTok::CurlyClose => return Err(s.err_msg("Unexpected }")),
            DeJsonTok::BlockOpen => {
                s.block_open(i)?;
                let mut out = Vec::with_capacity(8);
                while s.tok != DeJsonTok::BlockClose {
                    out.push(JsonValue::de_json(s, i)?);
                    s.eat_comma_block(i)?;
                }
                s.block_close(i)?;
                Ok(JsonValue::Array(out))
            }
            DeJsonTok::BlockClose => return Err(s.err_msg("Unexpected ]")),
            DeJsonTok::Comma => return Err(s.err_msg("Unexpected ,")),
            DeJsonTok::Bof => return Err(s.err_msg("Unexpected Bof")),
            DeJsonTok::Eof => return Err(s.err_msg("Unexpected Eof")),
        }
    }
}

impl<T> EnJson for [T]
where
    T: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('[');
        let mut values = self.iter();
        if let Some(first) = values.next() {
            first.en_json(d + 1, s);
            for value in values {
                s.out.push(',');
                value.en_json(d + 1, s);
            }
        }
        s.out.push(']');
    }
}

unsafe fn de_json_array_impl_inner<T>(
    top: *mut T,
    count: usize,
    s: &mut DeJsonState,
    i: &mut Chars,
) -> Result<(), DeJsonErr>
where
    T: DeJson,
{
    struct InitGuard<T> {
        top: *mut T,
        initialized: usize,
    }

    impl<T> Drop for InitGuard<T> {
        fn drop(&mut self) {
            // SAFETY: exactly `initialized` consecutive values were written at `top`.
            unsafe {
                for index in 0..self.initialized {
                    std::ptr::drop_in_place(self.top.add(index));
                }
            }
        }
    }

    s.block_open(i)?;
    let mut guard = InitGuard {
        top,
        initialized: 0,
    };
    for index in 0..count {
        let value = DeJson::de_json(s, i)?;
        // SAFETY: the caller provided storage for `count` elements and this slot is uninitialized.
        unsafe { top.add(index).write(value) };
        guard.initialized += 1;
        s.eat_comma_block(i)?;
    }
    s.block_close(i)?;
    std::mem::forget(guard);
    Ok(())
}

impl<T, const N: usize> DeJson for [T; N]
where
    T: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Self, DeJsonErr> {
        unsafe {
            let mut to = std::mem::MaybeUninit::<[T; N]>::uninit();
            let top: *mut T = &mut to as *mut _ as *mut T;
            de_json_array_impl_inner(top, N, s, i)?;
            Ok(to.assume_init())
        }
    }
}

fn de_json_comma_block<T>(s: &mut DeJsonState, i: &mut Chars) -> Result<T, DeJsonErr>
where
    T: DeJson,
{
    let value = DeJson::de_json(s, i)?;
    s.eat_comma_block(i)?;
    Ok(value)
}

impl<A, B> EnJson for (A, B)
where
    A: EnJson,
    B: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('[');
        self.0.en_json(d, s);
        s.out.push(',');
        self.1.en_json(d, s);
        s.out.push(']');
    }
}

impl<A, B> DeJson for (A, B)
where
    A: DeJson,
    B: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<(A, B), DeJsonErr> {
        s.block_open(i)?;
        let r = (de_json_comma_block(s, i)?, de_json_comma_block(s, i)?);
        s.block_close(i)?;
        Ok(r)
    }
}

impl<A, B, C> EnJson for (A, B, C)
where
    A: EnJson,
    B: EnJson,
    C: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('[');
        self.0.en_json(d, s);
        s.out.push(',');
        self.1.en_json(d, s);
        s.out.push(',');
        self.2.en_json(d, s);
        s.out.push(']');
    }
}

impl<A, B, C> DeJson for (A, B, C)
where
    A: DeJson,
    B: DeJson,
    C: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<(A, B, C), DeJsonErr> {
        s.block_open(i)?;
        let r = (
            de_json_comma_block(s, i)?,
            de_json_comma_block(s, i)?,
            de_json_comma_block(s, i)?,
        );
        s.block_close(i)?;
        Ok(r)
    }
}

impl<A, B, C, D> EnJson for (A, B, C, D)
where
    A: EnJson,
    B: EnJson,
    C: EnJson,
    D: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('[');
        self.0.en_json(d, s);
        s.out.push(',');
        self.1.en_json(d, s);
        s.out.push(',');
        self.2.en_json(d, s);
        s.out.push(',');
        self.3.en_json(d, s);
        s.out.push(']');
    }
}

impl<A, B, C, D> DeJson for (A, B, C, D)
where
    A: DeJson,
    B: DeJson,
    C: DeJson,
    D: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<(A, B, C, D), DeJsonErr> {
        s.block_open(i)?;
        let r = (
            de_json_comma_block(s, i)?,
            de_json_comma_block(s, i)?,
            de_json_comma_block(s, i)?,
            de_json_comma_block(s, i)?,
        );
        s.block_close(i)?;
        Ok(r)
    }
}

impl<K, V> EnJson for HashMap<K, V>
where
    K: EnJson,
    V: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        s.out.push('{');
        let mut entries = self.iter();
        if let Some((key, value)) = entries.next() {
            s.indent(d + 1);
            key.en_json(d + 1, s);
            s.out.push(':');
            value.en_json(d + 1, s);
            for (key, value) in entries {
                s.out.push(',');
                s.indent(d + 1);
                key.en_json(d + 1, s);
                s.out.push(':');
                value.en_json(d + 1, s);
            }
        }
        s.indent(d);
        s.out.push('}');
    }
}

impl<K, V> DeJson for HashMap<K, V>
where
    K: DeJson + Eq + Hash,
    V: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Self, DeJsonErr> {
        s.curly_open(i)?;
        if s.tok == DeJsonTok::CurlyClose {
            s.curly_close(i)?;
            return Ok(HashMap::new());
        }

        let mut out = HashMap::with_capacity(8);
        while s.tok != DeJsonTok::CurlyClose {
            let key = DeJson::de_json(s, i)?;
            s.colon(i)?;
            let value = DeJson::de_json(s, i)?;
            s.eat_comma_curly(i)?;
            out.insert(key, value);
        }
        s.curly_close(i)?;
        Ok(out)
    }
}

impl<T> EnJson for Box<T>
where
    T: EnJson,
{
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        (**self).en_json(d, s)
    }
}

impl<T> DeJson for Box<T>
where
    T: DeJson,
{
    fn de_json(s: &mut DeJsonState, i: &mut Chars) -> Result<Box<T>, DeJsonErr> {
        Ok(Box::new(DeJson::de_json(s, i)?))
    }
}

/// Borrowing JSON DOM. Strings and object keys without escapes are borrowed directly
/// from the input; only escaped strings allocate.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonValueRef<'a> {
    String(Cow<'a, str>),
    U64(u64),
    U128(u128),
    I64(i64),
    I128(i128),
    F64(f64),
    Bool(bool),
    BareIdent(Cow<'a, str>),
    Null,
    Undefined,
    Object(HashMap<Cow<'a, str>, JsonValueRef<'a>>),
    Array(Vec<JsonValueRef<'a>>),
}

impl<'a> JsonValueRef<'a> {
    /// Parses standard JSON while borrowing every unescaped string/key from `input`.
    #[inline]
    pub fn parse(input: &'a str) -> Result<Self, DeJsonErr> {
        parse_json_zero_copy(input)
    }

    /// Compatibility-oriented parser that additionally accepts bare identifiers,
    /// `undefined`, trailing commas, unknown escapes and raw control characters.
    #[inline]
    pub fn parse_lenient(input: &'a str) -> Result<Self, DeJsonErr> {
        parse_json_zero_copy_lenient(input)
    }

    pub fn object(&self) -> Option<&HashMap<Cow<'a, str>, JsonValueRef<'a>>> {
        match self {
            JsonValueRef::Object(value) => Some(value),
            _ => None,
        }
    }

    pub fn string(&self) -> Option<&str> {
        match self {
            JsonValueRef::String(value) => Some(value.as_ref()),
            _ => None,
        }
    }

    pub fn key(&self, key: &str) -> Option<&JsonValueRef<'a>> {
        self.object()?.get(key)
    }

    pub fn into_owned(self) -> JsonValue {
        match self {
            JsonValueRef::String(value) => JsonValue::String(value.into_owned()),
            JsonValueRef::U64(value) => JsonValue::U64(value),
            JsonValueRef::U128(value) => JsonValue::U128(value),
            JsonValueRef::I64(value) => JsonValue::I64(value),
            JsonValueRef::I128(value) => JsonValue::I128(value),
            JsonValueRef::F64(value) => JsonValue::F64(value),
            JsonValueRef::Bool(value) => JsonValue::Bool(value),
            JsonValueRef::BareIdent(value) => JsonValue::BareIdent(value.into_owned()),
            JsonValueRef::Null => JsonValue::Null,
            JsonValueRef::Undefined => JsonValue::Undefined,
            JsonValueRef::Object(value) => JsonValue::Object(
                value
                    .into_iter()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect(),
            ),
            JsonValueRef::Array(value) => {
                JsonValue::Array(value.into_iter().map(JsonValueRef::into_owned).collect())
            }
        }
    }
}

impl<'a> EnJson for Cow<'a, str> {
    #[inline]
    fn en_json(&self, _d: usize, s: &mut EnJsonState) {
        write_json_string(&mut s.out, self.as_ref());
    }
}

impl<'a> EnJson for JsonValueRef<'a> {
    fn en_json(&self, d: usize, s: &mut EnJsonState) {
        match self {
            JsonValueRef::String(value) => value.en_json(d, s),
            JsonValueRef::U64(value) => value.en_json(d, s),
            JsonValueRef::U128(value) => value.en_json(d, s),
            JsonValueRef::I64(value) => value.en_json(d, s),
            JsonValueRef::I128(value) => value.en_json(d, s),
            JsonValueRef::F64(value) => value.en_json(d, s),
            JsonValueRef::Bool(value) => value.en_json(d, s),
            JsonValueRef::BareIdent(value) => value.en_json(d, s),
            JsonValueRef::Null => s.out.push_str("null"),
            JsonValueRef::Undefined => s.out.push_str("undefined"),
            JsonValueRef::Object(value) => value.en_json(d, s),
            JsonValueRef::Array(value) => value.en_json(d, s),
        }
    }
}

#[inline]
pub fn parse_json_zero_copy<'a>(input: &'a str) -> Result<JsonValueRef<'a>, DeJsonErr> {
    BorrowedJsonParser::new(input, false).parse_document()
}

#[inline]
pub fn parse_json_zero_copy_lenient<'a>(
    input: &'a str,
) -> Result<JsonValueRef<'a>, DeJsonErr> {
    BorrowedJsonParser::new(input, true).parse_document()
}

struct BorrowedJsonParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    index: usize,
    lenient: bool,
}

impl<'a> BorrowedJsonParser<'a> {
    #[inline]
    fn new(input: &'a str, lenient: bool) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            index: 0,
            lenient,
        }
    }

    fn parse_document(mut self) -> Result<JsonValueRef<'a>, DeJsonErr> {
        self.skip_whitespace();
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.index != self.bytes.len() {
            return Err(self.error("Trailing characters after JSON value"));
        }
        Ok(value)
    }

    #[inline]
    fn skip_whitespace(&mut self) {
        self.index += skip_json_whitespace(&self.bytes[self.index..]);
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.index).copied()
    }

    fn parse_value(&mut self) -> Result<JsonValueRef<'a>, DeJsonErr> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => Ok(JsonValueRef::String(self.parse_string()?)),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b't') if self.consume_keyword(b"true") => Ok(JsonValueRef::Bool(true)),
            Some(b'f') if self.consume_keyword(b"false") => Ok(JsonValueRef::Bool(false)),
            Some(b'n') if self.consume_keyword(b"null") => Ok(JsonValueRef::Null),
            Some(b'u') if self.lenient && self.consume_keyword(b"undefined") => {
                Ok(JsonValueRef::Undefined)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(byte) if self.lenient && (byte.is_ascii_alphabetic() || byte == b'_') => {
                Ok(JsonValueRef::BareIdent(self.parse_bare_ident()))
            }
            Some(_) => Err(self.error("Unexpected token")),
            None => Err(self.error("Unexpected end of input")),
        }
    }

    #[inline]
    fn consume_keyword(&mut self, keyword: &[u8]) -> bool {
        if !self.bytes[self.index..].starts_with(keyword) {
            return false;
        }
        let end = self.index + keyword.len();
        if let Some(&next) = self.bytes.get(end) {
            if next.is_ascii_alphanumeric() || next == b'_' {
                return false;
            }
        }
        self.index = end;
        true
    }

    fn parse_bare_ident(&mut self) -> Cow<'a, str> {
        let start = self.index;
        self.index += 1;
        while self.index < self.bytes.len()
            && (self.bytes[self.index].is_ascii_alphanumeric()
                || self.bytes[self.index] == b'_')
        {
            self.index += 1;
        }
        // SAFETY: the identifier is ASCII and both bounds are UTF-8 boundaries.
        Cow::Borrowed(unsafe { self.input.get_unchecked(start..self.index) })
    }

    fn parse_string(&mut self) -> Result<Cow<'a, str>, DeJsonErr> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.index += 1;
        let string_start = self.index;
        let mut chunk_start = self.index;
        let mut owned: Option<String> = None;

        loop {
            let remaining = &self.bytes[self.index..];
            let relative = match find_string_special(remaining) {
                Some(index) => index,
                None => {
                    self.index = self.bytes.len();
                    return Err(self.error("Unterminated string"));
                }
            };
            let special = self.index + relative;
            let byte = self.bytes[special];

            match byte {
                b'"' => {
                    self.index = special + 1;
                    if let Some(mut value) = owned {
                        // SAFETY: special ASCII bytes always occur on UTF-8 boundaries.
                        value.push_str(unsafe {
                            self.input.get_unchecked(chunk_start..special)
                        });
                        return Ok(Cow::Owned(value));
                    }
                    // SAFETY: both bounds surround a substring of the original valid UTF-8.
                    return Ok(Cow::Borrowed(unsafe {
                        self.input.get_unchecked(string_start..special)
                    }));
                }
                b'\\' => {
                    let value = owned.get_or_insert_with(|| {
                        String::with_capacity((special - string_start).saturating_add(16))
                    });
                    // SAFETY: special ASCII bytes always occur on UTF-8 boundaries.
                    value.push_str(unsafe {
                        self.input.get_unchecked(chunk_start..special)
                    });
                    self.index = special + 1;
                    let escape = match self.peek() {
                        Some(value) => value,
                        None => return Err(self.error("Unterminated string escape")),
                    };
                    if !escape.is_ascii() {
                        if !self.lenient {
                            return Err(self.error("Invalid string escape"));
                        }
                        // SAFETY: `self.index` is at the start of a UTF-8 code point.
                        let escaped_char = unsafe {
                            self.input.get_unchecked(self.index..)
                        }
                        .chars()
                        .next()
                        .expect("non-ASCII escape byte must start a character");
                        self.index += escaped_char.len_utf8();
                        value.push(escaped_char);
                        chunk_start = self.index;
                        continue;
                    }
                    self.index += 1;
                    match escape {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{0008}'),
                        b'f' => value.push('\u{000c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'0' if self.lenient => value.push('\0'),
                        b'u' => {
                            let high = self.parse_hex4()?;
                            let code_point = if (0xd800..=0xdbff).contains(&high) {
                                if self.bytes.get(self.index) == Some(&b'\\')
                                    && self.bytes.get(self.index + 1) == Some(&b'u')
                                {
                                    self.index += 2;
                                    let low = self.parse_hex4()?;
                                    if (0xdc00..=0xdfff).contains(&low) {
                                        0x1_0000
                                            + (((high as u32 - 0xd800) << 10)
                                                | (low as u32 - 0xdc00))
                                    } else if self.lenient {
                                        0xfffd
                                    } else {
                                        return Err(self.error("Invalid low surrogate"));
                                    }
                                } else if self.lenient {
                                    0xfffd
                                } else {
                                    return Err(self.error("Missing low surrogate"));
                                }
                            } else if (0xdc00..=0xdfff).contains(&high) {
                                if self.lenient {
                                    0xfffd
                                } else {
                                    return Err(self.error("Unexpected low surrogate"));
                                }
                            } else {
                                high as u32
                            };
                            value.push(
                                std::char::from_u32(code_point).unwrap_or('\u{fffd}'),
                            );
                        }
                        value_byte if self.lenient => value.push(value_byte as char),
                        _ => return Err(self.error("Invalid string escape")),
                    }
                    chunk_start = self.index;
                }
                control if control < 0x20 => {
                    if !self.lenient {
                        self.index = special;
                        return Err(self.error("Unescaped control character in string"));
                    }
                    self.index = special + 1;
                }
                _ => unreachable!(),
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, DeJsonErr> {
        if self.index + 4 > self.bytes.len() {
            self.index = self.bytes.len();
            return Err(self.error("Incomplete unicode escape"));
        }

        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.bytes[self.index] {
                b'0'..=b'9' => self.bytes[self.index] - b'0',
                b'a'..=b'f' => self.bytes[self.index] - b'a' + 10,
                b'A'..=b'F' => self.bytes[self.index] - b'A' + 10,
                _ => return Err(self.error("Invalid unicode escape")),
            };
            value = (value << 4) | digit as u16;
            self.index += 1;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValueRef<'a>, DeJsonErr> {
        let start = self.index;
        let negative = self.peek() == Some(b'-');
        if negative {
            self.index += 1;
        }

        let integer_start = self.index;
        match self.peek() {
            Some(b'0') => {
                self.index += 1;
                if !self.lenient && matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("Leading zero in number"));
                }
                while self.lenient && matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.index += 1;
                }
            }
            Some(b'1'..=b'9') => {
                self.index += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.index += 1;
                }
            }
            _ => return Err(self.error("Expected digits in number")),
        }
        let integer_end = self.index;

        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.index += 1;
            let fraction_start = self.index;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
            if self.index == fraction_start && !self.lenient {
                return Err(self.error("Expected digits after decimal point"));
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            let exponent_start = self.index;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
            if self.index == exponent_start {
                return Err(self.error("Expected exponent digits"));
            }
        }

        if is_float {
            // SAFETY: the number grammar scans ASCII only, so the range is UTF-8 aligned.
            let text = unsafe { self.input.get_unchecked(start..self.index) };
            return text
                .parse::<f64>()
                .map(JsonValueRef::F64)
                .map_err(|_| self.error("Cannot parse floating-point number"));
        }

        let magnitude = parse_u128_digits(&self.bytes[integer_start..integer_end])
            .ok_or_else(|| self.error("Integer out of range"))?;

        if negative {
            const I128_MIN_MAGNITUDE: u128 = 1u128 << 127;
            if magnitude > I128_MIN_MAGNITUDE {
                return Err(self.error("Signed integer out of range"));
            }
            let value = if magnitude == I128_MIN_MAGNITUDE {
                i128::MIN
            } else {
                -(magnitude as i128)
            };
            if value >= i64::MIN as i128 {
                Ok(JsonValueRef::I64(value as i64))
            } else {
                Ok(JsonValueRef::I128(value))
            }
        } else if magnitude <= u64::MAX as u128 {
            Ok(JsonValueRef::U64(magnitude as u64))
        } else {
            Ok(JsonValueRef::U128(magnitude))
        }
    }

    fn parse_array(&mut self) -> Result<JsonValueRef<'a>, DeJsonErr> {
        debug_assert_eq!(self.peek(), Some(b'['));
        self.index += 1;
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.index += 1;
            return Ok(JsonValueRef::Array(Vec::new()));
        }

        let mut values = Vec::with_capacity(8);
        loop {
            values.push(self.parse_value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.index += 1;
                    self.skip_whitespace();
                    if self.lenient && self.peek() == Some(b']') {
                        self.index += 1;
                        break;
                    }
                }
                Some(b']') => {
                    self.index += 1;
                    break;
                }
                _ => return Err(self.error("Expected ',' or ']'")),
            }
        }
        Ok(JsonValueRef::Array(values))
    }

    fn parse_object(&mut self) -> Result<JsonValueRef<'a>, DeJsonErr> {
        debug_assert_eq!(self.peek(), Some(b'{'));
        self.index += 1;
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.index += 1;
            return Ok(JsonValueRef::Object(HashMap::new()));
        }

        let mut values = HashMap::with_capacity(8);
        loop {
            let key = match self.peek() {
                Some(b'"') => self.parse_string()?,
                Some(byte)
                    if self.lenient && (byte.is_ascii_alphabetic() || byte == b'_') =>
                {
                    self.parse_bare_ident()
                }
                _ => return Err(self.error("Expected object key")),
            };
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("Expected ':' after object key"));
            }
            self.index += 1;
            let value = self.parse_value()?;
            values.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.index += 1;
                    self.skip_whitespace();
                    if self.lenient && self.peek() == Some(b'}') {
                        self.index += 1;
                        break;
                    }
                }
                Some(b'}') => {
                    self.index += 1;
                    break;
                }
                _ => return Err(self.error("Expected ',' or '}'")),
            }
        }
        Ok(JsonValueRef::Object(values))
    }

    #[cold]
    #[inline(never)]
    fn error(&self, message: &str) -> DeJsonErr {
        let boundary = self.index.min(self.input.len());
        // All parser advances happen at ASCII delimiters, so `boundary` is a UTF-8 boundary.
        let prefix = &self.input[..boundary];
        let line = prefix.bytes().filter(|&byte| byte == b'\n').count();
        let column_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        let col = prefix[column_start..].chars().count();
        DeJsonErr {
            msg: message.to_owned(),
            line,
            col,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        find_string_special, find_string_special_scalar, find_string_special_swar,
        parse_json_zero_copy, parse_json_zero_copy_lenient, skip_json_whitespace,
        skip_json_whitespace_scalar, skip_json_whitespace_swar, DeJson, JsonValueRef, EnJson,
    };
    use std::borrow::Cow;
    use std::collections::HashMap;

    #[test]
    fn portable_scanners_match_scalar_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for len in 0..=257usize {
            let mut bytes = vec![0u8; len];
            for byte in &mut bytes {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *byte = (seed >> 32) as u8;
            }

            assert_eq!(
                find_string_special_swar(&bytes),
                find_string_special_scalar(&bytes),
                "SWAR string scan mismatch at length {len}",
            );
            assert_eq!(
                find_string_special(&bytes),
                find_string_special_scalar(&bytes),
                "dispatched string scan mismatch at length {len}",
            );
            assert_eq!(
                skip_json_whitespace_swar(&bytes),
                skip_json_whitespace_scalar(&bytes),
                "SWAR whitespace scan mismatch at length {len}",
            );
            assert_eq!(
                skip_json_whitespace(&bytes),
                skip_json_whitespace_scalar(&bytes),
                "dispatched whitespace scan mismatch at length {len}",
            );
        }
    }

    #[test]
    fn scanners_find_every_lane_boundary() {
        for len in 1..=96usize {
            for index in 0..len {
                let mut bytes = vec![b'a'; len];
                bytes[index] = b'"';
                assert_eq!(find_string_special(&bytes), Some(index));

                let mut whitespace = vec![b' '; len];
                whitespace[index] = b'x';
                assert_eq!(skip_json_whitespace(&whitespace), index);
            }
        }
    }

    #[test]
    fn encode_empty_hashmap_json() {
        let map: HashMap<String, String> = HashMap::new();
        assert_eq!(map.encode_json(), "{}");
    }

    #[test]
    fn decode_f64_scientific_notation() {
        let v: f64 = DeJson::decode_json("1.25e-2").unwrap();
        assert!((v - 0.0125).abs() < 1e-12);

        let v: f64 = DeJson::decode_json("-3E+1").unwrap();
        assert!((v + 30.0).abs() < 1e-12);
    }

    #[test]
    fn decode_u128_integer() {
        let value: u128 = DeJson::decode_json("1000000000000000019884624838656").unwrap();
        assert_eq!(value, 1000000000000000019884624838656u128);
    }

    #[test]
    fn decode_string_preserves_dollar_signs() {
        let value: String = DeJson::decode_json("\"▁$($\"").unwrap();
        assert_eq!(value, "▁$($");
    }

    #[test]
    fn encode_none_as_json_null() {
        let value: Option<u64> = None;
        assert_eq!(value.encode_json(), "null");
    }

    #[test]
    fn integer_ranges_are_checked_before_casting() {
        assert_eq!(u8::decode_json("255").unwrap(), 255);
        assert!(u8::decode_json("256").is_err());
        assert_eq!(i8::decode_json("-128").unwrap(), -128);
        assert!(i8::decode_json("-129").is_err());
        assert!(i32::decode_json("2147483648").is_err());
    }

    #[test]
    fn integer_fast_writers_match_standard_formatting() {
        for value in [
            0u128,
            9,
            10,
            99,
            100,
            u64::MAX as u128,
            u64::MAX as u128 + 1,
            10_000_000_000_000_000_000u128,
            u128::MAX,
        ] {
            assert_eq!(value.encode_json(), value.to_string());
        }
        for value in [i128::MIN, i64::MIN as i128, -1, 0, 1, i64::MAX as i128, i128::MAX] {
            assert_eq!(value.encode_json(), value.to_string());
        }
    }

    #[test]
    fn integer_parser_ignores_many_leading_zeroes_in_compat_mode() {
        let text = format!("{}1", "0".repeat(128));
        assert_eq!(u128::decode_json(&text).unwrap(), 1);
    }

    #[test]
    fn long_float_uses_reusable_fallback_buffer() {
        let text = format!("0.{}1", "0".repeat(80));
        let value = f64::decode_json(&text).unwrap();
        assert_eq!(value, 1e-81);
    }

    #[test]
    fn long_string_fast_path_roundtrips() {
        let value = format!("{}\\\"\n\r\t\0{}", "abcdef".repeat(64), "世界".repeat(32));
        let encoded = value.encode_json();
        let decoded = String::decode_json(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn unicode_surrogate_pair_is_decoded() {
        let decoded = String::decode_json("\"\\uD83D\\uDE80\"").unwrap();
        assert_eq!(decoded, "🚀");
    }

    #[test]
    fn zero_copy_parser_borrows_unescaped_strings() {
        let input = r#"{"name":"a long unescaped utf-8 value 世界","n":123}"#;
        let value = parse_json_zero_copy(input).unwrap();
        let name = value.key("name").unwrap();
        match name {
            JsonValueRef::String(value) => {
                assert!(matches!(value, &Cow::Borrowed(_)));
                assert_eq!(value.as_ref(), "a long unescaped utf-8 value 世界");
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn zero_copy_parser_allocates_only_for_escaped_strings() {
        let value = parse_json_zero_copy(r#""line\nvalue""#).unwrap();
        match value {
            JsonValueRef::String(value) => {
                assert!(matches!(value, Cow::Owned(_)));
                assert_eq!(value.as_ref(), "line\nvalue");
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn zero_copy_lenient_accepts_bare_values_and_trailing_commas() {
        let value = parse_json_zero_copy_lenient("{name: bare_value,}").unwrap();
        assert!(matches!(value.key("name"), Some(JsonValueRef::BareIdent(_))));
    }

    #[test]
    fn zero_copy_lenient_keeps_non_ascii_unknown_escape_utf8_safe() {
        let value = parse_json_zero_copy_lenient(r#""\世界""#).unwrap();
        match value {
            JsonValueRef::String(value) => assert_eq!(value.as_ref(), "世界"),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn zero_copy_keyword_requires_identifier_boundary() {
        let value = parse_json_zero_copy_lenient("true_value").unwrap();
        assert!(matches!(value, JsonValueRef::BareIdent(_)));
        assert!(parse_json_zero_copy("true_value").is_err());
    }

}
