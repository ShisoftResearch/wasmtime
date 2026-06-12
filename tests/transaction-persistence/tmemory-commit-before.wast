(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )

  (tfunc (export "write") (param $value i32)
    (i32.tstore (i32.const 64) (local.get $value))
  )
)

(assert_return (tinvoke "read") (i32.const 0))
(assert_return (tinvoke "write" (i32.const 0x11223344)))
(assert_return (tinvoke "read") (i32.const 0x11223344))
