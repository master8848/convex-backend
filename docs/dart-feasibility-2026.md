# Dart guest feasibility: upstream engine and toolchain status

This is the deep technical reference for running `dart compile wasm` output in wasm_runner: upstream Dart SDK and wasmtime status, the exact blockers, and the workarounds. The Dart guest status page and ABI are in [dart-guest.md](dart-guest.md); the language status matrix is [non-js-languages.md](non-js-languages.md); the runtime's WasmGC support is in [wasm.md](wasm.md).

## Verdict

Full Dart guest support is blocked on two upstream items: (a) a released SDK with `dart compile wasm --standalone` (next stable 3.13, ~Sept–Oct 2026) and (b) dart2wasm emitting the new exception instructions (stable ~Nov 2026, 3.14). Both are in flight, and a prototype is buildable with the beta SDK plus a binaryen post-step, so host-side work in wasm_runner is not blocked on upstream design — only on the two toolchain gates.

| Question | Answer |
|---|---|
| Newest wasmtime? | 47.0.3 (crates.io max_stable, updated 2026-07-31). No 48. |
| Newest stable Dart? | 3.12.2 (2026-06-09). Beta: 3.13.0-282.4.beta (2026-08-04). Dev: 3.14.0-110.0.dev (2026-08-08). |
| `dart compile wasm --standalone` in a released stable SDK? | No (not in the 3.12.2 tag). Yes in beta (hidden flag; prints "experimental, used imports may change"). Next stable (3.13) has it. |
| Standalone module still imports JS? | No. Only `dart.*` host imports (≤82 possible, tree-shaken to the used subset). `dart:js_interop` is unavailable in standalone mode; dart-lang/sdk#63166 closed 2026-07-29. |
| Does a standalone module compile in wasmtime 47 as-is? | No — legacy EH. Every module contains at least one legacy `try`. `wasm_legacy_exceptions` is a doc(hidden) spec-testsuite-only knob that `Engine::new` rejects (vendored wasmtime 47.0.3 `src/config.rs`: LEGACY_EXCEPTIONS ∉ `features_known_to_wasmtime`). |
| Will that change soon? | Yes. dart-lang/sdk#54394: dart2wasm switches to new EH in a stable ~Nov 2026 (3.14); CL 505900 in progress; the SDK's wasm_builder IR already supports try_table/throw_ref. |
| Other runtimes a way out? | No. wasmi has no EH support. WAMR tracks EH on the roadmap (bytecodealliance/wasm-micro-runtime#1884). wasmtime is the only Rust engine with the new EH encoding. |

## JS-mode output: the JavaScript host dependency

A trivial pure-Dart program (`print` + string concat, no JS interop) compiled with the stable SDK produces a module with 249 imports:

| Module | Count | What it is |
|---|---|---|
| `''` (empty) | 239 globals | string constants (class names, error messages...); the JS glue serves them with `new Proxy({}, {get: (_, p) => p})` |
| `dart2wasm` | 8 funcs | `_30` print, `_178`/`_179`/`_221` stack traces, `_185` undefined check, `_212` String(), `_315`/`_316` number→string |
| `wasm:js-string` | 1 func | `concat` (JS string builtins; the glue ships a 10-function polyfill: charCodeAt, compare, concat, equals, fromCharCode, length, substring, fromCharCodeArray, intoCharCodeArray, test) |
| `WebAssembly` | 1 global | `JSTag` |

Exports: `$invokeMain`, `$wasmI16ArrayGet`, `$wasmI16ArraySet`, `$setThisModule`. The module is requested with `WebAssembly.compile(bytes, {builtins: ['js-string']})` — the JS string builtins proposal.

Host imports written by a Dart developer (`@JS('__convex_input_length') external int ...` from `dart:js_interop`) do not become direct `env.__convex_*` wasm imports: dart2wasm lowers them to numbered `dart2wasm._NNN` helpers whose JS implementations dispatch through `globalThis.__convex_input_length()`. The whole runtime is JS-host-mediated (event loop, printing, timers, weak maps/finalizers, stack traces, double↔string, regexps, math — the list from dart-lang/sdk#53884).

## Dart toolchain: the standalone target

- Standalone target is feature-complete, merged to SDK main 2026-06-02 (dart-review 506920): "the standalone target for dart2wasm is feature-complete now ... adds the platform and outline files to built SDKs and exposes the `--standalone` flag in `dart compile wasm`."
- Not in released 3.12.2: `pkg/dartdev/lib/src/commands/compile.dart` at tag 3.12.2 (peeled commit `d684a576...` == the 3.12.2 VERSION revision) has no `standalone`; the beta branch has the full wiring: `final standalone = args.flag('standalone'); final platform = standalone ? sdk.wasmStandalonePlatformDill : sdk.wasmPlatformDill;` and `if (standalone) '--standalone'`, plus a hint pointing at `pkg/dart2wasm/docs/standalone.md`.
- Standalone platform ships in the SDK source tree on main/beta/stable: `sdk/lib/_internal/wasm/standalone/` (embedder.dart, print_patch.dart, stack_trace_patch.dart, timer_patch.dart, math_externs_patch.dart, regexp_patch.dart, weak_patch.dart, string_patch.dart, ...).
- Docs (main): `pkg/dart2wasm/docs/standalone.md` — "For WebAssembly targets without a JavaScript engine (like wasmtime ...), dart2wasm supports a standalone target too. This target is enabled with the `--standalone` compiler flag, and makes `dart:js_interop` unavailable. Even in standalone mode, the SDK needs host imports for functionality like timers, stack traces, regular expressions, `dart:math` or number formatting." Two run modes suggested: native engine APIs for the imports, or reimplementing imports in wasm + `wasm-merge`.
- Host import surface: `sdk/lib/_internal/wasm/standalone/embedder.dart` declares 82 `@pragma("wasm:import", "dart.<name>")` imports — e.g. `dart.print`, `dart.currentTime`, `dart.scheduleOnce`/`scheduleRepeated`, `dart.queueMicrotask`, `dart.stackTraceGetCurrent`/`stackTraceToString`, `dart.stringConcat`/`Length`/`Equals`/`Substring`/`FromCharCodeArray`/`ToCodeUnits`, `dart.math*`, `dart.randomInt`/`randomIntSecure`, `dart.weakRefCreate`/`Get`, `dart.finalizer*`, `dart.regexp*`, `dart.f64ToString`/`i64ToString`/`doubleTryParse`, `dart.jsonEncodeString`, `dart.timeZone*`, `dart.baseUri`, `dart.isWindows`. Only the used subset is imported by a given module (TFA tree-shaking).
- Compiler internals (beta): `pkg/dart2wasm/lib/compile.dart` selects `wasm.Mode.standalone`, skips `js.performJSInteropTransformations` and the JS runtime finalizer, and adds `dart:_embedder` to the platform libraries. `dart2wasm.dart` defines the `--standalone` flag (sets `dart.wasm.standalone` environment).
- dart-lang/sdk#63166 (split `dart:_wasm` to avoid `dart:js_interop`): closed 2026-07-29 by mkustermann — "391d6eead... has now removed the last JS bits from `dart:_wasm`" — so the standalone platform no longer pulls in js_interop.
- Status issue dart-lang/sdk#53884 (Support non-JS wasm runtimes): open; body updated 2026-06-08 lists the two remaining release-blockers as #54394 and shipping `--standalone` (the latter landed on main/beta); simolus3's 2026-05-20 comment: "As of 58f9d88fb2ee..., it's possible to compile Dart apps to WebAssembly without any JS interop! However, that doesn't mean that you can just `wasmtime run dart_program.wasm` now, we still need external definitions for things like stack traces, regular expressions and all the other things" — and asks whether a Rust crate to run Dart wasm programs would be useful (an opening for wasm_runner).

## Exception handling (#54394) — the engine gate

- Issue open, updated 2026-08-10. wasmtime only supports the new EH encoding; legacy `try`/`catch`/`rethrow` are deprecated in the spec.
- SDK's own wasm_builder (`pkg/wasm_builder/lib/src/ir/instruction.dart`, `builder/instructions.dart`, beta) already contains TryTable / try_table / ThrowRef / throw_ref / CatchRef — the low-level plumbing is done.
- What's missing is the dart2wasm codegen switch: dart-review 414140 (osa1, original): ABANDONED 2026-05-22. dart-review 505900 "[dart2wasm] Use new exception instructions (try_table, throw_ref)": rev 15, created 2026-05-22, updated 2026-08-10 (kevmoo: "a reasonable start — much further along than 414140").
- Timing (comments): mkustermann 2026-08-05: "switch entirely to new exception instructions in the stable release coming out roughly November this year" (simolus3: "That's the 3.14 release, right?"); kevmoo 2026-08-07: "absolutely worth it to move to the new try_table bits for the next stable release (after August)". Browser-impact analysis on the CL shows requiring try_table shifts only ~1.5% of Tier-1 web traffic to the dart2js fallback.
- Even in standalone mode, a module contains at least one legacy `try`: the `$invokeMain` export in `sdk/lib/_internal/wasm/standalone/invoke_main_patch.dart` is `try { ... } catch (e, s) { print(e); print(s); rethrow; }`.

## wasmtime

- 47.0.3 is the newest wasmtime on crates.io (max_stable, updated 2026-07-31); the workspace pins it. `default` features include `gc`, `gc-copying`, `gc-drc`, `gc-null`; `cargo tree -p wasm_runner -e features` confirms `gc` is enabled in this build. `Config::wasm_gc(bool)` is true by default.
- Vendored 47.0.3 `src/config.rs`: `wasm_exceptions` (new EH) is true by default; `wasm_legacy_exceptions` is `#[doc(hidden)]`, "only exists for internal usage with the spec testsuite. It may be removed at any time ... Do not rely on it!" and `LEGACY_EXCEPTIONS` is not in `features_known_to_wasmtime`, so enabling it fails at `Engine::new` ("the wasm_legacy_exceptions feature is not supported on this compiler configuration").
- wasm_runner's `Config` (engine.rs) sets nan-canonicalization, relaxed-SIMD off, fuel on → WasmGC and new EH stay on by default. The GC proof (`crates/wasm_runner/examples/gc_spike.rs`: struct/array/i31/ref.eq under the runner's exact Config) is documented in [wasm.md](wasm.md).
- `Store::add_fuel` does not exist in 47; wasm_runner uses `store.set_fuel(...)` + `fuel_async_yield_interval(...)` (engine.rs).

## The binaryen workaround

- `wasm-opt --translate-to-exnref` (alias `--translate-to-new-eh`, deprecated) — binaryen `src/passes/TranslateEH.cpp`: "translates the old Phase 3 EH instructions, which include try, catch, catch_all, delegate, and rethrow, into the new EH instructions, which include try_table ... and throw_ref ... This translator can be used as a standalone tool by users of the previous EH toolchain to generate binaries for the new spec without recompiling."
- This is the missing link for running beta-SDK standalone modules in wasmtime 47 today: compile with `dart compile wasm --standalone`, post-process with `wasm-opt --enable-gc --translate-to-exnref`, then instantiate in wasmtime. Not yet verified end-to-end on dart2wasm output — spike-stage.
- binaryen already handles WasmGC modules, and the Dart SDK itself invokes wasm-opt during `dart compile wasm` (its binaryen flags include `--enable-gc`, `--enable-exception-handling`).
- Other runtimes offer no shortcut: wasmi has no EH support; WAMR tracks EH in bytecodealliance/wasm-micro-runtime#1884 (open).

## Convex side (what a Dart guest needs from wasm_runner)

- Imports: the Convex ABI lives under module `env` (`HOST_FN_MODULE` in `crates/wasm_runner/src/abi.rs`) — in standalone Dart these are expressible as `@pragma("wasm:import", "env.__convex_input_length")` etc. (the SDK's `pkg/dart2wasm/docs/imports_and_exports.md` documents `foo.bar`-style imports; note it's marked "internal usage only, not intended for end-users (yet)" — tracked in dart-lang/sdk#55856 — and the exact `() -> i32` export signature mechanics still need pinning).
- Validation: `crates/wasm_runner/src/validation.rs` allowlists only `env` + `wasi_snapshot_preview1`; a Dart guest additionally imports module `dart` (the standalone host shim) → allowlist must be extended.
- Host shim: implement the used `dart.*` imports in Rust (a trivial program needs roughly 15–25: print, currentTime, monotonicClock*, string*, stackTraceGetCurrent/ToString, stringBuffer*, number↔string, math*, randomInt...). Stack traces map to wasmtime trap info, timers to tokio, `dart.currentTime`/`randomInt` to the existing virtual clock / seeded RNG for determinism (same pattern as the WASI host today).
- Once #54394 lands, stock `dart compile wasm --standalone` output runs in wasmtime 47 directly (WasmGC + new EH both on by default); nothing on the wasmtime side needs to change.

## Sources

- dart-lang/sdk#53884 (non-JS wasm runtimes, open): https://github.com/dart-lang/sdk/issues/53884
- dart-lang/sdk#54394 (new exception instructions, open, updated 2026-08-10): https://github.com/dart-lang/sdk/issues/54394
- dart-lang/sdk#63166 (closed 2026-07-29): https://github.com/dart-lang/sdk/issues/63166
- dart-review 506920 (standalone in SDK, MERGED 2026-06-02): https://dart-review.googlesource.com/c/sdk/+/506920
- dart-review 505900 (try_table codegen, rev 15, 2026-08-10): https://dart-review.googlesource.com/c/sdk/+/505900
- dart-review 414140 (ABANDONED): https://dart-review.googlesource.com/c/sdk/+/414140
- SDK version endpoints (stable 3.12.2 / beta 3.13.0-282.4.beta / dev 3.14.0-110.0.dev): https://storage.googleapis.com/dart-archive/channels/{stable,beta,dev}/release/latest/VERSION
- SDK changelog (stable branch head = 3.13.0 "Unreleased"): https://github.com/dart-lang/sdk/blob/stable/CHANGELOG.md
- Standalone docs: https://github.com/dart-lang/sdk/blob/main/pkg/dart2wasm/docs/standalone.md
- Imports/exports: https://github.com/dart-lang/sdk/blob/main/pkg/dart2wasm/docs/imports_and_exports.md
- Embedder host imports: https://github.com/dart-lang/sdk/blob/main/sdk/lib/_internal/wasm/standalone/embedder.dart
- binaryen TranslateEH.cpp (TranslateToExnref): https://github.com/WebAssembly/binaryen/blob/main/src/passes/TranslateEH.cpp
- crates.io wasmtime (47.0.3 max_stable, 2026-07-31): https://crates.io/api/v1/crates/wasmtime
- wasmtime 47.0.3 vendored `src/config.rs` (this repo's cargo registry cache)
- bytecodealliance/wasm-micro-runtime#1884 (EH roadmap, open)
