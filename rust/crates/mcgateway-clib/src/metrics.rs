//! Process-global metrics and their exposition endpoint.
//!
//! One registry per process, shared by every Lua state — the same
//! aggregation argument as the `SHARED` registries in `lib.rs`:
//! memcached runs a Lua VM per worker thread, so per-VM counters
//! would need cross-VM aggregation, while a process-global registry
//! has nothing to aggregate. Half the signal (WASM failures, UDF
//! rescans, reload triggers) never touches Lua anyway.
//!
//! Exposition is Prometheus/OpenMetrics text served by a listener
//! thread owned by this module, armed via [`METRICS_ADDR_ENV`] —
//! unset means off, mirroring how `MCGATEWAY_CONFIG` arms the config
//! watcher. The server is deliberately minimal: one endpoint, one
//! verb, serial connections, `Connection: close`. Scrapers poll on
//! the order of seconds; concurrency buys nothing here.
//!
//! Label values must come from configuration (pool, keyspace, merge
//! and module names), never from request traffic — the cardinality
//! contract from the stage 6 plan. Anything request-derived buckets
//! into fixed sentinels at the call site.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// Environment variable naming the address the `/metrics` endpoint
/// binds (e.g. `0.0.0.0:9151`). Unset → no exposition; standalone
/// deployments keep today's behaviour.
pub const METRICS_ADDR_ENV: &str = "MCGW_METRICS_ADDR";

/// Request/backend latency buckets: 100 µs → ~1.6 s, doubling.
/// Memcache round trips live at the low end, backend timeouts at the
/// top.
const DURATION_BUCKETS: [f64; 15] = [
    0.0001, 0.0002, 0.0004, 0.0008, 0.0016, 0.0032, 0.0064, 0.0128, 0.0256, 0.0512, 0.1024,
    0.2048, 0.4096, 0.8192, 1.6384,
];

/// Merge duration buckets: 10 µs → ~80 ms, doubling. The default WASM
/// deadline (50 ms) sits inside the range, so deadline kills are
/// visible as a cliff rather than falling off the histogram.
const MERGE_BUCKETS: [f64; 14] = [
    0.00001, 0.00002, 0.00004, 0.00008, 0.00016, 0.00032, 0.00064, 0.00128, 0.00256, 0.00512,
    0.01024, 0.02048, 0.04096, 0.08192,
];

fn duration_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS.iter().copied())
}

fn merge_histogram() -> Histogram {
    Histogram::new(MERGE_BUCKETS.iter().copied())
}

/// Histogram families need an explicit constructor (buckets aren't
/// `Default`); the `fn()` pointer keeps the Family type nameable.
type HistogramFamily<S> = Family<S, Histogram, fn() -> Histogram>;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabels {
    pub kind: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabels {
    pub result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabels {
    pub reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TriggerLabels {
    pub trigger: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MergeErrorLabels {
    pub merge: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RequestLabels {
    pub keyspace: String,
    pub op: &'static str,
    pub outcome: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KeyspaceOpLabels {
    pub keyspace: String,
    pub op: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct BackendLabels {
    pub pool: String,
    pub status: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PoolLabels {
    pub pool: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MergeLabels {
    pub merge: String,
}

/// Every metric the gateway exposes. Fields are `Family` handles —
/// cheap clones over shared state — so call sites can cache them; the
/// `Registry` is only walked at encode time.
pub struct Metrics {
    registry: Registry,
    /// Requests by keyspace, `op` = `read` | `write`, and outcome
    /// (reads: `hit`|`miss`|`error`; writes: `stored`|`negative`|`error`).
    pub requests: Family<RequestLabels, Counter>,
    /// End-to-end handler duration by keyspace and op.
    pub request_duration: HistogramFamily<KeyspaceOpLabels>,
    /// Per-pool read results by status, one per pool per fan-out.
    pub backend_requests: Family<BackendLabels, Counter>,
    /// Per-pool backend latency (memcached's `res:elapsed()`).
    pub backend_duration: HistogramFamily<PoolLabels>,
    /// Time spent inside the merge function, by merge name.
    pub merge_duration: HistogramFamily<MergeLabels>,
    /// Failed WASM merge calls, by module and error kind. These are
    /// the failures `Merge::apply` masks as `Miss` — the counter is
    /// what makes the masking observable.
    pub merge_errors: Family<MergeErrorLabels, Counter>,
    /// Config loads by `result` = `ok` | `fallback`. A climbing
    /// `fallback` count is the alertable form of "keeping previous
    /// config".
    pub config_reloads: Family<ResultLabels, Counter>,
    /// Shape of the config currently serving (last good on fallback).
    pub config_pools: Gauge,
    pub config_keyspaces: Gauge,
    /// Registered merge functions, by `kind` = `builtin` | `wasm`.
    pub registry_merges: Family<KindLabels, Gauge>,
    /// UDF directory rescans, by `result` = `ok` | `error`.
    pub udf_rescans: Family<ResultLabels, Counter>,
    /// Modules skipped during a rescan, by coarse `reason`.
    pub udf_module_failures: Family<ReasonLabels, Counter>,
    /// SIGHUP reload requests raised by the watcher, by `trigger` =
    /// `config` | `udf-swap`.
    pub reload_signals: Family<TriggerLabels, Counter>,
}

impl Metrics {
    #[must_use]
    #[allow(clippy::too_many_lines)] // linear register-and-collect, no logic
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let requests = Family::<RequestLabels, Counter>::default();
        registry.register(
            "mcgateway_requests",
            "Requests by keyspace, op, and outcome",
            requests.clone(),
        );
        let request_duration =
            HistogramFamily::<KeyspaceOpLabels>::new_with_constructor(duration_histogram);
        registry.register(
            "mcgateway_request_duration_seconds",
            "Request handler duration by keyspace and op",
            request_duration.clone(),
        );
        let backend_requests = Family::<BackendLabels, Counter>::default();
        registry.register(
            "mcgateway_backend_requests",
            "Per-pool backend results by status",
            backend_requests.clone(),
        );
        let backend_duration =
            HistogramFamily::<PoolLabels>::new_with_constructor(duration_histogram);
        registry.register(
            "mcgateway_backend_duration_seconds",
            "Per-pool backend request latency",
            backend_duration.clone(),
        );
        let merge_duration = HistogramFamily::<MergeLabels>::new_with_constructor(merge_histogram);
        registry.register(
            "mcgateway_merge_duration_seconds",
            "Merge function execution time by merge name",
            merge_duration.clone(),
        );
        let merge_errors = Family::<MergeErrorLabels, Counter>::default();
        registry.register(
            "mcgateway_merge_errors",
            "Failed WASM merge calls by module and error kind",
            merge_errors.clone(),
        );
        let config_reloads = Family::<ResultLabels, Counter>::default();
        registry.register(
            "mcgateway_config_reloads",
            "Config loads by result (fallback = kept previous config)",
            config_reloads.clone(),
        );
        let config_pools = Gauge::default();
        registry.register(
            "mcgateway_config_pools",
            "Pools in the config currently serving",
            config_pools.clone(),
        );
        let config_keyspaces = Gauge::default();
        registry.register(
            "mcgateway_config_keyspaces",
            "Keyspaces in the config currently serving",
            config_keyspaces.clone(),
        );
        let registry_merges = Family::<KindLabels, Gauge>::default();
        registry.register(
            "mcgateway_registry_merges",
            "Registered merge functions by kind",
            registry_merges.clone(),
        );
        let udf_rescans = Family::<ResultLabels, Counter>::default();
        registry.register(
            "mcgateway_udf_rescans",
            "UDF directory rescans by result",
            udf_rescans.clone(),
        );
        let udf_module_failures = Family::<ReasonLabels, Counter>::default();
        registry.register(
            "mcgateway_udf_module_failures",
            "WASM modules skipped during a rescan by reason",
            udf_module_failures.clone(),
        );
        let reload_signals = Family::<TriggerLabels, Counter>::default();
        registry.register(
            "mcgateway_reload_signals",
            "Proxy reloads (SIGHUP) requested by the file watcher by trigger",
            reload_signals.clone(),
        );
        Self {
            registry,
            requests,
            request_duration,
            backend_requests,
            backend_duration,
            merge_duration,
            merge_errors,
            config_reloads,
            config_pools,
            config_keyspaces,
            registry_merges,
            udf_rescans,
            udf_module_failures,
            reload_signals,
        }
    }

    /// Record a finished request: outcome counter plus, when the
    /// caller supplied a start timestamp, the duration histogram.
    pub fn observe_request(
        &self,
        keyspace: &str,
        op: &'static str,
        outcome: &'static str,
        duration_seconds: Option<f64>,
    ) {
        self.requests
            .get_or_create(&RequestLabels {
                keyspace: keyspace.to_owned(),
                op,
                outcome,
            })
            .inc();
        if let Some(seconds) = duration_seconds {
            self.request_duration
                .get_or_create(&KeyspaceOpLabels {
                    keyspace: keyspace.to_owned(),
                    op,
                })
                .observe(seconds);
        }
    }

    /// Record one pool's result within a fan-out read.
    pub fn observe_backend(&self, pool: &str, status: &'static str, elapsed_seconds: Option<f64>) {
        self.backend_requests
            .get_or_create(&BackendLabels {
                pool: pool.to_owned(),
                status,
            })
            .inc();
        if let Some(seconds) = elapsed_seconds {
            self.backend_duration
                .get_or_create(&PoolLabels {
                    pool: pool.to_owned(),
                })
                .observe(seconds);
        }
    }

    pub fn observe_merge_duration(&self, merge: &str, seconds: f64) {
        self.merge_duration
            .get_or_create(&MergeLabels {
                merge: merge.to_owned(),
            })
            .observe(seconds);
    }

    pub fn config_reload(&self, result: &'static str, pools: usize, keyspaces: usize) {
        self.config_reloads
            .get_or_create(&ResultLabels { result })
            .inc();
        self.config_pools.set(i64::try_from(pools).unwrap_or(i64::MAX));
        self.config_keyspaces
            .set(i64::try_from(keyspaces).unwrap_or(i64::MAX));
    }

    pub fn set_registry_merges(&self, kind: &'static str, n: usize) {
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        self.registry_merges.get_or_create(&KindLabels { kind }).set(n);
    }

    pub fn udf_rescan(&self, result: &'static str) {
        self.udf_rescans.get_or_create(&ResultLabels { result }).inc();
    }

    pub fn udf_module_failure(&self, reason: &'static str) {
        self.udf_module_failures
            .get_or_create(&ReasonLabels { reason })
            .inc();
    }

    pub fn reload_signal(&self, trigger: &'static str) {
        self.reload_signals
            .get_or_create(&TriggerLabels { trigger })
            .inc();
    }

    pub fn merge_error(&self, merge: &str, kind: &'static str) {
        self.merge_errors
            .get_or_create(&MergeErrorLabels {
                merge: merge.to_owned(),
                kind,
            })
            .inc();
    }

    /// Render the registry in `OpenMetrics` text format (trailing
    /// `# EOF` included).
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing to a String cannot fail; the Err type exists only
        // because the encoder is generic over fmt::Write.
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("encoding to String is infallible");
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-global instance every call site records into.
pub fn global() -> &'static Metrics {
    static GLOBAL: OnceLock<Metrics> = OnceLock::new();
    GLOBAL.get_or_init(Metrics::new)
}

/// Token bucket for rate-limited logging on per-request paths. A bad
/// UDF at full request rate must not produce a log line per request:
/// the counter carries the true rate, the log carries a sampled
/// diagnostic. Atomics with benign races — an occasional extra or
/// missed line is fine for logs.
pub struct TokenBucket {
    capacity: u64,
    nanos_per_token: u64,
    origin: Instant,
    /// Nanoseconds since `origin` up to which refill was credited.
    refilled_to: AtomicU64,
    tokens: AtomicU64,
}

impl TokenBucket {
    /// A bucket holding at most `capacity` tokens (the burst), earning
    /// one token per `refill_interval`. Starts full.
    #[must_use]
    pub fn new(capacity: u64, refill_interval: Duration) -> Self {
        let nanos = u64::try_from(refill_interval.as_nanos().max(1)).unwrap_or(u64::MAX);
        Self {
            capacity,
            nanos_per_token: nanos,
            origin: Instant::now(),
            refilled_to: AtomicU64::new(0),
            tokens: AtomicU64::new(capacity),
        }
    }

    /// Take one token if available.
    pub fn try_acquire(&self) -> bool {
        let now = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.try_acquire_at(now)
    }

    /// Clock-injected form of [`Self::try_acquire`]; `now` is
    /// nanoseconds since construction. Public as a test seam.
    #[doc(hidden)]
    pub fn try_acquire_at(&self, now: u64) -> bool {
        let last = self.refilled_to.load(Ordering::Relaxed);
        if now > last {
            let earned = (now - last) / self.nanos_per_token;
            // Credit whole tokens only, and only from one thread per
            // window: the CAS on `refilled_to` elects it.
            if earned > 0
                && self
                    .refilled_to
                    .compare_exchange(
                        last,
                        last + earned * self.nanos_per_token,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                let cap = self.capacity;
                let _ = self
                    .tokens
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                        Some((t.saturating_add(earned)).min(cap))
                    });
            }
        }
        self.tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| t.checked_sub(1))
            .is_ok()
    }
}

/// The limiter used for per-merge-failure log lines: a burst of 10,
/// then one line per second, shared across all modules.
pub fn log_limiter() -> &'static TokenBucket {
    static LIMITER: OnceLock<TokenBucket> = OnceLock::new();
    LIMITER.get_or_init(|| TokenBucket::new(10, Duration::from_secs(1)))
}

/// Bind `addr` and serve `metrics` from a background thread. Returns
/// the bound address (which differs from `addr` when it asked for
/// port 0, as tests do). Failure to bind is the caller's problem to
/// report — a gateway that cannot expose metrics must still serve
/// traffic, so callers log and continue rather than propagate.
pub fn spawn_exporter(addr: &str, metrics: &'static Metrics) -> Result<SocketAddr, String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let local = listener
        .local_addr()
        .map_err(|e| format!("local_addr of {addr}: {e}"))?;
    std::thread::Builder::new()
        .name("mcgw-metrics".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Per-connection failures (client hangup, timeout) are
                // the client's issue; the next scrape starts fresh.
                let _ = serve_one(&mut stream, metrics);
            }
        })
        .map_err(|e| format!("spawn metrics thread: {e}"))?;
    Ok(local)
}

/// Timeout guarding both halves of a scrape so a stalled client
/// cannot wedge the (single-threaded) exporter.
const SCRAPE_IO_TIMEOUT: Duration = Duration::from_secs(5);

fn serve_one(stream: &mut TcpStream, metrics: &Metrics) -> std::io::Result<()> {
    stream.set_read_timeout(Some(SCRAPE_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(SCRAPE_IO_TIMEOUT))?;

    // Read until end-of-headers or 4 KiB, whichever first. Only the
    // request line matters; the rest is drained so well-behaved
    // clients don't see a reset before the response.
    let mut buf = [0u8; 4096];
    let mut n = 0;
    loop {
        if n == buf.len() {
            break;
        }
        let read = stream.read(&mut buf[n..])?;
        if read == 0 {
            break;
        }
        n += read;
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let request = &buf[..n];
    let line_end = request
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(request.len());
    let mut parts = request[..line_end].split(|b| *b == b' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();

    let (status, content_type, body) = if method != b"GET" {
        (
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "method not allowed\n".to_owned(),
        )
    } else if path == b"/metrics" || path.starts_with(b"/metrics?") {
        (
            "200 OK",
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
            metrics.encode(),
        )
    } else {
        (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found; metrics are at /metrics\n".to_owned(),
        )
    };

    let mut response = String::with_capacity(body.len() + 128);
    write!(
        response,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        len = body.len(),
    )
    .expect("writing to String is infallible");
    response.push_str(&body);
    stream.write_all(response.as_bytes())?;
    stream.flush()
}
