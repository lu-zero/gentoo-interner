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
    type Key: Clone + Eq + std::hash::Hash + Send + Sync + 'static + Debug;

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
/// - `interner` (default): two lock-free [`papaya::HashMap`]s; strings are
///   leaked on first intern so `resolve` needs no guard.
/// - `lasso`: `lasso::ThreadedRodeo` arena; enable for benchmarking.
#[cfg(any(feature = "interner", feature = "lasso"))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalInterner;

// ── papaya backend ────────────────────────────────────────────────────────────

#[cfg(all(feature = "interner", not(feature = "lasso")))]
struct GlobalState {
    /// `str` → id. Keys are leaked so they are `'static` and can be looked
    /// up by `&str` via the `Borrow<str>` impl on `&'static str`.
    forward: papaya::HashMap<&'static str, u32>,
    /// id → `'static` str pointer (copied straight out, no guard needed).
    reverse: papaya::HashMap<u32, &'static str>,
    counter: std::sync::atomic::AtomicU32,
}

#[cfg(all(feature = "interner", not(feature = "lasso")))]
static GLOBAL: std::sync::OnceLock<GlobalState> = std::sync::OnceLock::new();

#[cfg(all(feature = "interner", not(feature = "lasso")))]
fn global() -> &'static GlobalState {
    GLOBAL.get_or_init(|| GlobalState {
        forward: papaya::HashMap::new(),
        reverse: papaya::HashMap::new(),
        counter: std::sync::atomic::AtomicU32::new(0),
    })
}

#[cfg(all(feature = "interner", not(feature = "lasso")))]
impl Interner for GlobalInterner {
    type Key = u32;

    fn get_or_intern(s: &str) -> u32 {
        use std::sync::atomic::Ordering;
        let state = global();
        let pinned = state.forward.pin();

        // Fast path: already interned — fully lock-free.
        if let Some(&id) = pinned.get(s) {
            return id;
        }

        // Slow path: new string. Leak it to obtain a `'static` pointer
        // for both the forward key and the reverse value.
        //
        // The reverse insert is done *inside* the closure so it completes
        // before the forward entry's CAS makes the id visible to other
        // threads. Any thread that reads id N from the forward map is
        // guaranteed to find reverse[N] already present.
        //
        // Under concurrent races for the same new string, the losing thread's
        // closure may also have run, leaving an orphaned counter slot and a
        // leaked string copy — bounded and harmless for a process-lifetime
        // interner.
        let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
        *pinned.get_or_insert_with(leaked, || {
            let id = state.counter.fetch_add(1, Ordering::Relaxed);
            state.reverse.pin().insert(id, leaked);
            id
        })
    }

    fn resolve(key: &u32) -> &str {
        // Values are `&'static str` so we can copy the pointer straight out
        // without retaining a guard.
        global()
            .reverse
            .pin()
            .get(key)
            .copied()
            .expect("invalid interner key")
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
#[cfg(any(feature = "interner", feature = "lasso"))]
pub type DefaultInterner = GlobalInterner;
#[cfg(not(any(feature = "interner", feature = "lasso")))]
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
}
