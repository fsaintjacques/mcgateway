# Stage 4 — Operator + CRDs on a pod-local volume

Fifth deliverable from the [implementation plan](implementation-plan.md).
Stages 1–3b built the complete data plane: prefix routing, fan-out, native
and WASM merges, all configured by two file surfaces the gateway already
watches or loads — `/etc/mcgateway/config.lua` (pools + keyspaces, loaded
at proxy config time) and the UDF directory (`.wasm` modules, hot-reloaded
by `UdfLoader`). Stage 4 adds the control plane: `Pool` and `Keyspace`
CRDs and an operator that reconciles them into exactly those files.

The point is that the gateway does not learn anything new. Stage 3b froze
disk as the registration surface for merges and rejected `__udf:` push;
this stage extends the same posture to routing config. The operator is a
CRD-to-files compiler. Everything it writes, a human could have written
by hand — and did, in Stages 1–3.

This document supersedes the push-model sketch in
[high-level.md](../high-level.md) (operator sending config and
`ms __udf:` over the memcache protocol) and the `MergeFunction` CRD.
Files won; the protocol push path stays dead, and merge modules ride
inline on `Keyspace` for v1.

---

## Goal

A user applies `Pool` and `Keyspace` CRs. Within seconds, every gateway
pod routes accordingly — no pod restart, no ConfigMap edit, no Helm
upgrade. A `Keyspace` carrying an inline WASM module gets that module
compiled, registered, and dispatching, end to end from `kubectl apply`.

Concretely:

1. `go/api/v1alpha1` defines `Pool` and `Keyspace` (group
   `mcgateway.dev`, namespaced). Small WASM merges ship inline as
   base64 in `Keyspace.spec.merge.wasm`.
2. `go/internal/operator` renders the full set of CRs in a namespace
   into a file tree — `config.lua` plus one `<name>.wasm` per inline
   module — and commits it to a mount atomically
   (write-temp-then-rename), in an order that never lets `config.lua`
   reference a module file that isn't on disk yet.
3. `go/cmd/mcgateway-operator` wraps that in a controller-runtime
   manager watching both kinds.
4. The operator runs as a **native sidecar** in each gateway pod,
   writing to a shared `emptyDir` (operator RW, gateway RO). No
   cross-pod distribution: every pod derives the same files from the
   same CRs independently.
5. `libmcgateway.so` grows a config-file watcher beside the existing
   `UdfLoader`: when `config.lua` changes, it raises `SIGHUP` in its
   own process, and memcached re-runs `mcp_config_pools` /
   `mcp_config_routes` — the proxy's built-in live-reload path.
6. `k8s/charts/mcgateway` gains CRDs, read-only RBAC, the sidecar, and
   an `operator.enabled` toggle. The static-ConfigMap path survives for
   operator-less deployments.

This stage exists to prove:

- The file contract Stages 1–3b accreted (`config.lua` shape + UDF
  directory semantics) is actually operator-shaped: a Go process can
  drive both surfaces without any gateway-side change beyond the reload
  trigger.
- The renderer/filesystem split is real: Stage 5 must be able to lift
  the identical rendering code into a standalone Deployment writing to
  a GCS mount, changing only the volume and the topology.
- Config propagation is live: CR change → files → SIGHUP → new routes,
  with in-flight requests completing against the old tables.
- The declarative UX is coherent: CRD validation plus renderer
  validation means one malformed CR degrades to a warning, never a
  fleet-wide outage.

---

## Non-goals

- **GCS Fuse, Workload Identity, multi-pod file distribution.** Stage 5.
  The sidecar-per-pod topology exists precisely so Stage 4 never has to
  move a file between pods.
- **`MergeFunction` CRD / OCI / ConfigMap artifact references.** Inline
  base64 on `Keyspace` covers small modules (etcd caps objects at
  ~1.5 MiB; our examples are far under). A shared-module CRD returns
  when someone actually needs one module across many keyspaces or a
  module too big to inline.
- **CRD status subresource, conditions, leader election.** N sidecars
  would race status writes for zero decision-making value in v1.
  Read-only RBAC is a feature: the operator cannot mutate cluster
  state at all. Status lands in Stage 5 alongside the single-operator
  topology, where exactly one writer exists.
- **Admission webhooks.** Cert plumbing and a failure mode (webhook
  down → no CR writes) we don't need: OpenAPI schema validation on the
  CRDs catches shape errors, and the renderer catches semantic ones.
- **Migration orchestration** (phased dual-write → read-shift → cut
  over). That's a workflow on top of `Keyspace` edits, not a v1
  reconciler feature.
- **Endpoint resolution from K8s Services.** `Pool.spec.addrs` is a
  static list in v1, same as the Lua config today. Watching
  `EndpointSlice`s is additive later.
- **Changing any Stage 1–3 gateway behaviour.** `config.lua` keeps its
  exact schema; the Lua validator is untouched; `UdfLoader` semantics
  (built-ins shadow disk modules, per-module failure isolation) carry
  over unchanged.

---

## Scope

| Feature | In scope |
|---|---|
| `Pool`, `Keyspace` CRD types (`mcgateway.dev/v1alpha1`), deepcopy + CRD YAML via controller-gen | ✓ |
| Inline WASM: `Keyspace.spec.merge.wasm` (base64) rendered to `<merge.name>.wasm` | ✓ |
| Renderer: CR snapshot → `{config.lua, *.wasm}` as a pure function | ✓ |
| Go-side semantic validation mirroring `lua/mcgateway/config.lua` (skip bad CRs, keep going) | ✓ |
| Atomic committer: temp-then-rename, fsync, wasm-before-config-before-deletes ordering | ✓ |
| `mcgateway-operator` binary: controller-runtime manager, snapshot reconciler over both kinds | ✓ |
| Operator as native sidecar (initContainer + `restartPolicy: Always`), shared `emptyDir` | ✓ |
| Config watcher in `mcgateway-clib` raising `SIGHUP` on `config.lua` change (debounced) | ✓ |
| `MCGATEWAY_CONFIG` / `MCGW_UDF_DIR` pointed at the shared mount in operator mode | ✓ |
| Helm: CRDs, ServiceAccount + namespaced read-only Role, `operator.enabled`, CRs from structured values | ✓ |
| `docker/Dockerfile.operator` (static Go binary, distroless) | ✓ |
| Kind tests: apply/update/delete CRs live, inline-wasm dispatch, bad-CR isolation | ✓ |
| Cross-language contract test: Go-rendered golden `config.lua` fed through the Lua validator | ✓ |
| Static ConfigMap path (`operator.enabled=false`) | ✓ (kept) |
| Status subresource, conditions, leader election | — (Stage 5) |
| GCS Fuse volume, standalone operator Deployment | — (Stage 5) |
| `MergeFunction` CRD, OCI/ConfigMap artifacts | — (deferred) |
| Admission webhooks, Service-based endpoint resolution | — (deferred) |

---

## Project layout

```
go/
  go.mod                          (edited) + sigs.k8s.io/controller-runtime
  api/v1alpha1/                   NEW  Pool, Keyspace types + zz_generated deepcopy
  cmd/mcgateway-operator/         NEW  main: manager setup, flags (--dir, --namespace)
  internal/operator/
    render.go                     NEW  snapshot → files, semantic validation
    commit.go                     NEW  atomic writer against an fs interface
    controller.go                 NEW  snapshot reconciler wiring
    testdata/                     NEW  golden config.lua fixtures
rust/crates/mcgateway-clib/
  src/config_watcher.rs           NEW  notify watch on config.lua → raise(SIGHUP)
  src/lib.rs                      (edited) start watcher beside UdfLoader
lua/
  tests/test_rendered_config.lua  NEW  golden fixtures through config.validate
k8s/charts/mcgateway/
  crds/                           NEW  controller-gen output, committed
  templates/operator-rbac.yaml    NEW  ServiceAccount, Role, RoleBinding
  templates/deployment.yaml       (edited) sidecar, emptyDir, env overrides
  templates/resources.yaml        NEW  Pool/Keyspace CRs from structured values
  values.yaml                     (edited) operator.*, pools/keyspaces values
docker/Dockerfile.operator        NEW  build + distroless runtime
Makefile                          (edited) operator build, generate, image; kind loads both
doc/plans/stage-4-operator.md     (this file)
```

The operator lives under the existing `go/` module
(`github.com/fsaintjacques/mcgateway/go`), which already carries
`client-go` for the kind tests. controller-runtime is the one new
dependency family. No kubebuilder scaffolding beyond `controller-gen`
for deepcopy and CRD YAML — the project doesn't need the full layout
for two list-and-render controllers.

---

## The file contract

This is the load-bearing decision of the stage: **the operator emits
`config.lua`, not YAML.** The implementation plan sketched
`config.yaml`; that sketch loses to three facts on the ground:

1. The gateway already parses and validates Lua config
   (`lua/mcgateway/config.lua`), including defaulting and referential
   checks, exercised by every stage's tests. A YAML surface means a
   YAML parser inside the proxy's Lua VM — a new C module or a
   pure-Lua dependency — plus a second validation path. All cost, no
   capability.
2. The config is pure data. What the operator writes is a table
   literal — `return { pools = {...}, keyspaces = {...} }` — so the
   Go side needs a ~60-line serializer (strings, ints, bools, string
   lists, sorted keys, escaped strings), not a code generator.
   Determinism matters more than elegance: identical CR snapshots must
   produce byte-identical files so mtime-based change detection and
   content diffing stay honest.
3. Humans already write this exact file (`lua/examples/config.lua`,
   `values.config`). One format for hand-rolled and operator-managed
   deployments means the docs, the validator, and the mental model
   don't fork.

`loadfile` executes the config, so a hostile file is code execution —
but the threat model doesn't change: today that file comes from a
ConfigMap anyone with namespace write access can edit; after Stage 4 it
comes from CRs behind the same RBAC. The renderer only ever emits data
literals. (Loading config in an empty Lua environment is a cheap
hardening we should do anyway; noted in risks.)

The full contract the operator targets, all frozen by earlier stages:

| Surface | Consumer | Change detection |
|---|---|---|
| `$MCGATEWAY_CONFIG` (`config.lua`) | `proxy.lua` via `gw.load_config` at `mcp_config_pools` time | **new:** clib watcher → `SIGHUP` |
| `$MCGW_UDF_DIR/*.wasm` | `UdfLoader` (notify + debounce + `ArcSwap`) | already live (Stage 3b) |

Both env overrides already exist (`MCGW_UDF_DIR` fails loudly when set
to a non-directory — exactly right for an operator-supplied path).
Operator mode just points them at the shared mount:
`/var/run/mcgateway/config.lua` and `/var/run/mcgateway/udf`. The
image's baked-in `/etc/mcgateway` paths stay untouched for standalone
`docker run` use.

---

## Config reload

memcached's proxy already has a live-reload mechanism: `SIGHUP`
re-executes `mcp_config_pools` in the config thread and rebuilds each
worker's routes via `mcp_config_routes`, with in-flight requests
completing against the old route objects. Stage 1's `proxy.lua` was
written for this (config is snapshotted in `mcp_config_pools` and
shipped to workers precisely so reloads don't race). Nobody sends the
signal today — the chart fakes it with a checksum annotation that
restarts the pod.

The gap is the trigger, and the shortest honest path is: **the config
watcher lives in `libmcgateway.so` and raises `SIGHUP` in its own
process.** A `config_watcher` module beside `udf_loader` reuses the
same `notify` + debounce (200 ms) machinery, watches the parent
directory of `$MCGATEWAY_CONFIG` (rename-based commits replace the
inode, so watch the directory, not the file), and calls
`libc::raise(SIGHUP)` when the file lands.

Why this over the alternatives:

- **Entrypoint wrapper script** (start memcached in the background,
  poll the file, `kill -HUP`): a second process supervision story
  inside the container, a shell loop as a correctness-critical
  component, and memcached loses PID 1.
- **Sidecar with `shareProcessNamespace`**: pod-spec surgery and a
  container whose only job is `kill`, to deliver a signal the target
  process could deliver to itself.
- **Pod restart via checksum annotation** (status quo): loses the
  stage's headline demo — config changes without dropping connections
  — and rolls the whole Deployment for a one-line keyspace edit.

The watcher starts in `luaopen_mcgateway_native` only when
`MCGATEWAY_CONFIG` is set (standalone images keep today's behaviour:
edit requires restart). Signal-storm safety comes from the debounce on
the watcher side and from the operator committing `config.lua` at most
once per reconcile. The watcher also re-raises SIGHUP after every UDF
registry swap — that closes the wasm-registration/config-reload race
that `has_merge` validation opens; see *Merge-name resolution* under
CRDs for the full argument.

One behaviour had to be verified early, because the design leans on
it: **a config file that fails to load on reload must leave the old
routes serving.** Step 1's spike settled it, negatively: memcached
treats an error thrown inside `mcp_config_pools` during a SIGHUP
reload as fatal and exits — and because the bad file persists in the
state volume across container restarts, the pod crash-loops. The fix
landed Lua-side rather than as the rust pre-flight originally
sketched: `gw.load_config` pcalls the load and, on a reload failure,
logs and returns the last good config (module state survives reloads
— the proxy re-runs `mcp_config_pools` in the same config VM). First
load stays strict: a gateway must not start blind. This is cheaper
than pre-flight validation in the watcher, keeps the fallback next to
the code that owns config loading, and degrades a half-written file
to one redundant-but-idempotent route rebuild.

---

## Topology: sidecar per pod

Stage 4's mandate is a *local-path* volume, and `emptyDir` is only
shared within a pod — so the operator rides in the gateway pod:

```
┌─ gateway pod ─────────────────────────────────────────┐
│  initContainer (native sidecar, restartPolicy=Always) │
│    mcgateway-operator --dir=/var/run/mcgateway        │
│      watches Pool/Keyspace CRs (this namespace)       │
│      renders + commits config.lua, udf/*.wasm    [RW] │
│      startupProbe: config.lua exists                  │
│  container                                            │
│    memcached + proxy.lua + libmcgateway.so            │
│      MCGATEWAY_CONFIG=/var/run/mcgateway/config.lua   │
│      MCGW_UDF_DIR=/var/run/mcgateway/udf         [RO] │
│  volumes: mcgateway-state (emptyDir)                  │
└───────────────────────────────────────────────────────┘
```

Every pod's sidecar independently watches the same CRs and renders the
same bytes. There is no shared mutable state, hence nothing to elect a
leader over, and CR-to-fleet propagation is exactly as fast as each
pod's watch stream. Scale-up needs no reconcile-on-pod-event logic:
a new pod's sidecar does a full render on start, by construction.

The **native sidecar** form (an initContainer with
`restartPolicy: Always`, GA since K8s 1.29 and available in kind's
default node image) buys the startup ordering for free: kubelet holds
the gateway container until the sidecar's startup probe passes, and
the probe is "the rendered `config.lua` exists." The gateway therefore
never boots against an empty mount, and `proxy.lua`'s strict
missing-file failure stays strict. If the API server is unreachable at
pod start, the pod (correctly) stays unready.

Stage 5 inverts the topology — one operator Deployment, RW on a GCS
bucket; gateways mount it RO — by swapping the volume and moving the
container. `render.go` and `commit.go` move without edits; `commit.go`
writes through a filesystem interface for exactly this reason, and
because `t.TempDir()` tests demand it anyway.

Cost of the interim topology, accepted for v1: N pods = N watch
streams on the API server (trivial at gateway replica counts), and a
brief window where pods disagree about config during propagation —
already true of any multi-pod rollout, and merges are stateless.

---

## CRDs

Group `mcgateway.dev`, version `v1alpha1`, two kinds — `Pool` and
`Keyspace` — both namespaced. The specs transcribe the Lua config
schema (`lua/mcgateway/config.lua` is the normative validator; the
CRDs exist to make its rules fail at `kubectl apply` time instead of
at reload time), with field names bent to Kubernetes convention:
`writePolicy` for `write_policy`, `metadata.name` for `pools[].name`.

### Pool

```go
// PoolSpec defines a named backend: a set of addresses plus the
// client-side hashing used to spread keys across them. The pool's
// name is metadata.name.
type PoolSpec struct {
    // Addrs lists backend addresses as "host:port". Order is
    // significant for distribution stability.
    // +kubebuilder:validation:MinItems=1
    // +kubebuilder:validation:items:MinLength=1
    // +listType=atomic
    Addrs []string `json:"addrs"`

    // Hash selects the key-hashing function. Backend default applies
    // when unset.
    // +optional
    // +kubebuilder:validation:Enum=xxhash;md5;crc32
    Hash string `json:"hash,omitempty"`

    // Dist selects the key-distribution strategy across Addrs.
    // +optional
    // +kubebuilder:validation:Enum=ring_hash;jump_hash
    Dist string `json:"dist,omitempty"`
}
```

Pool identity is `metadata.name` — no `spec.name` to drift from it,
one name means one referent, and duplicate detection is the API
server's job (a name collision is a CR *update*, not a config error).
The Lua validator accepts any non-empty pool name; DNS-1123 restricts
this to lowercase alphanumerics, `-`, `.`. A hand-written config can
name a pool `mc_A`; a CR cannot. Accepted: the operator path is
allowed to be stricter than the escape hatch, never looser.

### Keyspace

```go
// KeyspaceSpec binds a key prefix to backend pools with a merge
// strategy and a write policy. The routing key is spec.prefix;
// metadata.name is only the object's identity.
type KeyspaceSpec struct {
    // Prefix routes keys of the form "<prefix>:<rest>". Must be
    // unique across Keyspaces in the namespace (renderer-enforced).
    // +kubebuilder:validation:MinLength=1
    // +kubebuilder:validation:MaxLength=128
    // +kubebuilder:validation:Pattern=`^[A-Za-z0-9][A-Za-z0-9_.-]*$`
    Prefix string `json:"prefix"`

    // Read lists pools fanned out to on reads, in preference order
    // (pool-preferred consumes this order).
    // +kubebuilder:validation:MinItems=1
    // +kubebuilder:validation:items:MinLength=1
    // +kubebuilder:validation:XValidation:rule="self.all(x, self.filter(y, y == x).size() == 1)",message="read pools must be unique"
    // +listType=atomic
    Read []string `json:"read"`

    // Write lists pools writes fan out to. Required: the Lua schema
    // has no read-only keyspace, and inventing one is not this
    // stage's business.
    // +kubebuilder:validation:MinItems=1
    // +kubebuilder:validation:items:MinLength=1
    // +kubebuilder:validation:XValidation:rule="self.all(x, self.filter(y, y == x).size() == 1)",message="write pools must be unique"
    // +listType=atomic
    Write []string `json:"write"`

    // WritePolicy: "all" succeeds iff every write pool acks; "first"
    // succeeds on the first pool in Write order.
    // +optional
    // +kubebuilder:default=all
    // +kubebuilder:validation:Enum=all;first
    WritePolicy string `json:"writePolicy,omitempty"`

    // Merge selects the merge function for fan-out reads. Unset means
    // the gateway default ("first-hit").
    // +optional
    Merge *MergeSpec `json:"merge,omitempty"`
}

type MergeSpec struct {
    // Name of a built-in merge or of a WASM module.
    // +kubebuilder:validation:MinLength=1
    // +kubebuilder:validation:Pattern=`^[A-Za-z0-9][A-Za-z0-9_-]*$`
    Name string `json:"name"`

    // Wasm optionally inlines the module implementing Name
    // (base64-encoded in YAML; rendered to $MCGW_UDF_DIR/<Name>.wasm).
    // Capped well under etcd's object limit.
    // +optional
    // +kubebuilder:validation:MaxLength=1048576
    Wasm []byte `json:"wasm,omitempty"`
}
```

Example:

```yaml
apiVersion: mcgateway.dev/v1alpha1
kind: Pool
metadata:
  name: mc-a
spec:
  addrs: ["mc-a:11211"]
---
apiVersion: mcgateway.dev/v1alpha1
kind: Keyspace
metadata:
  name: profiles
spec:
  prefix: profile
  read: [mc-a, mc-b]
  write: [mc-a]
  writePolicy: first
  merge:
    name: profile-proto
    wasm: <base64 of merge_profile_proto.wasm>
```

### Field mapping and validation split

| `config.lua` | CRD | Rule | Enforced by |
|---|---|---|---|
| `pools[].name` non-empty, unique | `Pool` `metadata.name` | DNS-1123 | API server |
| `pools[].addrs` non-empty list of non-empty strings | `spec.addrs` | `MinItems=1`, items `MinLength=1` | schema |
| `pools[].hash` ∈ xxhash/md5/crc32 | `spec.hash` | enum | schema |
| `pools[].dist` ∈ ring_hash/jump_hash | `spec.dist` | enum | schema |
| `keyspaces[].prefix` non-empty, no `:`, not `__udf`/`__mcgw` | `spec.prefix` | pattern (no `:`, no leading `_`) | schema |
| prefix unique across keyspaces | — | skip later CR (by `metadata.name` order) + Warning | renderer |
| `read`/`write` non-empty, no duplicates | `spec.read`/`spec.write` | `MinItems=1` + CEL uniqueness | schema |
| `read`/`write` reference existing pools | — | skip CR + Warning | renderer |
| scalar→list promotion (`read = "mc-a"`) | — | lists only; the shorthand is a Lua-side nicety | (dropped) |
| `write_policy` ∈ all/first, default `all` | `spec.writePolicy` | enum + `+kubebuilder:default` | schema |
| `merge` default `"first-hit"` | `spec.merge` unset | renderer omits the key; Lua defaults | Lua |
| `merge` known to the gateway (`has_merge`) | — | see resolution rule below | renderer |

Two defaulting strategies on purpose: `writePolicy` is defaulted in
the schema (visible in `kubectl get -o yaml`, cheap), while the merge
default stays Lua-side — defaulting a nested object in OpenAPI is
noisy, and "unset means gateway default" is the honest semantic when
the default lives in `DEFAULT_MERGE` next to the validator.

The prefix pattern is deliberately stricter than Lua (which bans only
`:`, exact `__udf`, and exact `__mcgw`): no leading `_` reserves the
entire dunder namespace for future control prefixes, and a bounded
charset keeps prefixes shell-, log-, and metric-label-safe. Both
lists are `listType=atomic` rather than `set`: order is load-bearing
(`pool-preferred`, `writePolicy: first`) and server-side-apply merge
semantics on sets do not preserve it.

### Merge-name resolution

The sharpest constraint in the whole design: the Lua validator calls
`mcgw_native.has_merge(name)` **and rejects the entire config file on
an unknown name**. The renderer therefore never emits a keyspace whose
merge it cannot prove resolvable. The provable set is:

    built-ins  ∪  { merge.name of every inline-wasm Keyspace in the snapshot }

Built-in names (`first-hit`, `last-write-wins`, `pool-preferred`) are
a constant in the renderer, and the cross-language contract test pins
it: the Lua fixture harness preloads exactly these names, so a rust
built-in added without updating the renderer constant fails
`make check`. A keyspace referencing anything outside the set —
including a module baked into the image, which operator mode does not
load — is skipped with a Warning, same blast-radius policy as every
other relational failure.

A name is resolvable, though, only once `UdfLoader` has actually
*registered* the module — and registration is asynchronous. The
committer writes `module.wasm` before `config.lua`, but both watchers
debounce independently, so the SIGHUP can beat the registry swap:
`has_merge` fails, the whole reload is (correctly) rejected, old
routes keep serving — and nothing re-triggers. The fix is clib-side
and dumb: **after every UDF registry swap, the config watcher re-raises
SIGHUP.** Config reload is idempotent, registry swaps happen only on
module changes (never on config reload, so no cycle), and the
worst case is one redundant reload per wasm drop. The cost is real but
bounded — a reload rebuilds `mcp.pool` objects and their backend
connections — and it buys eventual consistency without any
cross-process handshake. `TestOperatorInlineWasm` covers exactly this
ordering.

Inline-module rules, matching Stage 3b loader semantics: a `Name`
colliding with a built-in is a Warning and the module is dropped (the
built-in would shadow it anyway); two Keyspaces inlining *different*
bytes under one name is a Warning and the lexicographically-first
`metadata.name` wins; identical bytes dedupe silently. A `MergeSpec`
with `Name` only (no `Wasm`) referencing another Keyspace's inline
module is fine — modules are pod-global capabilities, not
per-keyspace.

---

## Operator architecture

### Snapshot reconciler

There is no per-object reconcile. Any watch event on either kind
enqueues a single fixed key; the reconciler then lists **all** Pools
and Keyspaces in the namespace, renders the full desired file tree,
and commits it if it differs from what's on disk. Level-triggered,
stateless, and deletion needs no finalizers: a deleted CR is simply
absent from the next snapshot, and the files are a pod-local cache
rebuilt from scratch on every pod start.

controller-runtime gives us the watch plumbing, caching informers, and
backoff; we use a `source.Kind` per CRD funneling into one
`Reconcile(ctx, _)` via a constant request. Concurrency 1 — renders
are cheap and serializing them makes the commit ordering trivial.

### Renderer (`render.go`)

Pure function:

```go
type Snapshot struct {
    Pools     []v1alpha1.Pool
    Keyspaces []v1alpha1.Keyspace
}

// Render returns the desired file tree: path → content.
// Paths are relative: "config.lua", "udf/<name>.wasm".
func Render(s Snapshot) (map[string][]byte, []Warning)
```

Semantic validation mirrors `lua/mcgateway/config.lua` rule for rule:
unknown pool references, duplicate prefixes, empty addr lists, invalid
enum combos. The critical policy: **a CR that fails validation is
skipped with a Warning; rendering continues.** The Lua validator
rejects the *whole* file on any error, so shipping one bad keyspace
would take every keyspace down with it — the renderer must therefore
guarantee the emitted file passes the Lua validator, which it does by
construction plus the contract test below. Warnings surface as
operator log lines (and, once status exists in Stage 5, conditions).

Inline WASM: `merge.wasm` decodes to bytes rendered at
`udf/<merge.name>.wasm`. Renderer-enforced rules, matching Stage 3b's
loader semantics: a module name colliding with a built-in is a
Warning and the module is dropped (the built-in would shadow it
anyway); two Keyspaces inlining *different* bytes under the same merge
name is a Warning and the lexicographically-first Keyspace wins
(deterministic beats clever); identical bytes under the same name
dedupe silently.

Output is deterministic: pools and keyspaces sorted by name, table
keys emitted in fixed order, no timestamps or provenance comments.
Same snapshot, same bytes, forever.

### Committer (`commit.go`)

Takes the rendered tree and the mount root, computes the diff against
disk, and applies it in an order that keeps every intermediate state
loadable:

1. Write new/changed `.wasm` files (temp file in `udf/`, fsync,
   rename). `UdfLoader` picks these up immediately; registering a
   module no keyspace references yet is a no-op by design ("disk is
   the capability surface, config is the routing surface").
2. Write `config.lua` (temp-then-rename in the same directory) iff its
   bytes changed. The clib watcher fires here — the only SIGHUP per
   commit.
3. Delete stale `.wasm` files — only after the config that no longer
   references them is live. `UdfLoader` deregisters; in-flight merges
   hold the old `ArcSwap` snapshot and complete safely.

All writes go through a small `fs` interface (`WriteFile`, `Rename`,
`Remove`, `ReadDir`, `ReadFile`) so unit tests run against
`t.TempDir()` and Stage 5's GCS mount slots in unchanged.

### Binary (`cmd/mcgateway-operator`)

Flags: `--dir` (mount root), `--namespace` (defaults to the pod's own,
via the downward API). Health endpoint for the startup/liveness
probes: healthy once the first successful commit lands. Structured
logs via controller-runtime's zap defaults. That's the whole binary.

---

## Helm chart changes

- `crds/` — controller-gen output, committed to the repo (reviewable
  diffs; `helm install` applies them, upgrades follow Helm's usual
  crds-are-manual story, acceptable at v1alpha1).
- `operator.enabled` (default `false`) — flipping it:
  - adds the native-sidecar initContainer, the `mcgateway-state`
    `emptyDir`, and the `MCGATEWAY_CONFIG`/`MCGW_UDF_DIR` env vars on
    the gateway container (RO mount);
  - adds ServiceAccount + Role (`get/list/watch` on
    `pools.mcgateway.dev`, `keyspaces.mcgateway.dev`) + RoleBinding —
    read-only, namespaced;
  - drops the config ConfigMap, its mount, and the
    `checksum/config` restart annotation.
- `templates/resources.yaml` — renders `values.pools` /
  `values.keyspaces` (structured, schema-shaped) into CRs, so a Helm
  install can seed a working config declaratively. This replaces the
  free-form `values.config` Lua string *in operator mode*; the string
  stays for the static path.
- `values-kind.yaml` — switches to `operator.enabled=true` and defines
  the same pools/keyspaces today's static config carries, so the
  Stage 1–3 kind tests run against operator-delivered config without
  their bodies changing. (The two WASM keyspaces reference modules by
  name only; the kind tests inject the module bytes as inline-wasm CRs
  — see Testing.)

---

## Build and packaging

### Dockerfile.operator

Two stages: `golang:1.25-alpine` building
`CGO_ENABLED=0 go build ./cmd/mcgateway-operator`, then
`gcr.io/distroless/static` running it as nonroot. No shell, no libc,
~15 MB. The gateway image is untouched — wasm examples stay baked at
`/etc/mcgateway/udf` for standalone use; operator mode simply points
`MCGW_UDF_DIR` elsewhere. (Consequence, stated plainly: with the
operator enabled, image-baked modules are not loaded. A keyspace that
wants one gets it as inline wasm through the CR. One directory, one
owner — no merge-two-sources semantics.)

### Makefile

- `generate`: `controller-gen object` (deepcopy) +
  `controller-gen crd` into `k8s/charts/mcgateway/crds/`. `check`
  greps for a dirty tree after `generate` so drift fails CI.
- `build` grows the operator binary; `docker-build` becomes
  `docker-build-gateway docker-build-operator`; `kind-load` loads both
  images (`MCGATEWAY_OPERATOR_IMAGE ?= mcgateway-operator:dev`,
  mirroring the existing image/repo/tag split and the podman archive
  workaround).
- `helm-install-kind` passes the operator image repo/tag.

### CI

No workflow surgery: the `paths` filters already cover `go/**`,
`docker/**`, `k8s/**`, and the kind job drives everything through
`make test-kind`, which now builds and loads two images. Both
Dockerfiles ride the same buildx GHA layer cache; the operator image
is small and cheap. `make check` picks up `go vet` over the new
packages and the generate-drift check automatically.

---

## Testing

### Go unit — renderer

Golden tests: `testdata/<case>/input.yaml` (CR lists) →
`testdata/<case>/config.lua`. Cases: empty snapshot; single
passthrough pool; multi-pool keyspace with every enum exercised;
unknown-pool keyspace (skipped + Warning); duplicate prefix (second
skipped, deterministic); inline wasm (bytes land at the right path);
inline name colliding with a built-in (dropped + Warning); same name,
different bytes (first wins); same name, same bytes (dedupe).
Determinism test: render the same snapshot twice with shuffled input
order, assert byte-identical output.

### Cross-language contract test

The renderer's core guarantee — *emitted files always pass the Lua
validator* — is enforced, not assumed: `lua/tests/
test_rendered_config.lua` loads every golden `config.lua` fixture
through `config.validate` with the same fake-native preload the
existing Lua tests use. `make check` already runs the Lua suite, so a
renderer change that breaks the contract fails the same CI job that
built it. This is the test that keeps Go-side and Lua-side validation
from drifting apart silently.

### Go unit — committer

Against `t.TempDir()`: fresh commit writes everything; no-op commit
touches nothing (mtimes stable — this is what makes SIGHUP-per-change
honest); wasm-add ordering (config referencing a new module is never
observable before the module file); deletion ordering (stale module
outlives the config that dropped it); crash simulation (temp file left
behind from a killed rename is ignored and cleaned on next commit).

### Envtest — reconciler

One `envtest` case: apply CRs against a real API server, assert the
files appear; delete a CR, assert the tree converges. Thin by design —
the reconciler is a list-render-commit loop and the pieces above are
tested to death. If envtest's binary management is more CI trouble
than this one test is worth, it degrades to a fake-client test without
loss of nerve; the kind suite covers the real API path regardless.

### Kind integration

The suite installs the chart with `operator.enabled=true`; Stage 1–3
tests pass with config arriving via CRs instead of the ConfigMap.
New tests (`go test -tags kind`, existing helpers):

- `TestOperatorConfigApply` — apply a new Pool + Keyspace at runtime,
  poll the new prefix until it routes (<10 s), assert no gateway
  container restart (`restartCount` stable — the no-restart claim is
  an assertion, not a promise).
- `TestOperatorConfigUpdate` — flip a keyspace's `read` order /
  `merge`, assert behaviour change, again with `restartCount` stable.
- `TestOperatorInlineWasm` — apply a Keyspace inlining
  `merge_last_n_wins.wasm` (bytes read from the rust build tree, as
  Stage 3b tests do), assert `__mcgw:names` grows the module and
  dispatch picks the highest-`t` entry. This test also pins the
  wasm-registration/config-reload ordering: the keyspace must
  eventually route even when the SIGHUP beats the registry swap
  (the re-raise path in *Merge-name resolution*).
- `TestOperatorBadCR` — apply a Keyspace referencing a nonexistent
  pool alongside healthy ones; assert healthy keyspaces keep serving
  and the operator logs the Warning. The blast-radius test.
- `TestOperatorRemoval` — delete the inline-wasm Keyspace; assert the
  prefix stops routing and the module leaves `__mcgw:names`.

---

## Exit criteria

Stage 4 is done when all of the following hold:

1. `make check test-kind` is green with both docker and podman. Every
   Stage 1/2/3 kind test passes with configuration delivered through
   CRDs. The five new operator tests pass.
2. The headline demo works from a clean cluster: `helm install` with
   `operator.enabled=true`, `kubectl apply` a Pool and a Keyspace, and
   client traffic routes within seconds — with zero gateway container
   restarts across the whole sequence.
3. A reload with a failing config file (verified by test or by forced
   fault injection) leaves the old routes serving. The "log + keep
   serving" failure mode holds at every layer: CRD schema, renderer
   Warning, Lua validator, SIGHUP path.
4. The renderer/committer run identically against `t.TempDir()` and
   the in-cluster mount — no `if in-cluster` branches — and the golden
   fixtures pass the Lua validator via `make check`.
5. Neither `lua/mcgateway/config.lua` (validator), the `Merge`/
   `Registry` surfaces, nor `UdfLoader` semantics changed. The proof
   the file contract was operator-shaped is that only a watcher was
   added.
6. `helm install` with `operator.enabled=false` still deploys the
   static-ConfigMap gateway, bit-for-bit equivalent to Stage 3b's
   chart output.

Once these hold, Stage 5 lifts the operator into its own Deployment
against a GCS Fuse mount — touching topology and volumes, not
rendering logic — exactly as the implementation plan intends.

---

## Execution plan

One plan (this document), six steps. Each step lands as its own
reviewable unit, green under `make check test-kind`, ordered so the
riskiest assumptions are tested before anything is built on them —
the same step-wise cadence Stage 3b used.

### Step 1 — Config watcher + SIGHUP spike (rust)

The only gateway-side change in the stage, and the stage's entire
risk budget. Deliverables: `config_watcher.rs` (directory watch,
debounce, `raise(SIGHUP)`, re-raise on UDF registry swap), wired in
`luaopen_mcgateway_native` behind `MCGATEWAY_CONFIG`. Verification
items pinned here, before any operator code exists:

- A failing config on SIGHUP reload keeps old routes serving and does
  not kill the process. **Verified false**: memcached exits on a
  config error during reload. Fixed in this step with the last-good
  fallback in `gw.load_config` (see §Config reload) rather than the
  rust pre-flight originally sketched.
- Rename-based file replacement (the committer's commit primitive)
  reliably triggers the directory watcher on emptyDir.
- A kind test proving live reload with no operator: `kubectl cp` a
  changed `config.lua` into a pod, poll until the new route serves,
  assert `restartCount` stable.

Independently useful when it lands: hand-managed live reload.

### Step 2 — CRD types + generate plumbing (go)

`go/api/v1alpha1` exactly as specced in §CRDs, controller-gen for
deepcopy + CRD YAML into `k8s/charts/mcgateway/crds/`, Makefile
`generate` target and the dirty-tree drift check in `check`. Purely
additive; no behavior change anywhere. No dependency on step 1 —
steps 1 and 2 can proceed in parallel or in either order.

### Step 3 — Renderer + contract test (go + lua)

`render.go` with the semantic validation and skip-with-Warning
policy, the golden fixtures, the determinism test, and
`lua/tests/test_rendered_config.lua` in the same step — the contract
test is part of the renderer's definition of done, not a follow-up.
Needs step 2 (the types).

### Step 4 — Committer (go)

`commit.go` behind the `fs` interface, with the `t.TempDir()` suite:
no-op stability, wasm-before-config-before-deletes ordering, crash
leftovers. Consumes a rendered tree, not the renderer — can proceed
in parallel with step 3.

### Step 5 — Operator binary + reconciler (go + docker)

The manager wiring (`controller.go`, `cmd/mcgateway-operator`), the
thin envtest-or-fake-client reconcile test, `Dockerfile.operator`,
and the Makefile image targets. Glue by design: steps 3–4 already
carry the logic and the tests that matter. Needs steps 3 and 4.

### Step 6 — Chart integration + kind suite (helm + go)

Everything meets: sidecar + emptyDir + env overrides + RBAC behind
`operator.enabled`, structured `values.pools`/`values.keyspaces`,
`values-kind.yaml` flipped to operator mode, the five operator kind
tests, Stage 1–3 tests green via CR-delivered config, CI green with
both images. Exit criteria 1, 2, and 6 are checked off here; 3
inherits step 1's verification; 4 and 5 hold by construction from
steps 3–5.

Dependency shape: `{1, 2} → {3 ∥ 4} → 5 → 6`. No finer split — a
step below this granularity (types without their generated YAML, a
renderer without its goldens) can't be judged on its own.

---

## Risks and open questions

- **SIGHUP reload semantics under a bad config.** The design assumes
  memcached survives a failing `mcp_config_pools` on reload and keeps
  old routes. Verify in week one, before building on it; the fallback
  (watcher pre-validates the file in a scratch Lua state before
  raising the signal) is contained but should be built early if
  needed, not discovered late.
- **`notify` on emptyDir.** inotify on emptyDir-backed local
  filesystems is reliable (unlike the gcsfuse caveat Stage 3b flagged
  for Stage 5). The commit-via-rename pattern means the watcher must
  watch the directory, not the file inode — easy to get subtly wrong;
  the kind update test exists to catch it.
- **Native sidecar availability.** `restartPolicy: Always` on
  initContainers needs K8s ≥1.29. kind's current default is well past
  that; the chart should fail template rendering with a clear message
  on older clusters rather than producing a pod that deadlocks at
  startup.
- **Config execution surface.** `loadfile` on the rendered config runs
  it as code. Operator-rendered files are data literals by
  construction, but loading config with an empty environment
  (`load(chunk, name, "t", {})`) is cheap defense worth taking while
  we're in the area — it also hardens the hand-written path.
- **Inline wasm and object size.** base64 inflates modules ~33% and
  etcd caps objects around 1.5 MiB; `merge_profile_proto` fits with
  room, but a prost-heavy real-world module might not. When someone
  hits the ceiling, that's the `MergeFunction` CRD's cue — resist
  solving it speculatively.
- **Two config dialects in the chart.** `values.config` (Lua string,
  static path) and `values.pools`/`values.keyspaces` (structured,
  operator path) can drift. Mitigation: `values-kind.yaml` only uses
  the structured form, so CI exercises it; the static form is
  legacy-frozen and documented as such.
- **Renderer/Lua validator parity is a treadmill.** Every future
  keyspace field lands in three places: CRD schema, renderer
  validation, Lua validator. The contract test catches emit-side
  drift, but nothing catches the renderer being *stricter* than Lua
  (harmless) vs *looser* (caught by contract test only if a golden
  covers it). Discipline: new config fields add a golden case in the
  same PR, enforced by review.
- **Group name: decided.** `mcgateway.dev`, mimicking mcfreeze's
  `mcfreeze.dev` (its CRDs ship as `mcfreeze.dev_*.yaml` in the sibling
  chart). Same convention, same caveat inherited knowingly: the domain
  is unregistered, and a rename after CRs exist in real clusters is a
  migration. Consistency across the two projects outweighs that.
- **N sidecars, one API server.** Each gateway pod holds two informers'
  worth of watch + cache. Negligible at tens of replicas; if this
  project ever runs hundreds of gateway pods, Stage 5's single-operator
  topology is the answer anyway — don't optimize the interim.
