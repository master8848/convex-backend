---
name: convex-wasm-backend
description: Author Convex backend functions in Rust/Go/C/C++/Zig/Kotlin as wasm32-wasip1 guests via wasm_runner — when to use WASM vs TypeScript, how to scaffold/build/test, runtime benefits, and current limitations.
---

# convex-wasm-backend

Reference skill for authoring Convex backend functions as `wasm32-wasip1` guests executed by `crates/wasm_runner` (wasmtime 47, async, WASI p1). Covers when to use WASM vs TypeScript, scaffolding/build/deploy, per-language snippets, polyglot composition, testing/perf, local dev, limitations, and troubleshooting. Canonical runtime behavior lives in [wasm.md](../../../docs/wasm.md), authoring guidance in [wasm-best-practices.md](../../../docs/wasm-best-practices.md), language matrix in [non-js-languages.md](../../../docs/non-js-languages.md), ABI in [abi.rs](../../../crates/wasm_runner/src/abi.rs), and the doc standard in [dsh-doc-standards](../dsh-doc-standards/SKILL.md).

## Overview

Convex backend functions normally run as TypeScript/JavaScript on V8/deno_core. The WASM path lets you write the same `query`/`mutation`/`action` functions in Rust, Go, C, C++, Zig, or Kotlin, compile to `wasm32-wasip1`, and run them in the same deployment alongside TypeScript via `crates/wasm_runner`.

Use this skill when the user wants a Convex backend function in a non-JS language, wants to evaluate or migrate to WASM, is scaffolding/debugging a guest, or needs polyglot `convex/` composition.

## When to Use (and When NOT to)

Use WASM when you need native-language tooling/performance, deterministic sandboxing, freestanding small modules, or reuse of existing Rust/Go/C/C++/Zig/Kotlin libraries.

Use WASM when V8 RAM matters — `ISOLATE_EXECUTION_ENABLED=false` (or `--disable-js-engine` locally) skips V8 entirely and saves hundreds of MB for wasm-only deployments (see [wasm.md#deployment-modes](../../../docs/wasm.md#deployment-modes)).

Use WASM when you need one deployment with mixed languages — `convex/messages.ts` + `convex/search.rs` + `convex/analytics.go` merge into one `ApiSurface` by `CanonicalizedModulePath` (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment)).

Do NOT use WASM when you need indexes/filters/pagination, `storage`/`search`/`vector`/`scheduler`/`cron`, `ctx.runQuery`/`runMutation`/`runAction`, `httpAction`, `db.system.*`, or `ctx.auth.getUserIdentity` — those are TS-only today (see Limitations).

Do NOT use WASM when you need `npm` dependencies inside the guest — guests are freestanding wasm modules, not Node.

Prefer TypeScript for schema/auth-config evaluation, HTTP actions, and features that require indexes — keep WASM for the logic that benefits from the guest language and co-locate both in one `convex/` dir.

## Why WASM (Better)

This section is concrete, not aspirational — numbers and mechanisms from the implementation.

### V8 RAM savings (wasm-only mode)

The function runner has two deployment modes selected by `ISOLATE_EXECUTION_ENABLED` in [knobs.rs](../../../crates/common/src/knobs.rs) (default `true`) or the local backend flag `--disable-js-engine`.

Mixed (default) eagerly initializes V8 + ICU + the UDF runtime snapshot; wasm and TS both run.

Wasm-only (`ISOLATE_EXECUTION_ENABLED=false`) never initializes V8 — no ICU data load, no snapshot, no V8 platform threads, no worker isolates — saving hundreds of MB of process RAM for wasm-only deployments; any JS request fails with `JavaScriptExecutionDisabled` instead of loading V8 on demand (see [wasm.md#deployment-modes](../../../docs/wasm.md#deployment-modes) and [wasm-only deployment note](../../notes/implemented/architecture/2026-08-09-optional-js-engine-wasm-only.md)).

### Per-call overhead

Measured on Apple Silicon including transaction, instantiate, host functions, execution, and teardown (see [wasm.md#benchmark-results](../../../docs/wasm.md#benchmark-results) and `cargo bench -p wasm_runner --bench udf_bench` at [udf_bench.rs](../../../crates/wasm_runner/benches/udf_bench.rs)).

`native Rust (echo) ~0 µs/call` — baseline.

`Rust WASM: echo (warm) ~180 µs/call` — wasm execution itself is single-digit µs; the rest is instantiate + host functions + transaction.

`Go WASM: echo (warm) ~2,160 µs/call (Go guest 3.2 MB)` — ~12× Rust due to per-call `_initialize` + GC setup; `C` freestanding matches Rust single-digit µs with zero init (see [wasm.md#language-support](../../../docs/wasm.md#language-support) and [wasm-best-practices.md#per-language-notes](../../../docs/wasm-best-practices.md#per-language-notes)).

Zig reactor is the smallest possible guest: `394 B echo / 837 B args-parsing`, imports only `env` (zero WASI), `ReleaseSmall + fstrip` (see [wasm.md#language-support](../../../docs/wasm.md#language-support) and [zig-guest.md](../../../docs/zig-guest.md)).

### Toolchain reuse

Write in the language whose ecosystem you already use: `convex_sdk` + `#[convex_functions]` for Rust, `//go:wasmimport`/`//go:wasmexport` for Go, freestanding `clang` for C/C++, `zig build-exe` reactor for Zig, Kotlin `wasmWasi` for Kotlin — no custom Convex DSL to learn, just the guest ABI (see [wasm-best-practices.md#per-language-notes](../../../docs/wasm-best-practices.md#per-language-notes)).

Size tuning is per-toolchain and documented: Rust `opt-level="s"` + `lto + strip` → `160 KiB` (`"z"` saves `8-12 KiB`), Go `-ldflags="-s -w" -trimpath` saves `300-500 KiB` (`3.2 MiB → 2.7 MiB`), C `-Oz -flto --gc-sections --icf --strip-all` saves `15-30%` on a `2-3 KiB` guest, Zig `ReleaseSmall + fstrip` stays under `1 KiB` (see [wasm-best-practices.md#per-language-notes](../../../docs/wasm-best-practices.md#per-language-notes) and [Makefile](../../../examples/wasm-guests/Makefile)).

### Sandbox / fuel / timeout determinism

Wasmtime 47 sandbox: no filesystem, no network, no env vars, no inherited stdio; imports restricted to `env` + `wasi_snapshot_preview1` (see [validation.rs](../../../crates/wasm_runner/src/validation.rs) and [wasm.md#security-model](../../../docs/wasm.md#security-model)).

Determinism mirrors the isolate path: WASI clocks and `random_get` are virtualized to the transaction timestamp/RNG seed (`crates/wasm_runner/src/determinism.rs` at [determinism.rs](../../../crates/wasm_runner/src/determinism.rs) + `crates/wasm_runner/src/engine.rs` [engine.rs](../../../crates/wasm_runner/src/engine.rs)), `NaN` canonicalization, `relaxed SIMD` disabled, fuel-based interruption is deterministic, and `__convex_now_ms`/`__convex_random_bytes` are seeded per-invocation via `ChaCha12` (see [wasm.md#runtime-architecture](../../../docs/wasm.md#runtime-architecture) and [wasm-best-practices.md#determinism-is-the-contract](../../../docs/wasm-best-practices.md#determinism-is-the-contract)).

Limits are authoritative: memory cap `256 MiB` via validation + wasmtime limiter ([limits.rs](../../../crates/wasm_runner/src/limits.rs): `DEFAULT_MAX_MEMORY_BYTES`, `DEFAULT_MAX_MODULE_SIZE=32 MiB`, `DEFAULT_MAX_CALL_DATA=16 MiB`), fuel `10B units` ([limits.rs](../../../crates/wasm_runner/src/limits.rs): `DEFAULT_FUEL`), wall-clock timeout `30 s` default ([limits.rs](../../../crates/wasm_runner/src/limits.rs): `DEFAULT_USER_TIMEOUT`) covering host-function blocking — unlike the isolate path's paused-syscall accounting (see [wasm.md#timeouts--limits](../../../docs/wasm.md#timeouts--limits) and [engine.rs](../../../crates/wasm_runner/src/engine.rs)).

### One deployment polyglot via CanonicalizedModulePath / ApiSurface

One `convex/` dir mixes `convex/messages.ts` (TypeScript), `convex/ingest.rs` (Rust), `convex/analytics.kt` (Kotlin) by `CanonicalizedModulePath` into one `ApiSurface`; clients use one `api`/`fullApi` regardless of backend language — `api.search.*` appears alongside `api.messages.*` via `anyApi` Proxy at [api.ts](../../../npm-packages/convex/src/server/api.ts):`431` `createApi` and typed via `ApiFromModules` at [api.ts](../../../npm-packages/convex/src/server/api.ts):`255` (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment) and [polyglot backend note](../../notes/implemented/architecture/2026-08-19-polyglot-backend-and-client-generation.md)).

### Module cache (LRU 128 + AOT serialize)

Per-module compilation is cached in-memory LRU `128` and AOT `Module::serialize`/`deserialize` keyed by `sha256` at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`169` (`serialized_cache`, `cache_serialized`/`get_serialized`); deserialization is used when bytes match the current engine `Config` (wasmtime 47), avoiding recompilation per unique module; cross-restart persistence to disk is caller-owned (the cache is warmable via `cache_serialized`) (see [engine.rs](../../../crates/wasm_runner/src/engine.rs) and [wasm.md#runtime-architecture](../../../docs/wasm.md#runtime-architecture)).

Per-environment execution is bounded by a `64`-permit semaphore at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`85` `MAX_CONCURRENT_EXECUTIONS_PER_ENV` / `152` `execution_semaphore_for_env`, acquired in [server.rs](../../../crates/function_runner/src/server.rs):`423` mirroring the isolate path's per-env limiting (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

## How to Write Backend Code

### Prerequisites

Install only what your guest language needs; `make check` verifies all and explains how to install what is missing (see [Makefile](../../../examples/wasm-guests/Makefile) `check` target and [wasm-best-practices.md#deployment-checklist](../../../docs/wasm-best-practices.md#deployment-checklist)).

Rust: `rustup target add wasm32-wasip1` (`cargo` + `wasm32-wasip1`); Go `>=1.24` with `GOOS=wasip1 GOARCH=wasm` and `-buildmode=c-shared`; C/C++ `LLVM clang`/`clang++` (`--target=wasm32-wasip1 -nostdlib`, Apple clang lacks wasm — `brew install llvm`); Zig `0.16+` with `wasm32-wasip1 reactor` + `ReleaseSmall`; Kotlin `wasmWasi` (JDK + Gradle 8.x, currently `2.3.0` pinned, `2.4` removed `_initialize` in favor of `start` — runner handles both).

### Scaffolding

Templates live in [examples/wasm-guests/](../../../examples/wasm-guests/README.md); each directory is a standalone project pre-wired with ABI imports/exports.

List templates: `./examples/wasm-guests/scaffold.sh --list` (see [scaffold.sh](../../../examples/wasm-guests/scaffold.sh)).

Copy a template: `./examples/wasm-guests/scaffold.sh <lang> <name>` where `<lang>` is `rust|go|c|cpp|zig` — e.g., `./examples/wasm-guests/scaffold.sh rust my_guest` or `./examples/wasm-guests/scaffold.sh c my_guest` — it pre-wires `env` imports, both exports, and the build command.

One-command build: `make -C examples/wasm-guests` builds every supported example into `build/<lang>/*.wasm`; `make -C examples/wasm-guests check` verifies toolchains; `make -C examples/wasm-guests rust go c cpp zig` builds selected languages; `make -C examples/wasm-guests clean` cleans (see [Makefile](../../../examples/wasm-guests/Makefile) and [examples README](../../../examples/wasm-guests/README.md)).

### Project Layout

One `convex/` directory per deployment; extension selects toolchain; generated `api` is language-agnostic.

```
convex/
  messages.ts        // TypeScript query/mutation
  search.rs          // Rust guest (cargo)
  counter/           // Go package (main.go, GOOS=wasip1)
  ingest.c           // C guest (clang freestanding)
  analytics.kt       // Kotlin guest (gradle wasmWasi)
  schema.ts          // shared schema (TS, required)
  _generated/api.ts  // generated — do not edit
```

Invariant: one `convex/` dir, one deployment, one `api`; extension selects toolchain (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment)).

### ABI Essentials

Source of truth is [abi.rs](../../../crates/wasm_runner/src/abi.rs); the host-allocated (Extism) pattern is required reading in [wasm-best-practices.md#respect-the-memory-model](../../../docs/wasm-best-practices.md#respect-the-memory-model).

Guest exports — exactly two `() -> i32` functions:

- `__convex_run() -> i32` — dispatcher; `0` (`RUN_OK`) on success, non-zero after `__convex_error_set` on error.

- `__convex_functions() -> i32` — JSON list `[{"name":"<module>:<fn>","type":"query"|"mutation"|"action"|"httpAction"}]` via `__convex_output_set` for the module analyzer (`analyze_functions` in [engine.rs](../../../crates/wasm_runner/src/engine.rs)).

Host imports — module `env` only (plus `wasi_snapshot_preview1` for Rust std / Go runtime; freestanding C/C++/Zig import `env` only — smallest/fastest):

- `__convex_input_length() -> i32`, `__convex_input_load(offset, dest, len)`, `__convex_alloc(len) -> i32` (offset into host call-data), `__convex_call_data_load(offset, dest, len)`, `__convex_output_set(ptr, len)` (guest memory), `__convex_error_set(ptr, len)` (guest memory), `__convex_log(ptr, len)`, `__convex_now_ms() -> i64`, `__convex_random_bytes(dest, len)`, `__convex_db_get/insert/replace/patch/delete/count/query(args_ptr, args_len) -> i64` packed `(offset<<32|len)` envelope `{"ok":...}|{"err":...}` or `-1` (see [abi.rs](../../../crates/wasm_runner/src/abi.rs)).

Memory is host-allocated: guest never exports an allocator and never reads host memory; guests copy input via `__convex_input_length` + `__convex_input_load`, and DB results come via `__convex_alloc`/`__convex_call_data_load` (the host allocates the call-data buffer; the guest copies out what it needs); results go back through `__convex_output_set`/`__convex_error_set` pointing at guest memory that stays alive until the call returns (see [wasm-best-practices.md#respect-the-memory-model](../../../docs/wasm-best-practices.md#respect-the-memory-model) and [abi.rs](../../../crates/wasm_runner/src/abi.rs)).

Payloads are JSON: input is `{"function":"<name>","args":[...]}`; `__convex_functions` returns `[{"name":...,"type":...}]`; DB args are JSON objects `{"table":"messages","value":{...}}` and return envelopes `{"ok": id}` or `{"err":...}`; numbers are `Float64` semantics — exact `i64`/`bytes` require `ConvexValue` tagged encodings `{"$integer":...}`/`{"$bytes":...}` constructed by the guest (see [wasm.md#the-abi](../../../docs/wasm.md#the-abi) and [wasm.md#limitations](../../../docs/wasm.md#limitations)).

### Per-Language Hello-World Snippets

Each snippet exports `__convex_run` + `__convex_functions` and handles `{"function","args"}`; full runnable examples are in [examples/wasm-guests/](../../../examples/wasm-guests/README.md): `rust/src/lib.rs`, `go/main.go`, `c/guest.c`, `cpp/guest.cpp`, `zig/guest.zig`, `kotlin/README.md`.

#### Rust (convex_sdk query/mutation)

```rust
// convex/search.rs — compiled via cargo build --target wasm32-wasip1 --release
use convex_sdk::{convex_functions, query, mutation, Ctx, Value};

#[convex_functions]
mod search {
    use super::*;
    #[query]
    fn list(ctx: &mut Ctx) -> Vec<Value> {
        ctx.db.query("messages").collect()
    }
    #[mutation]
    fn send(ctx: &mut Ctx, body: String) -> Value {
        ctx.db.insert("messages", convex_sdk::object!{"body"=> body, "author"=> "anon"})
    }
}
```

Macros generate `__convex_run`/`__convex_functions` and `env` imports; release profile uses `opt-level="s"` (or `"z"` for smallest), `lto=true`, `codegen-units=1`, `strip="symbols"`, `panic="abort"` at [rust Cargo.toml](../../../examples/wasm-guests/rust/Cargo.toml):`19` and [fixtures rust Cargo.toml](../../../crates/wasm_runner/tests/fixtures/rust_guest/Cargo.toml):`19`; keep `crate-type=["cdylib"]` and `wasm32-wasip1` (see [wasm-best-practices.md#per-language-notes](../../../docs/wasm-best-practices.md#per-language-notes)).

#### Go (wasmimport/wasmexport echo)

```go
//go:wasmimport env __convex_input_length
func inputLen() int32

//go:wasmimport env __convex_input_load
func inputLoad(offset, dest, len int32)

//go:wasmimport env __convex_output_set
func outputSet(ptr, len int32)

//go:wasmexport __convex_run
func convexRun() int32 {
    n := inputLen()
    buf := make([]byte, n)
    inputLoad(0, int32(uintptr(unsafe.Pointer(&buf[0]))), n)
    // dispatch on {"function":...} and optionally call __convex_db_* via call-data
    outputSet(int32(uintptr(unsafe.Pointer(&buf[0]))), n)
    return 0
}

//go:wasmexport __convex_functions
func convexFunctions() int32 {
    s := `[{"name":"counter:inc","type":"mutation"}]`
    outputSet(int32(uintptr(unsafe.Pointer(unsafe.StringData(s)))), int32(len(s)))
    return 0
}
func main() {} // must be empty — runner calls _initialize
```

Go uses `//go:wasmimport`/`//go:wasmexport`, no `cgo`; the runner calls `_initialize` and registers WASI via `add_to_linker_async` at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`548`/`560`; prefer `json.Number` for int64 precision (see [wasm-best-practices.md](../../../docs/wasm-best-practices.md) and [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs)).

#### C (freestanding, no libc/WASI)

```c
// convex/ingest.c — freestanding env-only
__attribute__((import_module("env"), import_name("__convex_input_length"))) int __convex_input_length(void);
__attribute__((import_module("env"), import_name("__convex_input_load"))) void __convex_input_load(int off, int dst, int len);
__attribute__((import_module("env"), import_name("__convex_output_set"))) void __convex_output_set(int ptr, int len);
__attribute__((import_module("env"), import_name("__convex_error_set"))) void __convex_error_set(int ptr, int len);

__attribute__((visibility("default"))) int __convex_functions(void) {
    const char s[] = "[{\"name\":\"ingest:tick\",\"type\":\"mutation\"}]";
    __convex_output_set((int)s, sizeof(s)-1);
    return 0;
}
__attribute__((visibility("default"))) int __convex_run(void) {
    // input via __convex_input_length + __convex_input_load, dispatch, db via __convex_db_* (call-data)
    const char ok[] = "{\"ok\":1}";
    __convex_output_set((int)ok, sizeof(ok)-1);
    return 0;
}
```

Only `env` imports; no `libc`, no WASI — smallest/fastest single-digit µs; byte-scan JSON with a tiny scanner or minimal parser (see [c/guest.c](../../../examples/wasm-guests/c/guest.c) and [wasm-best-practices.md](../../../docs/wasm-best-practices.md)).

#### Zig (reactor, zero WASI imports)

```zig
// convex/search.zig — reactor, 394-837 B
const std = @import("std");
extern "env" fn __convex_input_length() i32;
extern "env" fn __convex_input_load(off: i32, dst: i32, len: i32) void;
extern "env" fn __convex_output_set(ptr: i32, len: i32) void;

export fn __convex_functions() i32 {
    const s = "[{\"name\":\"search:query\",\"type\":\"query\"}]";
    __convex_output_set(@intFromPtr(s.ptr), s.len);
    return 0;
}
export fn __convex_run() i32 {
    const n = __convex_input_length();
    // load input, dispatch, optional db/query via env, then output
    const ok = "{\"ok\":null}";
    __convex_output_set(@intFromPtr(ok.ptr), ok.len);
    return 0;
}
```

`ReleaseSmall + fstrip`, `wasm32-wasip1`, `--export=__convex_run --export=__convex_functions` required on Zig `0.16` (see [zig/guest.zig](../../../examples/wasm-guests/zig/guest.zig) and [zig-guest.md](../../../docs/zig-guest.md)).

#### Kotlin (wasmWasi, WasmGC, toolchain-gated)

```kotlin
// convex/analytics.kt — Kotlin wasmWasi (wasm32-wasip1 + WasmGC), no main -> reactor via start section
@WasmExport
fun __convex_run(): Int {
    // pull input via env.__convex_input_length/__convex_input_load, dispatch, db via env
    env.__convex_output_set(okPtr, okLen)
    return 0
}
@WasmExport
fun __convex_functions(): Int {
    val s = """[{"name":"analytics:track","type":"mutation"}]"""
    // write s to linear memory and output_set
    return 0
}
@WasmImport("env", "__convex_input_length")
external fun __convex_input_length(): Int
```

Uses `@WasmExport` + `@WasmImport("env", ...)`; reactor with no `main`; WasmGC binary is `MB`-scale (`2-4 MiB`); needs JDK + Gradle (`gradle wasmWasi`); pin `2.3.0` (see [kotlin/README.md](../../../examples/wasm-guests/kotlin/README.md) and [kotlin-guest.md](../../../docs/kotlin-guest.md)).

### Building

One `convex/` dir, multiple toolchains; `make` in [examples/wasm-guests/](../../../examples/wasm-guests/README.md) builds all into `build/<lang>/*.wasm` (see [Makefile](../../../examples/wasm-guests/Makefile)).

Rust: `cargo build --target wasm32-wasip1 --release` (or `cargo build --manifest-path rust/Cargo.toml --target wasm32-wasip1 --release`; `make rust` copies to `build/rust/wasm_guest_example.wasm` at [Makefile](../../../examples/wasm-guests/Makefile):`12`).

Go: `GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -ldflags="-s -w" -trimpath -o guest.wasm .` (from inside the Go package; `go/` uses exactly this at [Makefile](../../../examples/wasm-guests/Makefile):`19` and [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs):`52` and [udf_bench.rs](../../../crates/wasm_runner/benches/udf_bench.rs):`82`).

C: `clang --target=wasm32-wasip1 -Oz -flto -ffunction-sections -fdata-sections -fvisibility=hidden -nostdlib -Wl,--no-entry -Wl,--gc-sections -Wl,--icf=all -Wl,--strip-all -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined -o guest.wasm guest.c` (freestanding; tuned flags at [Makefile](../../../examples/wasm-guests/Makefile):`26` and [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs):`493`).

C++: `clang++ --target=wasm32-wasip1 -Oz -flto -ffunction-sections -fdata-sections -fvisibility=hidden -nostdlib -fno-exceptions -fno-rtti -fno-threadsafe-statics -Wl,--no-entry -Wl,--gc-sections -Wl,--icf=all -Wl,--strip-all -Wl,--export=__convex_run -Wl,--export=__convex_functions -Wl,--allow-undefined -o guest.wasm guest.cpp` (no headers — stock `clang++` has no `libc++` for `wasm32-wasip1` without wasi-sdk sysroot; use builtin integer types at [cpp/guest.cpp](../../../examples/wasm-guests/cpp/guest.cpp):`25`).

Zig: `zig build-exe guest.zig -target wasm32-wasip1 -mexec-model=reactor -O ReleaseSmall -fstrip --export=__convex_run --export=__convex_functions --name guest` (use `wasm32-wasip1` not legacy `wasm32-wasi`; `--export=` required on `0.16`; at [zig_guest_e2e.rs](../../../crates/wasm_runner/tests/zig_guest_e2e.rs):`150` and [Makefile](../../../examples/wasm-guests/Makefile):`42` and [zig-guest.md](../../../docs/zig-guest.md)).

Kotlin: `gradle wasmWasi` (or `./gradlew wasmWasiBinaries` depending on project); `wasmWasi` `binaries.executable()` with no `main` → reactor via `start` section at [kotlin Guest build.gradle.kts](../../../crates/wasm_runner/tests/fixtures/kotlin_guest/build.gradle.kts):`19`; engine enables `wasm_gc` + `wasm_function_references` explicitly at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`117` and caps GC heap to `64 MiB` reservation + `32 MiB` growth at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`88`).

### Deploy

`convex dev` merges every `convex/*` module by `CanonicalizedModulePath` into one `ApiSurface`; clients import one `api` regardless of backend language — TypeScript is bundled, wasm guests compile before deploy (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment)).

```
convex/
  messages.ts   // TypeScript query
  ingest.rs     // Rust guest
  analytics.kt  // Kotlin guest
convex/_generated/api.ts  // generated — do not edit
```

Client call (same for all languages):

```
import { api } from "./convex/_generated/api";
await client.query(api.messages.list, {});
await client.query(api.search.query, { q: "hello" });
```

`convex/_generated/api.ts` is typed via `ApiFromModules` at [api.ts](../../../npm-packages/convex/src/server/api.ts):`255` mapping module paths to functions; `anyApi` is a `Proxy` that builds `FunctionReference` strings on property access at [api.ts](../../../npm-packages/convex/src/server/api.ts):`431` `createApi` (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment) and [examples README](../../../examples/wasm-guests/README.md)).

## Polyglot Composition

One `convex/` dir, one deployment, one `api`; extension selects toolchain — no per-language deployment.

Directory layout:

```
convex/
  messages.ts        # TS query/mutation
  search.rs          # Rust query (compiled to wasm32-wasip1)
  counter/           # Go package (GOOS=wasip1)
  util.c             # C freestanding
```

Build: `cargo build --target wasm32-wasip1 --release` for Rust, `go build` for Go, `clang` for C, `zig build-exe` for Zig — all become `wasm` modules in the same package.

Runtime: `anyApi` Proxy at [api.ts](../../../npm-packages/convex/src/server/api.ts):`431` and `ApiFromModules` at [api.ts](../../../npm-packages/convex/src/server/api.ts):`255`; adding `convex/search.rs` makes `api.search.*` appear alongside `api.messages.*` in `fullApi`; `analyze_functions` at [engine.rs](../../../crates/wasm_runner/src/engine.rs) runs `__convex_functions` for wasm modules while isolate analysis covers TS (see [wasm.md#composing-polyglot-functions-in-one-deployment](../../../docs/wasm.md#composing-polyglot-functions-in-one-deployment)).

Invariant: one `convex/` dir, one deployment, one `api`; `extension selects toolchain` — see [wasm.md](../../../docs/wasm.md) and [non-js-languages.md](../../../docs/non-js-languages.md) for the full language table.

`ApiFromModules` vs `anyApi`: generated `api` is typed via `ApiFromModules` (module paths → functions); `anyApi` is the untyped `Proxy` fallback that builds `FunctionReference` strings on property access — both share `createApi` at [api.ts](../../../npm-packages/convex/src/server/api.ts):`145`.

## Testing & Perf

Follow the runner's own tests; they run real guests against a real `sqlite`-backed `Database`/`Transaction` and skip gracefully when a toolchain is missing.

Primary test: [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs) builds real Rust, Go, C, and C++ guests and exercises reads, writes, table queries, error propagation, deterministic randomness, log lines, and module analysis against a real `Database` + `Transaction` (`ProdRuntime`, `SqlitePersistence` preamble).

Additional e2e: [zig_guest_e2e.rs](../../../crates/wasm_runner/tests/zig_guest_e2e.rs) and [kotlin_guest_e2e.rs](../../../crates/wasm_runner/tests/kotlin_guest_e2e.rs) (Kotlin gated on JDK + Gradle, not run in CI) each build a real guest and run via `run_wasm_udf`.

Fixtures: [tests/fixtures/](../../../crates/wasm_runner/tests/fixtures/) (`rust_guest/`, `go_guest/`, `c_guest/guest.c`, `zig_guest/`, `kotlin_guest/`) provide minimal guests used by the tests.

Benches: `cargo bench -p wasm_runner --bench udf_bench` at [udf_bench.rs](../../../crates/wasm_runner/benches/udf_bench.rs) measures the full per-invocation path (module lookup, instantiate, host functions, execution, result parse) for an `echo` function; `cargo run -p wasm_runner --example gc_spike` at [gc_spike.rs](../../../crates/wasm_runner/examples/gc_spike.rs) proves WasmGC support under the runner's exact `Config`.

Deterministic test pattern (copy from [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs)):

```rust
let rt = ProdRuntime::new();
let db = Database::new(SqlitePersistence::new(...), ...);
let mut tx = db.begin(Identity::system()).await?;
let runner = WasmRunner::new(&wasm_bytes, WasmLimits::default())?;
let out = runner.run(WasmInput{function_name:"search:query".into(), args: r#"[{"q":"hello"}]"#.into()}, &mut tx, ...).await?;
```

Run locally: `cargo test -p wasm_runner --tests` (all), `cargo test -p wasm_runner --test end_to_end -- --nocapture` (single), `make -C examples/wasm-guests check` (toolchain), `cargo bench -p wasm_runner --bench udf_bench` (perf).

## Local Dev

Backend (needs `just` + the backend checkout):

```sh
# terminal 1 — backend
just run-local-backend --instance-name demo-polyglot
# or wasm-only (no V8): ISOLATE_EXECUTION_ENABLED=false just run-local-backend --instance-name demo-polyglot
```

App (per Convex project):

```sh
# terminal 2 — app
convex dev --url http://127.0.0.1:3210
# or: npx convex dev --url http://127.0.0.1:3210
```

Wasm-only mode (`ISOLATE_EXECUTION_ENABLED=false` at [knobs.rs](../../../crates/common/src/knobs.rs) or `just run-local-backend --disable-js-engine`) skips V8 entirely — TS functions, module analysis via isolate, HTTP actions, and schema/auth-config evaluation fail with `JavaScriptExecutionDisabled` (see [wasm.md#deployment-modes](../../../docs/wasm.md#deployment-modes) and [server.rs](../../../crates/function_runner/src/server.rs)).

Recommendation: test both modes; CI should cover wasm-only to catch accidental isolate dependencies (see [wasm.md](../../../docs/wasm.md) and [wasm execution engine note](../../notes/implemented/architecture/2026-08-08-wasm-execution-engine.md)).

## Limitations

Grouped by category; see [wasm.md#limitations](../../../docs/wasm.md#limitations) for the authoritative list and [wasm-best-practices.md](../../../docs/wasm-best-practices.md) for mitigations.

### Functional — must know before shipping

No `indexes`/`filter`/`pagination` in `db.query`: `db.query(table)` is a full table scan returning all documents; `queryPage` (journal-based pagination) is not implemented; query caching therefore cannot produce incremental journals — workaround is full scan + in-guest sort/filter (see [wasm.md#functional](../../../docs/wasm.md#functional)).

No nested UDF calls: `ctx.runQuery`/`runMutation`/`runAction` are not supported in wasm functions — split into two client calls; only `__convex_db_*` host functions exist (see [abi.rs](../../../crates/wasm_runner/src/abi.rs)).

No `httpAction` for wasm: `#[http_action]` registers a descriptor but the runner rejects http action invocations for `wasm` modules; expose wasm `query`/`mutation`/`action` and wrap with a thin TS `httpAction` that calls `ctx.runAction` if you need HTTP (see [wasm.md#functional](../../../docs/wasm.md#functional)).

No `storage`/`search`/`vector`/`cron`/`scheduler` host functions: only the database operations listed in [abi.rs](../../../crates/wasm_runner/src/abi.rs) (`get`/`count`/`insert`/`replace`/`patch`/`delete`/`query`); `ctx.storage`/`search` have no host function (compile error in `convex_sdk`, trap in hand-written guests) (see [wasm.md#functional](../../../docs/wasm.md#functional)).

No `db.system.*` and no `getUserIdentity`: wasm functions see an `anonymous` identity; system tables are rejected (see [wasm.md#functional](../../../docs/wasm.md#functional)).

Action DB writes are not committed: actions execute against the `Transaction` but their writes are discarded, matching the isolate path's action semantics for the primary write set (see [wasm.md#functional](../../../docs/wasm.md#functional)).

Numbers are `Float64`: plain `i64` results round-trip as floats matching TypeScript `number` semantics; exact `int64`/`bytes` require the `ConvexValue` type with tagged encodings guests construct as `{"$integer":...}`/`{"$bytes":...}` themselves — no helper exists (see [wasm.md#functional](../../../docs/wasm.md#functional)).

### Integration gaps

Deploy pipeline: nothing writes a `.wasm` binary into a source package today; the runtime seam expects the module's `source` to be `base64` wasm, but the bundler/CLI work to produce that is not done; JS bundling at [bundler](../../../npm-packages/convex/src/bundler) does not handle `.rs`/`.go` entry points (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

Analyze: `FunctionRunnerCore::analyze` requires isolate modules; wasm modules need an analyze path that runs `__convex_functions` (the `analyze_functions` helper exists and is tested) at [engine.rs](../../../crates/wasm_runner/src/engine.rs) (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

Query cache: no `QueryJournal` is produced so wasm queries bypass incremental caching benefits (they still get full-function caching via `observed_time`/`observed_rng` flags) (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

Compiled-module persistence: `Module::serialize`/`deserialize` is wired as an in-process AOT cache keyed by `sha256` at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`107`/`169` (`serialized_cache`, `cache_serialized`/`get_serialized`); cross-restart persistence to disk is caller-owned (warmable via `cache_serialized`) (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

System UDFs / components are not supported: `deploy_config` rejects wasm in components (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps)).

Log streaming for actions: `log_line_sender` is wired but action progress log streaming through `log_action_progress` is untested for wasm.

### Determinism

WASI clocks and `random_get` are virtualized to the transaction timestamp/RNG seed so `Rust std` (`SystemTime`, `getrandom`) and Go runtime (`math/rand`, `crypto/rand`) are reproducible per retry; `Go` spec-randomized map iteration is inherently nondeterministic — documented for Go SDK authors (see [wasm.md#determinism](../../../docs/wasm.md#determinism) and [determinism.rs](../../../crates/wasm_runner/src/determinism.rs)).

Fuel-based interruption is deterministic; epoch interruption is not used.

### Timeouts & limits

Wall-clock timeout (`WasmLimits::timeout`, default `30 s` at [limits.rs](../../../crates/wasm_runner/src/limits.rs):`15`) is authoritative and covers time spent inside host functions; fuel (`10B` units at [limits.rs](../../../crates/wasm_runner/src/limits.rs):`13`) bounds CPU; memory is capped at `256 MiB` via both module validation and the wasmtime limiter at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`92` (see [wasm.md#timeouts--limits](../../../docs/wasm.md#timeouts--limits) and [validation.rs](../../../crates/wasm_runner/src/validation.rs)).

The isolate path's user/system time distinction (time paused during syscalls) is not replicated — the `30 s` budget includes DB time.

### Per-env concurrency

Wasm executions are bounded by a per-environment `64`-permit semaphore at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`85` `MAX_CONCURRENT_EXECUTIONS_PER_ENV` / `152` `execution_semaphore_for_env` mirroring the isolate path's per-env limiting in `crates/isolate/src/concurrency_limiter.rs:109`; acquired in [server.rs](../../../crates/function_runner/src/server.rs):`423`.

### Per-language costs

Go per-call `_initialize` cost: the Go runtime re-initializes per call (GC setup), making Go `~12×` slower than Rust/C/Zig; a per-module `Store` pool would avoid this (see [wasm.md#integration-gaps](../../../docs/wasm.md#integration-gaps) and [wasm-best-practices.md#per-language-notes](../../../docs/wasm-best-practices.md#per-language-notes)).

Dart is blocked upstream: `dart2wasm 3.12` emits legacy exception-handling instructions that `wasmtime 47` rejects, and every module imports a JS host (`dart2wasm.*` helpers, `wasm:js-string` builtins, string-constant globals) with no standalone target in stable SDKs; `wasmtime 47` runs `WasmGC` modules under the runner's exact `Config` (`cargo run -p wasm_runner --example gc_spike` at [gc_spike.rs](../../../crates/wasm_runner/examples/gc_spike.rs)), but validation at [validation.rs](../../../crates/wasm_runner/src/validation.rs):`126` still rejects `dart2wasm.*`/`wasm:js-string` until upstream fixes `sdk#54394` (see [dart-guest.md](../../../docs/dart-guest.md)).

Kotlin is toolchain-gated: JDK `11+` + Gradle `8.x`, not in CI at [kotlin_guest_e2e.rs](../../../crates/wasm_runner/tests/kotlin_guest_e2e.rs):`51`.

## Do / Don't Checklist

Do use `convex_sdk` + `#[convex_functions]` for Rust; `//go:wasmimport`/`//go:wasmexport` for Go; freestanding `env`-only for C/C++/Zig.

Do keep `__convex_run` + `__convex_functions` exports exact — missing one fails `validate_module` at [validation.rs](../../../crates/wasm_runner/src/validation.rs).

Do call `__convex_input_length` + `__convex_input_load` to pull `{"function","args"}` and `__convex_output_set`/`__convex_error_set` to return via guest memory that lives until the call returns.

Do use `__convex_now_ms` / `__convex_random_bytes` for time/randomness — never wall clocks or unseeded RNGs (see [wasm-best-practices.md#determinism-is-the-contract](../../../docs/wasm-best-practices.md#determinism-is-the-contract)).

Do keep modules small: `opt-level=s`/`z`, `-O3`/`-Oz`, `lto`, `strip`, `fstrip`, `--gc-sections` (see [wasm-best-practices.md#shape-the-module-correctly](../../../docs/wasm-best-practices.md#shape-the-module-correctly)).

Do test via [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs) pattern (real `Database` + `Transaction`) and `cargo bench -p wasm_runner --bench udf_bench` for perf; `make -C examples/wasm-guests check` before `convex dev`.

Do keep `convex/_generated/**` generated — never edit; `api.search.*` appears automatically via [api.ts](../../../npm-packages/convex/src/server/api.ts).

Don't rely on `indexes`/`filter`/`paginate`/`storage`/`search`/`vector`/`cron`/`scheduler`/`runQuery`/`httpAction`/`system tables`/`getUserIdentity` in wasm — use TS for those and keep wasm for compute.

Don't use `i64` without `ConvexValue` tagged encodings if you need exact integers — plain numbers are `Float64`.

Don't use `WASI` filesystem/network/env vars/subprocesses — the WASI surface is restricted; `db.*` host functions are the only data access.

Don't forget per-language flags: Zig `0.16` needs `--export=`, Apple clang needs `brew install llvm`, Go `>=1.24` needs `wasip1` + `c-shared`, Kotlin needs `JDK+Gradle` and pins `2.3.0`.

Don't model actions as committing writes — action DB writes are discarded.

## Troubleshooting

`wasm32-wasip1` missing: `rustup target add wasm32-wasip1` (seen as `error: target 'wasm32-wasip1' not found`); verify with `rustup target list --installed | grep wasm32-wasip1`.

Apple clang lacks `wasm`: `clang --target=wasm32-wasip1` fails on Apple clang; install LLVM clang via `brew install llvm` and use `$(brew --prefix llvm)/bin/clang` (see [Makefile](../../../examples/wasm-guests/Makefile):`26` comment).

`__convex_functions not found` / `__convex_run not found`: guest didn't export it; check `-Wl,--export=__convex_run -Wl,--export=__convex_functions` (C/C++/Zig) or `#[convex_functions]` macros (Rust) or `//go:wasmexport` (Go); Zig `0.16` requires explicit `--export=` (see [wasm-best-practices.md](../../../docs/wasm-best-practices.md) and [validation.rs](../../../crates/wasm_runner/src/validation.rs)).

`DB_ERROR -1` from `db` host function: system error (e.g., call data overflow `>16 MiB` at [limits.rs](../../../crates/wasm_runner/src/limits.rs):`10` `DEFAULT_MAX_CALL_DATA`) or malformed envelope — check arg size and `{"table","value"}` shape; see [abi.rs](../../../crates/wasm_runner/src/abi.rs) envelope `{"ok":...}|{"err":...}`.

`validate_module too large`: module exceeds `32 MiB` at [limits.rs](../../../crates/wasm_runner/src/limits.rs):`8` `DEFAULT_MAX_MODULE_SIZE`; strip debug (`-s -w`, `ReleaseSmall`, `fstrip`, `-Wl,--strip-all`) and check `opt-level`/`lto` (see [validation.rs](../../../crates/wasm_runner/src/validation.rs)).

`ISOLATE_EXECUTION_ENABLED=false` errors: expected — isolate-dependent code (TS analysis, `httpAction`) is disabled in that mode at [server.rs](../../../crates/function_runner/src/server.rs); set `ISOLATE_EXECUTION_ENABLED=true` (default) or use wasm-only guests.

`Go wasip1 requires Go >=1.24`: older Go uses `GOOS=wasip1` but guest needs `c-shared` `buildmode` from `1.24`; check `go version`.

`convex dev` doesn't see `.wasm`: file must be under `convex/` and end with `.wasm`; run `make -C examples/wasm-guests check` to verify toolchain actually produced output in `build/`.

`WASI clocks virtualized` confusion: `SystemTime`/`time.Now()` return virtual time — don't compare to wall clock in tests; use `__convex_now_ms`.

`Fuel vs wall-clock`: fuel bounds CPU instructions, wall-clock bounds whole invocation including DB host calls — if a query scans a large table, `30 s` wall-clock may hit before fuel.

`Per-env 64 permits` starvation: semaphore at [engine.rs](../../../crates/wasm_runner/src/engine.rs):`85` limits concurrent wasm executions per env to `64`; bursts above that queue.

## References

Exact file paths — verify with `ls` before linking; all relative links below are from this skill file (`../../../` → repo root).

- ABI: [abi.rs](../../../crates/wasm_runner/src/abi.rs) (`HOST_FN_MODULE=env`, `GUEST_RUN=__convex_run`, `GUEST_FUNCTIONS=__convex_functions`, host imports `INPUT_LENGTH`/`INPUT_LOAD`/`CALL_DATA_ALLOC`/`CALL_DATA_LOAD`/`OUTPUT_SET`/`ERROR_SET`/`LOG`/`NOW_MS`/`RANDOM_BYTES`/`DB_*`)

- Runner: [engine.rs](../../../crates/wasm_runner/src/engine.rs) (`WasmRunner`, `analyze_functions`, `execution_semaphore_for_env`, `MAX_CONCURRENT_EXECUTIONS_PER_ENV`, `serialized_cache`/`cache_serialized`/`get_serialized`, GC heap caps, WASI `p1` via `add_to_linker_async`), [limits.rs](../../../crates/wasm_runner/src/limits.rs) (`WasmLimits`, `DEFAULT_FUEL=10B`, `DEFAULT_MAX_MEMORY_BYTES=256 MiB`, `DEFAULT_MAX_MODULE_SIZE=32 MiB`, `DEFAULT_MAX_CALL_DATA=16 MiB`, `DEFAULT_USER_TIMEOUT=30s`), [validation.rs](../../../crates/wasm_runner/src/validation.rs) (`validate_module`), [determinism.rs](../../../crates/wasm_runner/src/determinism.rs) (virtual clocks, seeded RNG, NaN canonicalization), [db.rs](../../../crates/wasm_runner/src/db.rs) (DB host functions)

- Knob: [knobs.rs](../../../crates/common/src/knobs.rs):`955` `ISOLATE_EXECUTION_ENABLED` (`env_config("ISOLATE_EXECUTION_ENABLED", true)`)

- Function runner gate: [server.rs](../../../crates/function_runner/src/server.rs):`423` (checks `*ISOLATE_EXECUTION_ENABLED`, error `ISOLATE_EXECUTION_ENABLED=false`)

- Examples: [examples/wasm-guests/](../../../examples/wasm-guests/README.md) ([Makefile](../../../examples/wasm-guests/Makefile), [scaffold.sh](../../../examples/wasm-guests/scaffold.sh), [rust/src/lib.rs](../../../examples/wasm-guests/rust/src/lib.rs) / [rust Cargo.toml](../../../examples/wasm-guests/rust/Cargo.toml), [go/main.go](../../../examples/wasm-guests/go/main.go), [c/guest.c](../../../examples/wasm-guests/c/guest.c), [cpp/guest.cpp](../../../examples/wasm-guests/cpp/guest.cpp), [zig/guest.zig](../../../examples/wasm-guests/zig/guest.zig), [kotlin/README.md](../../../examples/wasm-guests/kotlin/README.md))

- Client API: [api.ts](../../../npm-packages/convex/src/server/api.ts) (`ApiFromModules` ~:255, `anyApi` Proxy ~:431, `createApi` ~:145), generated `convex/_generated/api.ts` (`do not edit` — generated at deploy time)

- Tests: [end_to_end.rs](../../../crates/wasm_runner/tests/end_to_end.rs), [zig_guest_e2e.rs](../../../crates/wasm_runner/tests/zig_guest_e2e.rs), [kotlin_guest_e2e.rs](../../../crates/wasm_runner/tests/kotlin_guest_e2e.rs), [fixtures](../../../crates/wasm_runner/tests/fixtures/), [udf_bench.rs](../../../crates/wasm_runner/benches/udf_bench.rs), [gc_spike.rs](../../../crates/wasm_runner/examples/gc_spike.rs)

- Docs: [wasm.md](../../../docs/wasm.md) (runtime, deployment modes, polyglot, benchmarks, limitations, security), [wasm-best-practices.md](../../../docs/wasm-best-practices.md) (determinism, memory model, module shape, limits, transaction, testing, per-language notes, checklist), [non-js-languages.md](../../../docs/non-js-languages.md) (status matrix), [zig-guest.md](../../../docs/zig-guest.md), [kotlin-guest.md](../../../docs/kotlin-guest.md), [dart-guest.md](../../../docs/dart-guest.md)

- SDK: `crates/convex_sdk` + `crates/convex_sdk_macros` (`#[convex_functions]`, `#[query]`/`#[mutation]`/`#[action]`)

- Notes: [wasm execution engine note](../../notes/implemented/architecture/2026-08-08-wasm-execution-engine.md), [wasm-only deployment note](../../notes/implemented/architecture/2026-08-09-optional-js-engine-wasm-only.md), [fixtures note](../../notes/implemented/feature/2026-08-11-guest-language-fixtures-and-examples.md), [examples note](../../notes/implemented/feature/2026-08-11-wasm-examples-and-best-practices.md), [polyglot backend note](../../notes/implemented/architecture/2026-08-19-polyglot-backend-and-client-generation.md)

## Appendix: Quick Command Reference

```sh
rustup target add wasm32-wasip1
make -C examples/wasm-guests check
make -C examples/wasm-guests            # build all guests -> build/<lang>/*.wasm
cargo build --target wasm32-wasip1 --release
GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -ldflags="-s -w" -trimpath -o guest.wasm .
clang --target=wasm32-wasip1 -Oz -flto -nostdlib -Wl,--no-entry -Wl,--export=__convex_run -Wl,--export=__convex_functions -o guest.wasm guest.c
zig build-exe guest.zig -target wasm32-wasip1 -mexec-model=reactor -O ReleaseSmall -fstrip --export=__convex_run --export=__convex_functions
gradle wasmWasi                          # Kotlin
cargo test -p wasm_runner --tests
cargo bench -p wasm_runner --bench udf_bench
just run-local-backend --instance-name demo
ISOLATE_EXECUTION_ENABLED=false just run-local-backend --instance-name demo
convex dev --url http://127.0.0.1:3210
```
