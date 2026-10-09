;; word-count: an example NoteSec plugin (decision 55, docs/PLUGINS.md),
;; hand-written in WebAssembly text. NoteSec's input always starts with
;; `block = "<text>"`; this counts the words of that string.
;;   Command "count": sets the status to "Words: N".
;;   Render hook {{word-count}}: shows "N words" under the block.
;; Built into plugin.wasm by `cargo test` (plugins::tests checks they match).
(module
  (import "env" "host_log" (func $log (param i32 i32)))
  (memory (export "memory") 1)
  ;; Output templates, at fixed places below the heap.
  (data (i32.const 512) "[[actions]]\ntype = \"set_status\"\ntext = \"Words: ")
  (data (i32.const 768) "text = \"")
  (data (i32.const 896) " words")
  (data (i32.const 960) "word-count: counting")
  (global $heap (mut i32) (i32.const 1024))

  ;; A bump allocator: never frees; each call gets a fresh instance anyway.
  (func (export "alloc") (param $len i32) (result i32)
    (local $p i32) (local $need i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (local.get $p) (local.get $len)))
    (local.set $need
      (i32.sub (global.get $heap) (i32.mul (memory.size) (i32.const 65536))))
    (if (i32.gt_s (local.get $need) (i32.const 0))
      (then
        (if (i32.eq (memory.grow (i32.add (i32.div_u (local.get $need) (i32.const 65536)) (i32.const 1)))
                    (i32.const -1))
          (then unreachable))))
    (local.get $p))

  (func (export "dealloc") (param i32 i32))

  ;; Words in the string after `block = "` (9 bytes): runs of characters
  ;; that aren't spaces, control characters or the escapes \n \t \r.
  (func $count (param $p i32) (param $len i32) (result i32)
    (local $i i32) (local $end i32) (local $c i32) (local $sep i32) (local $in i32) (local $n i32)
    (local.set $i (i32.add (local.get $p) (i32.const 9)))
    (local.set $end (i32.add (local.get $p) (local.get $len)))
    (block $done
      (loop $next
        (br_if $done (i32.ge_u (local.get $i) (local.get $end)))
        (local.set $c (i32.load8_u (local.get $i)))
        (br_if $done (i32.eq (local.get $c) (i32.const 34)))
        (local.set $sep (i32.le_u (local.get $c) (i32.const 32)))
        (if (i32.eq (local.get $c) (i32.const 92))
          (then
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (local.set $c (i32.load8_u (local.get $i)))
            (local.set $sep
              (i32.or (i32.or (i32.eq (local.get $c) (i32.const 110))
                              (i32.eq (local.get $c) (i32.const 116)))
                      (i32.eq (local.get $c) (i32.const 114))))))
        (if (local.get $sep)
          (then (local.set $in (i32.const 0)))
          (else
            (if (i32.eqz (local.get $in))
              (then
                (local.set $n (i32.add (local.get $n) (i32.const 1)))
                (local.set $in (i32.const 1))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $next)))
    (local.get $n))

  ;; Write $n in decimal at $at; returns the end.
  (func $digits (param $n i32) (param $at i32) (result i32)
    (local $t i32) (local $len i32) (local $i i32)
    (local.set $t (local.get $n))
    (local.set $len (i32.const 1))
    (block $b
      (loop $l
        (br_if $b (i32.lt_u (local.get $t) (i32.const 10)))
        (local.set $t (i32.div_u (local.get $t) (i32.const 10)))
        (local.set $len (i32.add (local.get $len) (i32.const 1)))
        (br $l)))
    (local.set $i (i32.add (local.get $at) (local.get $len)))
    (loop $w
      (local.set $i (i32.sub (local.get $i) (i32.const 1)))
      (i32.store8 (local.get $i) (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
      (local.set $n (i32.div_u (local.get $n) (i32.const 10)))
      (br_if $w (i32.gt_u (local.get $i) (local.get $at))))
    (i32.add (local.get $at) (local.get $len)))

  ;; ptr << 32 | len
  (func $pack (param $ptr i32) (param $end i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
            (i64.extend_i32_u (i32.sub (local.get $end) (local.get $ptr)))))

  (func (export "run_command") (param $p i32) (param $len i32) (result i64)
    (local $end i32)
    (call $log (i32.const 960) (i32.const 20))
    (local.set $end (call $digits (call $count (local.get $p) (local.get $len))
                                  (i32.const 559)))
    (i32.store8 (local.get $end) (i32.const 34))
    (i32.store8 (i32.add (local.get $end) (i32.const 1)) (i32.const 10))
    (call $pack (i32.const 512) (i32.add (local.get $end) (i32.const 2))))

  (func (export "render") (param $p i32) (param $len i32) (result i64)
    (local $end i32)
    (local.set $end (call $digits (call $count (local.get $p) (local.get $len))
                                  (i32.const 776)))
    ;; " words" then the closing quote and a newline.
    (i64.store (local.get $end) (i64.load (i32.const 896)))
    (local.set $end (i32.add (local.get $end) (i32.const 6)))
    (i32.store8 (local.get $end) (i32.const 34))
    (i32.store8 (i32.add (local.get $end) (i32.const 1)) (i32.const 10))
    (call $pack (i32.const 768) (i32.add (local.get $end) (i32.const 2))))
)
