;; Merge module returning MergeResult::Synthesized(b"hello"). Exercises the
;; descriptor-indirection decode path: allocate the 5-byte payload, allocate
;; an 8-byte descriptor holding (ptr, len), return tag=2 with the
;; descriptor pointer in the high 32 bits.
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
    (local $payload i32)
    (local $desc i32)

    ;; Allocate 5 bytes for the payload and write "hello".
    i32.const 5
    i32.const 1
    call 1  ;; mcgw_alloc
    local.tee $payload
    i32.const 0x6c6c6568  ;; 'h','e','l','l' little-endian
    i32.store
    local.get $payload
    i32.const 4
    i32.add
    i32.const 0x6f  ;; 'o'
    i32.store8

    ;; Allocate 8 bytes for the {ptr, len} descriptor.
    i32.const 8
    i32.const 4
    call 1  ;; mcgw_alloc
    local.set $desc

    ;; desc[0..4] = payload ptr
    local.get $desc
    local.get $payload
    i32.store
    ;; desc[4..8] = payload len
    local.get $desc
    i32.const 4
    i32.add
    i32.const 5
    i32.store

    ;; result = (desc << 32) | 2
    local.get $desc
    i64.extend_i32_u
    i64.const 32
    i64.shl
    i64.const 2
    i64.or))
