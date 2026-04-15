//! End-to-end checks for the UDF loader. Uses a temporary directory
//! and hand-written `.wat` modules so the test does not depend on the
//! SDK build pipeline.

use std::fs;

use mcgateway_core::Registry;
use mcgateway_wasm_host::WasmHost;

// The Registries/udf_loader modules are private to the cdylib crate;
// the integration test exercises their Rust surface (not the Lua
// bindings) by including the sources via `#[path]`. This keeps the
// crate's public API Lua-only without duplicating the loader logic in
// a separate library crate.
#[path = "../src/registries.rs"]
#[allow(dead_code)]
mod registries;
#[path = "../src/udf_loader.rs"]
#[allow(dead_code)]
mod udf_loader;

use registries::Registries;

fn mk_valid_wasm() -> Vec<u8> {
    let wat = r#"(module
      (memory (export "memory") 1)
      (global $bump (mut i32) (i32.const 1024))
      (func (export "mcgw_abi_version") (result i32) i32.const 1)
      (func (export "mcgw_alloc") (param $size i32) (param $align i32) (result i32)
        (local $p i32)
        global.get $bump
        local.set $p
        global.get $bump
        local.get $size
        i32.add
        global.set $bump
        local.get $p)
      (func (export "mcgw_dealloc") (param i32 i32 i32))
      (func (export "mcgw_merge") (param i32 i32) (result i64)
        i64.const 0))"#;
    wat::parse_str(wat).unwrap()
}

fn mk_bad_abi_wasm() -> Vec<u8> {
    let wat = r#"(module
      (memory (export "memory") 1)
      (global $bump (mut i32) (i32.const 1024))
      (func (export "mcgw_abi_version") (result i32) i32.const 999)
      (func (export "mcgw_alloc") (param i32 i32) (result i32) i32.const 0)
      (func (export "mcgw_dealloc") (param i32 i32 i32))
      (func (export "mcgw_merge") (param i32 i32) (result i64) i64.const 0))"#;
    wat::parse_str(wat).unwrap()
}

fn build_registries_with_builtins() -> Registries {
    let mut builtins = Registry::new();
    mcgateway_merge_builtins::register(&mut builtins);
    Registries::new(builtins)
}

#[test]
fn valid_module_is_registered() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("ok.wasm"), mk_valid_wasm()).unwrap();

    let registries = build_registries_with_builtins();
    let host = WasmHost::new().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let table = udf_loader::scan_dir(&host, &registries, tmp.path(), |p, m| {
        problems.push(format!("{}: {m}", p.display()));
    })
    .unwrap();

    assert!(problems.is_empty(), "no problems expected: {problems:?}");
    registries.swap_wasm(table);

    assert!(registries.has("ok"));
    let names = registries.names();
    assert!(names.contains(&"ok".to_string()));
    assert!(names.contains(&"first-hit".to_string()));
}

#[test]
fn builtin_name_collision_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("first-hit.wasm"), mk_valid_wasm()).unwrap();
    fs::write(tmp.path().join("custom.wasm"), mk_valid_wasm()).unwrap();

    let registries = build_registries_with_builtins();
    let host = WasmHost::new().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let table = udf_loader::scan_dir(&host, &registries, tmp.path(), |p, m| {
        problems.push(format!("{}: {m}", p.display()));
    })
    .unwrap();
    registries.swap_wasm(table);

    assert!(
        problems.iter().any(|s| s.contains("first-hit.wasm") && s.contains("built-in")),
        "expected collision log, got: {problems:?}"
    );
    // Custom module still registered:
    assert!(registries.has("custom"));
    // first-hit keeps its built-in implementation; the name resolves to
    // a Merge (the built-in), not to the skipped WASM module.
    assert!(registries.has("first-hit"));
}

#[test]
fn bad_module_is_skipped_and_others_still_load() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("bad.wasm"), mk_bad_abi_wasm()).unwrap();
    fs::write(tmp.path().join("good.wasm"), mk_valid_wasm()).unwrap();

    let registries = build_registries_with_builtins();
    let host = WasmHost::new().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let table = udf_loader::scan_dir(&host, &registries, tmp.path(), |p, m| {
        problems.push(format!("{}: {m}", p.display()));
    })
    .unwrap();
    registries.swap_wasm(table);

    assert!(
        problems.iter().any(|s| s.contains("bad.wasm")),
        "expected bad.wasm to be reported, got: {problems:?}"
    );
    assert!(registries.has("good"));
    assert!(!registries.has("bad"));
}

#[test]
fn non_wasm_files_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("ok.wasm"), mk_valid_wasm()).unwrap();
    fs::write(tmp.path().join("README.md"), b"hello").unwrap();
    fs::write(tmp.path().join(".cache"), b"").unwrap();

    let registries = build_registries_with_builtins();
    let host = WasmHost::new().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let table = udf_loader::scan_dir(&host, &registries, tmp.path(), |p, m| {
        problems.push(format!("{}: {m}", p.display()));
    })
    .unwrap();
    registries.swap_wasm(table);

    assert!(problems.is_empty(), "no problems expected: {problems:?}");
    assert!(registries.has("ok"));
}

#[test]
fn empty_dir_produces_empty_table() {
    let tmp = tempfile::tempdir().unwrap();
    let registries = build_registries_with_builtins();
    let host = WasmHost::new().unwrap();
    let table = udf_loader::scan_dir(&host, &registries, tmp.path(), |_, _| {}).unwrap();
    assert!(table.is_empty());
    registries.swap_wasm(table);
    // Only built-ins remain.
    let names = registries.names();
    assert!(!names.iter().any(|n| n != "first-hit"
        && n != "pool-preferred"
        && n != "last-write-wins"));
}

