# Kotlin guest: Kotlin Multiplatform `wasmWasi` (WasmGC + WASI Preview 1)

Status: **fixture + e2e test committed; build untested locally — needs a Kotlin
toolchain (JDK + Gradle)**. The integration assumptions below are verified
against wasmtime 47.0.3's defaults and a real-world wasmtime-hosted Kotlin POC
(see [Evidence](#evidence)).

## Decision: `wasmWasi` target (Option A), not Kotlin/Wasm `wasmJs` (Option B)

Both Kotlin/Wasm targets expose the export/import annotations this ABI needs —
`@WasmExport` / `@WasmImport` (both since Kotlin 1.8, both targets, "without
type adapters": raw `Int` → `i32`). But only **`wasmWasi`** has a real
export-ABI path for a non-JS host:

| | `wasmWasi` (chosen) | `wasmJs` (WasmGC + JS interop) |
|---|---|---|
| Module | wasm32-wasip1 + WasmGC | WasmGC, designed for JS hosts |
| Imports | `wasi_snapshot_preview1` + declared `@WasmImport`s only | JS functions (e.g. `console.*`) under module `env`, driven by generated JS glue |
| Runner fit | WASI p1 already registered; `env` host funcs already registered | needs a JS host; unsupported in wasmtime (Kotlin slack: "wasmtime currently seems to be unusable for running Kotlin-generated code") |
| Exports | `@WasmExport("__convex_run") fun convexRun(): Int` | same annotation, but module is glued to a JS runtime |

Key references:

- `@WasmImport`/`@WasmExport` API (both targets, since Kotlin 1.8):
  https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-import/
  https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-export/
- Official wasmWasi template (Kotlin 2.3.0, imports `wasi_snapshot_preview1`,
  `@WasmExport`):
  https://github.com/Kotlin/kotlin-wasm-wasi-template
- Kotlin/Wasm uses WasmGC since 1.9.20; `wasmWasi` uses the *new* exception
  handling proposal (`exnref`) by default:
  https://kotlinlang.org/docs/wasm-configuration.html
- wasmtime 47.0.3 defaults (verified in the vendored crate source,
  `src/config.rs`): `wasm_function_references` = true, `wasm_gc` = true,
  `wasm_exceptions` (new EH) = true. New-EH support landed in wasmtime
  Aug 2025 (bytecodealliance/wasmtime#11326; https://bytecodealliance.org/articles/wasmtime-exceptions).
- Real-world validation of the exact callback ABI used here (Kotlin 2.4.20,
  wasmtime-py 47, reactor module without `main`):
  https://github.com/glandais/vcyclist/blob/develop/docs/kotlin-wasm-wasi.md
  https://github.com/glandais/vcyclist/blob/develop/docs/wasm-wasi-abi.md

## Module shape (what the runner will see)

A Kotlin `wasmWasi` module built with `binaries.executable()` and **no
`fun main()`** is a *reactor*:

- exports: `memory`, plus every `@WasmExport` function (raw numeric types);
- imports: only `wasi_snapshot_preview1` + declared `@WasmImport`s (here:
  `env.__convex_*`), so `validate_module`'s allowlist (`env` +
  `wasi_snapshot_preview1`) passes;
- a Wasm `start` section runs the runtime initializers at instantiation —
  **no `_initialize`, no `_start` to call**. The runner already handles both
  shapes (`_initialize` is called when present; otherwise the start section
  ran during `instantiate_async`).

## Fixture

`crates/wasm_runner/tests/fixtures/kotlin_guest/`:

- `settings.gradle.kts`, `build.gradle.kts` — minimal Kotlin Multiplatform
  build, `kotlin("multiplatform") version "2.3.0"`, target
  `wasmWasi { binaries.executable() }`.
- `src/wasmWasiMain/kotlin/Guest.kt` — the ABI implementation (note the KMP
  source-set layout: `wasmWasiMain`, not `main`):
  - `@WasmImport("env", "__convex_*")` external functions for
    `input_length`/`input_load`/`output_set`/`error_set`/`log`;
  - `@WasmExport("__convex_run")` / `@WasmExport("__convex_functions")`
    dispatchers, echo-style like the C guest, using the public
    `kotlin.wasm.unsafe.withScopedMemoryAllocator` arena (the same
    host-alloc callback pattern the runner's ABI assumes and the vcyclist POC
    validates);
  - no `println` / `kotlin.random` (the runner's WASI ctx has no stdio and
    provides deterministic clocks/RNG).
- `README.md` — prerequisites and exact build commands.
- `.gitignore` — build output and the generated `.wasm`.

Build (on a machine with JDK 11+ and Gradle 8.x):

```bash
cd crates/wasm_runner/tests/fixtures/kotlin_guest
gradle build --console=plain
find build -name '*.wasm'     # -> build/bin/wasmWasi/**/kotlin_guest.wasm
```

## E2E test

`crates/wasm_runner/tests/kotlin_guest_e2e.rs` (new; `tests/` is
auto-discovered): copies the `new_database` / `create_table` / `run_function`
helpers from `end_to_end.rs`, builds the fixture with `gradle build`, locates
the produced `.wasm` (prefers optimized/production artifacts), and asserts:

1. `echo` round-trips a string through a real Transaction;
2. an unknown function produces a guest error, not a host panic;
3. `analyze_functions` returns `["echo"]`.

The test returns early when `gradle` is missing or cannot run (no JDK),
mirroring the Go/C/C++ guest tests. Verified locally: it skips cleanly.

## Verified against the runner (no Kotlin toolchain needed)

A temporary proof test compiled a hand-written WasmGC module shaped like
Kotlin's wasmWasi output — WasmGC struct + global, `env` imports, `memory`
export, `() -> i32` exports, and a `start` section that allocates a GC struct
at instantiation — and ran it through the full runner path:

- `validate_module` accepts it (imports ⊆ `env` + `wasi_snapshot_preview1`;
  `__convex_run`/`__convex_functions` are `() -> i32`);
- `instantiate_async` with the WASI p1 + `env` linker runs the start section;
- `run_wasm_udf` echo against a sqlite-backed `Database` succeeds; unknown
  functions become `JsError`s; `analyze_functions` parses the descriptor
  JSON.

So a Kotlin module's GC + exceptions + start-section requirements are met by
wasmtime 47.0.3 under the runner's exact `Config` (which enables nothing
extra: function-references/GC/exceptions are all default-on).

## Known risks / open items

- **Untested end-to-end with a real Kotlin build** (no JDK/Gradle in this
  environment). First run should be on a machine with the toolchain.
- `validate_module` rejects modules whose declared memory *maximum* exceeds
  `WasmLimits::default().max_memory_bytes` (256 MiB). If a future Kotlin
  module declares a larger max, validation fails loudly — the fixture's tiny
  module is far below it.
- Kotlin `wasmWasi` is still Beta; the toolchain changes quickly (e.g. the
  `_initialize` export was removed in 2.4 in favor of the start section — the
  runner handles both). Pin the Kotlin version in `build.gradle.kts`.
- If the guest ever needs `println`-style output, use `__convex_log` instead
  of WASI `fd_write` (the runner's WASI context has no stdio configured).

## What docs/wasm.md's Kotlin row should say (for the sibling)

Replace the "under research" row with something like:

> Kotlin | ✅ fixture + e2e test (build gated on toolchain) | Kotlin
> Multiplatform `wasmWasi` (wasm32-wasip1 + WasmGC) | `@WasmExport` +
> `@WasmImport("env", ...)` give the exact ABI; reactor module (no `main`)
> self-initializes via the Wasm start section; imports only
> `wasi_snapshot_preview1` + `env`; needs JDK + Gradle to build (untested in
> CI). See `docs/kotlin-guest.md`.
