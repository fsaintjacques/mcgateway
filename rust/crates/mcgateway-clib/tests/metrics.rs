//! Tests for the metrics registry, the token bucket, and the
//! exposition endpoint. The exposition golden pins the `OpenMetrics`
//! text format the chart's scrape config and the kind tests grep —
//! an encoder or naming change must show up as a reviewable diff
//! here, not as a silently reshaped scrape.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

// Same inclusion trick as tests/udf_loader.rs: the module is private
// to the cdylib crate, so pull the source in directly.
#[path = "../src/metrics.rs"]
#[allow(dead_code)]
mod metrics;

use metrics::{Metrics, TokenBucket};

/// One label set per family so the output order is fully
/// deterministic (families encode in registration order; label sets
/// within a family iterate a `HashMap`).
#[test]
fn exposition_golden() {
    let m = Metrics::new();
    m.set_registry_merges("builtin", 3);
    m.udf_rescan("ok");
    m.udf_rescan("ok");
    m.udf_module_failure("load-failed");
    m.reload_signal("config");
    m.merge_error("angry", "deadline");

    let expected = "\
# HELP mcgateway_registry_merges Registered merge functions by kind.
# TYPE mcgateway_registry_merges gauge
mcgateway_registry_merges{kind=\"builtin\"} 3
# HELP mcgateway_udf_rescans UDF directory rescans by result.
# TYPE mcgateway_udf_rescans counter
mcgateway_udf_rescans_total{result=\"ok\"} 2
# HELP mcgateway_udf_module_failures WASM modules skipped during a rescan by reason.
# TYPE mcgateway_udf_module_failures counter
mcgateway_udf_module_failures_total{reason=\"load-failed\"} 1
# HELP mcgateway_reload_signals Proxy reloads (SIGHUP) requested by the file watcher by trigger.
# TYPE mcgateway_reload_signals counter
mcgateway_reload_signals_total{trigger=\"config\"} 1
# HELP mcgateway_merge_errors Failed WASM merge calls by module and error kind.
# TYPE mcgateway_merge_errors counter
mcgateway_merge_errors_total{merge=\"angry\",kind=\"deadline\"} 1
# EOF
";
    assert_eq!(m.encode(), expected);
}

#[test]
fn empty_registry_encodes_bare_eof() {
    // prometheus-client skips families with no observations entirely
    // (descriptor included), so an idle registry is just the EOF
    // marker. Pinned because consumers must not assume a series
    // exists before its first observation.
    let m = Metrics::new();
    assert_eq!(m.encode(), "# EOF\n");
}

#[test]
fn multiple_label_sets_accumulate_independently() {
    let m = Metrics::new();
    m.merge_error("a", "trap");
    m.merge_error("a", "trap");
    m.merge_error("a", "deadline");
    m.merge_error("b", "trap");

    let out = m.encode();
    assert!(out.contains(r#"mcgateway_merge_errors_total{merge="a",kind="trap"} 2"#));
    assert!(out.contains(r#"mcgateway_merge_errors_total{merge="a",kind="deadline"} 1"#));
    assert!(out.contains(r#"mcgateway_merge_errors_total{merge="b",kind="trap"} 1"#));
}

const NANOS_PER_SEC: u64 = 1_000_000_000;

#[test]
fn token_bucket_burst_then_deny() {
    let b = TokenBucket::new(3, Duration::from_secs(1));
    assert!(b.try_acquire_at(1));
    assert!(b.try_acquire_at(2));
    assert!(b.try_acquire_at(3));
    assert!(!b.try_acquire_at(4), "burst exhausted, must deny");
}

#[test]
fn token_bucket_refills_over_time() {
    let b = TokenBucket::new(3, Duration::from_secs(1));
    for i in 0..3 {
        assert!(b.try_acquire_at(i));
    }
    assert!(!b.try_acquire_at(10));
    // One second later: exactly one token earned.
    assert!(b.try_acquire_at(NANOS_PER_SEC + 10));
    assert!(!b.try_acquire_at(NANOS_PER_SEC + 11));
}

#[test]
fn token_bucket_refill_caps_at_capacity() {
    let b = TokenBucket::new(2, Duration::from_secs(1));
    assert!(b.try_acquire_at(1));
    assert!(b.try_acquire_at(2));
    // A long quiet period earns at most `capacity` tokens, not one
    // per elapsed second.
    let later = 100 * NANOS_PER_SEC;
    assert!(b.try_acquire_at(later));
    assert!(b.try_acquire_at(later + 1));
    assert!(!b.try_acquire_at(later + 2), "capacity is the ceiling");
}

fn leak_metrics() -> &'static Metrics {
    Box::leak(Box::new(Metrics::new()))
}

fn http_get(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn exporter_serves_metrics() {
    let m = leak_metrics();
    m.udf_rescan("ok");
    let addr = metrics::spawn_exporter("127.0.0.1:0", m).unwrap();

    let response = http_get(addr, "GET /metrics HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("application/openmetrics-text"), "{response}");
    assert!(
        response.contains(r#"mcgateway_udf_rescans_total{result="ok"} 1"#),
        "{response}"
    );
    assert!(response.ends_with("# EOF\n"), "{response}");

    // The listener serves connections serially; a second scrape after
    // the first must work (no one-shot accept).
    let again = http_get(addr, "GET /metrics HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(again.starts_with("HTTP/1.1 200 OK\r\n"), "{again}");
}

#[test]
fn exporter_rejects_other_paths_and_methods() {
    let addr = metrics::spawn_exporter("127.0.0.1:0", leak_metrics()).unwrap();

    let not_found = http_get(addr, "GET /healthz HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(not_found.starts_with("HTTP/1.1 404 Not Found\r\n"), "{not_found}");

    let bad_method = http_get(addr, "POST /metrics HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(
        bad_method.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{bad_method}"
    );
}

#[test]
fn exporter_bind_failure_is_an_error_not_a_panic() {
    let err = metrics::spawn_exporter("256.256.256.256:0", leak_metrics()).unwrap_err();
    assert!(err.contains("bind"), "{err}");
}
