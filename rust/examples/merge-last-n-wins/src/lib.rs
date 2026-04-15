//! Example UDF: picks the entry with the highest `t` (TTL remaining),
//! ignoring misses and errors. Compiled to `wasm32-wasip1` and loaded
//! through the gateway's wasmtime host.

use mcgateway_sdk::{merge_fn, Entry, MergeResult, Status};

#[merge_fn(required_flags = "t")]
#[must_use]
pub fn last_n_wins(entries: &[Entry<'_>]) -> MergeResult {
    let mut best: Option<(usize, i64)> = None;
    for (i, e) in entries.iter().enumerate() {
        if e.status != Status::Hit {
            continue;
        }
        let Some(t) = e.t else {
            continue;
        };
        match best {
            None => best = Some((i, t)),
            Some((_, bt)) if t > bt => best = Some((i, t)),
            _ => {}
        }
    }
    best.map_or(MergeResult::Miss, |(i, _)| MergeResult::Winner(i))
}
