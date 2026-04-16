//! Loads `.wasm` merge UDFs from a directory and publishes them into a
//! [`Registries`]'s WASM table.
//!
//! Scope for step 3: startup scan only. Hot-reload via `notify` lands
//! in a follow-up once the kind tests need it. This file exists now so
//! the step 3 boundary is right-shaped for the watcher to drop in
//! without re-threading ownership.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mcgateway_core::Merge;
use mcgateway_wasm_host::{WasmHost, WasmMerge};

use crate::registries::{Registries, WasmEntry, WasmTable};

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
    mut on_problem: impl FnMut(&Path, &str),
) -> std::io::Result<WasmTable> {
    let mut out = WasmTable::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            on_problem(&path, "module path has no utf-8 stem");
            continue;
        };
        if registries.builtins().has(&name) {
            on_problem(
                &path,
                "name collides with a built-in merge; disk module ignored",
            );
            continue;
        }

        match load_module(host, &path, &name) {
            Ok(entry) => {
                out.insert(name, Arc::new(entry));
            }
            Err(err) => on_problem(&path, &err),
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
        merge: Arc::new(merge) as Arc<dyn Merge>,
        required_flags,
    })
}
