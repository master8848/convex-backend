# C++ guest example

Freestanding C++ on the same ABI as the C example: namespaces, classes (POD),
constexpr, templates — but no libc++, no exceptions, no RTTI. Ideal for game
engines and C++ codebases that want to ship backend logic as a wasm module.

Rules that keep a C++ guest compilable with `-nostdlib`:

- `-fno-exceptions -fno-rtti -fno-threadsafe-statics`
- no headers at all — not even `<cstdint>`: stock LLVM clang++ has no libc++
  include dir for wasm32-wasip1 without a wasi-sdk sysroot, so use the
  built-in integer types (`int` = 32-bit, `long long` = 64-bit on wasm32)
- no heap allocation from the guest (host allocates); `static` globals must be
  const-initialized POD (guard variables need `__cxa_guard`, which `-nostdlib`
  does not provide)
- strings cross the boundary as `(ptr, len)` byte slices; no `std::string`

Build (from repo root; requires LLVM clang++ with wasm32-wasip1):

```sh
clang++ --target=wasm32-wasip1 -O3 -nostdlib -fno-exceptions -fno-rtti \
    -fno-threadsafe-statics -Wl,--no-entry \
    -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined \
    -o wasm_guest_example.wasm guest.cpp
```

or simply `make cpp` in examples/wasm-guests.
