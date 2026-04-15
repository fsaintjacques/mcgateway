//! Lua C module exposing the Rust merge registry as `mcgateway_native`.
//!
//! Loaded by the gateway's Lua library via `require("mcgateway_native")`.
//! Exposes four functions:
//!
//! - `merge(name, entries)` — run the named merge over the entry list;
//!   returns the 1-based index of the winning entry, a string of
//!   synthesized bytes, or `nil` for miss.
//! - `has_merge(name)` — boolean; used by config validation.
//! - `required_flags(name)` — single-character meta flags the merge needs
//!   returned on reads (e.g. `"t"` for `last-write-wins`).
//! - `names()` — list of registered merge names, lexicographically sorted.
//!
//! The entry table is walked once per call. No wire format crosses the
//! boundary: key, pool, status, and `t` are read directly off the Lua
//! stack.

use std::sync::Arc;

use mcgateway_core::{Entry, MergeResult, Registry, Status};
use mlua::prelude::*;
use mlua::Variadic;

fn build_registry() -> Arc<Registry> {
    let mut reg = Registry::new();
    mcgateway_merge_builtins::register(&mut reg);
    Arc::new(reg)
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

/// Owned copies of the fields the Rust side reads. Borrowed `Entry` views
/// are built from these for the merge call. `value` and `line` are not
/// materialized yet — no Stage 3a built-in reads them.
///
/// The one allocation per entry (key + pool) stays off the hot path for
/// every built-in; it lets us avoid juggling `mlua::String` guard
/// lifetimes while still keeping the Lua ↔ Rust boundary free of any
/// wire format.
struct OwnedEntry {
    key: Vec<u8>,
    pool: String,
    status: Status,
    t: Option<i64>,
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
        out.push(OwnedEntry {
            key: key.as_bytes().to_vec(),
            pool: pool.to_str()?.to_string(),
            status: parse_status(&status.as_bytes())?,
            t,
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
            value: None,
            line: None,
        })
        .collect()
}

fn lookup_merge<'r>(reg: &'r Registry, name: &LuaString) -> LuaResult<&'r Arc<dyn mcgateway_core::Merge>> {
    let bytes = name.as_bytes();
    let s = std::str::from_utf8(&bytes).map_err(|_| {
        LuaError::RuntimeError("mcgateway_native: merge name must be utf-8".into())
    })?;
    reg.get(s).ok_or_else(|| {
        LuaError::RuntimeError(format!("mcgateway_native: unknown merge {s:?}"))
    })
}

#[mlua::lua_module]
fn mcgateway_native(lua: &Lua) -> LuaResult<LuaTable> {
    let registry = build_registry();

    let exports = lua.create_table()?;

    {
        let reg = registry.clone();
        let merge = lua.create_function(move |lua, (name, entries): (LuaString, LuaTable)| {
            let m = lookup_merge(&reg, &name)?;
            let owned = project(&entries)?;
            let view = as_views(&owned);
            match m.apply(&view) {
                MergeResult::Winner(i) => Ok(LuaValue::Integer(i64::try_from(i + 1).map_err(
                    |_| LuaError::RuntimeError("merge winner index overflow".into()),
                )?)),
                MergeResult::Synthesized(bytes) => {
                    Ok(LuaValue::String(lua.create_string(&bytes)?))
                }
                MergeResult::Miss => Ok(LuaValue::Nil),
            }
        })?;
        exports.set("merge", merge)?;
    }

    {
        let reg = registry.clone();
        let has_merge = lua.create_function(move |_, name: LuaString| {
            let bytes = name.as_bytes();
            let s = std::str::from_utf8(&bytes).unwrap_or("");
            Ok(reg.has(s))
        })?;
        exports.set("has_merge", has_merge)?;
    }

    {
        let reg = registry.clone();
        let required_flags = lua.create_function(move |_, name: LuaString| {
            let m = lookup_merge(&reg, &name)?;
            Ok(m.required_flags().to_string())
        })?;
        exports.set("required_flags", required_flags)?;
    }

    {
        let reg = registry.clone();
        let names = lua.create_function(move |lua, _: Variadic<LuaValue>| {
            let tbl = lua.create_table()?;
            for (i, n) in reg.names().enumerate() {
                tbl.raw_set(i + 1, n)?;
            }
            Ok(tbl)
        })?;
        exports.set("names", names)?;
    }

    Ok(exports)
}
