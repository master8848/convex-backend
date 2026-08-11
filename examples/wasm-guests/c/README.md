# C guest example

Freestanding C: no libc, no WASI, no headers — just the ABI. This is the
smallest and fastest guest (no runtime init), on par with Rust. The same ABI
serves C++ engines, Zig, AssemblyScript, and Rust `no_std`.

Build (from repo root; requires LLVM clang with the wasm32-wasip1 target —
Apple's system clang does NOT ship it; `brew install llvm` works):

```sh
clang --target=wasm32-wasip1 -O3 -nostdlib -Wl,--no-entry \
    -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined \
    -o wasm_guest_example.wasm guest.c
```

or simply `make c` in examples/wasm-guests.
