//! Plumbing tests for the live-reload watcher: real filesystem events
//! through `notify`, fake actions through channels. No signals, no
//! wasm compilation — those are wired in lib.rs and exercised by the
//! kind suite.

use std::fs;
use std::sync::mpsc;
use std::time::Duration;

// Same inclusion trick as tests/udf_loader.rs: the module is private
// to the cdylib crate, so pull the source in directly.
#[path = "../src/watcher.rs"]
#[allow(dead_code)]
mod watcher;

/// Generous because macOS `FSEvents` delivery is lazy; Linux inotify
/// is effectively instant.
const WAIT: Duration = Duration::from_secs(10);

/// Long enough for a debounce window plus event delivery; used to
/// assert that nothing fires.
const QUIET: Duration = Duration::from_secs(1);

struct Fixture {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    udf: Option<std::path::PathBuf>,
    rx: mpsc::Receiver<&'static str>,
}

fn fixture(with_udf: bool) -> Fixture {
    fixture_with(with_udf, |_| {})
}

fn fixture_with(with_udf: bool, tune: impl FnOnce(&mut watcher::Plan)) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config.lua");
    fs::write(&config, "return {}").unwrap();
    let udf = with_udf.then(|| {
        let d = tmp.path().join("udf");
        fs::create_dir(&d).unwrap();
        d
    });

    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    let mut plan = watcher::Plan::new(config.clone(), udf.clone());
    tune(&mut plan);
    watcher::spawn(
        plan,
        move || tx2.send("rescan").unwrap(),
        move |trigger| {
            tx.send(match trigger {
                watcher::ReloadTrigger::Config => "reload-config",
                watcher::ReloadTrigger::UdfSwap => "reload-udf",
            })
            .unwrap();
        },
    )
    .unwrap();

    let f = Fixture {
        _tmp: tmp,
        config,
        udf,
        rx,
    };
    // FSEvents on macOS can deliver events from just *before* the
    // stream started — including the fixture's own initial writes.
    // Drain to quiescence so tests only observe what they caused.
    while f.rx.recv_timeout(QUIET).is_ok() {}
    f
}

#[test]
fn rename_commit_triggers_reload() {
    let f = fixture(false);

    // Commit the way the operator will: temp file in the same
    // directory, then rename over the config.
    let staged = f.config.with_extension("lua.tmp");
    fs::write(&staged, "return { pools = {} }").unwrap();
    fs::rename(&staged, &f.config).unwrap();

    assert_eq!(f.rx.recv_timeout(WAIT), Ok("reload-config"));
}

#[test]
fn in_place_write_triggers_reload() {
    let f = fixture(false);

    // kubectl-cp-style write: same inode, no rename.
    fs::write(&f.config, "return { keyspaces = {} }").unwrap();

    assert_eq!(f.rx.recv_timeout(WAIT), Ok("reload-config"));
}

#[test]
fn wasm_drop_rescans_then_reloads() {
    let f = fixture(true);

    fs::write(f.udf.as_ref().unwrap().join("m.wasm"), b"\0asm").unwrap();

    // Same debounce batch: rescan strictly before the reload it
    // triggers, so config re-validation sees the fresh table.
    assert_eq!(f.rx.recv_timeout(WAIT), Ok("rescan"));
    assert_eq!(f.rx.recv_timeout(WAIT), Ok("reload-udf"));
}

#[test]
fn config_and_wasm_batch_coalesces_to_one_reload() {
    // A wide debounce so both writes land in one batch even under
    // FSEvents' lazy delivery — the point here is coalescing order,
    // not window sizing.
    let f = fixture_with(true, |p| p.debounce = Duration::from_secs(2));

    fs::write(f.udf.as_ref().unwrap().join("m.wasm"), b"\0asm").unwrap();
    fs::write(&f.config, "return { pools = {} }").unwrap();

    assert_eq!(f.rx.recv_timeout(WAIT), Ok("rescan"));
    assert_eq!(f.rx.recv_timeout(WAIT), Ok("reload-udf"));
    // One batch, one reload: nothing else queued.
    assert_eq!(f.rx.recv_timeout(QUIET), Err(mpsc::RecvTimeoutError::Timeout));
}

#[test]
fn sustained_event_stream_cannot_defer_reload_past_batch_age() {
    let f = fixture_with(false, |p| {
        p.debounce = Duration::from_millis(200);
        p.max_batch_age = Duration::from_millis(800);
    });

    // Write continuously at half the debounce period: quiet never
    // happens, so only the age cap can close the batch.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (stop, config) = (stop.clone(), f.config.clone());
        std::thread::spawn(move || {
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                fs::write(&config, format!("return {{ n = {i} }}")).unwrap();
                i += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let got = f.rx.recv_timeout(WAIT);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    assert_eq!(got, Ok("reload-config"));
}

#[test]
fn unrelated_files_are_ignored() {
    let f = fixture(true);

    // Staged temp files (committer work-in-progress) and unrelated
    // names must not trigger anything.
    fs::write(f.config.parent().unwrap().join("scratch.txt"), b"x").unwrap();
    fs::write(f.udf.as_ref().unwrap().join("m.wasm.tmp"), b"x").unwrap();

    assert_eq!(f.rx.recv_timeout(QUIET), Err(mpsc::RecvTimeoutError::Timeout));
}
