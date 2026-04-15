# Stage 3b — WASM merge host behind the Stage 3a boundary

Fourth deliverable from the [implementation plan](implementation-plan.md), and
the second half of the original Stage 3. Stage 3a established the Lua ↔ Rust
boundary and moved the three built-in merges into `mcgateway-core` +
`mcgateway-merge-builtins`, dispatched through a `Registry` behind a trait
object. Stage 3b adds a second `Merge` implementation: a WASM-hosted one,
compiled from user-authored Rust via `wasm32-wasip1`, loaded from disk at
startup and on change.

The point is that the host shape is already fixed. A WASM merge is just an
`impl Merge` backed by a `wasmtime::Instance`. The Lua side does not learn
the difference; `routes.lua` keeps calling `mcgateway_native.merge(name,
entries)`. Everything new in this stage lives inside `mcgateway-clib` and
two new crates, against the exact trait surface Stage 3a froze.

---

## Goal

A user drops a `.wasm` file into `/etc/mcgateway/udf/`. The gateway compiles
it, registers it by name, and any keyspace whose `merge:` field references
that name dispatches through it on subsequent requests. Behaviour of the
three Stage 3a built-ins is unchanged; built-ins still win on name
collisions (native takes precedence, WASM fills the rest of the namespace).

Concretely:

1. A `mcgateway-wasm-host` crate wraps `wasmtime` and implements
   `mcgateway_core::Merge` for a compiled module. It owns a module cache, a
   `wasmtime::Engine`, and the per-call `Store`/`Instance` lifecycle.
2. A `mcgateway-sdk` crate gives users the `Entry` / `MergeResult` types
   and a `#[merge_fn]` proc macro that emits the guest-side ABI
   boilerplate (exported `merge` function, memory import, entry decoder,
   result encoder).
3. `mcgateway-clib` gains a `UdfLoader` that watches
   `/etc/mcgateway/udf/` via `notify`, compiles each `.wasm` through
   wasmtime's AOT pipeline, and installs a `WasmMerge` into the
   `Registry`. Registry becomes mutable behind an `ArcSwap` so in-flight
   merges never race against reloads.
4. `routes.lua` stays the way Stage 3a left it. Only `__mcgw:names` grows:
   names returned include both built-ins and currently-loaded WASM
   modules. The `__udf:` prefix handler is still `SERVER_ERROR`; disk is
   the primary registration path.

This stage exists to prove:

- The `Merge` trait Stage 3a shaped is actually WASM-shaped and we didn't
  accidentally bake Lua-specific assumptions into it.
- Wasmtime embedded inside the memcached proxy's Lua VM does not leak
  memory, file descriptors, or threads per request.
- Hot-reload via `ArcSwap` is race-free under the kind workload without
  draining in-flight requests.
- The SDK ergonomics are tolerable: a 30-line `merge_fn` in user-land
  with `cargo build --target wasm32-wasip1` works end-to-end.

---

## Non-goals

- **`__udf:` runtime registration over the memcache protocol.** Still
  rejected with `SERVER_ERROR udf not supported`. Disk is the only
  registration path; the operator (Stage 4) is what writes to disk in
  production. A dev-time push path lands with the CLI in Stage 7.
- **WASI beyond `wasi_snapshot_preview1` with capabilities stripped to
  nothing.** No filesystem, no clocks-beyond-monotonic, no env, no
  network. A merge is a pure function; giving it ambient authority is a
  footgun with no upside. If a future merge needs monotonic time for a
  bounded computation, we'll add `wasi:clocks/monotonic` explicitly.
- **Component Model.** wasmtime's core module API is sufficient and
  smaller to reason about. Revisit when a concrete need for typed
  interfaces across modules appears.
- **Preemptive fuel metering / epoch interruption in v1.** We set a
  wall-clock deadline per call via `Store::set_epoch_deadline` and a
  ticker thread; that's enough to kill runaway merges. Fuel-based
  accounting is observability work, not safety; defer to Stage 6.
- **Multi-language guest SDKs.** Rust only. The ABI is stable enough
  that another language can target it later, but we don't ship the
  bindings until someone asks.
- **Operator, CRDs, GCS fuse, metrics beyond Stage 3a's diagnostic
  path.** Stages 4 / 5 / 6 own those.
- **Changing Stage 3a built-in behaviour or any Stage 1/2 Lua.**
  Built-ins remain native; their Rust unit tests and the kind suite pass
  unchanged.

---

## Scope

| Feature | In scope |
|---|---|
| `mcgateway-wasm-host` crate: wasmtime `Engine`, AOT compile cache, `WasmMerge: Merge` | ✓ |
| `mcgateway-sdk` crate: guest-side `Entry` / `MergeResult`, `#[merge_fn]` proc macro | ✓ |
| `mcgateway-sdk-macros` crate: the proc-macro implementation | ✓ |
| Guest ABI: linear-memory entry buffer, exported `merge(ptr, len) -> u64`, allocator exports | ✓ |
| `UdfLoader` in `mcgateway-clib`: directory scan + `notify` watch + `ArcSwap<Registry>` | ✓ |
| Wall-clock deadline enforcement via epoch interruption | ✓ |
| AOT compile cache on disk (`$UDF_DIR/.cache/<hash>.cwasm`) | ✓ |
| Example merge UDF in `rust/examples/` exercising the ABI end-to-end | ✓ |
| Protobuf merge example (`merge-profile-proto`) using `prost` + `#[merge_fn]` with round-trip tests | ✓ |
| Host-imported `mcgw_log(level, ptr, len)` for guest-side diagnostics, rate-limited host-side | ✓ |
| Kind test: drop a `.wasm` into the image's UDF dir, query a keyspace referencing it | ✓ |
| Name collision policy: built-ins shadow disk modules; log a warning | ✓ |
| `__mcgw:names` lists WASM modules alongside built-ins | ✓ |
| `__udf:` handler | — (still `SERVER_ERROR`) |
| Operator / CRDs / GCS fuse | — (Stages 4 & 5) |
| Prom metrics (trap counter, compile-time histogram, deadline-kill counter) | — (Stage 6; structure log lines now so they're easy to scrape later) |
| Component Model, WASI beyond a no-authority preview1 | — |

---

## Project layout

```
rust/
  Cargo.toml                       (edited) add new crates to workspace
  crates/
    mcgateway-core/                (unchanged surface; see §ABI notes)
    mcgateway-merge-builtins/      (unchanged)
    mcgateway-clib/                (edited) embeds UdfLoader; Registry becomes ArcSwap
    mcgateway-wasm-host/           NEW lib: Engine, compile cache, WasmMerge
    mcgateway-sdk/                 NEW lib: guest-side types + re-exports
    mcgateway-sdk-macros/          NEW proc-macro lib: #[merge_fn]
  examples/
    merge-last-n-wins/             NEW user-facing example; builds to wasm32-wasip1
    merge-profile-proto/           NEW prost-based example: decode/merge/re-encode Profile
lua/mcgateway/
  routes.lua                       (edited) __mcgw:names covers WASM; __udf: unchanged
docker/Dockerfile                  (edited) create /etc/mcgateway/udf; strip wasmtime build cache
Makefile                           (edited) cargo build for the example WASM; copy into image for kind
doc/plans/stage-3b-wasm-merges.md  (this file)
```

Host-side layering: `mcgateway-clib` depends on `mcgateway-core` +
`mcgateway-merge-builtins` + `mcgateway-wasm-host`. The wasm-host crate
depends on `mcgateway-core` only. The guest SDK does **not** depend on
`mcgateway-core`; it has its own `Entry` / `MergeResult` shaped to the
wire format. Keeping the guest crate std-only-`no_std`-friendly prevents
accidental host-side deps leaking into user UDFs.

---

## Guest ABI

This is the one load-bearing design decision in the stage. Every other
choice flows from it, and it's the thing Stage 3a deliberately did not
lock in.

**Shape.** A WASM module exports:

- `memory` (the default linear memory).
- `mcgw_alloc(size: u32, align: u32) -> u32` — host-called allocator.
  Returns a pointer into linear memory or `0` on failure.
- `mcgw_dealloc(ptr: u32, size: u32, align: u32)` — paired free.
- `mcgw_merge(entries_ptr: u32, entries_len: u32) -> u64` — the merge
  entry point. Returns an encoded `MergeResult` (see below).
- `mcgw_abi_version() -> u32` — returns a host/guest handshake version.
  Host refuses to load modules whose version it doesn't understand.

**Entry encoding** (host → guest, inside linear memory):

```
entries := count:u32 | entry[count]
entry   := key_ptr:u32 | key_len:u32
         | pool_ptr:u32 | pool_len:u32
         | status:u8    | _pad:u8[3]
         | t_present:u8 | _pad:u8[7] | t:i64
         | value_ptr:u32 | value_len:u32   // value_ptr=0 iff value absent
         | line_ptr:u32  | line_len:u32    // line_ptr=0  iff line absent
```

All pointers point into the guest's own linear memory, into the region
`mcgw_alloc`'d by the host before the call. The host writes the fields,
calls `mcgw_merge`, and frees via `mcgw_dealloc` after reading the
result. Alignment is `8` for the outer buffer; fields are laid out so the
C-shape is `repr(C)`-compatible.

**Result encoding** (guest → host, returned as `u64`):

```
tag: u8 in the low byte
  0 = Miss           — remaining bits ignored
  1 = Winner(u32)    — index in the high 32 bits
  2 = Synthesized    — ptr:u32 in bits 8..40, len:u32 in bits 40..64
  0xFF = GuestError  — low 8 bits = tag, next 8 bits = error code
```

Synthesized buffers live in guest memory; the host copies them out and
then `mcgw_dealloc`'s. No ownership crosses the boundary.

**Why this shape and not wit/component-model.** Component Model buys us
nominally-typed interfaces across languages we don't ship today, at the
cost of a stack of generated adapters for a single entry point. Core
modules + a hand-written encoder give us a ~50-line host codec, a
~30-line guest codec, and zero codegen surprises. If we later want to
target non-Rust guests, revisit.

**`mcgateway-core::Entry` stays as-is.** The host converts the borrowed
`&[Entry<'_>]` into the linear-memory layout at call time. That's the
one place we pay the serialization cost — one write pass per merge, no
retention. Exactly the overhead the FFI boundary was designed to admit.

**Version negotiation.** `mcgw_abi_version` returns a `u32` the host
compares against a compile-time constant in `mcgateway-wasm-host`. On
mismatch: refuse to load, log the file path and both versions, keep any
previously-loaded module registered under that name.

---

## Host architecture

### `mcgateway-wasm-host`

One `Engine` per Lua VM (per IO thread). Engines are cheap; the expensive
thing is module compilation, which is cached.

```rust
pub struct WasmHost {
    engine: wasmtime::Engine,
    cache_dir: PathBuf,        // $UDF_DIR/.cache
}

impl WasmHost {
    pub fn new(cache_dir: PathBuf) -> Result<Self> { … }
    pub fn compile(&self, wasm: &[u8]) -> Result<wasmtime::Module> { … }
    pub fn instantiate(&self, module: &wasmtime::Module) -> Result<WasmMerge> { … }
}

pub struct WasmMerge {
    module: wasmtime::Module,
    engine: wasmtime::Engine,   // shared ref
    deadline_ticks: u64,        // epoch ticks = wall-ms budget
    required_flags: &'static str,
}

impl mcgateway_core::Merge for WasmMerge {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult { … }
    fn required_flags(&self) -> &'static str { self.required_flags }
}
```

Per call, `apply` creates a fresh `Store<()>` with epoch interruption
armed, instantiates the module (cheap; linking is done at compile time),
allocates the linear-memory buffer via `mcgw_alloc`, writes the entry
table, invokes `mcgw_merge`, decodes the result, frees via
`mcgw_dealloc`, drops the store. Stores are per-call so a trapping merge
cannot corrupt subsequent calls.

**Why per-call stores.** Reusing stores means reusing linear memory, which
means we'd need to reset guest state between calls. Merges are pure; the
cost of a fresh store is a fresh linear-memory allocation, which
wasmtime makes cheap through its pooling allocator. Enable
`PoolingAllocationConfig` with bounded instance counts sized to
per-IO-thread concurrency.

**Module cache.** `cache_dir/<blake3(wasm_bytes)>.cwasm` via
`Module::serialize` / `Module::deserialize`. Cache is populated on first
compile and read on startup; if the on-disk artifact fails to
deserialize (wasmtime version drift across image upgrades) we recompile
transparently. Size-cap the directory at N=64 entries with LRU eviction;
at 64 modules * ~1 MB each we're well under any reasonable image's
overhead budget.

**Deadline.** `Engine::config().epoch_interruption(true)`. A single host
thread, owned by `UdfLoader`, ticks the engine's epoch every 10 ms. Each
call sets `store.set_epoch_deadline(N)` where N = budget / 10 ms. On
deadline expiry wasmtime traps; the host catches the trap, logs it with
(module name, key, pool list), returns `MergeResult::Miss`. Budget
default: 50 ms, overrideable per-merge via a `required_flags`-style
metadata hook carved out in a follow-up — not configurable in v1.

### `mcgateway-clib` changes

Registry ownership moves from `Arc<Registry>` to
`arc_swap::ArcSwap<Registry>`. The Lua-exported functions load the
current registry pointer at call time (cheap; one atomic read).

A new `UdfLoader` owns:

- The `WasmHost`.
- A `notify::RecommendedWatcher` pointed at `/etc/mcgateway/udf/`.
- A background thread that debounces events (200 ms), rescans the
  directory, compiles new/changed modules, and atomically swaps in a
  fresh `Registry` via `ArcSwap::store`.
- The epoch-ticker thread.

`luaopen_mcgateway_native` blocks on the initial directory scan so
startup failures are caught during proxy config (matches Stage 3a's
posture on unknown merge names). Subsequent reloads are non-blocking and
log per-module success/failure. A failed reload of a specific module
leaves the previously-loaded version of that module in place; it does
not take down the whole registry.

Name-collision rule: built-ins are inserted first; `UdfLoader` refuses
to register a disk module whose name collides with a built-in, logs a
warning once per (name, file) pair, and leaves the built-in in place.
Rationale: built-ins are the observable contract; a user shouldn't be
able to silently replace `last-write-wins` by dropping a file.

### `routes.lua` changes

One edit: `__mcgw:names` already calls `mcgateway_native.names()`; since
the host-side `names()` reads from the current `ArcSwap` it automatically
covers WASM modules. No Lua-visible change beyond that.

`required_flags(name)` likewise resolves through the ArcSwap path and
will return the WASM module's declared flags. Keyspace config reload (a
Lua concern, out of scope for this stage) keeps driving which merge
name a keyspace references — adding a module to disk doesn't route
traffic to it; the keyspace config must mention it. That's intentional:
disk is the *capability* surface, config is the *routing* surface.

---

## `mcgateway-sdk`

Guest-facing. Published eventually to crates.io; for v1, just a path
dep used by `rust/examples/*`. Surface:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status { Hit, Miss, Error }

#[derive(Clone, Debug)]
pub struct Entry<'a> {
    pub key:    &'a [u8],
    pub pool:   &'a str,
    pub status: Status,
    pub t:      Option<i64>,
    pub value:  Option<&'a [u8]>,
    pub line:   Option<&'a [u8]>,
}

pub enum MergeResult {
    Winner(usize),
    Synthesized(Vec<u8>),
    Miss,
}

pub use mcgateway_sdk_macros::merge_fn;
```

A `#[merge_fn]` attribute on a user function generates:

- `#[no_mangle] pub extern "C" fn mcgw_merge(ptr: u32, len: u32) -> u64`
  — decodes the entry buffer, calls the user function, encodes the
  result.
- `#[no_mangle] pub extern "C" fn mcgw_alloc` / `mcgw_dealloc` — thin
  wrappers over the guest's global allocator.
- `#[no_mangle] pub extern "C" fn mcgw_abi_version() -> u32` — returns
  the SDK's compiled-in version constant.

Plus an optional `#[merge_fn(required_flags = "t")]` arg that wires the
value into an exported function the host calls once at load time to
populate `WasmMerge::required_flags`.

User-facing example:

```rust
use mcgateway_sdk::{merge_fn, Entry, MergeResult, Status};

#[merge_fn(required_flags = "t")]
pub fn last_n_wins(entries: &[Entry<'_>]) -> MergeResult {
    let mut best: Option<(usize, i64)> = None;
    for (i, e) in entries.iter().enumerate() {
        if e.status != Status::Hit { continue; }
        let Some(t) = e.t else { continue; };
        if best.map_or(true, |(_, bt)| t > bt) { best = Some((i, t)); }
    }
    best.map_or(MergeResult::Miss, |(i, _)| MergeResult::Winner(i))
}
```

`cargo build --target wasm32-wasip1 --release` produces a `.wasm`;
dropping it in `/etc/mcgateway/udf/` registers it as
`last_n_wins` (module file name without extension).

---

## Build and packaging

### Cargo

Workspace adds `mcgateway-wasm-host`, `mcgateway-sdk`,
`mcgateway-sdk-macros`. Host crates stay `cdylib` / `rlib` as before; SDK
crates are `rlib` + `proc-macro`. No new cdylib.

wasmtime pulled in with default features minus `wat` and `cranelift`'s
debugger integration; we build with cranelift but don't need the
introspection.

### Docker

Dockerfile adds:

- `mkdir -p /etc/mcgateway/udf` in the runtime stage; volume for the
  kind test bind-mounts module files there.
- Build-stage step that compiles `rust/examples/merge-last-n-wins` for
  `wasm32-wasip1` so the kind image ships one real `.wasm` for the
  integration test.
- `RUN` step that pre-populates `/etc/mcgateway/udf/.cache` by
  invoking the gateway's own compile path at build time (optional —
  trades image size for startup speed; skip if the hash is off).

Image size delta: wasmtime + cranelift is ~8 MB stripped. Acceptable;
keep an eye on it when Stage 4 lands and starts shipping multiple
binaries per image.

### Makefile

- `build` gains `cargo build --target wasm32-wasip1 --release -p
  merge-last-n-wins` as a separate target, gated on the wasi target
  being installed (`rustup target list --installed` check).
- `check` gains `cargo test -p mcgateway-wasm-host` which uses a small
  hand-written `.wat` fixture to exercise the host codec without
  shelling out to cargo-wasi.
- `test-kind` grows `TestWasmMergeDispatch`.

---

## Testing

### Rust unit tests — `mcgateway-wasm-host`

Against `.wat` fixtures checked in under `crates/mcgateway-wasm-host/
tests/fixtures/`:

- Module that always returns `Winner(0)` — smoke.
- Module that returns `Miss` — smoke.
- Module that returns `Synthesized(b"hello")` — covers the copy-out
  path.
- Module that traps unconditionally — host must return `Miss`, log
  once, not poison subsequent calls.
- Module that sleeps past the deadline (busy loop in `.wat`) — host
  must kill via epoch interruption and return `Miss`.
- Module with wrong `mcgw_abi_version` — compile succeeds, instantiate
  fails with a clear error, registry skips the name.
- Module that returns `Winner(999)` out of range — host must treat
  as `Miss` and log.

### Rust integration tests — `mcgateway-sdk`

Compile the `merge-last-n-wins` example inline via `cargo` as part of a
build.rs in a dedicated integration-test crate, then load the resulting
`.wasm` through `mcgateway-wasm-host`. Asserts the SDK + host round-trip
for the three `MergeResult` variants against hand-built `Entry` slices.

This is the one place in the tree that does a nested cargo build; keep
it behind `#[cfg(test)]` and out of `cargo build`'s default path.

### Rust integration tests — protobuf merge via `prost`

This is the test that proves the stage is production-shaped: a real UDF
doing the thing production will ask of it — merging protobuf-encoded
values from multiple pools into a single synthesized payload, round-tripped
through wasmtime exactly as a deployed merge would be.

Lives in a new `rust/examples/merge-profile-proto/` crate:

- Depends on `mcgateway-sdk` + `prost` (`default-features = false`,
  `features = ["std"]` — `prost` is no_std-compatible and compiles cleanly
  for `wasm32-wasip1`).
- Ships a `build.rs` invoking `prost-build` against a
  `proto/profile.proto`:

  ```proto
  syntax = "proto3";
  package mcgateway.test;

  message Profile {
    string user_id   = 1;
    int64  updated_at = 2;
    map<string, string> attrs = 3;
    repeated string badges = 4;
  }
  ```

- `#[merge_fn(required_flags = "t")]` that:
  1. Decodes each `Hit` entry's `value` as `Profile` via `Profile::decode`.
  2. Skips entries that fail to decode, logs via
     `mcgateway_sdk::log::warn!` (a thin macro wrapping a host-imported
     log function — see below).
  3. Picks the latest `updated_at` as the base.
  4. Unions the `attrs` map (latest `updated_at` wins on key collision).
  5. Unions `badges` as a stable-ordered dedup.
  6. Re-encodes via `Profile::encode_to_vec` and returns
     `MergeResult::Synthesized(bytes)`.
  7. Falls back to `MergeResult::Miss` if no entry decoded.

The integration test in `rust/crates/mcgateway-wasm-host/tests/proto.rs`:

1. Builds the example crate to `wasm32-wasip1` via a small
   `test_helpers` module that shells out to cargo (gated behind a
   `proto-tests` feature so plain `cargo test` on the host crate stays
   fast; `make check` sets the feature).
2. Loads the `.wasm` through `WasmHost::compile` + `instantiate`.
3. Constructs three `Entry` slices exercising:
   - **Three-pool union:** distinct `attrs` keys across pools, disjoint
     `badges`, differing `updated_at`. Assert the decoded result has the
     union of all attrs with the newest-pool's value on collision,
     sorted-deduped badges, and `updated_at == max(t)`.
   - **Corrupt payload tolerance:** one pool returns random bytes that
     don't decode as `Profile`. Merge succeeds using the remaining two.
   - **All-miss:** every entry is `Status::Miss`. Result is
     `MergeResult::Miss`.
   - **All-error:** every entry is `Status::Error`. Result is
     `MergeResult::Miss`.
   - **Single hit:** one hit, two misses. Result is `Synthesized` and
     decodes to exactly the input `Profile`.
   - **Allocation ceiling:** feed a 2 MiB `attrs` map across pools;
     assert the merge completes under the 50 ms deadline and the result
     round-trips cleanly. This is the load-bearing assertion that
     `prost` under wasmtime with a bounded linear memory (64 MiB) handles
     realistically-sized payloads.

The test asserts on the *decoded* `Profile`, not the bytes, so we're not
testing prost's determinism — we're testing that the merge logic is
correct.

### Host-imported log function

To make the proto merge (and any future real UDF) debuggable, the SDK
declares one host import:

```rust
extern "C" {
    fn mcgw_log(level: u32, ptr: u32, len: u32);
}
```

The host provides it in the import object; `level` is `0=trace .. 4=error`.
Messages are rate-limited on the host side (one log per module per second
per level) so a buggy merge can't flood stdout. The `mcgateway_sdk::log`
macros are thin wrappers; users who don't import them pay no cost.

Adding this now rather than later means the proto test can actually show
`warn!("profile decode failed for pool={pool}")` when fed a corrupt
payload, which is the only way an operator would ever diagnose a real
decode failure in production.

### Kind integration test

`TestWasmMergeDispatch`:

1. Bake a keyspace that references merge name `last-n-wins`.
2. Bake the `merge-last-n-wins.wasm` into `/etc/mcgateway/udf/` in the
   image.
3. Seed two pools with the same key at different `t` values.
4. Issue `mg k v t`, assert the response comes from the pool with the
   higher `t`.
5. Issue `__mcgw:names`, assert it lists the three built-ins + the
   WASM module.

A second test, `TestWasmMergeTrap`, bundles a deliberately-broken
module, asserts the gateway starts anyway and logs the load failure,
and asserts reads against that keyspace return `EN` (the Lua-side miss
shaping from Stage 2).

A third test, `TestWasmHotReload`, drops a `.wasm` into the UDF dir
after gateway startup via `kubectl cp`, waits for the reload (poll
`__mcgw:names` until the new name appears, <2 s), then exercises it.
This is the stage's flagship demo.

---

## Exit criteria

Stage 3b is done when all of the following hold:

1. `make check test-kind` is green with both docker and podman. Every
   Stage 1 / 2 / 3a kind test passes unchanged. The three new kind tests
   (`TestWasmMergeDispatch`, `TestWasmMergeTrap`, `TestWasmHotReload`)
   pass.
2. `rust/` builds cleanly on Alpine (the Docker build stage) and macOS /
   Linux hosts. `wasm32-wasip1` is installed by the toolchain pin; the
   Makefile checks for it and emits a clear error if missing.
3. `libmcgateway.so` embeds wasmtime; a fresh image loads and serves the
   example UDF without network access at runtime.
4. A user following the README's three-line "write a merge" recipe
   (`cargo new --lib`, drop in a `merge_fn`, `cargo build --target
   wasm32-wasip1`) ends up with a `.wasm` that loads in the kind image.
5. WASM traps, deadline kills, and ABI-version mismatches do not crash
   the gateway. Each has a unit test and a log line with enough context
   (module name, key, pool list) to debug from.
6. `mcgateway-core::Merge`, `Entry`, `MergeResult`, `Registry` were not
   modified in this stage. The proof that Stage 3a's boundary was the
   right shape is that Stage 3b adds no fields and removes no
   constraints.

Once all six hold, Stage 4 (operator + CRDs) can begin against an
operator that writes config + `.wasm` files into the same directory
shape this stage defined.

---

## Risks and open questions

- **Wasmtime pulls in cranelift at runtime.** A precompile-only build
  (Module::deserialize without Cranelift) would shrink the image by
  ~6 MB but forces us to compile modules in a separate builder image —
  more moving parts for Stage 4 and no win in v1. Stay with the
  in-process compile path; revisit when image size actually hurts.
- **Per-thread engines vs shared.** wasmtime `Engine` is `Send + Sync`
  and can be shared across threads, but compiled `Module`s cache
  Cranelift-generated code behind the engine. One `Engine` per process,
  shared across all Lua VMs on all IO threads, is the right call. Each
  VM has its own `ArcSwap<Registry>` view but they all point into the
  same engine. Verify no lock contention under the kind workload; if
  we see it, fall back to per-VM engines — the cost is duplicate compile
  cache misses at startup, which the on-disk cache absorbs.
- **`notify` on overlayfs.** inotify doesn't always fire on
  container-volume changes on some overlay drivers. The kind test uses
  `kubectl cp` into the pod's rootfs which should work, but GCS fuse
  (Stage 5) will route around inotify entirely — the loader needs a
  poll fallback then. For Stage 3b, inotify-only is fine; log a note to
  revisit in Stage 5.
- **Symbol stripping.** wasmtime + cranelift with `lto = "thin"`,
  `strip = "symbols"` in the release profile of `mcgateway-clib` is
  what keeps `libmcgateway.so` under a reasonable size. Verify in CI
  that the stripped `.so` still loads and all symbols wasmtime needs
  (pthread etc.) resolve.
- **Proc-macro on Alpine musl.** `mcgateway-sdk-macros` compiles on the
  *host* toolchain to run as a build-time proc-macro; the `.wasm`
  target doesn't matter. But the Alpine build stage doesn't need the
  proc-macro at all (it compiles the host crates, not guest
  examples). Make sure the Dockerfile's build stage doesn't
  accidentally pull in `mcgateway-sdk` — it's a dev-dep of the example
  only.
- **Silent shadowing in the reverse direction.** A disk module named
  `first-hit` is refused and logged. A disk module named `first_hit`
  (underscore) is a *different* name and loads — and a typo in a
  keyspace config could point at either. Accept this; the fix is
  config-time validation (Stage 4 concern), not load-time munging.
- **Epoch ticker thread lifetime.** The ticker must outlive the engine
  and be cleanly shut down on `luaclose`. `mlua::UserData` drop order
  in the proxy's VM is not something we should trust unconditionally;
  own the ticker handle in a process-global `OnceCell` tied to the
  first-loaded engine, accept that it lives until process exit. The
  proxy restarts on config reload anyway.
- **Guest allocator growth.** A malicious `mcgw_alloc` can return a
  pointer and then the guest can `memory.grow` unboundedly during
  `mcgw_merge`. Bound linear memory at `Module::resources_required` at
  compile time: `Config::max_memory_size(64 MiB)` per store. Overkill
  for merge use cases, cheap insurance.
- **ABI churn between v1 and v2.** The `mcgw_abi_version` handshake is
  the escape hatch. When we change the entry encoding, bump the
  constant; the host loads both versions side by side for a release,
  then drops v1 once all modules are rebuilt. Document this upgrade
  procedure in the Stage 3b README before the stage closes.
