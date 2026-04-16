//! Round-trip tests of the host codec against hand-written `.wat`
//! fixtures. Fixtures exist so we can pin down host behaviour without
//! depending on the SDK (which lands in step 2).

use mcgateway_core::{Entry, Merge, MergeResult, Status};
use mcgateway_wasm_host::{run_test as run, WasmHost, WasmMerge};

fn load(path: &str) -> Vec<u8> {
    let wat = std::fs::read_to_string(format!("tests/fixtures/{path}")).unwrap();
    wat::parse_str(&wat).unwrap()
}

fn sample_entries() -> Vec<(Vec<u8>, String)> {
    vec![
        (b"user:1".to_vec(), "pool-a".to_string()),
        (b"user:1".to_vec(), "pool-b".to_string()),
        (b"user:1".to_vec(), "pool-c".to_string()),
    ]
}

fn views(owned: &[(Vec<u8>, String)]) -> Vec<Entry<'_>> {
    owned
        .iter()
        .enumerate()
        .map(|(i, (k, p))| Entry {
            key: k,
            pool: p,
            status: if i == 0 { Status::Hit } else { Status::Miss },
            t: if i == 0 { Some(42) } else { None },
            value: None,
            line: None,
        })
        .collect()
}

#[test]
fn miss_fixture_returns_miss() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("miss.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    let result = run(host.engine(), &module, &entries).unwrap();
    assert!(matches!(result, MergeResult::Miss));
}

#[test]
fn winner_zero_fixture_returns_index_zero() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("winner_zero.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    let result = run(host.engine(), &module, &entries).unwrap();
    assert!(matches!(result, MergeResult::Winner(0)));
}

#[test]
fn winner_from_entries_uses_count_argument() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("winner_from_entries.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    let result = run(host.engine(), &module, &entries).unwrap();
    assert!(matches!(result, MergeResult::Winner(2)));
}

#[test]
fn trap_fixture_run_returns_err_merge_returns_miss() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("trap.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);

    // run() surfaces the trap verbatim so callers can log it.
    let direct = run(host.engine(), &module, &entries);
    assert!(direct.is_err(), "run() must propagate traps");

    // The Merge impl swallows it and degrades to Miss.
    let merge = WasmMerge::from_module(&host, &module, "fixture").unwrap();
    let result = merge.apply(&entries);
    assert!(matches!(result, MergeResult::Miss));
}

#[test]
fn bad_abi_is_rejected_at_load() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("bad_abi.wat")).unwrap();
    let Err(err) = WasmMerge::from_module(&host, &module, "fixture") else {
        panic!("expected ABI version mismatch error");
    };
    let msg = format!("{err:#}");
    assert!(
        msg.contains("ABI version mismatch"),
        "unexpected error: {msg}"
    );
}

#[test]
fn synthesized_fixture_returns_bytes() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("synthesized.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    let result = run(host.engine(), &module, &entries).unwrap();
    match result {
        MergeResult::Synthesized(b) => assert_eq!(b, b"hello"),
        other => panic!("expected Synthesized, got {other:?}"),
    }
}

#[test]
fn guest_error_fixture_run_returns_err_merge_returns_miss() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("guest_error.wat")).unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    let err = run(host.engine(), &module, &entries).unwrap_err();
    assert!(
        format!("{err:#}").contains("error code 7"),
        "expected error code 7, got: {err:#}"
    );

    let merge = WasmMerge::from_module(&host, &module, "fixture").unwrap();
    assert!(matches!(merge.apply(&entries), MergeResult::Miss));
}

#[test]
fn winner_out_of_range_is_rejected() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("winner_out_of_range.wat")).unwrap();
    let owned = sample_entries(); // 3 entries, guest claims index 999
    let entries = views(&owned);
    let err = run(host.engine(), &module, &entries).unwrap_err();
    assert!(
        format!("{err:#}").contains("out-of-range winner"),
        "expected out-of-range error, got: {err:#}"
    );

    let merge = WasmMerge::from_module(&host, &module, "fixture").unwrap();
    assert!(matches!(merge.apply(&entries), MergeResult::Miss));
}

#[test]
fn infinite_loop_hits_deadline_and_degrades_to_miss() {
    use std::time::Duration;

    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("infinite_loop.wat")).unwrap();
    let mut merge = WasmMerge::from_module(&host, &module, "infinite_loop").unwrap();
    // 3 ticks * TICK_INTERVAL_MS budget. Default is
    // ceil(50 / TICK_INTERVAL_MS) ticks; override to keep test
    // fast regardless of tick tuning.
    merge.set_deadline_ticks(3);

    let owned = sample_entries();
    let entries = views(&owned);

    let start = std::time::Instant::now();
    let result = merge.apply(&entries);
    let elapsed = start.elapsed();

    assert!(matches!(result, MergeResult::Miss), "expected Miss from deadline kill");
    assert!(
        elapsed < Duration::from_millis(500),
        "deadline should kill within a few ticks, took {elapsed:?}"
    );
}

#[test]
fn log_fixture_runs_without_error() {
    // The host's mcgw.log implementation writes to stderr; we can't
    // easily capture that here without a custom writer. The important
    // assertion is that the log import resolves, the guest's call
    // returns, and the merge completes. If the import weren't wired
    // up, instantiation would fail with "unknown import".
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("log_then_miss.wat")).unwrap();
    let merge = WasmMerge::from_module(&host, &module, "log_then_miss").unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    assert!(matches!(merge.apply(&entries), MergeResult::Miss));
}

#[test]
fn log_flood_is_dropped_without_failing_merge() {
    // 1000 log calls > LOG_BUDGET_PER_CALL (16). The host must drop
    // the surplus without trapping; the merge still returns Miss.
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("log_flood.wat")).unwrap();
    let merge = WasmMerge::from_module(&host, &module, "log_flood").unwrap();
    let owned = sample_entries();
    let entries = views(&owned);
    assert!(matches!(merge.apply(&entries), MergeResult::Miss));
}

#[test]
fn empty_entries_still_round_trip() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(&load("miss.wat")).unwrap();
    let entries: Vec<Entry<'_>> = vec![];
    let result = run(host.engine(), &module, &entries).unwrap();
    assert!(matches!(result, MergeResult::Miss));
}
