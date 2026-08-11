# Zig guests for Convex WASM functions

Status: **valid target** — fixture + end-to-end test landed and passing.

Zig is the smallest possible guest language: a freestanding reactor module
with **zero WASI imports** (only the four `env` host functions it uses), 394
bytes for the plain echo guest, 837 bytes for the args-parsing version used by
the tests. No runtime init, no GC, no libc.

## Module shape (verified against wasm_runner + wasmtime 47)

```
imports: env.__convex_input_length, env.__convex_input_load,
         env.__convex_output_set, env.__convex_error_set
exports: memory, __convex_functions, _initialize, __convex_run
```

- Reactor model (`-mexec-model=reactor`): no `_start`; the module
  self-initializes via the Wasm start section and exports `_initialize`.
- Zig 0.16 does **not** auto-export `export fn` symbols for wasm targets via
  `build-exe` — pass `--export=__convex_run --export=__convex_functions`
  explicitly.

## Build

```sh
zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor \
  -O ReleaseSmall -fstrip \
  --export=__convex_run --export=__convex_functions --name guest
```

Requires Zig 0.16+ (https://ziglang.org/download/). The e2e test
(`crates/wasm_runner/tests/zig_guest_e2e.rs`) probes `zig version` and skips
cleanly when the toolchain is absent, like the other guest tests.

## Files

- `crates/wasm_runner/tests/fixtures/zig_guest/` — tested fixture (guest.zig,
  README, .gitignore; guest.wasm is gitignored, built at test time)
- `crates/wasm_runner/tests/zig_guest_e2e.rs` — full e2e: echo against a real
  sqlite-backed Transaction, unknown-function guest error, `analyze_functions`
  descriptor analysis
- `examples/wasm-guests/zig/` — example + `make zig` target

## Notes

- The host payload is `{"function": "echo", "args": ["hello zig"]}` (spaces
  after colons); the fixture's arg extractor searches for `"args": [` like the
  C guest.
- `std.mem` is available without WASI (compiled in); `std.debug.print` and
  allocators that need `fd_write`/`environ` would add WASI imports — keep
  guests to the four `env` functions for the smallest module.
