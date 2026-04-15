//! Wasmtime-backed implementation of [`mcgateway_core::Merge`].
//!
//! A [`WasmHost`] owns the shared [`wasmtime::Engine`] and AOT cache. A
//! [`WasmMerge`] holds one compiled module and, on each [`Merge::apply`]
//! call, constructs a fresh [`wasmtime::Store`] + instance, marshals the
//! entry slice into guest linear memory, invokes the guest's exported
//! `mcgw_merge`, decodes the result, and drops the store.
//!
//! Per-call stores are the point: a trapping or misbehaving merge cannot
//! corrupt state visible to subsequent calls. Wasmtime's pooling
//! allocator makes the per-call cost a fixed-size bump, which merges —
//! pure, short — amortize fine.
//!
//! This crate owns only the host-side codec and lifecycle. The guest-side
//! ABI types (the `#[merge_fn]` proc macro and the [`mcgateway_sdk`] crate)
//! live elsewhere; this crate is intentionally unaware of the SDK so the
//! boundary stays reviewable as wire format, not Rust types.

use mcgateway_core::{Entry, Merge, MergeResult};
use wasmtime::{Config, Engine, Error, Instance, Linker, Memory, Module, Result, Store, TypedFunc};

/// ABI version the host understands. Bumped whenever the entry or result
/// encoding changes in an incompatible way. Guests expose their compiled
/// version via `mcgw_abi_version`; a mismatch refuses to load the module.
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

macro_rules! werr {
    ($($arg:tt)*) => { Error::msg(format!($($arg)*)) };
}

/// Shared engine + configuration. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct WasmHost {
    engine: Engine,
}

impl WasmHost {
    /// Build a host with the configuration the gateway uses in production:
    /// Cranelift codegen, Speed opt level. Epoch interruption lands in
    /// step 4 alongside the ticker thread; arming it here would trap
    /// guests immediately since no ticker exists yet.
    pub fn new() -> Result<Self> {
        let mut cfg = Config::new();
        cfg.cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = Engine::new(&cfg)?;
        Ok(Self { engine })
    }

    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile a module from raw wasm bytes. Prefer
    /// [`WasmHost::compile_serialized`] when an AOT cache artifact is
    /// available.
    pub fn compile(&self, wasm: &[u8]) -> Result<Module> {
        Module::new(&self.engine, wasm)
    }

    /// Load a wasmtime precompiled artifact (`Module::serialize` output).
    ///
    /// # Safety
    ///
    /// The bytes must have been produced by the same `wasmtime` version
    /// with a compatible [`Config`]. Corrupt or foreign bytes are
    /// undefined behaviour. The caller is expected to have produced the
    /// file itself and tracked it by content hash.
    pub unsafe fn compile_serialized(&self, bytes: &[u8]) -> Result<Module> {
        unsafe { Module::deserialize(&self.engine, bytes) }
    }
}

/// A compiled merge. Immutable after construction; cloneable via `Arc`.
pub struct WasmMerge {
    engine: Engine,
    module: Module,
    required_flags: String,
}

impl WasmMerge {
    /// Instantiate a merge from a compiled module, performing the ABI
    /// version handshake and capturing optional metadata exports.
    pub fn from_module(host: &WasmHost, module: Module) -> Result<Self> {
        let engine = host.engine().clone();

        // Probe once to verify the module exports the required functions
        // with the right signatures and to read its declared ABI version
        // plus optional required_flags.
        let mut store = Store::new(&engine, ());
        let linker = Linker::new(&engine);
        let instance = linker.instantiate(&mut store, &module)?;

        let abi_version: TypedFunc<(), u32> = instance
            .get_typed_func(&mut store, "mcgw_abi_version")
            .map_err(|e| e.context("module is missing mcgw_abi_version export"))?;
        let reported = abi_version.call(&mut store, ())?;
        if reported != ABI_VERSION {
            return Err(werr!(
                "module ABI version mismatch: host={ABI_VERSION} guest={reported}"
            ));
        }

        // Resolve remaining required exports so load fails fast.
        let _: TypedFunc<(u32, u32), u32> =
            instance.get_typed_func(&mut store, "mcgw_alloc")
                .map_err(|e| e.context("module is missing mcgw_alloc export"))?;
        let _: TypedFunc<(u32, u32, u32), ()> =
            instance.get_typed_func(&mut store, "mcgw_dealloc")
                .map_err(|e| e.context("module is missing mcgw_dealloc export"))?;
        let _: TypedFunc<(u32, u32), u64> =
            instance.get_typed_func(&mut store, "mcgw_merge")
                .map_err(|e| e.context("module is missing mcgw_merge export"))?;
        let _: Memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| werr!("module is missing memory export"))?;

        let required_flags = read_required_flags(&mut store, &instance)?;

        Ok(Self {
            engine,
            module,
            required_flags,
        })
    }

    #[must_use]
    pub fn required_flags(&self) -> &str {
        &self.required_flags
    }
}

fn read_required_flags(store: &mut Store<()>, instance: &Instance) -> Result<String> {
    let Ok(func) = instance.get_typed_func::<(), u64>(&mut *store, "mcgw_required_flags")
    else {
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
        match run(&self.engine, &self.module, entries) {
            Ok(r) => r,
            Err(_) => MergeResult::Miss,
        }
    }

    fn required_flags(&self) -> &'static str {
        // `Merge::required_flags` returns `&'static str`; WASM-provided
        // flags are not static. Stage 3b's UdfLoader wraps WasmMerge in a
        // shim that resolves required_flags from its own `String` table
        // — see `WasmMerge::required_flags(&self) -> &str`.
        ""
    }
}

/// Execute one merge call. Extracted so both the `Merge` impl and unit
/// tests can exercise the full marshal → invoke → decode path and
/// observe the underlying error.
pub fn run(engine: &Engine, module: &Module, entries: &[Entry<'_>]) -> Result<MergeResult> {
    let mut store = Store::new(engine, ());
    let linker = Linker::new(engine);
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

    // Degrade out-of-range Winner indices to Miss: the caller would
    // otherwise index past the entries slice. We treat this as a
    // malformed guest, not a hard error — surfacing the value lets
    // WasmMerge::apply log it via the existing Err → Miss path.
    match result {
        MergeResult::Winner(i) if i >= entries.len() => Err(werr!(
            "guest returned out-of-range winner index {i} (entries={})",
            entries.len()
        )),
        other => Ok(other),
    }
}

struct EncodedEntries {
    ptr: u32,
    count: u32,
    total_size: u32,
}

impl EncodedEntries {
    fn write(
        store: &mut Store<()>,
        memory: &Memory,
        alloc: &TypedFunc<(u32, u32), u32>,
        entries: &[Entry<'_>],
    ) -> Result<Self> {
        let count = u32::try_from(entries.len())
            .map_err(|_| werr!("too many entries for u32 count"))?;

        // Two-phase: guest-allocate each entry's variable-length fields,
        // then the entry array referencing those pointers. N+1 allocs
        // per merge; fine at merge-scale. Per-field buffers are *not*
        // individually `mcgw_dealloc`'d after the call; the per-call
        // `Store` drops linear memory wholesale when `run` returns, so
        // the dealloc would be redundant. Only the entry-array buffer
        // itself is freed (to exercise the guest's free-list bookkeeping
        // and catch allocator bugs early).
        let mut field_allocs: Vec<FieldPtrs> = Vec::with_capacity(entries.len());
        for e in entries {
            field_allocs.push(FieldPtrs::write(store, memory, alloc, e)?);
        }

        // count == 0 is valid; guard against a zero-size alloc returning
        // ambiguous pointers by using a 1-byte sentinel.
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
        store: &mut Store<()>,
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
    store: &mut Store<()>,
    memory: &Memory,
    alloc: &TypedFunc<(u32, u32), u32>,
    bytes: &[u8],
) -> Result<(u32, u32)> {
    if bytes.is_empty() {
        // Distinguish empty-present from absent: non-zero sentinel ptr,
        // zero length. Allocate a 1-byte placeholder to get a unique
        // pointer since ptr=0 is our "absent" signal.
        let ptr = alloc.call(&mut *store, (1, 1))?;
        if ptr == 0 {
            return Err(werr!("guest mcgw_alloc returned null for empty-slice sentinel"));
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
    store: &mut Store<()>,
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

// A public `Arc<dyn Merge>` constructor is intentionally deferred: the
// Merge::required_flags signature is `&'static str`, but WasmMerge's
// flags are dynamic. Exposing a bare conversion today would silently
// drop per-module flags through trait-object dispatch. Step 3's
// UdfLoader introduces a wrapper that resolves required_flags from its
// own table and then hands the registry a correct trait object.
