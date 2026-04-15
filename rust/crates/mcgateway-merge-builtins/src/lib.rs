//! Built-in merge strategies. Line-for-line ports of Stage 2's Lua
//! implementations, preserving the same behavioural matrix.

use std::sync::Arc;

use mcgateway_core::{Entry, Merge, MergeResult, Registry, Status};

/// Return the first entry whose status is [`Status::Hit`].
pub struct FirstHit;

impl Merge for FirstHit {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult {
        for (i, e) in entries.iter().enumerate() {
            if e.status == Status::Hit {
                return MergeResult::Winner(i);
            }
        }
        MergeResult::Miss
    }
}

/// Identical to [`FirstHit`] given the pool-order contract; exposed under a
/// distinct name for intent at the config call site.
pub struct PoolPreferred;

impl Merge for PoolPreferred {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult {
        FirstHit.apply(entries)
    }
}

/// Pick the hit entry with the greatest `t` flag. Entries without `t` never
/// displace a hit that has one; ties keep the earlier index (stable).
pub struct LastWriteWins;

impl Merge for LastWriteWins {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult {
        let mut best: Option<(usize, Option<i64>)> = None;
        for (i, e) in entries.iter().enumerate() {
            if e.status != Status::Hit {
                continue;
            }
            match best {
                None => best = Some((i, e.t)),
                Some((_, best_t)) => {
                    if let Some(this_t) = e.t {
                        if best_t.is_none() || this_t > best_t.unwrap() {
                            best = Some((i, Some(this_t)));
                        }
                    }
                }
            }
        }
        match best {
            Some((i, _)) => MergeResult::Winner(i),
            None => MergeResult::Miss,
        }
    }

    fn required_flags(&self) -> &'static str {
        "t"
    }
}

/// Insert the three built-ins into `reg` under their canonical names.
pub fn register(reg: &mut Registry) {
    reg.insert("first-hit", Arc::new(FirstHit));
    reg.insert("pool-preferred", Arc::new(PoolPreferred));
    reg.insert("last-write-wins", Arc::new(LastWriteWins));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(pool: &'static str, t: Option<i64>) -> Entry<'static> {
        Entry { key: b"k", pool, status: Status::Hit, t, value: None, line: None }
    }
    fn miss(pool: &'static str) -> Entry<'static> {
        Entry { key: b"k", pool, status: Status::Miss, t: None, value: None, line: None }
    }
    fn err(pool: &'static str) -> Entry<'static> {
        Entry { key: b"k", pool, status: Status::Error, t: None, value: None, line: None }
    }

    fn pick(r: &MergeResult) -> Option<usize> {
        match r {
            MergeResult::Winner(i) => Some(*i),
            _ => None,
        }
    }
    fn is_miss(r: &MergeResult) -> bool {
        matches!(r, MergeResult::Miss)
    }

    #[test]
    fn first_hit_returns_first() {
        let e = [hit("a", None), hit("b", None)];
        assert_eq!(pick(&FirstHit.apply(&e)), Some(0));
    }

    #[test]
    fn first_hit_skips_misses() {
        let e = [miss("a"), hit("b", None)];
        assert_eq!(pick(&FirstHit.apply(&e)), Some(1));
    }

    #[test]
    fn first_hit_skips_errors() {
        let e = [err("a"), hit("b", None)];
        assert_eq!(pick(&FirstHit.apply(&e)), Some(1));
    }

    #[test]
    fn first_hit_all_miss() {
        let e = [miss("a"), miss("b")];
        assert!(is_miss(&FirstHit.apply(&e)));
    }

    #[test]
    fn first_hit_all_error() {
        let e = [err("a"), err("b")];
        assert!(is_miss(&FirstHit.apply(&e)));
    }

    #[test]
    fn first_hit_empty() {
        assert!(is_miss(&FirstHit.apply(&[])));
    }

    #[test]
    fn pool_preferred_matches_first_hit() {
        let e = [miss("a"), hit("b", None), hit("c", None)];
        assert_eq!(pick(&PoolPreferred.apply(&e)), pick(&FirstHit.apply(&e)));
    }

    #[test]
    fn lww_picks_highest_t() {
        let e = [hit("a", Some(100)), hit("b", Some(300)), hit("c", Some(200))];
        assert_eq!(pick(&LastWriteWins.apply(&e)), Some(1));
    }

    #[test]
    fn lww_tie_keeps_first() {
        let e = [hit("a", Some(50)), hit("b", Some(50))];
        assert_eq!(pick(&LastWriteWins.apply(&e)), Some(0));
    }

    #[test]
    fn lww_known_t_replaces_nil_anchor() {
        let e = [hit("a", None), hit("b", Some(500))];
        assert_eq!(pick(&LastWriteWins.apply(&e)), Some(1));
    }

    #[test]
    fn lww_nil_does_not_displace_known_t() {
        let e = [hit("a", Some(100)), hit("b", None)];
        assert_eq!(pick(&LastWriteWins.apply(&e)), Some(0));
    }

    #[test]
    fn lww_no_hits() {
        let e = [miss("a"), err("b")];
        assert!(is_miss(&LastWriteWins.apply(&e)));
    }

    #[test]
    fn lww_required_flags() {
        assert_eq!(LastWriteWins.required_flags(), "t");
        assert_eq!(FirstHit.required_flags(), "");
    }

    #[test]
    fn registry_roundtrip() {
        let mut r = Registry::new();
        register(&mut r);
        for name in ["first-hit", "pool-preferred", "last-write-wins"] {
            assert!(r.has(name), "missing {name}");
            assert!(r.get(name).is_some());
        }
        assert!(!r.has("bogus"));
        let names: Vec<_> = r.names().collect();
        assert_eq!(names, vec!["first-hit", "last-write-wins", "pool-preferred"]);
    }
}
