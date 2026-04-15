;; Merge module that calls mcgw.log once with a known message and
;; returns Miss. Exercises the host-imported log function.
;;
;; Strategy: stash the 9-byte ASCII message "hi-from-wasm" at offset
;; 64 using i64.store/i32.store8, then call mcgw.log(level=3, ptr=64,
;; len=12).
(module
  (import "mcgw" "log" (func $log (param i32 i32 i32)))
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

  ;; "hi-from-wasm" = 68 69 2D 66 72 6F 6D 2D 77 61 73 6D (12 bytes).
  ;; Write the first 8 bytes as an i64, then 4 more bytes as an i32.
  (func (export "mcgw_merge") (param $ptr i32) (param $count i32) (result i64)
    i32.const 64
    i64.const 0x2D6D6F72662D6968  ;; "hi-from-" reversed LE
    i64.store
    i32.const 72
    i32.const 0x6D736177          ;; "wasm" reversed LE
    i32.store
    ;; mcgw.log(level=3 WARN, ptr=64, len=12)
    i32.const 3
    i32.const 64
    i32.const 12
    call $log
    i64.const 0))
