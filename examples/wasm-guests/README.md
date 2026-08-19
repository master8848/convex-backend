# Convex WASM guest examples

Ready-to-build, copy-paste guest modules for the Convex WASM backend-function
runtime (`crates/wasm_runner`). Each directory is a standalone project: build it
with its own toolchain, upload the resulting `.wasm` to Convex, and the module
analyzer + runner take it from there.

The guests implement the ABI in `crates/wasm_runner/src/abi.rs` (host-allocated
memory, `env` imports, `__convex_run`/`__convex_functions` exports).

## Supported targets

| Language | Status | Toolchain | Build |
|---|---|---|---|
| Rust | ✅ valid target | `wasm32-wasip1`, cargo | `make rust` |
| Go | ✅ valid target | Go >= 1.24, `GOOS=wasip1 GOARCH=wasm` | `make go` |
| C | ✅ valid target | LLVM clang `--target=wasm32-wasip1 -nostdlib` | `make c` |
| C++ | ✅ valid target | LLVM clang++ `--target=wasm32-wasip1 -nostdlib` | `make cpp` |
| Dart | 🚧 in progress | `dart compile wasm` → WasmGC; blocked upstream (legacy EH + JS host), see docs/dart-guest.md | — |
| Kotlin | 🚧 in progress | `wasmWasi` target under research | — |

See `docs/wasm.md` for the canonical status table and `docs/wasm-best-practices.md`
for authoring guidance.

## One-command build

```sh
make            # build rust + go + c + cpp examples into <dir>/build/*.wasm
make check      # verify your toolchains (skips what's missing)
make rust go c cpp
make clean
```

Each example prints its `__convex_functions` descriptor list when loaded and can
be exercised by the runner tests; the Rust and Go examples also read/write the
database through the host functions.

## Start a new guest project

```sh
./scaffold.sh rust my_guest       # copies the rust template into my_guest/
./scaffold.sh go my_guest
./scaffold.sh c my_guest
./scaffold.sh cpp my_guest
./scaffold.sh --list              # available templates
```

The scaffold is the fastest way to get a conforming guest: it pre-wires the
`env` imports, both exports, and the build command for your language.

## Layout

- `rust/`  — `#[convex_functions]` guest using the `convex_sdk` macros (queries,
  mutations, db reads/writes, logs, randomness, errors).
- `go/`    — hand-written `//go:wasmimport` / `//go:wasmexport` guest with db
  calls, no cgo.
- `c/`     — freestanding C: no libc, no WASI; smallest/fastest guest.
- `cpp/`   — freestanding C++: same ABI, no libc++/exceptions; shows classes,
  templates and `constexpr` that compile with `-nostdlib`.
- `dart/`, `kotlin/` — status + plan (see `docs/wasm.md`); scaffolds land with
  the WasmGC / `wasmWasi` work.

## Polyglot: add a Rust query in convex/search.rs

Mix languages in one `convex/` dir: add a Rust query in convex/search.rs next to `convex/messages.ts` and run `cargo build --target wasm32-wasip1` for the Rust guest. Both modules deploy as one `ApiSurface`; `convex/_generated/api.ts` is txt generated api.ts — do not edit — and `api.search.*` appears alongside `api.messages.*` via `anyApi` Proxy (`npm-packages/convex/src/server/api.ts:431`) and `ApiFromModules` (`npm-packages/convex/src/server/api.ts:255`).
