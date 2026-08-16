# Dart guest — in progress

Status: 🚧 **in progress**. `dart compile wasm` emits WasmGC modules. The runner
(crates/wasm_runner) uses wasmtime 47, and the GC spike
(crates/wasm_runner/examples/gc_spike.rs) has proven wasmtime 47 compiles and
runs WasmGC modules (struct/array/i31/ref) with the runner's exact Config — see
docs/wasm.md and docs/dart-guest.md; the worklog record lives in
.agents/notes/implemented/feature/2026-08-11-dart-guest-feasibility-workaround.md.

What lands here once validated end-to-end:

- `guest.dart` implementing the ABI (imports `env` host functions, exports
  `__convex_run` / `__convex_functions`) via `dart:js_interop`-style extern
  bindings compiled with `dart compile wasm`
- build command + `make dart` wiring
- an end-to-end test in crates/wasm_runner/tests/end_to_end.rs

Until then, the fastest path for Dart developers is the **C++ template**: Dart
FFI can drive a small C++ engine guest compiled with the cpp example's build
line.
