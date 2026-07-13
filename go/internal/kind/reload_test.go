//go:build kind

package kind_test

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// These tests exercise the Stage 4 step-1 deliverable: libmcgateway's
// config watcher SIGHUPs the proxy when the state-dir config (or UDF
// dir) changes, and memcached rebuilds routes without a pod restart.
// They run against any writable-state-dir mode — gateway.liveReload
// or operator mode (the kind suite uses the latter) — and write files
// directly, deliberately bypassing the operator: this is fault
// injection against the watcher machinery itself.

const (
	statePath  = "/var/run/mcgateway/config.lua"
	stateUdf   = "/var/run/mcgateway/udf"
	reloadWait = 45 * time.Second
)

// readGatewayConfig returns the live config file from the gateway pod.
func readGatewayConfig(t *testing.T, ctx context.Context, s *suite, pod string) string {
	t.Helper()
	out, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"cat", statePath}, nil)
	if err != nil {
		t.Fatalf("read %s: %v (stderr: %s)", statePath, err, errOut)
	}
	return out
}

// writeGatewayConfig commits a config the way the operator will: stage
// to a temp file in the same directory, then rename over the target.
func writeGatewayConfig(t *testing.T, ctx context.Context, s *suite, pod, content string) {
	t.Helper()
	script := fmt.Sprintf("cat > %[1]s.tmp && mv %[1]s.tmp %[1]s", statePath)
	_, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"sh", "-ec", script}, []byte(content))
	if err != nil {
		t.Fatalf("write %s: %v (stderr: %s)", statePath, err, errOut)
	}
}

// injectKeyspace splices a keyspace entry at the head of the config's
// keyspace list, leaving everything else byte-identical.
func injectKeyspace(t *testing.T, cfg, entry string) string {
	t.Helper()
	const marker = "keyspaces = {"
	if !strings.Contains(cfg, marker) {
		t.Fatalf("config has no %q marker:\n%s", marker, cfg)
	}
	return strings.Replace(cfg, marker, marker+"\n        "+entry, 1)
}

// waitPrefixRouted polls until a write+read round-trip on the prefix
// succeeds, i.e. the config defining it has been loaded.
func waitPrefixRouted(t *testing.T, addr, prefix string) {
	t.Helper()
	key := uniqueKey(prefix, "probe")
	deadline := time.Now().Add(reloadWait)
	var last string
	for time.Now().Before(deadline) {
		if resp, err := mckind.McSet(addr, key, "v"); err == nil && resp.Status == "HD" {
			if got, err := mckind.McGetWithRetry(addr, key, 3); err == nil && got == "v" {
				return
			}
		} else if err != nil {
			last = err.Error()
		} else {
			last = resp.Line
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("prefix %q not routed within %v (last: %s)", prefix, reloadWait, last)
}

// waitPrefixRejected polls until the prefix stops routing (config that
// defined it has been unloaded). Used by cleanups so tests don't leak
// keyspaces into each other.
func waitPrefixRejected(t *testing.T, addr, prefix string) {
	t.Helper()
	deadline := time.Now().Add(reloadWait)
	for time.Now().Before(deadline) {
		resp, err := mckind.McDo(addr, fmt.Sprintf("mg %s v", uniqueKey(prefix, "gone")), nil)
		if err == nil && resp.Status == "SERVER_ERROR" {
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("prefix %q still routed within %v after restore", prefix, reloadWait)
}

// fallbackLogLine is emitted by gw.load_config when a reload fails and
// the previous config is kept. Tests use it as the positive signal
// that a bad config was actually processed — "old routes still serve"
// alone would pass vacuously if the reload never happened.
const fallbackLogLine = "failed, keeping previous config"

// waitForLogOccurrences polls the gateway container's log until the
// substring appears at least n times.
func waitForLogOccurrences(t *testing.T, ctx context.Context, s *suite, pod, substr string, n int) {
	t.Helper()
	deadline := time.Now().Add(reloadWait)
	last := -1
	for time.Now().Before(deadline) {
		logs, err := mckind.PodLogs(ctx, s.cs, s.ns, pod, "gateway", 500)
		if err == nil {
			last = strings.Count(logs, substr)
			if last >= n {
				return
			}
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("log line %q seen %d times within %v, want >= %d", substr, last, reloadWait, n)
}

func assertNoRestarts(t *testing.T, ctx context.Context, s *suite, pod string, before int32, when string) {
	t.Helper()
	after, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count %s: %v", when, err)
	}
	if after != before {
		t.Fatalf("gateway restarted during %s: restartCount %d -> %d (reload must not bounce the pod)", when, before, after)
	}
}

// TestLiveReload_AddKeyspace is the step-1 headline: commit a config
// with a new keyspace via rename, watch it route with zero restarts.
func TestLiveReload_AddKeyspace(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	orig := readGatewayConfig(t, ctx, s, pod)
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		writeGatewayConfig(t, cctx, s, pod, orig)
		waitPrefixRejected(t, s.gwAddr, "livev1")
	})

	// Precondition: the prefix is unknown.
	resp, err := mckind.McDo(s.gwAddr, "mg livev1:pre v", nil)
	if err != nil {
		t.Fatalf("pre-check: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("pre-check: livev1 already routed: %q", resp.Line)
	}

	next := injectKeyspace(t, orig, `{ prefix = "livev1", read = "mc-a", write = "mc-a" },`)
	writeGatewayConfig(t, ctx, s, pod, next)

	waitPrefixRouted(t, s.gwAddr, "livev1")
	assertNoRestarts(t, ctx, s, pod, restarts0, "live reload")
}

// TestLiveReload_BadConfigKeepsServing pins the fail-safe semantics the
// stage-4 design leans on: a config that fails to load on SIGHUP leaves
// the old routes serving and does not kill the proxy; the watcher stays
// armed and a subsequent good config recovers.
func TestLiveReload_BadConfigKeepsServing(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	orig := readGatewayConfig(t, ctx, s, pod)
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		writeGatewayConfig(t, cctx, s, pod, orig)
		waitPrefixRejected(t, s.gwAddr, "livev2")
	})

	// Syntactically broken: loadfile itself fails. Wait for the
	// fallback log line — positive proof the bad config was processed
	// and rejected, not coalesced away — then prove the old config
	// still serves and nothing crashed.
	writeGatewayConfig(t, ctx, s, pod, "retur { this is not lua\n")
	waitForLogOccurrences(t, ctx, s, pod, fallbackLogLine, 1)
	key := uniqueKey("user", "badcfg")
	if _, err := mckind.McSet(s.gwAddr, key, "still-up"); err != nil {
		t.Fatalf("ms after bad config: %v", err)
	}
	got, err := mckind.McGetWithRetry(s.gwAddr, key, 10)
	if err != nil || got != "still-up" {
		t.Fatalf("mg after bad config: got %q err %v, want still-up", got, err)
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "bad config reload")

	// Semantically broken (validator rejects): same expectations.
	writeGatewayConfig(t, ctx, s, pod, "return { pools = 42 }\n")
	waitForLogOccurrences(t, ctx, s, pod, fallbackLogLine, 2)
	if _, err := mckind.McSet(s.gwAddr, uniqueKey("user", "badcfg2"), "x"); err != nil {
		t.Fatalf("ms after invalid config: %v", err)
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "invalid config reload")

	// The watcher must still be alive: a good config recovers.
	next := injectKeyspace(t, orig, `{ prefix = "livev2", read = "mc-b", write = "mc-b" },`)
	writeGatewayConfig(t, ctx, s, pod, next)
	waitPrefixRouted(t, s.gwAddr, "livev2")
	assertNoRestarts(t, ctx, s, pod, restarts0, "recovery reload")
}

// waitMergeRegistered polls `__mcgw:names` until the merge name's
// presence matches want. The names reply is rebuilt on each reload,
// and every UDF change triggers one via the re-raise.
func waitMergeRegistered(t *testing.T, s *suite, name string, want bool) {
	t.Helper()
	deadline := time.Now().Add(reloadWait)
	for time.Now().Before(deadline) {
		resp, err := mckind.McDo(s.gwAddr, "mg __mcgw:names v", nil)
		if err == nil && resp.Status == "VA" && strings.Contains(resp.Value, name) == want {
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
	t.Fatalf("merge %q registered=%v not observed within %v", name, want, reloadWait)
}

// TestLiveReload_WasmRemovalKeepsServing pins the review's High
// finding: removing a wasm module while the live config references it
// must degrade to per-request errors on that keyspace — not crash the
// proxy. The reload triggered by the removal fails validation
// (unknown merge), falls back to the last good config, and rebuilding
// routes from that fallback must not query the registry (the module
// is gone; a lookup would throw inside the reload lifecycle, which is
// fatal).
func TestLiveReload_WasmRemovalKeepsServing(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	orig := readGatewayConfig(t, ctx, s, pod)
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), time.Minute)
		defer ccancel()
		assertNoRestarts(t, cctx, s, pod, restarts0, "whole test incl. teardown")
	})
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		_, _, _ = mckind.ExecInPod(cctx, s.cs, s.cfg, s.ns, pod, "gateway",
			[]string{"rm", "-f", stateUdf + "/removal_probe.wasm"}, nil)
		writeGatewayConfig(t, cctx, s, pod, orig)
		waitPrefixRejected(t, s.gwAddr, "wrm")
	})

	// Land a module (cloned from the image's baked path; the
	// operator-owned state udf dir starts empty), route a keyspace
	// through it, prove it serves.
	_, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"sh", "-ec", fmt.Sprintf(
			"cp /etc/mcgateway/udf/merge_last_n_wins.wasm %[1]s/.removal_probe.tmp && mv %[1]s/.removal_probe.tmp %[1]s/removal_probe.wasm",
			stateUdf)}, nil)
	if err != nil {
		t.Fatalf("land module: %v (stderr: %s)", err, errOut)
	}
	waitMergeRegistered(t, s, "removal_probe", true)

	next := injectKeyspace(t, orig,
		`{ prefix = "wrm", read = { "mc-a", "mc-b" }, write = { "mc-a", "mc-b" }, merge = "removal_probe" },`)
	writeGatewayConfig(t, ctx, s, pod, next)
	waitPrefixRouted(t, s.gwAddr, "wrm")

	// Remove the module out from under the live config. The reload
	// falls back (log line is the positive signal), the proxy stays
	// up, and every other keyspace keeps serving.
	if _, _, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"rm", stateUdf + "/removal_probe.wasm"}, nil); err != nil {
		t.Fatalf("remove module: %v", err)
	}
	waitForLogOccurrences(t, ctx, s, pod, fallbackLogLine, 1)
	waitMergeRegistered(t, s, "removal_probe", false)

	key := uniqueKey("user", "wasmrm")
	if _, err := mckind.McSet(s.gwAddr, key, "survives"); err != nil {
		t.Fatalf("ms after module removal: %v", err)
	}
	if got, err := mckind.McGetWithRetry(s.gwAddr, key, 10); err != nil || got != "survives" {
		t.Fatalf("mg after module removal: got %q err %v, want survives", got, err)
	}

	// The affected keyspace degrades per-request instead of taking
	// down the gateway: dispatch hits an unknown merge and errors.
	resp, err := mckind.McDo(s.gwAddr, "mg wrm:degraded v", nil)
	if err != nil {
		t.Fatalf("mg on degraded keyspace: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("degraded keyspace: status=%q line=%q, want SERVER_ERROR", resp.Status, resp.Line)
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "module removal under live config")
}

// TestLiveReload_WasmArrivalRevalidatesConfig pins the merge-name
// resolution interlock: a config referencing a module that is not on
// disk is rejected wholesale (has_merge fails), old routes keep
// serving — and when the module lands, the registry-swap re-raise
// re-runs the config without any further config write.
func TestLiveReload_WasmArrivalRevalidatesConfig(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	orig := readGatewayConfig(t, ctx, s, pod)
	// Registered before the restore cleanup so it runs LAST (cleanups
	// are LIFO): the teardown below removes a wasm module while the
	// config still references it — exactly the sequence that used to
	// recreate the fatal reload — so the zero-restarts claim must
	// cover the teardown too, not just the test body.
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), time.Minute)
		defer ccancel()
		assertNoRestarts(t, cctx, s, pod, restarts0, "teardown (module removed while referenced)")
	})
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		_, _, _ = mckind.ExecInPod(cctx, s.cs, s.cfg, s.ns, pod, "gateway",
			[]string{"rm", "-f", stateUdf + "/reraise_probe.wasm"}, nil)
		writeGatewayConfig(t, cctx, s, pod, orig)
		waitPrefixRejected(t, s.gwAddr, "rrl")
	})

	// Config referencing a module that does not exist yet: the whole
	// reload is rejected, old routes must keep serving.
	next := injectKeyspace(t, orig,
		`{ prefix = "rrl", read = { "mc-a", "mc-b" }, write = { "mc-a", "mc-b" }, merge = "reraise_probe" },`)
	writeGatewayConfig(t, ctx, s, pod, next)

	time.Sleep(3 * time.Second)
	if _, err := mckind.McSet(s.gwAddr, uniqueKey("user", "prewasm"), "x"); err != nil {
		t.Fatalf("ms while config pending: %v", err)
	}
	resp, err := mckind.McDo(s.gwAddr, "mg rrl:pre v", nil)
	if err != nil {
		t.Fatalf("mg rrl pre: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("rrl routed before module landed: %q", resp.Line)
	}

	// Land the module (clone one baked into the image under the probe
	// name — the operator-owned state udf dir starts empty). The
	// registry swap must re-raise SIGHUP; the pending config becomes
	// valid and routes — with no further config write.
	_, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"sh", "-ec", fmt.Sprintf(
			"cp /etc/mcgateway/udf/merge_last_n_wins.wasm %[1]s/.reraise_probe.tmp && mv %[1]s/.reraise_probe.tmp %[1]s/reraise_probe.wasm",
			stateUdf)}, nil)
	if err != nil {
		t.Fatalf("land module: %v (stderr: %s)", err, errOut)
	}

	waitPrefixRouted(t, s.gwAddr, "rrl")
	assertNoRestarts(t, ctx, s, pod, restarts0, "wasm-arrival revalidation")
}
