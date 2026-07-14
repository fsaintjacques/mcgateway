//! Live-reload triggers for the two file surfaces the gateway consumes:
//! the config file (`$MCGATEWAY_CONFIG`) and the UDF directory.
//!
//! One background thread owns a `notify` watcher and a debounce loop.
//! The config watch is on the file's *parent directory*: atomic
//! rename commits replace the inode, so a watch on the file itself
//! would go stale after the first swap. The UDF watch reacts only to
//! `*.wasm` entries, so committers can stage temp files in the same
//! directory without triggering spurious work.
//!
//! A batch touching the UDF directory rescans first and then always
//! requests a reload: a config that failed validation because its
//! merge module had not been registered yet gets re-evaluated once
//! the registry is fresh (see the stage-4 plan, merge-name
//! resolution). Batches touching only the config file skip the
//! rescan. Rescan and reload are injected as closures so tests can
//! observe the plumbing without sending signals or compiling wasm.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

/// Why a reload is being requested. Carried out through the `reload`
/// closure so the caller can label its metrics without this module
/// depending on them (the watcher source is also compiled standalone
/// by its integration tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadTrigger {
    /// The batch touched only the config file.
    Config,
    /// The batch touched the UDF directory: the registry was
    /// rescanned first and the reload re-validates the config against
    /// the fresh table (the stage-4 race-closing re-raise).
    UdfSwap,
}

impl ReloadTrigger {
    /// Stable lowercase form, used as a metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::UdfSwap => "udf-swap",
        }
    }
}

/// Quiet period after the last event before a batch is acted on.
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// Upper bound on how long a batch can absorb events before acting
/// anyway. Without it, a sustained event stream (an operator syncing
/// many modules, a slow large copy emitting periodic writes) would
/// extend the quiet window forever and defer the reload indefinitely.
pub const MAX_BATCH_AGE: Duration = Duration::from_secs(2);

pub struct Plan {
    /// The config file to watch. Its parent directory must exist.
    pub config_path: PathBuf,
    /// The UDF directory, when one resolved at startup.
    pub udf_dir: Option<PathBuf>,
    /// Quiet period closing a batch. Injectable for tests.
    pub debounce: Duration,
    /// Hard cap on a batch's age. Injectable for tests.
    pub max_batch_age: Duration,
}

impl Plan {
    pub fn new(config_path: PathBuf, udf_dir: Option<PathBuf>) -> Self {
        Self {
            config_path,
            udf_dir,
            debounce: DEBOUNCE,
            max_batch_age: MAX_BATCH_AGE,
        }
    }
}

/// Arm the watches and spawn the debounce thread. Returns once the
/// OS-level watches are registered, so changes landing after `spawn`
/// returns are never missed. There is no shutdown path: the thread
/// lives until process exit, same as the proxy it signals.
pub fn spawn(
    plan: Plan,
    rescan: impl Fn() + Send + 'static,
    reload: impl Fn(ReloadTrigger) + Send + 'static,
) -> Result<(), String> {
    // Canonicalise both watch roots: event paths come back canonical
    // (macOS reports through /private, mounts may sit behind
    // symlinks like /var/run -> /run) and are compared by prefix.
    let config_dir = plan
        .config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            format!(
                "config path {} has no parent directory",
                plan.config_path.display()
            )
        })?
        .canonicalize()
        .map_err(|e| {
            format!(
                "config dir of {} not watchable: {e}",
                plan.config_path.display()
            )
        })?;
    let config_name = plan
        .config_path
        .file_name()
        .ok_or_else(|| format!("config path {} has no file name", plan.config_path.display()))?
        .to_os_string();
    let udf_dir = match plan.udf_dir {
        Some(dir) => Some(
            dir.canonicalize()
                .map_err(|e| format!("udf dir {} not watchable: {e}", dir.display()))?,
        ),
        None => None,
    };
    let (debounce, max_batch_age) = (plan.debounce, plan.max_batch_age);

    let (tx, rx) = mpsc::channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })
    .map_err(|e| format!("watcher init: {e}"))?;

    watcher
        .watch(&config_dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("watch {}: {e}", config_dir.display()))?;
    if let Some(dir) = &udf_dir {
        watcher
            .watch(dir, RecursiveMode::NonRecursive)
            .map_err(|e| format!("watch {}: {e}", dir.display()))?;
    }

    thread::Builder::new()
        .name("mcgw-watcher".into())
        .spawn(move || {
            // The watcher moves onto the thread so the OS watches live
            // exactly as long as the debounce loop consuming them.
            let _watcher = watcher;

            let touches_config = |ev: &notify::Event| {
                ev.paths.iter().any(|p| {
                    p.parent() == Some(config_dir.as_path())
                        && p.file_name() == Some(config_name.as_os_str())
                })
            };
            let touches_udf = |ev: &notify::Event| {
                let Some(dir) = &udf_dir else { return false };
                ev.paths.iter().any(|p| {
                    p == dir
                        || (p.parent() == Some(dir.as_path())
                            && p.extension().and_then(|e| e.to_str()) == Some("wasm"))
                })
            };

            loop {
                let Ok(first) = rx.recv() else { return };
                let batch_start = Instant::now();
                let mut config_dirty = touches_config(&first);
                let mut udf_dirty = touches_udf(&first);
                loop {
                    // Close the batch after a quiet debounce period —
                    // or when it hits max age, so a sustained event
                    // stream cannot defer the reload forever. A
                    // timeout on the shortened wait near the age cap
                    // is treated the same as quiet: act now, and let
                    // any still-arriving events open the next batch.
                    let elapsed = batch_start.elapsed();
                    if elapsed >= max_batch_age {
                        break;
                    }
                    let wait = debounce.min(max_batch_age.saturating_sub(elapsed));
                    match rx.recv_timeout(wait) {
                        Ok(ev) => {
                            config_dirty |= touches_config(&ev);
                            udf_dirty |= touches_udf(&ev);
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                if udf_dirty {
                    rescan();
                }
                if config_dirty || udf_dirty {
                    reload(if udf_dirty {
                        ReloadTrigger::UdfSwap
                    } else {
                        ReloadTrigger::Config
                    });
                }
            }
        })
        .map_err(|e| format!("spawn watcher thread: {e}"))?;

    Ok(())
}
