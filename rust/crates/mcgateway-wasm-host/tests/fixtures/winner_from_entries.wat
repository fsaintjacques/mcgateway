;; Merge module that returns MergeResult::Winner(count - 1). Proves the
;; host actually delivered the entry count to the guest. The `count`
;; parameter is the second argument to mcgw_merge.
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
    ;; result = ((count - 1) << 32) | 1
    local.get $count
    i32.const 1
    i32.sub
    i64.extend_i32_u
    i64.const 32
    i64.shl
    i64.const 1
    i64.or))
