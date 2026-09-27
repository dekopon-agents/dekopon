(component
  (import "dekopon:clock/monotonic@1.1.0" (instance $clock
    (export "now-nanos" (func (result u64)))))
  (import "dekopon:random/source@0.1.0" (instance $random
    (export "get-random-bytes" (func (param "length" u32) (result (list u8))))))
  (core module $heap-module
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 4096))
  (core instance $heap (instantiate $heap-module))
  (core func $clock-lowered (canon lower (func $clock "now-nanos")))
  (core func $random-lowered (canon lower (func $random "get-random-bytes")
    (memory (core memory $heap "memory")) (realloc (core func $heap "realloc"))))
  (core module $m
    (import "host" "clock" (func $clock (result i64)))
    (import "host" "random" (func $random (param i32 i32)))
    (import "heap" "memory" (memory 1))
    (func (export "read-clock") (result i64) call $clock)
    (func (export "read-random") (param i32) (result i32)
      local.get 0
      i32.const 16
      call $random
      i32.const 20
      i32.load))
  (core instance $i (instantiate $m
    (with "host" (instance (export "clock" (func $clock-lowered)) (export "random" (func $random-lowered))))
    (with "heap" (instance (export "memory" (memory $heap "memory"))))))
  (func (export "read-clock") (result u64) (canon lift (core func $i "read-clock")))
  (func (export "read-random") (param "length" u32) (result u32)
    (canon lift (core func $i "read-random"))))
