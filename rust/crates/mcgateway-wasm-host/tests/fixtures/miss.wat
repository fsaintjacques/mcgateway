;; Minimal merge module returning MergeResult::Miss unconditionally.
;; Exercises the load path, ABI handshake, entry marshaling, and decode
;; of the tag=0 result.
(module
  (memory (export "memory") 1)
  (global $bump (mut i32) (i32.const 1024))

  (func (export "mcgw_abi_version") (result i32)
    i32.const 1)

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
    i64.const 0))
