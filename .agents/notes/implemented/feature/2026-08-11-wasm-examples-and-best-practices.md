# Agent Note: WASM guest examples and best practices

Status: implemented

## Problem

Guest examples, best practices, and the C/C++ e2e coverage existed only as scattered worklog notes and uncommitted artifacts: there was no one-command build, no scaffolding path for new guests, and no committed C/C++ fixture in the end-to-end suite. The C++ guest as first drafted did not build with stock LLVM clang++: `#include <cstdint>` fails because stock clang++ for wasm32-wasip1 has no libc++ include dir without a wasi-sdk sysroot.

## Decision

The guest-examples work ships with code in `examples/wasm-guests/`, authoring rules in [docs/wasm-best-practices.md](../../../../docs/wasm-best-practices.md) (the one home for examples and best-practices facts), and verification in the wasm_runner e2e suite:

- Each supported language (`rust/`, `go/`, `c/`, `cpp/`, `zig/`) has a ready-to-build standalone guest in `examples/wasm-guests/`; `dart/` and `kotlin/` hold status stubs. The Rust and Go guests read/write the database through the host functions.
- A Makefile provides `make` (build all examples into `build/`), `make check` (verify toolchains, explain how to install what is missing), and per-language targets.
- `scaffold.sh <lang> <name>` copies a pre-wired template (ABI imports/exports included) and replaces the package name (Rust Cargo.toml package, Go module name); invalid names are rejected.
- C and C++ guests are fully freestanding: no libc, no libc++, no headers (not even `<cstdint>`), no guard vars, POD statics only — built-in integer types, so the documented build line (`clang --target=wasm32-wasip1 -nostdlib`, `clang++ --target=wasm32-wasip1 -nostdlib -fno-exceptions -fno-rtti`) works with stock LLVM without a wasi-sdk sysroot.
- `crates/wasm_runner/tests/fixtures/cpp_guest/` holds the committed C++ fixture, kept byte-identical to `examples/wasm-guests/cpp/guest.cpp`; [crates/wasm_runner/tests/end_to_end.rs](../../../../crates/wasm_runner/tests/end_to_end.rs) runs `test_c_guest_end_to_end` and `test_cpp_guest_end_to_end` with real compiled modules against a real sqlite-backed `Database`.
- `crates/wasm_runner/examples/gc_spike.rs` proves the engine's WasmGC support (struct/array/i31/ref.eq, `wasm_gc(false)` rejection) under the runner's exact Config.
- `examples/wasm-guests/.gitignore` ignores `build/` and `*.wasm` so `git add` of the tree stays clean.
- The Rust example declares `pub struct User` (silences the `private_interfaces` warning); the examples README Dart row states the upstream-blocked status.

## Alternatives considered

- **C++ guest with libc++ headers via a wasi-sdk sysroot**: the sysroot dependency would break the documented build line on stock toolchains; the freestanding no-headers rule keeps the example and fixture buildable with stock LLVM clang++.
- **Require every guest toolchain in CI**: the C/C++ e2e tests would depend on a Homebrew or wasi-sdk LLVM image CI lacks; the tests skip gracefully when the host toolchain has no wasm32-wasip1 target (e.g. Apple clang), keeping the suite green without the image.
- **Compile-and-run helpers instead of committed fixtures**: covered in the [guest language fixtures note](../feature/2026-08-11-guest-language-fixtures-and-examples.md).

## Consequences

- `docs/wasm-best-practices.md` is the one home for guest authoring rules and the deployment checklist; `examples/wasm-guests/` is the one home for runnable guest code; per-language status stays in `docs/non-js-languages.md`.
- The C++ no-headers rule is documented in `examples/wasm-guests/cpp/README.md` and `docs/wasm-best-practices.md`.
- The e2e suite covers rust, go, c, and cpp guests (zig and kotlin have their own e2e tests); toolchain-missing guests skip gracefully.
- Running the C/C++ e2e tests in CI requires an image with Homebrew or wasi-sdk LLVM; on hosts without a wasm32-wasip1 target the suite reports skips.
