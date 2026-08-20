# WASM backend functions: implementation & limitations

Reference for the WebAssembly execution path for Convex backend functions: the engine in `crates/wasm_runner`, the guest ABI, supported languages, limits, and the security model. Engine and deployment-mode decisions are recorded in the [wasm execution engine note](../.agents/notes/implemented/architecture/2026-08-08-wasm-execution-engine.md) and the [wasm-only deployment note](../.agents/notes/implemented/architecture/2026-08-09-optional-js-engine-wasm-only.md); the full candidate-language status matrix lives in [non-js-languages.md](non-js-languages.md).

## Deployment modes

The function runner supports two JS-engine modes, selected by `ISOLATE_EXECUTION_ENABLED` (default `true`) or the local backend's `--disable-js-engine` flag.

- **Mixed (default)**: V8 + ICU + the UDF runtime snapshot initialize eagerly at startup; wasm and TypeScript functions both run.
- **Wasm-only**: V8 is never initialized — no ICU data load, no UDF snapshot, no V8 platform threads, no worker isolates. This saves hundreds of MB of process RAM for deployments that run only wasm functions. Any request that needs the JS engine (TypeScript functions, module analysis, HTTP actions, schema/auth-config evaluation) fails with a `JavaScriptExecutionDisabled` error instead of loading V8 on demand.

Fuzz-related V8 flags passed via `ISOLATE_V8_FLAGS` (`--jit-fuzzing`, `--experimental-fuzzing`, `--randomize-hashes`) are dropped because they break UDF determinism; `V8_ALLOW_FUZZING_FLAGS=true` keeps them for local runtime fuzzing.

## Composing polyglot functions in one deployment

One deployment mixes `convex/messages.ts` (TypeScript), `convex/ingest.rs` (Rust), `convex/analytics.kt` (Kotlin) in one `convex/` dir. The deploy merges every `convex/*` module by `CanonicalizedModulePath` into one `ApiSurface`; clients use one `api`/`fullApi` regardless of backend language.

Directory layout:

```
convex/
  messages.ts   // TypeScript query
  ingest.rs     // Rust guest
  analytics.kt  // Kotlin guest
convex/_generated/api.ts  // txt generated — do not edit
```

Build — TypeScript is bundled; wasm guests compile before deploy:

- Rust: `cargo build --target wasm32-wasip1`
- Go: `GOOS=wasip1 GOARCH=wasm go build`
- Zig: `zig build-exe -target wasm32-wasi -mexec-model=reactor`
- C/C++: `clang --target=wasm32-wasip1 -nostdlib`
- Kotlin: `gradle wasmWasi`

All become `wasm` modules in the same package; no per-language deployment.

Runtime — `anyApi` is a Proxy that builds `FunctionReference` strings on property access (`npm-packages/convex/src/server/api.ts:431` `createApi`); the generated `api` is typed via `ApiFromModules` (`npm-packages/convex/src/server/api.ts:255`) mapping module paths to functions. Adding `convex/search.rs` makes `api.search.*` appear alongside `api.messages.*` in `fullApi`.

Example client call:

```
import { api } from "./convex/_generated/api";
await client.query(api.messages.list, {});
await client.query(api.search.query, { q: "hello" });
```

Language status and toolchains are in the Language support table below; full candidate matrix lives in [non-js-languages.md](non-js-languages.md).

Invariant: one `convex/` dir, one deployment, one `api`; extension selects toolchain.

## Runtime architecture

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
                    wall-clock timeout (30 s default), GC heap 64 MiB + 32 MiB growth (crates/wasm_runner/src/engine.rs:92)
                  - WASI p1 via add_to_linker_async (Rust std, Go runtime)
                  - module cache keyed by sha256: in-memory LRU (128) + AOT `Module::serialize`/`deserialize` (crates/wasm_runner/src/engine.rs:169)
                  - per-env execution semaphore 64 (crates/wasm_runner/src/engine.rs:85, crates/function_runner/src/server.rs:423)
                                     │
                                     ▼
                  Transaction (real DB reads/writes, commit or abort)
```

## The ABI

Guests export two functions:

- `__convex_run() -> i32` — the dispatcher. The input payload (`{"function": "...", "args": [...]}`) is pulled via host functions.
- `__convex_functions() -> i32` — a JSON list of `{"name": "...", "type": "query"|"mutation"|"action"|"httpAction"}` descriptors, used by the module analyzer.

Memory crossing the boundary is host-allocated (the Extism pattern), so any language that can import functions and export one `i32` function can implement the guest side. The host function set is in [crates/wasm_runner/src/abi.rs](../crates/wasm_runner/src/abi.rs).

## Language support

The detailed table below covers the supported targets; the full status matrix (including not-yet-supported candidates such as Javy and Python) lives in [non-js-languages.md](non-js-languages.md).

| Language | Status | Toolchain | Notes |
|---|---|---|---|
| Rust | ✅ **valid target** | `wasm32-wasip1`, `cargo build` | `#[convex_functions]` + `#[query]`/`#[mutation]`/`#[action]` macros; example: `examples/wasm-guests/rust` |
| Go | ✅ **valid target** | native Go `GOOS=wasip1 GOARCH=wasm -buildmode=c-shared` (Go ≥ 1.24) | `//go:wasmexport` + `//go:wasmimport`; `_initialize` called by the runner; example: `examples/wasm-guests/go` |
| C | ✅ **valid target** | stock LLVM clang `--target=wasm32-wasip1 -nostdlib` | Freestanding guest (no libc, no WASI): smallest/fastest. Fixture `tests/fixtures/c_guest/guest.c` + end-to-end test; example: `examples/wasm-guests/c` |
| C++ | ✅ **valid target** | stock LLVM clang++ `--target=wasm32-wasip1 -nostdlib -fno-exceptions -fno-rtti` | Same ABI as C; freestanding rules in `examples/wasm-guests/cpp` |
| Dart / Flutter | ❌ **blocked upstream** | `dart compile wasm` → WasmGC | wasmtime 47 runs WasmGC modules under the runner's exact Config (`cargo run -p wasm_runner --example gc_spike`), but dart2wasm 3.12 emits legacy exception-handling instructions that wasmtime 47 rejects, and every module imports a JS host (`dart2wasm.*` helpers, `wasm:js-string` builtins, string-constant globals) with no standalone target in stable SDKs. Flutter mobile stays on Dart AOT native. See [dart-guest.md](dart-guest.md) |
| Kotlin | ✅ fixture + e2e test (build gated on toolchain) | Kotlin Multiplatform `wasmWasi` (wasm32-wasip1 + WasmGC) | `@WasmExport` + `@WasmImport("env", ...)` give the exact ABI; reactor module (no `main`) self-initializes via the Wasm start section; imports only `wasi_snapshot_preview1` + `env`; needs JDK + Gradle to build (untested in CI). See [kotlin-guest.md](kotlin-guest.md) |
| Zig | ✅ **valid target** | Zig 0.16+ `build-exe -target wasm32-wasi -mexec-model=reactor` | Smallest possible guest: freestanding reactor module, **zero WASI imports** (only the 4 `env` host functions), 394 B echo / 837 B args-parsing; Zig 0.16 needs explicit `--export=__convex_run --export=__convex_functions`. Fixture `tests/fixtures/zig_guest` + e2e test; example `examples/wasm-guests/zig`. See [zig-guest.md](zig-guest.md) |

Go note: runner calls `_initialize` and registers WASI via `add_to_linker_async` to avoid the blocking `in_tokio` path.

C note: only `env` + WASI imports are allowed; freestanding C imports only `env` — smallest/fastest, single-digit-µs like Rust.

## Examples, scaffolding & best practices

- **Examples**: [examples/wasm-guests/](../examples/wasm-guests/README.md) has a ready-to-build standalone guest per language — `rust/`, `go/`, `c/`, `cpp/`, `zig/` (valid targets) plus `dart/` and `kotlin/` status stubs.
- **One-command build**: `make` in `examples/wasm-guests/` builds every supported example; `make check` verifies toolchains and explains how to install what is missing.
- **Scaffold a new guest**: `examples/wasm-guests/scaffold.sh <lang> <name>` copies a pre-wired template (ABI imports/exports included).
- **Best practices**: [wasm-best-practices.md](wasm-best-practices.md) — determinism, the host-allocated memory model, module shape, limits, the transaction model, testing, per-language notes, and a deployment checklist.

Per-language fixture + e2e + example requirement: [fixtures note](../.agents/notes/implemented/feature/2026-08-11-guest-language-fixtures-and-examples.md); packaging and C++ freestanding rule: [examples note](../.agents/notes/implemented/feature/2026-08-11-wasm-examples-and-best-practices.md).

## Verification

- [crates/wasm_runner/tests/end_to_end.rs](../crates/wasm_runner/tests/end_to_end.rs) builds real Rust, Go, C, and C++ guests and runs them against a real sqlite-backed `Database`: reads, writes, table queries, error propagation, deterministic randomness, log lines, and module analysis. Toolchain-missing guests are skipped gracefully.
- `crates/wasm_runner/tests/zig_guest_e2e.rs` and `crates/wasm_runner/tests/kotlin_guest_e2e.rs` cover the Zig and Kotlin guests (Kotlin is toolchain-gated: JDK + Gradle, not run in CI).
- `cargo bench -p wasm_runner --bench udf_bench` measures the full per-invocation path (module lookup, instantiate, host functions, execution, result parse) for a `SELECT`-like `echo` function.
- `cargo run -p wasm_runner --example gc_spike` proves the engine's WasmGC support (struct/array/i31/ref.eq) under the runner's exact Config — the engine-side prerequisite for a Dart guest.

## Benchmark results (Apple Silicon)

```
native Rust (echo)            ~0 µs/call
Rust WASM: echo (warm)      ~180 µs/call
Go WASM: echo (warm)      ~2,160 µs/call   (Go guest 3.2 MB)
```

Rust WASM number includes transaction, instantiation, host functions, execution, and teardown — wasm execution itself is single-digit µs. Go is ~12× slower due to per-call `_initialize` + GC setup; freestanding C has no init and matches Rust.

Containerized idle/load RAM and `rps/p50/p95` under pinned `--cpus=2 --memory=2g` (cgroup, device-irrelevant, `docker stats` + `perf/load.js`): see [wasm containerized note](../perf/2026-08-20-wasm-containerized-memory-cpu.md).

## Limitations

### Functional

- **No filters/indexes/pagination in `db.query`**: `db.query(table)` is a full table scan returning all documents. `queryPage` (journal-based pagination) is not implemented; query caching therefore cannot produce incremental journals.
- **No nested UDF calls**: `ctx.runQuery`/`runMutation`/`runAction` are not supported in wasm functions.
- **No HTTP actions**: `#[http_action]` registers a descriptor but the runner rejects http action invocations for wasm modules.
- **No storage/search/vector/cron/scheduling host functions**: only the database operations listed above.
- **No `getUserIdentity`**: wasm functions see an anonymous identity. `db.system.*` is unavailable (system tables are rejected).
- **Action DB writes are not committed**: actions execute against the transaction but their writes are discarded (matching the isolate path's action semantics for the primary write set).
- **Numbers are Float64**: plain `i64` results round-trip as floats, matching TypeScript `number` semantics. Exact int64/bytes require the `ConvexValue` type with tagged encodings (guests construct `{"$integer": ...}` / `{"$bytes": ...}` themselves; no helper exists).

### Determinism

- WASI clocks and `random_get` are virtualized to the transaction timestamp / rng seed, so Rust std (`SystemTime`, `getrandom`) and Go's runtime (`math/rand`, `crypto/rand`) are reproducible per retry.
- Fuel-based interruption is deterministic; epoch interruption is not used.
- Go's spec-randomized map iteration is inherently nondeterministic — documented for Go SDK authors.

### Timeouts & limits

- The wall-clock timeout (`WasmLimits::timeout`, default 30 s) is the authoritative limit and covers time spent inside host functions. Fuel (10B units) bounds CPU. Memory is capped at 256 MiB via both module validation and the wasmtime limiter.
- The isolate path's user/system time distinction (time paused during syscalls) is not replicated: the 30 s budget includes DB time.

### Integration gaps

- **Deploy pipeline**: nothing writes a `.wasm` binary into a source package. The runtime seam expects the module's `source` to be base64 wasm; the bundler/CLI work to produce that is not done. JS bundling (`npm-packages/convex/src/bundler`) does not handle `.rs`/`.go` entry points.
- **Analyze**: `FunctionRunnerCore::analyze` requires isolate modules; wasm modules need an analyze path that runs `__convex_functions` (the `analyze_functions` helper exists and is tested).
- **Query cache**: no `QueryJournal` is produced, so wasm queries bypass incremental caching benefits (they still get full-function caching via `observed_time`/`observed_rng` flags).
- **Compiled-module persistence**: `Module::serialize`/`deserialize` is wired as an in-process AOT cache keyed by sha256 (`crates/wasm_runner/src/engine.rs:107` `serialized_cache`, `cache_serialized`/`get_serialized`); modules are deserialized instead of recompiled when the bytes match the current engine `Config` (wasmtime 47, `crates/wasm_runner/src/engine.rs:169`). Cross-restart persistence to disk is caller-owned (the cache is warmable via `cache_serialized`).
- **Concurrency limits**: wasm executions are bounded by a per-environment 64-permit semaphore (`crates/wasm_runner/src/engine.rs:85` `MAX_CONCURRENT_EXECUTIONS_PER_ENV`, `crates/wasm_runner/src/engine.rs:152` `execution_semaphore_for_env`) mirroring the isolate path's per-env limiting in `crates/isolate/src/concurrency_limiter.rs:109`; the limit is acquired in `crates/function_runner/src/server.rs:423`.
- **Log streaming for actions**: `log_line_sender` is wired, but action progress log streaming through `log_action_progress` is untested for wasm.
- **Go performance**: a per-module `Store` pool would avoid re-running the Go runtime `_initialize` per call.
- **System UDFs / components**: system modules and component-scoped wasm functions are not supported (`deploy_config` rejects wasm in components).

## Security model

- Sandboxed execution in wasmtime with no filesystem, no network, no env vars, and no inherited stdio.
- All guest-supplied lengths are bounds-checked against the guest memory before host allocation or copying.
- Import surface restricted to `env` + `wasi_snapshot_preview1`; modules with other imports are rejected at compile time.
- Memory maximum validated at compile time and enforced by the limiter; call data (host-side staging) is capped at 16 MiB.
- Panics in host functions surface as traps (guest errors), never unwind into the backend.
- The per-call `Store` reclaims all guest memory between invocations.
