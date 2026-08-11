# Dart guest support — worklog

Task: verify the docs/wasm.md claim about Dart/WasmGC, prototype the wasmtime
prerequisite, and produce a Dart guest guide. Started on the repo's `main`.

## Milestone 1 — verify the claims

- Repo pins `wasmtime = 47.0.3` (workspace Cargo.toml line 269) with features
  `async`, `anyhow`; `wasmtime-wasi = 47.0.3` with `p1` (line 270). Cargo.lock
  confirms wasmtime 47.0.3 and wasmtime-wasi 47.0.3.
- `cargo search wasmtime` → **47.0.3 is the newest version on crates.io**; there
  is nothing to upgrade to. `cargo info wasmtime` shows 47.0.3's `default`
  feature set includes `gc`, `gc-copying`, `gc-drc`, `gc-null`.
- `cargo tree -p wasm_runner -e features` confirms `wasmtime feature "gc"` is
  enabled in the current workspace build (via default features).
- wasmtime 47 `Config` has `wasm_gc(bool)` — "true by default" — plus a GC
  collector selector and `wasm_exceptions` (new EH encoding) "true by default".
- **The docs/wasm.md claim "wasmtime 47 does not support WasmGC" is WRONG.**

## Milestone 2 — prototype the engine prerequisite

- Wrote `crates/wasm_runner/examples/gc_spike.rs`: compiles a WasmGC module
  (`struct.new`/`struct.get`, `array.new_default`/`array.set`/`array.get`,
  `ref.i31`/`i31.get_s`, `ref.eq`) under wasm_runner's exact Config (NaN
  canonicalization, relaxed-SIMD off, fuel on) and runs it.
  - `cargo run -p wasm_runner --example gc_spike` → **GC SPIKE OK** (sum()=72,
    i31 roundtrips, distinct structs not ref-eq, `wasm_gc(false)` rejects GC).
  - Note: `Store::add_fuel` no longer exists in 47; wasm_runner uses
    `store.set_fuel(...)` + `fuel_async_yield_interval(...)` (engine.rs).
- Conclusion: the engine-side prerequisite is ALREADY MET (47.0.3 > 27, gc on
  by default). No Cargo.toml bump, no migration checklist needed.

## Milestone 2b — is a real Dart module runnable? (ground truth)

Downloaded Dart SDK 3.12.2 stable (2026-06-09) from
storage.googleapis.com to /tmp/dart (not committed).

- `dart compile wasm main.dart` → `guest.wasm` + `guest.mjs` (JS glue) +
  `guest.support.js`. No `--standalone` flag exists in `dart compile wasm
  --help`; SDK ships `dart2wasm_platform.dill` (JS) and
  `dart2wasm_js_compatibility_platform.dill` only — no
  `dart2wasm_standalone_platform.dill`.
- wasmtime 47 compiles the module? **No**:
  `legacy_exceptions feature required for try instruction` (dart2wasm emits the
  OLD exception-handling encoding). Enabling `Config::wasm_legacy_exceptions`
  fails at Engine::new: `the wasm_legacy_exceptions feature is not supported on
  this compiler configuration` (wasmparser 0.252 features.rs: LEGACY_EXCEPTIONS
  ∉ features_known_to_wasmtime = WASM3 ∪ ..., so it lands in
  `compiler_panicking_wasm_features`, wasmtime src/config.rs).
- Import surface of a trivial module (parsed the wasm binary): 249 imports —
  239 globals from module `''` (string constants, served by a Proxy in the JS
  glue), 8 funcs from `dart2wasm` (`_315`, `_316`, `_30` print, `_185`,
  `_178` Error().stack, `_179`, `_212`, `_221`), `wasm:js-string.concat`
  (string builtins), `WebAssembly.JSTag`. Exports: `$invokeMain`,
  `$wasmI16ArrayGet/Set`, `$setThisModule`.
- Even Convex-style externals (`@JS('__convex_input_length') external int ...`)
  compile to `dart2wasm._NNN` helpers that dispatch through
  `globalThis.__convex_input_length()` in the JS glue — NOT direct
  `env.__convex_*` imports. The whole runtime is JS-host-mediated.
- Upstream: dart-lang/sdk#53884 (non-JS runtimes; askeksa's JS-dependency list;
  simolus3 May-2026 status), #54394 (dart2wasm must move to try_table/throw_ref/
  exnref; open, updated 2026-08-10), #63166 (split dart:_wasm from
  dart:js_interop; closed 2026-07-29).

## Verdict

Dart guests are NOT viable today, but not because of WasmGC: the engine already
has it. Blockers: (1) legacy EH instructions in dart2wasm output vs wasmtime 47
(new try_table only); (2) mandatory JS host (dart2wasm.* helpers, wasm:js-string,
string globals, no standalone mode in released SDKs). Watch #54394 + a shipped
`--standalone`; then the remaining work is a Rust host shim + validation
allowlist changes in wasm_runner.

## Files touched

- crates/wasm_runner/examples/gc_spike.rs (new, kept — GC capability proof)
- crates/wasm_runner/examples/wasm_info.rs (new, removed — scratch dumper)
- docs/wasm.md (fixed Dart row + notes)
- docs/dart-guest.md (new)
- docs/dart-wasm-WORKLOG.md (new, this file)

## Commands that passed

- `cargo check -p wasm_runner` (baseline + after changes)
- `cargo run -p wasm_runner --example gc_spike`


## Update (end of session)

- docs/wasm.md was concurrently edited by the kotlin-wasm sibling agent
  (language table now uses ✅/🚧 markers, added examples/wasm-guides/,
  docs/wasm-best-practices.md, C++ fixture + end_to_end.rs changes). My
  final edit replaced the Dart row ("in progress") with the verified verdict:
  engine GC works (examples/gc_spike.rs) but Dart is blocked end-to-end by
  legacy EH instructions + JS host dependency (docs/dart-guest.md).
- Final `cargo check -p wasm_runner` re-run after all edits.


## Update 2026-08-11 — re-verification (docs/dart-feasibility-2026.md)

Re-checked every claim via SDK source (dart.googlesource.com), Gerrit/GitHub
issue state, crates.io, and vendored wasmtime 47.0.3 source. What changed:

- **Standalone target is real now**: merged to SDK main 2026-06-02
  (dart-review 506920; "standalone target … is feature-complete now",
  exposes `dart compile wasm --standalone`); NOT in released 3.12.2 (tag
  verified), but present in 3.13.0-282.4.beta (2026-08-04). Next stable (3.13)
  ships it. Standalone modules import only `dart.*` host functions (≤82,
  tree-shaken) — no JS glue, no wasm:js-string, no JSTag. #63166 closed
  2026-07-29 (last JS bits removed from dart:_wasm).
- **Legacy EH still blocks wasmtime**: even standalone `$invokeMain` is
  try/catch+rethrow → every module has ≥1 legacy `try`; wasmtime 47 rejects.
  Upstream switching: CL 505900 (try_table codegen) NEW rev 15 (2026-08-10);
  plan = switch in stable ~Nov 2026 (3.14); wasm_builder already supports
  try_table. wasmtime 47.0.3 still newest (2026-07-31).
- **Workaround available today**: binaryen `wasm-opt --translate-to-exnref`
  converts legacy Phase-3 EH → new encoding (unverified on dart2wasm output;
  spike candidate).
- Verdict: PARTIALLY. Prototype path exists now (3.13 beta --standalone +
  wasm-opt translation + Rust `dart.*` shim + validation allowlist);
  clean path when 3.13 stable (--standalone) and #54394 (new EH) land.
