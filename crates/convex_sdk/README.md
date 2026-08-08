# convex_sdk

Write Convex backend functions in Rust and execute them as sandboxed
WebAssembly. The developer experience mirrors the TypeScript `convex`
package.

## Quick start

```rust
use convex_sdk::{
    convex_functions, mutation, query, Context, ConvexError, ConvexValue, Document,
};

#[convex_functions]
pub mod functions {
    use convex_sdk::{query, mutation, Context, ConvexError, ConvexValue, Document};

    /// A query: deterministic, reads the database.
    #[query]
    pub async fn get_user(ctx: Context, id: String) -> Result<Option<Document>, ConvexError> {
        ctx.db.get("users", &id).await
    }

    /// A mutation: transactional reads and writes.
    #[mutation]
    pub async fn create_user(ctx: Context, name: String) -> Result<String, ConvexError> {
        let value = ConvexValue::from_json(serde_json::json!({ "name": name }));
        ctx.db.insert("users", value).await
    }

    /// A mutation that reads then writes.
    #[mutation]
    pub async fn bump_counter(ctx: Context) -> Result<i64, ConvexError> {
        let count = ctx.db.count("counters").await?;
        let next = count + 1;
        let value = ConvexValue::from_json(serde_json::json!({ "count": next }));
        ctx.db.insert("counters", value).await?;
        Ok(next)
    }

    /// An action: side effects allowed (not yet committed to the DB).
    #[action]
    pub async fn send_webhook(ctx: Context, url: String) -> Result<(), ConvexError> {
        ctx.log(&format!("sending webhook to {url}"));
        Ok(())
    }
}
```

## Build

```sh
cargo build --target wasm32-wasip1 --release
```

The resulting cdylib exports `__convex_run` and `__convex_functions` and is
executed by the backend's `wasm_runner` crate.

## What the macros do

- `#[convex_functions]` on a `mod` generates the module exports, the
  function registry, and `__convex_functions` descriptors.
- `#[query]` / `#[mutation]` / `#[action]` / `#[http_action]` on a function
  generate a typed argument wrapper. The first parameter may be a `Context`
  (database, `now()`, `random_bytes()`, `log()`); the remaining parameters
  are deserialized from the request arguments via `serde`. Returns may be a
  plain serializable value or `Result<T, ConvexError>`.

## Conventions & gotchas

- Object keys are returned in sorted (BTreeMap) order, matching Convex's
  canonical JSON.
- Plain JSON numbers round-trip as Float64, matching TypeScript `number`
  semantics. For exact int64 or bytes, pass/return `ConvexValue` with the
  tagged encodings (`{"$integer": ...}`, `{"$bytes": ...}`).
- Function names are the Rust function identifiers; they must match what
  clients call (`api.functions.get_user`).
- Determinism: `ctx.now()` returns the transaction timestamp, and
  `ctx.random_bytes` is seeded per invocation — retries are reproducible.
  Don't use `std::time::Instant`, `SystemTime`, or `rand` directly.

See `docs/wasm.md` for the full list of limitations.
