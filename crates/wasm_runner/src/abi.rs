//! The host-guest ABI contract for Convex WASM functions.
//!
//! The ABI follows the "host-alloc" pattern proven by Extism for multi-language
//! support (Rust, Go, C, ...). The host owns all memory crossing the boundary;
//! guests never touch host memory directly and never export an allocator, so
//! any language that can import functions and export one `i32`-returning
//! function can implement the guest side.
//!
//! # Guest exports
//!
//! - `__convex_run() -> i32`: the dispatcher. The input payload is pulled via
//!   `__convex_input_*` host functions. Returns `0` on success or a non-zero
//!   status; on error the guest should also call [`ERROR_SET`] with details.
//! - `__convex_functions() -> i32`: returns, via [`OUTPUT_SET`], a JSON array
//!   of function descriptors `{"name": string, "type":
//!   "query"|"mutation"|"action"|"httpAction"}`. Used by the module analyzer.
//!
//! # Host functions (imported under module `env`)
//!
//! Input handling (the input is a JSON object `{"function": string, "args":
//! array}`):
//! - `__convex_input_length() -> i32`
//! - `__convex_input_load(offset: i32, dest: i32, len: i32)`
//!
//! Host-managed scratch memory ("call data"), allocated and read by the guest:
//! - `__convex_alloc(len: i32) -> i32` (offset into call data)
//! - `__convex_call_data_load(offset: i32, dest: i32, len: i32)`
//!
//! Result reporting (the output/error buffers live in guest memory; the host
//! copies them out when set):
//! - `__convex_output_set(ptr: i32, len: i32)` (guest memory)
//! - `__convex_error_set(ptr: i32, len: i32)` (guest memory)
//!
//! Utilities:
//! - `__convex_log(ptr: i32, len: i32)`
//! - `__convex_now_ms() -> i64` (virtual, deterministic time)
//! - `__convex_random_bytes(dest: i32, len: i32)` (seeded, deterministic RNG)
//!
//! Database operations. Each takes `(args_ptr: i32, args_len: i32)` where the
//! args are a JSON object, and returns an `i64`: a packed `(offset, len)` pair
//! into call data holding a JSON envelope `{"ok": <value>}` or `{"err":
//! <message>}`, or `-1` on system error:
//! - `__convex_db_get`, `__convex_db_insert`, `__convex_db_replace`,
//!   `__convex_db_patch`, `__convex_db_delete`, `__convex_db_count`,
//!   `__convex_db_query`
//!
//! # Result encoding
//!
//! Database host functions return `(offset << 32) | len` in the low 32 bits.

/// The import module namespace for all Convex host functions.
pub const HOST_FN_MODULE: &str = "env";

/// The guest's main dispatcher export.
pub const GUEST_RUN: &str = "__convex_run";
/// The guest export returning the list of functions in the module.
pub const GUEST_FUNCTIONS: &str = "__convex_functions";

/// Keys of the input JSON payload.
pub const INPUT_FUNCTION_KEY: &str = "function";
pub const INPUT_ARGS_KEY: &str = "args";

/// Input host functions.
pub const INPUT_LENGTH: &str = "__convex_input_length";
pub const INPUT_LOAD: &str = "__convex_input_load";

/// Call-data host functions.
pub const CALL_DATA_ALLOC: &str = "__convex_alloc";
pub const CALL_DATA_LOAD: &str = "__convex_call_data_load";

/// Result reporting host functions.
pub const OUTPUT_SET: &str = "__convex_output_set";
pub const ERROR_SET: &str = "__convex_error_set";

/// Utility host functions.
pub const LOG: &str = "__convex_log";
pub const NOW_MS: &str = "__convex_now_ms";
pub const RANDOM_BYTES: &str = "__convex_random_bytes";

/// Database host functions.
pub const DB_GET: &str = "__convex_db_get";
pub const DB_INSERT: &str = "__convex_db_insert";
pub const DB_REPLACE: &str = "__convex_db_replace";
pub const DB_PATCH: &str = "__convex_db_patch";
pub const DB_DELETE: &str = "__convex_db_delete";
pub const DB_COUNT: &str = "__convex_db_count";
pub const DB_QUERY: &str = "__convex_db_query";

/// The status returned by `__convex_run` on success.
pub const RUN_OK: i32 = 0;

/// Sentinel returned by database host functions on system error.
pub const DB_ERROR: i64 = -1;

/// Pack a call-data `(offset, len)` pair into a single `i64`.
pub const fn pack_result(offset: u32, len: u32) -> i64 {
    ((offset as i64) << 32) | (len as i64)
}

/// Unpack the call-data offset from a database host function result.
pub const fn unpack_offset(result: i64) -> u32 {
    (result >> 32) as u32
}

/// Unpack the call-data length from a database host function result.
pub const fn unpack_len(result: i64) -> u32 {
    result as u32
}
