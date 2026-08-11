//! End-to-end tests: a real Kotlin guest module (Kotlin Multiplatform
//! `wasmWasi` target: wasm32-wasip1 + WasmGC, no JS runtime) executing
//! against a real Transaction through the wasm_runner host functions.
//!
//! The fixture source lives in tests/fixtures/kotlin_guest. Building it
//! requires a JDK + Gradle + network (Kotlin Gradle plugin), so the test
//! returns early when the toolchain is unavailable, like the Go/C/C++
//! guest tests.

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

/// Builds the Kotlin guest fixture and returns the compiled wasm bytes.
/// Requires gradle + a JDK (and network for the Kotlin Gradle plugin).
/// Returns None if the toolchain is unavailable.
fn build_kotlin_guest_module() -> anyhow::Result<Option<Vec<u8>>> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kotlin_guest",);
    // Probe the toolchain first: `gradle` must exist *and* find a JVM.
    let probe = std::process::Command::new("gradle")
        .arg("--version")
        .output();
    let probe = match probe {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("Failed to run gradle to probe the Kotlin toolchain"),
    };
    if !probe.status.success() {
        eprintln!("gradle cannot run (no JDK?); skipping Kotlin guest test");
        return Ok(None);
    }
    let output = std::process::Command::new("gradle")
        .args(["build", "--console=plain", "--quiet"])
        .current_dir(dir)
        .output()
        .context("Failed to run gradle to build the Kotlin guest module")?;
    anyhow::ensure!(
        output.status.success(),
        "gradle build failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let wasm = find_kotlin_wasm(dir)?;
    std::fs::read(wasm)
        .context("Kotlin guest module binary not found")
        .map(Some)
}

/// Find a compiled `.wasm` under the fixture dir, preferring
/// optimized/production artifacts over debug ones.
fn find_kotlin_wasm(dir: &str) -> anyhow::Result<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // Skip caches and Node tooling (may contain unrelated .wasm).
                if matches!(name, ".gradle" | ".kotlin" | "kotlin-js-store" | "node_modules") {
                    continue;
                }
                walk(&path, out)?;
            } else if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    walk(std::path::Path::new(dir), &mut found)?;
    anyhow::ensure!(!found.is_empty(), "no .wasm file found under {dir}");
    let score = |p: &std::path::Path| -> (bool, bool, u64) {
        let s = p.to_string_lossy().to_lowercase();
        (
            s.contains("optimized"),
            s.contains("production") || s.contains("release"),
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
        )
    };
    Ok(found
        .into_iter()
        .max_by_key(|p| score(p))
        .expect("non-empty list"))
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
fn test_kotlin_guest_end_to_end() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let Some(module_binary) = build_kotlin_guest_module()? else {
            eprintln!("Kotlin toolchain (gradle + JDK) not found; skipping Kotlin guest test");
            return anyhow::Ok(());
        };
        let runner = WasmRunner::new()?;
        let rt = ProdRuntime::new(&tokio);
        let (persistence, mut database) = new_database(&rt).await?;
        database = create_table(&database, &persistence, &rt, "counters").await?;

        // echo: a Kotlin/Wasm (WasmGC) guest with no JS runtime. The module
        // has no `_initialize`/`_start`; its start section initializes the
        // Kotlin runtime at instantiation, and the runner handles either.
        let (_, result) = run_function(
            &runner,
            &module_binary,
            &database,
            "echo",
            serde_json::json!(["kotlin hello"]),
        )
        .await?;
        let value: PendingValue = result.result?.unpack()?;
        assert_eq!(
            value.to_uncommitted_json(),
            serde_json::json!("kotlin hello"),
        );

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
        assert_eq!(names, vec!["echo"]);

        anyhow::Ok(())
    })
}
