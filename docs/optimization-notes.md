# Optimization Notes

Reference for the hot-path performance optimizations in this fork: each entry names the shipped mechanism, the files it lives in, and its measured results. Per-change rationale, alternatives considered, and validation details are recorded in the [hot-path decode and validation speedups note](../.agents/notes/implemented/performance/2026-08-11-hot-path-decode-and-validation-speedups.md).

## 1. Direct JSON → ConvexValue deserialization

`value::json_deserialize` / `value::json_deserialize_bytes` parse the internal JSON encoding of a `ConvexValue` straight from `serde_json::Deserializer` through the dedicated serde visitors `InternalJsonValueVisitor` / `InternalJsonValueSeed` in `crates/value/src/json/mod.rs`, building the `ConvexValue` tree directly and never materializing an intermediate `serde_json::Value` tree.

The visitor produces the same results as `ConvexValue::try_from(JsonValue)`:

- JSON numbers map to `Float64` (`visit_i64` / `visit_u64` / `visit_f64` produce the same values as `serde_json::Number::as_f64`).
- Single-key objects decode as the `$integer` / `$bytes` / `$float` wrappers with the same validation: `$float` values that fit in a plain number are rejected, invalid base64 is rejected, non-string payloads are rejected.
- Duplicate keys collapse last-wins (matching `serde_json::Map` under `preserve_order`); keys that are not valid `FieldName`s (e.g. `$`-prefixed keys in multi-key objects) are rejected.
- Trailing garbage is rejected via `Deserializer::end()`; the same `serde_json` parser (including its recursion behavior) is used, so syntax errors and error ordering are unchanged for all valid inputs.

The storage read paths use the fast path:

- `crates/sqlite/src/lib.rs` — `row_to_document` and `_index_scan_inner` decode document JSON with `json_deserialize`.
- `crates/postgres/src/lib.rs` — `row_to_document`, previous-revision loads, and index scans decode with `json_deserialize_bytes`.
- `crates/mysql/src/document_encoding.rs` — calls `value::json_deserialize_bytes`.

Measured with the existing criterion bench (`cargo bench -p value --bench json`, release build):

| benchmark | before | after | speedup |
|---|---|---|---|
| `from_json::simple` | 1.380 µs | 1.071 µs | ~22% |
| `from_json::nested` | 12.81 µs | 9.56 µs | ~25% |

The `from_json_value::*` benchmarks measure the unchanged `ConvexValue::try_from(JsonValue)` path and are flat at ~727 ns / ~11.0 µs, confirming the win comes from removing the intermediate tree.

## 2. Memoized validator parsing in the validateArgs/validateReturns ops

The `validateArgs` / `validateReturns` v8 ops (`crates/isolate/src/ops/validate_args.rs`, `crates/isolate/src/ops/validate_returns.rs`) receive the module's `exportArgs()` / `exportReturns()` JSON string on every invocation of a wrapped system UDF (`_system/frontend/*`, `_system/cli/*`, scheduler helpers). `ArgsValidator::json_deserialize_cached` and `ReturnsValidator::json_deserialize_cached` in `crates/model/src/modules/function_validators.rs` memoize the parse in a bounded per-process LRU cache keyed on the validator JSON string (capacity 1000, same pattern as the existing validator cache in `crates/model/src/modules/module_versions.rs`). Cache hits return a cheap `Arc`-backed clone instead of re-allocating the validator tree; failed parses are not cached, so the error path is identical to a direct parse. The cache complements the `AnalyzedFunction::args()` cache: Rust-side request validation and in-isolate op validation both avoid re-parsing stable validator JSON.

## 3. Schema validation speedups

Schema validation in `crates/common/src/schemas/` avoids re-parse and redundant allocation on hot paths:

- `ValidationContext` (`crates/common/src/schemas/mod.rs`) renders error-path segments as a lazily-rendered linked list instead of eagerly-built strings; successful validations never allocate error-path strings.
- `ObjectValidator::check_object` / `ObjectValidator::check_value` (`crates/common/src/schemas/validator.rs`) validate against an object validator without cloning it into a `Validator::Object` wrapper.
- Union member dispatch uses `shared_literal_discriminator`: when members share a literal discriminator field, the member is selected by that field directly, skipping per-member checks for the common discriminated-union shape; the first missing/extra field error from a union member is surfaced instead of a generic `NoMatch`.
- `Validator::filter_system_fields` skips cloning and filtering objects that have no validator for system fields (`has_validator_for_system_field`).

## 4. SQL filters skip deep-cloning and fold constant expressions

SQL filters evaluate against the document by reference: `Filter::next` (`crates/database/src/query/filter.rs`) does not deep-clone the document before evaluating the filter expression. `Expression::fold_constants` (`crates/common/src/query.rs`), applied at `Filter` construction, memoizes field-free subtrees — bare literals, literal arithmetic, comparisons of literals — into `Literal` nodes once, so they are not re-evaluated per row; subtrees that fail to evaluate stay unfolded so errors are still reported at eval time. SQLite prepares statements with `prepare_cached` instead of recompiling them per query.

## 5. Request-lifecycle correct patterns

The request pipeline `router/auth/dispatch/transaction/WriteLog/re-execution/sync` uses the correct data structure at each stage; rationale and rejected alternatives are recorded in the [request lifecycle perf note](../.agents/notes/implemented/performance/2026-08-19-request-lifecycle-perf-and-correct-patterns.md).

- **Dispatch**: `ISOLATE_EXECUTION_ENABLED=false` never initializes V8 (`crates/common/src/knobs.rs:949`); the Wasm runner caps the WasmGC heap to 64 MiB reservation + 32 MiB growth (`crates/wasm_runner/src/engine.rs:92`) and AOT-caches compiled modules via `Module::serialize`/`deserialize` keyed by sha256 (`crates/wasm_runner/src/engine.rs:169`); per-environment execution is bounded by a 64-permit semaphore (`crates/wasm_runner/src/engine.rs:85` `MAX_CONCURRENT_EXECUTIONS_PER_ENV`, `function_runner/src/server.rs:423`) mirroring `crates/isolate/src/concurrency_limiter.rs:109`.
- **Read path**: `Transaction { reads: TransactionReadSet, writes }` (`crates/database/src/transaction.rs:162`) records `ReadSet` per `TabletIndexName`; `SubscriptionManager` (`crates/database/src/subscription.rs:587`) stores `IntervalMap` per table with dedup `HashMap<DedupKey, AtomicUsize>` using `ahash` and coalesces adjacent `IntervalSet` on `record_indexed_directly`; `advance_log` advances a watermark incrementally instead of scanning the full log.
- **Sync**: `StateModification::QueryUpdated` (`crates/convex/sync_types/src/types/mod.rs:341`) is emitted as RFC 6902 JSON-Patch (`crates/sync/src/patch.rs` `maybe_patch`, `is_patch_worth_it` 0.8 threshold, `MIN_PATCHABLE_SIZE` 1024) only when `patch < 0.8 * value`; otherwise the full `JsonPackedValue` is sent. `TransitionChunk` carries zero-copy `Bytes` and both WS (`crates/local_backend/src/subs/mod.rs:376`) and SSE (`crates/local_backend/src/subs/sse.rs:1`) share `SYNC_MAX_MESSAGE_SIZE` 5 MiB (`crates/common/src/knobs.rs:2041`) for chunking.

## Remaining serde-mediated conversions

- `JsonPackedValue::unpack` on the sync/cache paths and `PendingValue::from_uncommitted_json` still convert through an intermediate `serde_json::Value`; they sit off the hottest per-document paths.
- `AnalyzedFunction::args()` / `returns()` clone the cached `Arc` validator on each call.
