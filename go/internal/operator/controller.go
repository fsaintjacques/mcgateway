package operator

import (
	"context"
	"fmt"
	"sync/atomic"

	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/event"
	"sigs.k8s.io/controller-runtime/pkg/handler"
	"sigs.k8s.io/controller-runtime/pkg/log"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"
	"sigs.k8s.io/controller-runtime/pkg/source"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
)

// Reconciler is the snapshot reconciler: any watch event on either
// CRD enqueues the same fixed request, and a pass lists every Pool
// and Keyspace in the namespace, renders the full desired tree, and
// commits the diff. Level-triggered and stateless — a deleted CR is
// simply absent from the next snapshot, so there are no finalizers
// and no per-object bookkeeping.
type Reconciler struct {
	client.Client
	// FS is the mount root the rendered tree is committed to.
	FS FS
	// Namespace whose CRs drive the config.
	Namespace string

	committed atomic.Bool
}

// snapshotRequest is the constant request every event maps to. Its
// fields are never inspected; there is exactly one unit of work.
var snapshotRequest = reconcile.Request{}

func (r *Reconciler) Reconcile(ctx context.Context, _ reconcile.Request) (reconcile.Result, error) {
	logger := log.FromContext(ctx)

	var pools v1alpha1.PoolList
	if err := r.List(ctx, &pools, client.InNamespace(r.Namespace)); err != nil {
		return reconcile.Result{}, fmt.Errorf("list pools: %w", err)
	}
	var keyspaces v1alpha1.KeyspaceList
	if err := r.List(ctx, &keyspaces, client.InNamespace(r.Namespace)); err != nil {
		return reconcile.Result{}, fmt.Errorf("list keyspaces: %w", err)
	}

	files, warns := Render(Snapshot{Pools: pools.Items, Keyspaces: keyspaces.Items})
	for _, w := range warns {
		// Warnings are the operator's whole status surface until the
		// status subresource lands in Stage 5 of the plan.
		logger.Info("render warning", "kind", w.Kind, "name", w.Name, "reason", w.Message)
	}

	res, err := Commit(r.FS, files)
	if err != nil {
		return reconcile.Result{}, err
	}
	if res.WroteConfig || len(res.WroteModules)+len(res.RemovedModules)+len(res.CleanedTemp) > 0 {
		logger.Info("committed",
			"config", res.WroteConfig,
			"modulesWritten", res.WroteModules,
			"modulesRemoved", res.RemovedModules,
			"tempCleaned", res.CleanedTemp,
			"pools", len(pools.Items),
			"keyspaces", len(keyspaces.Items),
		)
	}
	r.committed.Store(true)
	return reconcile.Result{}, nil
}

// Ready reports whether at least one commit has succeeded. It backs
// the operator's readiness probe, which is what gates the gateway
// container's startup on a config being present (native-sidecar
// ordering).
func (r *Reconciler) Ready() bool { return r.committed.Load() }

// SetupWithManager wires both CRD watches into the single snapshot
// reconcile, plus a one-shot initial sync: with zero CRs in the
// namespace no watch event ever fires, yet the gateway still needs an
// (empty) config.lua to boot, so a pre-filled channel source enqueues
// one reconcile at start. Concurrency stays at the default 1 —
// renders serialize, keeping the committer's ordering trivial.
func (r *Reconciler) SetupWithManager(mgr ctrl.Manager) error {
	toSnapshot := handler.EnqueueRequestsFromMapFunc(
		func(context.Context, client.Object) []reconcile.Request {
			return []reconcile.Request{snapshotRequest}
		})

	initialSync := make(chan event.GenericEvent, 1)
	initialSync <- event.GenericEvent{Object: &v1alpha1.Pool{}}
	close(initialSync)

	return ctrl.NewControllerManagedBy(mgr).
		Named("mcgateway-config").
		Watches(&v1alpha1.Pool{}, toSnapshot).
		Watches(&v1alpha1.Keyspace{}, toSnapshot).
		WatchesRawSource(source.Channel(initialSync, toSnapshot)).
		Complete(r)
}
