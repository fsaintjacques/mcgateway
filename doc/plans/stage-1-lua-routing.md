# Stage 1 — Lua Routing, Single-Pool Passthrough

First deliverable from the [implementation plan](implementation-plan.md).
Ship a gateway that accepts memcache traffic, resolves a keyspace by prefix,
and forwards the request to a single backend pool. No fan-out, no merge, no
WASM, no operator.

---

## Goal

A running `memcached` proxy pod that, given a static configuration of pools
and keyspaces, correctly routes each memcache meta-protocol request
(`mg`/`ms`/`md`) to a single backend pool based on the key's prefix.

This stage exists to prove:

- The Lua library structure and packaging.
- Configuration loading and reload.
- memcached proxy lifecycle integration
  (`mcp_config_pools`, `mcp_config_routes`).
- Docker image layout.

Everything subsequent (fan-out, merge, FFI, operator) builds on the structure
established here.

---

## Non-goals

- Fan-out or merge logic — deferred to Stage 2.
- `libmcgateway.so` or any Rust code — deferred to Stage 2.
- Kubernetes operator, CRDs, or any control plane — deferred to Stage 4.
- WASM or user-defined functions — deferred to Stage 3.
- `write_policy`, multi-key `#` splitting, `merge_on_write` — deferred to
  Stage 2.
- Observability beyond what memcached's built-in stats provide — deferred to
  Stage 6.

---

## Scope

Exactly the following is in scope for Stage 1:

| Feature | In scope |
|---|---|
| Pool registry (`name`, `addrs`) | ✓ |
| Pool `hash` / `dist` options | ✓ (passed through to `mcp.pool`) |
| Keyspace registry (`prefix`, single `read` pool, single `write` pool) | ✓ |
| Prefix-based key routing | ✓ |
| `mg` / `ms` / `md` forwarding | ✓ |
| Reserved `__udf:` prefix (rejected explicitly for now) | ✓ |
| Config reload via memcached admin socket | ✓ |
| Unit tests for key parsing and route dispatch | ✓ |
| Docker image (`memcached` + Lua library) | ✓ |

Anything not in this table is out of scope.

---

## Project layout

Matches the mcfreeze repo shape — polyglot monorepo with `lua/`, `go/`,
`docker/`, `k8s/charts/`, `k8s/kind/`, and a top-level `Makefile`.

```
lua/
  mcgateway.lua              top-level module: require("mcgateway")
  mcgateway/
    config.lua               load/validate config file
    pools.lua                pool registry
    keyspaces.lua            keyspace registry, prefix resolver
    routes.lua               mg / ms / md handlers
    util.lua                 helpers (key parsing, logging)
  examples/
    proxy.lua                example user entry point
    config.lua               example config file
  tests/
    test_keyspaces.lua       Lua unit tests (prefix resolution)
    test_config.lua          Lua unit tests (config validation)
    run.sh                   plain lua 5.4 test driver

go/
  go.mod
  go.sum
  internal/
    kind/
      kind_test.go           //go:build kind — end-to-end tests
      helpers.go             clientset, namespace, port-forward helpers
    testutil/                shared fixtures

docker/
  Dockerfile                 memcached + lua library

k8s/
  kind/
    cluster.yaml             single-node kind cluster config
  charts/
    mcgateway/
      Chart.yaml
      values.yaml
      values-kind.yaml       kind-specific overrides
      templates/
        configmap.yaml       mounts config.lua
        deployment.yaml      gateway Deployment (stage 1: no operator)
        backends.yaml        two memcached Deployments for tests
        service.yaml         gateway + backend Services

Makefile                     build / test / kind lifecycle
```

No Rust code, no CRDs, no operator in Stage 1 — the `go/` tree contains only
the kind integration tests, and the Helm chart is a minimal manifest set
that stands up a gateway pod with a ConfigMap-provided config. Both expand in
later stages.

---

## Library API

`lua/mcgateway.lua` is the public surface. The user's `proxy.lua` should look
like this:

```lua
-- examples/proxy.lua
local gw = require("mcgateway")

gw.load_config("/etc/mcgateway/config.lua")

function mcp_config_pools()
    return gw.build_pools()
end

function mcp_config_routes()
    return gw.build_routes()
end
```

Module functions:

| Function | Purpose |
|---|---|
| `gw.load_config(path)` | Read and validate a config file from disk |
| `gw.build_pools()` | Create `mcp.pool()` objects for each pool in config |
| `gw.build_routes()` | Return the memcached proxy route table |
| `gw.reload()` | Re-read the config file; memcached will re-invoke the lifecycle hooks on admin-socket reload |

Internal modules (`mcgateway.config`, `mcgateway.pools`, etc.) are not
re-exported. Users should only use the top-level `gw.*` functions.

---

## Configuration format

Use Lua itself as the config format. memcached already loads Lua; no YAML
parser is needed. A config file is a pure-data Lua table returned from the
file:

```lua
-- examples/config.lua
return {
    pools = {
        {
            name = "frostmap",
            addrs = { "frostmap:11211" },
        },
        {
            name = "mc-cluster",
            addrs = { "mc1:11211", "mc2:11211", "mc3:11211" },
            hash = "xxhash",
            dist = "ring_hash",
        },
    },

    keyspaces = {
        {
            prefix = "user",
            read   = "frostmap",     -- single pool name for Stage 1
            write  = "mc-cluster",
        },
        {
            prefix = "session",
            read   = "mc-cluster",
            write  = "mc-cluster",
        },
    },
}
```

`gw.load_config` reads this file via `dofile`, validates shape, and stores it
in a module-level table. Validation:

- Every pool has a unique `name` and a non-empty `addrs` list.
- `hash` ∈ `{"xxhash", "md5", "crc32"}` or nil.
- `dist` ∈ `{"ring_hash", "jump_hash"}` or nil.
- Every keyspace has a unique `prefix`, a `read` pool name, a `write` pool
  name, and both names exist in `pools`.
- `prefix` does not contain `:` (the prefix-delimiter), is not empty, and
  is not the reserved `__udf`.

Validation failures raise a Lua error with a line-identifying message, which
memcached surfaces in its startup log.

---

## Key routing

Keys have the form `<prefix>:<rest>`. `gw.keyspaces.resolve(key)` splits on
the first `:` and returns the matching keyspace or `nil`.

```lua
-- mcgateway/keyspaces.lua (sketch)
function M.resolve(key)
    local colon = key:find(":", 1, true)
    if not colon then return nil end
    local prefix = key:sub(1, colon - 1)
    return M._by_prefix[prefix]
end
```

If resolution fails (no matching prefix, or key has no `:`), the route
handler returns `SERVER_ERROR unknown keyspace\r\n`. No default pool — Stage
1 requires every served key to match a declared prefix.

The reserved `__udf:` prefix returns `SERVER_ERROR udf not supported\r\n`.
This is a deliberate placeholder so clients that mistakenly try `__udf:`
operations get a clear error now; Stage 3 will replace the handler.

---

## Route handlers

Three handlers, one per verb:

```lua
-- mcgateway/routes.lua (sketch)
function M.handle_mg(r)
    local ks = keyspaces.resolve(r:key())
    if not ks then return "SERVER_ERROR unknown keyspace\r\n" end
    return mcp.await_logreqs(r, ks.read_pool)
end

function M.handle_ms(r)
    local ks = keyspaces.resolve(r:key())
    if not ks then return "SERVER_ERROR unknown keyspace\r\n" end
    return mcp.await_logreqs(r, ks.write_pool)
end

function M.handle_md(r)
    local ks = keyspaces.resolve(r:key())
    if not ks then return "SERVER_ERROR unknown keyspace\r\n" end
    return mcp.await_logreqs(r, ks.write_pool)
end
```

The handlers are deliberately thin — all routing logic is in `resolve`. This
structure survives into later stages: Stage 2 adds the fan-out and merge
steps inside these same handlers.

The route table returned from `mcp_config_routes` uses the memcached proxy
command constants:

```lua
return {
    cmd_default = handle_passthrough, -- reject by default; only MG/MS/MD land here
    [mcp.CMD_MG] = M.handle_mg,
    [mcp.CMD_MS] = M.handle_ms,
    [mcp.CMD_MD] = M.handle_md,
}
```

---

## Lifecycle

```
memcached starts with -o proxy_config=/etc/mcgateway/proxy.lua
  → proxy.lua runs: require("mcgateway"), load_config(...)
  → memcached calls mcp_config_pools() → gw.build_pools()
  → memcached calls mcp_config_routes() → gw.build_routes()
  → serving starts

admin: echo reload | nc -U <admin socket>
  → memcached re-invokes mcp_config_pools() and mcp_config_routes()
  → new pools and routes atomically replace the old ones
  → in-flight requests drain against old pools
```

Reload semantics are entirely memcached's — this stage just needs to make
sure `build_pools` and `build_routes` are idempotent and safe to call
multiple times.

---

## Docker image

```dockerfile
# docker/Dockerfile
FROM memcached:1.6.32

USER root
RUN mkdir -p /etc/mcgateway /usr/local/share/lua/5.4/mcgateway

COPY lua/mcgateway.lua         /usr/local/share/lua/5.4/
COPY lua/mcgateway/            /usr/local/share/lua/5.4/mcgateway/
COPY lua/examples/proxy.lua    /etc/mcgateway/proxy.lua
COPY lua/examples/config.lua   /etc/mcgateway/config.lua

USER memcache
EXPOSE 11211

CMD ["memcached", "-o", "proxy_config=/etc/mcgateway/proxy.lua", "-l", "0.0.0.0"]
```

Image size target: same as the base `memcached` image plus ~10KB of Lua.

---

## Testing

Two layers: Lua unit tests for in-process logic, Go integration tests in a
`kind` cluster for end-to-end behavior. `docker-compose` is not used —
kind covers the same ground and reuses the infrastructure we need in later
stages anyway.

### Lua unit tests

Run in a plain Lua 5.4 interpreter with a stubbed `mcp` namespace. They cover:

- Key parsing and prefix resolution (hit, miss, empty key, missing `:`).
- Config validation (well-formed, duplicate names, unknown pool reference,
  reserved prefix).
- Handler dispatch given a mocked request object.

`lua/tests/run.sh` sets `LUA_PATH` to point at `lua/` and invokes each
`test_*.lua` file. Exit non-zero on any failure. No external dependencies
beyond `lua5.4`.

Invoked by `make test-unit`.

### Go integration tests (kind)

`go/internal/kind/kind_test.go` has the `//go:build kind` tag so it only runs
when explicitly requested via `go test -tags kind`. Flow:

1. Load kubeconfig for the local kind cluster (from `KUBECONFIG` or
   `~/.kube/config` after `kind export kubeconfig`).
2. Create a fresh namespace per test (e.g., `stage1-<timestamp>`).
3. Wait for the Helm-installed gateway Deployment and backend Deployments to
   be ready.
4. `portforward` to the gateway Service port 11211 via `client-go`'s
   `portforward` package.
5. Speak the memcache meta protocol over the forwarded connection (plain
   `net.Dial` once the tunnel is up).
6. Assert routing behavior directly via protocol calls, AND assert the
   data landed on the expected backend by port-forwarding to each backend
   Service and inspecting it independently.

Test cases for Stage 1:

| Test | Assertion |
|---|---|
| `TestKeyspaceRouting_Write` | `ms user:foo bar` lands on the `mc-a` backend and is absent from `mc-b` |
| `TestKeyspaceRouting_Read` | `mg user:foo v` returns the value written via the gateway |
| `TestKeyspaceRouting_Other` | `mg session:bar v` routes to `mc-b`, not `mc-a` |
| `TestUnknownPrefix` | `mg unknown:x v` returns `SERVER_ERROR unknown keyspace` |
| `TestUdfPrefixRejected` | `ms __udf:foo 1\r\nx\r\n` returns `SERVER_ERROR udf not supported` |
| `TestConfigReload` | Patch the gateway ConfigMap with a new keyspace, send `reload` via kubectl exec, verify the new prefix resolves |

The kind tests borrow the client-go / port-forward pattern directly from
mcfreeze (`go/internal/controlplane/kind_e2e_test.go`) — same helpers for
namespace creation, deployment waiting, and port-forwarding — so the Go
scaffolding reads as a simplified subset of what mcfreeze already has.

Invoked by `make test-kind`, which also builds the image and installs the
chart into a running kind cluster (see the Makefile section).

### Makefile

The top-level `Makefile` mirrors mcfreeze's structure and target names.
Stage 1 needs a small subset; later stages add to it.

```make
.PHONY: build test test-unit test-kind clean \
        docker-build kind-up kind-down kind-load \
        helm-install-kind helm-uninstall-kind

# --- Build ---

build:
	cd go && go build ./...

# --- Test ---

test: test-unit test-kind

test-unit:
	bash lua/tests/run.sh
	cd go && go test ./...

test-kind: helm-install-kind
	kind export kubeconfig --name $(KIND_CLUSTER_NAME)
	cd go && MCGATEWAY_IMAGE=$(MCGATEWAY_IMAGE) go test -tags kind -count=1 -v ./...

# --- Clean ---

clean:
	rm -rf go/bin

# --- KIND / Container ---

KIND_CLUSTER_NAME ?= mcgateway
KIND_PROVIDER     ?= $(shell command -v podman >/dev/null 2>&1 && echo podman || echo docker)
MCGATEWAY_IMAGE   ?= $(shell [ "$(KIND_PROVIDER)" = "podman" ] && echo localhost/mcgateway:dev || echo mcgateway:dev)
MCGATEWAY_IMAGE_REPO = $(firstword $(subst :, ,$(MCGATEWAY_IMAGE)))
MCGATEWAY_IMAGE_TAG  = $(lastword  $(subst :, ,$(MCGATEWAY_IMAGE)))

export KIND_EXPERIMENTAL_PROVIDER = $(KIND_PROVIDER)

docker-build:
	$(KIND_PROVIDER) build -t $(MCGATEWAY_IMAGE) -f docker/Dockerfile .

kind-up:
	kind create cluster --name $(KIND_CLUSTER_NAME) --config k8s/kind/cluster.yaml

kind-down:
	kind delete cluster --name $(KIND_CLUSTER_NAME)

# Same podman workaround as mcfreeze: `kind load docker-image` shells out to
# `docker image inspect` and fails when only podman is installed. Work around
# via save-to-tarball + load-from-archive.
kind-load: docker-build
ifeq ($(KIND_PROVIDER),podman)
	podman save $(MCGATEWAY_IMAGE) -o /tmp/mcgateway-kind.tar
	kind load image-archive /tmp/mcgateway-kind.tar --name $(KIND_CLUSTER_NAME)
	rm -f /tmp/mcgateway-kind.tar
else
	kind load docker-image $(MCGATEWAY_IMAGE) --name $(KIND_CLUSTER_NAME)
endif

HELM_RELEASE_NAME ?= mcgateway
HELM_RELEASE_NS   ?= mcgateway-system

helm-install-kind: kind-load
	helm upgrade --install $(HELM_RELEASE_NAME) ./k8s/charts/mcgateway \
		-n $(HELM_RELEASE_NS) --create-namespace \
		-f ./k8s/charts/mcgateway/values-kind.yaml \
		--set image.repository=$(MCGATEWAY_IMAGE_REPO) \
		--set image.tag=$(MCGATEWAY_IMAGE_TAG) \
		--wait

helm-uninstall-kind:
	helm uninstall $(HELM_RELEASE_NAME) -n $(HELM_RELEASE_NS) --ignore-not-found
```

Developer loop:

```bash
make kind-up                 # one-time per dev session
make test-kind               # build image, load, install chart, run Go tests
```

Stage 1 adds `test-unit`, `test-kind`, and the KIND/helm targets. Stages 2+
add `format`, `lint`, `check`, `generate` (CRD codegen), and eventually
`test-gke`, mirroring mcfreeze's full Makefile shape as the project grows.

---

## Exit criteria

Stage 1 is done when all of the following are true:

1. `make kind-up && make test-kind` is green on a fresh machine (Linux or
   macOS, with either docker or podman).
2. Lua unit tests pass via `make test-unit`: config validation and prefix
   resolution covered.
3. Go kind tests pass: all six cases in the table above — per-keyspace
   routing for reads/writes, unknown prefix rejection, reserved `__udf:`
   rejection, and ConfigMap-driven config reload.
4. The Helm chart installs cleanly on kind and produces a ready gateway
   pod plus two backend pods within one minute.
5. `make docker-build` produces an image that runs on both docker and
   podman.

Once all five hold, Stage 2 can begin.

---

## Risks and open questions

- **Lua 5.4 vs LuaJIT.** memcached proxy embeds Lua 5.4. The library must
  not accidentally depend on LuaJIT extensions. Stick to pure Lua 5.4.
- **`mcp.await_logreqs` return shape.** With a single pool argument (not a
  list) the return value is the response object, not an array. The handlers
  above rely on this; confirm against a running memcached 1.6.32.
- **Admin socket availability.** If the deployment environment doesn't
  expose memcached's admin socket (some managed setups don't), config
  reload needs a fallback. For Stage 1 the kind tests trigger reload via
  `kubectl exec` into the gateway pod and `nc` against the admin socket;
  the operator in Stage 4 will revisit.
- **No metrics yet.** Stage 1 relies entirely on memcached's built-in
  `stats` output. If iteration surfaces a need for per-keyspace counters
  before Stage 6, introduce them opportunistically — they're cheap to add
  to the Lua handlers.
