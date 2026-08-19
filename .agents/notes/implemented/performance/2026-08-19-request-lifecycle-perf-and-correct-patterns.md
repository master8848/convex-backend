# Agent Note: Request lifecycle perf and correct patterns

Status: implemented

## Problem

Request stages hide architecture debt that hurts latency and RSS at scale. Hot paths use wrong data structures (full `QueryUpdated` values, `DefaultHasher` dedup, `String` chunk copies, `256MiB` GC reservation per `Store`, per-call recompilation) rather than missing micro-opts. `docs/optimization-notes.md:1` held speculative lists without stage ownership.

## Decision

Profile pipeline `router/auth/dispatch/transaction/WriteLog/re-execution/sync` and fix with correct patterns:

- **Dispatch**: `ISOLATE_EXECUTION_ENABLED=false` skips V8 load (`icudtl.dat` 10.8MiB+snapshots, `64+32+64MiB` per isolate) for wasm-only envs; `WasmRunner` `GC_HEAP_RESERVATION 64MiB+32MiB growth` (`engine.rs:92`) vs 256MiB, `Module::serialize` AOT cache (`engine.rs:169`) with per-env semaphore `64` mirrors `isolate/concurrency_limiter.rs:109`. Wire `is_wasm_only_env` auto-detect and env-scoped `execution_semaphore_for_env` acquisition in `function_runner/server.rs:423`.
- **Read path**: `Transaction {reads:TransactionReadSet,writes}` (`database/transaction.rs:162`) → `IntervalMap` per `TabletIndexName` (`subscription.rs:587`) with dedup `HashMap<DedupKey, AtomicUsize>` using `ahash` not `DefaultHasher`, coalesce adjacent `IntervalSet` on `record_indexed_directly:1072`, incremental `advance_log:725` watermark vs full scan.
- **Sync**: replace full-value `StateModification::QueryUpdated {value:JsonPackedValue,journal}` (`sync_types/mod.rs:341`) with threshold patch (`crates/sync/src/patch.rs` RFC6902 JSON-Patch when `patch<0.8*value`), `TransitionChunk` zero-copy `Bytes`, shared `SYNC_MAX_MESSAGE_SIZE 5MiB` (`knobs.rs:2041`) for WS `permessage-deflate` and SSE `text/event-stream` via `subs/mod.rs:376` + `subs/sse.rs:1`.

## Alternatives considered

- Micro-optimizing `JsonPackedValue` packing alone — rejected, gains <5% while full-value fanout dominates.
- Keep `DefaultHasher` for DoS resistance — rejected, non-adversarial dedup needs `ahash/fxhash`.
- Keep per-Store 256MiB GC — rejected, Kotlin WasmGC <10MiB, C/Zig/Rust/Go zero GC heap.

## Consequences

- `docs/wasm.md` owns wasm caps/AOT, `docs/optimization-notes.md` owns stage budgets, this note owns why patterns changed; `crates/value/src/heap_size.rs:10` tracks heap.
- Verification: `cargo test -p database --lib reads`, `cargo check -p wasm_runner`, `cargo check -p sync_types`, hygiene `./scripts/disk-hygiene.sh` (target cap 15Gi, free>20Gi).
- DX unchanged for devs/AI: patterns are internal, no API shift.
