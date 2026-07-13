//go:build kind

package kind_test

import (
	"context"
	"encoding/base64"
	"strings"
	"testing"
	"time"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	crclient "sigs.k8s.io/controller-runtime/pkg/client"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
	mckind "github.com/fsaintjacques/mcgateway/go/internal/kind"
)

// These tests exercise the Stage 4 step-6 deliverable: CRs applied at
// runtime drive the gateway's routing through the operator sidecar —
// render, commit, SIGHUP — with zero gateway restarts. They require
// the chart's operator mode (on in values-kind.yaml).

// bakedModule reads a module the Dockerfile bakes into the gateway
// image. Module bytes are build artifacts the test host doesn't have
// (the wasm is compiled inside the image build), so tests that need
// real module bytes for inline-wasm CRs pull them out of the pod.
func bakedModule(t *testing.T, ctx context.Context, s *suite, pod, name string) []byte {
	t.Helper()
	out, errOut, err := mckind.ExecInPod(ctx, s.cs, s.cfg, s.ns, pod, "gateway",
		[]string{"base64", "/etc/mcgateway/udf/" + name + ".wasm"}, nil)
	if err != nil {
		t.Fatalf("read baked module %s: %v (stderr: %s)", name, err, errOut)
	}
	raw, err := base64.StdEncoding.DecodeString(strings.ReplaceAll(out, "\n", ""))
	if err != nil {
		t.Fatalf("decode baked module %s: %v", name, err)
	}
	return raw
}

// applyKeyspace creates the Keyspace CR and registers a cleanup that
// deletes it and waits for its prefix to stop routing, so tests don't
// leak routing state into each other.
func applyKeyspace(t *testing.T, ctx context.Context, s *suite, cl crclient.Client, ks *v1alpha1.Keyspace) {
	t.Helper()
	if err := cl.Create(ctx, ks); err != nil {
		t.Fatalf("create keyspace %s: %v", ks.Name, err)
	}
	t.Cleanup(func() {
		cctx, ccancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer ccancel()
		if err := cl.Delete(cctx, ks); err != nil && !apierrors.IsNotFound(err) {
			t.Errorf("delete keyspace %s: %v", ks.Name, err)
		}
		waitPrefixRejected(t, s.gwAddr, ks.Spec.Prefix)
	})
}

// TestOperatorConfigApply is the stage's headline demo: kubectl-apply
// a Keyspace, traffic routes within seconds, zero restarts.
func TestOperatorConfigApply(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	cl := mckind.CRClient(t, s.cfg)

	resp, err := mckind.McDo(s.gwAddr, "mg opv1:pre v", nil)
	if err != nil {
		t.Fatalf("pre-check: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("pre-check: opv1 already routed: %q", resp.Line)
	}

	applyKeyspace(t, ctx, s, cl, &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "opv1", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "opv1",
			Read:   []string{"mc-a"},
			Write:  []string{"mc-a"},
		},
	})

	waitPrefixRouted(t, s.gwAddr, "opv1")
	assertNoRestarts(t, ctx, s, pod, restarts0, "CR apply")
}

// TestOperatorConfigUpdate edits a live Keyspace and asserts the
// routing actually moves: writes land on the pool the current spec
// names, verified by reading the backends directly.
func TestOperatorConfigUpdate(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	cl := mckind.CRClient(t, s.cfg)

	ks := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "opv2", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "opv2",
			Read:   []string{"mc-a"},
			Write:  []string{"mc-a"},
		},
	}
	applyKeyspace(t, ctx, s, cl, ks)
	waitPrefixRouted(t, s.gwAddr, "opv2")

	keyA := uniqueKey("opv2", "before")
	if _, err := mckind.McSet(s.gwAddr, keyA, "on-a"); err != nil {
		t.Fatalf("write before update: %v", err)
	}
	if got, err := mckind.McGetWithRetry(s.mcAAddr, keyA, 5); err != nil || got != "on-a" {
		t.Fatalf("write did not land on mc-a: %q err=%v", got, err)
	}

	// Move the keyspace to mc-b.
	if err := cl.Get(ctx, crclient.ObjectKeyFromObject(ks), ks); err != nil {
		t.Fatalf("get keyspace: %v", err)
	}
	ks.Spec.Read = []string{"mc-b"}
	ks.Spec.Write = []string{"mc-b"}
	if err := cl.Update(ctx, ks); err != nil {
		t.Fatalf("update keyspace: %v", err)
	}

	// Poll until a fresh write lands on mc-b: that is the observable
	// proof the reload applied the new spec.
	deadline := time.Now().Add(reloadWait)
	for {
		key := uniqueKey("opv2", "after")
		if _, err := mckind.McSet(s.gwAddr, key, "on-b"); err == nil {
			if got, _ := mckind.McGetWithRetry(s.mcBAddr, key, 2); got == "on-b" {
				break
			}
		}
		if time.Now().After(deadline) {
			t.Fatal("updated keyspace never routed writes to mc-b")
		}
		time.Sleep(500 * time.Millisecond)
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "CR update")
}

// TestOperatorInlineWasm proves the inline-module path end to end
// with a real UDF: the module bytes ride the CR, the operator lands
// them in the state dir, the gateway compiles and dispatches through
// them.
func TestOperatorInlineWasm(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	cl := mckind.CRClient(t, s.cfg)

	applyKeyspace(t, ctx, s, cl, &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "opwasm", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "opwasm",
			Read:   []string{"mc-a", "mc-b"},
			Write:  []string{"mc-a", "mc-b"},
			Merge: &v1alpha1.MergeSpec{
				Name: "op_probe",
				Wasm: bakedModule(t, ctx, s, pod, "merge_last_n_wins"),
			},
		},
	})

	waitMergeRegistered(t, s, "op_probe", true)
	waitPrefixRouted(t, s.gwAddr, "opwasm")

	// The module is a last-n-wins merge: differential-TTL seeds on
	// the backends prove dispatch actually runs the inline module.
	key := uniqueKey("opwasm", "lww")
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
		t.Fatalf("inline module dispatch: got %q, want %q", got, "newer")
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "inline wasm")
}

// TestOperatorBadCR is the blast-radius test: a Keyspace referencing
// a pool that does not exist is skipped with a warning, and every
// healthy keyspace keeps serving.
func TestOperatorBadCR(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	cl := mckind.CRClient(t, s.cfg)

	applyKeyspace(t, ctx, s, cl, &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "opbad", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "opbad",
			Read:   []string{"no-such-pool"},
			Write:  []string{"mc-a"},
		},
	})

	// Positive signal that the operator processed and skipped it.
	deadline := time.Now().Add(reloadWait)
	for {
		logs, err := mckind.PodLogs(ctx, s.cs, s.ns, pod, "operator", 200)
		if err == nil && strings.Contains(logs, "render warning") && strings.Contains(logs, "opbad") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("operator never logged a render warning for opbad; last PodLogs err: %v; last logs:\n%s", err, logs)
		}
		time.Sleep(500 * time.Millisecond)
	}

	// Healthy keyspaces keep serving; the bad one never routes.
	key := uniqueKey("user", "badcr")
	if _, err := mckind.McSet(s.gwAddr, key, "healthy"); err != nil {
		t.Fatalf("healthy keyspace write: %v", err)
	}
	if got, err := mckind.McGetWithRetry(s.gwAddr, key, 10); err != nil || got != "healthy" {
		t.Fatalf("healthy keyspace read: %q err=%v", got, err)
	}
	resp, err := mckind.McDo(s.gwAddr, "mg opbad:x v", nil)
	if err != nil {
		t.Fatalf("mg opbad: %v", err)
	}
	if resp.Status != "SERVER_ERROR" {
		t.Fatalf("bad CR routed: %q", resp.Line)
	}
	assertNoRestarts(t, ctx, s, pod, restarts0, "bad CR")
}

// TestOperatorRemoval deletes an inline-wasm Keyspace and asserts
// full convergence: the prefix stops routing and the module leaves
// the registry (the committer removed its file after the config that
// dropped it went live).
func TestOperatorRemoval(t *testing.T) {
	s := newSuite(t)
	ctx, cancel := context.WithTimeout(context.Background(), 4*time.Minute)
	defer cancel()

	pod := mckind.RunningPodName(t, ctx, s.cs, s.ns, gatewaySelector)
	restarts0, err := mckind.PodRestartCount(ctx, s.cs, s.ns, pod)
	if err != nil {
		t.Fatalf("restart count: %v", err)
	}
	cl := mckind.CRClient(t, s.cfg)

	ks := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "oprm", Namespace: s.ns},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "oprm",
			Read:   []string{"mc-a"},
			Write:  []string{"mc-a"},
			Merge: &v1alpha1.MergeSpec{
				Name: "op_removal",
				Wasm: bakedModule(t, ctx, s, pod, "merge_last_n_wins"),
			},
		},
	}
	// No applyKeyspace helper: this test's whole point is observing
	// the deletion itself.
	if err := cl.Create(ctx, ks); err != nil {
		t.Fatalf("create keyspace: %v", err)
	}
	deleted := false
	t.Cleanup(func() {
		if !deleted {
			cctx, ccancel := context.WithTimeout(context.Background(), time.Minute)
			defer ccancel()
			_ = cl.Delete(cctx, ks)
		}
	})

	waitMergeRegistered(t, s, "op_removal", true)
	waitPrefixRouted(t, s.gwAddr, "oprm")

	if err := cl.Delete(ctx, ks); err != nil {
		t.Fatalf("delete keyspace: %v", err)
	}
	deleted = true

	waitPrefixRejected(t, s.gwAddr, "oprm")
	waitMergeRegistered(t, s, "op_removal", false)
	assertNoRestarts(t, ctx, s, pod, restarts0, "CR removal")
}
