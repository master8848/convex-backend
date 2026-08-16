# Agent Note: Kotlin guest toolchain-gated e2e

Status: implemented

## Problem

Kotlin/Wasm offers an annotation-based export ABI, but nothing proved it against the wasm_runner: no fixture, no end-to-end test, and no evidence that the `wasmWasi` module shape (WasmGC + new exception handling + a start-section reactor) runs under wasmtime 47's defaults. The repository ships a guest language only with a committed fixture, an e2e test, and an example, and the CI environment has no Kotlin toolchain (no JDK, no Gradle) to run one.

## Decision

Kotlin Multiplatform's `wasmWasi` target (wasm32-wasip1 + WasmGC) is the Kotlin guest target. Its module imports only `wasi_snapshot_preview1` plus the declared `@WasmImport("env", ...)` functions, matching `validate_module`'s allowlist, and needs no JS host. The fixture lives at `crates/wasm_runner/tests/fixtures/kotlin_guest/` and the e2e test at `crates/wasm_runner/tests/kotlin_guest_e2e.rs`; the test builds the fixture with `gradle build` and returns early (skips) when `gradle` is missing or cannot run, so verification happens wherever a JDK + Gradle toolchain exists. The module is a reactor: no `main`, a Wasm start section runs the runtime initializers at instantiation, and there is no `_initialize` to call; the runner calls `_initialize` when present and otherwise relies on the start section having run during `instantiate_async`. wasmtime 47's default config (`wasm_function_references`, `wasm_gc`, `wasm_exceptions` all true) covers the module shape with no runner changes. All Kotlin facts have their one home in [docs/kotlin-guest.md](../../../../docs/kotlin-guest.md), with the verdict row in the [language status matrix](../../../../docs/non-js-languages.md).

## Alternatives considered

- **`wasmJs` (WasmGC + JS interop)**: exposes the same `@WasmExport` / `@WasmImport` annotations since Kotlin 1.8, but its runtime imports JS functions (e.g. `console.*`) under module `env` and is driven by generated JS glue; it needs a JS host and is unsupported in wasmtime. Rejected.
- **Engine-side proof only (a hand-written WasmGC WAT module)**: proves the runner accepts the module shape but not that a real Kotlin build produces it; used as an interim verification step, not as the guest's verification.
- **Fixture without a runner e2e test**: would record a claim the test suite cannot exercise; the committed fixture plus a toolchain-gated test reproduces on any machine with the toolchain and skips cleanly elsewhere.

## Consequences

- The e2e test is toolchain-gated: it runs on machines with JDK + Gradle and skips in CI; the matrix row records the gate so the absence from CI is a stated fact, not an omission.
- The runner's `Config` gains nothing for Kotlin: WasmGC, function references, and new exception handling are wasmtime 47 defaults.
- The reactor contract is fixed: Kotlin guests self-initialize via the Wasm start section; the runner accepts both `_initialize` and start-section modules.
- Guest stdout is unavailable (the WASI context has no stdio); `__convex_log` is the output path.
