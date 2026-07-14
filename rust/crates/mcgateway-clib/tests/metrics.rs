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
/// within a family iterate a `HashMap`). Histograms are pinned by the
/// dedicated test below — their bucket fan-out would drown this one.
#[test]
fn exposition_golden() {
    let m = Metrics::new();
    m.observe_request("user", "read", "hit", None);
    m.observe_backend("mc-a", "hit", None);
    m.merge_error("angry", "deadline");
    m.config_reload("ok", 2, 5);
    m.set_registry_merges("builtin", 3);
    m.udf_rescan("ok");
    m.udf_rescan("ok");
    m.udf_module_failure("load-failed");
    m.reload_signal("config");

    let expected = "\
# HELP mcgateway_requests Requests by keyspace, op, and outcome.
# TYPE mcgateway_requests counter
mcgateway_requests_total{keyspace=\"user\",op=\"read\",outcome=\"hit\"} 1
# HELP mcgateway_backend_requests Per-pool backend results by status.
# TYPE mcgateway_backend_requests counter
mcgateway_backend_requests_total{pool=\"mc-a\",status=\"hit\"} 1
# HELP mcgateway_merge_errors Failed WASM merge calls by module and error kind.
# TYPE mcgateway_merge_errors counter
mcgateway_merge_errors_total{merge=\"angry\",kind=\"deadline\"} 1
# HELP mcgateway_config_reloads Config loads by result (fallback = kept previous config).
# TYPE mcgateway_config_reloads counter
mcgateway_config_reloads_total{result=\"ok\"} 1
# HELP mcgateway_config_pools Pools in the config currently serving.
# TYPE mcgateway_config_pools gauge
mcgateway_config_pools 2
# HELP mcgateway_config_keyspaces Keyspaces in the config currently serving.
# TYPE mcgateway_config_keyspaces gauge
mcgateway_config_keyspaces 5
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
# EOF
";
    assert_eq!(m.encode(), expected);
}

#[test]
fn empty_registry_encodes_gauges_and_eof() {
    // prometheus-client skips *families* with no observations entirely
    // (descriptor included) — consumers must not assume a series
    // exists before its first observation. Bare gauges (config shape)
    // always encode, starting at zero.
    let m = Metrics::new();
    let expected = "\
# HELP mcgateway_config_pools Pools in the config currently serving.
# TYPE mcgateway_config_pools gauge
mcgateway_config_pools 0
# HELP mcgateway_config_keyspaces Keyspaces in the config currently serving.
# TYPE mcgateway_config_keyspaces gauge
mcgateway_config_keyspaces 0
# EOF
";
    assert_eq!(m.encode(), expected);
}

#[test]
fn histogram_exposition_shape() {
    let m = Metrics::new();
    // 0.0003 lands in the 0.0004 bucket; 0.05 in 0.0512.
    m.observe_request("user", "read", "hit", Some(0.0003));
    m.observe_backend("mc-a", "hit", Some(0.05));
    m.observe_merge_duration("first-hit", 0.00002);

    let out = m.encode();
    assert!(out.contains("# TYPE mcgateway_request_duration_seconds histogram"), "{out}");
    assert!(
        out.contains(
            r#"mcgateway_request_duration_seconds_bucket{le="0.0004",keyspace="user",op="read"} 1"#
        ),
        "{out}"
    );
    assert!(
        out.contains(r#"mcgateway_request_duration_seconds_count{keyspace="user",op="read"} 1"#),
        "{out}"
    );
    assert!(
        out.contains(r#"mcgateway_request_duration_seconds_sum{keyspace="user",op="read"} 0.0003"#),
        "{out}"
    );
    // The +Inf bucket must exist so the histogram is complete.
    assert!(
        out.contains(
            r#"mcgateway_request_duration_seconds_bucket{le="+Inf",keyspace="user",op="read"} 1"#
        ),
        "{out}"
    );
    assert!(
        out.contains(r#"mcgateway_backend_duration_seconds_bucket{le="0.0512",pool="mc-a"} 1"#),
        "{out}"
    );
    assert!(
        out.contains(r#"mcgateway_merge_duration_seconds_bucket{le="2e-5",merge="first-hit"} 1"#)
            || out.contains(
                r#"mcgateway_merge_duration_seconds_bucket{le="0.00002",merge="first-hit"} 1"#
            ),
        "{out}"
    );
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

/// Micro-benchmark for the per-read observation set (stage 6 exit
/// criterion: ≤ 2 µs added per request). Ignored by default — timing
/// assertions flake under CI load; run manually with
/// `cargo test -p mcgateway-clib --test metrics -- --ignored --nocapture`.
#[test]
#[ignore = "manual benchmark, run with --ignored --nocapture"]
fn bench_read_observation_set() {
    const ITERS: u32 = 200_000;
    let m = leak_metrics();
    // Steady state: label sets already exist (the first request per
    // keyspace pays the insert; every later one is the lookup path).
    m.observe_request("user", "read", "hit", Some(0.0005));
    m.observe_backend("mc-a", "hit", Some(0.0002));
    m.observe_backend("mc-b", "miss", Some(0.0003));
    m.observe_merge_duration("first-hit", 0.00002);

    let start = std::time::Instant::now();
    for _ in 0..ITERS {
        // One fan-out read over two pools: request outcome + duration,
        // two backend observations, one merge duration.
        m.observe_request("user", "read", "hit", Some(0.0005));
        m.observe_backend("mc-a", "hit", Some(0.0002));
        m.observe_backend("mc-b", "miss", Some(0.0003));
        m.observe_merge_duration("first-hit", 0.00002);
    }
    let per_request = start.elapsed() / ITERS;
    println!("read observation set: {per_request:?} per request");
    assert!(
        per_request < Duration::from_micros(2),
        "budget is 2 µs, measured {per_request:?}"
    );
}

/// A client trickling bytes must not hold the (serial) exporter past
/// the connection deadline: the budget spans the whole scrape, not
/// each read — a per-syscall timeout would reset on every byte.
#[test]
fn slow_client_is_cut_off_at_the_deadline() {
    use std::net::TcpListener;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let m = leak_metrics();

    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        // A request line that never finishes, one byte at a time —
        // each read succeeds, so only a whole-connection deadline can
        // end this. Stop when the server hangs up.
        for _ in 0..500 {
            if c.write_all(b"G").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });

    let (mut server, _) = listener.accept().unwrap();
    let started = Instant::now();
    let err = metrics::serve_one_until(&mut server, m, started + Duration::from_millis(200))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cut-off took {:?}, want ~the 200ms deadline",
        started.elapsed()
    );
    // Deadline exhaustion mid-loop reports TimedOut; a read blocked at
    // expiry surfaces the socket timeout (WouldBlock on unix).
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
        "unexpected error: {err}"
    );
    drop(server);
    client.join().unwrap();
}
