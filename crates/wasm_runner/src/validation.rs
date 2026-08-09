//! Module validation: rejects modules that violate the sandbox contract
//! before they are instantiated.

use anyhow::Context;
use wasmtime::{
    Module,
    ValType,
};

use crate::{
    abi::{
        GUEST_FUNCTIONS,
        GUEST_RUN,
        HOST_FN_MODULE,
    },
    limits::WasmLimits,
};

/// The WASI preview 1 import module, which guests may use for their runtime
/// (Rust std, Go runtime). All other imports are rejected.
const WASI_P1_MODULE: &str = "wasi_snapshot_preview1";

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
        if module_name == HOST_FN_MODULE || module_name == WASI_P1_MODULE {
            continue;
        }
        anyhow::bail!(
            "WASM module imports {module_name}.{name}, which is outside the allowed sandbox \
             surface",
        );
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
