//! Example UDF exercising the Synthesized return path: concatenates all
//! hit values with a single `|` separator. Used by the host round-trip
//! test to verify the full SDK encode → host decode path for
//! synthesized byte strings.

use mcgateway_sdk::{merge_fn, Entry, MergeResult, Status};

#[merge_fn]
#[must_use]
pub fn concat_values(entries: &[Entry<'_>]) -> MergeResult {
    let mut out: Vec<u8> = Vec::new();
    let mut wrote_any = false;
    for e in entries {
        if e.status != Status::Hit {
            continue;
        }
        let Some(value) = e.value else {
            continue;
        };
        if wrote_any {
            out.push(b'|');
        }
        out.extend_from_slice(value);
        wrote_any = true;
    }
    if wrote_any {
        MergeResult::Synthesized(out)
    } else {
        MergeResult::Miss
    }
}
