// The mcgateway operator compiles Pool/Keyspace CRs into the config
// file tree the gateway consumes. It runs as a native sidecar in each
// gateway pod (Stage 4 topology): watch CRs in the pod's namespace,
// render, commit to the shared mount — the gateway's own watchers do
// the rest. See doc/plans/stage-4-operator.md.
package main

import (
	"errors"
	"flag"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strings"

	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/cache"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	v1alpha1 "github.com/fsaintjacques/mcgateway/go/api/v1alpha1"
	"github.com/fsaintjacques/mcgateway/go/internal/operator"
)

const saNamespaceFile = "/var/run/secrets/kubernetes.io/serviceaccount/namespace"

func main() {
	var (
		dir         string
		namespace   string
		healthAddr  string
		metricsAddr string
	)
	flag.StringVar(&dir, "dir", "/var/run/mcgateway",
		"mount root the rendered config and modules are committed to")
	flag.StringVar(&namespace, "namespace", "",
		"namespace whose Pool/Keyspace CRs drive the config (default: POD_NAMESPACE, then the serviceaccount namespace file)")
	flag.StringVar(&healthAddr, "health-addr", ":8081",
		"listen address for the health and readiness probes")
	flag.StringVar(&metricsAddr, "metrics-addr", ":8080",
		"listen address for Prometheus metrics; \"0\" disables the endpoint")
	zapOpts := zap.Options{}
	zapOpts.BindFlags(flag.CommandLine)
	flag.Parse()

	ctrl.SetLogger(zap.New(zap.UseFlagOptions(&zapOpts)))
	logger := ctrl.Log.WithName("setup")

	if err := run(dir, namespace, healthAddr, metricsAddr); err != nil {
		logger.Error(err, "operator exiting")
		os.Exit(1)
	}
}

func run(dir, namespace, healthAddr, metricsAddr string) error {
	logger := ctrl.Log.WithName("setup")

	ns, err := resolveNamespace(namespace)
	if err != nil {
		return err
	}

	// The gateway fails loudly when MCGW_UDF_DIR points at a missing
	// directory, and the committer only creates it when a module is
	// written — so guarantee the empty shape up front, before
	// readiness lets the gateway container start. World-writable
	// (explicit Chmod: MkdirAll is umask-clipped) because the gateway
	// container runs as a different uid and writes its AOT cache
	// under this dir — and an emptyDir inside one pod is not a
	// security boundary.
	udfDir := filepath.Join(dir, operator.UdfDir)
	if err := os.MkdirAll(udfDir, 0o777); err != nil {
		return fmt.Errorf("create udf dir: %w", err)
	}
	if err := os.Chmod(udfDir, 0o777); err != nil {
		return fmt.Errorf("chmod udf dir: %w", err)
	}

	scheme := runtime.NewScheme()
	if err := v1alpha1.AddToScheme(scheme); err != nil {
		return fmt.Errorf("build scheme: %w", err)
	}

	mgr, err := ctrl.NewManager(ctrl.GetConfigOrDie(), ctrl.Options{
		Scheme: scheme,
		// Watch (and cache) only the one namespace RBAC grants.
		Cache: cache.Options{
			DefaultNamespaces: map[string]cache.Config{ns: {}},
		},
		// Serves controller-runtime's built-ins plus the custom
		// operator metrics (render warnings, commit results — see
		// internal/operator/metrics.go).
		Metrics:                metricsserver.Options{BindAddress: metricsAddr},
		HealthProbeBindAddress: healthAddr,
		// N sidecars render the same files from the same CRs; there
		// is no shared mutable state to elect a leader over.
		LeaderElection: false,
	})
	if err != nil {
		return fmt.Errorf("create manager: %w", err)
	}

	rec := &operator.Reconciler{
		Client:    mgr.GetClient(),
		FS:        operator.NewOSFS(dir),
		Namespace: ns,
	}
	if err := rec.SetupWithManager(mgr); err != nil {
		return fmt.Errorf("set up controller: %w", err)
	}

	if err := mgr.AddHealthzCheck("ping", healthz.Ping); err != nil {
		return fmt.Errorf("add healthz: %w", err)
	}
	// Ready only after the first successful commit: the gateway
	// container's startup is gated on this (native-sidecar ordering),
	// which is what guarantees it never boots against an empty mount.
	if err := mgr.AddReadyzCheck("committed", func(*http.Request) error {
		if rec.Ready() {
			return nil
		}
		return errors.New("no successful commit yet")
	}); err != nil {
		return fmt.Errorf("add readyz: %w", err)
	}

	logger.Info("starting", "namespace", ns, "dir", dir)
	return mgr.Start(ctrl.SetupSignalHandler())
}

// resolveNamespace picks the watched namespace: explicit flag, then
// the downward-API env var, then the serviceaccount namespace file.
// Guessing (e.g. "default") is worse than failing: an operator
// watching the wrong namespace renders an empty config and quietly
// blanks the gateway's routes.
func resolveNamespace(flagValue string) (string, error) {
	if flagValue != "" {
		return flagValue, nil
	}
	if ns := os.Getenv("POD_NAMESPACE"); ns != "" {
		return ns, nil
	}
	if b, err := os.ReadFile(saNamespaceFile); err == nil {
		if ns := strings.TrimSpace(string(b)); ns != "" {
			return ns, nil
		}
	}
	return "", errors.New("cannot determine namespace: set --namespace or POD_NAMESPACE")
}
