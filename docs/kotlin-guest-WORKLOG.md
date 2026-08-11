# Worklog: Kotlin guest (wasm_runner)

Task: implement the Kotlin guest for the WASM backend-function engine:
research the export-ABI path, create the fixture + e2e test, verify
`cargo check`, document. Sibling subagents own `examples/`,
`docs/wasm*.md`, `tests/end_to_end.rs` — untouched.

## 2026-07-XX (session)

### Research

1. **`@WasmImport` / `@WasmExport`** exist on BOTH Kotlin wasm targets
   (wasmJs = WasmGC+JS, wasmWasi = WASI p1 + WasmGC), since Kotlin 1.8, raw
   types only ("without type adapters"). API docs:
   - https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-import/
   - https://kotlinlang.org/api/core/kotlin-stdlib/kotlin.wasm/-wasm-export/
2. **Kotlin/Wasm emits WasmGC since 1.9.20**; `wasmWasi` uses the NEW
   exception-handling proposal (exnref) by default:
   https://kotlinlang.org/docs/wasm-configuration.html
3. **wasmtime 47.0.3** (vendored source, `src/config.rs`): defaults
   `wasm_function_references`=true, `wasm_gc`=true, `wasm_exceptions`(new
   EH)=true; new-EH landed Aug 2025 (bytecodealliance/wasmtime#11326). The
   runner's exact Config needs no changes. (No wasmtime bump needed.)
4. **Official wasmWasi template** (Kotlin 2.3.0) proves `@WasmImport("wasi_snapshot_preview1",
   "clock_time_get")` + `@WasmExport` on the WASI target:
   https://github.com/Kotlin/kotlin-wasm-wasi-template
5. **Real-world wasmtime POC (July 2026, Kotlin 2.4.20-Beta2 + wasmtime-py
   47)** — the single most valuable find; it uses EXACTLY our host-alloc
   callback ABI (`read_input(ptr,cap)` / `write_output(ptr,len)` via
   `withScopedMemoryAllocator`), and documents the module shape:
   - no `main` → reactor: exports `memory` + `@WasmExport`s only; Wasm start
     section initializes at instantiation; no `_initialize`/`_start`;
   - imports = `wasi_snapshot_preview1` + declared `@WasmImport`s only;
   - required proposals = function-references, gc, exceptions (exnref);
     wasmtime 46+ known good.
   https://github.com/glandais/vcyclist/blob/develop/docs/kotlin-wasm-wasi.md
   https://github.com/glandais/vcyclist/blob/develop/docs/wasm-wasi-abi.md
   https://github.com/glandais/vcyclist/blob/develop/tools/wasi/host.py

### Decision

**Option A: Kotlin Multiplatform `wasmWasi` target.** `wasmJs` (Option B,
WasmGC + JS interop) has the same annotations but its runtime imports JS
functions (console etc.) under module `env` and is glued to a JS host;
Kotlin slack threads report wasmtime unusable for it. `wasmWasi` imports only
`wasi_snapshot_preview1` (runner already registers WASI p1) + our `env`
imports, matching `validate_module`'s allowlist exactly.

### Implementation

- `crates/wasm_runner/tests/fixtures/kotlin_guest/` — settings.gradle.kts,
  build.gradle.kts (kotlin multiplatform 2.3.0, `wasmWasi {
  binaries.executable() }`), `src/wasmWasiMain/kotlin/Guest.kt` (echo-style
  ABI: `@WasmImport("env", ...)` externs, `@WasmExport("__convex_run")` /
  `@WasmExport("__convex_functions")`, scoped-arena linear memory helpers),
  README.md (exact build commands), .gitignore. NOTE: KMP source-set layout
  is `src/wasmWasiMain/kotlin/`, not `src/main/kotlin` (the latter would not
  compile).
- `crates/wasm_runner/tests/kotlin_guest_e2e.rs` — new test, helpers copied
  from end_to_end.rs (new_database/load_database/create_table/run_function),
  builds fixture with `gradle build` (probe: gradle must exist AND run),
  globs the produced .wasm, runs echo / unknown-fn / analyze_functions.
  Skips gracefully when the Kotlin toolchain is missing.
- Local env has NO kotlinc/gradle/JDK (and only 17Gi disk) → fixture is
  complete ready-to-run source, **untested locally — needs Kotlin toolchain**
  (as instructed).

### Verification (no Kotlin toolchain needed)

- `cargo check -p wasm_runner --tests` — PASSES (15s, incremental; never
  built the whole workspace).
- Temporary proof test (deleted after use, incl. the temp `wat` dev-dep,
  Cargo.toml reverted to pristine): hand-written **WasmGC WAT module shaped
  like Kotlin wasmWasi output** — GC struct/global, start section allocating
  a GC struct at instantiation, `env` imports, `memory` export, `() -> i32`
  exports — run through the FULL runner path:
  - validate_module accepts (env + wasi imports only, ()->i32 exports);
  - instantiate_async + start section + echo against sqlite Database OK;
  - unknown function → JsError (no panic); analyze_functions OK.
  Debug detour: initial WAT bug (echo branch checked the wrong needle) and
  out-of-bounds output buffer were found via instrumented error bytes; final
  proof PASSED.
- `cargo test -p wasm_runner --test kotlin_guest_e2e` — PASSES with skip
  message ("Kotlin toolchain (gradle + JDK) not found").

### For docs/wasm.md (sibling owns the file — do NOT edit)

Current row: "🚧 in progress — wasmWasi (wasm32-wasip1) target under research
— export-ABI story (@WasmExport/@JsExport) and toolchain being validated;
Kotlin/Wasm (WasmGC) may also become viable given wasmtime 47 GC support".

SHOULD become (per docs/kotlin-guest.md): ✅ fixture + e2e test (build gated
on toolchain); target `wasmWasi` (wasm32-wasip1 + WasmGC); `@WasmExport` +
`@WasmImport("env", ...)` give the exact ABI; reactor module (no main)
self-initializes via Wasm start section; imports only wasi_snapshot_preview1
+ env; build needs JDK + Gradle (untested in CI); see docs/kotlin-guest.md.

### Files touched

- NEW crates/wasm_runner/tests/fixtures/kotlin_guest/{settings.gradle.kts,
  build.gradle.kts, src/wasmWasiMain/kotlin/Guest.kt, README.md, .gitignore}
- NEW crates/wasm_runner/tests/kotlin_guest_e2e.rs
- NEW docs/kotlin-guest.md, docs/kotlin-guest-WORKLOG.md
- (temporarily) crates/wasm_runner/tests/gc_guest_proof.rs + a `wat`
  dev-dep — both removed; `git diff` for Cargo.toml is empty.

### Untouched (constraints)

crates/common, crates/model, root Cargo.toml, tests/end_to_end.rs,
examples/, docs/wasm*.md, docs/wasm-best-practices.md. No cargo clean;
workspace never built wholesale.
