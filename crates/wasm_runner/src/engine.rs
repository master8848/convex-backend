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
/// The maximum number of modules compiled concurrently. Compilation is
/// CPU-intensive, so concurrent compiles are limited to a small pool to avoid
/// starving the runtime under a flood of unique modules.
const MAX_CONCURRENT_COMPILES: usize = 2;
/// The maximum concurrent executions per environment (deployment). This bounds
/// per-env UDF concurrency so one env cannot starve others; the isolate path
/// uses a similar per-env semaphore via `concurrency_limiter.rs`.
const MAX_CONCURRENT_EXECUTIONS_PER_ENV: usize = 64;
/// GC heap reservation tuned for Kotlin WasmGC (wasmtime 47 enables GC by
/// default). The GC heap is separate from linear memory `StoreLimits`; capping
/// it at 64 MiB reservation + 32 MiB growth avoids 256 MiB of virtual-memory
/// reservation per store while still covering Kotlin `wasmWasi` modules (which
/// rarely exceed tens of MB of GC heap). Freestanding C/Zig/Rust/Go guests
/// allocate no GC heap, so this is pure saving.
const GC_HEAP_RESERVATION_BYTES: u64 = 64 * 1024 * 1024;
const GC_HEAP_GROWTH_BYTES: u64 = 32 * 1024 * 1024;
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
    serialized_cache: Mutex<HashMap<[u8; 32], Vec<u8>>>,
    compile_semaphore: tokio::sync::Semaphore,
    execution_semaphores: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
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
            // Explicitly enable WasmGC + function-references (wasmtime 47
            // defaults them on, but pin them for determinism and for the
            // Kotlin `wasmWasi` target which emits GC + exnref).
            .wasm_gc(true)
            .wasm_function_references(true)
            // CPU budget via fuel, with cooperative async yields.
            .consume_fuel(true)
            // WasmGC allocations (struct.new, array.new, ...) live in a GC
            // heap that StoreLimits::memory_size does not bound, so cap the
            // GC heap reservation separately. Tuned to 64 MiB + 32 MiB growth
            // (down from 256 MiB) to reduce virtual-memory pressure per store
            // while covering Kotlin WasmGC; freestanding C/Zig/Rust/Go guests
            // allocate no GC heap.
            .gc_heap_reservation(GC_HEAP_RESERVATION_BYTES)
            .gc_heap_reservation_for_growth(GC_HEAP_GROWTH_BYTES);
        let engine = Engine::new(&config)?;
        Ok(Self {
            engine,
            module_cache: Mutex::new(HashMap::new()),
            serialized_cache: Mutex::new(HashMap::new()),
            compile_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_COMPILES),
            execution_semaphores: Mutex::new(HashMap::new()),
        })
    }

    /// Per-environment execution semaphore. Each deployment env gets its own
    /// concurrency bound (`MAX_CONCURRENT_EXECUTIONS_PER_ENV`) so one noisy
    /// env does not starve others. This mirrors the isolate path's per-env
    /// limiting in `crates/isolate/src/concurrency_limiter.rs:109`.
    pub fn execution_semaphore_for_env(&self, env: &str) -> Arc<tokio::sync::Semaphore> {
        let mut map = self
            .execution_semaphores
            .lock()
            .expect("execution semaphore map poisoned");
        map.entry(env.to_string())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    MAX_CONCURRENT_EXECUTIONS_PER_ENV,
                ))
            })
            .clone()
    }

    /// Serialize a compiled module to bytes for AOT caching across restarts.
    /// The bytes can be restored with `deserialize_module` on an engine with
    /// an identical `Config` (same wasmtime 47, same GC/fuel/nan settings).
    pub fn serialize_module(module: &Module) -> anyhow::Result<Vec<u8>> {
        module
            .serialize()
            .map_err(|e| anyhow::anyhow!("Failed to serialize WASM module: {e}"))
    }

    /// Deserialize a previously serialized module. Safety: wasmtime requires
    /// the bytes came from `Module::serialize` on a compatible engine; this
    /// is `unsafe` in wasmtime's API because trusting arbitrary bytes would
    /// break sandboxing.
    pub fn deserialize_module(&self, bytes: &[u8]) -> anyhow::Result<Module> {
        // Safety: bytes are assumed to be from `Module::serialize` with the
        // same Config (wasmtime 47, wasm32-wasip1, nan/canonicalization etc).
        // Callers must validate provenance; we re-validate imports via
        // `validate_module` after deserialization where used.
        let module = unsafe { Module::deserialize(&self.engine, bytes) }
            .map_err(|e| anyhow::anyhow!("Failed to deserialize WASM module: {e}"))?;
        Ok(module)
    }

    /// Insert a serialized module into the in-memory serialized cache keyed by
    /// sha256. Useful for warming the cache from disk without recompilation.
    pub fn cache_serialized(&self, sha256: [u8; 32], bytes: Vec<u8>) {
        let mut cache = self
            .serialized_cache
            .lock()
            .expect("serialized cache poisoned");
        if cache.len() >= MAX_CACHED_MODULES {
            cache.clear();
        }
        cache.insert(sha256, bytes);
    }

    /// Retrieve a cached serialized module, if present.
    pub fn get_serialized(&self, sha256: &[u8; 32]) -> Option<Vec<u8>> {
        self.serialized_cache
            .lock()
            .expect("serialized cache poisoned")
            .get(sha256)
            .cloned()
    }

    /// The engine backing this runner.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile (or return from cache) a validated module for the given binary.
    ///
    /// Compilation runs on a blocking thread with a wall-clock timeout and a
    /// bound on the number of concurrent compiles, so a flood of unique
    /// modules cannot stall the async runtime or burn unbounded CPU.
    pub async fn get_or_compile_module(
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
        // AOT fast path: if we have a previously serialized module (from an
        // earlier compile in this process or warmed from disk via
        // `cache_serialized`), deserialize instead of recompiling.
        if let Some(bytes) = self.get_serialized(&sha256) {
            if let Ok(module) = unsafe { Module::deserialize(&self.engine, &bytes) } {
                if validate_module(&module, limits).is_ok() {
                    let module = Arc::new(module);
                    let mut cache = self.module_cache.lock().expect("cache poisoned");
                    if cache.len() >= MAX_CACHED_MODULES {
                        cache.clear();
                    }
                    cache.insert(sha256, module.clone());
                    return Ok(module);
                }
            }
        }
        let _permit = self
            .compile_semaphore
            .acquire()
            .await
            .context("WASM compilation semaphore closed")?;
        // Another task may have compiled the same module while we waited for
        // the permit.
        if let Some(module) = self
            .module_cache
            .lock()
            .expect("cache poisoned")
            .get(&sha256)
        {
            return Ok(module.clone());
        }
        // Check serialized cache again after acquiring the permit.
        if let Some(bytes) = self.get_serialized(&sha256) {
            if let Ok(module) = unsafe { Module::deserialize(&self.engine, &bytes) } {
                if validate_module(&module, limits).is_ok() {
                    let module = Arc::new(module);
                    let mut cache = self.module_cache.lock().expect("cache poisoned");
                    if cache.len() >= MAX_CACHED_MODULES {
                        cache.clear();
                    }
                    cache.insert(sha256, module.clone());
                    return Ok(module);
                }
            }
        }
        let engine = self.engine.clone();
        let module_binary = module_binary.to_vec();
        let compiled = tokio::time::timeout(
            limits.compile_timeout,
            tokio::task::spawn_blocking(move || Module::new(&engine, &module_binary)),
        )
        .await
        .context("Timed out compiling WASM module")??;
        let module = Arc::new(compiled.map_err(|e| {
            anyhow::anyhow!(
                "Failed to compile WASM module (was it built with the convex_sdk?): {e}"
            )
        })?);
        validate_module(&module, limits)?;
        // Populate the AOT serialized cache (best-effort). `Module::serialize`
        // is deterministic for a given engine Config (wasmtime 47, nan
        // canonicalization, GC settings) and avoids recompilation after a
        // restart if the bytes are persisted to disk by the caller.
        if let Ok(bytes) = module.serialize() {
            self.cache_serialized(sha256, bytes);
        }
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
        // Bound-check against the actual memory size before allocating, so a
        // guest cannot request a huge allocation that fails only after the
        // host has already committed the buffer.
        let data = memory.data(caller);
        let end = ptr
            .checked_add(len)
            .ok_or_else(|| wasmtime::Error::msg("guest memory read out of bounds"))?;
        if end > data.len() {
            return Err(wasmtime::Error::msg("guest memory read out of bounds"));
        }
        Ok(data[ptr..end].to_vec())
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
            let ptr =
                usize::try_from(dest).map_err(|_| wasmtime::Error::msg("negative pointer"))?;
            // Bound-check before allocating so a guest cannot force a huge
            // allocation that is rejected only after the host commits it.
            let data_size = HostContext::memory(&mut caller)?.data_size(&caller);
            if len > 0 && ptr.checked_add(len).is_none_or(|end| end > data_size) {
                return Err(wasmtime::Error::msg("guest memory write out of bounds"));
            }
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

    let instance =
        tokio::time::timeout(limits.timeout, linker.instantiate_async(&mut store, module))
            .await
            .context("Timed out instantiating WASM module")?
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
                // Use the alternate Display form: this wasmtime's default
                // Display only shows the outermost backtrace context and drops
                // the underlying trap/host-function message.
                let message = format!("{trap:#}");
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
///
/// Extended descriptor carries validator JSON (language-agnostic IR) so wasm
/// guests converge to the same `AnalyzedFunction` rows as the isolate path:
/// `[{name,type,args,returns,visibility}]` where `args`/`returns` are
/// `Option<String>` validator JSON (`None` = unvalidated). Older guests that
/// emit only `{name,type}` remain accepted via `Option` defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WasmFunctionDescriptor {
    pub name: String,
    #[serde(rename = "type")]
    pub function_type: String,
    /// JSON-serialized `ConvexValidator` for args (Convex validator JSON)
    #[serde(default)]
    pub args: Option<String>,
    /// JSON-serialized `ConvexValidator` for returns
    #[serde(default)]
    pub returns: Option<String>,
    /// `"public"` | `"internal"` — mirrors `Visibility` / `FilterApi` partition
    #[serde(default)]
    pub visibility: Option<String>,
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
    let instance =
        tokio::time::timeout(limits.timeout, linker.instantiate_async(&mut store, module))
            .await
            .context("Timed out instantiating WASM module")??;
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
