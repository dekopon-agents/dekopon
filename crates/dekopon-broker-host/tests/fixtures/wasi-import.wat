;; Deliberately unsupported generic WASI import. Regenerate the adjacent binary with:
;; wasm-tools parse wasi-import.wat -o wasi-import.wasm
(component
  (import "wasi:io/poll@0.2.0" (instance $poll
    (export "poll" (func (param "in" (list u32)) (result (list u32))))
  ))
)
