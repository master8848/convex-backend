# Agent Note: Optional JS engine for wasm-only deployments

Status: implemented

## Problem

The function runner initialized V8 + ICU and the UDF runtime snapshot eagerly at startup. For deployments that only run wasm functions, that meant hundreds of MB of process RAM for machinery never used.

## Decision

The isolate client is optional. `ISOLATE_EXECUTION_ENABLED` (default `true`) or the local backend's `--disable-js-engine` flag selects the deployment mode:

- **Mixed (default)**: V8 + ICU + the UDF runtime snapshot initialize eagerly; wasm and TypeScript functions both run.
- **Wasm-only**: V8 is never initialized — no ICU data load, no UDF snapshot, no V8 platform threads, no worker isolates. Any request that needs the JS engine (TypeScript functions, module analysis, HTTP actions, schema/auth-config evaluation) fails with a clear `JavaScriptExecutionDisabled` error instead of loading V8 on demand.

Fuzz-related V8 flags (`--jit-fuzzing`, `--experimental-fuzzing`, `--randomize-hashes`) passed via `ISOLATE_V8_FLAGS` are dropped by default because they break UDF determinism; `V8_ALLOW_FUZZING_FLAGS=true` keeps them for local runtime fuzzing.

## Alternatives considered

- **Lazy V8 initialization on first JS request**: keeps the option of JS while saving idle memory, but a later JS request still needs the full V8 + ICU + snapshot load, which can take seconds at runtime; the explicit wasm-only mode makes the trade-off visible at startup instead of at the first request.
- **Always-on V8**: the wasm-only deployment would pay the memory cost for an engine it never runs.

## Consequences

- Wasm-only deployments save the full V8 footprint; the error for JS-needing requests is explicit and names the disabled engine.
- The mode switch is part of the deployment contract: operators choosing wasm-only accept that all functions must be wasm.
- Deployment modes are documented in `docs/wasm.md`.
