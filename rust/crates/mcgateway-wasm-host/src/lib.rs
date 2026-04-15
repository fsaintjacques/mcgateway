//! Wasmtime-backed implementation of [`mcgateway_core::Merge`].
//!
//! A [`WasmHost`] owns the shared [`wasmtime::Engine`], the epoch
//! ticker thread, and the AOT cache. A [`WasmMerge`] holds one
//! compiled module plus its name and per-call deadline budget and, on
//! each [`Merge::apply`] call, constructs a fresh [`wasmtime::Store`]
//! with epoch interruption armed, marshals the entry slice into guest
//! linear memory, invokes the guest's exported `mcgw_merge`, decodes
//! the result, and drops the store.
//!
//! Per-call stores are the point: a trapping or misbehaving merge
//! cannot corrupt state visible to subsequent calls. Wasmtime's
//! pooling allocator makes the per-call cost a fixed-size bump, which
//! merges — pure, short — amortize fine.
//!
//! This crate owns only the host-side codec, lifecycle, deadlines,
//! and log import. The guest-side ABI types (the `#[merge_fn]` proc
//! macro and the [`mcgateway_sdk`] crate) live elsewhere; this crate
//! is intentionally unaware of the SDK so the boundary stays
//! reviewable as wire format, not Rust types.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use mcgateway_core::{Entry, Merge, MergeResult};
use wasmtime::{
    Caller, Config, Engine, Error, Instance, Linker, Memory, Module, Result, Store, TypedFunc,
};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::WasiCtxBuilder;

/// ABI version the host understands. Bumped whenever the entry or
/// result encoding changes in an incompatible way. Guests expose their
/// compiled version via `mcgw_abi_version`; a mismatch refuses to load
/// the module.
pub const ABI_VERSION: u32 = 1;

/// Size in bytes of the serialized `Entry` record on the wire.
///
/// Layout (little-endian, no padding beyond what the fields already imply):
///
/// ```text
/// 0  key_ptr    u32
/// 4  key_len    u32
/// 8  pool_ptr   u32
/// 12 pool_len   u32
/// 16 value_ptr  u32  (0 = absent)
/// 20 value_len  u32
/// 24 line_ptr   u32  (0 = absent)
/// 28 line_len   u32
/// 32 status     u32  (0=hit, 1=miss, 2=error)
/// 36 t_present  u32  (0 or 1)
/// 40 t          i64
/// ```
pub const ENTRY_SIZE: u32 = 48;

const STATUS_HIT: u32 = 0;
const STATUS_MISS: u32 = 1;
const STATUS_ERROR: u32 = 2;

const RESULT_TAG_MISS: u8 = 0;
const RESULT_TAG_WINNER: u8 = 1;
const RESULT_TAG_SYNTHESIZED: u8 = 2;
const RESULT_TAG_GUEST_ERROR: u8 = 0xFF;

/// How long a single merge call may run before the host traps it.
/// Fixed for now; a per-module override lands with Stage 6.
pub const DEFAULT_DEADLINE_MS: u64 = 50;

/// Epoch tick interval. The ticker thread advances the engine's epoch
/// this often; a deadline of N milliseconds maps to `ceil(N /
/// TICK_INTERVAL_MS)` epoch ticks. 10 ms gives a ~5-tick budget for
/// the default 50 ms deadline, which is coarse enough to not dominate
/// the hot path and fine enough for the intended kill granularity.
pub const TICK_INTERVAL_MS: u64 = 10;

/// Maximum number of `mcgw_log` lines the host will emit per merge
/// call. Exceeded lines are dropped silently so a runaway guest can't
/// flood the gateway's stderr. Per-second rate limiting lands with
/// Stage 6 structured logging.
pub const LOG_BUDGET_PER_CALL: u32 = 16;

/// Maximum size of a single `mcgw_log` message. Guest-supplied `len`
/// is untrusted; without this cap a malicious guest could force the
/// host to allocate multiple gigabytes per call.
pub const LOG_MAX_BYTES: u32 = 4096;

macro_rules! werr {
    ($($arg:tt)*) => { Error::msg(format!($($arg)*)) };
}

/// Per-call state the store carries. Holds the WASI preview1 context
/// (empty; see [`new_wasi_ctx`]) alongside bookkeeping the `mcgw_log`
/// host import needs.
struct MergeStoreData {
    wasi: WasiP1Ctx,
    module_name: Arc<str>,
    log_budget_remaining: u32,
}

impl MergeStoreData {
    fn new(module_name: Arc<str>) -> Self {
        Self {
            wasi: WasiCtxBuilder::new().build_p1(),
            module_name,
            log_budget_remaining: LOG_BUDGET_PER_CALL,
        }
    }
}

/// Owns the engine and keeps an epoch ticker alive for its lifetime.
/// The ticker signals the engine's epoch counter every
/// [`TICK_INTERVAL_MS`], which wasmtime combines with each store's
/// per-call `set_epoch_deadline` to trap runaway guests.
///
/// The host is cheap to clone (`Arc` inside): every clone shares the
/// same engine and the same ticker. The ticker thread only exits when
/// the last clone is dropped.
#[derive(Clone)]
pub struct WasmHost {
    engine: Engine,
    // Shared via Arc so clones of WasmHost don't each spawn their own
    // ticker, and the ticker only stops on the drop of the last
    // reference. TickerHandle's Drop impl sets the stop flag and joins.
    _ticker: Arc<TickerHandle>,
}

struct TickerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Drop for TickerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

fn spawn_ticker(engine: Engine) -> TickerHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = std::thread::Builder::new()
        .name("mcgw-epoch-ticker".into())
        .spawn(move || {
            let tick = Duration::from_millis(TICK_INTERVAL_MS);
            while !stop_clone.load(Ordering::Acquire) {
                std::thread::sleep(tick);
                engine.increment_epoch();
            }
        })
        .expect("spawn epoch ticker");
    TickerHandle {
        stop,
        join: Some(handle),
    }
}

impl WasmHost {
    /// Build a host with Cranelift codegen at Speed opt level, epoch
    /// interruption armed, and a background ticker thread.
    pub fn new() -> Result<Self> {
        let mut cfg = Config::new();
        cfg.cranelift_opt_level(wasmtime::OptLevel::Speed);
        cfg.epoch_interruption(true);
        let engine = Engine::new(&cfg)?;
        let ticker = spawn_ticker(engine.clone());
        Ok(Self {
            engine,
            _ticker: Arc::new(ticker),
        })
    }

    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile a module from raw wasm bytes.
    pub fn compile(&self, wasm: &[u8]) -> Result<Module> {
        Module::new(&self.engine, wasm)
    }

    /// Load a wasmtime precompiled artifact (`Module::serialize`
    /// output).
    ///
    /// # Safety
    /// The bytes must have been produced by the same `wasmtime`
    /// version with a compatible [`Config`]. Corrupt or foreign bytes
    /// are undefined behaviour.
    pub unsafe fn compile_serialized(&self, bytes: &[u8]) -> Result<Module> {
        unsafe { Module::deserialize(&self.engine, bytes) }
    }
}

/// Build a linker populated with WASI preview1 and the `mcgw.log`
/// import. Shared between the load-time probe and per-call
/// instantiation so both wire the same host surface.
fn build_linker(engine: &Engine) -> Result<Linker<MergeStoreData>> {
    let mut linker: Linker<MergeStoreData> = Linker::new(engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |d: &mut MergeStoreData| &mut d.wasi)?;

    // mcgw.log(level: i32, ptr: i32, len: i32) — level is a u32 by
    // convention (0=trace..4=error); wasmtime's Caller-based wrap API
    // works in i32 and we reinterpret above the bit level.
    linker.func_wrap(
        "mcgw",
        "log",
        |mut caller: Caller<'_, MergeStoreData>, level: u32, ptr: u32, len: u32| {
            let Some(memory) = caller
                .get_export("memory")
                .and_then(wasmtime::Extern::into_memory)
            else {
                return;
            };
            // Cap untrusted guest len: a malicious module could pass
            // u32::MAX and force a multi-gigabyte per-call allocation.
            // Truncated messages are fine for diagnostic logs.
            let len = len.min(LOG_MAX_BYTES) as usize;
            let mut buf = vec![0u8; len];
            if memory.read(&caller, ptr as usize, &mut buf).is_err() {
                return;
            }
            let state = caller.data_mut();
            if state.log_budget_remaining == 0 {
                return;
            }
            state.log_budget_remaining -= 1;
            let level_label = match level {
                0 => "TRACE",
                1 => "DEBUG",
                2 => "INFO",
                3 => "WARN",
                4 => "ERROR",
                _ => "?",
            };
            let msg = String::from_utf8_lossy(&buf);
            eprintln!(
                "mcgw.log [{level_label}] {module}: {msg}",
                module = state.module_name,
            );
        },
    )?;

    Ok(linker)
}

/// A compiled merge. Immutable after construction.
pub struct WasmMerge {
    engine: Engine,
    module: Module,
    module_name: Arc<str>,
    required_flags: String,
    deadline_ticks: u64,
    // Built once at load time and reused across all calls. A Linker
    // depends only on the Engine, so the same instance is safe to
    // share across stores; per-call rebuild would re-register every
    // WASI preview1 import on each merge.
    linker: Linker<MergeStoreData>,
}

fn default_deadline_ticks() -> u64 {
    DEFAULT_DEADLINE_MS.div_ceil(TICK_INTERVAL_MS)
}

impl WasmMerge {
    /// Instantiate a merge from a compiled module, performing the ABI
    /// version handshake and capturing optional metadata exports.
    /// `name` is included in `mcgw.log` output and in deadline error
    /// messages.
    pub fn from_module(host: &WasmHost, module: Module, name: &str) -> Result<Self> {
        let engine = host.engine().clone();
        let module_name: Arc<str> = Arc::from(name);

        // Probe once to verify exports match the ABI and read any
        // optional metadata. Epoch interruption is armed at the engine
        // level, so the probe store still needs a deadline — give it a
        // generous one since the metadata calls are O(1) and not worth
        // timing.
        let mut store = Store::new(&engine, MergeStoreData::new(module_name.clone()));
        store.set_epoch_deadline(u64::MAX / 2);
        store.epoch_deadline_trap();
        let linker = build_linker(&engine)?;
        let instance = linker.instantiate(&mut store, &module)
            .map_err(|e| e.context("instantiate module"))?;

        let abi_version: TypedFunc<(), u32> = instance
            .get_typed_func(&mut store, "mcgw_abi_version")
            .map_err(|e| e.context("module is missing mcgw_abi_version export"))?;
        let reported = abi_version.call(&mut store, ())?;
        if reported != ABI_VERSION {
            return Err(werr!(
                "module ABI version mismatch: host={ABI_VERSION} guest={reported}"
            ));
        }

        let _: TypedFunc<(u32, u32), u32> = instance
            .get_typed_func(&mut store, "mcgw_alloc")
            .map_err(|e| e.context("module is missing mcgw_alloc export"))?;
        let _: TypedFunc<(u32, u32, u32), ()> = instance
            .get_typed_func(&mut store, "mcgw_dealloc")
            .map_err(|e| e.context("module is missing mcgw_dealloc export"))?;
        let _: TypedFunc<(u32, u32), u64> = instance
            .get_typed_func(&mut store, "mcgw_merge")
            .map_err(|e| e.context("module is missing mcgw_merge export"))?;
        let _: Memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| werr!("module is missing memory export"))?;

        let required_flags = read_required_flags(&mut store, &instance)?;

        Ok(Self {
            engine,
            module,
            module_name,
            required_flags,
            deadline_ticks: default_deadline_ticks(),
            linker,
        })
    }

    #[must_use]
    pub fn required_flags(&self) -> &str {
        &self.required_flags
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.module_name
    }

    /// Override the per-call deadline (in epoch ticks). Tests use
    /// this to provoke timeouts without sleeping for 50 ms.
    pub fn set_deadline_ticks(&mut self, ticks: u64) {
        self.deadline_ticks = ticks;
    }

    /// Invoke the merge directly, surfacing wasmtime errors. Trap,
    /// deadline, and encoding failures are returned as `Err`; the
    /// [`Merge::apply`] impl maps them all to `MergeResult::Miss`.
    pub fn run(&self, entries: &[Entry<'_>]) -> Result<MergeResult> {
        run_with_linker(
            &self.engine,
            &self.module,
            &self.linker,
            self.module_name.clone(),
            self.deadline_ticks,
            entries,
        )
    }
}

fn read_required_flags(
    store: &mut Store<MergeStoreData>,
    instance: &Instance,
) -> Result<String> {
    let Ok(func) = instance.get_typed_func::<(), u64>(&mut *store, "mcgw_required_flags") else {
        return Ok(String::new());
    };
    let packed = func.call(&mut *store, ())?;
    if packed == 0 {
        return Ok(String::new());
    }
    let ptr = (packed >> 32) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;
    let mem = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| werr!("missing memory"))?;
    let mut buf = vec![0u8; len as usize];
    mem.read(&*store, ptr as usize, &mut buf)?;
    String::from_utf8(buf).map_err(|e| werr!("required_flags not utf-8: {e}"))
}

impl Merge for WasmMerge {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult {
        match self.run(entries) {
            Ok(r) => r,
            Err(_) => MergeResult::Miss,
        }
    }

    fn required_flags(&self) -> &'static str {
        // `Merge::required_flags` returns `&'static str`; WASM-provided
        // flags are not static. The UdfLoader wraps WasmMerge in a
        // shim that resolves required_flags from its own `String`
        // table — see `WasmMerge::required_flags(&self) -> &str`.
        ""
    }
}

/// Execute one merge call against a pre-built linker. Factored out
/// so [`WasmMerge::run`] and the test helpers can both drive the
/// full marshal → invoke → decode path.
fn run_with_linker(
    engine: &Engine,
    module: &Module,
    linker: &Linker<MergeStoreData>,
    name: Arc<str>,
    deadline_ticks: u64,
    entries: &[Entry<'_>],
) -> Result<MergeResult> {
    let mut store = Store::new(engine, MergeStoreData::new(name));
    store.set_epoch_deadline(deadline_ticks);
    store.epoch_deadline_trap();

    let instance = linker.instantiate(&mut store, module)?;

    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| werr!("missing memory"))?;
    let alloc: TypedFunc<(u32, u32), u32> =
        instance.get_typed_func(&mut store, "mcgw_alloc")?;
    let dealloc: TypedFunc<(u32, u32, u32), ()> =
        instance.get_typed_func(&mut store, "mcgw_dealloc")?;
    let merge: TypedFunc<(u32, u32), u64> =
        instance.get_typed_func(&mut store, "mcgw_merge")?;

    let encoded = EncodedEntries::write(&mut store, &memory, &alloc, entries)?;
    let packed = merge.call(&mut store, (encoded.ptr, encoded.count))?;
    let result = decode_result(&mut store, &memory, &dealloc, packed)?;

    dealloc.call(&mut store, (encoded.ptr, encoded.total_size, 8))?;

    match result {
        MergeResult::Winner(i) if i >= entries.len() => Err(werr!(
            "guest returned out-of-range winner index {i} (entries={})",
            entries.len()
        )),
        other => Ok(other),
    }
}

/// Test-only helper: builds a fresh linker and runs the merge with a
/// generous 1000-tick deadline under a "<test>" module name. Used by
/// the `.wat` fixtures that exercise `run` directly without going
/// through [`WasmMerge::from_module`].
#[doc(hidden)]
pub fn run_test(engine: &Engine, module: &Module, entries: &[Entry<'_>]) -> Result<MergeResult> {
    let linker = build_linker(engine)?;
    run_with_linker(
        engine,
        module,
        &linker,
        Arc::from("<test>"),
        1000,
        entries,
    )
}

struct EncodedEntries {
    ptr: u32,
    count: u32,
    total_size: u32,
}

impl EncodedEntries {
    fn write(
        store: &mut Store<MergeStoreData>,
        memory: &Memory,
        alloc: &TypedFunc<(u32, u32), u32>,
        entries: &[Entry<'_>],
    ) -> Result<Self> {
        let count = u32::try_from(entries.len())
            .map_err(|_| werr!("too many entries for u32 count"))?;

        // Two-phase: guest-allocate each entry's variable-length
        // fields, then the entry array referencing those pointers.
        // N+1 allocs per merge; fine at merge-scale. Per-field
        // buffers are not individually `mcgw_dealloc`'d — the
        // per-call `Store` drops linear memory wholesale when `run`
        // returns. Only the entry-array buffer itself is freed (to
        // exercise the guest's free-list bookkeeping).
        let mut field_allocs: Vec<FieldPtrs> = Vec::with_capacity(entries.len());
        for e in entries {
            field_allocs.push(FieldPtrs::write(store, memory, alloc, e)?);
        }

        let total_size = count
            .checked_mul(ENTRY_SIZE)
            .ok_or_else(|| werr!("entry array size overflow"))?;
        let alloc_size = if total_size == 0 { 1 } else { total_size };
        let ptr = alloc.call(&mut *store, (alloc_size, 8))?;
        if ptr == 0 {
            return Err(werr!("guest mcgw_alloc returned null"));
        }

        let mut scratch = [0u8; ENTRY_SIZE as usize];
        for (i, (entry, fields)) in entries.iter().zip(field_allocs.iter()).enumerate() {
            encode_entry(entry, fields, &mut scratch);
            let offset = ptr as usize + i * ENTRY_SIZE as usize;
            memory.write(&mut *store, offset, &scratch)?;
        }

        Ok(Self {
            ptr,
            count,
            total_size: alloc_size,
        })
    }
}

struct FieldPtrs {
    key: (u32, u32),
    pool: (u32, u32),
    value: (u32, u32),
    line: (u32, u32),
}

impl FieldPtrs {
    fn write(
        store: &mut Store<MergeStoreData>,
        memory: &Memory,
        alloc: &TypedFunc<(u32, u32), u32>,
        e: &Entry<'_>,
    ) -> Result<Self> {
        Ok(Self {
            key: write_bytes(store, memory, alloc, e.key)?,
            pool: write_bytes(store, memory, alloc, e.pool.as_bytes())?,
            value: match e.value {
                Some(b) => write_bytes(store, memory, alloc, b)?,
                None => (0, 0),
            },
            line: match e.line {
                Some(b) => write_bytes(store, memory, alloc, b)?,
                None => (0, 0),
            },
        })
    }
}

fn write_bytes(
    store: &mut Store<MergeStoreData>,
    memory: &Memory,
    alloc: &TypedFunc<(u32, u32), u32>,
    bytes: &[u8],
) -> Result<(u32, u32)> {
    if bytes.is_empty() {
        let ptr = alloc.call(&mut *store, (1, 1))?;
        if ptr == 0 {
            return Err(werr!(
                "guest mcgw_alloc returned null for empty-slice sentinel"
            ));
        }
        return Ok((ptr, 0));
    }
    let len = u32::try_from(bytes.len()).map_err(|_| werr!("field too large for u32"))?;
    let ptr = alloc.call(&mut *store, (len, 1))?;
    if ptr == 0 {
        return Err(werr!("guest mcgw_alloc returned null"));
    }
    memory.write(&mut *store, ptr as usize, bytes)?;
    Ok((ptr, len))
}

fn encode_entry(entry: &Entry<'_>, fields: &FieldPtrs, out: &mut [u8; ENTRY_SIZE as usize]) {
    let status = match entry.status {
        mcgateway_core::Status::Hit => STATUS_HIT,
        mcgateway_core::Status::Miss => STATUS_MISS,
        mcgateway_core::Status::Error => STATUS_ERROR,
    };
    let (t_present, t) = match entry.t {
        Some(v) => (1u32, v),
        None => (0u32, 0),
    };
    out[0..4].copy_from_slice(&fields.key.0.to_le_bytes());
    out[4..8].copy_from_slice(&fields.key.1.to_le_bytes());
    out[8..12].copy_from_slice(&fields.pool.0.to_le_bytes());
    out[12..16].copy_from_slice(&fields.pool.1.to_le_bytes());
    out[16..20].copy_from_slice(&fields.value.0.to_le_bytes());
    out[20..24].copy_from_slice(&fields.value.1.to_le_bytes());
    out[24..28].copy_from_slice(&fields.line.0.to_le_bytes());
    out[28..32].copy_from_slice(&fields.line.1.to_le_bytes());
    out[32..36].copy_from_slice(&status.to_le_bytes());
    out[36..40].copy_from_slice(&t_present.to_le_bytes());
    out[40..48].copy_from_slice(&t.to_le_bytes());
}

fn decode_result(
    store: &mut Store<MergeStoreData>,
    memory: &Memory,
    dealloc: &TypedFunc<(u32, u32, u32), ()>,
    packed: u64,
) -> Result<MergeResult> {
    let tag = (packed & 0xFF) as u8;
    match tag {
        RESULT_TAG_MISS => Ok(MergeResult::Miss),
        RESULT_TAG_WINNER => {
            let idx = (packed >> 32) as u32;
            Ok(MergeResult::Winner(idx as usize))
        }
        RESULT_TAG_SYNTHESIZED => {
            let desc_ptr = (packed >> 32) as u32;
            let mut desc = [0u8; 8];
            memory.read(&*store, desc_ptr as usize, &mut desc)?;
            let ptr = u32::from_le_bytes(desc[0..4].try_into().unwrap());
            let len = u32::from_le_bytes(desc[4..8].try_into().unwrap());
            let mut buf = vec![0u8; len as usize];
            memory.read(&*store, ptr as usize, &mut buf)?;
            dealloc.call(&mut *store, (ptr, len.max(1), 1))?;
            dealloc.call(&mut *store, (desc_ptr, 8, 4))?;
            Ok(MergeResult::Synthesized(buf))
        }
        RESULT_TAG_GUEST_ERROR => {
            let code = ((packed >> 8) & 0xFF) as u8;
            Err(werr!("guest returned error code {code}"))
        }
        other => Err(werr!("guest returned unknown result tag {other}")),
    }
}

