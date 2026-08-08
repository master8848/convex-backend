# WASM backend functions: implementation & limitations

This document describes the WebAssembly execution path for Convex backend
functions, how it relates to the existing TypeScript (isolate) and Node
paths, and its current limitations.

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
| Rust | **Working** | `wasm32-wasip1`, `cargo build` | `#[convex_functions]` + `#[query]`/`#[mutation]`/`#[action]` macros |
| Go | **Working** | native Go `GOOS=wasip1 GOARCH=wasm -buildmode=c-shared` (Go ≥ 1.24) | `//go:wasmexport` + `//go:wasmimport`; `_initialize` called by the runner |
| Kotlin | **Not viable** | — | Kotlin/Native dropped its wasm target; Kotlin/Wasm is beta, command-only, no export ABI, depends on wasm GC which wasmtime does not fully support |

Go note: the runner calls `_initialize` before dispatch (required by the Go
runtime), and registers WASI via `add_to_linker_async` so Go's runtime init
(`fd_fdstat_get`, `poll_oneoff`, ...) doesn't hit wasmtime-wasi's blocking
`in_tokio` path inside the async embedding.

### Verification

- `crates/wasm_runner/tests/end_to_end.rs` builds real Rust and Go guests
  and runs them against a real sqlite-backed `Database`: reads, writes,
  table queries, error propagation, deterministic randomness, log lines,
  and module analysis.
- `cargo bench -p wasm_runner --bench udf_bench` measures the full
  per-invocation path (module lookup, instantiate, host functions,
  execution, result parse) for a `SELECT`-like `echo` function.

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
gap (see "Future work").

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
- **wasmtime 47 pinned**: latest with p1 core-module support, async
  host functions, and per-store fuel/limits.
