//! Lua C module exposing the gateway's merge registry as
//! `mcgateway_native`.
//!
//! Loaded via `require("mcgateway_native")` inside the memcached proxy's
//! embedded Lua 5.4. Exposes four functions:
//!
//! - `merge(name, entries)` — run the named merge over the entry list;
//!   returns the 1-based index of the winning entry, a string of
//!   synthesized bytes, or `nil` for miss.
//! - `has_merge(name)` — boolean; used by config validation.
//! - `required_flags(name)` — single-character meta flags the merge
//!   needs returned on reads (e.g. `"t"` for `last-write-wins`).
//! - `names()` — list of registered merge names, lexicographically
//!   sorted. Covers both native built-ins and WASM modules loaded from
//!   the UDF directory.
//!
//! Native merges are resolved against
//! [`mcgateway_core::Registry`] at module init. WASM merges are
//! discovered at init time by scanning the UDF directory and live
//! behind an [`arc_swap::ArcSwap`] so a future hot-reload path can
//! publish new tables without pausing in-flight calls. The `merge`
//! dispatch walks the Lua entry table once per call — no wire format
//! crosses the boundary on the Lua → Rust side; the Rust → WASM wire
//! format is owned entirely by [`mcgateway_wasm_host`].

mod registries;
mod udf_loader;

use std::sync::Arc;

use mcgateway_core::{Entry, MergeResult, Registry, Status};
use mcgateway_wasm_host::WasmHost;
use mlua::prelude::*;
use mlua::Variadic;

use crate::registries::Registries;

fn build_registries() -> LuaResult<Arc<Registries>> {
    let mut builtins = Registry::new();
    mcgateway_core::builtins::register(&mut builtins);
    let registries = Arc::new(Registries::new(builtins));

    // Discover WASM modules on disk. Failure to read the directory —
    // or an explicit MCGW_UDF_DIR pointing at an invalid path — is
    // hard; per-module failures are soft (logged, module skipped) so
    // a single broken file can't take down the gateway's built-ins.
    let dir = udf_loader::udf_dir()
        .map_err(|e| LuaError::RuntimeError(format!("mcgateway_native: {e}")))?;
    if let Some(dir) = dir {
        let host = WasmHost::new().map_err(|e| {
            LuaError::RuntimeError(format!("mcgateway_native: wasm host init: {e:#}"))
        })?;
        let table = udf_loader::scan_dir(&host, &registries, &dir, |path, msg| {
            // stdout-log for now; structured logging lands with Stage 6.
            eprintln!("mcgateway_native: udf {} skipped: {msg}", path.display());
        })
        .map_err(|e| {
            LuaError::RuntimeError(format!(
                "mcgateway_native: scan {}: {e}",
                dir.display()
            ))
        })?;
        registries.swap_wasm(table);
    }

    Ok(registries)
}

fn parse_status(s: &[u8]) -> LuaResult<Status> {
    match s {
        b"hit" => Ok(Status::Hit),
        b"miss" => Ok(Status::Miss),
        b"error" => Ok(Status::Error),
        other => Err(LuaError::RuntimeError(format!(
            "mcgateway_native: invalid status {:?}",
            String::from_utf8_lossy(other),
        ))),
    }
}

/// Owned copies of the fields the Rust side reads. Borrowed `Entry`
/// views are built from these for the merge call. `value` and `line`
/// are materialised eagerly when present on the Lua entry so WASM
/// merges (e.g. protobuf decoders) can inspect the response body;
/// built-in merges don't touch them, so the only cost is one allocation
/// per hit, which Lua already paid to build the string in the first
/// place.
struct OwnedEntry {
    key: Vec<u8>,
    pool: String,
    status: Status,
    t: Option<i64>,
    value: Option<Vec<u8>>,
    line: Option<Vec<u8>>,
}

fn project(entries: &LuaTable) -> LuaResult<Vec<OwnedEntry>> {
    let len = entries.raw_len();
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        let e: LuaTable = entries.get(i)?;
        let key: LuaString = e.get("key")?;
        let pool: LuaString = e.get("pool")?;
        let status: LuaString = e.get("status")?;
        let t: Option<i64> = e.get("t")?;
        let value: Option<LuaString> = e.get("value")?;
        let line: Option<LuaString> = e.get("line")?;
        out.push(OwnedEntry {
            key: key.as_bytes().to_vec(),
            pool: pool.to_str()?.to_string(),
            status: parse_status(&status.as_bytes())?,
            t,
            value: value.map(|s| s.as_bytes().to_vec()),
            line: line.map(|s| s.as_bytes().to_vec()),
        });
    }
    Ok(out)
}

fn as_views(owned: &[OwnedEntry]) -> Vec<Entry<'_>> {
    owned
        .iter()
        .map(|o| Entry {
            key: &o.key,
            pool: &o.pool,
            status: o.status,
            t: o.t,
            value: o.value.as_deref(),
            line: o.line.as_deref(),
        })
        .collect()
}

fn name_str(name: &LuaString) -> LuaResult<String> {
    let bytes = name.as_bytes();
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|_| LuaError::RuntimeError("mcgateway_native: merge name must be utf-8".into()))
}

#[mlua::lua_module]
fn mcgateway_native(lua: &Lua) -> LuaResult<LuaTable> {
    let registries = build_registries()?;

    let exports = lua.create_table()?;

    {
        let registries = registries.clone();
        let merge = lua.create_function(move |lua, (name, entries): (LuaString, LuaTable)| {
            let name = name_str(&name)?;
            let owned = project(&entries)?;
            let view = as_views(&owned);
            let result = registries.apply(&name, &view).ok_or_else(|| {
                LuaError::RuntimeError(format!("mcgateway_native: unknown merge {name:?}"))
            })?;
            match result {
                MergeResult::Winner(i) => Ok(LuaValue::Integer(
                    i64::try_from(i + 1).map_err(|_| {
                        LuaError::RuntimeError("merge winner index overflow".into())
                    })?,
                )),
                MergeResult::Synthesized(bytes) => {
                    Ok(LuaValue::String(lua.create_string(&bytes)?))
                }
                MergeResult::Miss => Ok(LuaValue::Nil),
            }
        })?;
        exports.set("merge", merge)?;
    }

    {
        let registries = registries.clone();
        let has_merge = lua.create_function(move |_, name: LuaString| {
            let bytes = name.as_bytes();
            let s = std::str::from_utf8(&bytes).unwrap_or("");
            Ok(registries.has(s))
        })?;
        exports.set("has_merge", has_merge)?;
    }

    {
        let registries = registries.clone();
        let required_flags = lua.create_function(move |_, name: LuaString| {
            let name = name_str(&name)?;
            registries.required_flags(&name).ok_or_else(|| {
                LuaError::RuntimeError(format!("mcgateway_native: unknown merge {name:?}"))
            })
        })?;
        exports.set("required_flags", required_flags)?;
    }

    {
        let registries = registries.clone();
        let names = lua.create_function(move |lua, _: Variadic<LuaValue>| {
            let tbl = lua.create_table()?;
            for (i, n) in registries.names().iter().enumerate() {
                tbl.raw_set(i + 1, n.as_str())?;
            }
            Ok(tbl)
        })?;
        exports.set("names", names)?;
    }

    Ok(exports)
}
