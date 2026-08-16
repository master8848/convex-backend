# Agent Note: WASM execution engine for backend functions

Status: implemented

## Problem

Backend functions ran only in the V8 isolate or the Node executor. Supporting other languages (Rust, Go, C/C++, and the rest of the non-JS matrix) required an execution path that is sandboxed, deterministic, and independent of the JS engine.

## Decision

A complete WASM execution engine ships in `crates/wasm_runner` and `crates/convex_sdk` (plus `convex_sdk_macros`):

- **Runtime**: wasmtime 47 (wasm32-wasip1), an async per-call `Store`, and host functions for input/output, log, `now`, and random. The guest ABI is the reactor shape: guest exports `__convex_run` / `__convex_functions`, imports `env` host functions, and uses host-allocated memory.
- **Sandbox**: the engine applies the sandbox caps from the security hardening work — WasmGC heap reservation capped to the linear-memory budget, wall-clock compile/instantiate timeout behind a bounded compile semaphore, bound-checked guest reads before allocation, and an exact WASI p1 / `env` host surface enumerated at validation time.
- **Integration**: `crates/wasm_runner` plugs into the function runner (`crates/functions_runner` and friends) so wasm functions execute through the same transaction and UDF lifecycle as TypeScript functions.
- **Language support**: the Rust guest SDK (`convex_sdk` + derive macros) makes Rust a first-class guest; Go, C/C++, Zig, and Kotlin guests follow with fixtures and e2e tests.

## Alternatives considered

- **Add other runtimes inside the isolate worker**: each additional language would share V8's memory and determinism model and re-implement the UDF host API per language; wasm is a single target that already has mature toolchains for every candidate language.
- **A separate sidecar service per language**: serializes calls and doubles the transport surface; in-process wasmtime gives the same isolation with direct transaction access.
- **Compile-to-JS transpilation**: loses native FFI, standard libraries, and produces non-deterministic output on many platforms.

## Consequences

- The ABI contract (`__convex_run` / `__convex_functions`, reactor shape, `env` imports, host-allocated memory) is now part of the runtime's compatibility surface: guests that meet it run on this engine.
- A deployment can run wasm-only (see the optional JS engine note) and skip V8 entirely.
- Implementation details and limitations are documented in `docs/wasm.md`; the language status matrix lives in `docs/non-js-languages.md`.
