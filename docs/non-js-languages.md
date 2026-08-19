# Non-JS guest languages for Convex WASM functions

Status of every candidate guest language against the Convex WASM ABI (wasm32-wasip1 + wasmtime 47.0.3; a guest exports `__convex_run` / `__convex_functions`, imports the `env` host functions, uses host-allocated memory, and is a reactor). The ABI and runner behavior live in [wasm.md](wasm.md); the policy that each language ships with a fixture, e2e test, and example is recorded in [Agent Note 2026-08-11-guest-language-fixtures-and-examples](../.agents/notes/implemented/feature/2026-08-11-guest-language-fixtures-and-examples.md). Per-language client+server testing matrix and codegen goldens: `/tmp/impl-testing-report.md`.

## Status matrix

| Language | Verdict | Module shape | Notes |
|---|---|---|---|
| **Zig** | ✅ **valid target** | reactor, 394–837 B, imports only `env` | e2e test + example in repo; Zig 0.16 needs explicit `--export=` flags; see [zig-guest.md](zig-guest.md) |
| Rust | ✅ valid target | wasm32-wasip1, imports `env` + WASI p1 | examples + e2e |
| Go | ✅ valid target | wasip1, imports `env` + WASI p1 | `_initialize` required; examples + e2e |
| C / C++ | ✅ valid targets | freestanding, `env` only | smallest modules; examples + e2e |
| Kotlin | ✅ fixture + e2e (toolchain-gated) | wasmWasi (wasm32-wasip1 + WasmGC) | needs JDK + Gradle; untested in CI; see [kotlin-guest.md](kotlin-guest.md) and [Agent Note 2026-08-11-kotlin-guest-toolchain-gated-e2e](../.agents/notes/implemented/feature/2026-08-11-kotlin-guest-toolchain-gated-e2e.md) |
| Dart | 🚧 partially (upstream gates) | `--standalone` in 3.13 beta; legacy EH blocks wasmtime | wasm-opt `--translate-to-exnref` workaround; see [dart-guest.md](dart-guest.md) and [dart-feasibility-2026.md](dart-feasibility-2026.md) |
| **Javy (JS-in-wasm)** | ⚠️ ABI-blocked | WASI p1 + `_start`, custom exports kebab-case only | WIT forbids `_` in export names; return values dropped; needs custom plugin |
| **Python (CPython WASI)** | 🚧 not guest-ready | command-only: `memory` + `_start` only | 29 MB; custom guest = rebuild CPython with wasi-sdk (heavy) |
| AssemblyScript | not evaluated in depth | — | — |
| C#/.NET, Java/TeaVM, Ruby, PHP, Swift | not evaluated in depth | — | — |
| **Cross-language clients** | design adopted — validator JSON IR, per-target emitters for TS/Kotlin/Rust/C#/Dart; mixed backend merges into one `ApiSurface` | `ApiSurface` = `Vec<AnalyzedFunction>` by `CanonicalizedModulePath` | See [polyglot backend note](../.agents/notes/implemented/architecture/2026-08-19-polyglot-backend-and-client-generation.md) |
| **Internal tracking transport** | WS primary, SSE optional fallback; tracking is `ReadSet`→`IntervalMap`→`Transition` | `Token`/`IntervalMap` + `ServerMessage::Transition` (`TransitionChunk` at 5 MB) | See [internal tracking note](../.agents/notes/implemented/architecture/2026-08-19-internal-tracking-and-event-transport.md) |

## Verified findings

### Zig

A 394-byte reactor module imports only `env` (`__convex_input_length`, `__convex_input_load`, `__convex_output_set`, `__convex_error_set`) and exports `memory`, `__convex_functions`, `_initialize`, `__convex_run`. Build: `zig build-exe guest.zig -target wasm32-wasi -mexec-model=reactor -O ReleaseSmall -fstrip --export=__convex_run --export=__convex_functions --name guest`. Zig 0.16 does not auto-export `export fn` symbols for wasm targets via `build-exe`; the `--export=` flags are required. Verified by `cargo test -p wasm_runner --test zig_guest_e2e` against a real sqlite-backed Transaction.

### Javy (QuickJS-in-wasm, Shopify / Bytecode Alliance)

- A static build produces a ~1.3 MB module importing only `wasi_snapshot_preview1` and exporting `_start`, WIT-named functions, `cabi_realloc`, and `config-schema`; there is no `_initialize`.
- WIT identifiers cannot contain underscores (`export __convex_run` → `Error: invalid character in identifier '_'`, even with `%` escapes), so Javy cannot produce the ABI's export names.
- Exported function return values are dropped (WIT exports "with no return values" only), so the `() -> i32` ABI status cannot be expressed.
- The JS side cannot call the `env` imports without a custom Javy plugin; dynamic mode needs a host-provided QuickJS provider module.
- Running Javy requires engine-side export-name aliasing plus a custom plugin exposing the `env` host functions.

### Python (CPython WASI)

- The prebuilt `python-3.13.15-wasi_sdk-24.zip` (13 MB, brettcannon/cpython-wasi-build) yields `python.wasm` at 29.3 MB, exporting only `memory` and `_start` (a pure command module) and importing 42 `wasi_snapshot_preview1` functions. It runs on the wasmtime 47 CLI (`print(42)` → 42, ~0.2 s warm).
- There is no `_initialize` and no custom exports; a Convex guest requires patching CPython to embed the interpreter in a reactor module with custom exports, which needs a wasi-sdk rebuild (wasi-sdk-33 is a 173 MB download plus a heavy build). The build-tree zip contains `libpython3.13.a` (42 MB), so linking a C shim is possible in principle.
