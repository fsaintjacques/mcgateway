;; Merge module returning MergeResult::Winner(999). Host must reject the
;; out-of-range index: run() fails, Merge::apply degrades to Miss.
(module
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

  (func (export "mcgw_merge") (param $ptr i32) (param $count i32) (result i64)
    ;; (999 << 32) | 1
    i64.const 0x000003E700000001))
