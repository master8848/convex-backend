//! Resource limits enforced on guest modules.

use std::time::Duration;

/// The amount of guest linear memory a module is allowed to use.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
/// The maximum size of a module binary.
pub const DEFAULT_MAX_MODULE_SIZE: usize = 32 * 1024 * 1024;
/// The maximum size of host-managed call data handed to guests.
pub const DEFAULT_MAX_CALL_DATA: usize = 16 * 1024 * 1024;
/// The default CPU budget in wasmtime fuel units (~instructions). 10 billion
/// instructions is a generous upper bound for a single UDF invocation.
pub const DEFAULT_FUEL: u64 = 10_000_000_000;
/// The default wall-clock timeout for queries and mutations.
pub const DEFAULT_USER_TIMEOUT: Duration = Duration::from_secs(30);
/// The default wall-clock timeout for compiling a module binary. Cranelift
/// compilation is CPU-bound and can take seconds for large modules, so it is
/// bounded separately from execution.
pub const DEFAULT_COMPILE_TIMEOUT: Duration = Duration::from_secs(30);

/// Resource limits applied to a single WASM UDF execution.
#[derive(Debug, Clone, Copy)]
pub struct WasmLimits {
    /// Maximum guest linear memory in bytes.
    pub max_memory_bytes: u64,
    /// Maximum size of a module binary.
    pub max_module_size: usize,
    /// Maximum size of host-managed call data handed to the guest.
    pub max_call_data: usize,
    /// CPU budget in wasmtime fuel units.
    pub fuel: u64,
    /// Wall-clock timeout for the whole invocation. This is the authoritative
    /// limit: it also covers time spent blocked inside host functions, which
    /// fuel does not account for.
    pub timeout: Duration,
    /// Wall-clock timeout for compiling a module binary. Compilation happens
    /// once per unique module (results are cached), off the async runtime.
    pub compile_timeout: Duration,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
            max_module_size: DEFAULT_MAX_MODULE_SIZE,
            max_call_data: DEFAULT_MAX_CALL_DATA,
            fuel: DEFAULT_FUEL,
            timeout: DEFAULT_USER_TIMEOUT,
            compile_timeout: DEFAULT_COMPILE_TIMEOUT,
        }
    }
}
