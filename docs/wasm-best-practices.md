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

- **Rust**: use `convex_sdk` + `#[convex_functions]`; `panic = "abort"`, `opt-level = "s"`, `lto = true`, `strip = "symbols"` in the release profile.
- **Go**: `GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared`; keep `main()` empty; the runner calls `_initialize` for the Go runtime. Prefer `int64`-safe math — wasm `i64` is fine, but JSON numbers may round through f64 in Go's decoder; use `json.Number` or struct fields for precision.
- **C / C++**: freestanding (`-nostdlib`, `-fno-exceptions`, `-fno-rtti` for C++); only `env` imports; no dynamic static init in C++ (no guard vars); byte-scan JSON with a tiny scanner (as in the examples) or link a minimal JSON parser. C++ guests must not include headers at all — stock LLVM clang++ has no libc++ include dir for wasm32-wasip1 without a wasi-sdk sysroot; use built-in integer types.
- **Dart / Kotlin / Zig**: per-language details in [dart-guest.md](dart-guest.md), [kotlin-guest.md](kotlin-guest.md), and [zig-guest.md](zig-guest.md). The ABI is language agnostic — anything that imports `env` functions and exports two `i32` functions can be a guest.

## 8. Deployment checklist

1. `rustup target add wasm32-wasip1` (Rust) / Go ≥ 1.24 / LLVM clang (C/C++).
2. `make check` in `examples/wasm-guests/` to verify toolchains.
3. Build the module; confirm `__convex_functions` output with `wasm-tools print` or a quick run through `run_wasm_udf`.
4. For wasm-only deployments set `ISOLATE_EXECUTION_ENABLED=false` (or the local backend's `--disable-js-engine`) to skip loading V8 entirely; see the [deployment modes](wasm.md#deployment-modes) section of [wasm.md](wasm.md).
