.PHONY: build check test test-kind clean rust-build rust-check generate \
        docker-build kind-up kind-down kind-load \
        helm-install-kind helm-uninstall-kind

# --- Build ---

build: rust-build
	cd go && go build ./...

rust-build:
	cd rust && cargo build --release

# --- Check (read-only, CI gate) ---

# Lua smoke checks run under the host Lua with a fake `mcgateway_native`
# preloaded (see test_routes.lua). The real cdylib is exercised end-to-end
# by test-kind against the gateway image.
LUA_CHECK_PRELUDE = package.path='lua/?.lua;lua/?/init.lua;'..package.path; \
package.preload['mcgateway_native']=function() return { \
  merge=function() return nil end, \
  has_merge=function() return true end, \
  required_flags=function() return '' end, \
  names=function() return {'first-hit','last-write-wins','pool-preferred'} end, \
} end; \
mcp={pool=function() end,backend=function() end,attach=function() end,\
funcgen_new=function() return {new_handle=function() end, ready=function() end} end,\
router_new=function() end,request=function() end,\
CMD_MG=1,CMD_MS=2,CMD_MD=3,WAIT_ANY=0,WAIT_GOOD=1,\
MCMC_CODE_STORED=8,MCMC_CODE_DELETED=10,MCMC_CODE_OK=15}

check: rust-check
	@if command -v lua >/dev/null 2>&1; then \
	  lua -e "$(LUA_CHECK_PRELUDE); require('mcgateway')" && \
	  cd lua && lua tests/test_entries.lua && lua tests/test_routes.lua && \
	  lua tests/test_config_fallback.lua && lua tests/test_rendered_config.lua; \
	else \
	  echo "skip lua tests (lua not installed)"; \
	fi
	@$(MAKE) --no-print-directory generate && \
	  test -z "$$(git status --porcelain -- go/api k8s/charts/mcgateway/crds)" || \
	  { echo "error: generated files drifted; run 'make generate' and commit the result"; exit 1; }
	cd go && go vet -tags kind ./...

# Regenerates deepcopy methods and CRD manifests from kubebuilder
# markers in go/api/v1alpha1/. `go run pkg@version` pins the tool for
# every invoker (no PATH setup, no CI install step) and keeps the
# drift check in `check` comparing stable output. The check uses
# `git status --porcelain`, not `git diff`, so brand-new (untracked)
# generated files fail it too.
CONTROLLER_GEN_VERSION = v0.21.0
CONTROLLER_GEN = go run sigs.k8s.io/controller-tools/cmd/controller-gen@$(CONTROLLER_GEN_VERSION)

generate:
	cd go && $(CONTROLLER_GEN) object paths=./api/v1alpha1/...
	cd go && $(CONTROLLER_GEN) crd paths=./api/v1alpha1/... output:crd:dir=../k8s/charts/mcgateway/crds

rust-check:
	cd rust && cargo test && cargo clippy --all-targets -- -D warnings

# --- Test ---

test: check test-kind

test-kind: helm-install-kind
	kind export kubeconfig --name $(KIND_CLUSTER_NAME)
	cd go && MCGATEWAY_IMAGE=$(MCGATEWAY_IMAGE) go test -tags kind -count=1 -v ./...

# --- Clean ---

clean:
	rm -rf go/bin

# --- KIND / Container ---

KIND_CLUSTER_NAME ?= mcgateway
KIND_PROVIDER     ?= $(shell [ -n "$$GITHUB_ACTIONS" ] && echo docker || \
                       (command -v podman >/dev/null 2>&1 && echo podman || echo docker))
MCGATEWAY_IMAGE   ?= $(shell [ "$(KIND_PROVIDER)" = "podman" ] && echo localhost/mcgateway:dev || echo mcgateway:dev)
# Split on the *last* colon only so registries with a port survive:
# `localhost:5001/mcgateway:dev` -> repo=localhost:5001/mcgateway, tag=dev.
MCGATEWAY_IMAGE_REPO = $(shell printf '%s' '$(MCGATEWAY_IMAGE)' | sed -e 's/:[^:]*$$//')
MCGATEWAY_IMAGE_TAG  = $(shell printf '%s' '$(MCGATEWAY_IMAGE)' | sed -e 's/^.*://')

export KIND_EXPERIMENTAL_PROVIDER = $(KIND_PROVIDER)

# Extra flags injected by CI (buildx --load and gha layer-cache flags).
DOCKER_BUILD_ARGS ?=

docker-build:
	$(KIND_PROVIDER) build $(DOCKER_BUILD_ARGS) -t $(MCGATEWAY_IMAGE) -f docker/Dockerfile .

kind-up:
	kind create cluster --name $(KIND_CLUSTER_NAME) --config k8s/kind/cluster.yaml

kind-down:
	kind delete cluster --name $(KIND_CLUSTER_NAME)

# `kind load docker-image` shells out to `docker image inspect` to verify the
# image exists locally — even with the podman provider. When only podman is
# installed (no `docker` CLI), this always fails. Work around by saving to a
# tarball and loading via `kind load image-archive`.
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
