#![allow(dead_code)]

use std::{
    collections::{hash_map::Entry, HashMap},
    fmt,
    hash::{BuildHasher, Hasher},
    ops::{Deref, DerefMut, Index, IndexMut},
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock, RwLock,
    },
};

const VID_MASK: u64 = 0x0000_3fff_ffff_ffff;
const VID_HIGH_BIT: u64 = 0x8000_0000_0000_0000;

#[derive(Clone, Default, Eq, Hash, Copy, Ord, PartialOrd, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VID(pub u64);

impl VID {
    pub const SEED: u64 = 0xd6e8_feb8_6659_fd93;

    #[inline(always)]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[inline(always)]
    pub const fn from_lo_hi(lo: u32, hi: u32) -> Self {
        Self((lo as u64) | ((hi as u64) << 32))
    }

    #[inline(always)]
    pub const fn lo(&self) -> u32 {
        self.0 as u32
    }

    #[inline(always)]
    pub const fn hi(&self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[inline(always)]
    pub const fn seeded() -> Self {
        Self(Self::SEED)
    }

    #[inline(always)]
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    #[inline(always)]
    pub const fn get_value(&self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn from_bytes(seed: u64, id_bytes: &[u8], start: usize, end: usize, or: u64) -> Self {
        let mut x = seed;
        let mut i = start;
        while i < end {
            x = x.wrapping_add(id_bytes[i] as u64);
            x ^= x >> 32;
            x = x.wrapping_mul(Self::SEED);
            x ^= x >> 32;
            x = x.wrapping_mul(Self::SEED);
            x ^= x >> 32;
            i += 1;
        }
        Self((x & VID_MASK) | or)
    }

    #[inline(always)]
    pub const fn add(&self, what: u64) -> Self {
        Self(self.0.wrapping_add(what))
    }

    #[inline(always)]
    pub const fn xor(&self, what: u64) -> Self {
        Self((self.0 ^ what) | VID_HIGH_BIT)
    }

    #[inline(always)]
    pub const fn sub(&self, what: u64) -> Self {
        Self(self.0.wrapping_sub(what))
    }

    #[inline]
    pub const fn from_str(id_str: &str) -> Self {
        let bytes = id_str.as_bytes();
        Self::from_bytes(Self::SEED, bytes, 0, bytes.len(), 0)
    }

    #[inline]
    pub const fn from_bytes_lc(
        seed: u64,
        id_bytes: &[u8],
        start: usize,
        end: usize,
        or: u64,
    ) -> Self {
        let mut x = seed;
        let mut i = start;
        while i < end {
            let byte = id_bytes[i];
            let byte = if byte >= b'A' && byte <= b'Z' {
                byte + (b'a' - b'A')
            } else {
                byte
            };
            x = x.wrapping_add(byte as u64);
            x ^= x >> 32;
            x = x.wrapping_mul(Self::SEED);
            x ^= x >> 32;
            x = x.wrapping_mul(Self::SEED);
            x ^= x >> 32;
            i += 1;
        }
        Self((x & VID_MASK) | or)
    }

    #[inline]
    pub const fn from_str_lc(id_str: &str) -> Self {
        let bytes = id_str.as_bytes();
        Self::from_bytes_lc(Self::SEED, bytes, 0, bytes.len(), 0)
    }

    #[inline]
    pub const fn str_append(self, id_str: &str) -> Self {
        let bytes = id_str.as_bytes();
        Self::from_bytes(self.0, bytes, 0, bytes.len(), 0)
    }

    #[inline]
    pub const fn bytes_append(self, bytes: &[u8]) -> Self {
        Self::from_bytes(self.0, bytes, 0, bytes.len(), 0)
    }

    #[inline]
    pub const fn id_append(self, id: VID) -> Self {
        let bytes = id.0.to_be_bytes();
        Self::from_bytes(self.0, &bytes, 0, 8, 0)
    }

    #[inline]
    pub const fn from_str_num(id_str: &str, num: u64) -> Self {
        let bytes = id_str.as_bytes();
        let id = Self::from_bytes(Self::SEED, bytes, 0, bytes.len(), 0);
        Self::from_bytes(id.0, &num.to_be_bytes(), 0, 8, 0)
    }

    #[inline]
    pub const fn from_num(seed: u64, num: u64) -> Self {
        Self::from_bytes(seed, &num.to_be_bytes(), 0, 8, 0)
    }

    pub fn from_str_with_lut(id_str: &str) -> Result<Self, String> {
        let id = Self::from_str(id_str);
        VIDInterner::with(|idmap| match idmap.id_to_string.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(id_str.to_owned());
                Ok(id)
            }
            Entry::Occupied(entry) if entry.get() == id_str => Ok(id),
            Entry::Occupied(entry) => Err(entry.get().clone()),
        })
    }

    pub fn from_str_with_intern(id_str: &str, intern: InternVID) -> Self {
        let id = Self::from_str(id_str);
        if matches!(intern, InternVID::Yes) {
            VIDInterner::with(|idmap| match idmap.id_to_string.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(id_str.to_owned());
                }
                Entry::Occupied(mut entry) if entry.get() != id_str => {
                    entry.insert(id_str.to_owned());
                }
                Entry::Occupied(_) => {}
            });
        }
        id
    }

    pub fn from_str_num_with_lut(id_str: &str, num: u64) -> Result<Self, String> {
        let id = Self::from_str_num(id_str, num);
        VIDInterner::with(|idmap| {
            idmap.id_to_string.insert(id, format!("{id_str}{num}"));
            Ok(id)
        })
    }

    #[inline]
    pub fn as_string<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&str>) -> R,
    {
        VIDInterner::read(|idmap| f(idmap.id_to_string.get(self).map(String::as_str)))
    }

    #[inline(always)]
    pub const fn not_empty(&self) -> bool {
        self.0 != 0
    }

    #[inline(always)]
    pub fn unique() -> Self {
        Self(UNIQUE_VID.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Clone, Copy)]
pub enum InternVID {
    Yes,
    No,
}

pub(crate) static UNIQUE_VID: AtomicU64 = AtomicU64::new(1);

impl fmt::Debug for VID {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for VID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0");
        }
        self.as_string(|string| match string {
            Some(id) => f.write_str(id),
            None => write!(f, "{:016x}", self.0),
        })
    }
}

impl fmt::LowerHex for VID {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

pub struct VIDInterner {
    id_to_string: HashMap<VID, String, VIDHasherBuilder>,
}

impl VIDInterner {
    #[inline]
    fn global() -> &'static RwLock<Self> {
        static IDMAP: OnceLock<RwLock<VIDInterner>> = OnceLock::new();
        IDMAP.get_or_init(|| RwLock::new(Self::seeded_map()))
    }

    fn seeded_map() -> Self {
        const FILL: &[&str] = &[
            "buffer", "this", "native", "vec2", "assert", "Range", "start", "end", "sin", "ty",
            "step", "import", "retain", "extend", "push", "pop", "number", "nan", "bool", "nil",
            "color", "string", "object", "factory", "opcode", "mod", "global", "scope", "fn", "id",
            "default", "true", "false", "exp", "void", "use", "#", "$", "@", "^", "^=", "|", "||",
            "|=", "%", "%=", "!=", "!", "&&", "*=", "*", "+=", "+", ",", "-=", "->", "-", "..",
            "...", "..=", ".", "/=", "/", "::", ":", ":=", ";", "<=", "<", "<<", "<<=", "==", "=",
            ">=", "=>", ">", ">>", ">>=", "?", "tracks", "state", "state_id", "user", "play", "ended",
            "geom_pos", "geom_id", "geom_uv",
        ];

        let mut id_to_string =
            HashMap::with_capacity_and_hasher(FILL.len(), VIDHasherBuilder {});

        for &item in FILL {
            let id = VID::from_str(item);
            match id_to_string.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(item.to_owned());
                }
                Entry::Occupied(entry) if entry.get() != item => {
                    eprintln!("WE HAVE AN ID COLLISION!");
                }
                Entry::Occupied(_) => {}
            }
        }

        Self { id_to_string }
    }

    #[inline]
    pub fn add(&mut self, val: &str) {
        self.id_to_string
            .insert(VID::from_str(val), val.to_owned());
    }

    #[inline]
    pub fn contains(&self, val: &str) -> bool {
        self.id_to_string.contains_key(&VID::from_str(val))
    }

    #[inline]
    pub fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let mut idmap = Self::global().write().unwrap();
        f(&mut idmap)
    }

    #[inline]
    fn read<F, R>(f: F) -> R
    where
        F: FnOnce(&Self) -> R,
    {
        let idmap = Self::global().read().unwrap();
        f(&idmap)
    }
}

// Idea taken from the `nohash_hasher` crate.
#[derive(Default)]
pub struct VIDHasher(u64);

impl Hasher for VIDHasher {
    #[cold]
    fn write(&mut self, _: &[u8]) {
        unreachable!("Invalid use of VIDHasher");
    }

    #[inline(always)]
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }

    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Copy, Clone, Default)]
pub struct VIDHasherBuilder {}

impl BuildHasher for VIDHasherBuilder {
    type Hasher = VIDHasher;

    #[inline(always)]
    fn build_hasher(&self) -> Self::Hasher {
        VIDHasher(0)
    }
}

#[derive(Clone, Debug)]
pub struct VIDMap<K, V> {
    map: HashMap<K, V, VIDHasherBuilder>,
}

impl<K, V> VIDMap<K, V> {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity_and_hasher(capacity, VIDHasherBuilder {}),
        }
    }
}

impl<K, V> Default for VIDMap<K, V> {
    #[inline]
    fn default() -> Self {
        Self {
            map: HashMap::with_hasher(VIDHasherBuilder {}),
        }
    }
}

impl<K, V> Deref for VIDMap<K, V> {
    type Target = HashMap<K, V, VIDHasherBuilder>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<K, V> DerefMut for VIDMap<K, V> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

impl<K, V> Index<K> for VIDMap<K, V>
where
    K: Eq + std::hash::Hash,
{
    type Output = V;

    #[inline]
    fn index(&self, index: K) -> &Self::Output {
        self.map.get(&index).unwrap()
    }
}

impl<K, V> IndexMut<K> for VIDMap<K, V>
where
    K: Eq + std::hash::Hash,
{
    #[inline]
    fn index_mut(&mut self, index: K) -> &mut Self::Output {
        self.map.get_mut(&index).unwrap()
    }
}