# Non-JS guest languages for Convex WASM functions

Status of every candidate guest language against the Convex WASM ABI
(wasm32-wasip1 + wasmtime 47.0.3; guest exports `__convex_run` /
`__convex_functions`, imports `env` host functions, host-allocated memory,
reactor shape). Verified 2026-08-11 with real toolchains where noted.

## Summary

| Language | Verdict | Module shape | Notes |
|---|---|---|---|
| **Zig** | ✅ **valid target — landed** | reactor, 394–837 B, imports only `env` | e2e test + example in repo; Zig 0.16 needs explicit `--export=` flags |
| Rust | ✅ valid target | wasm32-wasip1, imports `env` + WASI p1 | examples + e2e (151c59cef) |
| Go | ✅ valid target | wasip1, imports `env` + WASI p1 | `_initialize` required; examples + e2e |
| C / C++ | ✅ valid targets | freestanding, `env` only | smallest modules; examples + e2e |
| Kotlin | ✅ fixture + e2e (toolchain-gated) | wasmWasi (wasm32-wasip1 + WasmGC) | needs JDK+Gradle; untested in CI (3fa8275a4) |
| Dart | 🚧 partially (upstream gates) | `--standalone` in 3.13 beta; legacy EH blocks wasmtime | wasm-opt `--translate-to-exnref` workaround; clean ~Nov 2026 |
| **Javy (JS-in-wasm)** | ⚠️ ABI-blocked | WASI p1 + `_start`, custom exports kebab-case only | WIT forbids `_` in export names; return values dropped; needs custom plugin |
| **Python (CPython WASI)** | 🚧 not guest-ready | command-only: `memory` + `_start` only | 29 MB; custom guest = rebuild CPython with wasi-sdk (heavy) |
| AssemblyScript | not evaluated in depth | — | — |
| C#/.NET, Java/TeaVM, Ruby, PHP, Swift | not evaluated in depth | — | — |

## Verified findings

### Zig — PROVEN end-to-end (wasmtime-py 47 + the real Rust runner)

- 394-byte reactor module: imports only `env`
  (`__convex_input_length/load/output_set/error_set`), exports
  `memory`/`__convex_functions`/`_initialize`/`__convex_run`.
- Echo + error path passed on wasmtime 47.0.1 via a Python host shim, then
  the full `cargo test -p wasm_runner --test zig_guest_e2e` passed against a
  real sqlite-backed Transaction.
- Build: `zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor
  -O ReleaseSmall -fstrip --export=__convex_run --export=__convex_functions
  --name guest`. Key gotcha: Zig 0.16 does not auto-export `export fn`
  symbols for wasm targets via `build-exe`.

### Javy (QuickJS-in-wasm, Shopify/Bytecode Alliance) — ABI-BLOCKED without engine changes

- Static build: ~1.3 MB module, imports ONLY `wasi_snapshot_preview1`,
  exports `_start` + WIT-named functions + `cabi_realloc` + `config-schema`.
  No `_initialize`.
- **Definitive blocker**: WIT identifiers cannot contain underscores
  (`export __convex_run` → `Error: invalid character in identifier '_'`, even
  with `%` escapes), so Javy cannot produce our exact export names.
- Exported function return values are dropped (WIT export "with no return
  values" only), so the `() -> i32` ABI status can't be expressed.
- JS side cannot call `env` imports without a custom Javy plugin; dynamic
  mode needs a host-provided QuickJS provider module.
- Would need: engine-side export-name aliasing + a custom plugin exposing the
  `env` host functions — not worth it vs. supporting Zig/etc. directly.
- Still interesting as a "users write JS, runs in wasm without our V8
  isolate" story if the ABI gets an alias layer.

### Python (CPython WASI) — not guest-ready from stock builds

- Prebuilt `python-3.13.15-wasi_sdk-24.zip` (13 MB, brettcannon/cpython-wasi-build):
  `python.wasm` is 29.3 MB, exports ONLY `memory` and `_start` (pure command
  module), imports 42 `wasi_snapshot_preview1` functions. Runs on the wasmtime
  47 CLI (`print(42)` → 42, ~0.2 s warm).
- No `_initialize`, no custom exports — a Convex guest would require patching
  CPython (embedding the interpreter in a reactor module with custom exports),
  which needs a wasi-sdk rebuild (wasi-sdk-33 is a 173 MB download + heavy
  build). The build-tree zip does contain `libpython3.13.a` (42 MB) so linking
  a C shim is possible in principle.
- Verdict: document-only; revisit if/when CPython ships a reactor/embedding
  WASI target.

## Recommendation

**Land Zig next** (done: fixture, e2e, example). After that, the highest-value
follow-ups are:
1. Dart once the 3.13/3.14 standalone + new-EH story lands (see
   docs/dart-feasibility-2026.md) — users keep asking for it.
2. Kotlin build verification on a machine with JDK+Gradle.
3. Javy only if the engine adds export aliasing (larger feature).
