# Go guest example

Hand-written `//go:wasmimport` / `//go:wasmexport` guest, no cgo. main.go shows
the full ABI surface: reading the input payload, calling db host functions
(get/insert/count/query), deterministic now/random, logs, and errors.

Build (from repo root; requires Go >= 1.24):

```sh
cd examples/wasm-guests/go
GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -o wasm_guest_example.wasm .
```

or simply `make go` in examples/wasm-guests.
