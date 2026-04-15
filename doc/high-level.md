# High-Level Architecture

## Product Goal

The system's success is measured by a single outcome: **a team can expose a
key-value dataset composed of multiple backends — immutable snapshots, mutable
caches, overlays — behind a single memcache endpoint, with custom merge logic,
without writing any routing code in their clients.**

Concretely: declare a set of backend pools, declare a keyspace that binds a
key prefix to those pools with a merge strategy, and within minutes clients can
issue standard memcache `mg`/`ms`/`md` requests against the gateway and receive
merged results. No client library to ship, no routing table to distribute, no
consistency protocol to implement in every application.

This is deliberately a separation of routing from compute. Clients speak the
standard memcache meta protocol and know nothing about backend topology. The
gateway owns prefix routing, fan-out, merge, and write policy. The operator
owns fleet-wide configuration. Backends own data.

The simplicity comes from the protocol choice: clients that already speak
memcache need no changes. The extensibility comes from WASM: teams iterate on
merge logic as pure Rust functions without redeploying the gateway.

---

## Problem Statement

Applications increasingly read key-value data from multiple backends with
different characteristics:

- **Immutable snapshots** — versioned datasets served read-only at high fan-out
  (e.g. mcfreeze).
- **Mutable caches** — memcached clusters with hash-based routing.
- **Streaming overlays** — recent updates layered on top of a base snapshot.
- **Legacy stores** — systems being migrated in or out of the serving path.

Without a gateway, every client must know which backend to query, how to
merge results, how to coordinate versions, and how to handle backend
migrations. This pushes routing, consistency, and transformation complexity
into every client library in every language.

The gateway centralises this into a single network hop with a single protocol.

---

## Solution Overview

The gateway sits between clients and backend pools. Clients send standard
memcache meta protocol requests. The gateway routes by key prefix, fans out
reads to multiple backends, merges results, handles write routing and policy,
and returns a single response.

```
                    ┌────────────────────────────┐
                    │         Operator           │  Go · Deployment
                    │  - Pool / Keyspace CRDs    │
                    │  - MergeFunction CRDs      │
                    │  - fleet reconciliation    │
                    └──────────────┬─────────────┘
                                   │ memcache protocol
                                   │ (config reload, __udf: push)
              ┌────────────────────┴───────────────────┐
              │                                        │
              ▼                                        ▼
   ┌────────────────────────────┐      ┌────────────────────────────┐
   │     Gateway Pod (N)        │      │     Gateway Pod (N)        │
   │                            │      │                            │
   │  ┌──────────────────────┐  │      │  ┌──────────────────────┐  │
   │  │  memcached (proxy)   │  │      │  │  memcached (proxy)   │  │
   │  │                      │  │      │  │                      │  │
   │  │  ┌────────────────┐  │  │      │  │  ┌────────────────┐  │  │
   │  │  │  proxy.lua     │  │  │      │  │  │  proxy.lua     │  │  │
   │  │  │  + mcgateway   │  │  │      │  │  │  + mcgateway   │  │  │
   │  │  │    (lua lib)   │  │  │      │  │  │    (lua lib)   │  │  │
   │  │  └───────┬────────┘  │  │      │  │  └───────┬────────┘  │  │
   │  │          │ FFI       │  │      │  │          │ FFI       │  │
   │  │  ┌───────▼────────┐  │  │      │  │  ┌───────▼────────┐  │  │
   │  │  │ libmcgateway.so│  │  │      │  │  │ libmcgateway.so│  │  │
   │  │  │  - wasmtime    │  │  │      │  │  │  - wasmtime    │  │  │
   │  │  │  - merge reg.  │  │  │      │  │  │  - merge reg.  │  │  │
   │  │  │  - built-ins   │  │  │      │  │  │  - built-ins   │  │  │
   │  │  └────────────────┘  │  │      │  │  └────────────────┘  │  │
   │  └──────────┬───────────┘  │      │  └──────────┬───────────┘  │
   └─────────────┼──────────────┘      └─────────────┼──────────────┘
                 │                                   │
                 └───────────────┬───────────────────┘
                                 │
             ┌───────────────────┼───────────────────┐
             ▼                   ▼                   ▼
     ┌──────────────┐    ┌──────────────┐    ┌──────────────┐
     │   Pool A     │    │   Pool B     │    │   Pool C     │
     │  (frostmap)  │    │  (memcached) │    │   (other)    │
     └──────────────┘    └──────────────┘    └──────────────┘

                         clients (memcache meta protocol)
                                       │
                                       ▼ (any gateway pod)
```

---

## Components

### Operator

**Language:** Go
**Deployment:** Kubernetes Deployment (single replica, leader-elected for HA)
**State:** Kubernetes CRDs (`Pool`, `Keyspace`, `MergeFunction`)

The operator is the authoritative source of truth for gateway configuration.
It reconciles CRDs into per-gateway-pod state. It never serves client traffic
and never touches backend data.

Responsibilities:
- Maintain a registry of pools and keyspaces via CRDs.
- Resolve backend endpoints for each pool (from K8s Services, Endpoints, or
  static config) and push the resolved pool definitions to every gateway pod.
- Store WASM merge function artifacts (in ConfigMaps or OCI registry) and
  register them on every gateway pod using the reserved `__udf:` prefix over
  the memcache protocol.
- On pod restart or scale-up, reconcile: re-push all pool and keyspace
  configuration, re-register all merge functions.
- Orchestrate pool migrations: update keyspace `write` lists across the fleet
  in phases (dual-write → shift reads → cut over).

See [kubernetes.md](kubernetes.md) for CRD schemas and reconciliation loops.

### Gateway

**Language:** Lua + Rust (memcached is a runtime dependency, not built)
**Deployment:** Kubernetes Deployment, unprivileged container
**Communicates with:** Clients (TCP), backend pools (TCP), operator (memcache
protocol)

One process per pod. The latency-critical serving path. Stateless from the
operator's perspective — all configuration arrives via memcache commands at
startup and reload.

The gateway is a stock `memcached` binary (1.6.23+) running in proxy mode,
loaded with two things:

- **`mcgateway.lua`** — the Lua library that implements prefix routing,
  keyspace resolution, fan-out, multi-key `#` splitting, write policy, and
  `__udf:` registration handlers. Wraps memcached's standard proxy API
  (`mcp.pool`, `mcp.await_logreqs`).
- **`libmcgateway.so`** — a Rust shared library loaded via Lua FFI. Hosts the
  wasmtime engine, the merge function registry, built-in merges
  (`last-write-wins`, `pool-preferred`, `first-hit`), and the merge execution
  path.

Responsibilities:
- Serve clients over the memcache meta protocol.
- Route each request by key prefix to a keyspace; fan out to the keyspace's
  read pools; call the merge function via FFI; return the merged response.
- For writes, fan out to the keyspace's write pools per `write_policy` (`all`
  or `first`).
- For `ms __udf:<name>`, register the WASM module with `libmcgateway` and
  persist it to local disk for restart survival.
- Treat pool errors (timeout, connection failure) as `ERROR`-status entries
  in the merge input — merge functions decide whether to degrade or fail.

See [gateway.md](gateway.md) for the Lua library internals and FFI ABI.

### SDK

**Language:** Rust
**Deployment:** Cargo crate published for user consumption
**Shared library:** `mcgateway-abi` (entry wire format)

User-facing crate for writing custom WASM merge functions. Users depend on
`mcgateway-sdk`, write typed Rust, compile to `wasm32-wasip1`, submit the
`.wasm` artifact to the operator (via `MergeFunction` CRD).

Responsibilities:
- Provide typed structs for `Entry { key, pool, status, value, metadata }`.
- Provide a `#[merge_fn]` proc macro that generates the `extern "C" fn merge`
  boilerplate conforming to the gateway's ABI.
- Provide serde for the ABI wire format, shared with `libmcgateway` through
  `mcgateway-abi`.

See [merge-abi.md](merge-abi.md) for the ABI specification.

### CLI (`mcgwctl`)

**Language:** Rust
**Deployment:** Standalone binary, optional

Debug and operator tool. Talks to a single gateway pod for diagnostics outside
the operator's reconciliation loop.

Responsibilities:
- Inspect registered keyspaces, pools, and merge functions on a gateway.
- Trigger a test merge with synthetic entries.
- Dump per-keyspace latency and merge-duration stats.
- Load-test a gateway with synthetic traffic.

---

## Kubernetes Resources

Three custom resources define the declarative interface:

| CRD | Scope | Owner | Purpose |
|---|---|---|---|
| `Pool` | Namespaced | User | Named backend: addresses, access mode, hash/dist for multi-backend pools |
| `Keyspace` | Namespaced | User | Prefix → pools mapping: read list, write list, write_policy, merge function |
| `MergeFunction` | Namespaced | User | Named WASM merge function: artifact reference (ConfigMap/OCI), ABI version |

See [kubernetes.md](kubernetes.md) for CRD schemas, ownership, and garbage
collection.

---

## Shared Code

### `rust/crates/mcgateway-abi`

Rust library crate that defines the ABI between the gateway host and WASM
merge functions: the `Entry` type, the framed binary layout passed across
the FFI boundary, and the ABI version. Compiled into both `libmcgateway`
(host side) and `mcgateway-sdk` (guest side, inside WASM), ensuring both
sides agree on the wire format.

| Module | Description |
|---|---|
| `entry` | `Entry { key, pool, status, value, metadata }` and status enum |
| `wire` | Serialization: framed binary format for N entries |
| `abi` | ABI version constants shared by host and SDK |

### `go/api`

Go module imported by the operator and any future fleet-management tooling.
Contains the CRD types.

| Type | Description |
|---|---|
| `PoolSpec` | Name, addrs, access, hash, dist |
| `KeyspaceSpec` | Prefix, read, write, write_policy, merge |
| `MergeFunctionSpec` | Name, artifact reference, ABI version |
| `PoolStatus` / `KeyspaceStatus` | Per-CRD rollout state across the fleet |

---

## Data Flow

### Configuration

```
1. User applies a Pool CR and a Keyspace CR (and optionally a MergeFunction CR).

2. Operator reconciles:
   a. Resolves Pool.spec.addrs (static or from K8s Service/Endpoints).
   b. For each gateway pod, sends a config reload to memcached's admin socket
      with the new pool/keyspace tables.
   c. For MergeFunction CRs, fetches the WASM artifact and sends
      `ms __udf:<name> <size>\r\n<bytes>\r\n` to every gateway pod.
   d. Updates CRD status with per-pod rollout state.

3. Gateway pods on reload:
   a. Re-execute proxy.lua lifecycle hooks (mcp_config_pools,
      mcp_config_routes).
   b. Rebuild mcp.pool() objects with new backend lists.
   c. Swap routing tables atomically; in-flight requests complete against
      the old tables.
```

### Read path (single key)

```
4. Client: mg <prefix>:<key> v\r\n

5. Gateway:
   a. Match prefix → keyspace.
   b. Parallel fan-out to all keyspace.read pools via mcp.await_logreqs.
   c. Each response → entry { key, pool, status, value, metadata }.
      Timeouts and connection errors produce status=ERROR.
   d. FFI into libmcgateway.mcgw_merge(keyspace.merge, entries) →
      merged bytes (or miss).
   e. Return VA response (or EN for miss) to the client.
```

### Read path (multi-key)

```
4'. Client: mg <prefix>:<k1>#<prefix>:<k2> v\r\n

5'. Gateway:
   a. Split key on `#` → list of keys, all sharing the same prefix.
   b. Fan out all keys to all read pools in parallel.
   c. Build entries[], ordered by key then by pool within each key.
   d. Single merge call with the full entry list → one merged result.
   e. Return VA response to the client.
```

### Write path

```
6. Client: ms <prefix>:<key> <size>\r\n<value>\r\n

7. Gateway:
   a. Match prefix → keyspace.
   b. If keyspace.merge_on_write: fan out read, merge, produce new value.
   c. Fan out write to all keyspace.write pools in parallel.
   d. Apply write_policy:
      - `all`: succeed iff all pools ack.
      - `first`: succeed when first pool in list acks; rest fire-and-forget.
   e. Return result to the client.
```

### Merge function registration

```
8. Operator (on MergeFunction CR create/update):
   a. Fetch WASM artifact from ConfigMap or OCI.
   b. For each gateway pod: ms __udf:<name> <size>\r\n<bytes>\r\n.
   c. Gateway: libmcgateway validates the module, compiles via wasmtime,
      stores in registry, persists to local disk.
   d. Subsequent mg requests whose keyspace.merge == <name> use the
      registered module.
```

---

## Project Layout

```
rust/crates/
  mcgateway-abi/         merge wire format (Entry, serde, ABI constants)
  mcgateway-merge/       built-in merges + wasmtime execution engine
  mcgateway-clib/        libmcgateway.so: C ABI shim loaded by Lua
  mcgateway-sdk/         user-facing crate for writing WASM merges
  mcgateway-cli/         mcgwctl binary: inspect, test, load

lua/
  mcgateway.lua          Lua library: routing, fan-out, write policy
  mcgateway/             submodules
  examples/proxy.lua     example user entry point

go/
  api/                   shared wire types
  api/v1alpha1/          Kubernetes CRD type definitions
  cmd/mcgateway-operator/  operator binary
  internal/
    controller/          Pool / Keyspace / MergeFunction reconcilers
    udfpush/             pushes __udf: registrations to gateway pods
    testutil/

k8s/charts/mcgateway/    Helm chart (CRDs, RBAC, Deployment, operator)
docker/                  Dockerfile (gateway image: memcached + .so + lua)
                         Dockerfile.operator
```

---

## Infrastructure Requirements

| Requirement | Detail |
|---|---|
| memcached version | 1.6.23+ (proxy mode + mcp.await_logreqs) |
| Node machine types | Any (no special hardware) |
| Gateway disk | Small local volume for persisting registered WASM modules |
| Operator permissions | Watch/update Pool, Keyspace, MergeFunction CRDs; read ConfigMaps / OCI |
| Gateway → operator | Not required; operator is the initiator |
| Gateway → backends | TCP to every pool's addresses |
| Client → gateway | TCP (standard memcache port) |
