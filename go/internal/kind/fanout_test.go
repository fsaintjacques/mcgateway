//go:build kind

package kind_test

import (
	"context"
	"fmt"
	"testing"
	"time"

	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// --- Fan-out reads --------------------------------------------------------

// first-hit: seed only on mc-b; gateway should return mc-b's value.
func TestFanoutRead_FirstHit_SkipMissOnA(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("fanfh", "skipA")
	val := "only-on-b"

	if _, err := mckind.McSet(s.mcBAddr, key, val); err != nil {
		t.Fatalf("seed mc-b: %v", err)
	}

	got, err := mckind.McGetWithRetry(s.gwAddr, key, 5)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}
	if got != val {
		t.Fatalf("merged read: got %q, want %q", got, val)
	}
}

// pool-preferred: both pools have different values; mc-a (first in list) wins.
func TestFanoutRead_PoolPreferred_PicksFirstPool(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("fanpp", "prefA")

	if _, err := mckind.McSet(s.mcAAddr, key, "from-a"); err != nil {
		t.Fatalf("seed mc-a: %v", err)
	}
	if _, err := mckind.McSet(s.mcBAddr, key, "from-b"); err != nil {
		t.Fatalf("seed mc-b: %v", err)
	}

	got, err := mckind.McGetWithRetry(s.gwAddr, key, 5)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}
	if got != "from-a" {
		t.Fatalf("merged read: got %q, want %q", got, "from-a")
	}
}

// All miss — gateway should report a miss (non-VA status).
func TestFanoutRead_AllMiss(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("fanfh", "miss")

	resp, err := mckind.McDo(s.gwAddr, fmt.Sprintf("mg %s v", key), nil)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}
	if resp.Status == "VA" {
		t.Fatalf("expected miss, got VA value %q", resp.Value)
	}
}

// last-write-wins: both pools have the key, but mc-b has a larger TTL so
// its `t` flag is larger. LWW should pick mc-b.
func TestFanoutRead_LastWriteWins_PicksLargerT(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("fanlww", "lww")

	if _, err := mckind.McSetTTL(s.mcAAddr, key, "older", 60); err != nil {
		t.Fatalf("seed mc-a: %v", err)
	}
	if _, err := mckind.McSetTTL(s.mcBAddr, key, "newer", 3600); err != nil {
		t.Fatalf("seed mc-b: %v", err)
	}

	got, err := mckind.McGetWithRetry(s.gwAddr, key, 5)
	if err != nil {
		t.Fatalf("mg via gateway: %v", err)
	}
	if got != "newer" {
		t.Fatalf("lww: got %q, want %q", got, "newer")
	}
}

// Pool down: mc-a scaled to 0; fan-out read should still succeed from mc-b.
func TestFanoutRead_PoolDown_FallsThrough(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()

	key := uniqueKey("fanfh", "pooldown")
	val := "survives"

	if _, err := mckind.McSet(s.mcBAddr, key, val); err != nil {
		t.Fatalf("seed mc-b: %v", err)
	}

	// Scale mc-a to 0 and restore at end.
	mckind.ScaleDeployment(t, ctx, s.cs, s.ns, "mc-a", 0, 2*time.Minute)
	t.Cleanup(func() {
		restoreCtx, cancelRestore := context.WithTimeout(context.Background(), 3*time.Minute)
		defer cancelRestore()
		mckind.ScaleDeployment(t, restoreCtx, s.cs, s.ns, "mc-a", 1, 2*time.Minute)
		// Flush the gateway's cached bad-backend state so later tests see
		// mc-a as healthy immediately.
		mckind.RestartDeployment(t, restoreCtx, s.cs, s.ns, gatewaySelector, 2*time.Minute)
	})

	got, err := mckind.McGetWithRetry(s.gwAddr, key, 10)
	if err != nil {
		t.Fatalf("mg with mc-a down: %v", err)
	}
	if got != val {
		t.Fatalf("got %q, want %q (mc-a should be skipped)", got, val)
	}
}

// --- Write policies -------------------------------------------------------

// write_policy = all: value must land on both pools.
func TestWritePolicy_All_WritesBoth(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("wall", "both")
	val := "two-pools"

	resp, err := mckind.McSet(s.gwAddr, key, val)
	if err != nil {
		t.Fatalf("ms via gateway: %v", err)
	}
	if resp.Status == "SERVER_ERROR" || resp.Status == "ERROR" {
		t.Fatalf("ms status=%q line=%q, want success", resp.Status, resp.Line)
	}

	gotA, err := mckind.McGetWithRetry(s.mcAAddr, key, 3)
	if err != nil {
		t.Fatalf("mg mc-a: %v", err)
	}
	if gotA != val {
		t.Fatalf("mc-a: got %q, want %q", gotA, val)
	}

	gotB, err := mckind.McGetWithRetry(s.mcBAddr, key, 3)
	if err != nil {
		t.Fatalf("mg mc-b: %v", err)
	}
	if gotB != val {
		t.Fatalf("mc-b: got %q, want %q", gotB, val)
	}
}

// write_policy = all with a backend down: gateway must surface the failure.
func TestWritePolicy_All_BackendDownFails(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()

	mckind.ScaleDeployment(t, ctx, s.cs, s.ns, "mc-b", 0, 2*time.Minute)
	t.Cleanup(func() {
		restoreCtx, cancelRestore := context.WithTimeout(context.Background(), 3*time.Minute)
		defer cancelRestore()
		mckind.ScaleDeployment(t, restoreCtx, s.cs, s.ns, "mc-b", 1, 2*time.Minute)
		mckind.RestartDeployment(t, restoreCtx, s.cs, s.ns, gatewaySelector, 2*time.Minute)
	})

	key := uniqueKey("wall", "downfail")
	resp, err := mckind.McSet(s.gwAddr, key, "x")
	if err != nil {
		t.Fatalf("ms via gateway: %v", err)
	}
	// The client must not observe a plain success (HD/OK/STORED) when one
	// of the write pools is unreachable.
	if resp.Status == "HD" || resp.Status == "OK" || resp.Status == "STORED" {
		t.Fatalf("policy=all with mc-b down returned success: status=%q line=%q",
			resp.Status, resp.Line)
	}
}

// write_policy = first: the primary (mc-a) acks immediately; the secondary
// (mc-b) receives the write asynchronously.
func TestWritePolicy_First_PrimaryAndShadow(t *testing.T) {
	s := newSuite(t)
	key := uniqueKey("wfirst", "primshd")
	val := "first-then-shadow"

	resp, err := mckind.McSet(s.gwAddr, key, val)
	if err != nil {
		t.Fatalf("ms via gateway: %v", err)
	}
	if resp.Status == "SERVER_ERROR" || resp.Status == "ERROR" {
		t.Fatalf("ms status=%q line=%q, want success", resp.Status, resp.Line)
	}

	gotA, err := mckind.McGetWithRetry(s.mcAAddr, key, 5)
	if err != nil {
		t.Fatalf("mg mc-a: %v", err)
	}
	if gotA != val {
		t.Fatalf("mc-a: got %q, want %q", gotA, val)
	}

	// Secondary lands asynchronously; poll.
	gotB, err := mckind.McGetWithRetry(s.mcBAddr, key, 15)
	if err != nil {
		t.Fatalf("mg mc-b (shadow): %v", err)
	}
	if gotB != val {
		t.Fatalf("mc-b shadow: got %q, want %q", gotB, val)
	}
}

// --- Multi-key not supported ---------------------------------------------

// Keys containing `#` are rejected with SERVER_ERROR. Multi-key fan-out is
// deferred; may return later via sub-funcgens once real demand exists.
func TestMultiKey_Rejected(t *testing.T) {
	s := newSuite(t)
	cmd := "mg fanfh:a#fanfh:b v"
	resp, err := mckind.McDo(s.gwAddr, cmd, nil)
	if err != nil {
		t.Fatalf("multi-key mg: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("status=%q line=%q, want SERVER_ERROR", resp.Status, resp.Line)
	}
}
