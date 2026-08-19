const INVALID_BASE64: u8 = 64;

const BASE64_DEC: [u8; 256] = [
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 62, 64, 62, 64, 63,
    52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 64, 64, 64, 0, 64, 64, 64, 0, 1, 2, 3, 4, 5, 6, 7, 8,
    9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 64, 64, 64, 64, 63, 64, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50,
    51, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
    64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
];

pub const BASE64_STANDARD: [u8; 64] =
    *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub const BASE64_URL_SAFE: [u8; 64] =
    *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64DecodeError {
    WrongPadding,
    InvalidCharacter,
}

#[inline]
pub fn base64_encode(input: &[u8], table: &[u8; 64]) -> Vec<u8> {
    let len = input.len();

    if len == 0 {
        return Vec::new();
    }

    let full_chunks = len / 3;
    let remainder = len % 3;
    let out_len = full_chunks * 4 + usize::from(remainder != 0) * 4;

    let mut out = Vec::with_capacity(out_len);

    // SAFETY:
    // - `out` has exactly enough capacity for `out_len` bytes.
    // - Every output byte is initialized before `set_len`.
    // - Input accesses stay within `input`.
    unsafe {
        let mut src = input.as_ptr();
        let mut dst = out.as_mut_ptr();

        for _ in 0..full_chunks {
            let b0 = *src;
            let b1 = *src.add(1);
            let b2 = *src.add(2);

            *dst = *table.get_unchecked((b0 >> 2) as usize);
            *dst.add(1) =
                *table.get_unchecked((((b0 & 0x03) << 4) | (b1 >> 4)) as usize);
            *dst.add(2) =
                *table.get_unchecked((((b1 & 0x0f) << 2) | (b2 >> 6)) as usize);
            *dst.add(3) = *table.get_unchecked((b2 & 0x3f) as usize);

            src = src.add(3);
            dst = dst.add(4);
        }

        match remainder {
            1 => {
                let b0 = *src;

                *dst = *table.get_unchecked((b0 >> 2) as usize);
                *dst.add(1) = *table.get_unchecked(((b0 & 0x03) << 4) as usize);
                *dst.add(2) = b'=';
                *dst.add(3) = b'=';
            }
            2 => {
                let b0 = *src;
                let b1 = *src.add(1);

                *dst = *table.get_unchecked((b0 >> 2) as usize);
                *dst.add(1) =
                    *table.get_unchecked((((b0 & 0x03) << 4) | (b1 >> 4)) as usize);
                *dst.add(2) =
                    *table.get_unchecked(((b1 & 0x0f) << 2) as usize);
                *dst.add(3) = b'=';
            }
            _ => {}
        }

        out.set_len(out_len);
    }

    out
}

#[inline]
pub fn base64_decode(input: &[u8]) -> Result<Vec<u8>, Base64DecodeError> {
    let len = input.len();

    if len == 0 {
        return Ok(Vec::new());
    }

    if len & 3 != 0 {
        return Err(Base64DecodeError::WrongPadding);
    }

    let padding = if input[len - 1] == b'=' {
        if input[len - 2] == b'=' {
            2
        } else {
            1
        }
    } else {
        0
    };

    let chunks = len / 4;
    let out_len = chunks * 3 - padding;

    let mut out: Vec<u8> = Vec::with_capacity(out_len);

    // SAFETY:
    // - Input length is a multiple of four.
    // - `out` has enough capacity for the exact decoded length.
    // - Output length is set only after every returned byte is initialized.
    unsafe {
        let src = input.as_ptr();
        let dst = out.as_mut_ptr();

        // Decode every block except the final one.
        for i in 0..chunks - 1 {
            let s = src.add(i * 4);

            let c0 = *s;
            let c1 = *s.add(1);
            let c2 = *s.add(2);
            let c3 = *s.add(3);

            // Padding is illegal before the final block.
            if c0 == b'=' || c1 == b'=' || c2 == b'=' || c3 == b'=' {
                return Err(Base64DecodeError::WrongPadding);
            }

            let b0 = *BASE64_DEC.get_unchecked(c0 as usize);
            let b1 = *BASE64_DEC.get_unchecked(c1 as usize);
            let b2 = *BASE64_DEC.get_unchecked(c2 as usize);
            let b3 = *BASE64_DEC.get_unchecked(c3 as usize);

            // All valid values are 0..=63.
            if (b0 | b1 | b2 | b3) & INVALID_BASE64 != 0 {
                return Err(Base64DecodeError::InvalidCharacter);
            }

            let d = dst.add(i * 3);

            *d = (b0 << 2) | (b1 >> 4);
            *d.add(1) = ((b1 & 0x0f) << 4) | (b2 >> 2);
            *d.add(2) = ((b2 & 0x03) << 6) | b3;
        }

        // Final block needs separate handling because of padding.
        let s = src.add((chunks - 1) * 4);

        let c0 = *s;
        let c1 = *s.add(1);
        let c2 = *s.add(2);
        let c3 = *s.add(3);

        if c0 == b'=' || c1 == b'=' {
            return Err(Base64DecodeError::WrongPadding);
        }

        let b0 = *BASE64_DEC.get_unchecked(c0 as usize);
        let b1 = *BASE64_DEC.get_unchecked(c1 as usize);

        if (b0 | b1) & INVALID_BASE64 != 0 {
            return Err(Base64DecodeError::InvalidCharacter);
        }

        let d = dst.add((chunks - 1) * 3);

        match padding {
            0 => {
                if c2 == b'=' || c3 == b'=' {
                    return Err(Base64DecodeError::WrongPadding);
                }

                let b2 = *BASE64_DEC.get_unchecked(c2 as usize);
                let b3 = *BASE64_DEC.get_unchecked(c3 as usize);

                if (b2 | b3) & INVALID_BASE64 != 0 {
                    return Err(Base64DecodeError::InvalidCharacter);
                }

                *d = (b0 << 2) | (b1 >> 4);
                *d.add(1) = ((b1 & 0x0f) << 4) | (b2 >> 2);
                *d.add(2) = ((b2 & 0x03) << 6) | b3;
            }

            1 => {
                if c2 == b'=' || c3 != b'=' {
                    return Err(Base64DecodeError::WrongPadding);
                }

                let b2 = *BASE64_DEC.get_unchecked(c2 as usize);

                if b2 == INVALID_BASE64 {
                    return Err(Base64DecodeError::InvalidCharacter);
                }

                // Canonical Base64 requires unused bits to be zero.
                if b2 & 0x03 != 0 {
                    return Err(Base64DecodeError::WrongPadding);
                }

                *d = (b0 << 2) | (b1 >> 4);
                *d.add(1) = ((b1 & 0x0f) << 4) | (b2 >> 2);
            }

            2 => {
                if c2 != b'=' || c3 != b'=' {
                    return Err(Base64DecodeError::WrongPadding);
                }

                // Canonical Base64 requires unused bits to be zero.
                if b1 & 0x0f != 0 {
                    return Err(Base64DecodeError::WrongPadding);
                }

                *d = (b0 << 2) | (b1 >> 4);
            }

            _ => unreachable!(),
        }

        out.set_len(out_len);
    }

    Ok(out)
}