//! End-to-end round-trip test: build the `merge-last-n-wins` example
//! through `cargo` for `wasm32-wasip1`, load the produced `.wasm` via
//! the host, and exercise the three `MergeResult` variants against
//! hand-built entry slices.
//!
//! Shells out to cargo; gated behind the `sdk-tests` feature so plain
//! `cargo test -p mcgateway-wasm-host` on laptops without the wasi
//! toolchain stays fast.

#![cfg(feature = "sdk-tests")]

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use mcgateway_core::{Entry, MergeResult, Status};
use mcgateway_wasm_host::{run, WasmHost, WasmMerge};

/// Build an example once per test process. Serializing here avoids
/// parallel `cargo build` invocations racing over the same target dir.
fn build_example(crate_name: &str, artifact: &str) -> Vec<u8> {
    let manifest = workspace_root().join(format!("examples/{crate_name}/Cargo.toml"));
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-wasip1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .status()
        .expect("invoke cargo");
    assert!(status.success(), "cargo build failed for {crate_name}");

    let wasm = workspace_root().join(format!("target/wasm32-wasip1/release/{artifact}.wasm"));
    std::fs::read(&wasm).unwrap_or_else(|e| panic!("read {}: {e}", wasm.display()))
}

fn example_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| build_example("merge-last-n-wins", "merge_last_n_wins"))
        .as_slice()
}

fn concat_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| build_example("merge-concat-values", "merge_concat_values"))
        .as_slice()
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at the wasm-host crate dir; the workspace
    // root is two levels up (crates/mcgateway-wasm-host → rust/).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("canonicalize workspace root")
}

fn owned(pool: &str, status: Status, t: Option<i64>) -> (Vec<u8>, String, Status, Option<i64>) {
    (b"user:42".to_vec(), pool.to_string(), status, t)
}

fn view(o: &(Vec<u8>, String, Status, Option<i64>)) -> Entry<'_> {
    Entry {
        key: &o.0,
        pool: &o.1,
        status: o.2,
        t: o.3,
        value: None,
        line: None,
    }
}

#[test]
fn picks_highest_t_hit() {
    let wasm = example_wasm();
    let host = WasmHost::new().unwrap();
    let module = host.compile(wasm).unwrap();

    let entries = [
        owned("a", Status::Hit, Some(100)),
        owned("b", Status::Hit, Some(500)),
        owned("c", Status::Hit, Some(250)),
    ];
    let views: Vec<_> = entries.iter().map(view).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    assert!(matches!(result, MergeResult::Winner(1)));
}

#[test]
fn all_miss_returns_miss() {
    let wasm = example_wasm();
    let host = WasmHost::new().unwrap();
    let module = host.compile(wasm).unwrap();

    let entries = [
        owned("a", Status::Miss, None),
        owned("b", Status::Miss, None),
    ];
    let views: Vec<_> = entries.iter().map(view).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    assert!(matches!(result, MergeResult::Miss));
}

#[test]
fn hits_without_t_are_ignored() {
    let wasm = example_wasm();
    let host = WasmHost::new().unwrap();
    let module = host.compile(wasm).unwrap();

    let entries = [
        owned("a", Status::Hit, None),
        owned("b", Status::Hit, Some(42)),
        owned("c", Status::Error, None),
    ];
    let views: Vec<_> = entries.iter().map(view).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    assert!(matches!(result, MergeResult::Winner(1)));
}

struct Hit {
    pool: String,
    value: Vec<u8>,
}

fn hit(pool: &str, value: &[u8]) -> Hit {
    Hit {
        pool: pool.to_string(),
        value: value.to_vec(),
    }
}

fn hit_view<'a>(key: &'a [u8], h: &'a Hit) -> Entry<'a> {
    Entry {
        key,
        pool: &h.pool,
        status: Status::Hit,
        t: None,
        value: Some(&h.value),
        line: None,
    }
}

#[test]
fn synthesized_round_trips_through_sdk_encoder() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(concat_wasm()).unwrap();

    let key = b"user:42".to_vec();
    let hits = [hit("a", b"alpha"), hit("b", b"beta"), hit("c", b"gamma")];
    let views: Vec<_> = hits.iter().map(|h| hit_view(&key, h)).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    match result {
        MergeResult::Synthesized(b) => assert_eq!(b, b"alpha|beta|gamma"),
        other => panic!("expected Synthesized, got {other:?}"),
    }
}

#[test]
fn synthesized_empty_round_trips() {
    // Single hit with empty value body. Exercises the payload=empty
    // branch in the SDK encoder (alloc(1, 1) sentinel).
    let host = WasmHost::new().unwrap();
    let module = host.compile(concat_wasm()).unwrap();

    let key = b"k".to_vec();
    let hits = [hit("a", b"")];
    let views: Vec<_> = hits.iter().map(|h| hit_view(&key, h)).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    match result {
        MergeResult::Synthesized(b) => assert!(b.is_empty(), "got {b:?}"),
        other => panic!("expected Synthesized, got {other:?}"),
    }
}

#[test]
fn synthesized_large_payload_round_trips() {
    // 128 KiB payload exercises multi-page linear-memory growth on the
    // guest side and the host's byte-for-byte copy-out.
    let host = WasmHost::new().unwrap();
    let module = host.compile(concat_wasm()).unwrap();

    let key = b"k".to_vec();
    let big = vec![b'x'; 128 * 1024];
    let hits = [hit("a", &big)];
    let views: Vec<_> = hits.iter().map(|h| hit_view(&key, h)).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    match result {
        MergeResult::Synthesized(b) => assert_eq!(b, big),
        other => panic!("expected Synthesized, got {other:?}"),
    }
}

#[test]
fn concat_all_miss_returns_miss() {
    let host = WasmHost::new().unwrap();
    let module = host.compile(concat_wasm()).unwrap();

    let entries = [
        owned("a", Status::Miss, None),
        owned("b", Status::Miss, None),
    ];
    let views: Vec<_> = entries.iter().map(view).collect();
    let result = run(host.engine(), &module, &views).unwrap();
    assert!(matches!(result, MergeResult::Miss));
}

#[test]
fn required_flags_is_wired_through() {
    let wasm = example_wasm();
    let host = WasmHost::new().unwrap();
    let module = host.compile(wasm).unwrap();
    let merge = WasmMerge::from_module(&host, module).unwrap();
    assert_eq!(merge.required_flags(), "t");
}
