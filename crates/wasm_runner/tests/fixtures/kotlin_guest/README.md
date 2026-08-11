# Kotlin guest fixture (WasmGC + WASI Preview 1)

A fixture Kotlin guest module implementing the Convex WASM ABI
(`crates/wasm_runner/src/abi.rs`), used by the wasm_runner integration tests
(`crates/wasm_runner/tests/kotlin_guest_e2e.rs`).

## Target

Kotlin Multiplatform **`wasmWasi`** target: `wasm32-wasip1` + WasmGC, compiled
with the Kotlin Gradle plugin (Kotlin 2.3.0; 2.4.x also works). Kotlin 1.9.20+
emits WasmGC for both wasm targets, and `wasmWasi` uses the *new* exception
handling proposal (`exnref`) by default. wasmtime 47 enables all three
proposals Kotlin emits — function-references, `gc`, `exceptions` — by
default, and the runner registers WASI preview1 plus the `env` host
functions, so the module runs without any JS glue.

This is a **reactor** module: there is no `fun main()`, so the compiler emits
a Wasm `start` section that runs the runtime initializers at instantiation
(no `_initialize`/`_start` export to call). The runner handles both forms.

## Why wasmWasi and not wasmJs?

`@WasmExport`/`@WasmImport` exist on both wasm targets since Kotlin 1.8, but
the `wasmJs` (WasmGC + JS-interop) target is designed for JS hosts: its
runtime imports JS functions (e.g. `console.*`) under module `env` and is
meant to be driven by generated JS glue. `wasmWasi` imports only
`wasi_snapshot_preview1` (provided by the runner) plus the `@WasmImport`s you
declare, and is the officially supported shape for standalone VMs
(Node, Deno, WasmEdge, wasmtime).

## Prerequisites

- JDK 11+ (Temurin 17 recommended): the Gradle JVM.
- Gradle 8.x on `PATH` (or vendor a wrapper: `gradle wrapper`).
- Network access to Maven Central / the Gradle Plugin Portal on first build
  (resolves the Kotlin Gradle plugin, stdlib, and binaryen).

## Build

```bash
cd crates/wasm_runner/tests/fixtures/kotlin_guest
gradle build --console=plain
```

The module lands under `build/bin/wasmWasi/...` (the exact path depends on
the Kotlin/Gradle versions; look for `kotlin_guest.wasm`):

```bash
find build -name '*.wasm'
```

`tests/kotlin_guest_e2e.rs` locates the artifact the same way and skips the
test when no Kotlin toolchain (gradle + JDK) is available.

## Verify the module shape

With `wasm-tools` (or `wasmtime`):

```bash
wasm-tools print build/bin/wasmWasi/debugExecutable/kotlin_guest.wasm | head -50
```

Expected: imports only from `wasi_snapshot_preview1` and `env`
(`__convex_*`); exports `memory`, `__convex_run` (`() -> i32`),
`__convex_functions` (`() -> i32`).

## Notes / status

- **Untested locally in this repo** — building requires a JDK + Gradle, which
  are not installed in the development environment. The source follows the
  patterns validated by the official Kotlin WASI template
  (`Kotlin/kotlin-wasm-wasi-template`, Kotlin 2.3.0) and by a real-world
  wasmtime-hosted POC (July 2026, Kotlin 2.4.20-Beta2 + wasmtime-py 47),
  which exercises the exact callback ABI used here.
- The guest deliberately avoids `println` and `kotlin.random`: the runner's
  WASI context has no stdio configured and provides deterministic
  clocks/RNG; all logging goes through `__convex_log` if needed.
