# Agent Note: Polyglot backend and cross-language client generation

Status: implemented

## Problem

A project needs backend functions written in multiple languages at once (e.g., a TypeScript app with some functions in Rust, some in Kotlin, some in TypeScript) while every client language (TypeScript, Kotlin, Rust, C#, Dart) receives a complete typed `api` surface — the same guarantee the current TypeScript-only `convex/_generated/api` provides via `ApiFromModules`. Generating clients only for Kotlin (e.g., KMP) or only for the backend language breaks this.

## Decision

Use `AnalyzedFunction { name, udfType, visibility, argsValidatorJson, returnsValidatorJson }` in `crates/model/src/modules/module_versions.rs:282` as the language-agnostic IR, and make codegen a per-target emitter over the merged deployment `ApiSurface`. `ModuleMetadata.environment` in `crates/model/src/modules/types.rs:22` carries `ModuleLanguage { TypeScript, RustWasm, GoWasm, Zig, CWasm, KotlinWasm, DartWasm, CSharpWasm }`; each language analyzes via its own path — TypeScript via `crates/isolate` `exportArgs`/`exportReturns`, WASM guests via `crates/wasm_runner::analyze_functions` calling `__convex_functions` (host-allocated `env` ABI in `crates/wasm_runner/src/abi.rs`) — but all produce the same `argsValidatorJson`/`returnsValidatorJson` strings (Convex validator JSON in `crates/model/src/modules/function_validators.rs:33`). The deploy step merges all `AnalyzedModule`s into one `ApiSurface` by `CanonicalizedModulePath`; `npm-packages/convex/src/cli/lib/codegen.ts` `doCodegen` and `codegen_templates/api.ts:78` `apiCodegen` become one of several emitters run for each requested client target, each mapping the same validator IR to target types. Target emitters are: TypeScript (`convex/_generated/api.ts`/`api.d.ts` via `ApiFromModules`/`FilterApi` in `npm-packages/convex/src/cli/codegen_templates/api.ts`), Kotlin (`convex/_generated/Api.kt` `data class` args/returns with `kotlinx.serialization` + Ktor `ConvexClient`), Rust (`convex/_generated/api.rs` `struct` with `serde` + `reqwest` `Client::query`), C# (`Convex/_Generated/Api.cs` `record` with `System.Text.Json` + `HttpClient`), Dart (`lib/convex/_generated/api.dart` `class` with `json_annotation` + `http`). Client transport is HTTP/WS against the same `crates/local_backend` routes, independent of backend language; `ISOLATE_EXECUTION_ENABLED=false` (`crates/common/src/knobs.rs:949`, `docs/wasm.md:6`) still selects wasm-only deploys regardless of client language.

## Alternatives considered

- **One backend language per deployment**: restrict a deployment to a single `ModuleLanguage`; rejected — contradicts the mixed `convex/users.ts` + `convex/rust_guest.wasm` + `convex/kotlin_guest.wasm` requirement and forces monorepo splits.
- **Per-language API namespaces (`api.rust.*` vs `api.ts.*`)**: rejected — fragments the `api` surface and breaks `api.module.func` ergonomic that `convex/server` `FunctionReference` relies on (`npm-packages/convex/src/server/api.ts:24`).
- **KMP-only (Kotlin) client generation**: generate only Kotlin clients because KMP was the example; rejected — leaves Rust, C#, Dart, and TypeScript clients untyped even though server functions already expose validator JSON usable by any emitter.
- **Emitter parses source per language**: each codegen reads guest source to infer types; rejected — duplicates `function_validators` parsing and couples clients to guest SDK internals; validator JSON is the one IR.
- **OpenAPI as IR for function signatures**: reuse `crates/local_backend` `PlatformApiDoc` OpenAPI; rejected — function validators are richer than HTTP schemas (Convex `ConvexValue` types, `NoMatch` union errors) and already versioned via `ArgsValidator::json_deserialize_cached`.

## Consequences

- `docs/non-js-languages.md` owns the guest status matrix; this note owns why the IR is validator JSON and why emitters are per client target; generated files are the per-target homes and are not hand-edited.
- A TypeScript project that adds a Rust `#[query]` or Kotlin `@WasmExport` function gets its typed reference in `convex/_generated/api.ts` without changing client code; a Dart project that calls functions whose backends are Rust+TypeScript still gets `lib/convex/_generated/api.dart` via the same pipeline.
- Verification: snapshot tests per emitter (`cargo test -p model` validator round-trips, `npm-packages/convex` codegen golden files per language, `cargo test -p wasm_runner` `analyze_functions` for Rust/Go/C/Zig/Kotlin guests, `gradle build`-gated Kotlin guest), and `just generate-api-specs` for HTTP route changes.
- Disk hygiene is preserved: emitters are template-only, no per-language `cargo clean` or toolchain builds in codegen.
