//! A fixture guest module exercising the convex_sdk surface, used by the
//! wasm_runner integration tests.

use convex_sdk::{
    convex_functions,
    Context,
    ConvexError,
    ConvexValue,
    Document,
};
use serde::{
    Deserialize,
    Serialize,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    name: String,
    age: i64,
}

#[convex_functions]
pub mod functions {
    use super::*;

    /// Echoes the argument back. Tests argument deserialization and result
    /// serialization.
    #[query]
    pub async fn echo(ctx: Context, value: String) -> Result<String, ConvexError> {
        ctx.log(&format!("echo called with {value}"));
        Ok(value)
    }

    /// Adds two numbers. Tests two-argument functions.
    #[query]
    pub async fn add(ctx: Context, a: i64, b: i64) -> Result<i64, ConvexError> {
        Ok(a + b + ctx.now())
    }

    /// Returns a typed struct. Tests non-scalar results.
    #[query]
    pub async fn get_user(ctx: Context, name: String) -> Result<Option<User>, ConvexError> {
        let _ = ctx;
        Ok(Some(User { name, age: 42 }))
    }

    /// A query that reads from the database.
    #[query]
    pub async fn read_user(ctx: Context, id: String) -> Result<Option<Document>, ConvexError> {
        ctx.db.get("users", &id).await
    }

    /// A mutation that writes to the database.
    #[mutation]
    pub async fn insert_user(ctx: Context, name: String) -> Result<String, ConvexError> {
        let value = ConvexValue::from_json(serde_json::json!({ "name": name }));
        ctx.db.insert("users", value).await
    }

    /// A mutation that reads then writes (tests the transaction overlay).
    #[mutation]
    pub async fn bump(ctx: Context) -> Result<i64, ConvexError> {
        let count = ctx.db.count("counters").await?;
        let next = count + 1;
        let value = ConvexValue::from_json(serde_json::json!({ "count": next }));
        ctx.db.insert("counters", value).await?;
        Ok(next)
    }

    /// A query over a whole table.
    #[query]
    pub async fn list_users(ctx: Context) -> Result<Vec<Document>, ConvexError> {
        ctx.db.query("users").await
    }

    /// A function that raises an error.
    #[query]
    pub async fn fail(ctx: Context) -> Result<(), ConvexError> {
        let _ = ctx;
        Err(ConvexError::with_code("boom", "Boom"))
    }

    /// A function that exercises randomness.
    #[query]
    pub async fn random(ctx: Context) -> Result<Vec<i64>, ConvexError> {
        let bytes = ctx.random_bytes(8);
        Ok(bytes.into_iter().map(i64::from).collect())
    }

    /// A sync function.
    #[query]
    pub fn double(ctx: Context, value: i64) -> Result<i64, ConvexError> {
        let _ = ctx;
        Ok(value * 2)
    }
}
