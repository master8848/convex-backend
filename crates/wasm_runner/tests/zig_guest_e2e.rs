//! End-to-end tests: a real Zig guest module (freestanding wasm32-wasi
//! reactor, no WASI imports) executing against a real Transaction through the
//! wasm_runner host functions.
//!
//! The fixture source lives in tests/fixtures/zig_guest. Building it requires
//! the Zig toolchain (0.16+), so the test returns early when it's unavailable,
//! like the Go/C/C++/Kotlin guest tests.

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

/// Builds the Zig guest fixture and returns the compiled wasm bytes.
/// Requires `zig` (0.16+). Returns None if the toolchain is unavailable.
fn build_zig_guest_module() -> anyhow::Result<Option<Vec<u8>>> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/zig_guest",);
    // Probe the toolchain first.
    let probe = std::process::Command::new("zig").arg("version").output();
    let probe = match probe {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to run zig to probe the toolchain"),
    };
    if !probe.status.success() {
        eprintln!("zig cannot run; skipping Zig guest test");
        return Ok(None);
    }
    // Zig 0.16 does not auto-export `export fn` symbols for wasm targets via
    // build-exe, so pass the two ABI exports explicitly. Reactor model: no
    // `_start`, module self-initializes via the start section + `_initialize`.
    let output = std::process::Command::new("zig")
        .args([
            "build-exe",
            "guest.zig",
            "-target",
            "wasm32-wasi",
            "-mexec-model=reactor",
            "-O",
            "ReleaseSmall",
            "-fstrip",
            "--export=__convex_run",
            "--export=__convex_functions",
            "--name",
            "guest",
        ])
        .current_dir(dir)
        .output()
        .context("Failed to run zig to build the guest module")?;
    anyhow::ensure!(
        output.status.success(),
        "zig build failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let wasm = std::path::Path::new(dir).join("guest.wasm");
    anyhow::ensure!(wasm.exists(), "zig guest module binary not found");
    std::fs::read(wasm)
        .context("Zig guest module binary not found")
        .map(Some)
}

#[test]
fn test_zig_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let Some(module_binary) = build_zig_guest_module()? else {
            eprintln!("Zig toolchain not found; skipping Zig guest test");
            return anyhow::Ok(());
        };
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // echo: a 394-byte Zig reactor module with no WASI imports at all.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["hello zig"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(value.to_uncommitted_json(), serde_json::json!("hello zig"),);

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
