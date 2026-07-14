//! Loads `.wasm` merge UDFs from a directory and publishes them into a
//! [`Registries`]'s WASM table.
//!
//! Owns the scan only: hot-reload triggering lives in `watcher`, and
//! `lib.rs` wires the two together (rescan on UDF-directory change,
//! then swap). Loaded modules enter the table wrapped in
//! [`ObservedMerge`] so their failures are observable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mcgateway_core::{Entry, Merge, MergeResult};
use mcgateway_wasm_host::{MergeErrorKind, WasmHost, WasmMerge};

use crate::registries::{Registries, WasmEntry, WasmTable};

/// Why a module was skipped during a scan. Coarse by design: these
/// become metric label values, so the set must stay closed — the
/// human-readable detail rides in the message alongside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdfProblem {
    /// The module path has no UTF-8 stem to derive a name from.
    InvalidName,
    /// The name collides with a built-in merge; the built-in wins.
    BuiltinCollision,
    /// The module failed to read, compile, or instantiate.
    LoadFailed,
}

impl UdfProblem {
    /// Stable lowercase form, used as a metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidName => "invalid-name",
            Self::BuiltinCollision => "builtin-collision",
            Self::LoadFailed => "load-failed",
        }
    }
}

/// Environment variable overriding the default UDF directory.
pub const UDF_DIR_ENV: &str = "MCGW_UDF_DIR";

/// Default on-disk location for compiled WASM merges.
pub const DEFAULT_UDF_DIR: &str = "/etc/mcgateway/udf";

/// Resolve the effective UDF directory.
///
/// - `Ok(Some(path))` when a usable directory is found.
/// - `Ok(None)` when `MCGW_UDF_DIR` is unset *and* the default path
///   does not exist. A gateway running with only built-in merges is
///   a valid deployment, so this is not an error.
/// - `Err(msg)` when `MCGW_UDF_DIR` is explicitly set but points at
///   something that is not a directory. Silently ignoring an
///   operator-supplied path is a footgun; fail loudly instead.
pub fn udf_dir() -> Result<Option<PathBuf>, String> {
    if let Ok(custom) = std::env::var(UDF_DIR_ENV) {
        let p = PathBuf::from(&custom);
        if p.is_dir() {
            return Ok(Some(p));
        }
        return Err(format!(
            "{UDF_DIR_ENV}={custom} is not a directory (or is unreadable)"
        ));
    }
    let default = PathBuf::from(DEFAULT_UDF_DIR);
    Ok(default.is_dir().then_some(default))
}

/// Build a [`WasmTable`] by scanning `dir` for `*.wasm` files. Each
/// module's name is the file stem (e.g. `last_n_wins.wasm` registers
/// as `last_n_wins`). Modules whose names collide with a built-in are
/// skipped; the built-in keeps the name.
///
/// Per-file failures (compile errors, bad ABI, missing exports) are
/// logged via the provided callback and the remaining modules still
/// load. An unreadable directory surfaces as an `Err`.
pub fn scan_dir(
    host: &WasmHost,
    registries: &Registries,
    dir: &Path,
    mut on_problem: impl FnMut(&Path, UdfProblem, &str),
) -> std::io::Result<WasmTable> {
    let mut out = WasmTable::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            on_problem(
                &path,
                UdfProblem::InvalidName,
                "module path has no utf-8 stem",
            );
            continue;
        };
        if registries.builtins().has(&name) {
            on_problem(
                &path,
                UdfProblem::BuiltinCollision,
                "name collides with a built-in merge; disk module ignored",
            );
            continue;
        }

        match load_module(host, &path, &name) {
            Ok(entry) => {
                out.insert(name, Arc::new(entry));
            }
            Err(err) => on_problem(&path, UdfProblem::LoadFailed, &err),
        }
    }
    Ok(out)
}

fn load_module(host: &WasmHost, path: &Path, name: &str) -> Result<WasmEntry, String> {
    let bytes = fs::read(path).map_err(|e| format!("read: {e}"))?;
    let module = host.compile(&bytes).map_err(|e| format!("compile: {e:#}"))?;
    let merge = WasmMerge::from_module(host, &module, name)
        .map_err(|e| format!("instantiate: {e:#}"))?;
    let required_flags = merge.required_flags().to_owned();
    Ok(WasmEntry {
        merge: Arc::new(ObservedMerge { inner: merge }) as Arc<dyn Merge>,
        required_flags,
    })
}

/// What actually enters the registry: a [`WasmMerge`] wrapped so
/// dispatch failures are counted and (rate-limited) logged before
/// degrading to `Miss`. The wrapper — not `WasmMerge`'s own `Merge`
/// impl — carries the observation, so the host crate stays
/// metrics-free and classification runs on the un-wrapped error,
/// where downcasting still works. Dispatch behaviour is byte-
/// identical to the bare impl: same inputs, same `Miss`.
struct ObservedMerge {
    inner: WasmMerge,
}

impl Merge for ObservedMerge {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult {
        match self.inner.run(entries) {
            Ok(r) => r,
            Err(e) => {
                let kind = MergeErrorKind::classify(&e);
                crate::metrics::global().merge_error(self.inner.name(), kind.as_str());
                if crate::metrics::log_limiter().try_acquire() {
                    log::warn!(
                        "merge {name} failed ({kind}), degrading to miss: {e:#}",
                        name = self.inner.name(),
                        kind = kind.as_str(),
                    );
                }
                MergeResult::Miss
            }
        }
    }
    // required_flags stays the trait default (""): the registry reads
    // flags from WasmEntry.required_flags, captured above from the
    // inherent String-backed accessor — same shim story as WasmMerge.
}
