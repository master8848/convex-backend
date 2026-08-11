# WASM guest examples — finalization worklog

Task: finalize + commit the guest-examples work (examples/wasm-guests/,
docs/wasm-best-practices.md, docs/wasm.md language table, docs/dart-guest.md +
docs/dart-wasm-WORKLOG.md from a prior subagent, C++ fixture +
end_to_end.rs additions, examples/gc_spike.rs).

## Validation performed (all local, repo at main)

1. **Go example** (`examples/wasm-guests/go`): `GOOS=wasip1 GOARCH=wasm go
   build -buildmode=c-shared -o /tmp/go_example.wasm .` → OK (3.2 MB module).
2. **Rust example** (`examples/wasm-guests/rust`): `cargo build
   --target wasm32-wasip1 --release` → OK (wasm32-wasip1 target already
   installed; 163 KB module). Only warnings (pre-existing convex_sdk dead-code
   consts + one `private_interfaces` warning in the example, fixed).
3. **Makefile**: `make -n` dry-run OK; `make check` OK (cargo/wasm32-wasip1/go
   detected, clang(wasm) correctly reported MISSING on Apple clang);
   `make rust go` built both examples into build/.
4. **scaffold.sh**: `--list` OK; scaffolded rust/go/c/cpp into a /tmp dir —
   all four produce correct trees; rust Cargo.toml package name replaced, go
   module name replaced; invalid-name input rejected.
5. **C/C++ builds**: Apple clang lacks wasm32-wasip1 (expected; end_to_end.rs
   skips gracefully on "No available targets are compatible"). BUT Homebrew
   LLVM 22.1.8 is installed, so I ran the exact documented commands:
   - C example + fixture: compile OK (1.5 KB modules).
   - C++ example + fixture: **initially FAILED** — `#include <cstdint>` can't
     resolve: stock clang++ for wasm32-wasip1 has no libc++ include dir
     without a wasi-sdk sysroot. FIXED by making the C++ guest fully
     freestanding (no headers; built-in int types). Now compiles with the
     exact Makefile/test command. Example + fixture kept byte-identical.
   - `llvm-objdump` on the C++ module: imports only the 4 `env` functions in
     use, exports `__convex_run`/`__convex_functions` (+`__stack_pointer`).
6. **cargo check -p wasm_runner --tests** → OK (rc=0; deps cached).
7. **cargo run -p wasm_runner --example gc_spike** → GC SPIKE OK (sum=72,
   i31 roundtrips, ref_eq semantics, wasm_gc(false) rejection).
8. **Full e2e suite**: `cargo test -p wasm_runner --test end_to_end` with
   Homebrew LLVM first on PATH → **5/5 passed**, including the new
   test_c_guest_end_to_end and test_cpp_guest_end_to_end with real compiled
   modules (rust/go/c/cpp + module validation).

## Fixes applied (minimal)

- `examples/wasm-guests/cpp/guest.cpp` + `crates/wasm_runner/tests/fixtures/
  cpp_guest/guest.cpp` (kept identical): removed `<cstdint>`/`std::*` typedefs
  → built-in integer types, so the documented build line works with stock
  LLVM clang++ (no wasi-sdk). Documented in both files.
- `examples/wasm-guests/cpp/README.md` + `docs/wasm-best-practices.md`: added
  the "no headers at all (not even <cstdint>)" rule.
- `examples/wasm-guests/rust/src/lib.rs`: `struct User` → `pub struct User`
  (silences the `private_interfaces` warning in the committed example).
- `examples/wasm-guests/README.md`: Dart row corrected — not "GC validation in
  flight" but "blocked upstream (legacy EH + JS host), see docs/dart-guest.md".
- `examples/wasm-guests/kotlin/README.md`: dropped reference to nonexistent
  docs/kotlin-wasm-WORKLOG.md.
- `examples/wasm-guests/.gitignore` (NEW): ignores `build/` (make output) and
  `*.wasm` so `git add` of the tree stays clean.

## Commit

Single commit with ONLY the intended paths (no `git add -A`):
examples/wasm-guests/, docs/wasm-best-practices.md, docs/wasm.md,
docs/dart-guest.md, docs/dart-wasm-WORKLOG.md,
crates/wasm_runner/tests/end_to_end.rs,
crates/wasm_runner/tests/fixtures/cpp_guest/,
crates/wasm_runner/examples/gc_spike.rs.

Follow-ups:
- Kotlin `wasmWasi` export-ABI research (kotlin/ README stub; kotlin-wasm
  worklog does not exist — sibling agent may add it).
- Dart needs upstream dart-lang/sdk#54394 + standalone target (#53884) plus a
  wasm_runner host shim; wasmtime GC side already proven (gc_spike).
- A CI image with Homebrew/wasi-sdk LLVM would make the C/C++ e2e tests run
  (they skip on Apple clang).
