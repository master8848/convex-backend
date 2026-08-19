# Zig guests for Convex WASM functions

Zig is the smallest possible guest language for the Convex WASM ABI: a freestanding reactor module with zero WASI imports (only the four `env` host functions it uses), 394 bytes for the plain echo guest, 837 bytes for the args-parsing version used by the tests. No runtime init, no GC, no libc. The language status matrix is [non-js-languages.md](non-js-languages.md); the ABI, host functions, and runtime limits are documented in [wasm.md](wasm.md).

## Module shape

```
imports: env.__convex_input_length, env.__convex_input_load,
         env.__convex_output_set, env.__convex_error_set
exports: memory, __convex_functions, _initialize, __convex_run
```

- Reactor model (`-mexec-model=reactor`): no `_start`; the module self-initializes via the Wasm start section and exports `_initialize`.
- Zig 0.16 does not auto-export `export fn` symbols for wasm targets via `build-exe` — pass `--export=__convex_run --export=__convex_functions` explicitly.

## Build

```sh
zig build-exe guest.zig -target wasm32-wasip1 -mexec-model=reactor \
  -O ReleaseSmall -fstrip \
  --export=__convex_run --export=__convex_functions --name guest
```

Per-lang tuning: `wasm32-wasip1` (not legacy `wasm32-wasi`) keeps the engine's `wasmtime 47` `wasm32-wasip1` contract (`crates/wasm_runner/src/engine.rs:105`); `ReleaseSmall` + `fstrip` + `--export=` are the size minima (394 B echo / 837 B with arg parsing). `ReleaseSmall` vs `ReleaseSafe` saves ~200 B; `fstrip` saves ~1 KiB of debug names; no `ReleaseFast` — it adds unrolling. Keep imports to 4 `env` functions for the smallest module; adding `std.debug.print` would pull `wasi_snapshot_preview1.fd_write` and 2-4 KiB.

Requires Zig 0.16+ (https://ziglang.org/download/). The e2e test (`crates/wasm_runner/tests/zig_guest_e2e.rs`) probes `zig version` and skips cleanly when the toolchain is absent, like the other guest tests.

## Files

- `crates/wasm_runner/tests/fixtures/zig_guest/` — tested fixture (guest.zig, README, .gitignore; guest.wasm is gitignored, built at test time)
- `crates/wasm_runner/tests/zig_guest_e2e.rs` — full e2e: echo against a real sqlite-backed Transaction, unknown-function guest error, `analyze_functions` descriptor analysis
- `examples/wasm-guests/zig/` — example + `make zig` target

## Notes

- The host payload is `{"function": "echo", "args": ["hello zig"]}` (spaces after colons); the fixture's arg extractor searches for `"args": [` like the C guest.
- `std.mem` is available without WASI (compiled in); `std.debug.print` and allocators that need `fd_write`/`environ` would add WASI imports — keep guests to the four `env` functions for the smallest module.
