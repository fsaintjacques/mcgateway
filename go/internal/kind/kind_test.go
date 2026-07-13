//go:build kind

package kind_test

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

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

