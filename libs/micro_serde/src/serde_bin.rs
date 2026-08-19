use makepad_live_id::LiveId;
use std::{collections::HashMap, hash::Hash, str};

#[allow(unused_imports)]
#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios"
))]
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

pub trait SerBin {
    fn serialize_bin(&self) -> Vec<u8> {
        let mut s = Vec::new();
        self.ser_bin(&mut s);
        s
    }

    fn ser_bin(&self, s: &mut Vec<u8>);

    /// Serialize a contiguous slice of `Self`. The default loops element by
    /// element; scalar POD types override it to bulk-copy their little-endian
    /// bytes in one `memcpy` (see [`impl_ser_de_bin_for`]). Called by the
    /// `Vec<T>` / `[T]` / `[T; N]` impls so the fast path applies uniformly.
    #[inline]
    fn ser_bin_slice(slice: &[Self], s: &mut Vec<u8>)
    where
        Self: Sized,
    {
        for item in slice {
            item.ser_bin(s);
        }
    }
}

pub trait DeBin: Sized {
    fn deserialize_bin(d: &[u8]) -> Result<Self, DeBinErr> {
        DeBin::de_bin(&mut 0, d)
    }

    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr>;

    /// Deserialize `count` contiguous `Self`, appending them to `out`. The
    /// default loops element by element; scalar POD types override it to
    /// bulk-copy from the input in one pass after a single bounds check.
    /// Called by the `Vec<T>` impl (which has already read `count` from the
    /// wire).
    ///
    /// The default path hardens against hostile lengths: `count` is
    /// attacker-controlled, so the allocation is never sized directly from it,
    /// and a count the buffer cannot possibly back is rejected before the loop
    /// runs long. The POD overrides get an even stronger guarantee — an exact
    /// byte-span bounds check — so they skip this probe.
    #[inline]
    fn de_bin_slice(count: usize, out: &mut Vec<Self>, o: &mut usize, d: &[u8]) -> Result<(), DeBinErr>
    where
        Self: Sized,
    {
        if count == 0 {
            return Ok(());
        }
        // Never size the allocation from `count`. Every element that consumes
        // at least one byte puts the true ceiling at the remaining byte count;
        // measure the first element to tell that case apart from zero-sized
        // ones, whose count is bounded only by an absolute cap.
        out.reserve(count.min(1024));
        let start = *o;
        out.push(DeBin::de_bin(o, d)?);
        let max_len = if *o == start {
            DE_BIN_MAX_ZERO_SIZED_LEN
        } else {
            (d.len() - start) as u64
        };
        if count as u64 > max_len {
            return Err(DeBinErr {
                o: start,
                l: 1,
                s: d.len(),
                msg: "Vec length exceeds buffer".to_string(),
            });
        }
        for _ in 1..count {
            out.push(DeBin::de_bin(o, d)?);
        }
        Ok(())
    }
}

pub struct DeBinErr {
    pub msg: String,
    pub o: usize,
    pub l: usize,
    pub s: usize,
}

impl std::fmt::Display for DeBinErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error deserializing {} ", self.msg)?;
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

macro_rules! impl_ser_de_bin_for {
    ($ty:ident) => {
        impl SerBin for $ty {
            #[inline]
            fn ser_bin(&self, s: &mut Vec<u8>) {
                s.extend_from_slice(&self.to_le_bytes());
            }

            // Bulk path for `Vec<$ty>` / `[$ty]` / `[$ty; N]`. On little-endian
            // targets the in-memory representation of a `&[$ty]` is already the
            // exact wire bytes (LE-packed, no padding for these scalars), so a
            // single `extend_from_slice` reinterprets the slice as raw bytes and
            // emits them in one `memcpy`. Big-endian falls back to the element
            // loop so the wire format stays byte-identical across platforms.
            #[cfg(target_endian = "little")]
            #[inline]
            fn ser_bin_slice(slice: &[$ty], s: &mut Vec<u8>) {
                let byte_len = std::mem::size_of_val(slice);
                // SAFETY: `$ty` is a plain scalar with no padding or invalid
                // bit patterns; `slice.as_ptr()` is valid for `byte_len` bytes
                // and `u8` has alignment 1, so the reinterpret is sound. The
                // borrow of `slice` outlives the `extend_from_slice` call.
                let bytes =
                    unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, byte_len) };
                s.extend_from_slice(bytes);
            }
        }

        impl DeBin for $ty {
            #[inline]
            fn de_bin(o: &mut usize, d: &[u8]) -> Result<$ty, DeBinErr> {
                let l = std::mem::size_of::<$ty>();
                if *o + l > d.len() {
                    return Err(DeBinErr {
                        o: *o,
                        l,
                        s: d.len(),
                        msg: format!("{}", stringify!($ty)),
                    });
                }
                // SAFETY: the bounds check above guarantees `d[*o..*o + l]` is in
                // range; `read_unaligned` needs no alignment and copies `l` bytes.
                let ret = unsafe {
                    (d.as_ptr().add(*o) as *const $ty).read_unaligned()
                };
                // `from_le` is a no-op on LE and a byte swap on BE, matching the
                // LE wire format on every platform (float types have no `from_le`
                // so they take the branch below).
                let ret = $ty::from_le_bytes(ret.to_le_bytes());
                *o += l;
                Ok(ret)
            }

            // Bulk path for `Vec<$ty>`. On little-endian targets the wire bytes
            // are the in-memory representation, so after one bounds check we
            // reserve exact capacity and `memcpy` straight into the vector's
            // spare capacity, then set the length. Big-endian falls back to the
            // element loop.
            #[cfg(target_endian = "little")]
            #[inline]
            fn de_bin_slice(
                count: usize,
                out: &mut Vec<$ty>,
                o: &mut usize,
                d: &[u8],
            ) -> Result<(), DeBinErr> {
                let elem = std::mem::size_of::<$ty>();
                // `count` is already validated against the remaining buffer by
                // the caller (see the `Vec<T>` impl), but re-check the byte span
                // here so this method is sound on its own terms.
                let byte_len = count.checked_mul(elem).filter(|n| *o + *n <= d.len()).ok_or_else(
                    || DeBinErr {
                        o: *o,
                        l: elem,
                        s: d.len(),
                        msg: concat!("Vec<", stringify!($ty), "> length exceeds buffer").to_string(),
                    },
                )?;
                out.reserve(count);
                // SAFETY: `reserve(count)` guarantees `count` slots of spare
                // capacity past `out.len()` (which is 0 on the fast path); the
                // source span `d[*o..*o + byte_len]` is in range by the check
                // above; both are `#[repr]`-compatible scalar bytes and the
                // regions do not overlap. We advance the length only after the
                // copy succeeds.
                unsafe {
                    let dst = out.as_mut_ptr().add(out.len()) as *mut u8;
                    std::ptr::copy_nonoverlapping(d.as_ptr().add(*o), dst, byte_len);
                    out.set_len(out.len() + count);
                }
                *o += byte_len;
                Ok(())
            }
        }
    };
}

impl_ser_de_bin_for!(f64);
impl_ser_de_bin_for!(f32);
impl_ser_de_bin_for!(u64);
impl_ser_de_bin_for!(i64);
impl_ser_de_bin_for!(u32);
impl_ser_de_bin_for!(i32);
impl_ser_de_bin_for!(u16);
impl_ser_de_bin_for!(i16);

impl SerBin for usize {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        s.extend_from_slice(&(*self as u64).to_le_bytes());
    }
}

impl DeBin for usize {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<usize, DeBinErr> {
        let l = std::mem::size_of::<u64>();
        if *o + l > d.len() {
            return Err(DeBinErr {
                o: *o,
                l,
                s: d.len(),
                msg: "usize".to_string(),
            });
        }
        let ret = u64::from_le_bytes(d[*o..*o + l].try_into().unwrap()) as usize;
        *o += l;
        Ok(ret)
    }
}

impl SerBin for LiveId {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.0.ser_bin(s);
    }
}

impl DeBin for LiveId {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<LiveId, DeBinErr> {
        Ok(LiveId(u64::de_bin(o, d)?))
    }
}

impl DeBin for u8 {
    #[inline]
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<u8, DeBinErr> {
        if *o + 1 > d.len() {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "u8".to_string(),
            });
        }
        let m = d[*o];
        *o += 1;
        Ok(m)
    }

    // Byte-width elements need no endianness handling, so the raw `memcpy`
    // fast path is unconditional. See the scalar-macro impl for the SAFETY
    // reasoning; here `elem == 1` so `byte_len == count`.
    #[inline]
    fn de_bin_slice(count: usize, out: &mut Vec<u8>, o: &mut usize, d: &[u8]) -> Result<(), DeBinErr> {
        if o.checked_add(count).map_or(true, |end| end > d.len()) {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "Vec<u8> length exceeds buffer".to_string(),
            });
        }
        out.extend_from_slice(&d[*o..*o + count]);
        *o += count;
        Ok(())
    }
}

impl SerBin for u8 {
    #[inline]
    fn ser_bin(&self, s: &mut Vec<u8>) {
        s.push(*self);
    }

    #[inline]
    fn ser_bin_slice(slice: &[u8], s: &mut Vec<u8>) {
        s.extend_from_slice(slice);
    }
}

impl DeBin for i8 {
    #[inline]
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<i8, DeBinErr> {
        if *o + 1 > d.len() {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "u8".to_string(),
            });
        }
        let m = d[*o];
        *o += 1;
        Ok(m as i8)
    }

    #[inline]
    fn de_bin_slice(count: usize, out: &mut Vec<i8>, o: &mut usize, d: &[u8]) -> Result<(), DeBinErr> {
        if o.checked_add(count).map_or(true, |end| end > d.len()) {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "Vec<i8> length exceeds buffer".to_string(),
            });
        }
        out.reserve(count);
        // SAFETY: `i8` and `u8` share layout; `reserve` backs `count` slots and
        // the source span is bounds-checked above.
        unsafe {
            let dst = out.as_mut_ptr().add(out.len()) as *mut u8;
            std::ptr::copy_nonoverlapping(d.as_ptr().add(*o), dst, count);
            out.set_len(out.len() + count);
        }
        *o += count;
        Ok(())
    }
}

impl SerBin for i8 {
    #[inline]
    fn ser_bin(&self, s: &mut Vec<u8>) {
        s.push(*self as u8);
    }

    #[inline]
    fn ser_bin_slice(slice: &[i8], s: &mut Vec<u8>) {
        // SAFETY: `i8` and `u8` have identical layout.
        let bytes = unsafe { std::slice::from_raw_parts(slice.as_ptr() as *const u8, slice.len()) };
        s.extend_from_slice(bytes);
    }
}

impl SerBin for bool {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        s.push(if *self { 1 } else { 0 });
    }
}

impl DeBin for bool {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<bool, DeBinErr> {
        if *o + 1 > d.len() {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "bool".to_string(),
            });
        }
        let m = d[*o];
        *o += 1;
        if m == 0 {
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

impl SerBin for String {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        let len = self.len();
        len.ser_bin(s);
        s.extend_from_slice(self.as_bytes());
    }
}

impl DeBin for String {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<String, DeBinErr> {
        let len: u64 = DeBin::de_bin(o, d)?;
        // Untrusted input: `len` is attacker-controlled, so the end offset is
        // computed with checked arithmetic (it wraps in release otherwise) and
        // invalid UTF-8 returns an error instead of panicking.
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| o.checked_add(len))
            .filter(|end| *end <= d.len())
            .ok_or_else(|| DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "String".to_string(),
            })?;
        let r = std::str::from_utf8(&d[*o..end])
            .map_err(|_| DeBinErr {
                o: *o,
                l: end - *o,
                s: d.len(),
                msg: "String is not valid utf8".to_string(),
            })?
            .to_string();
        *o = end;
        Ok(r)
    }
}

impl<T> SerBin for Vec<T>
where
    T: SerBin,
{
    #[inline]
    fn ser_bin(&self, s: &mut Vec<u8>) {
        let len = self.len() as u64;
        len.ser_bin(s);
        // POD element types override `ser_bin_slice` to bulk-copy; everything
        // else falls back to the per-element loop in the default method.
        T::ser_bin_slice(self, s);
    }
}

/// Iteration ceiling for vectors whose elements serialize to nothing: their
/// count is unbounded by the buffer, so only an absolute cap terminates a
/// hostile length.
const DE_BIN_MAX_ZERO_SIZED_LEN: u64 = 1 << 20;

impl<T> DeBin for Vec<T>
where
    T: DeBin,
{
    #[inline]
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Vec<T>, DeBinErr> {
        let len: u64 = DeBin::de_bin(o, d)?;
        // Untrusted input: `len` is attacker-controlled. `usize::try_from`
        // rejects a length that can't index this platform, and the actual
        // hostile-length hardening (never allocate from `len`, cap zero-sized
        // elements, reject counts beyond the buffer) lives in `de_bin_slice`,
        // whose POD overrides additionally do an exact byte-span check.
        let count = usize::try_from(len).map_err(|_| DeBinErr {
            o: *o,
            l: 8,
            s: d.len(),
            msg: "Vec length exceeds usize".to_string(),
        })?;
        let mut out = Vec::new();
        T::de_bin_slice(count, &mut out, o, d)?;
        Ok(out)
    }
}

impl<T> SerBin for Option<T>
where
    T: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        match self {
            None => s.push(0),
            Some(v) => {
                s.push(1);
                v.ser_bin(s);
            }
        }
    }
}

impl<T> DeBin for Option<T>
where
    T: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Option<T>, DeBinErr> {
        if *o + 1 > d.len() {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "Option<T>".to_string(),
            });
        }
        let m = d[*o];
        *o += 1;
        Ok(match m {
            0 => None,
            1 => Some(DeBin::de_bin(o, d)?),
            _ => {
                return Err(DeBinErr {
                    o: *o,
                    l: 0,
                    s: d.len(),
                    msg: "Option<T>".to_string(),
                })
            }
        })
    }
}

impl<T, E> SerBin for Result<T, E>
where
    T: SerBin,
    E: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        match self {
            Ok(v) => {
                s.push(0);
                v.ser_bin(s);
            }
            Err(e) => {
                s.push(1);
                e.ser_bin(s);
            }
        }
    }
}

impl<T, E> DeBin for Result<T, E>
where
    T: DeBin,
    E: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        if *o + 1 > d.len() {
            return Err(DeBinErr {
                o: *o,
                l: 1,
                s: d.len(),
                msg: "Result<T, E>".to_string(),
            });
        }
        let m = d[*o];
        *o += 1;
        Ok(match m {
            0 => Ok(T::de_bin(o, d)?),
            1 => Err(E::de_bin(o, d)?),
            _ => {
                return Err(DeBinErr {
                    o: *o,
                    l: 0,
                    s: d.len(),
                    msg: "Result<T, E>".to_string(),
                })
            }
        })
    }
}

impl<T> SerBin for [T]
where
    T: SerBin,
{
    #[inline]
    fn ser_bin(&self, s: &mut Vec<u8>) {
        // POD element types bulk-copy via the override; others loop.
        T::ser_bin_slice(self, s);
    }
}

unsafe fn de_bin_array_impl_inner<T>(
    top: *mut T,
    count: usize,
    o: &mut usize,
    d: &[u8],
) -> Result<(), DeBinErr>
where
    T: DeBin,
{
    for c in 0..count {
        top.add(c).write(DeBin::de_bin(o, d)?);
    }
    Ok(())
}

impl<T, const N: usize> DeBin for [T; N]
where
    T: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        unsafe {
            let mut to = std::mem::MaybeUninit::<[T; N]>::uninit();
            let top: *mut T = &mut to as *mut _ as *mut T;
            de_bin_array_impl_inner(top, N, o, d)?;
            Ok(to.assume_init())
        }
    }
}

impl<A, B> SerBin for (A, B)
where
    A: SerBin,
    B: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.0.ser_bin(s);
        self.1.ser_bin(s);
    }
}

impl<A, B> DeBin for (A, B)
where
    A: DeBin,
    B: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<(A, B), DeBinErr> {
        Ok((DeBin::de_bin(o, d)?, DeBin::de_bin(o, d)?))
    }
}

impl<A, B, C> SerBin for (A, B, C)
where
    A: SerBin,
    B: SerBin,
    C: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.0.ser_bin(s);
        self.1.ser_bin(s);
        self.2.ser_bin(s);
    }
}

impl<A, B, C> DeBin for (A, B, C)
where
    A: DeBin,
    B: DeBin,
    C: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<(A, B, C), DeBinErr> {
        Ok((
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
        ))
    }
}

impl<A, B, C, D> SerBin for (A, B, C, D)
where
    A: SerBin,
    B: SerBin,
    C: SerBin,
    D: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.0.ser_bin(s);
        self.1.ser_bin(s);
        self.2.ser_bin(s);
        self.3.ser_bin(s);
    }
}

impl<A, B, C, D> DeBin for (A, B, C, D)
where
    A: DeBin,
    B: DeBin,
    C: DeBin,
    D: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<(A, B, C, D), DeBinErr> {
        Ok((
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
        ))
    }
}

impl<A, B, C, D, E> SerBin for (A, B, C, D, E)
where
    A: SerBin,
    B: SerBin,
    C: SerBin,
    D: SerBin,
    E: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.0.ser_bin(s);
        self.1.ser_bin(s);
        self.2.ser_bin(s);
        self.3.ser_bin(s);
        self.4.ser_bin(s);
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
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<(A, B, C, D, E), DeBinErr> {
        Ok((
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
            DeBin::de_bin(o, d)?,
        ))
    }
}

impl<K, V> SerBin for HashMap<K, V>
where
    K: SerBin,
    V: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        let len = self.len() as u64;
        len.ser_bin(s);
        for (k, v) in self {
            k.ser_bin(s);
            v.ser_bin(s);
        }
    }
}

impl<K, V> DeBin for HashMap<K, V>
where
    K: DeBin + Eq + Hash,
    V: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        let len: u64 = DeBin::de_bin(o, d)?;
        let mut h = HashMap::new();
        for _ in 0..len {
            let k = DeBin::de_bin(o, d)?;
            let v = DeBin::de_bin(o, d)?;
            h.insert(k, v);
        }
        Ok(h)
    }
}

impl<T> SerBin for Box<T>
where
    T: SerBin,
{
    fn ser_bin(&self, s: &mut Vec<u8>) {
        (**self).ser_bin(s)
    }
}

impl<T> DeBin for Box<T>
where
    T: DeBin,
{
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Box<T>, DeBinErr> {
        Ok(Box::new(DeBin::de_bin(o, d)?))
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
impl SerBin for PathBuf {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.as_os_str().ser_bin(s)
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
impl SerBin for Path {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.as_os_str().ser_bin(s)
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
impl SerBin for OsString {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        self.as_os_str().ser_bin(s)
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
impl SerBin for OsStr {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;

        self.as_bytes().ser_bin(s)
    }
}

impl SerBin for char {
    fn ser_bin(&self, s: &mut Vec<u8>) {
        let mut bytes = [0; 4];
        self.encode_utf8(&mut bytes).as_bytes().ser_bin(s);
    }
}
/*
#[cfg(unix)]
impl DeBin for PathBuf {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        Ok(PathBuf::from(OsString::de_bin(o, d)?))
    }
}

#[cfg(unix)]
impl DeBin for OsString {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        use std::os::unix::ffi::OsStringExt;

        Ok(OsString::from_vec(Vec::de_bin(o, d)?))
    }
}*/

impl DeBin for char {
    fn de_bin(o: &mut usize, d: &[u8]) -> Result<Self, DeBinErr> {
        let mut bytes = [0; 4];
        bytes[0] = u8::de_bin(o, d)?;
        let width = utf8_char_width(bytes[0]);
        for byte in &mut bytes[1..width] {
            *byte = u8::de_bin(o, d)?;
        }
        Ok(str::from_utf8(&bytes[..width])
            .unwrap()
            .chars()
            .next()
            .unwrap())
    }
}

// Given a first byte, determines how many bytes are in this UTF-8 character.
#[inline]
pub fn utf8_char_width(b: u8) -> usize {
    static UTF8_CHAR_WIDTH: [u8; 256] = [
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, // 0x1F
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, // 0x3F
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, // 0x5F
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, // 0x7F
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, // 0x9F
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, // 0xBF
        0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, // 0xDF
        3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, // 0xEF
        4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 0xFF
    ];

    UTF8_CHAR_WIDTH[b as usize] as usize
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
        "hällo".to_string().ser_bin(&mut s);
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
        v.ser_bin(&mut s);
        let mut o = 0;
        assert_eq!(Vec::<String>::de_bin(&mut o, &s).unwrap(), v);
    }

    /// A unit type serializes to nothing, so its count is unbounded by the
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
                let bulk = v.serialize_bin();

                // Reconstruct the reference bytes element by element.
                let mut manual = Vec::new();
                (v.len() as u64).ser_bin(&mut manual);
                for it in &v {
                    it.ser_bin(&mut manual);
                }
                assert_eq!(bulk, manual, "wire bytes diverge for {}", stringify!($ty));

                // Round-trips through the bulk de path.
                let back = Vec::<$ty>::deserialize_bin(&bulk).unwrap();
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
        vec!["alpha".to_string(), "beta".to_string()].ser_bin(&mut full);
        for cut in 0..full.len() {
            let mut o = 0;
            let _ = Vec::<String>::de_bin(&mut o, &full[..cut]);
        }
    }
}
