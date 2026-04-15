# Implementation Plan

This plan orders the high-level components from
[high-level.md](../high-level.md) by dependency and demoable milestones. Each
stage produces something that runs end-to-end before moving on.

## Principles

- **Data plane before control plane.** Prove the gateway works before building
  the operator. The operator is a configuration management layer on top of a
  gateway that must already run.
- **Each stage is shippable.** At any stage boundary the product is coherent
  and usable, just less capable. Nothing is half-done across stages.
- **Defer what doesn't unblock the next stage.** Observability, CLI, and polish
  slot in where they're needed, not upfront.

---

## Stage 1 — Lua routing, single-pool passthrough

**Goal:** requests flow through the gateway with prefix-based routing to a
single backend pool per keyspace. No fan-out, no merge.

**Deliverables:**

- `lua/mcgateway.lua` — pool registry, keyspace registry, prefix resolution,
  passthrough route handlers (`mg`, `ms`, `md`).
- Configuration loaded from a local Lua or YAML file at startup. No operator
  yet.
- `docker/Dockerfile` — base image with memcached 1.6.23+ and the Lua library
  mounted.

**Demo:**

Two backend memcacheds on `mc-a:11211` and `mc-b:11211`. One gateway pod
configured with two keyspaces: `user:*` → `mc-a`, `session:*` → `mc-b`. Client
issues `mg user:foo v`, gateway forwards to `mc-a`, returns response. Same for
`session:bar` → `mc-b`.

**Validates:** memcached proxy integration, Lua library structure,
configuration loading, Docker layout.

---

## Stage 2 — Fan-out + built-in merges

**Goal:** multi-pool reads with native merge functions. Writes fan out with a
policy. Multi-key reduce via `#` works. No WASM.

**Deliverables:**

- `rust/crates/mcgateway-abi` — `Entry { key, pool, status, value, metadata }`
  type and status enum (`HIT`, `MISS`, `ERROR`). Wire format for the FFI
  boundary.
- `rust/crates/mcgateway-merge` — native built-in merges (`first-hit`,
  `pool-preferred`, `last-write-wins`). No wasmtime yet.
- `rust/crates/mcgateway-clib` — `libmcgateway.so` with C ABI exporting
  `mcgw_merge`. Dispatches to built-ins by name.
- Lua: parallel fan-out via `mcp.await_logreqs`, entry serialization across
  the FFI boundary, multi-key `#` splitting, `ERROR` status mapping from pool
  timeouts and connection failures.
- Write path: fan-out to all `write` pools in parallel with `write_policy`
  (`all` / `first`).

**Demo:**

Keyspace with `read: [frostmap, mc-cluster]`, `merge: pool-preferred`. Client
issues `mg user:123 v`, gateway fans out to both pools, returns the first hit
by pool order. Multi-key `mg user:123#user:456 v` works with a single merged
response. Writes fan out to multiple pools (for a migration scenario).

**Validates:** the FFI boundary, the merge dispatch path, fan-out
orchestration, write policies, multi-key reduce. This is the minimum viable
gateway — most use cases work without WASM.

---

## Stage 3 — WASM merge functions

**Goal:** custom merge logic written in Rust, compiled to WASM, loaded at
runtime by the gateway.

**Deliverables:**

- `rust/crates/mcgateway-merge` — wasmtime integration, module registry,
  disk persistence.
- `rust/crates/mcgateway-sdk` — user-facing crate: typed `Entry`,
  `#[merge_fn]` proc macro generating the ABI boilerplate, serde for the
  wire format.
- `libmcgateway` — loads `.wasm` files from `/etc/mcgateway/udf/` on startup
  and on change. Validates and compiles via wasmtime AOT. Registers by name.
- Lua: inotify on the UDF directory; `__udf:` handlers for
  debug/dev-time registration via the memcache protocol (not the primary
  path).

**Demo:**

User writes a merge function in Rust, `cargo build --target wasm32-wasip1`,
drops the `.wasm` into the UDF directory. Gateway picks it up, compiles, and
subsequent requests for keyspaces referencing that merge name use the custom
logic. A test merge operating on a nested protobuf demonstrates the ABI works
end-to-end.

**Validates:** the WASM ABI, the SDK ergonomics, wasmtime embedding, compile
caching, hot-reload semantics.

---

## Stage 4 — Operator + CRDs (local-path volume)

**Goal:** declarative CRDs drive the gateway configuration. Start with the
simplest mount setup — `emptyDir` or `hostPath` — so the CRD-to-files flow is
proven without wrestling with GCS.

**Deliverables:**

- `go/api/v1alpha1` — `Pool`, `Keyspace` CRD types. WASM inlined in
  `Keyspace.spec.merge.wasm` (base64) for small modules.
- `go/internal/controller` — reconcilers for `Pool` and `Keyspace`.
- `go/cmd/mcgateway-operator` — binary.
- Operator compiles CRDs to files on a mount path:
  `/etc/mcgateway/config.yaml` (pool + keyspace tables) and
  `/etc/mcgateway/udf/<name>.wasm` (one file per registered merge). Uses
  write-temp-then-rename for atomicity.
- `k8s/charts/mcgateway` — CRDs, Deployment (operator + gateway),
  RBAC, a shared volume between operator (RW) and gateway (RO).
- Integration tests using `t.TempDir()` against the same filesystem
  interface the operator uses in-cluster.

**Demo:**

`kubectl apply -f pool.yaml` and `kubectl apply -f keyspace.yaml`. Operator
reconciles, writes files, gateway picks up changes via inotify, new routing
takes effect. Client traffic is served with the new configuration within
seconds, no pod restart.

**Validates:** the filesystem abstraction between operator and gateway, CRD
reconciliation, the declarative UX.

---

## Stage 5 — GCS Fuse integration

**Goal:** swap the local-path volume for a shared GCS bucket in production,
without changing the operator's file-writing code.

**Deliverables:**

- Helm chart adds GCS Fuse CSI volume as an alternative to the local mount.
  Operator container and gateway containers both mount the same bucket (RW
  for operator, RO for gateways).
- Workload Identity setup: K8s ServiceAccount bound to GCP IAM with bucket
  read/write permissions.
- `mountOptions` tuning: `file-cache:max-size-mb`, `metadata-cache:ttl-secs`
  for sensible propagation latency.
- Docs for bucket provisioning and the IAM setup.
- E2E test on a real GKE cluster: multi-pod gateway Deployment, single
  operator, shared bucket, CRD changes propagate within the configured TTL.

**Demo:**

Same CRD workflow as Stage 4, but the gateway runs as a multi-replica
Deployment across zones, sharing a single GCS bucket. Operator is a separate
Deployment. A `Keyspace` change reaches all gateway pods within
~10 seconds (configurable via metadata TTL).

**Validates:** production topology, fleet-wide config propagation, gcsfuse
caching semantics (generation-based invalidation).

---

## Stage 6 — Observability

**Goal:** the gateway is the single instrumentation point for the entire
backend system; expose per-keyspace, per-pool, per-merge metrics.

**Deliverables:**

- Prometheus exposition from `libmcgateway`:
  - Request latency per keyspace (broken down by pool fan-out and merge time).
  - Per-pool backend latency and error rates.
  - Merge function duration and error counters (WASM traps, timeouts).
  - Hit/miss/error counts per keyspace, per pool.
  - Registry gauges: keyspaces, pools, merge functions.
- Scrape endpoint on a sidecar port.
- Grafana dashboard in `k8s/charts/mcgateway`.

**Slot:** could move earlier if debugging earlier stages needs it, but does
not block functional correctness.

---

## Stage 7 — CLI and polish

**Goal:** operator- and developer-facing tooling for inspection and testing.

**Deliverables:**

- `rust/crates/mcgateway-cli` — `mcgwctl`:
  - `inspect` — dump registered pools, keyspaces, merge functions from a
    running gateway.
  - `test-merge` — run a merge function against synthetic entries locally.
  - `register-udf` — push a WASM module via `__udf:` for dev-time iteration
    outside the CRD flow.
  - `bench` — load-test a gateway with synthetic traffic patterns.
- User-facing docs: writing merge functions, CRD reference, migration
  playbook, tuning guide.

---

## Parallelization

Work that can proceed concurrently once prerequisites are in place:

- **SDK** (Stage 3) development can start as soon as the ABI (Stage 2) is
  settled, in parallel with wasmtime integration inside `libmcgateway`.
- **Operator CRD types** (Stage 4) can be drafted in parallel with Stage 3 —
  the Go struct definitions don't depend on WASM being complete.
- **Helm chart scaffolding** can start whenever, converge with Stages 4 and 5.
- **Observability** (Stage 6) can accumulate from Stage 2 onward as each
  component adds metrics.

## Deferred from v1

Not on the critical path; revisit after the full stack ships.

- Transform UDFs (the `E<id>` flag for post-merge projection).
- Dynamic field-mask projections and runtime UDF generation.
- L1 caching of merged results in the gateway's own memory.
- Per-pool key transformation (strip / rewrite prefix before querying).
- Client identity and access control.
- Expression-based filters (inline predicates without WASM).
