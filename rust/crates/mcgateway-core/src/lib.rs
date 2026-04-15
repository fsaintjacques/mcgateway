//! Core types for the gateway's merge path.
//!
//! A merge is a pure function over an ordered list of [`Entry`] values, one
//! per backend pool, returning either the index of the winning entry, a
//! freshly-synthesized byte string, or [`MergeResult::Miss`].
//!
//! Entries are borrowed from the caller — typically the Lua host — and are
//! only valid for the duration of a single [`Merge::apply`] call. The
//! boundary carries no serialization format: the caller projects its native
//! representation into an [`Entry`] slice by reference.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Outcome of a per-pool read, classified by the gateway before the merge
/// sees it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Status {
    /// Backend returned a value.
    Hit,
    /// Backend returned a well-formed miss (`EN`/`NF`).
    Miss,
    /// Transport failure, timeout, or protocol-level `SERVER_ERROR`.
    Error,
}

/// A single pool's response, projected into a view the merge can read.
///
/// All reference fields are borrowed from memory the caller owns and keeps
/// alive for the duration of the [`Merge::apply`] call. Implementors must
/// not stash these references past the call.
///
/// Entries arrive in the read-list order declared on the keyspace; this is
/// part of the contract between the gateway and merges (see
/// `doc/plans/stage-2-fanout-merge.md`).
#[derive(Clone, Debug)]
pub struct Entry<'a> {
    /// The requested key (bytes; memcached keys are not required to be UTF-8).
    pub key: &'a [u8],
    /// The pool name as declared in the keyspace config.
    pub pool: &'a str,
    pub status: Status,
    /// Parsed meta `t` flag (TTL remaining in seconds). `None` when absent.
    pub t: Option<i64>,
    /// Response value body. `None` unless [`Status::Hit`]; may also be `None`
    /// when the caller chose not to materialize it (lazy projection).
    pub value: Option<&'a [u8]>,
    /// Full meta-response header line, without trailing CRLF, for merges
    /// that need flags beyond `t`. `None` if the caller did not expose it.
    pub line: Option<&'a [u8]>,
}

/// What a merge decided.
#[derive(Clone, Debug)]
pub enum MergeResult {
    /// The zero-based index of the winning entry in the input slice. The
    /// caller forwards that pool's response verbatim.
    Winner(usize),
    /// A freshly-built byte string. Reserved for merges that synthesize a
    /// value rather than pick one — no Stage 3a built-in uses this.
    Synthesized(Vec<u8>),
    /// No entry qualifies; caller emits a miss reply.
    Miss,
}

/// A named merge strategy.
///
/// Implementations must be pure, side-effect free, and cheap; the gateway
/// holds the request in-flight while the merge runs.
pub trait Merge: Send + Sync {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult;

    /// Single-character meta flags the merge needs returned on reads. The
    /// gateway augments outgoing `mg` commands with these flags so the
    /// merge sees the fields it depends on.
    fn required_flags(&self) -> &'static str {
        ""
    }
}

/// A name → merge mapping. Built at startup; lookups are `O(log n)` and
/// allocation-free.
pub struct Registry {
    by_name: BTreeMap<&'static str, Arc<dyn Merge>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_name: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, name: &'static str, m: Arc<dyn Merge>) {
        self.by_name.insert(name, m);
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Merge>> {
        self.by_name.get(name)
    }

    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Registered names in lexicographic order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_name.keys().copied()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
