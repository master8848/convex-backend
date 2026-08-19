# Agent Note: Legacy audit — WASM migration incremental cleanup

Status: implemented

## Problem
WASM polyglot registry (`crates/model/src/modules/language.rs:26`, `npm-packages/convex/src/bundler/index.ts:54`, `crates/wasm_runner/src/validation.rs:114`) and sync evolution shipped, but no inventory of legacy leftovers existed. Previous steps added registries and DX docs without auditing deprecated knobs, bundler shims, and reverted commits (`c0cb7ae`).

## Decision
Run lightweight read-only audit (see `/tmp/legacy-cleanup-report.md`) and record findings without deleting code in this chunk. Five items triaged with `file_path:line_number` and disposition `keep` / `candidate for removal` / `already removed`:
- `keep` — `crates/common/src/knobs.rs:955` `ISOLATE_EXECUTION_ENABLED` at `crates/function_runner/src/server.rs:261`, `docs/wasm.md:6` plus `crates/common/src/knobs.rs:2054` `WS_PERMESSAGE_DEFLATE`, `crates/common/src/knobs.rs:2065` `SYNC_MAX_MESSAGE_SIZE`, `crates/common/src/knobs.rs:2061` `SSE_SYNC_ENABLED` via `crates/local_backend/src/subs/sse.rs:46`, `crates/common/src/http/websocket.rs:79`.
- `candidate` — `crates/model/src/config/types.rs:88` `deprecated_extract_environment_from_path` with `crates/model/src/source_packages/upload_download.rs:250`, `crates/application/src/deploy_config.rs:1304`.
- `candidate` — `crates/database/src/bootstrap_model/index.rs:293` legacy CLI comment and `crates/common/src/bootstrap_model/index/vector_index/index_config.rs:29` alpha compat.
- `candidate` — `npm-packages/convex/src/cli/lib/codegen.ts:138` `legacyCodegenPath` shim; `npm-packages/convex/src/bundler/index.ts:54` `WASM_GUEST_EXTENSIONS` mirrored at `crates/model/src/modules/language.rs:26` — keep.
- `already removed` — `crates/model/src/scheduler_cursor/` removed in `4032f5850`; revert `c0cb7ae` in ancestry; `crates/isolate/build.rs:56` `http_legacy_routes` kept as fixture, `crates/common/src/types/mod.rs:141` sentinel not debt.

## Alternatives considered
- **Delete in same chunk**: remove `deprecated_extract_environment_from_path` and legacy shims now; rejected — violates incremental policy, needs prod query guard.
- **Broader grep sweep with mass cleanup**: rejected — exceeds 10-minute budget, risks touching committed files (`crates/model/src/modules/language.rs`, `npm-packages/convex/src/cli/lib/codegenLang.ts`, `docs/wasm.md`).
- **No audit artifact**: keep findings in memory; rejected — orchestrator requires `/tmp/legacy-cleanup-report.md` as one home.

## Consequences
- `docs/wasm.md` owns runtime limits; this note owns why legacy items were kept or deferred; `/tmp/legacy-cleanup-report.md` is the one home for the full inventory.
- Deletions deferred to next chunks, each with its own `cargo check` and verification (see report Recommendation).
- Verification for this chunk: `grep` for knobs at `crates/common/src/knobs.rs:955`/`2054`/`2065`, `git log --oneline -20` (contains `4032f5850`+`c0cb7ae`), `git branch -a`, `ls -lh /tmp/legacy-cleanup-report.md`.
- No large deletion done here — incremental per task constraint.
- Budgets met: this note ~340w / ~30l (≤1000w, ≤70l), report 3784B (2–4KB); `docs/AGENTS.md:39` ceiling respected via relocation, not duplication.
- One-home-per-fact: behavior → `docs/wasm.md`, operator knobs → `crates/common/src/knobs.rs:955`, rationale → here.
- `crates/wasm_runner/src/validation.rs:114` mirrors `crates/model/src/modules/language.rs:26`; `npm-packages/convex/src/bundler/index.ts:54` is the JS mirror.
- Next step is orchestrator `git add` of this note only; `/tmp` report stays untracked as required.
- No edits to `crates/model/src/modules/language.rs`, `npm-packages/convex/src/cli/lib/codegenLang.ts`, `docs/wasm.md` per task constraint.
