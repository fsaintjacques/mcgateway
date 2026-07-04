# mcgateway

A memcache-protocol-native gateway that merges reads across multiple key-value
backends. Clients speak the standard memcache [meta
protocol](https://github.com/memcached/memcached/blob/master/doc/protocol.txt);
the gateway routes each request by key prefix to a *keyspace*, fans reads out
to that keyspace's backend pools in parallel, applies a merge function to the
per-pool results, and returns a single response. Writes fan out to the
keyspace's write pools under a configurable policy.

The point is separation of routing from compute: clients need no routing
tables, no client-side merge logic, no migration awareness — any process that
already speaks memcache works unchanged. Merge logic is extensible at runtime
via WASM modules written in Rust against a small SDK, so teams iterate on
merge behaviour without redeploying the gateway.

```
clients (memcache meta protocol)
    │
    ▼
┌──────────────────────────────────────────┐
│  memcached (proxy mode)                  │
│    ├── lua/mcgateway      routing,       │
│    │                      fan-out,       │
│    │                      write policy   │
│    └── mcgateway_native.so (Lua C module)│
│          ├── built-in merges             │
│          └── wasmtime: WASM merge UDFs   │
└──────┬──────────────────┬────────────────┘
       │                  │
       ▼                  ▼
    pool A             pool B
 (e.g. snapshot)   (e.g. memcached)
```

The gateway is a stock `memcached` binary run in proxy mode — no custom
network stack. Two artifacts are loaded into it:

- **`lua/mcgateway`** — a Lua library implementing prefix routing, keyspace
  resolution, parallel fan-out, and write policies on top of memcached's
  proxy API (`mcp.router_new`, funcgens).
- **`mcgateway_native.so`** — a Rust cdylib loaded as a Lua module. It hosts
  the merge registry: native built-in merges plus a wasmtime-backed runtime
  for user-provided WASM merge functions.

## Status

The data plane is functional end-to-end; the Kubernetes control plane is not
built yet. See [doc/plans/implementation-plan.md](doc/plans/implementation-plan.md).

| Capability | State |
|---|---|
| Prefix routing, single- and multi-pool keyspaces | ✅ |
| Parallel read fan-out + merge (`mg`) | ✅ |
| Write/delete fan-out with `write_policy: all \| first` (`ms`, `md`) | ✅ |
| Built-in merges: `first-hit`, `pool-preferred`, `last-write-wins` | ✅ |
| WASM merge UDFs (Rust SDK, loaded from disk at startup) | ✅ |
| Kubernetes operator + `Pool`/`Keyspace` CRDs | ⏳ planned (Stage 4+) |
| Runtime UDF registration over the protocol (`__udf:` prefix) | ⏳ planned — currently returns `SERVER_ERROR` |
| Multi-key reduce (`mg k1#k2#k3`) | ⏳ planned — currently returns `SERVER_ERROR` |
| Read-modify-write (`merge_on_write`) | ⏳ planned |
| Prometheus metrics, `mcgwctl` CLI | ⏳ planned (Stages 6–7) |

## How it works

### Keyspaces and pools

Configuration is a Lua file (default `/etc/mcgateway/config.lua`) declaring
pools and keyspaces:

```lua
return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
        { name = "mc-b", addrs = { "mc-b:11211" } },
    },
    keyspaces = {
        -- Single-pool passthrough.
        { prefix = "user", read = "mc-a", write = "mc-a" },

        -- Fan-out read with fallback; writes mirrored to both pools.
        {
            prefix       = "session",
            read         = { "mc-a", "mc-b" },
            write        = { "mc-a", "mc-b" },
            write_policy = "all",
            merge        = "pool-preferred",
        },

        -- Migration: read old + new, pick freshest; write new-primary,
        -- shadow old (fire-and-forget).
        {
            prefix       = "cache",
            read         = { "mc-b", "mc-c" },
            write        = { "mc-c", "mc-b" },
            write_policy = "first",
            merge        = "last-write-wins",
        },
    },
}
```

Keys route on the prefix before the first `:` — `mg session:abc v` matches the
`session` keyspace. Unknown prefixes get `SERVER_ERROR unknown keyspace`.

### Read path

On `mg`, the gateway fans out to every pool in the keyspace's `read` list in
parallel (latency is `max` of pool latencies, not the sum), classifies each
response as `hit`, `miss`, or `error` (timeout / connection failure /
`SERVER_ERROR`), and hands the ordered entry list to the merge function. The
merge returns either a *winner* (one pool's response is forwarded verbatim), a
*synthesized* value (fresh bytes, e.g. a re-encoded protobuf), or a *miss*.
Merges declare which meta flags they need (e.g. `t` for TTL) and the gateway
adds them to outgoing backend requests automatically.

### Write path

On `ms`/`md`, the gateway fans out to every pool in `write`:

- `write_policy = "all"` (default) — wait for every pool; the client sees the
  "strongest negative" response (any error beats not-stored beats stored).
- `write_policy = "first"` — return as soon as the first-listed pool
  acknowledges; remaining writes are fire-and-forget.

### Built-in merge functions

| Name | Behaviour |
|---|---|
| `first-hit` | First entry with a hit wins. |
| `pool-preferred` | Same as `first-hit`; entries arrive in the keyspace's `read` order, so this reads as "prefer earlier pools". |
| `last-write-wins` | Hit with the greatest remaining TTL (`t` flag) wins. Correct as LWW only when all pools write with the same initial TTL — see the caveat in [`builtins.rs`](rust/crates/mcgateway-core/src/builtins.rs). |

## Writing a WASM merge function

Custom merges are Rust `cdylib` crates built against
[`mcgateway-sdk`](rust/crates/mcgateway-sdk) and compiled to `wasm32-wasip1`:

```rust
use mcgateway_sdk::{merge_fn, Entry, MergeResult, Status};

#[merge_fn(required_flags = "t")]
pub fn last_n_wins(entries: &[Entry<'_>]) -> MergeResult {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == Status::Hit)
        .filter_map(|(i, e)| e.t.map(|t| (i, t)))
        .max_by_key(|(_, t)| *t)
        .map_or(MergeResult::Miss, |(i, _)| MergeResult::Winner(i))
}
```

The `#[merge_fn]` macro generates all ABI exports; the SDK is `no_std` and the
crate needs nothing else. Build and deploy:

```sh
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1 -p my-merge
# drop the .wasm into the gateway's UDF directory (default /etc/mcgateway/udf,
# override with MCGW_UDF_DIR); it is picked up at gateway startup, registered
# under its file stem, and referenced from a keyspace as merge = "my_merge".
```

Each entry exposes `key`, `pool`, `status`, the parsed `t` flag, the value
bytes, and the raw meta response line for merges that need other flags. A
merge returns `Winner(index)`, `Synthesized(bytes)`, or `Miss`. Guest code can
log via `mcgateway_sdk::{trace!, debug!, info!, warn!, error!}` (rate-limited
by the host).

Execution is sandboxed by wasmtime: each call runs in a fresh store with a
50 ms deadline (epoch interruption), a 16 MiB linear-memory cap, and a
pooling allocator bounding process-wide concurrency. A trapping, looping, or
memory-hungry merge degrades that request to a miss; it cannot corrupt later
calls or take down the gateway. Modules must match the host's ABI version
(currently 1) or they are refused at load.

Three example UDFs live in [`rust/examples/`](rust/examples):
`merge-last-n-wins`, `merge-concat-values`, and `merge-profile-proto` (a
field-level protobuf merge, demonstrating `Synthesized`).

## Getting started

### Prerequisites

- Rust 1.80+ (plus the `wasm32-wasip1` target for building UDFs)
- Go 1.25+ (kind e2e tests only)
- Docker or Podman, [kind](https://kind.sigs.k8s.io/), and Helm for the
  end-to-end environment
- Lua 5.4 (optional, for host-side Lua smoke tests)

### Build and unit-test

```sh
make build   # cargo build --release + go build
make check   # cargo test + clippy, Lua smoke tests, go vet
```

### Run the full stack in kind

```sh
make kind-up      # create the kind cluster
make test-kind    # build the gateway image, load it, helm-install, run e2e tests
make kind-down    # tear down
```

This deploys the gateway plus two dev memcached backends
([`k8s/charts/mcgateway`](k8s/charts/mcgateway)) and runs the Go e2e suite in
[`go/internal/kind`](go/internal/kind) against real traffic — passthrough,
fan-out, write policies, native and WASM merges.

### Poke at it manually

The gateway image ([`docker/Dockerfile`](docker/Dockerfile)) is memcached
1.6.41 built with `--enable-proxy`, plus the Lua library, the native module,
and the example UDFs baked in. Any memcache client works:

```
mg session:foo v          # fan-out read, merged response
ms session:foo 3\r\nbar   # fan-out write
mg __mcgw:names v         # diagnostic: list registered merge names
```

## Repository layout

```
lua/
  mcgateway.lua            library entry point (config → pools → routes)
  mcgateway/               routing, fan-out, entries, write policy
  examples/proxy.lua       memcached proxy entry point (mcp_config_* hooks)
  examples/config.lua      example pools/keyspaces config
rust/crates/
  mcgateway-core/          Entry/Merge/Registry types + built-in merges
  mcgateway-clib/          mcgateway_native.so — Lua C module (mlua), UDF loader
  mcgateway-wasm-host/     wasmtime host: ABI codec, deadlines, sandboxing
  mcgateway-sdk/           guest-side SDK for writing WASM merges (no_std)
  mcgateway-sdk-macros/    the #[merge_fn] proc macro
rust/examples/             example WASM merge UDFs
go/internal/kind/          end-to-end tests against a kind cluster
k8s/charts/mcgateway/      Helm chart (gateway + dev backends)
docker/Dockerfile          gateway image (memcached + .so + lua + UDFs)
doc/                       PRD, high-level architecture, staged implementation plan
```

## Documentation

- [doc/memcache-gateway-prd.md](doc/memcache-gateway-prd.md) — product
  requirements: concepts, protocol semantics, merge model.
- [doc/high-level.md](doc/high-level.md) — target architecture, including the
  planned Kubernetes operator and CRD-driven configuration.
- [doc/plans/](doc/plans) — the staged implementation plan and per-stage
  design notes.
