//! Runtime support for Convex WASM guest modules: host function imports and
//! the dispatcher glue.
//!
//! This module is compiled into guest modules and only exists on
//! `wasm32`; on other targets the host functions are unavailable and most
//! functions will fail to link.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use serde_json::Value as JsonValue;

// Namespace shared with the backend's wasm_runner host functions.
#[cfg(target_arch = "wasm32")]
const HOST_FN_MODULE: &str = "env";

// Host function names (must match wasm_runner/src/abi.rs). Only used by the
// wasm32 implementation; on other targets they are dead.
pub(crate) const INPUT_LENGTH: &str = "__convex_input_length";
pub(crate) const INPUT_LOAD: &str = "__convex_input_load";
pub(crate) const CALL_DATA_LOAD: &str = "__convex_call_data_load";
pub(crate) const OUTPUT_SET: &str = "__convex_output_set";
pub(crate) const ERROR_SET: &str = "__convex_error_set";
pub(crate) const LOG: &str = "__convex_log";
pub(crate) const NOW_MS: &str = "__convex_now_ms";
pub(crate) const RANDOM_BYTES: &str = "__convex_random_bytes";
pub(crate) const DB_GET: &str = "__convex_db_get";
pub(crate) const DB_INSERT: &str = "__convex_db_insert";
pub(crate) const DB_REPLACE: &str = "__convex_db_replace";
pub(crate) const DB_PATCH: &str = "__convex_db_patch";
pub(crate) const DB_DELETE: &str = "__convex_db_delete";
pub(crate) const DB_COUNT: &str = "__convex_db_count";
pub(crate) const DB_QUERY: &str = "__convex_db_query";

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn __convex_input_length() -> i32;
    fn __convex_input_load(offset: i32, dest: i32, len: i32);
    fn __convex_call_data_load(offset: i32, dest: i32, len: i32);
    fn __convex_output_set(ptr: i32, len: i32);
    fn __convex_error_set(ptr: i32, len: i32);
    fn __convex_log(ptr: i32, len: i32);
    fn __convex_now_ms() -> i64;
    fn __convex_random_bytes(dest: i32, len: i32);
    fn __convex_db_get(ptr: i32, len: i32) -> i64;
    fn __convex_db_insert(ptr: i32, len: i32) -> i64;
    fn __convex_db_replace(ptr: i32, len: i32) -> i64;
    fn __convex_db_patch(ptr: i32, len: i32) -> i64;
    fn __convex_db_delete(ptr: i32, len: i32) -> i64;
    fn __convex_db_count(ptr: i32, len: i32) -> i64;
    fn __convex_db_query(ptr: i32, len: i32) -> i64;
}

/// A wrapped function: takes the function's arguments (as Convex JSON array
/// elements) and returns the result as JSON, or an error message.
pub type WrappedFn = fn(&[JsonValue]) -> Result<JsonValue, String>;

/// A function descriptor returned by `__convex_functions`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FunctionDescriptor {
    /// The function name, matching the name used in queries/mutations.
    pub name: &'static str,
    /// "query", "mutation", "action", or "httpAction".
    #[serde(rename = "type")]
    pub function_type: &'static str,
    /// JSON-serialized ConvexValidator for args (object validator). None = unvalidated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<&'static str>,
    /// JSON-serialized ConvexValidator for returns. None = unvalidated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returns: Option<&'static str>,
    /// "public" or "internal". None defaults to public.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<&'static str>,
}

/// Run a wrapped function inside the generated `__convex_run` export.
///
/// Reads the input payload `{"function": string, "args": array}`, dispatches
/// to the matching function, and writes the result via `__convex_output_set`.
#[cfg(target_arch = "wasm32")]
pub fn dispatch(functions: &[(&str, WrappedFn)]) -> i32 {
    let input = match read_input() {
        Ok(input) => input,
        Err(message) => {
            write_error(&message);
            return 1;
        },
    };
    let input: JsonValue = match serde_json::from_slice(&input) {
        Ok(input) => input,
        Err(e) => {
            write_error(&format!("Invalid input payload: {e}"));
            return 1;
        },
    };
    let Some(name) = input.get("function").and_then(JsonValue::as_str) else {
        write_error("Input payload is missing the \"function\" field");
        return 1;
    };
    let args: Vec<JsonValue> = match input.get("args") {
        Some(JsonValue::Array(args)) => args.clone(),
        Some(_) => {
            write_error("Input payload's \"args\" field must be an array");
            return 1;
        },
        None => Vec::new(),
    };
    let Some((_, wrapped)) = functions.iter().find(|(candidate, _)| *candidate == name) else {
        write_error(&format!("Function {name} not found in this module"));
        return 1;
    };
    match wrapped(&args) {
        Ok(value) => {
            write_output(&value);
            0
        },
        Err(message) => {
            write_error(&message);
            1
        },
    }
}

/// Return the function descriptors via `__convex_output_set`, for the
/// `__convex_functions` export.
#[cfg(target_arch = "wasm32")]
pub fn functions_output(descriptors: &[FunctionDescriptor]) -> i32 {
    match serde_json::to_value(descriptors) {
        Ok(value) => {
            write_output(&value);
            0
        },
        Err(e) => {
            write_error(&format!("Failed to serialize function descriptors: {e}"));
            1
        },
    }
}

/// Block on an async function. Database operations are synchronous host
/// calls from the guest's perspective, so a simple poller is sufficient.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    pollster::block_on(future)
}

/// Read the full input payload into guest memory.
#[cfg(target_arch = "wasm32")]
fn read_input() -> Result<Vec<u8>, String> {
    let len = input_length();
    if len <= 0 {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u8; len as usize];
    input_load(0, buffer.as_mut_ptr() as i32, len);
    Ok(buffer)
}

/// Copy `len` bytes of host call data (a database result) into guest memory.
#[cfg(target_arch = "wasm32")]
fn read_call_data(offset: u32, len: u32) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u8; len as usize];
    call_data_load(offset as i32, buffer.as_mut_ptr() as i32, len as i32);
    Ok(buffer)
}

/// Write a JSON value as the function result.
#[cfg(target_arch = "wasm32")]
fn write_output(value: &JsonValue) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        output_set(bytes.as_ptr() as i32, bytes.len() as i32);
    }
}

/// Write an error message.
#[cfg(target_arch = "wasm32")]
fn write_error(message: &str) {
    let bytes = message.as_bytes();
    error_set(bytes.as_ptr() as i32, bytes.len() as i32);
}

/// Call a database host function with a JSON argument object, returning the
/// `{"ok": <value>}` / `{"err": <message>}` envelope's value.
#[cfg(target_arch = "wasm32")]
pub fn db_call(name: &str, args: &JsonValue) -> Result<JsonValue, ConvexDbError> {
    let args = serde_json::to_vec(args)
        .map_err(|e| ConvexDbError::system(format!("Failed to serialize arguments: {e}")))?;
    let result = match name {
        DB_GET => unsafe { __convex_db_get(args.as_ptr() as i32, args.len() as i32) },
        DB_INSERT => unsafe { __convex_db_insert(args.as_ptr() as i32, args.len() as i32) },
        DB_REPLACE => unsafe { __convex_db_replace(args.as_ptr() as i32, args.len() as i32) },
        DB_PATCH => unsafe { __convex_db_patch(args.as_ptr() as i32, args.len() as i32) },
        DB_DELETE => unsafe { __convex_db_delete(args.as_ptr() as i32, args.len() as i32) },
        DB_COUNT => unsafe { __convex_db_count(args.as_ptr() as i32, args.len() as i32) },
        DB_QUERY => unsafe { __convex_db_query(args.as_ptr() as i32, args.len() as i32) },
        _ => {
            return Err(ConvexDbError::system(format!(
                "Unknown database operation {name}"
            )))
        },
    };
    if result < 0 {
        return Err(ConvexDbError::system(
            "Database operation failed".to_string(),
        ));
    }
    let offset = (result >> 32) as u32;
    let len = result as u32;
    let bytes = read_call_data(offset, len)
        .map_err(|e| ConvexDbError::system(format!("Failed to read result: {e}")))?;
    let envelope: JsonValue = serde_json::from_slice(&bytes)
        .map_err(|e| ConvexDbError::system(format!("Failed to parse result: {e}")))?;
    match envelope.get("ok") {
        Some(value) => Ok(value.clone()),
        None => {
            let message = envelope
                .get("err")
                .and_then(JsonValue::as_str)
                .unwrap_or("Database operation failed")
                .to_string();
            Err(ConvexDbError::User(message))
        },
    }
}

/// Non-wasm stub for `db_call`, used only so the SDK compiles on the host.
#[cfg(not(target_arch = "wasm32"))]
pub fn db_call(_name: &str, _args: &JsonValue) -> Result<JsonValue, ConvexDbError> {
    Err(ConvexDbError::System(
        "Database operations are only available on wasm32".to_string(),
    ))
}

/// The result of a database host function call.
#[derive(Debug)]
pub enum ConvexDbError {
    /// A user-facing error message from the database.
    User(String),
    /// A system error (protocol failure, not the function's fault).
    System(String),
}

impl std::fmt::Display for ConvexDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvexDbError::User(message) => f.write_str(message),
            ConvexDbError::System(message) => write!(f, "System error: {message}"),
        }
    }
}

impl ConvexDbError {
    #[cfg(target_arch = "wasm32")]
    fn system(message: String) -> Self {
        ConvexDbError::System(message)
    }
}

/// The current virtual time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        unsafe { __convex_now_ms() }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        testing::now_ms()
    }
}

/// Deterministic random bytes.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; len];
    #[cfg(target_arch = "wasm32")]
    unsafe {
        __convex_random_bytes(buffer.as_mut_ptr() as i32, buffer.len() as i32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    testing::random_bytes(&mut buffer);
    buffer
}

/// Emit a log line.
pub fn log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        let bytes = message.as_bytes();
        __convex_log(bytes.as_ptr() as i32, bytes.len() as i32);
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = message;
}

#[cfg(target_arch = "wasm32")]
fn input_length() -> i32 {
    unsafe { __convex_input_length() }
}

#[cfg(target_arch = "wasm32")]
fn input_load(offset: i32, dest: i32, len: i32) {
    unsafe { __convex_input_load(offset, dest, len) }
}

#[cfg(target_arch = "wasm32")]
fn call_data_load(offset: i32, dest: i32, len: i32) {
    unsafe { __convex_call_data_load(offset, dest, len) }
}

#[cfg(target_arch = "wasm32")]
fn output_set(ptr: i32, len: i32) {
    unsafe { __convex_output_set(ptr, len) }
}

#[cfg(target_arch = "wasm32")]
fn error_set(ptr: i32, len: i32) {
    unsafe { __convex_error_set(ptr, len) }
}

/// Host-function stubs for non-wasm builds, used only in unit tests.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod testing {
    use std::sync::atomic::{
        AtomicU64,
        AtomicU8,
        Ordering,
    };

    static NOW_MS: AtomicU64 = AtomicU64::new(0);
    static SEED: AtomicU8 = AtomicU8::new(0);

    pub(crate) fn now_ms() -> i64 {
        NOW_MS.load(Ordering::SeqCst) as i64
    }

    pub(crate) fn random_bytes(buffer: &mut [u8]) {
        let mut seed = SEED.load(Ordering::SeqCst);
        for byte in buffer {
            seed = seed.wrapping_add(7);
            *byte = seed;
        }
    }
}
