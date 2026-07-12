//go:build kind

package kind

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/kubernetes/scheme"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/clientcmd"
	"k8s.io/client-go/tools/portforward"
	"k8s.io/client-go/tools/remotecommand"
	"k8s.io/client-go/transport/spdy"
)

// ReleaseNamespace is where the Helm chart installs the gateway + backends.
// Tests don't create fresh namespaces: they share the installed release and
// simply poke it with unique keys.
const ReleaseNamespace = "mcgateway-system"

// ClientAndConfig loads the local kubeconfig.
func ClientAndConfig(t *testing.T) (kubernetes.Interface, *rest.Config) {
	t.Helper()
	kubeconfig := os.Getenv("KUBECONFIG")
	if kubeconfig == "" {
		home, _ := os.UserHomeDir()
		kubeconfig = home + "/.kube/config"
	}
	cfg, err := clientcmd.BuildConfigFromFlags("", kubeconfig)
	if err != nil {
		t.Fatalf("build kubeconfig: %v", err)
	}
	cs, err := kubernetes.NewForConfig(cfg)
	if err != nil {
		t.Fatalf("create clientset: %v", err)
	}
	return cs, cfg
}

// WaitForDeploymentReady waits until a deployment reports at least one ready replica.
func WaitForDeploymentReady(t *testing.T, ctx context.Context, cs kubernetes.Interface, ns, name string, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		dep, err := cs.AppsV1().Deployments(ns).Get(ctx, name, metav1.GetOptions{})
		if err == nil && dep.Status.ReadyReplicas >= 1 {
			return
		}
		select {
		case <-ctx.Done():
			t.Fatalf("context cancelled waiting for deployment %s", name)
		case <-time.After(2 * time.Second):
		}
	}
	t.Fatalf("deployment %s not ready within %v", name, timeout)
}

// RunningPodName returns the name of a Running, Ready, non-terminating pod
// matching the selector, waiting up to 2 minutes for one to appear.
func RunningPodName(t *testing.T, ctx context.Context, cs kubernetes.Interface, ns, labelSelector string) string {
	t.Helper()
	deadline := time.Now().Add(2 * time.Minute)
	for time.Now().Before(deadline) {
		pods, err := cs.CoreV1().Pods(ns).List(ctx, metav1.ListOptions{LabelSelector: labelSelector})
		if err == nil {
			for _, p := range pods.Items {
				// Skip pods that are terminating — a freshly-deleted pod
				// may still report Running while kubelet tears it down,
				// and targeting it would hit the old config.
				if p.DeletionTimestamp != nil {
					continue
				}
				if p.Status.Phase != corev1.PodRunning {
					continue
				}
				// Only accept a pod that's actually Ready.
				for _, c := range p.Status.Conditions {
					if c.Type == corev1.PodReady && c.Status == corev1.ConditionTrue {
						return p.Name
					}
				}
			}
		}
		time.Sleep(2 * time.Second)
	}
	t.Fatalf("no running pod found for selector %q in ns %s", labelSelector, ns)
	return ""
}

// ExecInPod runs a command in a pod's container, optionally feeding stdin,
// and returns stdout and stderr. Container may be empty for the default.
func ExecInPod(ctx context.Context, cs kubernetes.Interface, cfg *rest.Config, ns, pod, container string, cmd []string, stdin []byte) (string, string, error) {
	req := cs.CoreV1().RESTClient().Post().
		Resource("pods").
		Namespace(ns).
		Name(pod).
		SubResource("exec").
		VersionedParams(&corev1.PodExecOptions{
			Container: container,
			Command:   cmd,
			Stdin:     len(stdin) > 0,
			Stdout:    true,
			Stderr:    true,
		}, scheme.ParameterCodec)

	exec, err := remotecommand.NewSPDYExecutor(cfg, http.MethodPost, req.URL())
	if err != nil {
		return "", "", fmt.Errorf("create executor: %w", err)
	}
	var stdout, stderr bytes.Buffer
	var stdinR io.Reader
	if len(stdin) > 0 {
		stdinR = bytes.NewReader(stdin)
	}
	err = exec.StreamWithContext(ctx, remotecommand.StreamOptions{
		Stdin:  stdinR,
		Stdout: &stdout,
		Stderr: &stderr,
	})
	return stdout.String(), stderr.String(), err
}

// PodLogs returns the tail of a container's log.
func PodLogs(ctx context.Context, cs kubernetes.Interface, ns, pod, container string, tailLines int64) (string, error) {
	req := cs.CoreV1().Pods(ns).GetLogs(pod, &corev1.PodLogOptions{
		Container: container,
		TailLines: &tailLines,
	})
	raw, err := req.DoRaw(ctx)
	if err != nil {
		return "", err
	}
	return string(raw), nil
}

// PodRestartCount sums restartCount across all of the pod's containers.
// The live-reload tests assert this stays flat: a reload that bounced the
// container is a restart, not a reload.
func PodRestartCount(ctx context.Context, cs kubernetes.Interface, ns, pod string) (int32, error) {
	p, err := cs.CoreV1().Pods(ns).Get(ctx, pod, metav1.GetOptions{})
	if err != nil {
		return 0, err
	}
	var n int32
	for _, c := range p.Status.ContainerStatuses {
		n += c.RestartCount
	}
	return n, nil
}

// PortForwardPod finds a running pod matching selector and forwards remotePort
// to a local port. Returns the local port. The forward is torn down on test cleanup.
func PortForwardPod(t *testing.T, ctx context.Context, cs kubernetes.Interface, cfg *rest.Config, ns, labelSelector string, remotePort int) int {
	t.Helper()

	podName := RunningPodName(t, ctx, cs, ns, labelSelector)

	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("find free port: %v", err)
	}
	localPort := l.Addr().(*net.TCPAddr).Port
	l.Close()

	url := cs.CoreV1().RESTClient().Post().
		Resource("pods").
		Namespace(ns).
		Name(podName).
		SubResource("portforward").
		URL()

	transport, upgrader, err := spdy.RoundTripperFor(cfg)
	if err != nil {
		t.Fatalf("create round tripper: %v", err)
	}
	dialer := spdy.NewDialer(upgrader, &http.Client{Transport: transport}, http.MethodPost, url)

	stopCh := make(chan struct{})
	readyCh := make(chan struct{})
	ports := []string{fmt.Sprintf("%d:%d", localPort, remotePort)}
	fw, err := portforward.New(dialer, ports, stopCh, readyCh, io.Discard, io.Discard)
	if err != nil {
		t.Fatalf("create port-forward: %v", err)
	}

	go func() {
		if err := fw.ForwardPorts(); err != nil {
			select {
			case <-stopCh:
			default:
				t.Logf("port-forward error: %v", err)
			}
		}
	}()

	select {
	case <-readyCh:
	case <-time.After(30 * time.Second):
		t.Fatal("port-forward not ready within 30s")
	}

	t.Cleanup(func() { close(stopCh) })
	return localPort
}

// --- Memcache meta protocol helpers ---

// Addr formats a 127.0.0.1:port address.
func Addr(port int) string { return fmt.Sprintf("127.0.0.1:%d", port) }

type McResponse struct {
	Status string // "VA", "EN", "HD", "NF", "SERVER_ERROR", etc.
	Line   string // Full response line (trimmed).
	Value  string // For VA, the value bytes.
}

// McDo writes a single memcache meta command and reads the response.
// The command should be a complete meta request (without trailing \r\n).
// For `ms` with a value, include the value bytes after the command header.
func McDo(addr, cmd string, body []byte) (*McResponse, error) {
	conn, err := net.DialTimeout("tcp", addr, 3*time.Second)
	if err != nil {
		return nil, fmt.Errorf("dial %s: %w", addr, err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(5 * time.Second))

	if _, err := fmt.Fprintf(conn, "%s\r\n", cmd); err != nil {
		return nil, err
	}
	if len(body) > 0 {
		if _, err := conn.Write(body); err != nil {
			return nil, err
		}
		if _, err := conn.Write([]byte("\r\n")); err != nil {
			return nil, err
		}
	}

	r := bufio.NewReader(conn)
	line, err := r.ReadString('\n')
	if err != nil {
		return nil, fmt.Errorf("read response: %w", err)
	}
	line = strings.TrimRight(line, "\r\n")
	fields := strings.Fields(line)
	if len(fields) == 0 {
		return nil, fmt.Errorf("empty response")
	}
	resp := &McResponse{Status: fields[0], Line: line}

	// SERVER_ERROR is two tokens: "SERVER_ERROR <msg>"; reassemble.
	if resp.Status == "SERVER_ERROR" || resp.Status == "CLIENT_ERROR" || resp.Status == "ERROR" {
		return resp, nil
	}

	if resp.Status == "VA" {
		if len(fields) < 2 {
			return nil, fmt.Errorf("malformed VA: %q", line)
		}
		vlen, err := strconv.Atoi(fields[1])
		if err != nil {
			return nil, fmt.Errorf("bad VA length: %w", err)
		}
		buf := make([]byte, vlen+2)
		if _, err := io.ReadFull(r, buf); err != nil {
			return nil, fmt.Errorf("read VA body: %w", err)
		}
		resp.Value = string(buf[:vlen])
	}
	return resp, nil
}

// McGetWithRetry tries `mg <key> v` until it succeeds with a VA or times out.
// Returns the value or error.
func McGetWithRetry(addr, key string, attempts int) (string, error) {
	var last error
	for i := 0; i < attempts; i++ {
		resp, err := McDo(addr, fmt.Sprintf("mg %s v", key), nil)
		if err != nil {
			last = err
		} else if resp.Status == "VA" {
			return resp.Value, nil
		} else {
			last = fmt.Errorf("status=%s line=%q", resp.Status, resp.Line)
		}
		time.Sleep(300 * time.Millisecond)
	}
	return "", last
}

// McSet writes a value via `ms`.
func McSet(addr, key, value string) (*McResponse, error) {
	cmd := fmt.Sprintf("ms %s %d", key, len(value))
	return McDo(addr, cmd, []byte(value))
}

// McSetTTL writes a value via `ms` with a TTL. Used by the LWW tests to
// create entries with different remaining-TTL (`t`) flag values on each
// backend. ttlSeconds must be > 0.
func McSetTTL(addr, key, value string, ttlSeconds int) (*McResponse, error) {
	cmd := fmt.Sprintf("ms %s %d T%d", key, len(value), ttlSeconds)
	return McDo(addr, cmd, []byte(value))
}

// McDelete removes a key via `md`.
func McDelete(addr, key string) (*McResponse, error) {
	return McDo(addr, fmt.Sprintf("md %s", key), nil)
}

// RestartDeployment deletes pods matching labelSelector in ns and waits
// until at least one replacement pod is Running. Used by scale-cleanup paths
// to flush proxy backend connection state after a backend flapped.
func RestartDeployment(t *testing.T, ctx context.Context, cs kubernetes.Interface, ns, labelSelector string, timeout time.Duration) {
	t.Helper()
	pods, err := cs.CoreV1().Pods(ns).List(ctx, metav1.ListOptions{LabelSelector: labelSelector})
	if err != nil {
		t.Fatalf("list pods %q: %v", labelSelector, err)
	}
	for _, p := range pods.Items {
		_ = cs.CoreV1().Pods(ns).Delete(ctx, p.Name, metav1.DeleteOptions{})
	}
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		current, err := cs.CoreV1().Pods(ns).List(ctx, metav1.ListOptions{LabelSelector: labelSelector})
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
		time.Sleep(1 * time.Second)
	}
	t.Fatalf("no fresh pod running for %q within %v", labelSelector, timeout)
}

// ScaleDeployment sets the replica count on a deployment and waits for the
// observed ReadyReplicas to match. Timeout applies to the wait loop.
func ScaleDeployment(t *testing.T, ctx context.Context, cs kubernetes.Interface, ns, name string, replicas int32, timeout time.Duration) {
	t.Helper()
	scale, err := cs.AppsV1().Deployments(ns).GetScale(ctx, name, metav1.GetOptions{})
	if err != nil {
		t.Fatalf("get scale %s: %v", name, err)
	}
	scale.Spec.Replicas = replicas
	if _, err := cs.AppsV1().Deployments(ns).UpdateScale(ctx, name, scale, metav1.UpdateOptions{}); err != nil {
		t.Fatalf("update scale %s=%d: %v", name, replicas, err)
	}
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		dep, err := cs.AppsV1().Deployments(ns).Get(ctx, name, metav1.GetOptions{})
		if err == nil && dep.Status.ReadyReplicas == replicas && dep.Status.Replicas == replicas {
			return
		}
		time.Sleep(1 * time.Second)
	}
	t.Fatalf("deployment %s did not reach %d replicas within %v", name, replicas, timeout)
}
