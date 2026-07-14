//go:build kind

package kind_test

import (
	"context"
	"fmt"
	"strconv"
	"strings"
	"testing"
	"time"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// These tests exercise the stage-6 deliverable: the gateway's
// /metrics exposition (libmcgateway, :9151) and the operator's
// (controller-runtime, :8080). Scrapes run from inside the gateway
// container via busybox wget — the operator is a native sidecar in
// the same pod, so its (distroless, shell-less) endpoint is reachable
// over the shared pod network.

const (
	gatewayMetricsPort  = 9151
	operatorMetricsPort = 8080
	metricsWait         = 60 * time.Second
)

func scrapeMetrics(ctx context.Context, s *suite, pod string, port int) (string, error) {
	url := fmt.Sprintf("http://127.0.0.1:%d/metrics", port)
	out, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"wget", "-q", "-O-", url}, nil)
	if err != nil {
		return "", fmt.Errorf("scrape %s: %w (stderr: %s)", url, err, errOut)
	}
	return out, nil
}

// metricValue finds the sample for an exact series (name plus full
// label set, as pinned by the rust exposition golden) and returns its
// value. Series absence is not an error: families encode only after
// their first observation.
func metricValue(exposition, series string) (float64, bool) {
	for _, line := range strings.Split(exposition, "\n") {
		rest, ok := strings.CutPrefix(line, series+" ")
		if !ok {
			continue
		}
		v, err := strconv.ParseFloat(strings.TrimSpace(rest), 64)
		if err != nil {
			return 0, false
		}
		return v, true
	}
	return 0, false
}

// waitForMetric polls a pod's exposition until pred holds for the
// series. Metrics lag traffic by nothing (same process), but operator
// metrics lag CR changes by a reconcile, and scrapes can race pod
// startup — polling absorbs both.
func waitForMetric(t *testing.T, ctx context.Context, s *suite, pod string, port int,
	series string, pred func(float64) bool) {
	t.Helper()
	deadline := time.Now().Add(metricsWait)
	last := "never scraped"
	for time.Now().Before(deadline) {
		exposition, err := scrapeMetrics(ctx, s, pod, port)
		if err != nil {
			last = err.Error()
		} else if v, ok := metricValue(exposition, series); ok && pred(v) {
			return
		} else {
			last = fmt.Sprintf("series %q value %v (present=%v)", series, v, ok)
		}
		time.Sleep(2 * time.Second)
	}
	t.Fatalf("metric %q did not satisfy predicate within %v (last: %s)", series, metricsWait, last)
}

// TestMetricsDataPath drives traffic across a passthrough keyspace, a
// fan-out keyspace, both write outcomes reachable without fault
// injection, and an unknown prefix — then asserts every data-path
// family moved with the right labels.
func TestMetricsDataPath(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)

	// Traffic. Failures of the unknown-prefix probe are the point.
	key := uniqueKey("user", "metrics")
	if _, err := mckind.McSet(s.gwAddr, key, "v"); err != nil {
		t.Fatalf("set user: %v", err)
	}
	if _, err := mckind.McGetWithRetry(s.gwAddr, key, 5); err != nil {
		t.Fatalf("get user: %v", err)
	}
	fanKey := uniqueKey("fanfh", "metrics")
	if _, err := mckind.McSet(s.gwAddr, fanKey, "v"); err != nil {
		t.Fatalf("set fanfh: %v", err)
	}
	if _, err := mckind.McGetWithRetry(s.gwAddr, fanKey, 5); err != nil {
		t.Fatalf("get fanfh: %v", err)
	}
	if _, err := mckind.McDo(s.gwAddr, "mg notaks:x v", nil); err != nil {
		t.Fatalf("unknown prefix probe: %v", err)
	}

	exposition, err := scrapeMetrics(ctx, s, pod, gatewayMetricsPort)
	if err != nil {
		t.Fatal(err)
	}

	atLeastOne := []string{
		// Request outcomes, read and write, per keyspace.
		`mcgateway_requests_total{keyspace="user",op="read",outcome="hit"}`,
		`mcgateway_requests_total{keyspace="user",op="write",outcome="stored"}`,
		`mcgateway_requests_total{keyspace="fanfh",op="read",outcome="hit"}`,
		// Unknown prefixes bucket into the fixed sentinel, never a label.
		`mcgateway_requests_total{keyspace="__unknown__",op="read",outcome="error"}`,
		// Per-pool status counters and latency histograms: the fan-out
		// read touched both pools.
		`mcgateway_backend_requests_total{pool="mc-a",status="hit"}`,
		`mcgateway_backend_duration_seconds_count{pool="mc-a"}`,
		`mcgateway_backend_duration_seconds_count{pool="mc-b"}`,
		// Merge and request durations.
		`mcgateway_merge_duration_seconds_count{merge="first-hit"}`,
		`mcgateway_request_duration_seconds_count{keyspace="user",op="read"}`,
	}
	for _, series := range atLeastOne {
		v, ok := metricValue(exposition, series)
		if !ok || v < 1 {
			t.Errorf("series %q = %v (present=%v), want >= 1", series, v, ok)
		}
	}
	if t.Failed() {
		t.Logf("exposition:\n%s", exposition)
	}

	// Cardinality: the unknown prefix itself must not appear as a
	// label value anywhere.
	if strings.Contains(exposition, "notaks") {
		t.Fatalf("request-derived prefix leaked into a label:\n%s", exposition)
	}
}

// TestMetricsReloadFallback pairs the stage-4 survival test with its
// stage-6 observability: the same fault injection that proves "bad
// reload keeps serving" must increment the fallback counter — the
// alertable form of that stderr line.
func TestMetricsReloadFallback(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)

	orig := readGatewayConfig(t, ctx, s, pod)
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		writeGatewayConfig(t, cctx, s, pod, orig)
	})

	before, _ := func() (float64, bool) {
		exposition, err := scrapeMetrics(ctx, s, pod, gatewayMetricsPort)
		if err != nil {
			return 0, false
		}
		return metricValue(exposition, `mcgateway_config_reloads_total{result="fallback"}`)
	}()
	// Baseline the log count too: earlier tests against the same pod
	// (the stage-4 fallback test) may already have produced the line,
	// and waiting for the first occurrence would then synchronize on
	// nothing.
	logBaseline := 0
	if logs, err := mckind.PodLogs(ctx, s.cs, s.ns, pod, "gateway", 500); err == nil {
		logBaseline = strings.Count(logs, fallbackLogLine)
	}

	writeGatewayConfig(t, ctx, s, pod, "retur { this is not lua\n")
	waitForLogOccurrences(t, ctx, s, pod, fallbackLogLine, logBaseline+1)

	waitForMetric(t, ctx, s, pod, gatewayMetricsPort,
		`mcgateway_config_reloads_total{result="fallback"}`,
		func(v float64) bool { return v > before })

	// The gauges keep describing the serving (last good) config.
	exposition, err := scrapeMetrics(ctx, s, pod, gatewayMetricsPort)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := metricValue(exposition, "mcgateway_config_keyspaces"); !ok || v < 1 {
		t.Fatalf("config_keyspaces = %v (present=%v) during fallback, want >= 1", v, ok)
	}
}

// TestOperatorMetrics asserts the render-warnings gauge is the
// level-triggered signal the plan promises: nonzero while a bad CR
// exists, zero again once it is deleted.
func TestOperatorMetrics(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()
	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	cl := mckind.CRClient(t, s.cfg)

	// The manager's built-ins ride the same endpoint.
	exposition, err := scrapeMetrics(ctx, s, pod, operatorMetricsPort)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(exposition, "controller_runtime_reconcile_total") {
		t.Fatalf("controller-runtime built-ins missing from operator exposition:\n%s", exposition)
	}

	bad := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "metricsbad", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "metricsbad",
			Read:   []string{"no-such-pool"},
			Write:  []string{"no-such-pool"},
		},
	}
	if err := cl.Create(ctx, bad); err != nil {
		t.Fatalf("create bad keyspace: %v", err)
	}
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), time.Minute)
		defer ccancel()
		if err := cl.Delete(cctx, bad); err != nil && !apierrors.IsNotFound(err) {
			t.Errorf("delete bad keyspace: %v", err)
		}
	})

	waitForMetric(t, ctx, s, pod, operatorMetricsPort,
		"mcgateway_operator_render_warnings",
		func(v float64) bool { return v >= 1 })

	if err := cl.Delete(ctx, bad); err != nil {
		t.Fatalf("delete bad keyspace: %v", err)
	}
	waitForMetric(t, ctx, s, pod, operatorMetricsPort,
		"mcgateway_operator_render_warnings",
		func(v float64) bool { return v == 0 })

	// Commits succeeded throughout; snapshot gauges are labeled.
	exposition, err = scrapeMetrics(ctx, s, pod, operatorMetricsPort)
	if err != nil {
		t.Fatal(err)
	}
	if v, ok := metricValue(exposition, `mcgateway_operator_commits_total{result="ok"}`); !ok || v < 1 {
		t.Fatalf("ok commits = %v (present=%v), want >= 1", v, ok)
	}
	if _, ok := metricValue(exposition, `mcgateway_operator_snapshot_objects{kind="keyspace"}`); !ok {
		t.Fatal("snapshot keyspace gauge missing")
	}
}
