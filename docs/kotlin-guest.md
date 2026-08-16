# Kotlin guest: Kotlin Multiplatform `wasmWasi` (WasmGC + WASI Preview 1)

The Kotlin guest is a committed fixture plus a toolchain-gated end-to-end test in `crates/wasm_runner`. Its verdict row lives in the [non-JS languages status matrix](non-js-languages.md), and the target-selection decision in [Agent Note 2026-08-11-kotlin-guest-toolchain-gated-e2e](../.agents/notes/implemented/feature/2026-08-11-kotlin-guest-toolchain-gated-e2e.md).

## Status in the language matrix

The Kotlin row of the [language status matrix](non-js-languages.md) is the one home for the verdict and build gating: fixture + e2e test, toolchain-gated, JDK + Gradle required, untested in CI. The fixtures-and-examples policy behind every guest is recorded in [Agent Note 2026-08-11-guest-language-fixtures-and-examples](../.agents/notes/implemented/feature/2026-08-11-guest-language-fixtures-and-examples.md).

## Target selection: `wasmWasi`, not `wasmJs`

Both Kotlin/Wasm targets expose the export/import annotations this ABI needs — `@WasmExport` / `@WasmImport` (both since Kotlin 1.8, on both targets, "without type adapters": raw `Int` → `i32`). The two targets differ in host fit:

| | `wasmWasi` (chosen) | `wasmJs` (WasmGC + JS interop) |
|---|---|---|
| Module | wasm32-wasip1 + WasmGC | WasmGC, designed for JS hosts |
| Imports | `wasi_snapshot_preview1` + declared `@WasmImport`s only | JS functions (e.g. `console.*`) under module `env`, driven by generated JS glue |
| Runner fit | WASI p1 already registered; `env` host funcs already registered | needs a JS host; unsupported in wasmtime |
| Exports | `@WasmExport("__convex_run") fun convexRun(): Int` | same annotation, but the module is glued to a JS runtime |

`wasmJs`'s JS-host dependency makes `wasmWasi` the only target with a real export-ABI path for the runner. The decision is recorded in [Agent Note 2026-08-11-kotlin-guest-toolchain-gated-e2e](../.agents/notes/implemented/feature/2026-08-11-kotlin-guest-toolchain-gated-e2e.md).

Kotlin/Wasm emits WasmGC since 1.9.20; `wasmWasi` uses the new exception-handling proposal (`exnref`) by default. wasmtime 47 enables all three proposals by default (`wasm_function_references` = true, `wasm_gc` = true, `wasm_exceptions` = true in the vendored crate's `src/config.rs`), so the runner's `Config` needs no changes.

## Module shape

A Kotlin `wasmWasi` module built with `binaries.executable()` and no `fun main()` is a reactor:

- exports `memory` plus every `@WasmExport` function (raw numeric types);
- imports only `wasi_snapshot_preview1` + declared `@WasmImport`s (here: `env.__convex_*`), so `validate_module`'s allowlist (`env` + `wasi_snapshot_preview1`) passes;
- a Wasm `start` section runs the runtime initializers at instantiation — there is no `_initialize` and no `_start` to call. The runner handles both shapes: `_initialize` is called when present, otherwise the start section ran during `instantiate_async`.

## Fixture

`crates/wasm_runner/tests/fixtures/kotlin_guest/`:

- `settings.gradle.kts`, `build.gradle.kts` — minimal Kotlin Multiplatform build, `kotlin("multiplatform") version "2.3.0"`, target `wasmWasi { binaries.executable() }`.
- `src/wasmWasiMain/kotlin/Guest.kt` — the ABI implementation (the KMP source-set layout is `wasmWasiMain`, not `main`):
  - `@WasmImport("env", "__convex_*")` external functions for `input_length` / `input_load` / `output_set` / `error_set` / `log`;
  - `@WasmExport("__convex_run")` / `@WasmExport("__convex_functions")` dispatchers, echo-style like the C guest, using the public `kotlin.wasm.unsafe.withScopedMemoryAllocator` arena (the host-alloc callback pattern the runner's ABI assumes);
  - no `println` / `kotlin.random` — the runner's WASI context has no stdio and provides deterministic clocks and RNG.
- `README.md` — prerequisites and exact build commands.
- `.gitignore` — build output and the generated `.wasm`.

Build (JDK 11+ and Gradle 8.x):

```bash
cd crates/wasm_runner/tests/fixtures/kotlin_guest
gradle build --console=plain
find build -name '*.wasm'     # -> build/bin/wasmWasi/**/kotlin_guest.wasm
```

## E2E test

`crates/wasm_runner/tests/kotlin_guest_e2e.rs` builds the fixture with `gradle build` (only when `gradle` exists and runs), locates the produced `.wasm` (preferring optimized/production artifacts), and asserts:

1. `echo` round-trips a string through a real Transaction;
2. an unknown function produces a guest error, not a host panic;
3. `analyze_functions` returns `["echo"]`.

The test returns early (skip) when the Kotlin toolchain is missing, mirroring the Go/C/C++ guest tests; the CI absence is recorded in the [matrix row](non-js-languages.md).

## Runner compatibility

wasmtime 47 under the runner's exact `Config` runs a Kotlin module's GC + new-exception-handling + start-section requirements with no extra configuration: function-references, GC, and exceptions are all default-on. The engine-side WasmGC proof is `cargo run -p wasm_runner --example gc_spike` (see [wasm.md](wasm.md)).

## Constraints

- `validate_module` rejects modules whose declared memory maximum exceeds `WasmLimits::default().max_memory_bytes` (256 MiB); a module declaring a larger max fails validation loudly.
- Kotlin `wasmWasi` is Beta and the toolchain changes quickly; the `_initialize` export was removed in Kotlin 2.4 in favor of the start section, and the runner handles both. Pin the Kotlin version in `build.gradle.kts`.
- Guest output goes through `__convex_log`, not WASI `fd_write` — the runner's WASI context has no stdio configured.

## References

- `@WasmImport` / `@WasmExport` API (both targets, since Kotlin 1.8): https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-import/ and https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-export/
- Official wasmWasi template (Kotlin 2.3.0, imports `wasi_snapshot_preview1`, `@WasmExport`): https://github.com/Kotlin/kotlin-wasm-wasi-template
- Kotlin/Wasm configuration (WasmGC since 1.9.20; `wasmWasi` uses the new exception-handling proposal by default): https://kotlinlang.org/docs/wasm-configuration.html
- Real-world validation of the exact callback ABI used here (Kotlin 2.4.20, wasmtime-py 47, reactor module without `main`): https://github.com/glandais/vcyclist/blob/develop/docs/kotlin-wasm-wasi.md and https://github.com/glandais/vcyclist/blob/develop/docs/wasm-wasi-abi.md
