# Zig guest fixture

A minimal Convex WASM guest written in Zig (freestanding ABI, no std, no WASI
imports). Used by `crates/wasm_runner/tests/zig_guest_e2e.rs`.

## ABI contract (crates/wasm_runner/src/abi.rs)

- exports: `__convex_run() -> i32`, `__convex_functions() -> i32`, `_initialize()`
- imports (module `env`): `__convex_input_length`, `__convex_input_load`,
  `__convex_output_set`, `__convex_error_set`
- WASI reactor model (`-mexec-model=reactor`): no `_start`; the module
  self-initializes via the Wasm start section and `_initialize` is exported.

## Build

Requires Zig 0.16+ (`zig version`). Zig 0.16 does NOT auto-export `export fn`
symbols for wasm targets via `build-exe`, so the two ABI exports are passed
explicitly:

```sh
zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor \
  -O ReleaseSmall -fstrip \
  --export=__convex_run --export=__convex_functions --name guest
# -> guest.wasm (394 bytes)
```

Inspect: `wasm-tools print guest.wasm` (imports = `env` only; exports =
`memory`, `__convex_functions`, `_initialize`, `__convex_run`).

The result is a 394-byte module that imports only the four `env` host
functions — the same module shape as the C guest, no WASI dependency at all.
