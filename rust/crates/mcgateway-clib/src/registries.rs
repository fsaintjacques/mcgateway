//! The composite name → merge lookup the Lua C module exposes.
//!
//! Two flavours of merge coexist: native built-ins (first-hit,
//! pool-preferred, last-write-wins) resolved through
//! [`mcgateway_core::Registry`] with `'static` names, and WASM modules
//! loaded from disk whose names are dynamic. [`Registries`] fuses both
//! behind one lookup surface so the Lua dispatch path does not care.
//!
//! The WASM side lives behind [`arc_swap::ArcSwap`] so the UDF loader
//! can publish a new table atomically without pausing in-flight merges.

use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use mcgateway_core::{Entry, Merge, MergeResult, Registry};

/// A loaded WASM merge and its advertised `required_flags`. The flags
/// string is duplicated out of the `WasmMerge` so callers don't need to
/// jump through the `&'static str` constraint on `Merge::required_flags`.
pub struct WasmEntry {
    pub merge: Arc<dyn Merge>,
    pub required_flags: String,
}

pub type WasmTable = BTreeMap<String, Arc<WasmEntry>>;

pub struct Registries {
    builtins: Arc<Registry>,
    wasm: ArcSwap<WasmTable>,
}

impl Registries {
    pub fn new(builtins: Registry) -> Self {
        Self {
            builtins: Arc::new(builtins),
            wasm: ArcSwap::from_pointee(WasmTable::new()),
        }
    }

    /// Atomically replace the WASM table. Merges running against the
    /// previous table see consistent state until they return; new
    /// lookups see the new table.
    pub fn swap_wasm(&self, new_table: WasmTable) {
        self.wasm.store(Arc::new(new_table));
    }

    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        if self.builtins.has(name) {
            return true;
        }
        self.wasm.load().contains_key(name)
    }

    /// Built-ins shadow WASM modules on name collision: the registry
    /// check is "builtins first, fall back to WASM". A disk module
    /// named `first-hit` will never be reached for dispatch; the
    /// [`crate::udf_loader`] rejects such files at load time and logs
    /// the collision.
    pub fn apply(&self, name: &str, entries: &[Entry<'_>]) -> Option<MergeResult> {
        if let Some(m) = self.builtins.get(name) {
            return Some(m.apply(entries));
        }
        let guard = self.wasm.load();
        guard.get(name).map(|e| e.merge.apply(entries))
    }

    /// Flags the named merge wants returned on reads. `None` if the
    /// merge is not registered; an empty string if the merge declares
    /// no flags.
    #[must_use]
    pub fn required_flags(&self, name: &str) -> Option<String> {
        if let Some(m) = self.builtins.get(name) {
            return Some(m.required_flags().to_string());
        }
        self.wasm.load().get(name).map(|e| e.required_flags.clone())
    }

    /// All registered merge names, lexicographically sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.builtins.names().map(String::from).collect();
        for k in self.wasm.load().keys() {
            if !out.iter().any(|n| n == k) {
                out.push(k.clone());
            }
        }
        out.sort();
        out
    }

    /// Read access to the built-in registry for callers that need the
    /// raw `&'static str` name view (e.g. the UDF loader's
    /// name-collision check).
    #[must_use]
    pub fn builtins(&self) -> &Registry {
        &self.builtins
    }
}
