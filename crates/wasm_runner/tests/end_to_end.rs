//! End-to-end tests: a real Rust guest module (compiled to wasm32-wasip1)
//! executing against a real Transaction through the wasm_runner host
//! functions.

use std::sync::Arc;

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
    TableModel,
    Database,
    Transaction,
};
use indexing::index_cache::IndexCache;
use keybroker::Identity;
use runtime::prod::ProdRuntime;
use search::{
    searcher::SearcherStub,
    Searcher,
};
use sqlite::SqlitePersistence;
use tokio::sync::mpsc;
use serde_json::Value as JsonValue;
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
async fn new_database(rt: &ProdRuntime) -> anyhow::Result<Database<ProdRuntime>> {
    let persistence: Arc<dyn Persistence> = Arc::new(SqlitePersistence::new(":memory:")?);
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
    )
    .await?;
    Ok(db)
}

/// Create a user table and commit it.
async fn create_table(database: &Database<ProdRuntime>, name: &str) -> anyhow::Result<()> {
    let identity = Identity::Unknown(None);
    let mut tx = database.begin(identity.clone()).await?;
    let table_name: TableName = name.parse()?;
    TableModel::new(&mut tx)
        .insert_table_metadata(TableNamespace::Global, &table_name)
        .await?;
    database
        .commit_with_write_source(tx, database::WriteSource::System("test"))
        .await?;
    Ok(())
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
        let database = new_database(&rt).await?;
        create_table(&database, "users").await?;
        create_table(&database, "counters").await?;

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
            .context(format!("insert_user should return the id string, got: {value:?}"))?
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
        assert_eq!(json.get("id").and_then(JsonValue::as_str), Some(id.as_str()));

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
        let module = runner.get_or_compile_module(&module_binary, &WasmLimits::default())?;
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
fn test_module_validation_rejects_bad_modules() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let runner = WasmRunner::new()?;
        // Not a wasm module at all.
        assert!(runner
            .get_or_compile_module(b"not wasm", &WasmLimits::default())
            .is_err());
        anyhow::Ok(())
    })
}
