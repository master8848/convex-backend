//! End-to-end tests: a real Rust guest module (compiled to wasm32-wasip1)
//! executing against a real Transaction through the wasm_runner host
//! functions.

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use common::{
    components::ComponentId,
    persistence::Persistence,
    runtime::{
        new_unlimited_rate_limiter,
        UnixTimestamp,
    },
    shutdown::ShutdownSignal,
    types::TableName,
    virtual_system_mapping::VirtualSystemMapping,
};
use database::{
    Database,
    TableModel,
    Transaction,
};
use indexing::index_cache::IndexCache;
use keybroker::Identity;
use runtime::prod::ProdRuntime;
use search::{
    searcher::SearcherStub,
    Searcher,
};
use serde_json::Value as JsonValue;
use sqlite::SqlitePersistence;
use tokio::sync::mpsc;
use value::{
    PendingValue,
    TableNamespace,
};
use wasm_runner::{
    run_wasm_udf,
    WasmInput,
    WasmLimits,
    WasmRunner,
};

/// Builds the Go guest fixture and returns the compiled wasm bytes.
/// Requires the Go toolchain. Returns None if `go` is not installed.
fn build_go_guest_module() -> anyhow::Result<Option<Vec<u8>>> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/go_guest",);
    let output = std::process::Command::new("go")
        .args([
            "build",
            "-buildmode=c-shared",
            "-ldflags=-s -w",
            "-trimpath",
            "-o",
            "go_guest.wasm",
            ".",
        ])
        .current_dir(dir)
        .env("GOOS", "wasip1")
        .env("GOARCH", "wasm")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to run go to build the guest module"),
    };
    anyhow::ensure!(
        output.status.success(),
        "go build failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let wasm_path = format!("{dir}/go_guest.wasm");
    std::fs::read(wasm_path)
        .context("Go guest module binary not found")
        .map(Some)
}

/// Builds the guest fixture crate and returns the compiled wasm bytes.
/// Requires cargo and the wasm32-wasip1 target to be installed.
fn build_guest_module() -> anyhow::Result<Vec<u8>> {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rust_guest/Cargo.toml",
    );
    let output = std::process::Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            manifest,
            "--target",
            "wasm32-wasip1",
            "--release",
            "--quiet",
        ])
        .output()
        .context("Failed to run cargo to build the guest module")?;
    anyhow::ensure!(
        output.status.success(),
        "cargo build failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rust_guest/target/wasm32-wasip1/release/rust_guest_fixture.wasm",
    );
    std::fs::read(wasm_path).context("Guest module binary not found")
}

/// Create an empty database with a fresh sqlite persistence layer.
async fn new_database(
    rt: &ProdRuntime,
) -> anyhow::Result<(Arc<dyn Persistence>, Database<ProdRuntime>)> {
    let persistence: Arc<dyn Persistence> = Arc::new(SqlitePersistence::new(":memory:")?);
    let database = load_database(rt, persistence.clone()).await?;
    Ok((persistence, database))
}

async fn load_database(
    rt: &ProdRuntime,
    persistence: Arc<dyn Persistence>,
) -> anyhow::Result<Database<ProdRuntime>> {
    let searcher: Arc<dyn Searcher> = Arc::new(SearcherStub);
    let (shutdown_tx, _) = tokio::sync::oneshot::channel::<anyhow::Error>();
    let shutdown = ShutdownSignal::new(shutdown_tx);
    let index_cache = IndexCache::new(u64::MAX).new_handle();
    let retention_rate_limiter = Arc::new(new_unlimited_rate_limiter(rt.clone()));
    let (deleted_tablet_tx, _) = mpsc::channel(1);
    let db = Database::load(
        persistence,
        rt.clone(),
        searcher,
        shutdown,
        VirtualSystemMapping::default(),
        index_cache,
        retention_rate_limiter,
        deleted_tablet_tx,
        String::from("test"),
    )
    .await?;
    Ok(db)
}

/// Create a user table and commit it.
async fn create_table(
    database: &Database<ProdRuntime>,
    persistence: &Arc<dyn Persistence>,
    rt: &ProdRuntime,
    name: &str,
) -> anyhow::Result<Database<ProdRuntime>> {
    let identity = Identity::Unknown(None);
    let mut tx = database.begin(identity.clone()).await?;
    let table_name: TableName = name.parse()?;
    TableModel::new(&mut tx)
        .insert_table_metadata(TableNamespace::Global, &table_name)
        .await?;
    database
        .commit_with_write_source(tx, database::WriteSource::System("test"))
        .await?;
    // Reload so the table count snapshot includes the new table.
    load_database(rt, persistence.clone()).await
}

/// Run a function in a fresh transaction against the given database.
async fn run_function(
    runner: &WasmRunner,
    module_binary: &[u8],
    database: &Database<ProdRuntime>,
    function_name: &str,
    args: JsonValue,
) -> anyhow::Result<(Transaction<ProdRuntime>, wasm_runner::WasmUdfResult)> {
    let identity = Identity::Unknown(None);
    let tx = database.begin(identity).await?;
    let args_json = serde_json::to_string(&args)?;
    run_wasm_udf(
        runner,
        module_binary,
        WasmInput {
            function_name: function_name.to_string(),
            args_json,
        },
        tx,
        ComponentId::Root,
        [7u8; 32],
        UnixTimestamp::from_millis(1_700_000_000_000),
        WasmLimits::default(),
        true,
        None,
    )
    .await
}

#[test]
fn test_rust_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let module_binary = build_guest_module()?;
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "users").await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // echo: argument deserialization + result serialization.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["hello"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!("hello"));

        // add: two arguments + virtual time. Note: plain JSON numbers
        // round-trip as Float64, matching TypeScript `number` semantics.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "add",
            serde_json::json!([2, 3]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        let json = value.to_uncommitted_json();
        let JsonValue::Number(n) = json else {
            panic!("expected number, got {json:?}");
        };
        assert_eq!(n.as_f64(), Some(1_700_000_000_005.0));

        // Typed struct result.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "get_user",
            serde_json::json!(["alice"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        // Object keys are sorted (BTreeMap) and numbers are Float64.
        assert_eq!(
            value.to_uncommitted_json(),
            serde_json::json!({ "age": 42.0, "name": "alice" }),
        );

        // Mutation: insert, then read back in a subsequent transaction.
        let (tx, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "insert_user",
            serde_json::json!(["bob"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        let id = value
            .to_uncommitted_json()
            .as_str()
            .context(format!(
                "insert_user should return the id string, got: {value:?}"
            ))?
            .to_string();
        assert!(!id.is_empty());
        database
            .commit_with_write_source(tx, database::WriteSource::System("test"))
            .await?;

        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "read_user",
            serde_json::json!([id]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        let json = value.to_uncommitted_json();
        // The function returns a Document: { id, creation_time, value }.
        assert_eq!(
            json.pointer("/value/_id").and_then(JsonValue::as_str),
            Some(id.as_str()),
        );
        assert_eq!(
            json.get("id").and_then(JsonValue::as_str),
            Some(id.as_str())
        );

        // Query over the whole table.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "list_users",
            serde_json::json!([]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        let json = value.to_uncommitted_json();
        assert!(json.as_array().is_some());

        // Error propagation.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "fail",
            serde_json::json!([]),
        )
        .await?;
        assert!(result.result.is_err());

        // Deterministic randomness: same seed, same bytes.
        let (_, result_a) = run_function(
            &runner,
            &module_binary,
            &database,
            "random",
            serde_json::json!([]),
        )
        .await?;
        let (_, result_b) = run_function(
            &runner,
            &module_binary,
            &database,
            "random",
            serde_json::json!([]),
        )
        .await?;
        let a: PendingValue = result_a.result?.unpack()?;
        let b: PendingValue = result_b.result?.unpack()?;
        assert_eq!(a.to_uncommitted_json(), b.to_uncommitted_json());

        // Sync functions.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "double",
            serde_json::json!([21]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!(42.0));

        // Log lines are captured.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["hi"]),
        )
        .await?;
        assert!(
            !result.log_lines.is_empty(),
            "echo should have emitted at least one log line",
        );

        // Function descriptor analysis.
        let module = runner
            .get_or_compile_module(&module_binary, &WasmLimits::default())
            .await?;
        let tx = database.begin(Identity::Unknown(None)).await?;
        let functions = wasm_runner::analyze_functions(
            &runner,
            &module,
            tx,
            ComponentId::Root,
            [7u8; 32],
            UnixTimestamp::from_millis(1_700_000_000_000),
            WasmLimits::default(),
        )
        .await?;
        let names: Vec<_> = functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"insert_user"));
        assert!(functions
            .iter()
            .all(|f| matches!(f.function_type.as_str(), "query" | "mutation")));

        anyhow::Ok(())
    })
}

#[test]
fn test_go_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let Some(module_binary) = build_go_guest_module()? else {
            eprintln!("go not found; skipping Go guest test");
            return anyhow::Ok(());
        };
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // Go's runtime requires _initialize before any export; the runner
        // handles that internally.

        // echo
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["go hello"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!("go hello"),);

        // add (with virtual time)
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "add",
            serde_json::json!([2.0, 3.0]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(
            value.to_uncommitted_json(),
            serde_json::json!(1_700_000_000_005.0),
        );

        // mutation: bump commits a write to the counters table
        let (tx, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "bump",
            serde_json::json!([]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!(1.0));
        database
            .commit_with_write_source(tx, database::WriteSource::System("test"))
            .await?;

        // deterministic randomness
        let (_, a) = run_function(
            &runner,
            &module_binary,
            &database,
            "random",
            serde_json::json!([]),
        )
        .await?;
        let (_, b) = run_function(
            &runner,
            &module_binary,
            &database,
            "random",
            serde_json::json!([]),
        )
        .await?;
        let av: PendingValue = a.result?.unpack()?;
        let bv: PendingValue = b.result?.unpack()?;
        assert_eq!(av.to_uncommitted_json(), bv.to_uncommitted_json());

        // descriptor analysis
        let module = runner
            .get_or_compile_module(&module_binary, &WasmLimits::default())
            .await?;
        let tx = database.begin(Identity::Unknown(None)).await?;
        let functions = wasm_runner::analyze_functions(
            &runner,
            &module,
            tx,
            ComponentId::Root,
            [7u8; 32],
            UnixTimestamp::from_millis(1_700_000_000_000),
            WasmLimits::default(),
        )
        .await?;
        let names: Vec<_> = functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"bump"));

        anyhow::Ok(())
    })
}

/// Builds the freestanding C guest fixture and returns the compiled wasm
/// bytes. Requires a clang that ships the `wasm32-wasip1` target (stock LLVM;
/// Apple's system clang does not). Returns None if unavailable.
fn build_c_guest_module() -> anyhow::Result<Option<Vec<u8>>> {
    let source = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/c_guest/guest.c"
    );
    let output = std::process::Command::new("clang")
        .args([
            "--target=wasm32-wasip1",
            "-Oz",
            "-flto",
            "-ffunction-sections",
            "-fdata-sections",
            "-fvisibility=hidden",
            "-nostdlib",
            "-Wl,--no-entry",
            "-Wl,--gc-sections",
            "-Wl,--icf=all",
            "-Wl,--strip-all",
            "-Wl,--export=__convex_run",
            "-Wl,--export=__convex_functions",
            "-Wl,--allow-undefined",
            "-o",
            "c_guest.wasm",
            source,
        ])
        .current_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/c_guest"
        ))
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to run clang to build the C guest module"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No available targets are compatible")
            || stderr.contains("unknown target")
        {
            eprintln!("clang lacks the wasm32-wasip1 target; skipping C guest test");
            return Ok(None);
        }
        anyhow::bail!("clang build failed: {stderr}");
    }
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/c_guest/c_guest.wasm",
    );
    std::fs::read(wasm_path)
        .context("C guest module binary not found")
        .map(Some)
}

#[test]
fn test_c_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let Some(module_binary) = build_c_guest_module()? else {
            return anyhow::Ok(());
        };
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // echo: a freestanding C guest with no libc and no WASI imports.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["c hello"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!("c hello"));

        // unknown function -> guest error, not a host panic.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "no_such_fn",
            serde_json::json!([]),
        )
        .await?;
        assert!(result.result.is_err());

        // descriptor analysis
        let module = runner
            .get_or_compile_module(&module_binary, &WasmLimits::default())
            .await?;
        let tx = database.begin(Identity::Unknown(None)).await?;
        let functions = wasm_runner::analyze_functions(
            &runner,
            &module,
            tx,
            ComponentId::Root,
            [7u8; 32],
            UnixTimestamp::from_millis(1_700_000_000_000),
            WasmLimits::default(),
        )
        .await?;
        let names: Vec<_> = functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["echo"]);

        anyhow::Ok(())
    })
}

/// Builds the freestanding C++ guest fixture and returns the compiled wasm
/// bytes. Requires a clang++ that ships the `wasm32-wasip1` target (stock
/// LLVM; Apple's system clang does not). Returns None if unavailable.
fn build_cpp_guest_module() -> anyhow::Result<Option<Vec<u8>>> {
    let source = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpp_guest/guest.cpp"
    );
    let output = std::process::Command::new("clang++")
        .args([
            "--target=wasm32-wasip1",
            "-Oz",
            "-flto",
            "-ffunction-sections",
            "-fdata-sections",
            "-fvisibility=hidden",
            "-nostdlib",
            "-fno-exceptions",
            "-fno-rtti",
            "-fno-threadsafe-statics",
            "-Wl,--no-entry",
            "-Wl,--gc-sections",
            "-Wl,--icf=all",
            "-Wl,--strip-all",
            "-Wl,--export=__convex_run",
            "-Wl,--export=__convex_functions",
            "-Wl,--allow-undefined",
            "-o",
            "cpp_guest.wasm",
            source,
        ])
        .current_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cpp_guest"
        ))
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to run clang++ to build the C++ guest module"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No available targets are compatible")
            || stderr.contains("unknown target")
        {
            eprintln!("clang++ lacks the wasm32-wasip1 target; skipping C++ guest test");
            return Ok(None);
        }
        anyhow::bail!("clang++ build failed: {stderr}");
    }
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cpp_guest/cpp_guest.wasm",
    );
    std::fs::read(wasm_path)
        .context("C++ guest module binary not found")
        .map(Some)
}

#[test]
fn test_cpp_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let Some(module_binary) = build_cpp_guest_module()? else {
            return anyhow::Ok(());
        };
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // echo: a freestanding C++ guest (classes/templates/constexpr, no libc++).
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["cpp hello"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!("cpp hello"));

        // unknown function -> guest error, not a host panic.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "no_such_fn",
            serde_json::json!([]),
        )
        .await?;
        assert!(result.result.is_err());

        // descriptor analysis
        let module = runner
            .get_or_compile_module(&module_binary, &WasmLimits::default())
            .await?;
        let tx = database.begin(Identity::Unknown(None)).await?;
        let functions = wasm_runner::analyze_functions(
            &runner,
            &module,
            tx,
            ComponentId::Root,
            [7u8; 32],
            UnixTimestamp::from_millis(1_700_000_000_000),
            WasmLimits::default(),
        )
        .await?;
        let names: Vec<_> = functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["echo"]);

        anyhow::Ok(())
    })
}

#[test]
fn test_module_validation_rejects_bad_modules() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let runner = WasmRunner::new()?;
        // Not a wasm module at all.
        assert!(runner
            .get_or_compile_module(b"not wasm", &WasmLimits::default())
            .await
            .is_err());

        let mk_module = |imports: &str| {
            format!(
                r#"
(module
  {imports}
  (memory (export "memory") 1)
  (func (export "__convex_run") (result i32)
    i32.const 0)
  (func (export "__convex_functions") (result i32)
    i32.const 0))
"#,
            )
        };
        // Imports outside the WASI preview 1 surface are rejected.
        let bad_wasi = mk_module(
            r#"(import "wasi_snapshot_preview1" "not_a_wasi_function" (func $x (result i32)))"#,
        );
        let err = runner
            .get_or_compile_module(bad_wasi.as_bytes(), &WasmLimits::default())
            .await
            .expect_err("unknown WASI import should be rejected");
        assert!(
            format!("{err:#}").contains("outside the WASI preview 1 surface"),
            "{err:#}"
        );
        // Imports outside the host function surface are rejected.
        let bad_env = mk_module(r#"(import "env" "not_a_host_function" (func $x (result i32)))"#);
        let err = runner
            .get_or_compile_module(bad_env.as_bytes(), &WasmLimits::default())
            .await
            .expect_err("unknown env import should be rejected");
        assert!(
            format!("{err:#}").contains("outside the allowed host function surface"),
            "{err:#}"
        );
        // Imports from a foreign module are rejected.
        let bad_module = mk_module(r#"(import "some_other_module" "foo" (func $x (result i32)))"#);
        let err = runner
            .get_or_compile_module(bad_module.as_bytes(), &WasmLimits::default())
            .await
            .expect_err("foreign module import should be rejected");
        assert!(
            format!("{err:#}").contains("outside the allowed sandbox surface"),
            "{err:#}"
        );
        anyhow::Ok(())
    })
}

#[test]
fn test_huge_guest_buffers_are_rejected() -> anyhow::Result<()> {
    // A guest can pass arbitrary (ptr, len) pairs to host functions. The host
    // must reject out-of-bounds ranges before allocating anything.
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        let huge_output_set = r#"
(module
  (import "env" "__convex_output_set" (func $output_set (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "__convex_run") (result i32)
    i32.const 0
    i32.const 2147483647
    call $output_set
    i32.const 0)
  (func (export "__convex_functions") (result i32)
    i32.const 0))
"#;
        let (_, result) = run_function(
            &runner,
            huge_output_set.as_bytes(),
            &database,
            "echo",
            serde_json::json!([]),
        )
        .await?;
        let message = result
            .result
            .expect_err("huge output_set length should fail")
            .message;
        assert!(message.contains("out of bounds"), "{message}");

        let huge_random_bytes = r#"
(module
  (import "env" "__convex_random_bytes" (func $random_bytes (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "__convex_run") (result i32)
    i32.const 0
    i32.const 2147483647
    call $random_bytes
    i32.const 0)
  (func (export "__convex_functions") (result i32)
    i32.const 0))
"#;
        let (_, result) = run_function(
            &runner,
            huge_random_bytes.as_bytes(),
            &database,
            "echo",
            serde_json::json!([]),
        )
        .await?;
        let message = result
            .result
            .expect_err("huge random_bytes length should fail")
            .message;
        assert!(message.contains("out of bounds"), "{message}");

        anyhow::Ok(())
    })
}

#[test]
fn test_instantiation_timeout() -> anyhow::Result<()> {
    // A module whose `start` function runs forever (burning the entire fuel
    // budget with no wall-clock bound before the fix) must be cut off by the
    // invocation timeout.
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (_persistence, database) = new_database(&rt).await?;

        let spinning_start = r#"
(module
  (memory (export "memory") 1)
  (start $start)
  (func $start
    (loop $spin
      br $spin))
  (func (export "__convex_run") (result i32)
    i32.const 0)
  (func (export "__convex_functions") (result i32)
    i32.const 0))
"#;
        let identity = Identity::Unknown(None);
        let tx = database.begin(identity).await?;
        let result = run_wasm_udf(
            &runner,
            spinning_start.as_bytes(),
            WasmInput {
                function_name: "echo".to_string(),
                args_json: "[]".to_string(),
            },
            tx,
            ComponentId::Root,
            [7u8; 32],
            UnixTimestamp::from_millis(1_700_000_000_000),
            WasmLimits {
                timeout: Duration::from_millis(500),
                ..WasmLimits::default()
            },
            true,
            None,
        )
        .await;
        let err = match result {
            Ok(_) => anyhow::bail!("blocking start function should time out"),
            Err(e) => e,
        };
        let message = format!("{err:#}");
        assert!(message.contains("Timed out instantiating"), "{message}");
        anyhow::Ok(())
    })
}
