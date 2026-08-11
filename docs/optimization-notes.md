# Optimization Notes

This file documents performance optimizations landed in this repository. Each
entry describes the change, the reasoning, the files touched, and how it was
validated.

---

## 1. Parse internal JSON directly into `ConvexValue` (skip the `serde_json::Value` tree)

**Status: landed.**

### Before

`value::json_deserialize` / `value::json_deserialize_bytes` (and the SQLite /
Postgres / MySQL document read paths) parsed the internal JSON encoding of a
`ConvexValue` in two stages:

1. `serde_json::from_str::<serde_json::Value>` / `from_slice` materialized a
   full `serde_json::Value` tree (one heap node per scalar, plus per-key
   `String`s and per-object `BTreeMap`/`Vec` containers).
2. `ConvexValue::try_from(JsonValue)` converted that tree into a second,
   entirely separate `ConvexValue` tree (another set of allocations, plus a
   second parse of every key into a `FieldName`).

For a document with N nodes this allocates roughly 2x the nodes and 2x the
key strings that are strictly necessary.

### After

`json_deserialize` / `json_deserialize_bytes` now feed the JSON text through a
dedicated serde `Visitor` (`InternalJsonValueVisitor` / `InternalJsonValueSeed`
in `crates/value/src/json/mod.rs`) that builds the `ConvexValue` tree directly
from `serde_json::Deserializer`, never materializing the intermediate
`serde_json::Value`. The visitor mirrors the old `TryFrom<JsonValue>` semantics
exactly:

- JSON numbers map to `Float64` (`visit_i64`/`visit_u64`/`visit_f64` produce
  the same values as `serde_json::Number::as_f64` did).
- Single-key objects are decoded as the `$integer` / `$bytes` / `$float`
  wrappers, with the same validation (`$float` values that fit in a plain
  number are rejected; invalid base64 is rejected; non-string payloads are
  rejected).
- Duplicate keys collapse last-wins (matching `serde_json::Map` under
  `preserve_order`), and keys that aren't valid `FieldName`s (e.g. `$`-prefixed
  keys in multi-key objects) are rejected exactly as before.
- Trailing garbage is still rejected via `Deserializer::end()`, and the same
  `serde_json` parser (including its recursion behavior) is used, so syntax
  errors and error ordering are unchanged for all valid inputs.

The storage backends now use the fast path directly:
- `crates/sqlite/src/lib.rs` — `row_to_document` and `_index_scan_inner`
  decode document JSON with `json_deserialize`.
- `crates/postgres/src/lib.rs` — `row_to_document`, previous-revision loads,
  and index scans decode with `json_deserialize_bytes`.
- `crates/mysql/src/document_encoding.rs` already called
  `value::json_deserialize_bytes`, so it picks up the improvement for free.

### Files

- `crates/value/src/json/mod.rs` — direct-deserialization visitor + unit tests.
- `crates/sqlite/src/lib.rs`, `crates/postgres/src/lib.rs` — storage read paths.

### Validation

- `cargo check -p value`, `cargo check -p sqlite`, `cargo check -p postgres`,
  `cargo check -p mysql`, `cargo check -p sync -p local_backend` — all pass.
- `cargo test -p value` — 5 new tests pass (round-trips through both the str
  and bytes paths, parity with `ConvexValue::try_from(JsonValue)` for wrapper
  and plain cases, error cases, trailing-garbage rejection).
- `cargo test -p sqlite`, `cargo test -p postgres`, `cargo test -p udf`,
  `cargo test -p database` — pass.
- Existing criterion bench `cargo bench -p value --bench json` (same machine,
  release build):

  | benchmark | before | after | speedup |
  |---|---|---|---|
  | `from_json::simple` | 1.380 µs | 1.071 µs | ~22% |
  | `from_json::nested` | 12.81 µs | 9.56 µs | ~25% |

  (`from_json_value::*`, which measures the unchanged
  `ConvexValue::try_from(JsonValue)` path, was flat at ~727 ns / ~11.0 µs,
  confirming the win comes from removing the intermediate tree.)

---

## 2. Memoize args/returns validator parsing in the `validateArgs`/`validateReturns` ops

**Status: landed.**

### Before

The JS runtime wraps every system UDF (`_system/frontend/*`, `_system/cli/*`,
scheduler helpers) with an args/returns validation wrapper that, on **every
invocation**, calls the `validateArgs` / `validateReturns` v8 ops and passes
the module's `exportArgs()` / `exportReturns()` JSON string. The ops
(`crates/isolate/src/ops/validate_args.rs`, `validate_returns.rs`) re-parsed
that validator JSON from scratch on each call, rebuilding the
`ArgsValidator` / `ReturnsValidator` tree every time — even though the JSON is
identical across calls and deployments.

### After

`ArgsValidator::json_deserialize_cached` and
`ReturnsValidator::json_deserialize_cached`
(`crates/model/src/modules/function_validators.rs`) memoize the parse with a
bounded per-process LRU cache keyed on the validator JSON string (capacity
1000, same pattern as the existing validator cache in
`crates/model/src/modules/module_versions.rs`). Cache hits return a cheap
`Arc`-backed clone instead of re-allocating the validator tree. Failed parses
are **not** cached, so the error path is byte-for-byte identical to a direct
parse. This complements the earlier `AnalyzedFunction::args()` cache: the Rust
-side request validation and the in-isolate op validation now both avoid
re-parsing stable validator JSON.

### Files

- `crates/model/src/modules/function_validators.rs` — LRU caches +
  `json_deserialize_cached` methods + 4 unit tests.
- `crates/isolate/src/ops/validate_args.rs`,
  `crates/isolate/src/ops/validate_returns.rs` — call the cached variants.

### Validation

- `cargo check -p model`, `cargo check -p isolate` — pass.
- `cargo test -p model function_validators` — 4 new tests pass: cached parses
  equal direct parses, repeated cached parses are stable, failed parses don't
  poison the cache, and `"any"`/`null` validators map to the `Unvalidated`
  variants.
- `cargo test -p model`, `cargo test -p udf` — full suites pass.

---

## Notes for future work

- The remaining `serde_json::Value`-mediated conversions (e.g.
  `JsonPackedValue::unpack` on the sync/cache paths, `PendingValue::from_uncommitted_json`)
  could adopt the same direct-deserialization approach if profiling shows they
  matter; they currently sit off the hottest per-document paths.
- `AnalyzedFunction::args()`/`returns()` clone the cached `Arc` validator on
  each call; returning `Arc<ArgsValidator>` from the cache would remove that
  clone too, at the cost of a small API change.
