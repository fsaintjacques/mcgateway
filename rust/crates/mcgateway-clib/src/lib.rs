//! Lua C module exposing the gateway's merge registry as
//! `mcgateway_native`.
//!
//! Loaded via `require("mcgateway_native")` inside the memcached proxy's
//! embedded Lua 5.4. Exposes:
//!
//! - `merge(name, entries, opts?)` — run the named merge over the
//!   entry list; returns the 1-based index of the winning entry, a
//!   string of synthesized bytes, or `nil` for miss. The optional
//!   `opts` table (`prefix`, `start`) attributes the call to a
//!   keyspace for metrics — the read path's whole instrumentation
//!   rides this existing crossing.
//! - `has_merge(name)` — boolean; used by config validation.
//! - `required_flags(name)` — single-character meta flags the merge
//!   needs returned on reads (e.g. `"t"` for `last-write-wins`).
//! - `names()` — list of registered merge names, lexicographically
//!   sorted. Covers both native built-ins and WASM modules loaded from
//!   the UDF directory.
//! - `now()` — monotonic nanoseconds, the clock for `opts.start` /
//!   `observe(start)`.
//! - `observe(prefix, op, outcome, start?)` — record a request that
//!   doesn't pass through `merge` (writes, sentinel routes).
//! - `observe_reload(result, n_pools, n_keyspaces)` — record a config
//!   load outcome (`ok` | `fallback`) and the serving config's shape.
//!
//! Native merges are resolved against
//! [`mcgateway_core::Registry`] at module init. WASM merges are
//! discovered at init time by scanning the UDF directory and live
//! behind an [`arc_swap::ArcSwap`] so a future hot-reload path can
//! publish new tables without pausing in-flight calls. The `merge`
//! dispatch walks the Lua entry table once per call — no wire format
//! crosses the boundary on the Lua → Rust side; the Rust → WASM wire
//! format is owned entirely by [`mcgateway_wasm_host`].

mod logging;
mod metrics;
mod registries;
mod udf_loader;
mod watcher;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use mcgateway_core::{Entry, MergeResult, Registry, Status};
use mcgateway_wasm_host::WasmHost;
use mlua::prelude::*;
use mlua::Variadic;

use crate::registries::Registries;

/// Environment variable naming the config file to watch for live
/// reload. Same variable proxy.lua reads for its `loadfile` path.
/// Unset → no watcher: standalone deployments keep edit-and-restart
/// semantics.
pub const CONFIG_ENV: &str = "MCGATEWAY_CONFIG";

/// Process-global registries, shared by every Lua state in the proxy
/// (config thread and workers alike). Hot reload requires this: a WASM
/// table swap published by the watcher thread must be visible to the
/// state that re-runs `has_merge` during config validation, not just
/// to whichever state happened to trigger the initial scan. The first
/// state to require the module pays for the scan; failures are cached
/// and re-surfaced to every subsequent state.
static SHARED: OnceLock<Result<Arc<Registries>, String>> = OnceLock::new();

fn rescan_into(host: &WasmHost, registries: &Arc<Registries>, dir: &Path) -> Result<(), String> {
    let m = metrics::global();
    let table = udf_loader::scan_dir(host, registries, dir, |path, problem, msg| {
        m.udf_module_failure(problem.as_str());
        log::warn!("udf {} skipped ({}): {msg}", path.display(), problem.as_str());
    })
    .map_err(|e| {
        m.udf_rescan("error");
        format!("scan {}: {e}", dir.display())
    })?;
    m.set_registry_merges("wasm", table.len());
    m.udf_rescan("ok");
    registries.swap_wasm(table);
    Ok(())
}

fn init_shared() -> Result<Arc<Registries>, String> {
    logging::init();

    // Metrics exposition arms first and independently: bind failure
    // is loud but non-fatal — a gateway that cannot expose metrics
    // must still serve traffic.
    if let Ok(addr) = std::env::var(metrics::METRICS_ADDR_ENV) {
        match metrics::spawn_exporter(&addr, metrics::global()) {
            Ok(bound) => log::info!("metrics exposition listening on {bound}"),
            Err(e) => log::error!("metrics exposition disabled: {e}"),
        }
    }

    let mut builtins = Registry::new();
    mcgateway_core::builtins::register(&mut builtins);
    metrics::global().set_registry_merges("builtin", builtins.names().count());
    let registries = Arc::new(Registries::new(builtins));

    // Discover WASM modules on disk. Failure to read the directory —
    // or an explicit MCGW_UDF_DIR pointing at an invalid path — is
    // hard; per-module failures are soft (logged, module skipped) so
    // a single broken file can't take down the gateway's built-ins.
    let dir = udf_loader::udf_dir().map_err(|e| format!("mcgateway_native: {e}"))?;
    let host = if dir.is_some() {
        Some(
            WasmHost::new()
                .map_err(|e| format!("mcgateway_native: wasm host init: {e:#}"))?,
        )
    } else {
        None
    };
    if let (Some(dir), Some(host)) = (&dir, &host) {
        rescan_into(host, &registries, dir).map_err(|e| format!("mcgateway_native: {e}"))?;
    }

    // Live reload: only armed when the config path is explicit. The
    // reload signal is process-directed (`kill(getpid())`), not
    // `raise()`: raise targets the calling thread, and a thread that
    // inherited a blocked SIGHUP would hold the signal pending forever.
    if let Ok(config_path) = std::env::var(CONFIG_ENV) {
        let rescan: Box<dyn Fn() + Send> = match (&dir, &host) {
            (Some(dir), Some(host)) => {
                let (host, registries, dir) = (host.clone(), registries.clone(), dir.clone());
                Box::new(move || {
                    if let Err(e) = rescan_into(&host, &registries, &dir) {
                        log::error!("udf rescan failed (keeping previous table): {e}");
                    }
                })
            }
            _ => Box::new(|| {}),
        };
        watcher::spawn(
            watcher::Plan::new(PathBuf::from(&config_path), dir),
            rescan,
            |trigger| {
                metrics::global().reload_signal(trigger.as_str());
                log::info!(
                    "change detected ({}); requesting proxy reload (SIGHUP)",
                    trigger.as_str(),
                );
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGHUP);
                }
            },
        )
        .map_err(|e| format!("mcgateway_native: watcher: {e}"))?;
        log::info!("live reload armed for {config_path}");
    }

    Ok(registries)
}

fn build_registries() -> LuaResult<Arc<Registries>> {
    SHARED
        .get_or_init(init_shared)
        .clone()
        .map_err(LuaError::RuntimeError)
}

/// Origin for the monotonic clock `mcgw_native.now()` exposes.
/// Process-wide so timestamps taken in one Lua VM compare against
/// `now()` read in the dispatch path regardless of which VM armed it.
fn clock_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

fn now_nanos() -> i64 {
    i64::try_from(clock_origin().elapsed().as_nanos()).unwrap_or(i64::MAX)
}

#[allow(clippy::cast_precision_loss)] // sub-second values; f64 is exact enough
fn micros_to_seconds(us: i64) -> f64 {
    us as f64 / 1_000_000.0
}

#[allow(clippy::cast_precision_loss)]
fn nanos_to_seconds(ns: i64) -> f64 {
    ns as f64 / 1_000_000_000.0
}

fn status_label(s: Status) -> &'static str {
    match s {
        Status::Hit => "hit",
        Status::Miss => "miss",
        Status::Error => "error",
    }
}

/// Map caller-supplied label strings onto the closed label sets. The
/// fallthrough keeps request-derived or future values from minting
/// new label values — the cardinality contract.
fn op_label(s: &str) -> &'static str {
    match s {
        "read" => "read",
        "write" => "write",
        _ => "other",
    }
}

fn outcome_label(s: &str) -> &'static str {
    match s {
        "hit" => "hit",
        "miss" => "miss",
        "error" => "error",
        "stored" => "stored",
        "negative" => "negative",
        _ => "other",
    }
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
    /// Backend elapsed time in microseconds (memcached's
    /// `res:elapsed()`), captured for metrics only — it is not part
    /// of the merge ABI and never reaches the `Entry` views.
    elapsed: Option<i64>,
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
        let elapsed: Option<i64> = e.get("elapsed")?;
        out.push(OwnedEntry {
            key: key.as_bytes().to_vec(),
            pool: pool.to_str()?.to_string(),
            status: parse_status(&status.as_bytes())?,
            t,
            value: value.map(|s| s.as_bytes().to_vec()),
            line: line.map(|s| s.as_bytes().to_vec()),
            elapsed,
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

/// Record everything the read path exposes, from inside the merge
/// dispatch — the crossing already sees per-pool statuses and elapsed
/// times on the entries, the merge body was just timed, and `opts`
/// carries the keyspace attribution. One FFI crossing, all the read
/// metrics.
///
/// Infallible on purpose: the merge already succeeded, and metrics
/// must never re-couple serving to their own health (the same
/// contract the Lua side keeps by pcall'ing its hooks). Malformed
/// opts fields are simply not observed.
fn observe_read_dispatch(
    merge_name: &str,
    merge_seconds: f64,
    owned: &[OwnedEntry],
    result: &MergeResult,
    opts: Option<&LuaTable>,
) {
    let m = metrics::global();
    m.observe_merge_duration(merge_name, merge_seconds);
    for e in owned {
        m.observe_backend(
            &e.pool,
            status_label(e.status),
            e.elapsed.map(micros_to_seconds),
        );
    }
    let Some(opts) = opts else { return };
    let prefix: Option<LuaString> = opts.get("prefix").ok().flatten();
    let start: Option<i64> = opts.get("start").ok().flatten();
    if let Some(prefix) = prefix {
        let duration = start.map(|s| nanos_to_seconds(now_nanos().saturating_sub(s).max(0)));
        m.observe_request(
            &String::from_utf8_lossy(&prefix.as_bytes()),
            "read",
            read_outcome(result, owned),
            duration,
        );
    }
}

/// Mirror routes.lua's reply selection. A winner is labeled by the
/// *chosen entry's* status — Lua returns that entry's response
/// verbatim, so a merge that picks a miss entry yields a
/// client-visible miss, not a hit. An out-of-range winner falls
/// through to the same no-winner path Lua takes. (One accepted
/// sliver: a winner pointing at an error entry whose `res` was nil is
/// counted `error`, while Lua's fallback may synthesize a miss reply
/// when another backend missed.)
fn read_outcome(result: &MergeResult, owned: &[OwnedEntry]) -> &'static str {
    let no_winner = |owned: &[OwnedEntry]| {
        if owned.iter().any(|e| e.status == Status::Miss) {
            "miss"
        } else {
            "error"
        }
    };
    match result {
        MergeResult::Synthesized(_) => "hit",
        MergeResult::Winner(i) => match owned.get(*i).map(|e| e.status) {
            Some(status) => status_label(status),
            None => no_winner(owned),
        },
        MergeResult::Miss => no_winner(owned),
    }
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
        let merge = lua.create_function(
            move |lua, (name, entries, opts): (LuaString, LuaTable, Option<LuaTable>)| {
                let name = name_str(&name)?;
                let owned = project(&entries)?;
                let view = as_views(&owned);
                let merge_started = Instant::now();
                let result = registries.apply(&name, &view).ok_or_else(|| {
                    LuaError::RuntimeError(format!("mcgateway_native: unknown merge {name:?}"))
                })?;

                observe_read_dispatch(
                    &name,
                    merge_started.elapsed().as_secs_f64(),
                    &owned,
                    &result,
                    opts.as_ref(),
                );

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
            },
        )?;
        exports.set("merge", merge)?;
    }

    {
        let now = lua.create_function(|_, _: Variadic<LuaValue>| Ok(now_nanos()))?;
        exports.set("now", now)?;
    }

    {
        let observe = lua.create_function(
            |_, (prefix, op, outcome, start): (LuaString, LuaString, LuaString, Option<i64>)| {
                let duration =
                    start.map(|s| nanos_to_seconds(now_nanos().saturating_sub(s).max(0)));
                metrics::global().observe_request(
                    &String::from_utf8_lossy(&prefix.as_bytes()),
                    op_label(&String::from_utf8_lossy(&op.as_bytes())),
                    outcome_label(&String::from_utf8_lossy(&outcome.as_bytes())),
                    duration,
                );
                Ok(())
            },
        )?;
        exports.set("observe", observe)?;
    }

    {
        let observe_reload = lua.create_function(
            |_, (result, pools, keyspaces): (LuaString, usize, usize)| {
                let result = match &*String::from_utf8_lossy(&result.as_bytes()) {
                    "ok" => "ok",
                    "fallback" => "fallback",
                    _ => "other",
                };
                metrics::global().config_reload(result, pools, keyspaces);
                Ok(())
            },
        )?;
        exports.set("observe_reload", observe_reload)?;
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
