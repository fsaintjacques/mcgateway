//! Minimal leveled stderr logger behind the `log` facade.
//!
//! Still line-oriented stderr — what changed with stage 6 is levels
//! and storm-safety (rate limiting lives at the call sites, see
//! `metrics::log_limiter`), not a logging pipeline. memcached itself
//! logs to stderr, so the container's log stream stays one stream.

use std::io::Write as _;

/// Environment variable selecting the log level: `error`, `warn`,
/// `info` (default), `debug`, or `trace` (case-insensitive).
pub const LOG_ENV: &str = "MCGW_LOG";

struct StderrLogger;

static LOGGER: StderrLogger = StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        // Level filtering is handled globally via `log::set_max_level`.
        true
    }

    fn log(&self, record: &log::Record) {
        // One formatted write per line so concurrent threads don't
        // interleave fragments.
        let line = format!(
            "mcgw [{level}] {target}: {args}\n",
            level = record.level(),
            target = record.target(),
            args = record.args(),
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Install the logger and set the level from [`LOG_ENV`]. Idempotent:
/// if a logger is already installed (another library in the host
/// process got there first), leave it in place and do nothing.
pub fn init() {
    let level = std::env::var(LOG_ENV)
        .ok()
        .and_then(|v| v.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(level);
    }
}
