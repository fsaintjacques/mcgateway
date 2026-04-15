//! Production-shaped merge UDF: decode Profile protobufs from each
//! pool, union their attributes, re-encode.
//!
//! Uses `prost` derive macros instead of `prost-build`/`protoc` to
//! keep the build pipeline toolchain-light. The equivalent `.proto`
//! declaration is in `proto/profile.proto` for reference.

// The SDK log macros expand to `::alloc::format!(...)` so they work
// in either std or no_std+alloc user crates. Pulling `alloc` into
// scope here (std reexports it) lets the example compile on both.
extern crate alloc;

use std::collections::{BTreeMap, BTreeSet};

use mcgateway_sdk::{merge_fn, warn, Entry, MergeResult, Status};
use prost::Message;

/// `profile.Profile` (proto3). Matches `proto/profile.proto`.
#[derive(Clone, PartialEq, Message)]
pub struct Profile {
    #[prost(string, tag = "1")]
    pub user_id: String,
    /// Writer-supplied wall-clock timestamp. Used as the merge anchor.
    #[prost(int64, tag = "2")]
    pub updated_at: i64,
    /// Arbitrary key/value metadata. On key collision the entry from
    /// the pool with the largest `updated_at` wins.
    #[prost(btree_map = "string, string", tag = "3")]
    pub attrs: BTreeMap<String, String>,
    /// Per-user badge labels. Merged as a stable-sorted deduped union
    /// across pools.
    #[prost(string, repeated, tag = "4")]
    pub badges: Vec<String>,
}

/// Merge a keyspace of `Profile` records.
///
/// Tiebreak rule when two pools report identical `updated_at`
/// timestamps: **pool order wins.** `sort_by_key` is stable, so pools
/// earlier in the keyspace's `read:` list are placed first in the
/// sorted sequence; the later (second) iteration overwrites the
/// earlier on attr collisions and sets the final `user_id` /
/// `updated_at`. This matches the "prefer the pool closer to the
/// right of the read list when timestamps are tied" convention
/// operators expect when scheduling a migration between pools.
#[merge_fn(required_flags = "t")]
// The only caller is the #[merge_fn]-generated ABI wrapper, which
// always consumes the return. #[must_use] would be no-ops here.
#[allow(clippy::must_use_candidate)]
pub fn merge_profile(entries: &[Entry<'_>]) -> MergeResult {
    // Decode each hit entry's bytes into a Profile. Corrupt entries
    // are skipped with a host-side warn log; an all-corrupt read
    // still surfaces through the "no decoded entries" path below.
    let mut decoded: Vec<Profile> = Vec::new();
    for e in entries {
        if e.status != Status::Hit {
            continue;
        }
        let Some(bytes) = e.value else {
            continue;
        };
        match Profile::decode(bytes) {
            Ok(p) => decoded.push(p),
            Err(_) => {
                warn!("profile decode failed for pool={}", e.pool);
            }
        }
    }
    if decoded.is_empty() {
        return MergeResult::Miss;
    }

    // Sort by updated_at ascending so later iteration overwrites
    // earlier values on attr collisions and the final user_id +
    // updated_at come from the newest profile.
    decoded.sort_by_key(|p| p.updated_at);

    let mut out = Profile::default();
    let mut badges: BTreeSet<String> = BTreeSet::new();
    for p in decoded {
        out.user_id = p.user_id;
        out.updated_at = p.updated_at;
        for (k, v) in p.attrs {
            out.attrs.insert(k, v);
        }
        for b in p.badges {
            badges.insert(b);
        }
    }
    out.badges = badges.into_iter().collect();

    MergeResult::Synthesized(out.encode_to_vec())
}
