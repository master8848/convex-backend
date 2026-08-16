# Dart guests for Convex WASM functions

This is the Dart guest status page: what the ABI is, why a stock `dart compile wasm` module cannot run end-to-end, and how a Dart guest is structured once the blockers clear. Upstream engine and toolchain evidence (wasmtime EH encoding, the standalone dart2wasm target, the binaryen workaround, issue and SDK-version data) lives in [dart-feasibility-2026.md](dart-feasibility-2026.md); the language status matrix is [non-js-languages.md](non-js-languages.md); the runtime's WasmGC support is documented in [wasm.md](wasm.md).

## Status

- Dart is not a valid guest target today. The engine-side WasmGC requirement is met (wasmtime 47 runs WasmGC modules under the runner's exact Config — see [wasm.md](wasm.md)), but two blockers remain: dart2wasm emits legacy exception-handling instructions that wasmtime 47 rejects, and stock stable-SDK modules import a JavaScript host (`dart2wasm.*` helpers, `wasm:js-string` builtins, string-constant globals). The standalone target exists only in the 3.13 beta SDK, and even standalone modules contain legacy `try` instructions.
- A prototype path exists with the beta SDK plus a binaryen post-step; the gates and workarounds are detailed in [dart-feasibility-2026.md](dart-feasibility-2026.md).
- Flutter mobile stays on Dart AOT native.

## The ABI a Dart guest must implement

Same as every other guest (`crates/wasm_runner/src/abi.rs`):

- Exports (both `() -> i32`):
  - `__convex_run` — dispatcher; pulls the input `{"function": ..., "args": [...]}` via the input host functions, runs the named function, reports the JSON result with `__convex_output_set`.
  - `__convex_functions` — returns, via `__convex_output_set`, a JSON array of `{"name": ..., "type": "query"|"mutation"|"action"|"httpAction"}`.
- Imports under module `env`:
  - `__convex_input_length() -> i32`, `__convex_input_load(offset, dest, len)`
  - `__convex_alloc(len) -> i32`, `__convex_call_data_load(offset, dest, len)`
  - `__convex_output_set(ptr, len)`, `__convex_error_set(ptr, len)`
  - `__convex_log(ptr, len)`, `__convex_now_ms() -> i64`, `__convex_random_bytes(dest, len)`
  - `__convex_db_get/insert/replace/patch/delete/count/query(args_ptr, args_len) -> i64` (packed `(offset << 32) | len` into call data, or -1)

Memory crossing the boundary is host-allocated (the Extism pattern), so the guest only needs linear memory, the imports above, and the two exports.

## Skeleton guest (once the blockers clear)

With the standalone dart2wasm target (or a hand-rolled shim), a Dart guest looks like this. Today's toolchain cannot produce this module — see [Status](#status).

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

The exported names (`convexRun`/`convexFunctions`) must be renamed to `__convex_run`/`__convex_functions`; with dart2wasm that is an exporter concern (a `@wasm:export`-style annotation or a post-processing step that aliases the exports) — exact mechanics land with the standalone target.

## What you can do today

- Write and inspect the ABI in Dart: `@JS('__convex_input_length') external int ...` from `dart:js_interop` compiles, and the generated `*.mjs` shows the host contract (`dart2wasm._NNN` → `globalThis.__convex_input_length()`). Guest logic can be prototyped in Dart and ported when standalone lands. The JS-mode import surface and lowering rules are in [dart-feasibility-2026.md](dart-feasibility-2026.md).
- It will not run in wasm_runner yet: (1) `Module::new` fails on legacy exception instructions; (2) `validate_module` (`crates/wasm_runner/src/validation.rs`) rejects all imports outside `env` and `wasi_snapshot_preview1` — `dart2wasm`, `wasm:js-string`, `''`, and `WebAssembly` are all rejected even after compilation succeeds.

## What wasm_runner needs when Dart becomes viable

1. Compilation: dart2wasm must stop emitting legacy `try`/`catch` (upstream dart-lang/sdk#54394), or modules must be post-processed with `wasm-opt --translate-to-exnref`. wasmtime will not grow legacy-EH codegen — legacy EH is deprecated there. Upstream status: [dart-feasibility-2026.md](dart-feasibility-2026.md).
2. Host shim (new Rust code): for JS-mode output, implement the `dart2wasm` helper module (queueMicrotask → tokio, `Error().stack` → trap info, print → `__convex_log`, number/string conversions) and the `wasm:js-string` polyfill over `externref` strings, plus the `''` string-constant globals and `WebAssembly.JSTag`; wire the module's `$invokeMain` bootstrap to call `__convex_run`/`__convex_functions`. For standalone output the surface is the `dart.*` host imports (see [dart-feasibility-2026.md](dart-feasibility-2026.md)).
3. Validation (`validation.rs`): extend the import allowlist or translate the module before validation.
4. Determinism: `Math.random`/`Date` host functions must go through the seeded RNG / virtual clock, as the WASI ones do today.
