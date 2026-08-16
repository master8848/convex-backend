# Agent Note: Guest language fixtures and examples

Status: implemented

## Problem

The WASM ABI is language-agnostic, but shipping support for a language requires proof: a compilable guest fixture, an end-to-end test against the real runner, and an example. Without those, each claimed target language is an unverified promise.

## Decision

Each guest language lands with a fixture, an e2e test, and an example, and the status of every candidate language is tracked in one matrix:

- **Go**: wasip1 guest requiring `_initialize`; e2e against the real runner.
- **C / C++**: freestanding guests importing only `env`, the smallest modules; e2e tests and examples.
- **Zig**: reactor module (394–837 B) importing only `env`; Zig 0.16 needs explicit `--export=` flags for wasm targets; e2e test and example.
- **Kotlin**: wasmWasi (wasm32-wasip1 + WasmGC) fixture with a toolchain-gated e2e test (needs JDK + Gradle, not run in CI).
- **Dart**: feasibility verified against upstream gates (standalone target in 3.13 beta; legacy exception-handling blocks wasmtime; a wasm-opt `--translate-to-exnref` workaround exists); not shipped as a guest yet.

The status matrix lives in `docs/non-js-languages.md`; guest best practices and examples in `docs/wasm-best-practices.md` and the guest examples directory.

## Alternatives considered

- **Claim a language without a fixture**: the matrix would record support that no test exercises; every shipped claim is e2e-tested.
- **Compile-and-run helpers instead of committed fixtures**: a fixture committed with the toolchain invocation reproduces across machines; helpers drift silently.

## Consequences

- The language matrix is the one home for target status; a language moves from "not evaluated" to "landed" only when its fixture and e2e test ship.
- Toolchain-gated tests (Kotlin) document their gate in the matrix so the absence from CI is a fact, not an omission.
