# WASM guest best practices

Advice for authoring Convex backend functions that run as WebAssembly guests in `crates/wasm_runner`. The ABI is in [crates/wasm_runner/src/abi.rs](../crates/wasm_runner/src/abi.rs), the runnable examples live in [examples/wasm-guests/](../examples/wasm-guests/README.md), and the supported-language status matrix lives in [non-js-languages.md](non-js-languages.md).

## 1. Determinism is the contract

Backend functions can be retried and re-executed. A guest must be deterministic given the same inputs:

- **Time**: use `__convex_now_ms()` (virtual clock) — never a wall clock. `time.Now()`, `System.nanoTime()`, `chrono::Utc::now()` are forbidden.
- **Randomness**: use `__convex_random_bytes()` — seeded per-invocation (ChaCha12), so replays reproduce. Never use the language's unseeded RNG.
- **No ambient I/O**: no sockets, no filesystem, no env vars, no subprocesses. The WASI surface in the runner is restricted; `db.*` host functions are the only data access.
- **NaN / floats**: the runner canonicalizes NaNs and disables relaxed-SIMD so float math stays identical across runs. Prefer integers when you can.
- **Iteration order**: don't rely on hash-map iteration order; sort keys or use ordered structures.

## 2. Respect the memory model (host-allocated ABI)

Memory crossing the guest boundary is **host-allocated** (the Extism pattern):

- The guest never reads host memory and never exports an allocator.
- Inputs arrive through `__convex_input_length` + `__convex_input_load`; copy them into guest memory, then parse.
- Results go back through `__convex_output_set(ptr, len)` / `__convex_error_set` pointing at **guest** memory that stays alive until the call returns.
- Database results come back via `__convex_alloc`/`__convex_call_data_load`: the host allocates the call-data buffer; the guest copies out what it needs.
- Keep buffers in-bounds: the runner validates lengths, but out-of-bounds guest reads are your bug (no host OOB is possible from a guest).

## 3. Shape the module correctly

- Export exactly two functions: `__convex_run() -> i32` (0 = success) and `__convex_functions() -> i32` (JSON descriptor list via `output_set`).
- Import host functions under module name `env` only. The runner also accepts WASI p1 imports (needed by Rust std and the Go runtime); freestanding C/C++ guests import `env` only, which is the smallest and fastest shape.
- The payload is `{"function": "<name>", "type": "query"|"mutation"|"action"}`; `__convex_functions` must return `[{"name": ..., "type": ...}]` so the module analyzer can discover functions.
- Keep the module small: `opt-level=s`/`-O3`, `-nostdlib` where possible, `strip`, `lto` for Rust. Go guests carry a runtime (~3 MB); C/Rust are single-digit KB to tens of KB.

## 4. Mind the limits

- Memory cap: 256 MiB per instance. `db.query` is a full table scan with no pagination, so don't buffer whole-table results.
- Fuel: 10B units per call — CPU-bound loops in Go are ~10× more expensive than Rust/C; keep hot loops tight.
- Wall-clock timeout: 30 s default. Long actions should be chunked.
- Logs: use `__convex_log` (bounded); don't log secrets.

## 5. Write for the transaction model

- Queries must be read-only; mutations write through `db.*` host functions inside a real transaction — reads see the transaction overlay, and the whole thing commits or aborts atomically. Never try to persist state yourself.
- Errors: return a non-zero code from `__convex_run` after setting `__convex_error_set`; the runner surfaces the message. Use error codes (Convex-style) so clients can branch, not just free-form text.
- Don't call `db.*` after returning; the call-data buffer is only valid during the call.

## 6. Test like the runner does

- Start from [crates/wasm_runner/tests/end_to_end.rs](../crates/wasm_runner/tests/end_to_end.rs): it builds real guests (Rust, Go, C, C++) and runs them against a real sqlite-backed `Database`, covering reads, writes, queries, error propagation, deterministic randomness, log lines, and module analysis.
- Mirror that pattern for your own guest: build the `.wasm` in the test (skip gracefully when the toolchain is missing), execute through `run_wasm_udf`, assert on both success values and error envelopes.
- Test module analysis (`__convex_functions`) separately — a bad descriptor list fails at deploy time, not runtime.
- Add a bench (`cargo bench -p wasm_runner --bench udf_bench`) when perf matters; measure the full per-invocation path, not just guest execution.

## 7. Per-language notes

- **Rust**: use `convex_sdk` + `#[convex_functions]`; release profile `opt-level = "s"` (or `"z"` for absolute smallest), `lto = true`, `codegen-units = 1`, `strip = "symbols"` (or `"debuginfo"`), `panic = "abort"`, `debug = false`, `incremental = false` (`crates/wasm_runner/tests/fixtures/rust_guest/Cargo.toml:19` + `examples/wasm-guests/rust/Cargo.toml:19`). Measured fixture: 160 KiB with `s+lto+strip`; switching to `opt-level="z"` saves ~8-12 KiB at ~5% runtime cost; `codegen-units=1` saves ~6 KiB. Keep `crate-type = ["cdylib"]` and `wasm32-wasip1` target; no `--no-entry` needed (cdylib handles it). Avoid `std` features that pull WASI `fd_write` unless needed — they grow the WASI import surface.
- **Go**: `GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared -ldflags="-s -w" -trimpath` (`examples/wasm-guests/Makefile:19` + `crates/wasm_runner/tests/end_to_end.rs:52` + `crates/wasm_runner/benches/udf_bench.rs:82`). `-s -w` strips DWARF/symbols (~300-500 KiB saved from 3.2 MiB → ~2.7 MiB), `-trimpath` makes builds reproducible. Keep `main()` empty; the runner calls `_initialize` before dispatch (`crates/wasm_runner/src/engine.rs:560`) and registers WASI via `add_to_linker_async` (`crates/wasm_runner/src/engine.rs:548`). Prefer `int64`-safe math — wasm `i64` is fine, but JSON numbers may round through f64 in Go's decoder; use `json.Number` or struct fields for precision. `TinyGo` alternative (`tinygo build -target=wasi`) can cut to <1 MiB but requires SDK porting.
- **C / C++**: freestanding (`-nostdlib`, `-fno-exceptions`, `-fno-rtti`, `-fno-threadsafe-statics` for C++); only `env` imports; no dynamic static init in C++ (no guard vars); byte-scan JSON with a tiny scanner (as in the examples) or link a minimal JSON parser. Size tuning: `-Oz -flto -ffunction-sections -fdata-sections -fvisibility=hidden -Wl,--gc-sections -Wl,--icf=all -Wl,--strip-all` (`examples/wasm-guests/Makefile:26`, `crates/wasm_runner/tests/end_to_end.rs:493`). `-Oz` vs `-O3` saves 15-30% on the 2-3 KiB C guest; `--gc-sections` drops unused WASI glue, `--icf=all` deduplicates, `--strip-all` removes names; `-flto` cross-TU inlining. C++ guests must not include headers at all — stock LLVM clang++ has no libc++ include dir for wasm32-wasip1 without a wasi-sdk sysroot; use built-in integer types (`examples/wasm-guests/cpp/guest.cpp:25`).
- **Zig**: `zig build-exe guest.zig -target wasm32-wasip1 -mexec-model=reactor -O ReleaseSmall -fstrip --export=__convex_run --export=__convex_functions` (`crates/wasm_runner/tests/zig_guest_e2e.rs:150` + `examples/wasm-guests/Makefile:42` + `docs/zig-guest.md:19`). `ReleaseSmall` + `fstrip` yields 394 B echo / 837 B args-parsing, imports only `env` (zero WASI). Use `wasm32-wasip1` (not legacy `wasm32-wasi`); `--export=` is required on Zig 0.16 because `export fn` is not auto-exported for wasm reactor targets. Keep to `std.mem` only; `std.debug.print`/allocators needing `fd_write` would add WASI imports and 2-4 KiB.
- **Kotlin**: `wasmWasi` (`wasm32-wasip1` + WasmGC) `binaries.executable()` with no `main` → reactor via `start` section (`crates/wasm_runner/tests/fixtures/kotlin_guest/build.gradle.kts:19`). Engine enables `wasm_gc` + `wasm_function_references` explicitly (`crates/wasm_runner/src/engine.rs:117`) and caps GC heap to 64 MiB reservation + 32 MiB growth (`crates/wasm_runner/src/engine.rs:88`) — Kotlin runtime rarely exceeds tens of MB, so this saves 192 MiB of virtual reservation vs 256 MiB while keeping 256 MiB linear cap (`crates/wasm_runner/src/limits.rs:6`). Pin Kotlin 2.3.0; 2.4 removed `_initialize` in favor of `start` (runner handles both shapes). WasmGC binary is MB-scale (~2-4 MiB) — not optimizable via flags alone; rely on ProGuard/R8 and `isMinified` when available. Toolchain-gated: JDK 11+ Gradle 8.x, not in CI (`crates/wasm_runner/tests/kotlin_guest_e2e.rs:51`).
- **Dart**: blocked upstream (`docs/dart-guest.md:5`). No size tuning today; when `--standalone` stabilizes in 3.13+, expected flags are `dart compile wasm --standalone --no-source-maps` plus `wasm-opt --translate-to-exnref` to rewrite legacy EH for wasmtime 47. Engine's WasmGC proof is `cargo run -p wasm_runner --example gc_spike` (`crates/wasm_runner/examples/gc_spike.rs:1`), but validation (`crates/wasm_runner/src/validation.rs:126`) still rejects `dart2wasm.*`/`wasm:js-string` imports until upstream fixes `sdk#54394`.
- **Dart / Kotlin / Zig**: per-language details in [dart-guest.md](dart-guest.md), [kotlin-guest.md](kotlin-guest.md), and [zig-guest.md](zig-guest.md). The ABI is language agnostic — anything that imports `env` functions and exports two `i32` functions can be a guest.

## 8. Deployment checklist

1. `rustup target add wasm32-wasip1` (Rust) / Go ≥ 1.24 / LLVM clang (C/C++).
2. `make check` in `examples/wasm-guests/` to verify toolchains.
3. Build the module; confirm `__convex_functions` output with `wasm-tools print` or a quick run through `run_wasm_udf`.
4. For wasm-only deployments set `ISOLATE_EXECUTION_ENABLED=false` (or the local backend's `--disable-js-engine`) to skip loading V8 entirely; see the [deployment modes](wasm.md#deployment-modes) section of [wasm.md](wasm.md).
