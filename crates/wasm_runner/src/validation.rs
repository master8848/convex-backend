//! Module validation: rejects modules that violate the sandbox contract
//! before they are instantiated.

use anyhow::Context;
use wasmtime::{
    Module,
    ValType,
};

use crate::{
    abi::{
        CALL_DATA_ALLOC,
        CALL_DATA_LOAD,
        DB_COUNT,
        DB_DELETE,
        DB_GET,
        DB_INSERT,
        DB_PATCH,
        DB_QUERY,
        DB_REPLACE,
        ERROR_SET,
        GUEST_FUNCTIONS,
        GUEST_RUN,
        HOST_FN_MODULE,
        INPUT_LENGTH,
        INPUT_LOAD,
        LOG,
        NOW_MS,
        OUTPUT_SET,
        RANDOM_BYTES,
    },
    limits::WasmLimits,
};

/// The WASI preview 1 import module, which guests may use for their runtime
/// (Rust std, Go runtime). All other imports are rejected.
const WASI_P1_MODULE: &str = "wasi_snapshot_preview1";

/// The WASI preview 1 function names that wasmtime-wasi registers on the
/// linker. Only these may be imported; anything else under the
/// `wasi_snapshot_preview1` namespace is rejected at validation time with a
/// clear error instead of failing at instantiation.
const WASI_P1_FUNCTIONS: &[&str] = &[
    "args_get",
    "args_sizes_get",
    "clock_res_get",
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_advise",
    "fd_allocate",
    "fd_close",
    "fd_datasync",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_fdstat_set_rights",
    "fd_filestat_get",
    "fd_filestat_set_size",
    "fd_filestat_set_times",
    "fd_pread",
    "fd_prestat_dir_name",
    "fd_prestat_get",
    "fd_pwrite",
    "fd_read",
    "fd_readdir",
    "fd_renumber",
    "fd_seek",
    "fd_sync",
    "fd_tell",
    "fd_write",
    "path_create_directory",
    "path_filestat_get",
    "path_filestat_set_times",
    "path_link",
    "path_open",
    "path_readlink",
    "path_remove_directory",
    "path_rename",
    "path_symlink",
    "path_unlink_file",
    "poll_oneoff",
    "proc_exit",
    "proc_raise",
    "random_get",
    "sched_yield",
    "sock_accept",
    "sock_recv",
    "sock_send",
    "sock_shutdown",
];

/// The host functions this engine registers under the `env` namespace.
const HOST_FUNCTIONS: &[&str] = &[
    INPUT_LENGTH,
    INPUT_LOAD,
    CALL_DATA_ALLOC,
    CALL_DATA_LOAD,
    OUTPUT_SET,
    ERROR_SET,
    LOG,
    NOW_MS,
    RANDOM_BYTES,
    DB_GET,
    DB_COUNT,
    DB_INSERT,
    DB_REPLACE,
    DB_PATCH,
    DB_DELETE,
    DB_QUERY,
];

/// The page size of wasm linear memory.
const WASM_PAGE_SIZE: u64 = 64 * 1024;

/// Validate a compiled module against the sandbox contract.
///
/// Checks, in order:
/// 1. The module exports `__convex_run` and `__convex_functions` with the
///    expected `() -> i32` signature.
/// 2. All imports come from `env` or `wasi_snapshot_preview1`.
/// 3. Declared memory maximums fit within `limits.max_memory_bytes`.
///
/// Note: wasm `start` functions are not rejected here because wasmtime runs
/// them at instantiation, where they are already bounded by fuel and the
/// wall-clock timeout.
pub fn validate_module(module: &Module, limits: &WasmLimits) -> anyhow::Result<()> {
    let exports: Vec<_> = module
        .exports()
        .map(|export| export.name().to_string())
        .collect();
    anyhow::ensure!(
        exports.iter().any(|name| name == GUEST_RUN),
        "WASM module is missing the {} export; was it built with convex_sdk?",
        GUEST_RUN,
    );
    anyhow::ensure!(
        exports.iter().any(|name| name == GUEST_FUNCTIONS),
        "WASM module is missing the {} export; was it built with convex_sdk?",
        GUEST_FUNCTIONS,
    );

    for import in module.imports() {
        let (module_name, name, _ty) = (import.module(), import.name(), import.ty());
        if module_name == HOST_FN_MODULE {
            anyhow::ensure!(
                HOST_FUNCTIONS.contains(&name),
                "WASM module imports {module_name}.{name}, which is outside the allowed host \
                 function surface",
            );
        } else if module_name == WASI_P1_MODULE {
            anyhow::ensure!(
                WASI_P1_FUNCTIONS.contains(&name),
                "WASM module imports {module_name}.{name}, which is outside the WASI preview 1 \
                 surface",
            );
        } else {
            anyhow::bail!(
                "WASM module imports {module_name}.{name}, which is outside the allowed sandbox \
                 surface",
            );
        }
    }

    for export in module.exports() {
        let name = export.name();
        if let Some(max) = export
            .ty()
            .memory()
            .and_then(|memory_type| memory_type.maximum())
        {
            let max_bytes = max
                .checked_mul(WASM_PAGE_SIZE)
                .context("Memory maximum overflow")?;
            anyhow::ensure!(
                max_bytes <= limits.max_memory_bytes,
                "WASM module declares {max_bytes} bytes of memory, exceeding the limit of {} bytes",
                limits.max_memory_bytes,
            );
        }
        // Reject multi-value exports other than our two entry points. Guests
        // must use the (ptr, len) ABI.
        if name == GUEST_RUN || name == GUEST_FUNCTIONS {
            let params = export.ty().func().map(|f| f.params().collect::<Vec<_>>());
            let results = export.ty().func().map(|f| f.results().collect::<Vec<_>>());
            let valid = match (&params, &results) {
                (Some(params), Some(results)) => {
                    params.is_empty() && results.len() == 1 && matches!(results[0], ValType::I32)
                },
                _ => false,
            };
            anyhow::ensure!(
                valid,
                "The {} export must have signature () -> i32, found {:?} -> {:?}",
                name,
                params,
                results,
            );
        }
    }
    Ok(())
}
