# Agent Note: Dart guest feasibility and the wasm-opt workaround

Status: implemented

## Problem

Dart looked viable as a guest language once the engine gained WasmGC, but a stock `dart compile wasm` module cannot run end-to-end in wasm_runner: dart2wasm emits legacy exception-handling instructions that wasmtime 47 rejects, and every stable-SDK module imports a JavaScript host (`dart2wasm.*` helpers, `wasm:js-string` builtins, string-constant globals, `WebAssembly.JSTag`).

## Decision

Dart feasibility is verified against upstream state and recorded as facts in two reference pages: `docs/dart-feasibility-2026.md` owns the upstream engine/toolchain evidence, and `docs/dart-guest.md` owns the Dart ABI and guest shape.

- The engine-side WasmGC prerequisite is met by wasmtime 47.0.3 (`gc` in default features; `Config::wasm_gc` true by default), proven under wasm_runner's exact Config by `crates/wasm_runner/examples/gc_spike.rs`, which stays in the repo as the GC capability proof.
- Dart is not a shipped guest; it becomes one only when a stock toolchain module runs end-to-end (fixture + e2e + example), per the guest-language fixtures note.
- The standalone dart2wasm target exists in the 3.13 beta SDK (`dart compile wasm --standalone`); standalone modules import only `dart.*` host functions, never JS globals.
- Legacy exception handling remains the engine blocker: every dart2wasm module contains at least one legacy `try`, and `wasm_legacy_exceptions` is a spec-testsuite-only knob `Engine::new` rejects. The workaround is to post-process modules with binaryen `wasm-opt --translate-to-exnref`, which converts Phase-3 EH instructions (`try`/`catch`/`catch_all`/`delegate`/`rethrow`) into the new encoding (`try_table` + `throw_ref`) that wasmtime 47 accepts.
- Upstream gates: dart-lang/sdk#54394 (dart2wasm new-EH codegen, planned for stable ~Nov 2026 / 3.14) and a released SDK with `--standalone` (3.13, ~Sept–Oct 2026).

## Alternatives considered

- **Claim Dart as a valid target on engine GC support alone**: the language matrix records only e2e-proven claims; a module wasmtime rejects at compile time is not a guest.
- **Enable legacy exceptions in wasmtime**: `wasm_legacy_exceptions` is `#[doc(hidden)]`, exists only for the spec testsuite, and `LEGACY_EXCEPTIONS` is not in `features_known_to_wasmtime` — `Engine::new` refuses it; legacy EH is deprecated in the spec.
- **Another engine**: wasmi has no EH support; WAMR tracks EH on the roadmap (bytecodealliance/wasm-micro-runtime#1884); wasmtime is the only Rust engine with the new EH encoding.
- **Wait for upstream before recording anything**: the beta standalone target plus the binaryen translator already open a prototype path, so the record and the host-side requirements are written now rather than deferred.

## Consequences

- `docs/dart-feasibility-2026.md` is the one home for upstream facts (SDK versions, issue and CL state, wasmtime config, the binaryen workaround); `docs/dart-guest.md` is the one home for the Dart ABI, the skeleton guest, and the runner-side requirements.
- The remaining runner-side work is recorded in `docs/dart-guest.md`: a Rust host shim for the `dart.*` imports (or the JS-mode helper modules), an extended `validate_module` allowlist, and determinism wiring through the seeded RNG / virtual clock for `Math.random`/`Date`-style host functions.
- `crates/wasm_runner/examples/gc_spike.rs` remains in the repo as the engine-side GC proof.
