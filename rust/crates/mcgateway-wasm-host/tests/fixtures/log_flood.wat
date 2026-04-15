;; Merge module that calls mcgw.log 1000 times. The host is expected
;; to drop everything beyond its per-call budget without failing the
;; merge — the final result is Miss.
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

  (func (export "mcgw_merge") (param $ptr i32) (param $count i32) (result i64)
    (local $i i32)
    ;; Seed 4 bytes of message at offset 64: "spam"
    i32.const 64
    i32.const 0x6D617073
    i32.store
    (loop $more
      ;; mcgw.log(level=0, ptr=64, len=4)
      i32.const 0
      i32.const 64
      i32.const 4
      call $log
      local.get $i
      i32.const 1
      i32.add
      local.tee $i
      i32.const 1000
      i32.lt_s
      br_if $more)
    i64.const 0))
