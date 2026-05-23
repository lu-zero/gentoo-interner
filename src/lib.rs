//! String interning for Gentoo-related crates.
//!
//! Provides a flexible interning system for reducing memory usage when
//! processing large numbers of repeated strings.
//!
//! # Components
//!
//! - [`Interner`]: Trait for interning strings into compact keys
//! - [`Interned<I>`]: An interned string key parameterized by interner type
//! - [`DefaultInterner`]: Default interner based on feature flags
//!
//! # Features
//!
//! | Feature | DefaultInterner | Key Type | Behavior |
//! |---------|-----------------|----------|----------|
//! | `interner` (default) | `GlobalInterner` | `u32` | papaya-backed, lock-free, `Copy` |
//! | `lasso` | `GlobalInterner` | `u32` | lasso-backed, arena alloc, `Copy` |
//! | neither | `NoInterner` | `Box<str>` | No deduplication, `Clone` only |
//!
//! # Example
//!
//! ```
//! use gentoo_interner::{Interned, DefaultInterner};
//!
//! let interned = Interned::<DefaultInterner>::intern("amd64");
//! assert_eq!(interned.resolve(), "amd64");
//! ```

use std::fmt::Debug;
use std::marker::PhantomData;

/// Trait for interning strings into compact keys.
///
/// Implementations map strings to keys and resolve keys back to strings.
/// All methods are static, allowing the interner type to serve as a
/// configuration parameter without carrying runtime state.
pub trait Interner: Clone + Send + Sync + 'static {
    /// Key type returned by [`get_or_intern`](Self::get_or_intern).
    type Key: Clone + Eq + Ord + std::hash::Hash + Send + Sync + 'static + Debug;

    /// Intern `s`, returning a stable key.
    fn get_or_intern(s: &str) -> Self::Key;

    /// Resolve `key` back to its original string.
    fn resolve(key: &Self::Key) -> &str;
}

/// Non-interning fallback that allocates each string as a `Box<str>`.
///
/// No deduplication occurs. The [`Key`](Interner::Key) type is `Box<str>`,
/// making `Interned<NoInterner>` `Clone` but not `Copy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoInterner;

impl Interner for NoInterner {
    type Key = Box<str>;

    fn get_or_intern(s: &str) -> Box<str> {
        Box::from(s)
    }

    fn resolve(key: &Box<str>) -> &str {
        key
    }
}

/// Global process-wide interner. Zero-sized type; all state lives in a
/// process-wide static. Keys are stable `u32` values, making
/// `Interned<GlobalInterner>` `Copy`.
///
/// The backing store is selected by feature flags:
/// - `interner` (default): lock-free [`papaya::HashMap`] for lookups +
///   [`boxcar::Vec`] for indexed `resolve`, with a global `Mutex` gating
///   inserts so the slow path never wastes allocations on races.
/// - `lasso`: `lasso::ThreadedRodeo` arena; enable for benchmarking.
#[cfg(any(feature = "interner", feature = "lasso", feature = "symbol-table"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalInterner;

// ── papaya backend ────────────────────────────────────────────────────────────
//
//  * Fast path: lock-free `papaya` `get`, no allocation.
//  * Slow path: sharded mutexes (mirror `lasso::ThreadedRodeo`). The
//    string's hash picks one of `N_SHARDS` mutexes; only threads colliding
//    on the same shard contend. Inside the lock we re-check (a lock-free
//    reader may have missed us on the fast path), then allocate + push to
//    the reverse vec + use papaya's cheap `insert()`. This avoids the
//    `get_or_insert_with()` overhead while still scaling across threads.
//  * Resolve: O(1) indexed read into a `boxcar::Vec<&'static str>`; no
//    `.pin()` or hash lookup per call.
//
// Known non-failures:
//  * If a thread panics between `reverse.push` and `forward.insert`, the
//    reverse vec keeps an orphan slot — a leaked string that no forward
//    entry refers to. The next intern of the same string allocates a
//    fresh slot. This wastes memory but does not corrupt state. Lasso has
//    the analogous window between `strings.insert` and the forward
//    `insert_in_slot`.

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
const N_SHARDS: usize = 32;

// Bitmask sharding (`& (N_SHARDS - 1)`) requires a power of two.
#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
const _: () = assert!(
    N_SHARDS.is_power_of_two(),
    "N_SHARDS must be a power of two for bitmask sharding to be correct",
);

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
#[repr(align(64))]
struct PaddedMutex(parking_lot::Mutex<()>);

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
struct GlobalState {
    /// `str` → id. Keys are leaked so they are `'static`; lookups by
    /// `&str` work via the `Borrow<str>` impl on `&'static str`.
    forward: papaya::HashMap<&'static str, u32>,
    /// id → `'static` str. Indexed Vec — `resolve` is `reverse[id]`.
    reverse: boxcar::Vec<&'static str>,
    /// One mutex per shard. The string's hash selects the shard so threads
    /// colliding on the same shard serialize; everyone else proceeds in
    /// parallel. Each is cache-line aligned (64 bytes) to avoid false sharing.
    insert_shards: [PaddedMutex; N_SHARDS],
}

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
static GLOBAL: std::sync::OnceLock<GlobalState> = std::sync::OnceLock::new();

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
fn global() -> &'static GlobalState {
    GLOBAL.get_or_init(|| GlobalState {
        forward: papaya::HashMap::new(),
        reverse: boxcar::Vec::new(),
        insert_shards: std::array::from_fn(|_| PaddedMutex(parking_lot::Mutex::new(()))),
    })
}

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
fn shard_for(s: &str) -> usize {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    use std::sync::OnceLock;
    // Process-stable hasher so shard assignment is consistent.
    static HASHER: OnceLock<RandomState> = OnceLock::new();
    let hasher = HASHER.get_or_init(RandomState::new);
    (hasher.hash_one(s) as usize) & (N_SHARDS - 1)
}

#[cfg(all(
    feature = "interner",
    not(feature = "lasso"),
    not(feature = "symbol-table")
))]
impl Interner for GlobalInterner {
    type Key = u32;

    fn get_or_intern(s: &str) -> u32 {
        let state = global();

        // Fast path: lock-free read.
        if let Some(&id) = state.forward.pin().get(s) {
            return id;
        }

        // Slow path: lock just the shard this string hashes to.
        let shard = shard_for(s);
        let _guard = state.insert_shards[shard].0.lock();
        let pinned = state.forward.pin();
        if let Some(&id) = pinned.get(s) {
            return id;
        }
        let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
        let idx = state.reverse.push(leaked);
        // The forward map stores u32 ids; truncating here would associate the
        // new string with a stale id and silently corrupt later resolves.
        // Lasso handles the analogous overflow with `KeySpaceExhaustion`;
        // we panic since our trait doesn't surface an error path.
        let id = u32::try_from(idx)
            .expect("gentoo-interner: id space exhausted (more than u32::MAX unique strings)");
        pinned.insert(leaked, id);
        id
    }

    fn resolve(key: &u32) -> &str {
        global()
            .reverse
            .get(*key as usize)
            .copied()
            .expect("invalid interner key")
    }
}

// ── symbol-table backend ──────────────────────────────────────────────────────
//
// `symbol_table::GlobalSymbol` is a process-wide interner backed by a
// sharded `SymbolTable` (16 shards by default). Each shard holds a
// `Mutex<HashMap>` and an arena allocator for the strings.

#[cfg(all(feature = "symbol-table", not(feature = "lasso")))]
impl Interner for GlobalInterner {
    type Key = u32;

    fn get_or_intern(s: &str) -> u32 {
        std::num::NonZeroU32::from(symbol_table::GlobalSymbol::from(s)).get()
    }

    fn resolve(key: &u32) -> &str {
        let sym = symbol_table::GlobalSymbol::from(
            std::num::NonZeroU32::new(*key).expect("invalid interner key (zero)"),
        );
        sym.as_str()
    }
}

// ── lasso backend ─────────────────────────────────────────────────────────────

#[cfg(feature = "lasso")]
static GLOBAL_LASSO: std::sync::OnceLock<lasso::ThreadedRodeo> = std::sync::OnceLock::new();

#[cfg(feature = "lasso")]
fn global_lasso() -> &'static lasso::ThreadedRodeo {
    GLOBAL_LASSO.get_or_init(lasso::ThreadedRodeo::default)
}

#[cfg(feature = "lasso")]
impl Interner for GlobalInterner {
    type Key = u32;

    fn get_or_intern(s: &str) -> u32 {
        use lasso::Key as _;
        global_lasso().get_or_intern(s).into_usize() as u32
    }

    fn resolve(key: &u32) -> &str {
        use lasso::Key as _;
        let spur = lasso::Spur::try_from_usize(*key as usize).expect("invalid interner key");
        global_lasso().resolve(&spur)
    }
}

// ── DefaultInterner selection ─────────────────────────────────────────────────

/// Default interner type based on feature flags.
///
/// - `interner` (default) or `lasso`: [`GlobalInterner`] — process-global, `Copy` keys
/// - neither: [`NoInterner`] — no deduplication, `Clone` only
#[cfg(any(feature = "interner", feature = "lasso", feature = "symbol-table"))]
pub type DefaultInterner = GlobalInterner;
#[cfg(not(any(feature = "interner", feature = "lasso", feature = "symbol-table")))]
pub type DefaultInterner = NoInterner;

/// An interned string key parameterized by [`Interner`] type `I`.
///
/// With [`GlobalInterner`], this is 4 bytes and `Copy`.
/// With [`NoInterner`], this is a pointer and `Clone` only.
///
/// Serde support serializes as the string value and deserializes via interning.
pub struct Interned<I: Interner> {
    key: <I as Interner>::Key,
    _marker: PhantomData<I>,
}

impl<I: Interner> Clone for Interned<I> {
    fn clone(&self) -> Self {
        Self {
            key: self.key.clone(),
            _marker: PhantomData,
        }
    }
}
impl<I: Interner> Copy for Interned<I> where <I as Interner>::Key: Copy {}
impl<I: Interner> PartialEq for Interned<I> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<I: Interner> Eq for Interned<I> {}
impl<I: Interner> PartialOrd for Interned<I> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<I: Interner> Ord for Interned<I> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}
impl<I: Interner> std::hash::Hash for Interned<I> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}
impl<I: Interner> std::fmt::Debug for Interned<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Interned").field(&self.key).finish()
    }
}

impl<I: Interner> Interned<I> {
    /// Intern a string, returning a new `Interned<I>`.
    pub fn intern(s: &str) -> Self {
        Self {
            key: I::get_or_intern(s),
            _marker: PhantomData,
        }
    }

    /// Resolve this interned key back to its original string.
    pub fn resolve(&self) -> &str {
        I::resolve(&self.key)
    }

    /// Get the interned string as a `&str`.
    pub fn as_str(&self) -> &str {
        self.resolve()
    }
}

impl<I: Interner> std::ops::Deref for Interned<I> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.resolve()
    }
}

impl<I: Interner> AsRef<str> for Interned<I> {
    fn as_ref(&self) -> &str {
        self.resolve()
    }
}

impl<I: Interner> From<&str> for Interned<I> {
    fn from(s: &str) -> Self {
        Self::intern(s)
    }
}

impl<I: Interner> From<String> for Interned<I> {
    fn from(s: String) -> Self {
        Self::intern(&s)
    }
}

impl<I: Interner> std::fmt::Display for Interned<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.resolve())
    }
}

impl<I: Interner> PartialEq<str> for Interned<I> {
    fn eq(&self, other: &str) -> bool {
        self.resolve() == other
    }
}

impl<I: Interner> PartialEq<&str> for Interned<I> {
    fn eq(&self, other: &&str) -> bool {
        self.resolve() == *other
    }
}

impl<I: Interner> PartialEq<Interned<I>> for str {
    fn eq(&self, other: &Interned<I>) -> bool {
        self == other.resolve()
    }
}

impl<I: Interner> PartialEq<Interned<I>> for &str {
    fn eq(&self, other: &Interned<I>) -> bool {
        *self == other.resolve()
    }
}

#[cfg(feature = "serde")]
impl<I: Interner> serde::Serialize for Interned<I> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.resolve())
    }
}

#[cfg(feature = "serde")]
impl<'de, I: Interner> serde::Deserialize<'de> for Interned<I> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize<'de>>::deserialize(deserializer)?;
        Ok(Self::intern(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interned_basic() {
        let a = Interned::<DefaultInterner>::intern("test");
        assert_eq!(a.resolve(), "test");
        assert_eq!(a.as_str(), "test");
    }

    #[test]
    fn test_interned_equality() {
        let a = Interned::<DefaultInterner>::intern("foo");
        let b = Interned::<DefaultInterner>::intern("foo");
        let c = Interned::<DefaultInterner>::intern("bar");

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn test_interned_copy() {
        let a = Interned::<DefaultInterner>::intern("test");
        #[allow(clippy::clone_on_copy)]
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn test_interned_from_str() {
        let a: Interned<DefaultInterner> = "hello".into();
        assert_eq!(a.as_str(), "hello");
    }

    #[test]
    fn test_interned_deref() {
        let a = Interned::<DefaultInterner>::intern("test");
        assert!(a.starts_with("te"));
        assert!(a.ends_with("st"));
    }

    #[test]
    fn test_interned_as_ref() {
        let a = Interned::<DefaultInterner>::intern("test");
        let s: &str = a.as_ref();
        assert_eq!(s, "test");
    }

    #[test]
    fn test_interned_display() {
        let a = Interned::<DefaultInterner>::intern("test");
        assert_eq!(format!("{}", a), "test");
    }

    #[test]
    fn test_interned_str_eq() {
        let a = Interned::<DefaultInterner>::intern("test");
        assert_eq!(a, "test");
        assert_eq!("test", a);
        assert_ne!(a, "other");
    }

    /// Roundtrip stress test: intern many unique strings, resolve them
    /// all back, verify every one round-trips. Runs across every backend
    /// since it uses `DefaultInterner`.
    #[test]
    fn test_roundtrip_many() {
        // Use a salted prefix so concurrent tests don't collide with
        // strings interned by other tests (the global interner is shared).
        let prefix = format!("rt_many_{}_", std::process::id());
        let strings: Vec<String> = (0..1024).map(|i| format!("{prefix}{i:08}")).collect();
        let keys: Vec<Interned<DefaultInterner>> =
            strings.iter().map(|s| Interned::intern(s)).collect();
        for (k, s) in keys.iter().zip(strings.iter()) {
            assert_eq!(k.as_str(), s, "roundtrip failed for {s}");
        }
    }

    /// Same string interned multiple times must always yield the same key,
    /// regardless of interleaving with other interns.
    #[test]
    fn test_intern_stable() {
        let s = format!("stable_{}", std::process::id());
        let first = Interned::<DefaultInterner>::intern(&s);
        for _ in 0..100 {
            Interned::<DefaultInterner>::intern(&format!("noise_{}", rand_like()));
            let again = Interned::<DefaultInterner>::intern(&s);
            assert_eq!(first, again, "intern of {s} was not stable");
        }
    }

    /// Multi-threaded roundtrip: many threads each intern a private set
    /// of strings and a shared set. All resolves must round-trip and the
    /// shared strings must produce the same key across threads.
    #[test]
    fn test_concurrent_roundtrip() {
        use std::sync::Arc;
        use std::thread;

        let n_threads = 8;
        let n_private = 256;
        let pid = std::process::id();

        // Shared strings every thread interns
        let shared: Vec<String> = (0..32)
            .map(|i| format!("ct_shared_{pid}_{i}"))
            .collect();
        let shared = Arc::new(shared);

        // Reference keys interned from the main thread first
        let shared_keys: Vec<Interned<DefaultInterner>> =
            shared.iter().map(|s| Interned::intern(s)).collect();
        let shared_keys = Arc::new(shared_keys);

        let handles: Vec<_> = (0..n_threads)
            .map(|t| {
                let shared = Arc::clone(&shared);
                let shared_keys = Arc::clone(&shared_keys);
                thread::spawn(move || {
                    // Private strings for this thread
                    let private: Vec<String> = (0..n_private)
                        .map(|i| format!("ct_priv_{pid}_t{t}_{i:08}"))
                        .collect();
                    let private_keys: Vec<Interned<DefaultInterner>> = private
                        .iter()
                        .map(|s| Interned::intern(s))
                        .collect();
                    // Private roundtrip
                    for (k, s) in private_keys.iter().zip(private.iter()) {
                        assert_eq!(k.as_str(), s);
                    }
                    // Shared keys should match the main-thread reference
                    for s in shared.iter() {
                        let k = Interned::<DefaultInterner>::intern(s);
                        let expected = shared_keys
                            .iter()
                            .find(|sk| sk.as_str() == s)
                            .copied()
                            .expect("shared key not found");
                        assert_eq!(k, expected, "thread {t} got mismatched key for {s}");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panic");
        }
    }

    /// Cheap pseudo-random helper for `test_intern_stable` noise — no
    /// dependency on `rand`.
    fn rand_like() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        COUNTER.fetch_add(1, Ordering::Relaxed)
    }
}
