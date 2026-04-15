//go:build kind

package kind_test

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"

	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

const (
	gatewaySelector = "app.kubernetes.io/name=mcgateway,app.kubernetes.io/component=gateway"
	mcASelector     = "mcgateway.io/backend=mc-a"
	mcBSelector     = "mcgateway.io/backend=mc-b"
)

// suite holds per-test-file infra: port-forwards to gateway and both backends.
type suite struct {
	cs       kubernetes.Interface
	cfg      *rest.Config
	ns       string
	gwAddr   string
	mcAAddr  string
	mcBAddr  string
}

func newSuite(t *testing.T) *suite {
	t.Helper()
	cs, cfg := mckind.ClientAndConfig(t)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
	defer cancel()

	ns := mckind.ReleaseNamespace
	mckind.WaitForDeploymentReady(t, ctx, cs, ns, "mcgateway", 2*time.Minute)
	mckind.WaitForDeploymentReady(t, ctx, cs, ns, "mc-a", 2*time.Minute)
	mckind.WaitForDeploymentReady(t, ctx, cs, ns, "mc-b", 2*time.Minute)

	gwPort := mckind.PortForwardPod(t, ctx, cs, cfg, ns, gatewaySelector, 11211)
	mcAPort := mckind.PortForwardPod(t, ctx, cs, cfg, ns, mcASelector, 11211)
	mcBPort := mckind.PortForwardPod(t, ctx, cs, cfg, ns, mcBSelector, 11211)

	return &suite{
		cs: cs, cfg: cfg, ns: ns,
		gwAddr:  mckind.Addr(gwPort),
		mcAAddr: mckind.Addr(mcAPort),
		mcBAddr: mckind.Addr(mcBPort),
	}
}

// uniqueKey appends a timestamp to make per-test keys distinct across runs.
func uniqueKey(prefix, suffix string) string {
	return fmt.Sprintf("%s:%s-%d", prefix, suffix, time.Now().UnixNano()%1000000)
}

func TestKeyspaceRouting_WriteAndRead(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("user", "wr")
	val := "hello-mc-a"

	// Write via gateway — should land on mc-a.
	resp, err := mckind.McSet(s.gwAddr, key, val)
	if err != nil {
		t.Fatalf("ms via gateway: %v", err)
	}
	if resp.Status != "HD" && resp.Status != "OK" {
		t.Fatalf("ms via gateway: got %q, want HD/OK", resp.Line)
	}

	// Read via gateway — round-trip.
	got, err := mckind.McGetWithRetry(s.gwAddr, key, 5)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}
	if got != val {
		t.Fatalf("mg via gateway: got %q, want %q", got, val)
	}

	// Value must be on mc-a.
	gotA, err := mckind.McGetWithRetry(s.mcAAddr, key, 3)
	if err != nil {
		t.Fatalf("mg directly from mc-a: %v", err)
	}
	if gotA != val {
		t.Fatalf("mg mc-a: got %q, want %q", gotA, val)
	}

	// And absent on mc-b.
	respB, err := mckind.McDo(s.mcBAddr, fmt.Sprintf("mg %s v", key), nil)
	if err != nil {
		t.Fatalf("mg mc-b: %v", err)
	}
	if respB.Status == "VA" {
		t.Fatalf("mc-b unexpectedly has key %q", key)
	}
}

func TestKeyspaceRouting_Other(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("session", "other")
	val := "only-on-mc-b"

	if _, err := mckind.McSet(s.gwAddr, key, val); err != nil {
		t.Fatalf("ms via gateway: %v", err)
	}

	gotB, err := mckind.McGetWithRetry(s.mcBAddr, key, 5)
	if err != nil {
		t.Fatalf("mg mc-b: %v", err)
	}
	if gotB != val {
		t.Fatalf("mg mc-b: got %q, want %q", gotB, val)
	}

	respA, err := mckind.McDo(s.mcAAddr, fmt.Sprintf("mg %s v", key), nil)
	if err != nil {
		t.Fatalf("mg mc-a: %v", err)
	}
	if respA.Status == "VA" {
		t.Fatalf("mc-a unexpectedly has session key %q", key)
	}
}

func TestUnknownPrefix(t *testing.T) {
	s := newSuite(t)
	resp, err := mckind.McDo(s.gwAddr, "mg unknown:x v", nil)
	if err != nil {
		t.Fatalf("mg unknown via gateway: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("status=%q line=%q, want SERVER_ERROR", resp.Status, resp.Line)
	}
	if !strings.Contains(resp.Line, "unknown keyspace") {
		t.Fatalf("line=%q, want to contain 'unknown keyspace'", resp.Line)
	}
}

func TestUdfPrefixRejected(t *testing.T) {
	s := newSuite(t)
	resp, err := mckind.McDo(s.gwAddr, "ms __udf:foo 1", []byte("x"))
	if err != nil {
		t.Fatalf("ms __udf: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("status=%q line=%q, want SERVER_ERROR", resp.Status, resp.Line)
	}
	if !strings.Contains(resp.Line, "udf not supported") {
		t.Fatalf("line=%q, want to contain 'udf not supported'", resp.Line)
	}
}

func TestConfigReload(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	newConfig := `return {
    pools = {
        { name = "mc-a", addrs = { "mc-a:11211" } },
        { name = "mc-b", addrs = { "mc-b:11211" } },
    },
    keyspaces = {
        { prefix = "user",    read = "mc-a", write = "mc-a" },
        { prefix = "session", read = "mc-b", write = "mc-b" },
        { prefix = "addedv2", read = "mc-b", write = "mc-b" },
    },
}
`

	cm, err := s.cs.CoreV1().ConfigMaps(s.ns).Get(ctx, "mcgateway-config", metav1.GetOptions{})
	if err != nil {
		t.Fatalf("get configmap: %v", err)
	}
	orig := cm.Data["config.lua"]
	t.Cleanup(func() {
		restoreCtx, restoreCancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer restoreCancel()
		cur, err := s.cs.CoreV1().ConfigMaps(s.ns).Get(restoreCtx, "mcgateway-config", metav1.GetOptions{})
		if err != nil {
			return
		}
		cur.Data["config.lua"] = orig
		s.cs.CoreV1().ConfigMaps(s.ns).Update(restoreCtx, cur, metav1.UpdateOptions{})
		restartGateway(t, restoreCtx, s.cs, s.ns)
	})

	cm.Data["config.lua"] = newConfig
	if _, err := s.cs.CoreV1().ConfigMaps(s.ns).Update(ctx, cm, metav1.UpdateOptions{}); err != nil {
		t.Fatalf("update configmap: %v", err)
	}

	restartGateway(t, ctx, s.cs, s.ns)

	// After pod restart the port-forward is dead; make a fresh one.
	mckind.WaitForDeploymentReady(t, ctx, s.cs, s.ns, "mcgateway", 2*time.Minute)
	gwPort := mckind.PortForwardPod(t, ctx, s.cs, s.cfg, s.ns, gatewaySelector, 11211)
	gwAddr := mckind.Addr(gwPort)

	key := uniqueKey("addedv2", "reload")
	val := "reload-ok"

	// Pre-check: old gateway would have rejected addedv2 with SERVER_ERROR;
	// the reload just happened, so we can only assert the new state.
	if _, err := mckind.McSet(gwAddr, key, val); err != nil {
		t.Fatalf("ms after reload: %v", err)
	}
	got, err := mckind.McGetWithRetry(gwAddr, key, 10)
	if err != nil {
		t.Fatalf("mg after reload: %v", err)
	}
	if got != val {
		t.Fatalf("mg after reload: got %q, want %q", got, val)
	}
}

// restartGateway deletes gateway pods so the Deployment creates fresh ones
// that pick up the updated ConfigMap mount.
func restartGateway(t *testing.T, ctx context.Context, cs kubernetes.Interface, ns string) {
	t.Helper()
	pods, err := cs.CoreV1().Pods(ns).List(ctx, metav1.ListOptions{LabelSelector: gatewaySelector})
	if err != nil {
		t.Fatalf("list gateway pods: %v", err)
	}
	for _, p := range pods.Items {
		if err := cs.CoreV1().Pods(ns).Delete(ctx, p.Name, metav1.DeleteOptions{}); err != nil {
			t.Logf("delete pod %s: %v", p.Name, err)
		}
	}

	// Wait for the old pod to go away and a new one to enter Running.
	deadline := time.Now().Add(3 * time.Minute)
	for time.Now().Before(deadline) {
		current, err := cs.CoreV1().Pods(ns).List(ctx, metav1.ListOptions{LabelSelector: gatewaySelector})
		if err == nil {
			running := 0
			for _, p := range current.Items {
				if p.Status.Phase == corev1.PodRunning && p.DeletionTimestamp == nil {
					running++
				}
			}
			if running >= 1 {
				return
			}
		}
		time.Sleep(2 * time.Second)
	}
	t.Fatalf("no fresh gateway pod Running within timeout")
}
