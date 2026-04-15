# Stage 3a — Rust-native merges behind a C-pointer boundary

Third deliverable from the [implementation plan](implementation-plan.md), split
from the original Stage 3 scope: introduce the Lua ↔ Rust boundary and move the
Stage 2 built-in merges into a Rust shared library, **without** WASM,
wasmtime, an SDK crate, or any on-disk wire format. Stage 3b picks up WASM
once this boundary has lived in production.

The original Stage 3 bundled "FFI + WASM" on the assumption that the FFI
boundary only existed to sandbox user code. We've since decided the boundary is
worth paying for on its own: it lets the gateway ship native Rust logic
(merges today, filters / transforms later) that Lua can't express cleanly, and
it makes the WASM host a late addition against a boundary already shaped by
real traffic.

---

## Goal

Replace `lua/mcgateway/merges.lua`'s three built-ins with a Rust
implementation loaded as a shared object. Observable behaviour is unchanged;
the kind suite and per-merge Lua unit tests from Stage 2 still pass
byte-for-byte against the new dispatch path.

Concretely:

1. A new `rust/` tree produces `libmcgateway.so`, a **Lua C module** loadable
   via `require("mcgateway_native")` inside the memcached proxy's embedded
   Lua 5.4.
2. The three built-ins (`first-hit`, `pool-preferred`, `last-write-wins`)
   are implemented in Rust against a `Merge` trait and registered at
   module-init time.
3. `routes.lua` calls `mcgateway_native.merge(name, entries)` where
   `entries` is the same Lua table shape Stage 2 already builds. The Rust
   side reads key, pool, status, `t`, and response value directly off the
   Lua stack — no `serde`, no bincode, no buffer format.
4. `merges.lua` is deleted. Name resolution moves to Rust.

This stage exists to prove:

- The Lua ↔ Rust boundary (Lua C API, C pointers, borrows pinned for one
  call) works under the kind workload without per-request allocation
  regressions.
- A `Merge` trait and a registry exist in Rust and can dispatch uniformly
  to either a native implementation or, in Stage 3b, a WASM-backed one.
- The build, packaging, and image shape absorb a Rust cdylib cleanly.

---

## Non-goals

- **WASM, wasmtime, the `mcgateway-sdk` crate, `#[merge_fn]` proc macro,
  user-defined merges, UDF directory, inotify, hot-reload, AOT cache.**
  All deferred to Stage 3b. The trait signature chosen here must *admit* a
  WASM implementation cleanly; it does not need to host one.
- A generic C ABI consumable from non-Lua callers. `libmcgateway.so` is a
  Lua C module in this stage; the externally callable surface is the Lua
  C API. A pure-C ABI for non-Lua consumers is not a requirement until
  something wants it.
- A stable on-disk wire format for entries. None exists; none is needed
  while Lua is the only caller and borrows from the Lua stack are the
  transport.
- Serialization, zero-copy shared-memory tricks, pointer stability across
  calls. Entry data is borrowed from Lua-owned memory for the duration of
  a single merge call and invalidated on return.
- Kubernetes operator, CRDs, metrics beyond what Stage 2 exposes — still
  owned by Stages 4 and 6.
- Changing Stage 2 kind behaviour. Every Stage 2 kind test must pass
  unchanged.

---

## Scope

| Feature | In scope |
|---|---|
| Cargo workspace under `rust/` | ✓ |
| `mcgateway-core` crate: `Entry`, `Status`, `Merge` trait, `Registry` | ✓ |
| `mcgateway-merge-builtins` crate: the three Stage 2 merges in Rust | ✓ |
| `mcgateway-clib` crate: `cdylib` exporting `luaopen_mcgateway_native` | ✓ |
| Rust-side Lua stack walking to project entries without serialization | ✓ |
| `routes.lua` switched to `mcgateway_native.merge(name, entries)` | ✓ |
| Deletion of `lua/mcgateway/merges.lua` and its Lua unit tests, replaced by Rust unit tests | ✓ |
| Docker image carries `libmcgateway.so` next to the Lua library | ✓ |
| Stage 2 kind suite passes unchanged | ✓ |
| WASM backend, SDK, hot-reload | — (Stage 3b) |
| Synthesized-bytes merge results | ✓ (shape only; no built-in uses it) |
| `__udf:` registration | — (still `SERVER_ERROR udf not supported`) |

---

## Project layout

```
rust/
  Cargo.toml                     (workspace)
  crates/
    mcgateway-core/              lib: Entry, Status, Merge trait, Registry
    mcgateway-merge-builtins/    lib: first_hit, pool_preferred, last_write_wins
    mcgateway-clib/              cdylib: luaopen_mcgateway_native + dispatch glue
lua/mcgateway/
  routes.lua                     (edited) call mcgateway_native.merge(...)
  merges.lua                     DELETED
  entries.lua                    (unchanged)
  keyspaces.lua                  (edited) merge-name validation via mcgateway_native.has_merge
  config.lua                     (unchanged)
  pools.lua                      (unchanged)
  util.lua                       (unchanged)
lua/tests/
  test_merges.lua                DELETED (coverage moves to Rust)
  test_entries.lua               (unchanged)
  test_routes.lua                (edited) stubs out mcgateway_native
docker/Dockerfile                (edited) add Rust build stage; ship .so
Makefile                         (edited) cargo build/test targets wired into check/test
```

The Rust workspace is self-contained; `go/` is untouched in this stage.

---

## Rust API shape

### `mcgateway-core`

```rust
pub enum Status { Hit, Miss, Error }

pub struct Entry<'a> {
    pub key:    &'a [u8],
    pub pool:   &'a str,
    pub status: Status,
    pub t:      Option<i64>,   // parsed meta-`t` flag; None if absent
    pub value:  Option<&'a [u8]>, // response value body; None unless Hit
    pub line:   &'a [u8],      // full meta response line for flag parsing
}

pub enum MergeResult<'a> {
    Winner(usize),             // index into the Entry slice
    Synthesized(Vec<u8>),      // future-use; no built-in returns this
    Miss,
}

pub trait Merge: Send + Sync {
    fn apply(&self, entries: &[Entry<'_>]) -> MergeResult<'_>;
    /// Single-character meta flags this merge needs returned on reads.
    fn required_flags(&self) -> &'static str { "" }
}

pub struct Registry { /* name -> Arc<dyn Merge> */ }
impl Registry {
    pub fn new() -> Self;
    pub fn insert(&mut self, name: &'static str, m: Arc<dyn Merge>);
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Merge>>;
}
```

Lifetimes in `Entry` are tied to the Lua call frame. No `Entry` escapes
the merge call. `MergeResult::Synthesized` exists so the shape doesn't
shift when Stage 3b adds WASM merges that build new bytes — built-ins
ignore it.

### `mcgateway-merge-builtins`

One `struct` per merge implementing `Merge`. A `register(reg: &mut
Registry)` entry point the module crate calls at `luaopen_*`. Behaviour is
a line-for-line port of Stage 2's Lua; unit tests in this crate cover the
all-hit / all-miss / mixed / all-error / tie-break matrix.

### `mcgateway-clib`

A `cdylib` built with `mlua` (features = `["lua54", "module"]`) or raw
`lua-sys` bindings — `mlua` is strongly preferred because it handles the
stack mechanics safely without forcing us to audit hand-written `unsafe`
for every field access. Exports:

- `luaopen_mcgateway_native(L)` — builds a `Registry` with built-ins
  inserted, returns a Lua table exposing:
  - `merge(name, entries) -> (res_index | value_bytes | nil)` — runs the
    named merge. Returns the 1-based Lua index of the winner on
    `Winner`, a Lua string on `Synthesized`, `nil` on `Miss`. Raises a
    Lua error for unknown merge names (startup-time catch; unknown names
    are rejected at config load via `has_merge`).
  - `has_merge(name) -> bool` — for `keyspaces.lua` validation.
  - `required_flags(name) -> string` — replaces
    `merges.required_flags` in Lua.
  - `names() -> table` — replaces `merges.names`; used by diagnostics.

The module *does not* take ownership of the entry table. It walks the
table at call time, pulls out `key` / `pool` / `status` / `t` and the
`res` userdata, and builds an `Entry<'_>` array on the Rust stack. For
hit entries, it calls `res:line()` (already a Lua string) and, when
needed, `res:value()` to get ptr+len borrows — these allocate Lua strings
but do not copy beyond what Stage 2 already paid for.

Returning `Winner(idx)` as a 1-based index lets Lua do
`entries[idx].res` and forward the existing response userdata untouched;
this preserves the Stage 2 behaviour of returning the original pool's
response verbatim, flags and all.

---

## Lua integration

`routes.lua` changes at two points:

1. Keyspace build (`keyspaces.lua`): `merge_fn = merges.lookup(name)`
   becomes a string reference `merge_name = name` plus an upfront
   `mcgateway_native.has_merge(name)` check at config load. The funcgen
   closure captures `merge_name`, not a function.
2. Read handler: `ks.merge_fn(entries)` becomes

   ```lua
   local idx_or_bytes = mcgateway_native.merge(merge_name, entries)
   if type(idx_or_bytes) == "number" then
       return entries[idx_or_bytes].res
   elseif type(idx_or_bytes) == "string" then
       return idx_or_bytes   -- synthesized bytes (unused by builtins)
   else
       -- miss: fall through to Stage 2's miss / error logic unchanged
   end
   ```

   The miss and all-error fallthrough (return a pool's own `EN` /
   `ERROR` response when available, else `EN\r\n`) stays in Lua — the
   Rust merge only decides the winner; shaping the client response
   is still Lua's job.

`entries.lua` is unchanged. Entries remain Lua tables holding the
`mcp.response` userdata; Rust reads directly off them.

---

## Build and packaging

### Cargo

`rust/Cargo.toml` declares a workspace. `mcgateway-clib` is the only
`cdylib`; the others are `rlib`. Release build with `lto = "thin"` and
`codegen-units = 1` for the cdylib; debug build used by tests.

### Docker

A new build stage in `docker/Dockerfile` compiles the cdylib against the
same Alpine toolchain and copies `libmcgateway.so` into
`/etc/mcgateway/lua/`. The memcached embedded Lua's `package.cpath` is
extended (or the `.so` lives next to `mcgateway.lua` where Lua's default
cpath finds it) so `require("mcgateway_native")` resolves without
proxy-config changes.

Image size delta is roughly the size of the cdylib plus `libgcc_s`
(already present via libevent) — expect well under 2 MB added in
release. Track it; if it balloons, revisit `lto`/`opt-level`.

### Makefile

- `build` now also runs `cargo build --release` in `rust/`.
- `check` gains `cargo test` and `cargo clippy -- -D warnings`.
- `docker-build` is unchanged at the invocation level; the Dockerfile
  handles the Rust stage.
- `test-kind` is unchanged; the built image contains the new `.so`.

Lua `make check` loses `test_merges.lua` (deleted); coverage now lives in
Rust unit tests.

---

## Testing

### Rust unit tests

In `mcgateway-merge-builtins`, a table-driven suite covering:

- `first-hit`: all-hit returns first; first-miss-then-hit returns the hit;
  all-miss returns `Miss`; all-error returns `Miss`.
- `pool-preferred`: identical matrix to `first-hit` given the ordering
  contract.
- `last-write-wins`: higher `t` wins; `t == None` on any hit never beats
  a hit with `t`; equal `t` returns the earlier index (stable); all-miss
  returns `Miss`.

No `mcp.response` stubbing is needed — tests build `Entry<'_>` values
directly.

In `mcgateway-clib`, a thin smoke test using `mlua` in
`features = ["lua54", "vendored"]` boots a standalone Lua state, loads
the module, and runs `merge("first-hit", {...})` end-to-end against a
fake entry table. This pins the Lua-stack walker without needing
memcached.

### Lua unit tests

`test_merges.lua` is removed. `test_entries.lua` stays. `test_routes.lua`
swaps the old `merges.lookup` stub for a fake `mcgateway_native` table
with a controllable `merge` function.

### Kind integration tests

Stage 2's kind suite runs unchanged. The passing bar: byte-identical
responses for all existing cases. Add one new case — `TestNativeMergeDispatch`
— that confirms the image actually loaded `libmcgateway.so` by reading
`mcgateway_native.names()` via a diagnostic path (see below) and
asserting it lists the three built-ins. This catches the silent-fallback
failure mode where a missing `.so` would otherwise let Lua continue
without native merges (we'll make that a hard error instead — see
risks).

### Diagnostic path

`routes.lua` exposes a single new prefix handler, `__mcgw:names`, that
returns the list of registered merges as a newline-delimited string over
the memcache protocol. Cheap to add, gives the kind test a way to
observe registry contents without a sidecar. This is *not* `__udf:` —
still rejected — and is intentionally minimal; real diagnostics land
with the CLI in Stage 7.

---

## Exit criteria

Stage 3a is done when all of the following hold:

1. `make check test-kind` is green on a fresh checkout with both docker
   and podman providers. Every Stage 1 and Stage 2 kind test passes
   unchanged.
2. `rust/` builds cleanly on Alpine (the Docker build stage) and on the
   developer's host (macOS / Linux) with a stable Rust toolchain pinned
   via `rust-toolchain.toml`.
3. `libmcgateway.so` is present in the production image and loaded by
   `routes.lua` at proxy config time. A missing or unloadable `.so` is a
   hard startup failure, not a silent Lua-fallback.
4. `lua/mcgateway/merges.lua` no longer exists. All merge behaviour
   flows through `mcgateway_native.merge`.
5. Rust unit tests cover the three built-ins' behaviour matrix; Clippy
   is clean at `-D warnings`.
6. The `Merge` trait, `Entry` struct, `MergeResult` enum, and `Registry`
   shape are documented in `rust/crates/mcgateway-core/src/lib.rs` with
   rustdoc so Stage 3b's WASM host can target the exact surface without
   re-negotiating it.

Once all six hold, Stage 3b (wasmtime host + SDK crate + hot-reload) can
begin against the boundary this stage lays down.

---

## Risks and open questions

- **`mcp.response:value()` allocation.** Fetching the value body through
  the proxy's Lua API allocates a Lua string. That's one allocation per
  hit per merge that inspects the body. Stage 2 already paid this for
  any merge that touched `res:value()`; built-ins don't, so today it's
  free. A future synthesis-merge will pay it. Not a blocker; flag for
  profiling once WASM lands and real merges start touching bodies. A
  zero-copy accessor in the proxy is the right long-term fix but needs
  an upstream change to memcached.
- **`mlua` vs raw `lua-sys`.** `mlua` with `features = ["lua54",
  "module"]` gives us safe stack handling and reasonable error
  propagation at the cost of a dependency and some binary size. Raw
  `lua-sys` is leaner but every field access becomes hand-audited
  `unsafe`. Decision: start with `mlua`; revisit only if the cdylib
  exceeds ~2 MB or the module shows up in profiles, neither of which is
  likely for merge dispatch.
- **ABI symbol compatibility.** Alpine uses musl; the Docker build stage
  must compile against musl so the `.so` loads under the same libc the
  memcached binary uses. `cargo build --target x86_64-unknown-linux-musl`
  in the build stage; verify with `ldd` in a smoke step.
- **Silent fallback.** If `require("mcgateway_native")` fails, we must
  not let Lua fall back to the old (deleted) `merges.lua`. The load is
  `require(...)` without `pcall` and any failure aborts proxy init —
  matches the Stage 2 posture on unknown merge names.
- **Thread model.** The memcached proxy runs Lua VMs per IO thread. Each
  VM will `require("mcgateway_native")` independently, which means one
  `Registry` per thread — fine given merges are stateless. If we later
  add mutable state (e.g. WASM module cache in Stage 3b), it must live
  behind `Arc<Mutex<_>>` or equivalent; noting it here so the trait
  surface doesn't accidentally bake in `&mut self`.
- **Ordering contract.** Still hinges on Lua building the entry array in
  `read` order. Unchanged from Stage 2; reinforce in the `Entry`
  rustdoc so Stage 3b doesn't re-derive it wrong.
- **Kind image size.** Alpine + Rust cross-compile in a single
  Dockerfile can bloat the build-stage cache. Use a dedicated `FROM
  rust:alpine AS rust-build` stage and copy only the `.so` into the
  runtime stage; keeps the final image slim.
