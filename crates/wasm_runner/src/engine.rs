//! The WASM execution engine: compilation, sandboxing, host functions, and
//! call orchestration.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        Mutex,
    },
};

use anyhow::Context;
use common::{
    components::ComponentId,
    errors::JsError,
    log_lines::{
        LogLevel,
        LogLine,
        LogLineStructured,
    },
    runtime::{
        Runtime,
        UnixTimestamp,
    },
};
use database::Transaction;
use rand_chacha::ChaCha12Rng;
use rand_core::{
    Rng,
    SeedableRng,
};
use serde::{
    Deserialize,
    Serialize,
};
use sha2::Digest;
use tokio::sync::mpsc;
use wasmtime::{
    Caller,
    Config,
    Engine,
    Linker,
    Memory,
    Module,
    Store,
    StoreLimits,
    StoreLimitsBuilder,
    TypedFunc,
};
use wasmtime_wasi::{
    p1::WasiP1Ctx,
    WasiCtxBuilder,
};

use crate::{
    abi::*,
    db::{
        db_count,
        db_delete,
        db_get,
        db_insert,
        db_patch,
        db_query,
        db_replace,
        DbShared,
    },
    determinism::{
        DeterministicRng,
        VirtualMonotonicClock,
        VirtualWallClock,
    },
    limits::WasmLimits,
    validation::validate_module,
};

/// The maximum number of compiled modules kept in memory.
const MAX_CACHED_MODULES: usize = 128;
/// The reactor initialization export used by Go guests.
const INITIALIZE: &str = "_initialize";
/// The maximum number of log lines captured per invocation, matching the
/// isolate path.
const MAX_LOG_LINES: usize = 256;

/// Executes WASM modules in a sandboxed wasmtime runtime.
///
/// One [`Engine`] is shared across all executions; each invocation gets a
/// fresh [`Store`] so guest memory is fully reclaimed between calls.
pub struct WasmRunner {
    engine: Engine,
    module_cache: Mutex<HashMap<[u8; 32], Arc<Module>>>,
}

impl WasmRunner {
    /// Create a runner with an engine configured for deterministic,
    /// sandboxed execution.
    pub fn new() -> anyhow::Result<Self> {
        let mut config = Config::new();
        config
            // NaN canonicalization makes floating point deterministic across
            // replays and architectures.
            .cranelift_nan_canonicalization(true)
            // Disable relaxed SIMD, whose behavior is nondeterministic.
            .wasm_relaxed_simd(false)
            // CPU budget via fuel, with cooperative async yields.
            .consume_fuel(true);
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine,
            module_cache: Mutex::new(HashMap::new()),
        })
    }

    /// The engine backing this runner.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile (or return from cache) a validated module for the given binary.
    pub fn get_or_compile_module(
        &self,
        module_binary: &[u8],
        limits: &WasmLimits,
    ) -> anyhow::Result<Arc<Module>> {
        anyhow::ensure!(
            module_binary.len() <= limits.max_module_size,
            "WASM module is {} bytes, exceeding the limit of {} bytes",
            module_binary.len(),
            limits.max_module_size,
        );
        let sha256: [u8; 32] = sha2::Sha256::digest(module_binary).into();
        if let Some(module) = self
            .module_cache
            .lock()
            .expect("cache poisoned")
            .get(&sha256)
        {
            return Ok(module.clone());
        }
        let module = Arc::new(Module::new(&self.engine, module_binary).map_err(|e| {
            anyhow::anyhow!(
                "Failed to compile WASM module (was it built with the convex_sdk?): {e}"
            )
        })?);
        validate_module(&module, limits)?;
        let mut cache = self.module_cache.lock().expect("cache poisoned");
        if cache.len() >= MAX_CACHED_MODULES {
            cache.clear();
        }
        cache.insert(sha256, module.clone());
        Ok(module)
    }
}

/// Host state stored in the wasmtime [`Store`] for a single invocation.
struct HostContext {
    wasi: WasiP1Ctx,
    input: Vec<u8>,
    call_data: Arc<Mutex<Vec<u8>>>,
    output: Option<Vec<u8>>,
    error: Option<Vec<u8>>,
    log_lines: Vec<LogLine>,
    rng: DeterministicRng,
    unix_timestamp_ms: i64,
    observed_rng: bool,
    observed_time: bool,
    max_call_data: usize,
    log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
    limits: StoreLimits,
}

impl HostContext {
    fn memory(caller: &mut Caller<'_, Self>) -> Result<Memory, wasmtime::Error> {
        caller
            .get_export("memory")
            .and_then(|e| e.into_memory())
            .ok_or_else(|| wasmtime::Error::msg("Guest memory export not found"))
    }

    fn read_guest(
        caller: &mut Caller<'_, Self>,
        ptr: i32,
        len: i32,
    ) -> Result<Vec<u8>, wasmtime::Error> {
        let len = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative length"))?;
        if len == 0 {
            return Ok(Vec::new());
        }
        let ptr = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative pointer"))?;
        let memory = Self::memory(caller)?;
        let mut buffer = vec![0u8; len];
        memory
            .read(caller, ptr, &mut buffer)
            .map_err(|_| wasmtime::Error::msg("guest memory read out of bounds"))?;
        Ok(buffer)
    }

    fn write_guest(
        caller: &mut Caller<'_, Self>,
        ptr: i32,
        bytes: &[u8],
    ) -> Result<(), wasmtime::Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        let ptr = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative pointer"))?;
        let memory = Self::memory(caller)?;
        memory
            .write(caller, ptr, bytes)
            .map_err(|_| wasmtime::Error::msg("guest memory write out of bounds"))
    }

    fn log(&mut self, bytes: &[u8]) {
        let message = String::from_utf8_lossy(bytes).to_string();
        let mut message = message;
        if message.len() > 32768 {
            message = message[..32768].to_string();
        }
        let line = LogLine::Structured(LogLineStructured {
            messages: vec![message].into(),
            level: LogLevel::Log,
            is_truncated: false,
            timestamp: UnixTimestamp::from_millis(
                u64::try_from(self.unix_timestamp_ms).unwrap_or_default(),
            ),
            system_metadata: None,
        });
        if self.log_lines.len() < MAX_LOG_LINES {
            self.log_lines.push(line.clone());
        }
        if let Some(sender) = &self.log_line_sender {
            let _ = sender.send(line);
        }
    }
}

/// Register the synchronous host functions (input, call data, output, log,
/// now, random) on the linker.
fn register_sync_host_functions(linker: &mut Linker<HostContext>) -> Result<(), wasmtime::Error> {
    linker.func_wrap(
        HOST_FN_MODULE,
        INPUT_LENGTH,
        |caller: Caller<'_, HostContext>| -> i32 {
            caller.data().input.len().min(i32::MAX as usize) as i32
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        INPUT_LOAD,
        |mut caller: Caller<'_, HostContext>,
         offset: i32,
         dest: i32,
         len: i32|
         -> Result<(), wasmtime::Error> {
            let (offset, len) = checked_range(offset, len, caller.data().input.len())
                .ok_or_else(|| wasmtime::Error::msg("__convex_input_load out of bounds"))?;
            let bytes = caller.data().input[offset..offset + len].to_vec();
            HostContext::write_guest(&mut caller, dest, &bytes)
                .map_err(|e| wasmtime::Error::msg(format!("{e}")))
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        CALL_DATA_ALLOC,
        |caller: Caller<'_, HostContext>, len: i32| -> i32 {
            let len = match usize::try_from(len) {
                Ok(len) => len,
                Err(_) => return -1,
            };
            let max = caller.data().max_call_data;
            let call_data = caller.data().call_data.clone();
            let mut call_data = match call_data.lock() {
                Ok(guard) => guard,
                Err(_) => return -1,
            };
            if call_data
                .len()
                .checked_add(len)
                .is_none_or(|total| total > max)
            {
                return -1;
            }
            let offset = call_data.len();
            call_data.extend(std::iter::repeat_n(0u8, len));
            i32::try_from(offset).unwrap_or(-1)
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        CALL_DATA_LOAD,
        |mut caller: Caller<'_, HostContext>,
         offset: i32,
         dest: i32,
         len: i32|
         -> Result<(), wasmtime::Error> {
            let (offset, len) = checked_range(offset, len, call_data_len(&caller))
                .ok_or_else(|| wasmtime::Error::msg("__convex_call_data_load out of bounds"))?;
            let call_data = caller.data().call_data.clone();
            let guard = call_data
                .lock()
                .map_err(|_| wasmtime::Error::msg("call data poisoned"))?;
            let bytes = guard[offset..offset + len].to_vec();
            drop(guard);
            HostContext::write_guest(&mut caller, dest, &bytes)
                .map_err(|e| wasmtime::Error::msg(format!("{e}")))
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        OUTPUT_SET,
        |mut caller: Caller<'_, HostContext>, ptr: i32, len: i32| -> Result<(), wasmtime::Error> {
            // The output lives in guest memory; copy it out synchronously so
            // pointers never dangle after the call returns.
            let bytes = HostContext::read_guest(&mut caller, ptr, len)?;
            caller.data_mut().output = Some(bytes);
            Ok(())
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        ERROR_SET,
        |mut caller: Caller<'_, HostContext>, ptr: i32, len: i32| -> Result<(), wasmtime::Error> {
            let bytes = HostContext::read_guest(&mut caller, ptr, len)?;
            caller.data_mut().error = Some(bytes);
            Ok(())
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        LOG,
        |mut caller: Caller<'_, HostContext>, ptr: i32, len: i32| -> Result<(), wasmtime::Error> {
            let bytes = HostContext::read_guest(&mut caller, ptr, len)?;
            caller.data_mut().log(&bytes);
            Ok(())
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        NOW_MS,
        |mut caller: Caller<'_, HostContext>| -> i64 {
            caller.data_mut().observed_time = true;
            caller.data().unix_timestamp_ms
        },
    )?;

    linker.func_wrap(
        HOST_FN_MODULE,
        RANDOM_BYTES,
        |mut caller: Caller<'_, HostContext>, dest: i32, len: i32| -> Result<(), wasmtime::Error> {
            let len = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative length"))?;
            let mut bytes = vec![0u8; len];
            caller.data_mut().rng.fill_bytes(&mut bytes);
            caller.data_mut().observed_rng = true;
            HostContext::write_guest(&mut caller, dest, &bytes)
        },
    )?;
    Ok(())
}

/// Check `(ptr, len)` against a buffer size, returning the validated range.
fn checked_range(ptr: i32, len: i32, size: usize) -> Option<(usize, usize)> {
    let ptr = usize::try_from(ptr).ok()?;
    let len = usize::try_from(len).ok()?;
    let end = ptr.checked_add(len)?;
    if end <= size {
        Some((ptr, len))
    } else {
        None
    }
}

fn call_data_len(caller: &Caller<'_, HostContext>) -> usize {
    caller
        .data()
        .call_data
        .lock()
        .map(|guard| guard.len())
        .unwrap_or(0)
}

/// Register the async database host functions. Each guest import is an async
/// host function so database operations can await without blocking the tokio
/// runtime.
macro_rules! register_db_host_function {
    ($linker:expr, $shared:expr, $name:expr, $func:path) => {{
        let shared = $shared.clone();
        $linker.func_wrap_async(
            HOST_FN_MODULE,
            $name,
            move |mut caller: Caller<'_, HostContext>,
                  (args_ptr, args_len): (i32, i32)|
                  -> Box<
                dyn std::future::Future<Output = Result<i64, wasmtime::Error>> + Send + '_,
            > {
                let args = match HostContext::read_guest(&mut caller, args_ptr, args_len) {
                    Ok(args) => args,
                    Err(e) => {
                        return Box::new(async move { Err(wasmtime::Error::msg(format!("{e}"))) });
                    },
                };
                let shared = shared.clone();
                Box::new(async move { Ok($func(&shared, &args).await) })
            },
        )?;
    }};
}

/// Register the async database host functions.
fn register_db_host_functions<RT: Runtime>(
    linker: &mut Linker<HostContext>,
    shared: &Arc<DbShared<RT>>,
) -> Result<(), wasmtime::Error> {
    register_db_host_function!(linker, shared, DB_GET, db_get::<RT>);
    register_db_host_function!(linker, shared, DB_COUNT, db_count::<RT>);
    register_db_host_function!(linker, shared, DB_INSERT, db_insert::<RT>);
    register_db_host_function!(linker, shared, DB_REPLACE, db_replace::<RT>);
    register_db_host_function!(linker, shared, DB_PATCH, db_patch::<RT>);
    register_db_host_function!(linker, shared, DB_DELETE, db_delete::<RT>);
    register_db_host_function!(linker, shared, DB_QUERY, db_query::<RT>);
    Ok(())
}

/// The parsed result of a WASM execution: the guest's output or error.
pub(crate) struct ExecutionResult {
    pub(crate) output: Option<Vec<u8>>,
    pub(crate) error: Option<Vec<u8>>,
    pub(crate) log_lines: Vec<LogLine>,
    pub(crate) observed_rng: bool,
    pub(crate) observed_time: bool,
}

/// Execute a module, returning the guest's raw output/error.
pub(crate) async fn execute_module<RT: Runtime>(
    runner: &WasmRunner,
    module: &Module,
    input: &[u8],
    tx: Transaction<RT>,
    component: ComponentId,
    rng_seed: [u8; 32],
    unix_timestamp: UnixTimestamp,
    limits: WasmLimits,
    log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
) -> anyhow::Result<(Transaction<RT>, ExecutionResult)> {
    let call_data = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::new(DbShared {
        tx: tokio::sync::Mutex::new(tx),
        call_data: call_data.clone(),
        component,
        max_call_data: limits.max_call_data,
    });

    let now = std::time::Duration::from_millis(
        u64::try_from(unix_timestamp.as_nanos() / 1_000_000).unwrap_or_default(),
    );
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .secure_random(DeterministicRng::from_seed(rng_seed))
        .wall_clock(VirtualWallClock::new(now))
        .monotonic_clock(VirtualMonotonicClock::new(now));
    let context = HostContext {
        wasi: wasi_builder.build_p1(),
        input: input.to_vec(),
        call_data,
        output: None,
        error: None,
        log_lines: Vec::new(),
        rng: ChaCha12Rng::from_seed(rng_seed),
        unix_timestamp_ms: i64::try_from(unix_timestamp.as_nanos() / 1_000_000).unwrap_or_default(),
        observed_rng: false,
        observed_time: false,
        max_call_data: limits.max_call_data,
        log_line_sender,
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes as usize)
            .instances(1)
            .memories(1)
            .tables(1)
            .build(),
    };

    let mut store = Store::new(&runner.engine, context);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(limits.fuel)?;
    store.fuel_async_yield_interval(Some(1_000_000))?;

    let mut linker = Linker::new(&runner.engine);
    wasmtime_wasi::p1::add_to_linker_async(&mut linker, |state: &mut HostContext| &mut state.wasi)
        .map_err(|e| anyhow::anyhow!("registering WASI: {e}"))?;
    register_sync_host_functions(&mut linker)?;
    register_db_host_functions(&mut linker, &shared)?;

    let instance = linker
        .instantiate_async(&mut store, module)
        .await
        .map_err(|e| anyhow::anyhow!("Instantiating WASM module: {e}"))?;
    // Go guests (and other reactor-style runtimes) require `_initialize` to
    // be called once before any export.
    if let Ok(initialize) = instance.get_typed_func::<(), ()>(&mut store, INITIALIZE) {
        tokio::time::timeout(limits.timeout, initialize.call_async(&mut store, ()))
            .await
            .context("Timed out initializing WASM module")?
            .map_err(|e| anyhow::anyhow!("WASM module initialization failed: {e}"))?;
    }
    let run: TypedFunc<(), i32> = instance
        .get_typed_func(&mut store, GUEST_RUN)
        .map_err(|e| anyhow::anyhow!("Missing __convex_run export: {e}"))?;

    let call_result =
        match tokio::time::timeout(limits.timeout, run.call_async(&mut store, ())).await {
            Err(_elapsed) => Err(JsError::from_message(format!(
                "WASM function exceeded the {} second timeout",
                limits.timeout.as_secs(),
            ))),
            Ok(Ok(status)) => {
                if status == RUN_OK {
                    Ok(())
                } else {
                    let error = store.data().error.clone().unwrap_or_default();
                    Err(JsError::from_message(
                        String::from_utf8_lossy(&error).to_string(),
                    ))
                }
            },
            Ok(Err(trap)) => {
                let message = trap.to_string();
                if message.contains("all fuel consumed") {
                    Err(JsError::from_message(
                        "WASM function exceeded its instruction budget".to_string(),
                    ))
                } else if message.contains("memory") && message.contains("limit") {
                    Err(JsError::from_message(format!(
                        "WASM function exceeded the memory limit: {message}"
                    )))
                } else {
                    Err(JsError::from_message(message))
                }
            },
        };

    let execution_result = match call_result {
        Ok(()) => {
            let output = store.data().output.clone();
            ExecutionResult {
                output,
                error: None,
                log_lines: std::mem::take(&mut store.data_mut().log_lines),
                observed_rng: store.data().observed_rng,
                observed_time: store.data().observed_time,
            }
        },
        Err(error) => ExecutionResult {
            output: None,
            error: Some(error.message.into_bytes()),
            log_lines: std::mem::take(&mut store.data_mut().log_lines),
            observed_rng: store.data().observed_rng,
            observed_time: store.data().observed_time,
        },
    };
    drop(store);
    drop(linker);
    let shared = Arc::try_unwrap(shared)
        .map_err(|_| anyhow::anyhow!("WASM host function state still in use"))?;
    Ok((shared.tx.into_inner(), execution_result))
}

/// A function descriptor returned by a guest module's `__convex_functions`
/// export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WasmFunctionDescriptor {
    pub name: String,
    #[serde(rename = "type")]
    pub function_type: String,
}

/// Parse the JSON array returned by a guest's `__convex_functions` export.
fn parse_function_descriptors(
    output: Option<Vec<u8>>,
) -> anyhow::Result<Vec<WasmFunctionDescriptor>> {
    let output = output.context("__convex_functions returned no output")?;
    let descriptors: Vec<WasmFunctionDescriptor> =
        serde_json::from_slice(&output).context("__convex_functions returned invalid JSON")?;
    Ok(descriptors)
}

/// Instantiate a module with a fresh store and return the result of calling
/// `__convex_functions`. Used by the module analyzer.
pub async fn analyze_functions<RT: Runtime>(
    runner: &WasmRunner,
    module: &Module,
    tx: Transaction<RT>,
    component: ComponentId,
    rng_seed: [u8; 32],
    unix_timestamp: UnixTimestamp,
    limits: WasmLimits,
) -> anyhow::Result<Vec<WasmFunctionDescriptor>> {
    let call_data = Mutex::new(Vec::new());
    let now = std::time::Duration::from_millis(
        u64::try_from(unix_timestamp.as_nanos() / 1_000_000).unwrap_or_default(),
    );
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder
        .secure_random(DeterministicRng::from_seed(rng_seed))
        .wall_clock(VirtualWallClock::new(now))
        .monotonic_clock(VirtualMonotonicClock::new(now));
    let context = HostContext {
        wasi: wasi_builder.build_p1(),
        input: Vec::new(),
        call_data: Arc::new(call_data),
        output: None,
        error: None,
        log_lines: Vec::new(),
        rng: ChaCha12Rng::from_seed(rng_seed),
        unix_timestamp_ms: 0,
        observed_rng: false,
        observed_time: false,
        max_call_data: limits.max_call_data,
        log_line_sender: None,
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes as usize)
            .instances(1)
            .memories(1)
            .tables(1)
            .build(),
    };
    let mut store = Store::new(&runner.engine, context);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(limits.fuel)?;
    store.fuel_async_yield_interval(Some(1_000_000))?;
    let mut linker = Linker::new(&runner.engine);
    wasmtime_wasi::p1::add_to_linker_async(&mut linker, |state: &mut HostContext| &mut state.wasi)
        .map_err(|e| anyhow::anyhow!("registering WASI: {e}"))?;
    register_sync_host_functions(&mut linker)?;
    let shared = Arc::new(DbShared::<RT> {
        tx: tokio::sync::Mutex::new(tx),
        call_data: Arc::new(Mutex::new(Vec::new())),
        component,
        max_call_data: limits.max_call_data,
    });
    register_db_host_functions(&mut linker, &shared)?;
    let instance = linker.instantiate_async(&mut store, module).await?;
    if let Ok(initialize) = instance.get_typed_func::<(), ()>(&mut store, INITIALIZE) {
        tokio::time::timeout(limits.timeout, initialize.call_async(&mut store, ()))
            .await
            .context("Timed out initializing WASM module")?
            .map_err(|e| anyhow::anyhow!("WASM module initialization failed: {e}"))?;
    }
    let functions: TypedFunc<(), i32> = instance
        .get_typed_func(&mut store, GUEST_FUNCTIONS)
        .map_err(|e| anyhow::anyhow!("Missing __convex_functions export: {e}"))?;
    let status = tokio::time::timeout(limits.timeout, functions.call_async(&mut store, ()))
        .await
        .context("Timed out querying WASM module functions")??;
    if status != RUN_OK {
        anyhow::bail!("__convex_functions returned error status {status}");
    }
    let output = store.data().output.clone();
    parse_function_descriptors(output)
}
