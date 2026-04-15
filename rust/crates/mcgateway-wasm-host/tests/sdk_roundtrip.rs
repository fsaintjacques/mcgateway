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

/// Build the example once per test process. Serializing here avoids
/// four parallel `cargo build` invocations racing over the same target
/// dir.
fn example_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let manifest = workspace_root().join("examples/merge-last-n-wins/Cargo.toml");
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
        assert!(status.success(), "cargo build failed");

        let wasm = workspace_root().join("target/wasm32-wasip1/release/merge_last_n_wins.wasm");
        std::fs::read(&wasm).unwrap_or_else(|e| panic!("read {}: {e}", wasm.display()))
    })
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

#[test]
fn required_flags_is_wired_through() {
    let wasm = example_wasm();
    let host = WasmHost::new().unwrap();
    let module = host.compile(wasm).unwrap();
    let merge = WasmMerge::from_module(&host, module).unwrap();
    assert_eq!(merge.required_flags(), "t");
}
