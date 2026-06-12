(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )
)

(assert_return (tinvoke "read") (i32.const 0x11223344))
