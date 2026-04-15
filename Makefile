.PHONY: build check test test-kind clean rust-build rust-check \
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
	@command -v lua >/dev/null 2>&1 && { \
	  lua -e "$(LUA_CHECK_PRELUDE); require('mcgateway')" && \
	  cd lua && lua tests/test_entries.lua && lua tests/test_routes.lua; \
	} || echo "skip lua tests (lua not installed)"
	cd go && go vet -tags kind ./...

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
MCGATEWAY_IMAGE_REPO = $(firstword $(subst :, ,$(MCGATEWAY_IMAGE)))
MCGATEWAY_IMAGE_TAG  = $(lastword  $(subst :, ,$(MCGATEWAY_IMAGE)))

export KIND_EXPERIMENTAL_PROVIDER = $(KIND_PROVIDER)

docker-build:
	$(KIND_PROVIDER) build -t $(MCGATEWAY_IMAGE) -f docker/Dockerfile .

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
