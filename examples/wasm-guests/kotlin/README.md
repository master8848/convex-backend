# Kotlin guest — in progress

Status: 🚧 **in progress**. The Kotlin Multiplatform `wasmWasi` target
(wasm32-wasip1) is under research (see docs/wasm.md). Key questions being resolved:

- can a `wasmWasi` module EXPORT `__convex_run` / `__convex_functions`
  (the `@WasmExport` / `@JsExport` story), and with what toolchain
- whether the produced module needs WasmGC (which wasmtime 47 now runs per the
  gc_spike) or runs on wasip1 directly

What lands here once validated: `guest.kt` implementing the ABI, the gradle
build wiring, `make kotlin`, and an end-to-end test. Until then, Kotlin/Native
or JVM developers can use the **C++ template** (Kotlin/Native can link C)
or the **Rust template** (Kotlin/Rust interop via C ABI).
