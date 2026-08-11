# Dart guests for Convex WASM functions

Status: **not viable today — but not for the reason previously documented.**
The engine already supports WasmGC; the blockers are dart2wasm's legacy
exception-handling instructions and its hard dependency on a JavaScript host.
This page is the Dart developer's guide: what the ABI is, why a stock
`dart compile wasm` module can't run yet, and exactly how a Dart guest will be
structured once the blockers clear.

## TL;DR

| Question | Answer |
|---|---|
| Does wasmtime 47 support WasmGC? | **Yes.** `gc` is in wasmtime 47.0.3's default features and `Config::wasm_gc` is `true` by default. `cargo run -p wasm_runner --example gc_spike` proves struct/array/i31/ref.eq modules run under wasm_runner's exact Config. |
| Can a stock `dart compile wasm` module run today? | **No.** wasmtime rejects it at compile time: `legacy_exceptions feature required for try instruction` (dart2wasm emits the old exception-handling encoding; wasmtime 47 only supports the new `try_table` encoding). |
| Even if that compiled? | **No.** Every dart2wasm module imports a JavaScript host: `dart2wasm.*` helpers (print, timers, `Error().stack`, `queueMicrotask`, ...), `wasm:js-string` string builtins, string-constant globals, `WebAssembly.JSTag`. No standalone mode is shipped in stable SDKs (verified on Dart 3.12.2). |
| What unblocks it? | dart-lang/sdk#54394 (dart2wasm → new exception instructions) + a shipped `--standalone` target (dart-lang/sdk#53884), then a Rust host shim in wasm_runner. |

## Verified facts (2026-08, wasmtime 47.0.3, Dart 3.12.2)

### Engine side: WasmGC is already here

- `cargo search wasmtime` → newest version on crates.io is **47.0.3**, the same
  version the workspace already pins (`Cargo.toml`: `wasmtime = { version =
  "47.0.3", features = ["async", "anyhow"] }`). Nothing to upgrade.
- `cargo info wasmtime` → 47.0.3's `default` features include `gc`,
  `gc-copying`, `gc-drc`, `gc-null`; `cargo tree -p wasm_runner -e features`
  confirms `gc` is enabled in this build.
- wasmtime 47 `Config`: `wasm_gc(bool)` is "true by default"; the collector is
  selectable (`Collector::Auto` default). The new exception-handling proposal
  (`wasm_exceptions`, `try_table`/`throw_ref`/`exnref`) is also "true by
  default". `wasm_legacy_exceptions` exists but is a deprecated spec-testsuite
  knob — `Engine::new` rejects it
  (`the wasm_legacy_exceptions feature is not supported on this compiler
  configuration`), because `LEGACY_EXCEPTIONS` is not in wasmtime's
  `features_known_to_wasmtime` set (wasmparser `features.rs`).
- Proof of GC execution under wasm_runner's exact Config
  (NaN canonicalization, relaxed-SIMD off, fuel on):
  `cargo run -p wasm_runner --example gc_spike` (kept in the repo).

### Dart side: `dart compile wasm` output

A trivial pure-Dart program (`print` + string concat, no JS interop) compiles to
a module with **249 imports**:

| Module | Count | What it is |
|---|---|---|
| `''` (empty) | 239 globals | string constants (class names, error messages...); the JS glue serves them with `new Proxy({}, {get: (_, p) => p})` |
| `dart2wasm` | 8 funcs | `_30` print, `_178`/`_179`/`_221` stack traces, `_185` undefined check, `_212` String(), `_315`/`_316` number→string |
| `wasm:js-string` | 1 func | `concat` (JS string builtins; the glue ships a 10-function polyfill: charCodeAt, compare, concat, equals, fromCharCode, length, substring, fromCharCodeArray, intoCharCodeArray, test) |
| `WebAssembly` | 1 global | `JSTag` |

Exports: `$invokeMain`, `$wasmI16ArrayGet`, `$wasmI16ArraySet`,
`$setThisModule`. The module is requested with `WebAssembly.compile(bytes,
{builtins: ['js-string']})` — the JS string builtins proposal.

Host imports written by a Dart developer (`@JS('__convex_input_length')
external int ...` from `dart:js_interop`) do **not** become direct
`env.__convex_*` wasm imports: dart2wasm lowers them to numbered
`dart2wasm._NNN` helpers whose JS implementations dispatch through
`globalThis.__convex_input_length()` etc. The whole runtime is
JS-host-mediated (event loop, printing, timers, weak maps/finalizers, stack
traces, double↔string, regexps, math — the list from dart-lang/sdk#53884).

There is no escape hatch in released SDKs: `dart compile wasm --help` has no
`--standalone` flag, and the SDK ships no `dart2wasm_standalone_platform.dill`
(Dart 3.12.2, verified). Upstream: dart-lang/sdk#53884 tracks non-JS runtimes;
#54394 tracks dart2wasm switching to the new exception instructions ("should be
a straightforward refactoring", still open); #63166 (removing `dart:js_interop`
from the standalone platform) closed 2026-07-29.

## The ABI a Dart guest must implement

Same as every other guest (`crates/wasm_runner/src/abi.rs`):

- Exports (both `() -> i32`):
  - `__convex_run` — dispatcher; pulls the input
    `{"function": ..., "args": [...]}` via the input host functions, runs the
    named function, reports the JSON result with `__convex_output_set`.
  - `__convex_functions` — returns, via `__convex_output_set`, a JSON array of
    `{"name": ..., "type": "query"|"mutation"|"action"|"httpAction"}`.
- Imports under module `env`:
  - `__convex_input_length() -> i32`, `__convex_input_load(offset, dest, len)`
  - `__convex_alloc(len) -> i32`, `__convex_call_data_load(offset, dest, len)`
  - `__convex_output_set(ptr, len)`, `__convex_error_set(ptr, len)`
  - `__convex_log(ptr, len)`, `__convex_now_ms() -> i64`,
    `__convex_random_bytes(dest, len)`
  - `__convex_db_get/insert/replace/patch/delete/count/query(args_ptr,
    args_len) -> i64` (packed `(offset << 32) | len` into call data, or -1)

Memory crossing the boundary is host-allocated (the Extism pattern), so the
guest only needs linear memory, the imports above, and the two exports.
## Skeleton guest (once the blockers clear)

With the standalone dart2wasm target (or a hand-rolled shim), a Dart guest
would look like this. Today's toolchain cannot produce this module — see
"Current status" below for what to try right now.

```dart
// convex_guest.dart — skeleton for a future dart2wasm standalone target.
// Host functions come from the Convex runtime (crates/wasm_runner/src/abi.rs);
// in a standalone dart2wasm these are `dart:_wasm` host imports rather than
// JS interop. Signature and dispatch follow the Rust/Go/C guests.

import 'dart:convert';
import 'dart:typed_data';

// -- host ABI (imported; names match abi.rs) --------------------------------
// (syntax placeholder: dart2wasm standalone will expose host imports; today
//  `@JS(...)` externals compile to dart2wasm._NNN -> globalThis dispatch)
int convexInputLength() => _hostInputLength();
void convexInputLoad(int offset, int dest, int len) => _hostInputLoad(offset, dest, len);
int convexAlloc(int len) => _hostAlloc(len);
void convexCallDataLoad(int offset, int dest, int len) => _hostCallDataLoad(offset, dest, len);
void convexOutputSet(int ptr, int len) => _hostOutputSet(ptr, len);
void convexErrorSet(int ptr, int len) => _hostErrorSet(ptr, len);
void convexLog(int ptr, int len) => _hostLog(ptr, len);
int convexNowMs() => _hostNowMs();
void convexRandomBytes(int dest, int len) => _hostRandomBytes(dest, len);

// -- guest memory helpers ---------------------------------------------------
final Uint8List _mem = _wasmMemory(); // the exported `memory`

int _readLen() {
  final n = convexInputLength();
  final ptr = convexAlloc(n);
  convexInputLoad(0, ptr, n);
  return ptr;
}

Uint8List _bytes(int ptr, int len) => Uint8List.sublistView(_mem, ptr, ptr + len);

void _writeStr(String s) {
  final b = utf8.encode(s);
  final ptr = convexAlloc(b.length);
  _mem.setRange(ptr, ptr + b.length, b);
  convexOutputSet(ptr, b.length);
}

// -- exported entry points --------------------------------------------------
int convexRun() {
  final ptr = _readLen();
  final input = jsonDecode(utf8.decode(_bytes(ptr, convexInputLength())));
  try {
    final result = _dispatch(input['function'] as String, input['args'] as List);
    _writeStr(jsonEncode(result));
    return 0;
  } catch (e) {
    _writeStr(jsonEncode({'error': '$e'}));
    return 1;
  }
}

int convexFunctions() {
  _writeStr(jsonEncode([
    {'name': 'addOne', 'type': 'query'},
  ]));
  return 0;
}

// -- user functions ---------------------------------------------------------
Object? _dispatch(String name, List args) {
  switch (name) {
    case 'addOne':
      return (args[0] as num) + 1;
    default:
      throw StateError('unknown function: $name');
  }
}
```

The exported names (`convexRun`/`convexFunctions`) must be renamed to
`__convex_run`/`__convex_functions`; with dart2wasm that is an exporter
concern (a `@wasm:export`-style annotation or a post-processing step that
aliases the exports) — exact mechanics land with the standalone target.

## Current status: what you can do today

- **Write and inspect the ABI in Dart**: `@JS('__convex_input_length') external
  int ...` from `dart:js_interop` compiles, and the generated `*.mjs` shows the
  host contract (`dart2wasm._NNN` → `globalThis.__convex_input_length()`). You
  can prototype the guest logic in Dart and port it when standalone lands.
- **It will not run in wasm_runner yet**: (1) `Module::new` fails on legacy
  exception instructions; (2) `validate_module`
  (`crates/wasm_runner/src/validation.rs`) rejects all imports outside `env`
  and `wasi_snapshot_preview1` — `dart2wasm`, `wasm:js-string`, `''`, and
  `WebAssembly` would all be rejected even after compilation succeeds.
- **Watch**: dart-lang/sdk#54394 (new EH instructions), dart-lang/sdk#53884
  (non-JS runtimes / standalone), a stable `dart compile wasm --standalone`.

## What wasm_runner needs when Dart becomes viable

1. **Compilation**: dart2wasm must stop emitting legacy `try`/`catch`
   (upstream #54394), or wasmtime must grow legacy-EH codegen (unlikely —
   legacy EH is deprecated there).
2. **Host shim** (new Rust code): implement the `dart2wasm` helper module
   (queueMicrotask → tokio, `Error().stack` → trap info, print → `__convex_log`,
   number/string conversions) and the `wasm:js-string` polyfill over
   `externref` strings, plus the `''` string-constant globals and
   `WebAssembly.JSTag`; wire the module's `$invokeMain` bootstrap to call
   `__convex_run`/`__convex_functions`.
3. **Validation** (`validation.rs`): extend the import allowlist or translate
   the module before validation.
4. **Determinism**: `Math.random`/`Date` host functions must go through the
   seeded RNG / virtual clock, as the WASI ones do today.
