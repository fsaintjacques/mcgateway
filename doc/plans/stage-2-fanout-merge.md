# Stage 2 — Fan-out + Built-in Merges (pure Lua)

Second deliverable from the [implementation plan](implementation-plan.md).
Extend the Stage 1 passthrough gateway into a real multi-pool gateway: reads
fan out to a list of pools, a named built-in merge function reduces the
per-pool responses into a single response, and writes fan out to a list of
pools with a configurable policy.

**No Rust, no FFI, no shared library in this stage.** All three built-in
merges fit in a few dozen lines of Lua each. The FFI boundary and the wire
format exist to let untrusted WASM code run behind a stable ABI; without
WASM there is nothing to defend against and nothing to serialize for. Stage
3 introduces the FFI boundary alongside WASM, where it is actually
load-bearing.

---

## Goal

One gateway pod, configured with keyspaces that reference **multiple** read
pools and **multiple** write pools, correctly:

1. Fans out each read to every `read` pool, collects per-pool entries, runs
   a named built-in merge, returns the merged response.
2. Fans out each write to every `write` pool according to `write_policy`
   (`all` or `first`).
3. Translates pool timeouts and connection failures into `ERROR`-status
   entries so the merge function can decide whether to degrade.

Multi-key `#` fan-out is **out of scope** and keys containing `#` are
rejected with `SERVER_ERROR multi-key not supported`. See the Deferred
section at the end of this doc.

This stage exists to prove:

- Parallel fan-out orchestration in Lua via funcgens with multiple handles.
- Write policy semantics end-to-end.
- The built-in merge set is sufficient for the common cases (migrations,
  overlays, snapshot-plus-cache) before we pay the complexity of WASM.

This is the **minimum viable gateway**. Most of the stated product use
cases work with just the three built-in merges shipped here.

---

## Non-goals

- Any Rust code, shared library, or FFI boundary — deferred to Stage 3.
- Any stable wire format for entries — deferred to Stage 3 (the WASM ABI
  forces it; no reason to design it now).
- WASM modules, the `mcgateway-sdk` crate, user-defined merge logic —
  deferred to Stage 3.
- `__udf:` registration. The handler continues to return `SERVER_ERROR udf
  not supported`.
- `merge_on_write` (read-modify-write on the write path) — deferred.
- Multi-key `#` fan-out — keys with `#` are rejected. See risks section.
- Per-pool key transformation (prefix rewrite before querying) — deferred.
- Kubernetes operator, CRDs, metrics beyond existing memcached stats —
  deferred to Stages 4 / 6.
- Dynamic field-mask projections, post-merge transform UDFs (`E<id>`) —
  explicitly deferred from v1.

---

## Scope

| Feature | In scope |
|---|---|
| Pool registry (unchanged shape from Stage 1) | ✓ |
| Keyspace `read` as a list of pool names | ✓ |
| Keyspace `write` as a list of pool names | ✓ |
| `write_policy` ∈ `{ "all", "first" }` | ✓ |
| Keyspace `merge` name ∈ built-ins | ✓ |
| Built-in merges: `first-hit`, `pool-preferred`, `last-write-wins` | ✓ |
| Parallel read fan-out across all `read` pools | ✓ |
| Parallel write fan-out per `write_policy` | ✓ |
| Multi-key `#` splitting on `mg` | — (rejected; deferred) |
| Pool timeout / connection errors → `ERROR` entry | ✓ |
| Integration tests covering all three merges, both write policies, multi-key | ✓ |

Anything outside this table is out of scope for Stage 2.

---

## Project layout

Stage 2 only touches the existing `lua/` tree and tests. No `rust/`
directory yet; no Dockerfile changes beyond what Stage 1 already ships.

```
lua/mcgateway/
  config.lua         (existing) config loading + validation
  pools.lua          (existing) mcp.pool() construction
  keyspaces.lua      (existing) prefix resolution
  routes.lua         (rewritten) funcgens for fan-out + router attach
  merges.lua         (new) first_hit, pool_preferred, last_write_wins
  entries.lua        (new) build an entry table from fan-out responses
  util.lua           (existing) helpers (key parsing, logging)
```

No additions under `go/`, `docker/`, or `k8s/` in this stage beyond the
test fixtures needed for the new kind cases.

---

## Configuration format

Extends Stage 1's Lua table. Pools are unchanged; keyspaces gain lists,
a policy, and a merge name.

```lua
return {
    pools = {
        { name = "frostmap",   addrs = { "frostmap:11211" } },
        { name = "mc-cluster", addrs = { "mc1:11211", "mc2:11211" },
          hash = "xxhash", dist = "ring_hash" },
        { name = "legacy",     addrs = { "legacy:11211" } },
    },

    keyspaces = {
        {
            prefix       = "user",
            read         = { "frostmap", "mc-cluster" },  -- list, ordered
            write        = { "mc-cluster" },
            write_policy = "all",                         -- default
            merge        = "pool-preferred",
        },
        {
            prefix       = "session",
            read         = { "mc-cluster" },
            write        = { "mc-cluster", "legacy" },
            write_policy = "first",
            merge        = "first-hit",
        },
    },
}
```

Validation additions over Stage 1:

- `read` is a non-empty list; every entry names a declared pool.
- `write` is a non-empty list; every entry names a declared pool.
- `write_policy` ∈ `{ "all", "first" }`, defaults to `"all"` if omitted.
- `merge` names a built-in registered in `mcgateway.merges`. Unknown names
  fail at `load_config`, not at request time.
- For back-compat with Stage 1 configs, `read = "poolname"` is accepted
  and promoted to `{ "poolname" }`; same for `write`. Parser convenience;
  not a supported long-term form.

Validation failures raise a Lua error with a line-identifying message,
surfaced in the memcached startup log. Same error model as Stage 1.

---

## Fan-out and merge — read path

One funcgen per keyspace for reads, wiring one handle per `read` pool and
closing over the merge function at build time.

```lua
-- lua/mcgateway/routes.lua (sketch)
local function read_fgen(ks)
    local fg = mcp.funcgen_new()
    local handles = {}
    for i, pool in ipairs(ks.read_pools) do
        handles[i] = fg:new_handle(pool)
    end
    fg:ready({
        f = function(rctx)
            return function(r)
                if r:key():find("#", 1, true) then
                    return "SERVER_ERROR multi-key not supported\r\n"
                end
                -- Merge may need extra flags in the response (e.g. `t`).
                for f in (ks.merge_flags or ""):gmatch(".") do
                    r:flag_add(f)
                end
                rctx:enqueue(r, handles)
                rctx:wait_cond(#handles, mcp.WAIT_ANY)

                local row = {}
                for j, h in ipairs(handles) do
                    row[j] = rctx:res_any(h)
                end
                local entries = entries_mod.build(r:key(), ks.read_names, row)
                local winner = ks.merge_fn(entries)
                if winner then return winner.res end
                return "EN\r\n"
            end
        end,
    })
    return fg
end
```

### Entry shape

A Lua table per pool. Purely an in-process structure — no serialization.

```lua
{
    key    = "user:123",
    pool   = "frostmap",
    status = "hit" | "miss" | "error",
    res    = <mcp.response userdata or nil>,
    t      = 3600,  -- parsed `t` flag if present in the response line
}
```

Merges return the winning entry (or `nil` for miss); the caller forwards
`entry.res` back to the client. Built-in merges only need `status` and `t`.
Future merges that need other flags can parse `res:line()` themselves.

### Ordering contract

Entries come back in the pool's order within the keyspace's `read` list.
Merge functions rely on this — it's part of the interface between the
gateway and the merges.

### Error mapping

| Backend outcome | Entry status |
|---|---|
| Normal hit | `"hit"` |
| `EN` / miss | `"miss"` |
| Connection refused, TCP reset | `"error"` |
| `mcp.await_logreqs` timeout | `"error"` |
| Protocol-level `SERVER_ERROR` from pool | `"error"` |

The merge function receives the full entry list including errors and
decides. `first-hit` skips non-hits; `pool-preferred` skips non-hits and
continues down the list; `last-write-wins` considers only hits.

---

## Fan-out — write path

Writes do not merge in Stage 2 (no `merge_on_write`). They fan out per
policy:

```lua
local function write_fgen(write_pools, policy)
    local fg = mcp.funcgen_new()
    local handles = {}
    for i, p in ipairs(write_pools) do
        handles[i] = fg:new_handle(p)
    end
    fg:ready({
        f = function(rctx)
            return function(r)
                if policy == "all" then
                    local responses = enqueue_and_wait_all(rctx, r, handles)
                    return reduce_write_all(responses)   -- worst outcome wins
                else  -- "first"
                    -- Send to every pool; wait only for handles[1].
                    -- Others continue in-flight; their completion is
                    -- fire-and-forget.
                    for _, h in ipairs(handles) do rctx:enqueue(r, h) end
                    return rctx:wait(handles[1])
                end
            end
        end,
    })
    return fg
end
```

Policy semantics:

- `all`: client sees success iff every pool returns success. Any
  `NS`/`EX`/`NF` or error from any pool becomes the returned status; the
  gateway picks the "strongest negative" (error > NS > EX > NF).
- `first`: client sees the first listed pool's response. Later pools are
  dispatched in parallel but their outcomes are discarded. If the first
  pool errors, the request fails even if a later pool would have
  succeeded — the list order is the contract.

`ms` and `md` share the same funcgen shape; only the underlying command
differs.

---

## Built-in merges (`lua/mcgateway/merges.lua`)

All three are pure Lua functions over the entry list. Each returns the
merged value string, or `nil` for miss. No I/O, no coroutines, no
allocation beyond the returned string.

```lua
local M = {}

function M.first_hit(entries)
    for _, e in ipairs(entries) do
        if e.status == "hit" then return e.value end
    end
    return nil
end

function M.pool_preferred(entries)
    -- Identical to first_hit given our entry ordering contract, but kept
    -- as a distinct name for clarity of intent at the call site.
    return M.first_hit(entries)
end

function M.last_write_wins(entries)
    local best, best_t
    for _, e in ipairs(entries) do
        if e.status == "hit" then
            local t = e.flags and e.flags.t
            if best == nil or (t and (best_t == nil or t > best_t)) then
                best, best_t = e.value, t
            end
        end
    end
    return best
end

M._by_name = {
    ["first-hit"]       = M.first_hit,
    ["pool-preferred"]  = M.pool_preferred,
    ["last-write-wins"] = M.last_write_wins,
}

function M.lookup(name) return M._by_name[name] end
function M.names()
    local ns = {}
    for n in pairs(M._by_name) do ns[#ns+1] = n end
    return ns
end

return M
```

`config.lua`'s validator calls `merges.lookup(ks.merge)` at load time;
unknown names fail startup.

---

## Docker image

Unchanged from Stage 1. The image still ships memcached plus the Lua
library. No new layers, no new binaries. This is the cheapest property
of doing Stage 2 in pure Lua — the serving image does not change shape
until Stage 3.

---

## Testing

### Lua smoke check

Stage 1's `make check` already loads `require('mcgateway')` under
`lua5.4`. Extend it to exercise `mcgateway.merges` against hand-built
entry tables:

```
make check   # adds a small test script that calls the three merges
             # with fixture entry tables and asserts outputs.
```

Pure-Lua unit tests for the merges are worth keeping here because the
behavior table (`first-hit` on all-miss, `last-write-wins` tie-break
order, etc.) is small, stable, and easier to read as a table than as an
integration test assertion.

### Go kind integration tests

Extend the Stage 1 `go/internal/kind/` tree. New cases use a
three-backend topology (`mc-a`, `mc-b`, `mc-c`) with keyspaces
exercising the matrix:

| Test | Setup | Assertion |
|---|---|---|
| `TestFanoutReadFirstHit` | `read=[a,b]`, seed only on `b` | Gateway returns the `b` value |
| `TestFanoutReadPoolPreferred` | `read=[a,b]`, seed both with different values | Gateway returns the `a` value |
| `TestFanoutReadAllMiss` | `read=[a,b]`, no seed | Gateway returns `EN` |
| `TestFanoutReadPoolError` | `read=[a,b]`, `a` scaled to 0 | Gateway returns `b`'s value; no error surfaced |
| `TestWriteAll` | `write=[a,b]`, `policy=all`; read back from each backend | Value present on both `a` and `b` |
| `TestWriteFirst` | `write=[a,b]`, `policy=first` | Returns success when `a` acks; `b` eventually has the value (retry) |
| `TestWriteAllBackendDown` | `policy=all`, `b` unreachable | Returns failure status |
| `TestMultiKey` | `mg user:1#user:2#user:3 v`, varied seeds | Single merged response |
| `TestMergeLastWriteWins` | Two pools with overlapping keys, different `t` flags | Returns the entry with greater `t` |
| `TestConfigReloadFanout` | Stage 1's reload test, but a new multi-pool keyspace | New prefix resolves and fans out |

The existing Stage 1 tests must continue to pass unchanged — they simply
pin single-pool `read`/`write` lists via the list-of-one form.

### Makefile

No new targets. Stage 1's `check`, `test-kind`, `docker-build`,
`kind-up`, `kind-load`, `helm-install-kind` all apply unchanged.

---

## Lifecycle changes from Stage 1

None at the memcached level. The Lua library still exposes
`mcp_config_pools` and `mcp_config_routes`; the routing module now
builds funcgens with N handles and attaches routers the same way.

Reload is unchanged: `mcp_config_routes` runs again and replaces the
router atomically. Merge names are re-resolved against
`mcgateway.merges` on reload — swapping a keyspace's merge name takes
effect on the first request after reload.

---

## Exit criteria

Stage 2 is done when all of the following are true:

1. `make check test-kind` is green on a fresh checkout with both docker
   and podman. Stage 1 cases still pass; Stage 2 cases pass.
2. All three built-in merges have pure-Lua unit coverage for the basic
   matrix (all-hit, all-miss, mixed, all-error, ties).
3. The kind tests demonstrate: fan-out reads across two pools with all
   three merges; write fan-out under both policies; pool-down resilience
   for reads; pool-down surface for `write_policy=all`; `#` keys rejected.
4. Stage 1's config format still loads (list-of-one promotion works),
   so existing Stage 1 deployments upgrade without a config change.
5. The Docker image is byte-for-byte structurally identical to Stage 1
   (same base, same layers, same non-root user) — only the Lua content
   differs.

Once all five hold, Stage 3 (WASM + FFI boundary + the wire ABI) can
begin.

---

## Risks and open questions

- **Meta-flag parsing cost.** LWW parses the response line for the `t`
  flag on every hit. If profiling shows it on hot paths, switch to a
  lazy accessor without changing the merge function signatures.
- **Base image version.** Write fan-out uses `r:raw_line()` and
  `r:raw_value()` to clone the request per pool. These were added in
  memcached 1.6.40; the Dockerfile pins 1.6.41. Downgrading without
  adjusting the clone path breaks fan-out writes.
- **`write_policy=first` fire-and-forget failure modes.** The
  memcached proxy does not guarantee that a request enqueued on a
  handle we never `wait` on actually completes — connection errors on
  the unwaited pools may be silently swallowed. Document this clearly;
  operators using `first` are explicitly choosing latency over
  durability on the non-primary pools.
- **Deterministic entry ordering across reloads.** The merge contract
  depends on the `read` list order being stable. Reloads that reorder
  pools within a keyspace change `pool-preferred` output. User-visible
  behavior; document it in the Stage 4 keyspace CRD reference. For
  Stage 2 the kind tests pin ordering explicitly.
- **Multi-key `#` is deferred.** The proxy's rqueue slot state machine
  doesn't let a single handle be re-used within one request, and the
  number of sub-keys isn't known at funcgen-build time. Pre-allocating
  a K×M matrix works but inflates idle resources and adds two dispatch
  paths (single-key preserves the client's flags; multi-key synthesises
  a stripped `mg`). For Stage 2 we reject `#` outright with
  `SERVER_ERROR multi-key not supported`; revisit behind sub-funcgens
  when real demand exists.
- **No metrics yet.** Same caveat as Stage 1: if iteration on fan-out
  needs per-pool latency / error counters before Stage 6, add them
  opportunistically in Lua around the fan-out call site. Cheap to add,
  cheap to remove.
