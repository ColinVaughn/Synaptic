//! A shared string for the fields that repeat across the whole graph.

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The interning pool stops growing past this many distinct strings. Beyond it
/// values are still correct, just no longer shared -- a bound on a per-thread
/// cache that would otherwise accumulate across graph reloads in a long-lived
/// server. A large real corpus interns a few tens of thousands of paths.
const MAX_POOL: usize = 1 << 17;

thread_local! {
    static POOL: RefCell<HashMap<Arc<str>, ()>> = RefCell::new(HashMap::new());
}

/// An immutable string that many values can share.
///
/// Reads exactly like `String` -- it derefs to `str`, and compares, hashes,
/// orders and serializes identically -- but equal values share one allocation.
/// `source_file` and `relation` are the graph's most repetitive fields: a large
/// corpus holds 571 distinct source paths across 2.1M node and edge occurrences
/// (68 MiB of individual `String`s for 20 KB of distinct text) and 17 distinct
/// relation names across 1.5M edges.
///
/// Values built through [`Deserialize`] and [`From`] are pooled per thread, so a
/// parsed graph holds one allocation per distinct string. The pool holds only
/// `Arc`s, so dropping the graph still frees the text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Interned(Arc<str>);

impl Interned {
    /// Borrow the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when the string is empty, matching `String::is_empty`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The pooled handle for `s`, allocating only on first sight.
    fn intern(s: &str) -> Arc<str> {
        POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            if let Some((existing, ())) = pool.get_key_value(s) {
                return Arc::clone(existing);
            }
            let shared: Arc<str> = Arc::from(s);
            if pool.len() < MAX_POOL {
                pool.insert(Arc::clone(&shared), ());
            }
            shared
        })
    }
}

impl Default for Interned {
    fn default() -> Self {
        Interned(Arc::from(""))
    }
}

impl std::ops::Deref for Interned {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Interned {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Interned {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// So a `source_file` can be handed straight to `Path::new` like the `String` it
/// replaced.
impl AsRef<std::ffi::OsStr> for Interned {
    fn as_ref(&self) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(&*self.0)
    }
}

impl AsRef<std::path::Path> for Interned {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(&*self.0)
    }
}

impl From<&str> for Interned {
    fn from(s: &str) -> Self {
        Interned(Interned::intern(s))
    }
}

impl From<String> for Interned {
    fn from(s: String) -> Self {
        Interned(Interned::intern(&s))
    }
}

impl From<&String> for Interned {
    fn from(s: &String) -> Self {
        Interned(Interned::intern(s))
    }
}

impl From<Interned> for String {
    fn from(s: Interned) -> String {
        s.0.to_string()
    }
}

impl PartialEq<str> for Interned {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Interned {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for Interned {
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

impl PartialEq<Interned> for str {
    fn eq(&self, other: &Interned) -> bool {
        self == &*other.0
    }
}

impl PartialEq<Interned> for String {
    fn eq(&self, other: &Interned) -> bool {
        self.as_str() == &*other.0
    }
}

impl std::fmt::Display for Interned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Interned {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Interned {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrVisitor;

        impl serde::de::Visitor<'_> for StrVisitor {
            type Value = Interned;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Interned, E> {
                Ok(Interned::from(v))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Interned, E> {
                Ok(Interned::from(v))
            }
        }

        deserializer.deserialize_str(StrVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_values_share_one_allocation() {
        let a = Interned::from("crates/synaptic-core/src/node.rs");
        let b = Interned::from(String::from("crates/synaptic-core/src/node.rs"));
        assert_eq!(a, b);
        assert!(
            std::ptr::eq(Arc::as_ptr(&a.0), Arc::as_ptr(&b.0)),
            "equal strings must share storage"
        );
    }

    #[test]
    fn distinct_values_do_not_share() {
        let a = Interned::from("a.rs");
        let b = Interned::from("b.rs");
        assert_ne!(a, b);
        assert!(!std::ptr::eq(Arc::as_ptr(&a.0), Arc::as_ptr(&b.0)));
    }

    #[test]
    fn reads_like_a_string() {
        let s = Interned::from("src/auth.rs");
        assert_eq!(s.len(), 11);
        assert!(s.ends_with(".rs"));
        assert!(s.starts_with("src/"));
        assert_eq!(&*s, "src/auth.rs");
        assert_eq!(s, "src/auth.rs");
        assert_eq!(s.to_string(), "src/auth.rs");
        assert!(Interned::default().is_empty());
    }

    #[test]
    fn orders_and_hashes_by_content() {
        let mut v = [Interned::from("b"), Interned::from("a")];
        v.sort();
        assert_eq!(v[0], "a");
        let set: std::collections::HashSet<Interned> = [Interned::from("x"), Interned::from("x")]
            .into_iter()
            .collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let s = Interned::from("src/auth.rs");
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!("src/auth.rs")
        );
        let back: Interned = serde_json::from_value(serde_json::json!("src/auth.rs")).unwrap();
        assert_eq!(back, s);
    }

    /// A string arriving through a reader (owned, not borrowed) must pool too.
    #[test]
    fn owned_input_is_pooled() {
        let json = "\"pooled/from/reader.rs\"";
        let a: Interned = serde_json::from_reader(json.as_bytes()).unwrap();
        let b = Interned::from("pooled/from/reader.rs");
        assert!(std::ptr::eq(Arc::as_ptr(&a.0), Arc::as_ptr(&b.0)));
    }
}
