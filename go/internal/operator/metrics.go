// Operator metrics, registered with controller-runtime's global
// registry so they ride the same /metrics endpoint as the manager's
// built-ins (reconcile counts and durations, workqueue depth, client
// latencies). The custom set is small: it covers exactly what the
// built-ins cannot see — the render's blast-radius policy and the
// commit outcome.
package operator

import (
	"github.com/prometheus/client_golang/prometheus"
	"sigs.k8s.io/controller-runtime/pkg/metrics"
)

var (
	// renderWarnings is the alertable form of skip-with-Warning: a CR
	// skipped by the renderer is an operator log line, but a *gauge*
	// stays nonzero until the offending CR is fixed — level-triggered,
	// like the reconciler that sets it.
	renderWarnings = prometheus.NewGauge(prometheus.GaugeOpts{
		Name: "mcgateway_operator_render_warnings",
		Help: "CRs skipped by the last render (blast-radius policy); nonzero until the CRs are fixed",
	})

	commitsTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
		Name: "mcgateway_operator_commits_total",
		Help: "Reconcile commit attempts by result",
	}, []string{"result"})

	snapshotObjects = prometheus.NewGaugeVec(prometheus.GaugeOpts{
		Name: "mcgateway_operator_snapshot_objects",
		Help: "Objects in the last reconciled snapshot by kind",
	}, []string{"kind"})
)

func init() {
	metrics.Registry.MustRegister(renderWarnings, commitsTotal, snapshotObjects)
}
