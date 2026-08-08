//! WASM UDF execution engine for Convex backend functions.
//!
//! This crate executes user functions compiled to `wasm32-wasip1` in a
//! sandboxed [`wasmtime`] runtime, as an alternative to the V8 isolate-based
//! execution path for TypeScript functions.
//!
//! # ABI
//!
//! Guest modules export a single dispatcher function `__convex_run() -> i32`.
//! The input is provided through host functions rather than function
//! parameters, following the "host-alloc" pattern proven by Extism across
//! many guest languages (Rust, Go, C, ...). The guest pulls input from the
//! host, allocates output buffers in host-managed memory, and reports its
//! result through host functions. See [`crate::abi`] for the complete
//! contract.
//!
//! # Determinism
//!
//! Queries and mutations receive a seeded ChaCha12 RNG and a virtual
//! timestamp, mirroring the isolate execution path. WASI's clocks and
//! `random_get` are overridden so guests cannot observe wall time or OS
//! entropy.

mod abi;
mod db;
mod determinism;
mod engine;
mod limits;
mod validation;

use anyhow::Context;
use common::{
    components::ComponentId,
    errors::JsError,
    knobs::FUNCTION_MAX_RESULT_SIZE,
    log_lines::{
        LogLine,
        LogLines,
    },
    runtime::{
        Runtime,
        UnixTimestamp,
    },
};
use database::Transaction;
use tokio::sync::mpsc;
use value::{
    JsonPackedValue,
    PendingValue,
    Size,
};
pub use abi::*;
pub use engine::{
    analyze_functions,
    WasmFunctionDescriptor,
    WasmRunner,
};
pub use limits::WasmLimits;
pub use validation::validate_module;

/// The result of a single WASM UDF execution.
#[derive(Debug)]
pub struct WasmUdfResult {
    /// The function's return value.
    pub result: Result<JsonPackedValue<PendingValue>, JsError>,
    /// Log lines emitted by the function.
    pub log_lines: LogLines,
    /// Whether the function requested random bytes.
    pub observed_rng: bool,
    /// Whether the function requested the current time.
    pub observed_time: bool,
}

/// The input payload handed to a guest module: which function to run and its
/// arguments (as a Convex JSON array).
#[derive(Debug, Clone)]
pub struct WasmInput {
    /// The resolved function name, e.g. `"users:list"`.
    pub function_name: String,
    /// The arguments as a Convex JSON array string.
    pub args_json: String,
}

/// Run a WASM UDF against a transaction.
///
/// The `transaction` is handed to the guest through host functions that
/// mirror the isolate's database syscalls; all reads and writes are recorded
/// in it and returned to the caller for commit or abort.
///
/// `allow_unresolved_commit_ts` must be set for mutations, which may return
/// a `$commitTs` token that is resolved at commit time; queries and actions
/// must pass `false`.
pub async fn run_wasm_udf<RT: Runtime>(
    runner: &WasmRunner,
    module_binary: &[u8],
    input: WasmInput,
    tx: Transaction<RT>,
    component: ComponentId,
    rng_seed: [u8; 32],
    unix_timestamp: UnixTimestamp,
    limits: WasmLimits,
    allow_unresolved_commit_ts: bool,
    log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
) -> anyhow::Result<(Transaction<RT>, WasmUdfResult)> {
    let module = runner.get_or_compile_module(module_binary, &limits)?;
    let input_json = format!(
        r#"{{"{}": {}, "{}": {}}}"#,
        INPUT_FUNCTION_KEY,
        serde_json::to_string(&input.function_name)?,
        INPUT_ARGS_KEY,
        input.args_json,
    );
    let input_bytes = input_json.into_bytes();

    let (tx, execution) = engine::execute_module(
        runner,
        &module,
        &input_bytes,
        tx,
        component,
        rng_seed,
        unix_timestamp,
        limits,
        log_line_sender,
    )
    .await?;

    let result = match (execution.output, execution.error) {
        (Some(output), _) => {
            let result_str = String::from_utf8_lossy(&output);
            deserialize_result(&result_str, allow_unresolved_commit_ts)
                .map(JsonPackedValue::pack)
                .map_err(|e| JsError::from_message(e.to_string()))
        },
        (None, Some(error)) => {
            let message = String::from_utf8_lossy(&error).to_string();
            let message = if message.is_empty() {
                "WASM function returned an error".to_string()
            } else {
                message
            };
            Err(JsError::from_message(message))
        },
        (None, None) => Err(JsError::from_message(
            "WASM function returned no result".to_string(),
        )),
    };

    Ok((
        tx,
        WasmUdfResult {
            result,
            log_lines: LogLines::from(execution.log_lines),
            observed_rng: execution.observed_rng,
            observed_time: execution.observed_time,
        },
    ))
}

/// Parse a guest return value as Convex JSON into a [`PendingValue`],
/// enforcing the result size limit.
fn deserialize_result(
    result_str: &str,
    allow_unresolved_commit_ts: bool,
) -> anyhow::Result<PendingValue> {
    let result_v: serde_json::Value = serde_json::from_str(result_str)
        .context("Function return value is not valid JSON")?;
    let value = PendingValue::from_uncommitted_json(result_v)
        .context("Function return value is not a valid Convex value")?;
    if !allow_unresolved_commit_ts && value.is_pending() {
        anyhow::bail!("Function return value contains an unresolved commit timestamp");
    }
    let size = value.size();
    let limit = *FUNCTION_MAX_RESULT_SIZE;
    anyhow::ensure!(
        size <= limit,
        "Function return value is too large (actual: {size}, limit: {limit})",
    );
    Ok(value)
}
