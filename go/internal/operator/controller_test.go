package operator

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/prometheus/client_golang/prometheus/testutil"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
)

// The reconciler is a thin list→render→commit loop and its pieces are
// tested exhaustively on their own; this exercises the loop against a
// fake client. The kind suite covers the real API-server path.
func TestReconcileConverges(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}

	pool := &v1alpha1.Pool{
		ObjectMeta: metav1.ObjectMeta{Name: "mc-a", Namespace: "gw"},
		Spec:       v1alpha1.PoolSpec{Addrs: []string{"mc-a:11211"}},
	}
	ks := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "user", Namespace: "gw"},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "user",
			Read:   []string{"mc-a"},
			Write:  []string{"mc-a"},
		},
	}
	// Same names in another namespace: must be invisible to the
	// reconciler's namespace-scoped lists.
	foreign := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "other", Namespace: "elsewhere"},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "other",
			Read:   []string{"mc-a"},
			Write:  []string{"mc-a"},
		},
	}

	cl := fake.NewClientBuilder().WithScheme(scheme).WithObjects(pool, ks, foreign).Build()
	root := t.TempDir()
	r := &Reconciler{Client: cl, FS: NewOSFS(root), Namespace: "gw"}
	ctx := context.Background()

	if r.Ready() {
		t.Fatal("ready before any commit")
	}
	if _, err := r.Reconcile(ctx, reconcile.Request{}); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if !r.Ready() {
		t.Fatal("not ready after a successful commit")
	}

	cfg, err := os.ReadFile(filepath.Join(root, ConfigFile))
	if err != nil {
		t.Fatalf("read committed config: %v", err)
	}
	if !strings.Contains(string(cfg), `prefix = "user"`) {
		t.Fatalf("config missing keyspace:\n%s", cfg)
	}
	if strings.Contains(string(cfg), `"other"`) {
		t.Fatalf("config leaked a foreign-namespace keyspace:\n%s", cfg)
	}

	// Deletion converges with no finalizer machinery: the next
	// snapshot simply lacks the object.
	if err := cl.Delete(ctx, ks); err != nil {
		t.Fatalf("delete keyspace: %v", err)
	}
	if _, err := r.Reconcile(ctx, reconcile.Request{}); err != nil {
		t.Fatalf("reconcile after delete: %v", err)
	}
	cfg, err = os.ReadFile(filepath.Join(root, ConfigFile))
	if err != nil {
		t.Fatalf("read config after delete: %v", err)
	}
	if len(cfg) == 0 {
		t.Fatal("config empty after delete; want a valid config without the keyspace")
	}
	if strings.Contains(string(cfg), `prefix = "user"`) {
		t.Fatalf("deleted keyspace still rendered:\n%s", cfg)
	}
}

// failingFS refuses all writes; used to pin that readiness never
// reports before a commit has actually succeeded.
type failingFS struct{ FS }

func (failingFS) WriteFile(string, []byte) error {
	return errors.New("injected write failure")
}

// A failed commit must surface as a reconcile error and leave
// Ready() false — readiness is what gates the gateway container's
// startup on a config being present, so reporting ready without a
// commit would boot the gateway against an empty mount.
func TestReconcileFailureLeavesNotReady(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	cl := fake.NewClientBuilder().WithScheme(scheme).Build()
	r := &Reconciler{Client: cl, FS: failingFS{NewOSFS(t.TempDir())}, Namespace: "gw"}

	if _, err := r.Reconcile(context.Background(), reconcile.Request{}); err == nil {
		t.Fatal("expected reconcile to surface the commit failure")
	}
	if r.Ready() {
		t.Fatal("ready after a failed commit")
	}
}

// The render-warnings gauge is the alertable form of the blast-radius
// policy: it must rise when a CR is skipped and — being a gauge set
// from every render — fall back to zero once the CR is fixed.
func TestReconcileSetsMetrics(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}

	pool := &v1alpha1.Pool{
		ObjectMeta: metav1.ObjectMeta{Name: "mc-a", Namespace: "gw"},
		Spec:       v1alpha1.PoolSpec{Addrs: []string{"mc-a:11211"}},
	}
	// References a pool that doesn't exist: skipped with a Warning.
	bad := &v1alpha1.Keyspace{
		ObjectMeta: metav1.ObjectMeta{Name: "bad", Namespace: "gw"},
		Spec: v1alpha1.KeyspaceSpec{
			Prefix: "bad",
			Read:   []string{"nope"},
			Write:  []string{"nope"},
		},
	}

	cl := fake.NewClientBuilder().WithScheme(scheme).WithObjects(pool, bad).Build()
	r := &Reconciler{Client: cl, FS: NewOSFS(t.TempDir()), Namespace: "gw"}
	ctx := context.Background()

	okBefore := testutil.ToFloat64(commitsTotal.WithLabelValues("ok"))

	if _, err := r.Reconcile(ctx, reconcile.Request{}); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if got := testutil.ToFloat64(renderWarnings); got != 1 {
		t.Fatalf("render warnings gauge = %v, want 1 (bad keyspace skipped)", got)
	}
	if got := testutil.ToFloat64(snapshotObjects.WithLabelValues("pool")); got != 1 {
		t.Fatalf("snapshot pool gauge = %v, want 1", got)
	}
	if got := testutil.ToFloat64(snapshotObjects.WithLabelValues("keyspace")); got != 1 {
		t.Fatalf("snapshot keyspace gauge = %v, want 1", got)
	}
	if got := testutil.ToFloat64(commitsTotal.WithLabelValues("ok")); got != okBefore+1 {
		t.Fatalf("ok commits = %v, want %v", got, okBefore+1)
	}

	// Fix the CR; the gauge must return to zero on the next render.
	bad.Spec.Read = []string{"mc-a"}
	bad.Spec.Write = []string{"mc-a"}
	if err := cl.Update(ctx, bad); err != nil {
		t.Fatalf("update keyspace: %v", err)
	}
	if _, err := r.Reconcile(ctx, reconcile.Request{}); err != nil {
		t.Fatalf("reconcile after fix: %v", err)
	}
	if got := testutil.ToFloat64(renderWarnings); got != 0 {
		t.Fatalf("render warnings gauge = %v after fix, want 0", got)
	}

	// A failing commit counts under result="error".
	errBefore := testutil.ToFloat64(commitsTotal.WithLabelValues("error"))
	rFail := &Reconciler{Client: cl, FS: failingFS{NewOSFS(t.TempDir())}, Namespace: "gw"}
	if _, err := rFail.Reconcile(ctx, reconcile.Request{}); err == nil {
		t.Fatal("expected commit failure")
	}
	if got := testutil.ToFloat64(commitsTotal.WithLabelValues("error")); got != errBefore+1 {
		t.Fatalf("error commits = %v, want %v", got, errBefore+1)
	}
}

// An empty namespace must still commit an (empty but valid) config:
// the operator's readiness gates the gateway's startup, and a cluster
// with no CRs yet is a legitimate state to boot into.
func TestReconcileEmptyNamespaceCommitsEmptyConfig(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	cl := fake.NewClientBuilder().WithScheme(scheme).Build()
	root := t.TempDir()
	r := &Reconciler{Client: cl, FS: NewOSFS(root), Namespace: "gw"}

	if _, err := r.Reconcile(context.Background(), reconcile.Request{}); err != nil {
		t.Fatalf("reconcile: %v", err)
	}
	if !r.Ready() {
		t.Fatal("not ready after empty commit")
	}
	if _, err := os.Stat(filepath.Join(root, ConfigFile)); err != nil {
		t.Fatalf("empty namespace did not commit a config: %v", err)
	}
}
