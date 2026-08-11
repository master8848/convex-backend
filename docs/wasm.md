# WASM backend functions: implementation & limitations

This document describes the WebAssembly execution path for Convex backend
functions, how it relates to the existing TypeScript (isolate) and Node
paths, and its current limitations.

## Deployment modes

The function runner supports two JS-engine modes, controlled by the
`ISOLATE_EXECUTION_ENABLED` env var (default `true`) or the local backend's
`--disable-js-engine` flag:

- **Mixed (default)**: V8 + ICU + the UDF runtime snapshot are initialized
  eagerly at startup; wasm and TypeScript functions both run.
- **Wasm-only**: V8 is never initialized — no ICU data load, no UDF snapshot,
  no V8 platform threads, no worker isolates. This saves hundreds of MB of
  process RAM for deployments that only run wasm functions. Any request that
  needs the JS engine (TypeScript functions, module analysis, HTTP actions,
  schema/auth-config evaluation) fails with a clear
  `JavaScriptExecutionDisabled` error instead of loading V8 on demand.

Related: fuzz-related V8 flags passed via `ISOLATE_V8_FLAGS` (`--jit-fuzzing`,
`--experimental-fuzzing`, `--randomize-hashes`) are dropped by default
because they break UDF determinism; set `V8_ALLOW_FUZZING_FLAGS=true` to keep
them for local runtime fuzzing.

## What exists

A complete, tested WASM execution engine in `crates/wasm_runner`, a Rust
guest SDK in `crates/convex_sdk` (+ `convex_sdk_macros`), a Go guest
fixture, and integration in the function runner.

### Architecture

```
Rust/Go source ──compile──▶ wasm32-wasip1 module
                                     │
                                     ▼
                  wasm_runner (wasmtime 47, async, per-call Store)
                  - host functions: input/output, log, now, random,
                    db get/count/insert/replace/patch/delete/query
                  - determinism: seeded ChaCha12 RNG, virtual clocks,
                    NaN canonicalization, relaxed SIMD disabled
                  - limits: memory cap (256 MiB), fuel (10B units),
                    wall-clock timeout (30 s default)
                  - WASI p1 via add_to_linker_async (Rust std, Go runtime)
                  - module compile cache keyed by sha256
                                     │
                                     ▼
                  Transaction (real DB reads/writes, commit or abort)
```

### The ABI

Guests export two functions:

- `__convex_run() -> i32` — the dispatcher. The input payload
  (`{"function": "...", "args": [...]}`) is pulled via host functions.
- `__convex_functions() -> i32` — a JSON list of
  `{"name": "...", "type": "query"|"mutation"|"action"|"httpAction"}`
  descriptors, used by the module analyzer.

Memory crossing the boundary is host-allocated (the Extism pattern), so any
language that can import functions and export one `i32` function can
implement the guest side. See `crates/wasm_runner/src/abi.rs`.

### Language support

| Language | Status | Toolchain | Notes |
|---|---|---|---|
| Rust | ✅ **valid target** | `wasm32-wasip1`, `cargo build` | `#[convex_functions]` + `#[query]`/`#[mutation]`/`#[action]` macros; example: `examples/wasm-guests/rust` |
| Go | ✅ **valid target** | native Go `GOOS=wasip1 GOARCH=wasm -buildmode=c-shared` (Go ≥ 1.24) | `//go:wasmexport` + `//go:wasmimport`; `_initialize` called by the runner; example: `examples/wasm-guests/go` |
| C | ✅ **valid target** | stock LLVM clang `--target=wasm32-wasip1 -nostdlib` | Freestanding guest (no libc, no WASI): smallest/fastest. Fixture `tests/fixtures/c_guest/guest.c` + end-to-end test; example: `examples/wasm-guests/c` |
| C++ | ✅ **valid target** | stock LLVM clang++ `--target=wasm32-wasip1 -nostdlib -fno-exceptions -fno-rtti` | Same ABI as C; freestanding C++ rules in `examples/wasm-guests/cpp` (no libc++, no guard vars, POD statics). Serves game engines, Zig, AssemblyScript, Rust `no_std` |
| Dart / Flutter | ❌ **blocked (engine GC ✓)** | `dart compile wasm` → WasmGC | wasmtime 47 (in use here) already runs WasmGC modules (`examples/gc_spike.rs`: struct/array/i31/ref under the runner's exact Config), so the historical "wasmtime ≥ 27 with `gc`" prerequisite is met — but a stock Dart module still cannot run end-to-end: dart2wasm 3.12 emits *legacy* exception-handling instructions that wasmtime 47 rejects, and every module imports a JS host (`dart2wasm.*` helpers, `wasm:js-string` builtins, string-constant globals) with no standalone target in stable SDKs. Flutter mobile stays on Dart AOT native. See `docs/dart-guest.md` |
| Kotlin | ✅ fixture + e2e test (build gated on toolchain) | Kotlin Multiplatform `wasmWasi` (wasm32-wasip1 + WasmGC) | `@WasmExport` + `@WasmImport("env", ...)` give the exact ABI; reactor module (no `main`) self-initializes via the Wasm start section; imports only `wasi_snapshot_preview1` + `env`; needs JDK + Gradle to build (untested in CI). See `docs/kotlin-guest.md` |

Go note: the runner calls `_initialize` before dispatch (required by the Go
runtime), and registers WASI via `add_to_linker_async` so Go's runtime init
(`fd_fdstat_get`, `poll_oneoff`, ...) doesn't hit wasmtime-wasi's blocking
`in_tokio` path inside the async embedding.

C note: the runner validates that modules only import `env` + WASI; a
freestanding C guest imports only `env`, so it is the smallest and fastest
guest (no runtime init, no GC, no WASI), on par with the Rust guest's
single-digit-µs execution cost.

### Examples, scaffolding & best practices

- **Examples**: `examples/wasm-guests/` has a ready-to-build standalone guest
  per language — `rust/`, `go/`, `c/`, `cpp/` (all ✅ valid targets) plus
  `dart/` and `kotlin/` status stubs that land with the in-flight work.
- **One-command build**: `make` in `examples/wasm-guests/` builds every
  supported example; `make check` verifies your toolchains and explains how to
  install what's missing.
- **Scaffold a new guest**: `examples/wasm-guests/scaffold.sh <lang> <name>`
  copies a pre-wired template (ABI imports/exports included) so you never start
  from the raw ABI.
- **Best practices**: `docs/wasm-best-practices.md` — determinism, the
  host-allocated memory model, module shape, limits, the transaction model,
  testing, per-language notes, and a deployment checklist.

### Verification

- `crates/wasm_runner/tests/end_to_end.rs` builds real Rust, Go and C
  guests (C++ joins the same suite) and runs them against a real
  sqlite-backed `Database`: reads, writes, table queries, error propagation,
  deterministic randomness, log lines, and module analysis. Toolchain-missing
  guests are skipped gracefully.
- `cargo bench -p wasm_runner --bench udf_bench` measures the full
  per-invocation path (module lookup, instantiate, host functions,
  execution, result parse) for a `SELECT`-like `echo` function.
- `cargo run -p wasm_runner --example gc_spike` proves the engine's WasmGC
  support (struct/array/i31/ref.eq) under the runner's exact Config — the
  engine-side prerequisite for a future Dart guest.

## Benchmark results (2026-08, Apple Silicon)

```
native Rust (echo)            ~0 µs/call
Rust WASM: echo (warm)      ~180 µs/call
Go WASM: echo (warm)      ~2,160 µs/call   (Go guest 3.2 MB)
```

The Rust WASM number includes transaction begin, wasmtime instantiation,
host-function setup, execution, result parse, and teardown — the wasm
execution itself is single-digit microseconds. Go is ~12× slower because a
fresh `Store` per call runs the full Go runtime initialization
(`_initialize`, GC setup). A per-module store pool would close most of the
gap (see "Future work"). A freestanding C guest has no runtime init, so it
lands at the Rust guest's end of the spectrum.

## Limitations (v1)

### Functional

- **No filters/indexes/pagination in `db.query`**: `db.query(table)` is a
  full table scan returning all documents. `queryPage` (journal-based
  pagination) is not implemented; query caching therefore cannot produce
  incremental journals.
- **No nested UDF calls**: `ctx.runQuery`/`runMutation`/`runAction` are not
  supported in wasm functions.
- **No HTTP actions**: `#[http_action]` registers a descriptor but the
  runner rejects http action invocations for wasm modules.
- **No storage/search/vector/cron/scheduling host functions**: only the
  database operations listed above.
- **No `getUserIdentity`**: wasm functions see an anonymous identity.
  `db.system.*` is unavailable (system tables are rejected).
- **Action DB writes are not committed**: actions execute against the
  transaction but their writes are discarded (matching the isolate path's
  action semantics for the primary write set).
- **Numbers are Float64**: plain `i64` results round-trip as floats,
  matching TypeScript `number` semantics. Exact int64/bytes require the
  `ConvexValue` type with tagged encodings (guests must construct
  `{"$integer": ...}` / `{"$bytes": ...}` themselves; no helper yet).

### Determinism

- WASI clocks and `random_get` are virtualized to the transaction
  timestamp / rng seed, so Rust std (`SystemTime`, `getrandom`) and Go's
  runtime (`math/rand`, `crypto/rand`) are reproducible per retry.
- Fuel-based interruption is deterministic; epoch interruption is not used.
- Go's spec-randomized map iteration is inherently nondeterministic —
  document this for Go SDK authors.

### Timeouts & limits

- The wall-clock timeout (`WasmLimits::timeout`, default 30 s) is the
  authoritative limit and covers time spent inside host functions. Fuel
  (10B units) bounds CPU. Memory is capped at 256 MiB via both module
  validation and the wasmtime limiter.
- The isolate path's user/system time distinction (time paused during
  syscalls) is not replicated: the 30 s budget includes DB time.

### Integration gaps (follow-up work)

- **Deploy pipeline**: nothing yet writes a `.wasm` binary into a source
  package. The runtime seam expects the module's `source` to be base64
  wasm; the bundler/CLI work to produce that is not done. JS bundling
  (`npm-packages/convex/src/bundler`) does not yet handle `.rs`/`.go`
  entry points.
- **Analyze**: `FunctionRunnerCore::analyze` still requires isolate
  modules; wasm modules need an analyze path that runs `__convex_functions`
  (the `analyze_functions` helper exists and is tested).
- **Query cache**: no `QueryJournal` is produced, so wasm queries bypass
  incremental caching benefits (they still get full-function caching via
  `observed_time`/`observed_rng` flags).
- **Compiled-module persistence**: `Module::serialize` (AOT caching across
  restarts) is not wired; modules are recompiled per process.
- **Concurrency limits**: wasm executions don't yet share the application
  layer's per-environment semaphores.
- **Log streaming for actions**: `log_line_sender` is wired, but action
  progress log streaming through `log_action_progress` is untested for
  wasm.
- **Go performance**: a per-module `Store` pool would avoid re-running the
  Go runtime `_initialize` per call.
- **System UDFs / components**: system modules and component-scoped wasm
  functions are not supported (`deploy_config` rejects wasm in
  components).

## Security model

- Sandboxed execution in wasmtime with no filesystem, no network, no env
  vars, and no inherited stdio.
- All guest-supplied lengths are bounds-checked against the guest memory
  before host allocation or copying.
- Import surface restricted to `env` + `wasi_snapshot_preview1`; modules
  with other imports are rejected at compile time.
- Memory maximum validated at compile time and enforced by the limiter;
  call data (host-side staging) is capped at 16 MiB.
- Panics in host functions surface as traps (guest errors), never unwind
  into the backend.
- The per-call `Store` reclaims all guest memory between invocations.

## Design rationale (from research)

- **ABI**: Extism's host-alloc model (guest pulls input, host stages
  output) is the only scheme proven across Rust + Go + C guests; it avoids
  guest-allocator ABI differences.
- **Async host functions**: sync host functions that block would stall
  tokio workers; `func_wrap_async` + `call_async` keeps the runtime free,
  and wasmtime's stack-switching lets the guest suspend inside awaits.
- **Timeouts**: `tokio::time::timeout` on the whole call covers guests
  parked inside awaiting host functions (fuel/epochs don't tick there).
- **Determinism**: mirror the isolate's ChaCha12 seed + virtual timestamp;
  wasmtime's NaN canonicalization and relaxed-SIMD disabling close the
  remaining nondeterminism sources.
- **wasmtime 47 pinned**: newest on crates.io at the time of writing; p1
  core-module support, async host functions, per-store fuel/limits, and
  WasmGC (`gc` is in the default feature set).
