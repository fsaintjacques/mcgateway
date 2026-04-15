# mcgateway — PRD

## Problem

Applications increasingly need to read key-value data from multiple backends
with different characteristics: immutable snapshots (frostmap), mutable caches
(memcached clusters), streaming overlays, legacy stores. Today, clients must
know which backend to query, handle merge logic themselves, and coordinate
versioning across sources. This pushes routing, consistency, and
transformation complexity into every client.

## Solution

mcgateway is a memcache-protocol-native API gateway that sits between clients
and backend pools. Clients send standard memcache meta protocol requests. The
gateway routes by key prefix, fans out reads to multiple backends, merges
results, and returns a single response. Write routing, read-modify-write, and
custom merge logic are handled transparently.

```
clients (memcache meta protocol)
    │
    ▼
┌──────────────────────────────┐
│          mcgateway           │
│                              │
│  keyspace routing            │
│  fan-out reads               │
│  merge (built-in or WASM)    │
│  write routing               │
└──────┬───────────────┬───────┘
       │               │
       ▼               ▼
   pool A           pool B
  (frostmap)      (memcached)
```

## Core Concepts

### Pool

A named backend that speaks the memcache meta protocol.

| Field | Description |
|---|---|
| `name` | Unique identifier |
| `addrs` | One or more backend addresses |
| `access` | `read`, `write`, or `read-write` |
| `hash` | Hash function for key-to-backend routing (`xxhash`, `md5`, `crc32`). Optional — omit for single-backend pools. |
| `dist` | Distribution strategy (`ring_hash`, `jump_hash`). Optional — omit for single-backend pools. |

Pools are registered rarely and are expected to be long-lived. A pool can be a
single mcfreeze instance, a memcached cluster, or any process that speaks the
memcache meta protocol. Multi-backend pools require `hash` and `dist` to
determine which backend handles each key. Single-backend pools need neither.

### Keyspace

A key prefix bound to one or more pools, with a merge strategy.

| Field | Description |
|---|---|
| `prefix` | Key prefix used for routing (split on first `:`) |
| `read` | Ordered list of pools to query on read |
| `write` | Ordered list of pools to write to |
| `write_policy` | `all` (default): succeed only when every write pool acknowledges. `first`: succeed once the first pool in the list acknowledges; remaining writes are fire-and-forget. |
| `merge` | Merge function name (built-in or registered WASM) |
| `merge_on_write` | If true, reads current state from read pools and merges with incoming write before storing. Default false. |

On a read (`mg`), the gateway fans out to all read pools in parallel, collects
results, applies the merge function, and returns the merged entry.

On a write (`ms`), the gateway fans out to all write pools. When
`merge_on_write` is true, the gateway reads from the read pools first, merges
the fetched entries with the incoming write payload, and stores the merged
result to all write pools. The `write_policy` controls when the client receives
a success response.

Keyspaces are registered when a new dataset or use case is onboarded —
infrequent.

### Merge Function

A pure function that takes an ordered list of entries and returns a single
entry.

```
merge(entries[(key, pool, status, value, metadata)]) → entry | miss
```

Each entry carries:

| Field | Description |
|---|---|
| `key` | The key this entry corresponds to |
| `pool` | The pool this entry was fetched from |
| `status` | `HIT`, `MISS`, or `ERROR`. `MISS` means the backend confirmed the key does not exist. `ERROR` means the backend did not respond (timeout, connection failure, etc.) — the value may or may not exist. |
| `value` | The value bytes (present only when status is `HIT`) |
| `metadata` | Protocol metadata: flags, TTL, opaque user token (`u` flag) (present only when status is `HIT`) |

The merge function can inspect any of these to make decisions — for example,
comparing user tokens as version timestamps, selecting values by pool, or
failing the request when a critical pool returns `ERROR`.

Entries are ordered by key first, then by pool within each key (matching the
order of the keyspace's `read` list). This ordering is deterministic and the
merge function can rely on it.

Three categories:

- **Built-in:** `last-write-wins`, `pool-preferred`, `first-hit`. Ship with the
  gateway and cover common cases.
- **WASM:** User-provided Rust functions compiled to WASM. Registered
  dynamically at runtime without redeployment.

Merge functions are the most frequently changing component — teams iterate on
merge logic as they develop their data models.

## Merge Function Registration

Merge functions are registered at runtime over the memcache protocol itself,
using a reserved `__udf:` key prefix:

```
# Register a WASM merge function
ms __udf:<name> <size>\r\n
<wasm bytes>\r\n
→ STORED\r\n

# Unregister
md __udf:<name>\r\n
→ DELETED\r\n

# List registered functions
mg __udf v\r\n
→ VA <size>\r\n
  <json: [{"name": "sum_counters", "registered_at": "..."}]>\r\n
```

The WASM module must export a `merge` function conforming to the gateway's ABI.
Modules are compiled and validated on registration; invalid modules are rejected
with `SERVER_ERROR`.

Registered functions are persisted to local disk so they survive gateway
restarts.

### WASM Merge ABI

The merge function is written in Rust, compiled to `wasm32-wasip1`:

```rust
#[no_mangle]
pub extern "C" fn merge(
    // input: serialized entries (pool name + hit/miss/tombstone + value + metadata)
    input_ptr: *const u8,
    input_len: u32,
    // output: buffer for the merged value
    output_ptr: *mut u8,
    output_cap: u32,
) -> i32;  // >0: bytes written to output, 0: miss, <0: error
```

A helper crate (`mcgateway-sdk`) will provide ergonomic wrappers so users
work with typed Rust structs rather than raw pointers.

## Read Path

### Single-key read

```
client: mg <prefix>:<key> v\r\n

gateway:
  1. match prefix → keyspace
  2. parallel fetch from all keyspace.read pools
  3. collect entries (key, pool, hit/miss/tombstone per pool)
  4. apply keyspace.merge function
  5. return merged result to client
```

### Multi-key read

Multiple keys can be fetched and reduced into a single result by joining keys
with `#`:

```
client: mg <prefix>:<k1>#<prefix>:<k2>#<prefix>:<k3> v\r\n

gateway:
  1. split key on `#`
  2. all keys must share the same prefix → same keyspace
  3. parallel fetch all keys from all keyspace.read pools
  4. collect entries ordered by key, then by pool within each key
  5. apply keyspace.merge function to the full entry list
  6. return single merged result to client
```

The merge function receives all entries across all keys and pools in a single
call. It does not need to distinguish between single-key and multi-key
requests — a single-key read is just the degenerate case with one key group.

If the key does not contain `#`, it is treated as a single-key read.

### Common

Latency is `max(pool latencies)` since fetches are parallel.

The gateway requests the `t` and `u` flags from backends even if the client
didn't ask for them — these carry version metadata that merge functions need.
Only client-requested flags are included in the response.

## Write Path

### Direct write (default)

```
client: ms <prefix>:<key> <size>\r\n<value>\r\n

gateway:
  1. match prefix → keyspace
  2. fan out write to all keyspace.write pools in parallel
  3. return result to client per write_policy
```

### Read-modify-write (`merge_on_write: true`)

```
client: ms <prefix>:<key> <size>\r\n<value>\r\n

gateway:
  1. match prefix → keyspace
  2. parallel fetch from all keyspace.read pools
  3. apply keyspace.merge function to [fetched entries..., incoming write]
  4. fan out merged result to all keyspace.write pools in parallel
  5. return result to client per write_policy
```

For non-commutative merge functions, the gateway uses memcached CAS tokens to
detect concurrent writes. On conflict, the gateway retries the
read-modify-write cycle (configurable max retries).

### Write fan-out policy

When a keyspace has multiple write pools (e.g., during a pool migration), the
`write_policy` determines when the client receives a success response:

- `all` (default): The gateway waits for all write pools to acknowledge. If any
  pool fails, the client receives an error. Safe for migrations where both pools
  must stay in sync.
- `first`: The gateway returns success as soon as the first pool in the list
  acknowledges. Remaining writes complete asynchronously. Useful when the
  secondary pool is best-effort.

## Delete Path

```
client: md <prefix>:<key>\r\n

gateway:
  1. match prefix → keyspace
  2. fan out delete to all keyspace.write pools in parallel
  3. return result to client per write_policy
```

If the write pool uses tombstones (e.g., for merge-on-read with an immutable
pool like frostmap), the gateway writes a tombstone entry. The merge function
receives tombstones as a distinct entry type and is expected to handle them
(typically: return miss).

## Implementation

The gateway is built on the memcached built-in proxy (memcached 1.6.23+):

- **Lua scripts** handle routing, fan-out, pool management, and response
  assembly.
- **libmcgateway.so** is a Rust shared library loaded via Lua FFI. It provides
  the WASM runtime (wasmtime), merge function registry, and the merge execution
  engine.
- **memcached's IO layer** manages connection pooling, pipelining, and parallel
  request dispatch to backends.

```
memcached (proxy mode)
  ├── Lua: routing, fan-out, protocol handling
  └── libmcgateway.so (FFI)
        ├── wasmtime engine
        ├── function registry: name → compiled WASM module
        └── merge(): deserialize entries, call WASM, return result
```

### Why memcached proxy

- Native memcache meta protocol support on both sides
- Proven connection pooling and pipelining
- Per-worker Lua VMs — scales with cores, no GIL
- The gateway *is* a memcached instance — can use local memory as an L1 cache
- No custom network stack to build or maintain

## Deployment

mcgateway is Kubernetes-native. Onboarding a new dataset or backend requires no
infrastructure work from client teams — they declare what they need and the
system handles the rest.

Adding a pool or keyspace is a CRD change:

```yaml
apiVersion: mcgateway.io/v1
kind: Keyspace
metadata:
  name: user-profiles
spec:
  prefix: user
  read: [frostmap, mc-cluster]
  write: [mc-cluster]
  merge: pool-preferred
```

A Kubernetes operator reconciles CRDs across the gateway fleet:

- **Pools and keyspaces** are pushed to all gateway pods automatically. Client
  teams never SSH into a gateway or manage individual instances.
- **WASM merge functions** are stored in ConfigMaps or an OCI registry. The
  operator registers them on each gateway pod via the `__udf:` protocol.
- **Pool migrations** are orchestrated as CRD updates — the operator
  coordinates dual-write phases and cutover across the fleet.
- On pod restart or scale-up, the operator reconciles: re-pushes all
  configuration and registered functions.

## Observability

The gateway is the ideal instrumentation point — all traffic passes through it.

| Metric | Description |
|---|---|
| Request latency | Per-keyspace, broken down by pool fan-out and merge time |
| Pool latency | Per-pool backend latency |
| Merge function duration | Time spent in WASM execution |
| Hit/miss rates | Per-keyspace, per-pool |
| Merge function errors | WASM traps, timeouts |
| Active connections | Client-side and per-pool backend-side |
| Keyspace/function registry | Gauge of registered keyspaces and functions |

## Built-in Merge Functions

| Name | Behavior |
|---|---|
| `last-write-wins` | Compare `u` tokens (or `t` flag) as timestamps; highest wins |
| `pool-preferred` | Return first hit in pool order as listed in keyspace config |
| `first-hit` | Return first hit regardless of pool; for fallback chains |

## Future Work

Explicitly deferred from v1:

- **Per-pool key transformation** — strip, keep, or rewrite the prefix before
  querying each backend. For now, keys are forwarded verbatim.
- **Post-merge transforms** — map/filter on the merged result before returning
  to the client (projection, encoding, field stripping).
- **Write-side transforms** — validation, schema enforcement, automatic version
  stamping on writes.
- **Client identity and access control** — per-client keyspace visibility and
  rate limiting.
- **Expression-based filters** — inline predicates for conditional routing
  without WASM.
