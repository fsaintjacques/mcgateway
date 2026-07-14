# Stage 6 — Observability

Sixth deliverable from the [implementation plan](implementation-plan.md),
taken ahead of Stage 5: the plan slots observability as movable, and it
has no cloud dependency — every deliverable builds and verifies in the
existing kind suite, while Stage 5 is mostly GKE/GCS provisioning. The
metrics code is identical whether config arrives from a per-pod sidecar
or a shared bucket, so nothing here is thrown away by the topology flip.

The pitch is the PRD's: **the gateway is the single instrumentation
point for the entire backend system.** It is the only component that
knows the key → keyspace → pools → merge mapping, so it is the only
place that can answer the questions operators actually ask — which
*keyspace* is hot or slow, whether latency comes from the backend
fan-out or the merge, and whether one pool in a replicated pair is
erroring while `first-hit` papers over it. That last one is the sharp
edge: the merge layer's whole job is masking partial failures, which
means **without metrics, partial failures are invisible by design**.
Worse, today they are invisible even to the merge layer itself: a WASM
merge that traps or blows its deadline is silently mapped to
`MergeResult::Miss` (`mcgateway-wasm-host`'s `Merge::apply` discards
the error). Stage 6 makes every masked failure countable.

---

## Goal

A Prometheus scrape of any gateway pod answers, per keyspace, per pool,
and per merge function: request rates and outcomes, latency (fan-out
wait vs merge time, separately), hit/miss/error breakdowns, merge
failures by kind (trap, deadline, decode), and config-reload health —
including the Stage 4 last-good fallback, which today is a single
stderr line and is exactly the condition a fleet operator must be able
to alarm on.

Concretely:

1. `libmcgateway.so` grows a process-global metrics registry and a
   `/metrics` exposition endpoint on a dedicated port, serving
   Prometheus text format from a listener thread — same lifecycle
   pattern as the Stage 4 config watcher.
2. The Lua data path reports request outcomes and timings through the
   FFI boundary it already crosses: the read path rides the existing
   `mcgw_native.merge` call (one *new* FFI call per read — a monotonic
   clock read at handler entry); the write path gains one small
   observe hook.
3. WASM merge failures stop being silent: the host's error taxonomy
   (trap / deadline / decode) surfaces as counters and rate-limited,
   leveled log lines. `Merge::apply` still degrades to `Miss` for the
   caller — behavior is unchanged; visibility is new.
4. The operator enables controller-runtime's built-in metrics endpoint
   (disabled in Stage 4) plus custom gauges for the blast-radius
   policy: render warnings — today log-only — become an alertable
   signal.
5. `k8s/charts/mcgateway` exposes the ports, adds scrape annotations by
   default, and gates a `ServiceMonitor` behind a value for clusters
   that run the Prometheus operator.
6. The clib's `eprintln!` logging becomes leveled and, on per-request
   paths, rate-limited — the "structured logging lands with Stage 6"
   note in `lib.rs` comes due. Still line-oriented stderr; levels and
   storm-safety are what change, not a pipeline.

This stage exists to prove:

- The instrumentation can live where the aggregation problem is
  already solved. memcached runs N worker threads, each with its own
  Lua VM; per-VM Lua counters would need cross-VM aggregation. The
  process-global `SHARED` registries pattern from Stage 4 already
  solves exactly this — metrics are one more process-global table.
- The hot path can afford it: instrumentation adds at most two FFI
  calls per request, and the budget is pinned by a micro-benchmark,
  not asserted.
- "Log + keep serving" failure modes (Stage 4's design center) are
  *observable*, not just survivable: reload fallbacks, skipped CRs,
  and masked merge failures each get a counter a human can alert on.

---

## Non-goals

- **Distributed tracing / OpenTelemetry.** Spans across client →
  gateway → backend need client cooperation and a collector story;
  metrics answer this stage's questions without either.
- **Re-exporting memcached's own stats** (`curr_connections`, slab
  stats, `stats proxy` internals). That is
  [memcached_exporter](https://github.com/prometheus/memcached_exporter)'s
  job — run it as a sidecar if core stats are wanted. The PRD's
  "active connections" line is covered by that, not by us; our surface
  is what only the routing layer can see.
- **Per-key or per-client metrics.** Unbounded cardinality. Labels
  come from configured names only (see Cardinality).
- **Alerting rules (`PrometheusRule`) and Grafana dashboards.** Which
  thresholds page — and how a deployment visualizes — are that
  deployment's decisions; shipping a dashboard JSON in the chart is a
  bit-rot magnet no test executes. The metric inventory below is the
  stable contract to build either on.
- **A metrics push path or OpenMetrics/protobuf exposition.** Pull +
  text format is universal and testable with `wget`.
- **Changing any routing, merge, or reload behavior.** Instrumentation
  observes; the one behavioral nuance (WASM errors get counted) still
  returns `Miss` to the dispatch path exactly as today.

---

## Scope

| Feature | In scope |
|---|---|
| `metrics.rs` in `mcgateway-clib`: process-global registry, counters/gauges/histograms | ✓ |
| `/metrics` HTTP listener thread (text format), armed via `MCGW_METRICS_ADDR` | ✓ |
| Read-path instrumentation riding `mcgw_native.merge` (outcome, per-pool status/latency, merge duration, request duration) | ✓ |
| `entries.lua` gains per-pool backend latency (`res:elapsed()`) | ✓ |
| Write-path observe hook (policy, outcome, duration) | ✓ |
| Config-reload counters incl. last-good fallback, wired from `gw.load_config` | ✓ |
| WASM merge error taxonomy: trap/deadline/decode counters + rate-limited logs | ✓ |
| Registry gauges (built-in/wasm merges), UDF rescan counters, SIGHUP trigger counters | ✓ |
| Leveled logging in the clib (`log` facade over stderr, rate-limited request-path errors) | ✓ |
| Operator: controller-runtime metrics endpoint + render-warning/commit metrics | ✓ |
| Chart: metrics ports, scrape annotations, gated `ServiceMonitor` | ✓ |
| Kind tests: scrape assertions for data path, fallback counter, operator warnings | ✓ |
| Tracing/OTel, memcached core-stat re-export | — (non-goal) |
| Per-key/per-client labels, push exposition | — (non-goal) |
| Alerting rules | — (non-goal) |

---

## Project layout

```
rust/crates/mcgateway-clib/
  src/metrics.rs                 NEW  registry, metric types, text exposition, listener
  src/lib.rs                     (edited) merge-dispatch instrumentation, now/observe
                                          exports, metrics + logging init
  src/udf_loader.rs              (edited) rescan/skip counters, leveled logs
  src/watcher.rs                 (edited) reload-trigger counters
rust/crates/mcgateway-wasm-host/
  src/lib.rs                     (edited) error kind surfaced from run()
lua/mcgateway/
  entries.lua                    (edited) entry gains `elapsed`
  routes.lua                     (edited) handler timing, write observe, sentinel counters
lua/mcgateway.lua                (edited) reload result hook (pcall'd, non-fatal)
lua/tests/                       (edited) fake native gains now/observe; new assertions
go/cmd/mcgateway-operator/main.go (edited) --metrics-addr, custom collectors registered
go/internal/operator/metrics.go  NEW  render-warning gauge, commit counters
k8s/charts/mcgateway/
  templates/deployment.yaml      (edited) metrics ports, scrape annotations, env
  templates/servicemonitor.yaml  NEW  gated on metrics.serviceMonitor.enabled
  values.yaml                    (edited) metrics.* block
go/internal/kind/metrics_test.go NEW  scrape assertions
doc/plans/stage-6-observability.md  (this file)
```

New dependency: one Prometheus client crate in `mcgateway-clib`
(`prometheus-client` — text-format-only, no protobuf tail). The HTTP
side is a hand-rolled GET-only responder on `std::net::TcpListener`
(~60 lines): one endpoint, one verb, no framework. If the client crate
chafes (API friction with dynamic-but-bounded label values), the
fallback is hand-rolled atomics with fixed histogram buckets — the
exposition format is simple and the golden test pins it either way.
Go side adds nothing: controller-runtime already vendors
`client_golang`.

---

## Where the metrics live

The load-bearing decision, and it is already made: **metrics are
process-global state in `libmcgateway.so`, exposed by a thread the
library owns.** Three facts force it:

1. **memcached's threading model.** The proxy runs one Lua VM per
   worker thread plus a config VM. A counter incremented in Lua lives
   in one VM; per-keyspace totals would need cross-VM aggregation.
   Stage 4 hit the same wall for the UDF registry and solved it with
   the process-global `SHARED: OnceLock` — every VM that requires
   `mcgateway_native` sees the same tables. The metrics registry is
   one more field behind the same pattern; aggregation is free because
   there is nothing to aggregate.
2. **Half the signal never touches Lua.** WASM traps, deadline kills,
   UDF rescans, registry swaps, SIGHUP raises — all Rust-side events.
   Any Lua-side accumulation scheme still needs a Rust-side registry
   for these, i.e. two pipelines for one scrape.
3. **Histograms.** The PRD wants latency distributions, not averages.
   Atomic bucket arrays in Rust are trivial; per-VM Lua histograms
   plus merge-on-scrape is a project.

The alternative considered and rejected: memcached's proxy user stats
(`mcp.add_stat`/`mcp.stat`), which memcached aggregates across workers
for free. But they are counters only (no histograms), they expose over
the memcache text protocol (`stats proxyuser`) so a Prometheus
translator sidecar is needed anyway, and the Rust-side events still
need a second path. One registry, one endpoint, one format wins.

The exposition endpoint follows the config watcher's precedent
exactly: armed by an environment variable (`MCGW_METRICS_ADDR`, e.g.
`0.0.0.0:9151`), unset means off, standalone `docker run` behavior
unchanged. The listener thread is spawned in `init_shared()` beside
the watcher and serves every request from the registry snapshot —
scrapes never take a lock the data path contends on.

---

## Instrumentation points

### Read path: ride the existing FFI crossing

Every fan-out read already crosses into Rust once —
`mcgw_native.merge(name, entries)` — and the entries it carries
already contain per-pool `status`. The dispatch is therefore a
complete read-path observer that costs nothing new; it only lacks
three inputs, all cheap:

- **The keyspace.** The merge call gains an options table:
  `mcgw_native.merge(name, entries, { prefix = ..., start = ... })`.
  Passing the prefix explicitly beats deriving it from the key in Rust
  (which would duplicate the router's `stop = ":"` semantics in a
  second language). The argument is optional — existing tests and any
  out-of-tree caller keep working.
- **The clock.** `mcgw_native.now()` returns a monotonic timestamp
  (nanoseconds, Rust `Instant`-based). The handler reads it once at
  entry and passes it as `start`; since `enqueue`/`wait_cond` happen
  before the merge call, `merge-return − start` covers fan-out wait +
  merge — the whole request minus response formatting (nanoseconds).
  Total new FFI on the read path: **one call**. Rust-side, the
  dispatch times the merge body separately, which yields the PRD's
  "broken down by pool fan-out and merge time" as two honest series
  rather than one estimated split.
- **Backend latency.** `entries.lua` adds `elapsed` from
  `res:elapsed()` (memcached's per-response backend timing) next to
  the existing `t` parse. Availability and semantics of `elapsed()` on
  our pinned memcached (1.6.41) is this stage's first verification
  item — see Risks.

Outcome classification happens Rust-side from what the dispatch
already sees: `Winner`/`Synthesized` → hit; else any entry `miss` →
miss; else error. This mirrors `routes.lua`'s reply-selection logic by
construction on the same inputs.

### Write path: one small hook

Writes never cross the FFI today, so they get the minimal crossing:
`mcgw_native.observe(prefix, op, outcome, start)`, called where the
handler already computes the answer — `reduce_write_all`'s rank *is*
the outcome label (`stored` / `negative` / `error`), and the `first`
policy classifies its single response the same way. The same hook,
with `start = nil`, counts the sentinel routes. Two FFI calls per
write (`now` + `observe`), strings and integers only. Policy is not a
label: it is a per-keyspace constant, so it would only duplicate what
the config already says.

### Reload path: make the fallback alertable

`gw.load_config` reports each load through
`mcgw_native.observe_reload(result, n_pools, n_keyspaces)` — `result`
∈ `ok`/`fallback`. The pool/keyspace counts double as config-shape
gauges. Two constraints carry over from Stage 4's fallback design:

- The hook is `pcall`'d and its failure ignored. Stage 4 deliberately
  made the fallback path registry-independent (`keyspaces.lua` no
  longer requires the native module); metrics must not re-couple
  survival to the native module's health. A gateway that cannot count
  reloads must still reload.
- First-load-strict is untouched: the hook observes outcomes, it does
  not create new ones.

Rust-side, the watcher counts its SIGHUP raises by trigger
(`config` change vs `udf-swap` re-raise — the Stage 4 race-closing
re-raise becomes visible), and the UDF loader counts rescans and
per-module load failures.

### WASM failures: count what is currently swallowed

`WasmMerge::apply` maps every host error to `Miss` and drops the
error. The fix is taxonomy, not behavior: `run()`'s error becomes
classifiable (trap / deadline / decode / oversize — the kinds the host
already distinguishes internally), `apply` counts it and emits a
rate-limited warning, and still returns `Miss`. Dispatch semantics,
the Lua contract, and the fallback story are all byte-identical; the
difference is that `mcgateway_merge_errors_total{merge="...",kind="deadline"}`
climbing is now a dashboard fact instead of a mystery of elevated
miss rates.

### Sentinel routes

The router's `default` (unknown keyspace), `__udf`, and `__mcgw`
handlers count too — a spike in unknown-keyspace traffic is a client
misconfiguration signal. Unknown prefixes are **not** labels (see
Cardinality); they all count under the fixed sentinel
`keyspace="__unknown__"`.

---

## Metric inventory

Gateway (`libmcgateway`, `:9151`), all prefixed `mcgateway_`:

| Metric | Type | Labels | Source |
|---|---|---|---|
| `requests_total` | counter | `keyspace`, `op`=read\|write, `outcome` (read: hit\|miss\|error; write: stored\|negative\|error) | merge dispatch / `observe` |
| `request_duration_seconds` | histogram | `keyspace`, `op` | `start` → hook return |
| `backend_requests_total` | counter | `pool`, `status`=hit\|miss\|error | entry statuses in dispatch |
| `backend_duration_seconds` | histogram | `pool` | entry `elapsed` |
| `merge_duration_seconds` | histogram | `merge` | timed around `Registries::apply` |
| `merge_errors_total` | counter | `merge`, `kind`=trap\|deadline\|decode\|oversize | wasm-host error taxonomy |
| `config_reloads_total` | counter | `result`=ok\|fallback | `observe_reload` |
| `config_pools`, `config_keyspaces` | gauge | — | `observe_reload` |
| `registry_merges` | gauge | `kind`=builtin\|wasm | registry swap |
| `udf_rescans_total` | counter | `result`=ok\|error | `rescan_into` |
| `udf_module_failures_total` | counter | `reason` | loader skip callback |
| `reload_signals_total` | counter | `trigger`=config\|udf-swap | watcher |

Histogram buckets are fixed constants: request/backend span 100 µs –
2.5 s (exponential, ~14 buckets — memcache round trips live at the
bottom, timeouts at the top); merge spans 10 µs – 100 ms (the default
WASM deadline is 50 ms, so the deadline cliff sits inside the range).

Operator (`:8080`): controller-runtime's built-ins (reconcile
counts/durations, workqueue depth, client latencies) come free once
the endpoint is enabled; custom additions:

| Metric | Type | Labels |
|---|---|---|
| `mcgateway_operator_render_warnings` | gauge | — (count from last render) |
| `mcgateway_operator_commits_total` | counter | `result`=ok\|error |
| `mcgateway_operator_snapshot_objects` | gauge | `kind`=pool\|keyspace |

`render_warnings` is the alertable form of Stage 4's blast-radius
policy: a skipped CR today is an operator log line; after this stage
it is a nonzero gauge that stays nonzero until the CR is fixed —
level-triggered, like the reconciler that sets it.

### Cardinality

Label values come from configuration, never from traffic: keyspace
prefixes, pool names, and merge names are all operator-controlled and
bounded (and the Stage 4 CRD prefix pattern was chosen
"metric-label-safe" for exactly this moment). Anything
request-derived buckets into fixed sentinels (`__unknown__`).
Worst-case series count is
`keyspaces × 3 outcomes + pools × 3 + merges × ~4` plus histogram
buckets — hundreds, not thousands, at realistic configs.

Series for a deleted keyspace/pool persist until process restart
(counters must not reset on reload; a Prometheus counter that goes
backwards corrupts `rate()`). Gauges re-set on every reload, so
`config_keyspaces` is always current. Accepted: stale series cost a
few bytes per scrape and age out with the pod.

---

## Logging

The clib's `eprintln!` calls move to the `log` facade with a minimal
stderr logger (level from `MCGW_LOG`, default `info`). Two rules:

- Per-request paths (merge errors, entry projection failures) log at
  `warn` through a token-bucket rate limiter — one bad UDF at 50k rps
  must not produce 50k log lines per second. The counter carries the
  true rate; the log carries a sampled diagnostic.
- Lifecycle paths (watcher armed, rescan failed, reload fallback) stay
  unconditional — they are rare by construction and each one matters.

The Lua side keeps its existing behavior (the fallback line
`config reload from %s failed, keeping previous config` is load-bearing
for a Stage 4 kind test and stays byte-stable).

---

## Helm chart changes

- `metrics.enabled` (default `true`): sets `MCGW_METRICS_ADDR` on the
  gateway container, exposes `metrics` (9151) on the pod and Service,
  and adds `prometheus.io/scrape|port|path` pod annotations. In
  operator mode the operator container likewise gets
  `--metrics-addr=:8080` and a named port.
- `metrics.serviceMonitor.enabled` (default `false`): renders a
  `ServiceMonitor` for both ports. Off by default because it requires
  the Prometheus operator's CRDs — installing it into a cluster
  without them fails the release.
- All three modes (operator / liveReload / static) get metrics — the
  exposition depends only on the clib, not on how config arrives.

No dashboard ships in the chart (see Non-goals): the queries worth
graphing — request rate/outcome by keyspace, latency quantiles,
backend latency/error rate by pool, merge duration and error rate,
reload fallbacks, operator warnings — follow directly from the metric
inventory, which is the supported contract.

---

## Testing

### Rust unit — registry and exposition

Golden test on the text exposition (fixed input counters → exact
output bytes, format pinned); histogram bucket-boundary tests; listener
smoke test (bind `127.0.0.1:0`, GET `/metrics`, assert content type
and a known series); rate-limiter behavior. The wasm-host error
taxonomy gets a test per kind using the existing trap/deadline fixture
modules — proving a deadline blow increments
`merge_errors_total{kind="deadline"}` and still returns `Miss`.

A micro-benchmark pins the data-path cost: `merge` dispatch with
instrumentation vs the Stage 4 baseline over representative entries.
Budget: ≤ 2 µs added per call. This is the number that makes
"the hot path can afford it" a measurement instead of a vibe.

### Lua unit

The fake `mcgateway_native` preload (three tests use it) grows `now`,
`observe`, and `observe_reload` as recording stubs, and `merge`
accepts the optional third argument. New assertions: the read handler
passes `prefix`/`start` through; write handlers classify outcomes onto
the recorded calls; `load_config` reports `ok` and `fallback`
correctly *and* still falls back cleanly when the observe hook itself
errors (the non-fatality contract).

### Kind integration

Scrapes run via `kubectl exec` + busybox `wget -qO- localhost:9151/metrics`
(the gateway image is alpine) — no port-forward plumbing.

- `TestMetricsDataPath` — drive traffic across a passthrough keyspace,
  a fan-out keyspace, both write policies, and an unknown prefix;
  scrape; assert `requests_total` moved with the right labels,
  `backend_duration_seconds` has series for both pools,
  `merge_duration_seconds` has series for the exercised merges, and
  the unknown traffic landed on `keyspace="__unknown__"`.
- `TestMetricsReloadFallback` — reuse the Stage 4 fault-injection
  pattern (write a bad config through the RW state mount), assert
  `config_reloads_total{result="fallback"}` increments and the gateway
  keeps serving. Pairs the existing survival test with its new
  observability.
- `TestMetricsWasmError` — if a trapping fixture module can ride the
  inline-wasm CR path cheaply, assert `merge_errors_total` moves;
  otherwise this coverage stays at the rust unit level (the taxonomy
  is host-side; kind adds transport, not logic).
- `TestOperatorMetrics` — scrape the operator port; after applying a
  bad CR (the `TestOperatorBadCR` fixture), assert
  `mcgateway_operator_render_warnings > 0`, then fix the CR and assert
  it returns to zero.

Stage 1–5 tests are untouched except the fake-native shims — the
no-behavior-change claim is enforced by the suite that already exists.

---

## Exit criteria

Stage 6 is done when all of the following hold:

1. `make check test-kind` is green; the four metrics tests pass; every
   metric in the inventory is asserted present-and-moving by at least
   one test (unit or kind).
2. A WASM merge that traps or exceeds its deadline is countable and
   logged (rate-limited) — and dispatch behavior is byte-identical to
   Stage 4 (`Miss` to the caller, old routes/entries semantics
   untouched).
3. The Stage 4 last-good fallback is alertable: the kind fault
   injection demonstrably increments `config_reloads_total{result="fallback"}`.
4. The data-path budget holds: the dispatch micro-benchmark shows
   ≤ 2 µs added per request, and the read path added exactly one new
   FFI call.
5. `helm install` with defaults exposes and annotates both endpoints;
   with `metrics.enabled=false`, the rendered manifests are
   env-var-and-port-identical to Stage 4's output; the ServiceMonitor
   renders only when enabled.

---

## Execution plan

Four steps, riskiest assumption first, each green under
`make check test-kind` on its own — the Stage 3b/4 cadence.

### Step 1 — Metrics core in the clib (rust)

`metrics.rs` (registry, types, text exposition, listener thread,
`MCGW_METRICS_ADDR` arming), the `log` facade migration with the
rate limiter, and every metric that needs no Lua change: registry
gauges, rescan/module-failure counters, SIGHUP trigger counters, and
the WASM error taxonomy (wasm-host `run()` error kinds surfaced,
counted, rate-limit-logged). Golden exposition test, listener smoke
test, taxonomy tests against the trap fixtures. Independently useful:
a scrape already answers "is the registry healthy, are reloads
firing."

### Step 2 — Data-path instrumentation (lua + rust)

The stage's risk budget, so its verification items live here, first:
`res:elapsed()` exists on memcached 1.6.41 and means per-backend
request time (spike in the fixture harness before building on it),
and the micro-benchmark pins the dispatch overhead. Then:
`entries.lua` `elapsed`, `merge` opts (`prefix`, `start`),
`mcgw_native.now`/`observe`/`observe_reload`, read/write/reload
instrumentation in `routes.lua` and `mcgateway.lua`, sentinel
counters, and the Lua test updates (recording fakes, non-fatality of
the observe hooks). Needs step 1.

### Step 3 — Operator metrics (go)

Enable the manager's metrics endpoint (`--metrics-addr`), add
`metrics.go` (warnings gauge set by every render, commit counters,
snapshot gauges), unit assertions in the existing reconciler tests.
Independent of steps 1–2 — can land in either order.

### Step 4 — Chart and kind suite (helm + go)

`metrics.*` values, ports/annotations/env across all three modes, the
gated ServiceMonitor, and the kind tests. Exit criteria close here.
Needs steps 1–3.

Dependency shape: `{1, 3} → 2 → 4` (3 only truly blocks 4).

---

## Risks and open questions

- **`res:elapsed()` availability/semantics.** The per-pool latency
  design leans on memcached's response object exposing backend elapsed
  time. Verified first thing in step 2; if absent or wrong-meaning on
  1.6.41, the fallback is Lua-side timing around `wait_cond` — which
  yields max-across-pools fan-out wait instead of per-pool series
  (coarser but honest), with per-pool latency deferred until the
  proxy API provides it.
- **Hot-path label lookup.** `prometheus-client` resolves label sets
  through a locked map per observation. At memcache request rates this
  is the plausible budget-breaker; the micro-benchmark exists to catch
  it, and the mitigation ladder is: pre-resolved metric handles cached
  per (keyspace, pool) in a read-mostly table, then hand-rolled
  atomics. Decide on evidence, not taste.
- **A listener thread inside a dlopen'd library.** Same lifecycle
  class as the Stage 4 watcher thread (which shipped without incident):
  spawned once per process from `init_shared`, never joined, dies with
  the process. The one new wrinkle is binding a port — failure to bind
  (port taken, no perms) must be loud but non-fatal: a gateway that
  cannot expose metrics must still serve traffic. Log-and-continue,
  matching the fallback philosophy everywhere else.
- **Scrape-vs-swap consistency.** A scrape concurrent with a registry
  swap or reload sees a torn-but-valid snapshot (each series
  individually atomic). Prometheus tolerates this by design; noted so
  nobody "fixes" it with a lock the data path would then contend on.
- **Counter monotonicity across reloads.** Deleted keyspaces leave
  stale series (accepted, documented above). The subtle trap is
  *reusing* a prefix after deletion: the old counter continues, which
  is correct for `rate()` but can surprise a human reading raw totals.
  A documentation fact, not something to "fix."
- **Two FFI calls per write vs zero today.** Writes gain the larger
  relative overhead (reads already crossed for the merge). If the
  benchmark shows the write hook mattering, `observe` can take
  the `now()` reading itself (one call, duration measured Rust-side
  from enqueue is lost — but write outcome/rate matter more than write
  latency split). Kept as the documented fallback, not the default.
