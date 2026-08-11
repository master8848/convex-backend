# Zig guest example

A minimal Convex WASM function in Zig: 837-byte reactor module, imports only
the four `env` host functions, exports `__convex_run`, `__convex_functions`,
`_initialize`. No std usage beyond `std.mem.eql`, no WASI imports.

## Build

Requires Zig 0.16+ (https://ziglang.org/download/). Zig 0.16 does not
auto-export `export fn` symbols for wasm targets via `build-exe`, so the two
ABI exports are passed explicitly:

```sh
zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor \
  -O ReleaseSmall -fstrip \
  --export=__convex_run --export=__convex_functions --name guest
```

Or `make zig` from `examples/wasm-guests/`. See
`crates/wasm_runner/tests/fixtures/zig_guest/` for the tested fixture and
`docs/zig-guest.md` for the guide.
