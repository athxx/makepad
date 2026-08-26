use viso_vid::VID;
use std::{collections::HashMap, hash::Hash, mem::MaybeUninit, ptr, str};

#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

#[inline(always)]
fn span_fits(offset: usize, len: usize, total: usize) -> bool {
    offset <= total && len <= total - offset
}

#[inline(always)]
unsafe fn append_bytes_unchecked(out: &mut Vec<u8>, src: *const u8, len: usize) {
    let old_len = out.len();
    debug_assert!(len <= out.capacity() - old_len);
    unsafe {
        ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(old_len), len);
        out.set_len(old_len + len);
    }
}

#[inline(always)]
fn append_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.reserve(bytes.len());
    unsafe { append_bytes_unchecked(out, bytes.as_ptr(), bytes.len()) }
}

#[inline(always)]
fn append_u64_le(out: &mut Vec<u8>, value: u64) {
    append_bytes(out, &value.to_le_bytes());
}

#[inline(always)]
const fn sat_add_usize(a: usize, b: usize) -> usize {
    if a > usize::MAX - b {
        usize::MAX
    } else {
        a + b
    }
}

#[inline(always)]
const fn sat_mul_usize(a: usize, b: usize) -> usize {
    if a == 0 || b == 0 {
        0
    } else if a > usize::MAX / b {
        usize::MAX
    } else {
        a * b
    }
}

pub trait EnBin {
    #[inline]
    fn encode_bin(&self) -> Vec<u8> {
        // The in-memory size is a cheap, allocation-free starting estimate for
        // derived structs. Variable-sized built-ins reserve a tighter amount in
        // their own `en_bin` implementations.
        let mut out = Vec::with_capacity(std::mem::size_of_val(self));
        self.en_bin(&mut out);
        out
    }

    fn en_bin(&self, out: &mut Vec<u8>);

    /// Cheap estimate used by enclosing containers to reserve once. The
    /// default deliberately avoids walking arbitrary user values.
    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        std::mem::size_of_val(self)
    }

    /// Estimate for a contiguous slice. POD and variable-sized standard types
    /// override this when they can provide a better answer cheaply.
    #[inline]
    fn en_bin_slice_size_hint(slice: &[Self]) -> usize
    where
        Self: Sized,
    {
        std::mem::size_of_val(slice)
    }

    /// Encode a contiguous slice. Scalar POD types override this with one
    /// bulk copy on little-endian targets.
    #[inline]
    fn en_bin_slice(slice: &[Self], out: &mut Vec<u8>)
    where
        Self: Sized,
    {
        for item in slice {
            item.en_bin(out);
        }
    }
}

pub trait DeBin: Sized {
    /// Minimum number of wire bytes consumed by one value. Zero means unknown.
    /// Container implementations use this to reject impossible lengths and
    /// reserve their destination exactly before decoding.
    const MIN_BIN_SIZE: usize = 0;

    #[inline]
    fn decode_bin(data: &[u8]) -> Result<Self, DeBinErr> {
        let mut offset = 0;
        Self::de_bin(&mut offset, data)
    }

    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr>;

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<Self>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        if count == 0 {
            return Ok(());
        }
        if *offset > data.len() {
            return Err(de_bin_error(*offset, 1, data.len(), "Vec length exceeds buffer"));
        }

        let remaining = data.len() - *offset;
        if Self::MIN_BIN_SIZE != 0 {
            if count > remaining / Self::MIN_BIN_SIZE {
                return Err(de_bin_error(*offset, Self::MIN_BIN_SIZE, data.len(), "Vec length exceeds buffer"));
            }
            out.reserve(count);
            for _ in 0..count {
                out.push(Self::de_bin(offset, data)?);
            }
            return Ok(());
        }

        // Unknown custom types get one probe. If it consumes no bytes, cap the
        // count absolutely; otherwise the input byte count safely bounds the
        // allocation and loop count.
        out.reserve(count.min(1024));
        let start = *offset;
        out.push(Self::de_bin(offset, data)?);
        let consumed = (*offset).saturating_sub(start);
        let max_count = if consumed == 0 {
            DE_BIN_MAX_ZERO_SIZED_LEN
        } else {
            data.len().saturating_sub(start)
        };
        if count > max_count {
            return Err(de_bin_error(start, 1, data.len(), "Vec length exceeds buffer"));
        }
        out.reserve(count.saturating_sub(out.len()));
        for _ in 1..count {
            out.push(Self::de_bin(offset, data)?);
        }
        Ok(())
    }

    /// Decode a fixed-size array. POD implementations override this with a
    /// single copy; the default uses a drop guard so partially initialized
    /// arrays are cleaned up on errors and panics.
    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[Self; N], DeBinErr> {
        de_bin_array_default::<Self, N>(offset, data)
    }
}

struct ArrayInitGuard<T> {
    ptr: *mut T,
    initialized: usize,
}

impl<T> Drop for ArrayInitGuard<T> {
    fn drop(&mut self) {
        unsafe {
            for index in 0..self.initialized {
                ptr::drop_in_place(self.ptr.add(index));
            }
        }
    }
}

#[inline]
fn de_bin_array_default<T: DeBin, const N: usize>(
    offset: &mut usize,
    data: &[u8],
) -> Result<[T; N], DeBinErr> {
    if T::MIN_BIN_SIZE != 0 {
        if *offset > data.len() || N > (data.len() - *offset) / T::MIN_BIN_SIZE {
            return Err(de_bin_error(*offset, T::MIN_BIN_SIZE, data.len(), "array length exceeds buffer"));
        }
    }

    let mut out = MaybeUninit::<[T; N]>::uninit();
    let top = out.as_mut_ptr().cast::<T>();
    let mut guard = ArrayInitGuard {
        ptr: top,
        initialized: 0,
    };

    for index in 0..N {
        unsafe { top.add(index).write(T::de_bin(offset, data)?) };
        guard.initialized += 1;
    }

    std::mem::forget(guard);
    Ok(unsafe { out.assume_init() })
}

pub struct DeBinErr {
    pub msg: String,
    pub o: usize,
    pub l: usize,
    pub s: usize,
}

impl std::fmt::Display for DeBinErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error decoding {} ", self.msg)?;
        if self.l != 0 {
            write!(f, "while trying to read {} bytes ", self.l)?
        }
        write!(f, " at offset {} in buffer of size {}", self.o, self.s)
    }
}

impl std::fmt::Debug for DeBinErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[cold]
#[inline(never)]
fn de_bin_error(offset: usize, len: usize, size: usize, message: &'static str) -> DeBinErr {
    DeBinErr {
        msg: message.to_owned(),
        o: offset,
        l: len,
        s: size,
    }
}

#[inline(always)]
fn remaining_bytes(offset: usize, data: &[u8]) -> Option<usize> {
    if offset <= data.len() {
        Some(data.len() - offset)
    } else {
        None
    }
}

macro_rules! impl_en_de_bin_for {
    ($ty:ident) => {
        impl EnBin for $ty {
            #[inline(always)]
            fn en_bin(&self, out: &mut Vec<u8>) {
                append_bytes(out, &self.to_le_bytes());
            }

            #[inline(always)]
            fn en_bin_size_hint(&self) -> usize {
                std::mem::size_of::<$ty>()
            }

            #[inline(always)]
            fn en_bin_slice_size_hint(slice: &[$ty]) -> usize {
                std::mem::size_of_val(slice)
            }

            #[cfg(target_endian = "little")]
            #[inline]
            fn en_bin_slice(slice: &[$ty], out: &mut Vec<u8>) {
                let byte_len = std::mem::size_of_val(slice);
                out.reserve(byte_len);
                unsafe {
                    append_bytes_unchecked(out, slice.as_ptr().cast::<u8>(), byte_len);
                }
            }

            #[cfg(target_endian = "big")]
            #[inline]
            fn en_bin_slice(slice: &[$ty], out: &mut Vec<u8>) {
                let byte_len = std::mem::size_of_val(slice);
                out.reserve(byte_len);
                for value in slice {
                    let bytes = value.to_le_bytes();
                    unsafe { append_bytes_unchecked(out, bytes.as_ptr(), bytes.len()) };
                }
            }
        }

        impl DeBin for $ty {
            const MIN_BIN_SIZE: usize = std::mem::size_of::<$ty>();

            #[inline(always)]
            fn de_bin(offset: &mut usize, data: &[u8]) -> Result<$ty, DeBinErr> {
                const WIDTH: usize = std::mem::size_of::<$ty>();
                let start = *offset;
                if !span_fits(start, WIDTH, data.len()) {
                    return Err(de_bin_error(start, WIDTH, data.len(), stringify!($ty)));
                }
                let bytes = unsafe {
                    (data.as_ptr().add(start) as *const [u8; WIDTH]).read_unaligned()
                };
                *offset = start + WIDTH;
                Ok(<$ty>::from_le_bytes(bytes))
            }

            #[cfg(target_endian = "little")]
            #[inline]
            fn de_bin_slice(
                count: usize,
                out: &mut Vec<$ty>,
                offset: &mut usize,
                data: &[u8],
            ) -> Result<(), DeBinErr> {
                const WIDTH: usize = std::mem::size_of::<$ty>();
                let start = *offset;
                let remaining = remaining_bytes(start, data).ok_or_else(|| {
                    de_bin_error(start, WIDTH, data.len(), concat!("Vec<", stringify!($ty), "> length exceeds buffer"))
                })?;
                if count > remaining / WIDTH {
                    return Err(de_bin_error(
                        start,
                        WIDTH,
                        data.len(),
                        concat!("Vec<", stringify!($ty), "> length exceeds buffer"),
                    ));
                }
                let byte_len = count * WIDTH;
                let old_len = out.len();
                out.reserve(count);
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr().add(start),
                        out.as_mut_ptr().add(old_len).cast::<u8>(),
                        byte_len,
                    );
                    out.set_len(old_len + count);
                }
                *offset = start + byte_len;
                Ok(())
            }

            #[cfg(target_endian = "big")]
            #[inline]
            fn de_bin_slice(
                count: usize,
                out: &mut Vec<$ty>,
                offset: &mut usize,
                data: &[u8],
            ) -> Result<(), DeBinErr> {
                const WIDTH: usize = std::mem::size_of::<$ty>();
                let start = *offset;
                let remaining = remaining_bytes(start, data).ok_or_else(|| {
                    de_bin_error(start, WIDTH, data.len(), concat!("Vec<", stringify!($ty), "> length exceeds buffer"))
                })?;
                if count > remaining / WIDTH {
                    return Err(de_bin_error(
                        start,
                        WIDTH,
                        data.len(),
                        concat!("Vec<", stringify!($ty), "> length exceeds buffer"),
                    ));
                }
                let old_len = out.len();
                out.reserve(count);
                let dst = unsafe { out.as_mut_ptr().add(old_len) };
                for index in 0..count {
                    let source_offset = start + index * WIDTH;
                    let bytes = unsafe {
                        (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
                    };
                    unsafe { dst.add(index).write(<$ty>::from_le_bytes(bytes)) };
                }
                unsafe { out.set_len(old_len + count) };
                *offset = start + count * WIDTH;
                Ok(())
            }

            #[cfg(target_endian = "little")]
            #[inline]
            fn de_bin_array<const N: usize>(
                offset: &mut usize,
                data: &[u8],
            ) -> Result<[$ty; N], DeBinErr> {
                const WIDTH: usize = std::mem::size_of::<$ty>();
                let start = *offset;
                let remaining = remaining_bytes(start, data)
                    .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), stringify!($ty)))?;
                if N > remaining / WIDTH {
                    return Err(de_bin_error(start, WIDTH, data.len(), stringify!($ty)));
                }
                let byte_len = N * WIDTH;
                let mut out = MaybeUninit::<[$ty; N]>::uninit();
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr().add(start),
                        out.as_mut_ptr().cast::<u8>(),
                        byte_len,
                    );
                }
                *offset = start + byte_len;
                Ok(unsafe { out.assume_init() })
            }

            #[cfg(target_endian = "big")]
            #[inline]
            fn de_bin_array<const N: usize>(
                offset: &mut usize,
                data: &[u8],
            ) -> Result<[$ty; N], DeBinErr> {
                const WIDTH: usize = std::mem::size_of::<$ty>();
                let start = *offset;
                let remaining = remaining_bytes(start, data)
                    .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), stringify!($ty)))?;
                if N > remaining / WIDTH {
                    return Err(de_bin_error(start, WIDTH, data.len(), stringify!($ty)));
                }
                let mut out = MaybeUninit::<[$ty; N]>::uninit();
                let dst = out.as_mut_ptr().cast::<$ty>();
                for index in 0..N {
                    let source_offset = start + index * WIDTH;
                    let bytes = unsafe {
                        (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
                    };
                    unsafe { dst.add(index).write(<$ty>::from_le_bytes(bytes)) };
                }
                *offset = start + N * WIDTH;
                Ok(unsafe { out.assume_init() })
            }
        }
    };
}

impl_en_de_bin_for!(f64);
impl_en_de_bin_for!(f32);
impl_en_de_bin_for!(u64);
impl_en_de_bin_for!(i64);
impl_en_de_bin_for!(u32);
impl_en_de_bin_for!(i32);
impl_en_de_bin_for!(u16);
impl_en_de_bin_for!(i16);

impl EnBin for usize {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        append_u64_le(out, *self as u64);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        8
    }

    #[inline(always)]
    fn en_bin_slice_size_hint(slice: &[usize]) -> usize {
        sat_mul_usize(slice.len(), 8)
    }

    #[inline]
    fn en_bin_slice(slice: &[usize], out: &mut Vec<u8>) {
        let byte_len = sat_mul_usize(slice.len(), 8);
        out.reserve(byte_len);

        #[cfg(all(target_pointer_width = "64", target_endian = "little"))]
        unsafe {
            append_bytes_unchecked(out, slice.as_ptr().cast::<u8>(), byte_len);
        }

        #[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
        for value in slice {
            let bytes = (*value as u64).to_le_bytes();
            unsafe { append_bytes_unchecked(out, bytes.as_ptr(), 8) };
        }
    }
}

impl DeBin for usize {
    const MIN_BIN_SIZE: usize = 8;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<usize, DeBinErr> {
        let start = *offset;
        let value = u64::de_bin(offset, data)?;
        if value > usize::MAX as u64 {
            return Err(de_bin_error(start, 8, data.len(), "usize"));
        }
        Ok(value as usize)
    }

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<usize>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        const WIDTH: usize = 8;
        let start = *offset;
        let remaining = remaining_bytes(start, data)
            .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), "Vec<usize> length exceeds buffer"))?;
        if count > remaining / WIDTH {
            return Err(de_bin_error(start, WIDTH, data.len(), "Vec<usize> length exceeds buffer"));
        }
        let byte_len = count * WIDTH;
        let old_len = out.len();
        out.reserve(count);

        #[cfg(all(target_pointer_width = "64", target_endian = "little"))]
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr().add(start),
                out.as_mut_ptr().add(old_len).cast::<u8>(),
                byte_len,
            );
            out.set_len(old_len + count);
        }

        #[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
        {
            let dst = unsafe { out.as_mut_ptr().add(old_len) };
            for index in 0..count {
                let source_offset = start + index * WIDTH;
                let bytes = unsafe {
                    (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
                };
                let value = u64::from_le_bytes(bytes);
                if value > usize::MAX as u64 {
                    return Err(de_bin_error(source_offset, WIDTH, data.len(), "usize"));
                }
                unsafe { dst.add(index).write(value as usize) };
            }
            unsafe { out.set_len(old_len + count) };
        }

        *offset = start + byte_len;
        Ok(())
    }

    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[usize; N], DeBinErr> {
        const WIDTH: usize = 8;
        let start = *offset;
        let remaining = remaining_bytes(start, data)
            .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), "usize"))?;
        if N > remaining / WIDTH {
            return Err(de_bin_error(start, WIDTH, data.len(), "usize"));
        }
        let mut out = MaybeUninit::<[usize; N]>::uninit();

        #[cfg(all(target_pointer_width = "64", target_endian = "little"))]
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr().add(start),
                out.as_mut_ptr().cast::<u8>(),
                N * WIDTH,
            );
        }

        #[cfg(not(all(target_pointer_width = "64", target_endian = "little")))]
        {
            let dst = out.as_mut_ptr().cast::<usize>();
            for index in 0..N {
                let source_offset = start + index * WIDTH;
                let bytes = unsafe {
                    (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
                };
                let value = u64::from_le_bytes(bytes);
                if value > usize::MAX as u64 {
                    return Err(de_bin_error(source_offset, WIDTH, data.len(), "usize"));
                }
                unsafe { dst.add(index).write(value as usize) };
            }
        }

        *offset = start + N * WIDTH;
        Ok(unsafe { out.assume_init() })
    }
}

impl EnBin for VID {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.0.en_bin(out);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        8
    }

    #[inline(always)]
    fn en_bin_slice_size_hint(slice: &[VID]) -> usize {
        sat_mul_usize(slice.len(), 8)
    }

    #[inline]
    fn en_bin_slice(slice: &[VID], out: &mut Vec<u8>) {
        out.reserve(sat_mul_usize(slice.len(), 8));
        for value in slice {
            let bytes = value.0.to_le_bytes();
            unsafe { append_bytes_unchecked(out, bytes.as_ptr(), 8) };
        }
    }
}

impl DeBin for VID {
    const MIN_BIN_SIZE: usize = 8;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<VID, DeBinErr> {
        Ok(VID(u64::de_bin(offset, data)?))
    }

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<VID>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        const WIDTH: usize = 8;
        let start = *offset;
        let remaining = remaining_bytes(start, data)
            .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), "Vec<VID> length exceeds buffer"))?;
        if count > remaining / WIDTH {
            return Err(de_bin_error(start, WIDTH, data.len(), "Vec<VID> length exceeds buffer"));
        }
        let old_len = out.len();
        out.reserve(count);
        let dst = unsafe { out.as_mut_ptr().add(old_len) };
        for index in 0..count {
            let source_offset = start + index * WIDTH;
            let bytes = unsafe {
                (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
            };
            unsafe { dst.add(index).write(VID(u64::from_le_bytes(bytes))) };
        }
        unsafe { out.set_len(old_len + count) };
        *offset = start + count * WIDTH;
        Ok(())
    }

    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[VID; N], DeBinErr> {
        const WIDTH: usize = 8;
        let start = *offset;
        let remaining = remaining_bytes(start, data)
            .ok_or_else(|| de_bin_error(start, WIDTH, data.len(), "VID"))?;
        if N > remaining / WIDTH {
            return Err(de_bin_error(start, WIDTH, data.len(), "VID"));
        }
        let mut out = MaybeUninit::<[VID; N]>::uninit();
        let dst = out.as_mut_ptr().cast::<VID>();
        for index in 0..N {
            let source_offset = start + index * WIDTH;
            let bytes = unsafe {
                (data.as_ptr().add(source_offset) as *const [u8; WIDTH]).read_unaligned()
            };
            unsafe { dst.add(index).write(VID(u64::from_le_bytes(bytes))) };
        }
        *offset = start + N * WIDTH;
        Ok(unsafe { out.assume_init() })
    }
}

impl EnBin for u8 {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        out.push(*self);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        1
    }

    #[inline(always)]
    fn en_bin_slice_size_hint(slice: &[u8]) -> usize {
        slice.len()
    }

    #[inline]
    fn en_bin_slice(slice: &[u8], out: &mut Vec<u8>) {
        append_bytes(out, slice);
    }
}

impl DeBin for u8 {
    const MIN_BIN_SIZE: usize = 1;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<u8, DeBinErr> {
        let start = *offset;
        if start >= data.len() {
            return Err(de_bin_error(start, 1, data.len(), "u8"));
        }
        let value = unsafe { *data.get_unchecked(start) };
        *offset = start + 1;
        Ok(value)
    }

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<u8>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        let start = *offset;
        if !span_fits(start, count, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "Vec<u8> length exceeds buffer"));
        }
        out.reserve(count);
        unsafe { append_bytes_unchecked(out, data.as_ptr().add(start), count) };
        *offset = start + count;
        Ok(())
    }

    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[u8; N], DeBinErr> {
        let start = *offset;
        if !span_fits(start, N, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "u8"));
        }
        let mut out = MaybeUninit::<[u8; N]>::uninit();
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr().add(start), out.as_mut_ptr().cast::<u8>(), N);
        }
        *offset = start + N;
        Ok(unsafe { out.assume_init() })
    }
}

impl EnBin for i8 {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        1
    }

    #[inline(always)]
    fn en_bin_slice_size_hint(slice: &[i8]) -> usize {
        slice.len()
    }

    #[inline]
    fn en_bin_slice(slice: &[i8], out: &mut Vec<u8>) {
        out.reserve(slice.len());
        unsafe { append_bytes_unchecked(out, slice.as_ptr().cast::<u8>(), slice.len()) };
    }
}

impl DeBin for i8 {
    const MIN_BIN_SIZE: usize = 1;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<i8, DeBinErr> {
        Ok(u8::de_bin(offset, data)? as i8)
    }

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<i8>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        let start = *offset;
        if !span_fits(start, count, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "Vec<i8> length exceeds buffer"));
        }
        let old_len = out.len();
        out.reserve(count);
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr().add(start),
                out.as_mut_ptr().add(old_len).cast::<u8>(),
                count,
            );
            out.set_len(old_len + count);
        }
        *offset = start + count;
        Ok(())
    }

    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[i8; N], DeBinErr> {
        let start = *offset;
        if !span_fits(start, N, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "i8"));
        }
        let mut out = MaybeUninit::<[i8; N]>::uninit();
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr().add(start), out.as_mut_ptr().cast::<u8>(), N);
        }
        *offset = start + N;
        Ok(unsafe { out.assume_init() })
    }
}

impl EnBin for bool {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        1
    }

    #[inline(always)]
    fn en_bin_slice_size_hint(slice: &[bool]) -> usize {
        slice.len()
    }

    #[inline]
    fn en_bin_slice(slice: &[bool], out: &mut Vec<u8>) {
        debug_assert_eq!(std::mem::size_of::<bool>(), 1);
        out.reserve(slice.len());
        unsafe { append_bytes_unchecked(out, slice.as_ptr().cast::<u8>(), slice.len()) };
    }
}

impl DeBin for bool {
    const MIN_BIN_SIZE: usize = 1;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<bool, DeBinErr> {
        Ok(u8::de_bin(offset, data)? != 0)
    }

    #[inline]
    fn de_bin_slice(
        count: usize,
        out: &mut Vec<bool>,
        offset: &mut usize,
        data: &[u8],
    ) -> Result<(), DeBinErr> {
        let start = *offset;
        if !span_fits(start, count, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "Vec<bool> length exceeds buffer"));
        }
        let old_len = out.len();
        out.reserve(count);
        let dst = unsafe { out.as_mut_ptr().add(old_len) };
        for index in 0..count {
            let value = unsafe { *data.get_unchecked(start + index) != 0 };
            unsafe { dst.add(index).write(value) };
        }
        unsafe { out.set_len(old_len + count) };
        *offset = start + count;
        Ok(())
    }

    #[inline]
    fn de_bin_array<const N: usize>(
        offset: &mut usize,
        data: &[u8],
    ) -> Result<[bool; N], DeBinErr> {
        let start = *offset;
        if !span_fits(start, N, data.len()) {
            return Err(de_bin_error(start, 1, data.len(), "bool"));
        }
        let mut out = MaybeUninit::<[bool; N]>::uninit();
        let dst = out.as_mut_ptr().cast::<bool>();
        for index in 0..N {
            let value = unsafe { *data.get_unchecked(start + index) != 0 };
            unsafe { dst.add(index).write(value) };
        }
        *offset = start + N;
        Ok(unsafe { out.assume_init() })
    }
}

impl EnBin for String {
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        let required = sat_add_usize(8, bytes.len());
        out.reserve(required);
        let len_bytes = (bytes.len() as u64).to_le_bytes();
        unsafe {
            append_bytes_unchecked(out, len_bytes.as_ptr(), 8);
            append_bytes_unchecked(out, bytes.as_ptr(), bytes.len());
        }
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(8, self.len())
    }

    #[inline]
    fn en_bin_slice_size_hint(slice: &[String]) -> usize {
        slice.iter().fold(0usize, |total, value| {
            sat_add_usize(total, sat_add_usize(8, value.len()))
        })
    }
}

impl DeBin for String {
    const MIN_BIN_SIZE: usize = 8;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<String, DeBinErr> {
        let len_offset = *offset;
        let wire_len = u64::de_bin(offset, data)?;
        if wire_len > usize::MAX as u64 {
            return Err(de_bin_error(len_offset, 8, data.len(), "String"));
        }
        let len = wire_len as usize;
        let start = *offset;
        if !span_fits(start, len, data.len()) {
            return Err(de_bin_error(start, len.max(1), data.len(), "String"));
        }
        let end = start + len;
        let bytes = &data[start..end];
        str::from_utf8(bytes)
            .map_err(|_| de_bin_error(start, len, data.len(), "String is not valid utf8"))?;
        let owned = bytes.to_vec();
        *offset = end;
        Ok(unsafe { String::from_utf8_unchecked(owned) })
    }
}

impl<T> EnBin for Vec<T>
where
    T: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let payload_hint = T::en_bin_slice_size_hint(self);
        if let Some(required) = payload_hint.checked_add(8) {
            out.reserve(required);
        }
        append_u64_le(out, self.len() as u64);
        T::en_bin_slice(self, out);
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(8, T::en_bin_slice_size_hint(self))
    }

    #[inline]
    fn en_bin_slice_size_hint(slice: &[Vec<T>]) -> usize {
        slice.iter().fold(0usize, |total, value| {
            sat_add_usize(total, value.en_bin_size_hint())
        })
    }
}

/// Iteration ceiling for values whose custom decoder consumes no bytes.
const DE_BIN_MAX_ZERO_SIZED_LEN: usize = 1 << 20;

impl<T> DeBin for Vec<T>
where
    T: DeBin,
{
    const MIN_BIN_SIZE: usize = 8;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Vec<T>, DeBinErr> {
        let len_offset = *offset;
        let wire_len = u64::de_bin(offset, data)?;
        if wire_len > usize::MAX as u64 {
            return Err(de_bin_error(len_offset, 8, data.len(), "Vec length exceeds usize"));
        }
        let count = wire_len as usize;
        let mut out = Vec::new();
        T::de_bin_slice(count, &mut out, offset, data)?;
        Ok(out)
    }
}

impl<T> EnBin for Option<T>
where
    T: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(value) => {
                out.push(1);
                value.en_bin(out);
            }
        }
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        match self {
            None => 1,
            Some(value) => sat_add_usize(1, value.en_bin_size_hint()),
        }
    }

    #[inline]
    fn en_bin_slice_size_hint(slice: &[Option<T>]) -> usize {
        slice.iter().fold(0usize, |total, value| {
            sat_add_usize(total, value.en_bin_size_hint())
        })
    }
}

impl<T> DeBin for Option<T>
where
    T: DeBin,
{
    const MIN_BIN_SIZE: usize = 1;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Option<T>, DeBinErr> {
        let tag_offset = *offset;
        match u8::de_bin(offset, data)? {
            0 => Ok(None),
            1 => Ok(Some(T::de_bin(offset, data)?)),
            _ => Err(de_bin_error(tag_offset, 1, data.len(), "Option<T>")),
        }
    }
}

impl<T, E> EnBin for Result<T, E>
where
    T: EnBin,
    E: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        match self {
            Ok(value) => {
                out.push(0);
                value.en_bin(out);
            }
            Err(error) => {
                out.push(1);
                error.en_bin(out);
            }
        }
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        match self {
            Ok(value) => sat_add_usize(1, value.en_bin_size_hint()),
            Err(error) => sat_add_usize(1, error.en_bin_size_hint()),
        }
    }

    #[inline]
    fn en_bin_slice_size_hint(slice: &[Result<T, E>]) -> usize {
        slice.iter().fold(0usize, |total, value| {
            sat_add_usize(total, value.en_bin_size_hint())
        })
    }
}

impl<T, E> DeBin for Result<T, E>
where
    T: DeBin,
    E: DeBin,
{
    const MIN_BIN_SIZE: usize = 1;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        let tag_offset = *offset;
        match u8::de_bin(offset, data)? {
            0 => Ok(Ok(T::de_bin(offset, data)?)),
            1 => Ok(Err(E::de_bin(offset, data)?)),
            _ => Err(de_bin_error(tag_offset, 1, data.len(), "Result<T, E>")),
        }
    }
}

impl<T> EnBin for [T]
where
    T: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let hint = T::en_bin_slice_size_hint(self);
        if hint != usize::MAX {
            out.reserve(hint);
        }
        T::en_bin_slice(self, out);
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        T::en_bin_slice_size_hint(self)
    }
}

impl<T, const N: usize> EnBin for [T; N]
where
    T: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let hint = T::en_bin_slice_size_hint(self);
        if hint != usize::MAX {
            out.reserve(hint);
        }
        T::en_bin_slice(self, out);
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        T::en_bin_slice_size_hint(self)
    }
}

impl<T, const N: usize> DeBin for [T; N]
where
    T: DeBin,
{
    const MIN_BIN_SIZE: usize = sat_mul_usize(T::MIN_BIN_SIZE, N);

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        T::de_bin_array::<N>(offset, data)
    }
}

impl<A, B> EnBin for (A, B)
where
    A: EnBin,
    B: EnBin,
{
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.0.en_bin(out);
        self.1.en_bin(out);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(self.0.en_bin_size_hint(), self.1.en_bin_size_hint())
    }
}

impl<A, B> DeBin for (A, B)
where
    A: DeBin,
    B: DeBin,
{
    const MIN_BIN_SIZE: usize = sat_add_usize(A::MIN_BIN_SIZE, B::MIN_BIN_SIZE);

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        Ok((
            A::de_bin(offset, data)?,
            B::de_bin(offset, data)?,
        ))
    }
}

impl<A, B, C> EnBin for (A, B, C)
where
    A: EnBin,
    B: EnBin,
    C: EnBin,
{
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.0.en_bin(out);
        self.1.en_bin(out);
        self.2.en_bin(out);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(sat_add_usize(self.0.en_bin_size_hint(), self.1.en_bin_size_hint()), self.2.en_bin_size_hint())
    }
}

impl<A, B, C> DeBin for (A, B, C)
where
    A: DeBin,
    B: DeBin,
    C: DeBin,
{
    const MIN_BIN_SIZE: usize = sat_add_usize(sat_add_usize(A::MIN_BIN_SIZE, B::MIN_BIN_SIZE), C::MIN_BIN_SIZE);

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        Ok((
            A::de_bin(offset, data)?,
            B::de_bin(offset, data)?,
            C::de_bin(offset, data)?,
        ))
    }
}

impl<A, B, C, D> EnBin for (A, B, C, D)
where
    A: EnBin,
    B: EnBin,
    C: EnBin,
    D: EnBin,
{
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.0.en_bin(out);
        self.1.en_bin(out);
        self.2.en_bin(out);
        self.3.en_bin(out);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(sat_add_usize(sat_add_usize(self.0.en_bin_size_hint(), self.1.en_bin_size_hint()), self.2.en_bin_size_hint()), self.3.en_bin_size_hint())
    }
}

impl<A, B, C, D> DeBin for (A, B, C, D)
where
    A: DeBin,
    B: DeBin,
    C: DeBin,
    D: DeBin,
{
    const MIN_BIN_SIZE: usize = sat_add_usize(sat_add_usize(sat_add_usize(A::MIN_BIN_SIZE, B::MIN_BIN_SIZE), C::MIN_BIN_SIZE), D::MIN_BIN_SIZE);

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        Ok((
            A::de_bin(offset, data)?,
            B::de_bin(offset, data)?,
            C::de_bin(offset, data)?,
            D::de_bin(offset, data)?,
        ))
    }
}

impl<A, B, C, D, E> EnBin for (A, B, C, D, E)
where
    A: EnBin,
    B: EnBin,
    C: EnBin,
    D: EnBin,
    E: EnBin,
{
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.0.en_bin(out);
        self.1.en_bin(out);
        self.2.en_bin(out);
        self.3.en_bin(out);
        self.4.en_bin(out);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        sat_add_usize(sat_add_usize(sat_add_usize(sat_add_usize(self.0.en_bin_size_hint(), self.1.en_bin_size_hint()), self.2.en_bin_size_hint()), self.3.en_bin_size_hint()), self.4.en_bin_size_hint())
    }
}

impl<A, B, C, D, E> DeBin for (A, B, C, D, E)
where
    A: DeBin,
    B: DeBin,
    C: DeBin,
    D: DeBin,
    E: DeBin,
{
    const MIN_BIN_SIZE: usize = sat_add_usize(sat_add_usize(sat_add_usize(sat_add_usize(A::MIN_BIN_SIZE, B::MIN_BIN_SIZE), C::MIN_BIN_SIZE), D::MIN_BIN_SIZE), E::MIN_BIN_SIZE);

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        Ok((
            A::de_bin(offset, data)?,
            B::de_bin(offset, data)?,
            C::de_bin(offset, data)?,
            D::de_bin(offset, data)?,
            E::de_bin(offset, data)?,
        ))
    }
}

impl<K, V> EnBin for HashMap<K, V>
where
    K: EnBin,
    V: EnBin,
{
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let estimate = self.en_bin_size_hint();
        if estimate != usize::MAX {
            out.reserve(estimate);
        }
        append_u64_le(out, self.len() as u64);
        for (key, value) in self {
            key.en_bin(out);
            value.en_bin(out);
        }
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        let per_entry = sat_add_usize(std::mem::size_of::<K>(), std::mem::size_of::<V>());
        sat_add_usize(8, sat_mul_usize(self.len(), per_entry))
    }
}

impl<K, V> DeBin for HashMap<K, V>
where
    K: DeBin + Eq + Hash,
    V: DeBin,
{
    const MIN_BIN_SIZE: usize = 8;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        let len_offset = *offset;
        let wire_len = u64::de_bin(offset, data)?;
        if wire_len > usize::MAX as u64 {
            return Err(de_bin_error(len_offset, 8, data.len(), "HashMap length exceeds usize"));
        }
        let count = wire_len as usize;
        if count == 0 {
            return Ok(HashMap::new());
        }
        if *offset > data.len() {
            return Err(de_bin_error(*offset, 1, data.len(), "HashMap length exceeds buffer"));
        }

        let entry_min = sat_add_usize(K::MIN_BIN_SIZE, V::MIN_BIN_SIZE);
        if entry_min != 0 {
            let remaining = data.len() - *offset;
            if count > remaining / entry_min {
                return Err(de_bin_error(*offset, entry_min, data.len(), "HashMap length exceeds buffer"));
            }
            let mut map = HashMap::with_capacity(count);
            for _ in 0..count {
                let key = K::de_bin(offset, data)?;
                let value = V::de_bin(offset, data)?;
                map.insert(key, value);
            }
            return Ok(map);
        }

        // Unknown custom key/value decoders get the same bounded probe used by
        // Vec<T>, avoiding attacker-controlled allocation before one entry has
        // demonstrated whether it consumes input.
        let start = *offset;
        let first_key = K::de_bin(offset, data)?;
        let first_value = V::de_bin(offset, data)?;
        let consumed = (*offset).saturating_sub(start);
        let max_count = if consumed == 0 {
            DE_BIN_MAX_ZERO_SIZED_LEN
        } else {
            data.len().saturating_sub(start)
        };
        if count > max_count {
            return Err(de_bin_error(start, 1, data.len(), "HashMap length exceeds buffer"));
        }

        let mut map = HashMap::with_capacity(count);
        map.insert(first_key, first_value);
        for _ in 1..count {
            let key = K::de_bin(offset, data)?;
            let value = V::de_bin(offset, data)?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

impl<T> EnBin for Box<T>
where
    T: EnBin + ?Sized,
{
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        (**self).en_bin(out)
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        (**self).en_bin_size_hint()
    }
}

impl<T> DeBin for Box<T>
where
    T: DeBin,
{
    const MIN_BIN_SIZE: usize = T::MIN_BIN_SIZE;

    #[inline(always)]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Box<T>, DeBinErr> {
        Ok(Box::new(T::de_bin(offset, data)?))
    }
}

#[cfg(unix)]
impl EnBin for PathBuf {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.as_os_str().en_bin(out)
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        self.as_os_str().en_bin_size_hint()
    }
}

#[cfg(unix)]
impl EnBin for Path {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.as_os_str().en_bin(out)
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        self.as_os_str().en_bin_size_hint()
    }
}

#[cfg(unix)]
impl EnBin for OsString {
    #[inline(always)]
    fn en_bin(&self, out: &mut Vec<u8>) {
        self.as_os_str().en_bin(out)
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        self.as_os_str().en_bin_size_hint()
    }
}

#[cfg(unix)]
impl EnBin for OsStr {
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        append_bytes(out, self.as_bytes());
    }

    #[inline]
    fn en_bin_size_hint(&self) -> usize {
        use std::os::unix::ffi::OsStrExt;
        self.as_bytes().len()
    }
}

impl EnBin for char {
    #[inline]
    fn en_bin(&self, out: &mut Vec<u8>) {
        let mut buffer = [0u8; 4];
        let bytes = self.encode_utf8(&mut buffer).as_bytes();
        append_bytes(out, bytes);
    }

    #[inline(always)]
    fn en_bin_size_hint(&self) -> usize {
        self.len_utf8()
    }
}

impl DeBin for char {
    const MIN_BIN_SIZE: usize = 1;

    #[inline]
    fn de_bin(offset: &mut usize, data: &[u8]) -> Result<Self, DeBinErr> {
        let start = *offset;
        if start >= data.len() {
            return Err(de_bin_error(start, 1, data.len(), "char"));
        }
        let first = unsafe { *data.get_unchecked(start) };
        if first < 0x80 {
            *offset = start + 1;
            return Ok(first as char);
        }

        let width = utf8_char_width(first);
        if width == 0 || !span_fits(start, width, data.len()) {
            return Err(de_bin_error(start, width.max(1), data.len(), "char"));
        }
        let end = start + width;
        let text = str::from_utf8(&data[start..end])
            .map_err(|_| de_bin_error(start, width, data.len(), "char"))?;
        let value = match text.chars().next() {
            Some(value) => value,
            None => return Err(de_bin_error(start, width, data.len(), "char")),
        };
        *offset = end;
        Ok(value)
    }
}

/// Width of a valid UTF-8 leading byte; zero denotes a continuation or an
/// invalid/overlong leading byte.
#[inline(always)]
pub fn utf8_char_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod hostile_input_tests {
    use crate::*;

    /// `d` is a length prefix of `len` with no payload behind it.
    fn only_len(len: u64) -> Vec<u8> {
        len.to_le_bytes().to_vec()
    }

    #[test]
    fn string_rejects_invalid_utf8_instead_of_panicking() {
        let mut d = only_len(2);
        d.extend_from_slice(&[0xff, 0xfe]);
        let mut o = 0;
        assert!(String::de_bin(&mut o, &d).is_err());
    }

    #[test]
    fn string_rejects_length_beyond_buffer_without_overflowing() {
        for len in [4u64, u64::MAX, u64::MAX - 8, 1 << 40] {
            let d = only_len(len);
            let mut o = 0;
            assert!(String::de_bin(&mut o, &d).is_err(), "len {len}");
        }
    }

    #[test]
    fn string_roundtrips_valid_utf8() {
        let mut s = Vec::new();
        "hällo".to_string().en_bin(&mut s);
        let mut o = 0;
        assert_eq!(String::de_bin(&mut o, &s).unwrap(), "hällo");
        assert_eq!(o, s.len());
    }

    #[test]
    fn vec_rejects_length_beyond_buffer() {
        // One decodable u8 followed by a claim of billions more.
        let mut d = only_len(1 << 32);
        d.push(7);
        let mut o = 0;
        assert!(Vec::<u8>::de_bin(&mut o, &d).is_err());
    }

    #[test]
    fn vec_roundtrips_variable_size_elements() {
        // A long first element must not shrink the bound for later ones.
        let v: Vec<String> = std::iter::once("x".repeat(200))
            .chain((0..500).map(|_| "a".to_string()))
            .collect();
        let mut s = Vec::new();
        v.en_bin(&mut s);
        let mut o = 0;
        assert_eq!(Vec::<String>::de_bin(&mut o, &s).unwrap(), v);
    }

    /// A unit type encodes to nothing, so its count is unbounded by the
    /// buffer — the case the absolute ceiling exists for.
    #[derive(Debug)]
    struct Unit;

    impl DeBin for Unit {
        fn de_bin(_o: &mut usize, _d: &[u8]) -> Result<Unit, DeBinErr> {
            Ok(Unit)
        }
    }

    #[test]
    fn vec_of_zero_sized_elements_is_capped() {
        let d = only_len(u64::MAX);
        let mut o = 0;
        assert!(Vec::<Unit>::de_bin(&mut o, &d).is_err());
    }

    /// The bulk POD path must emit the exact same bytes as an element-by-element
    /// encode — this is the wire-format-compatibility guarantee that downstream
    /// crates (network + on-disk assets) depend on.
    #[test]
    fn pod_bulk_matches_element_wise_wire_format() {
        macro_rules! check {
            ($ty:ty, $vals:expr) => {{
                let v: Vec<$ty> = $vals;
                let bulk = v.encode_bin();

                // Reconstruct the reference bytes element by element.
                let mut manual = Vec::new();
                (v.len() as u64).en_bin(&mut manual);
                for it in &v {
                    it.en_bin(&mut manual);
                }
                assert_eq!(bulk, manual, "wire bytes diverge for {}", stringify!($ty));

                // Round-trips through the bulk de path.
                let back = Vec::<$ty>::decode_bin(&bulk).unwrap();
                assert_eq!(back, v, "roundtrip mismatch for {}", stringify!($ty));
            }};
        }
        check!(u8, vec![0, 1, 2, 254, 255]);
        check!(i8, vec![-128, -1, 0, 1, 127]);
        check!(u16, vec![0, 1, 0xffff, 0x1234]);
        check!(i16, vec![i16::MIN, -1, 0, 1, i16::MAX]);
        check!(u32, vec![0, 0xdead_beef, u32::MAX]);
        check!(i32, vec![i32::MIN, -1, 0, i32::MAX]);
        check!(u64, vec![0, 0x0123_4567_89ab_cdef, u64::MAX]);
        check!(i64, vec![i64::MIN, -1, 0, i64::MAX]);
        check!(f32, vec![0.0, -1.5, f32::consts_pi(), f32::MAX]);
        check!(f64, vec![0.0, -2.5, f64::consts_pi(), f64::MAX]);
    }

    // Small helpers to avoid pulling in std::f32::consts into the macro.
    trait ConstsPi {
        fn consts_pi() -> Self;
    }
    impl ConstsPi for f32 {
        fn consts_pi() -> f32 {
            std::f32::consts::PI
        }
    }
    impl ConstsPi for f64 {
        fn consts_pi() -> f64 {
            std::f64::consts::PI
        }
    }

    #[test]
    fn truncated_payloads_never_panic() {
        let mut full = Vec::new();
        vec!["alpha".to_string(), "beta".to_string()].en_bin(&mut full);
        for cut in 0..full.len() {
            let mut o = 0;
            let _ = Vec::<String>::de_bin(&mut o, &full[..cut]);
        }
    }
}
