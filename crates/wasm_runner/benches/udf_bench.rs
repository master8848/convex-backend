//! Benchmarks comparing WASM UDF execution (Rust and Go guests) against a
//! native Rust baseline, including the full per-invocation overhead:
//! module lookup, instantiation, host-function setup, execution, and result
//! parsing — all against a real (sqlite-backed) database.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p wasm_runner --bench udf_bench
//! ```

use std::{
    sync::Arc,
    time::Instant,
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
    virtual_system_mapping::VirtualSystemMapping,
};
use database::Database;
use indexing::index_cache::IndexCache;
use keybroker::Identity;
use runtime::prod::ProdRuntime;
use search::{
    searcher::SearcherStub,
    Searcher,
};
use sqlite::SqlitePersistence;
use tokio::sync::mpsc;
use wasm_runner::{
    run_wasm_udf,
    WasmFunctionDescriptor,
    WasmInput,
    WasmLimits,
    WasmRunner,
};

/// Build the Rust guest fixture, returning wasm bytes + the echo descriptor.
fn build_rust_guest() -> anyhow::Result<(Vec<u8>, WasmFunctionDescriptor)> {
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
        .context("Failed to run cargo")?;
    anyhow::ensure!(output.status.success(), "cargo build failed");
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/rust_guest/target/wasm32-wasip1/release/rust_guest_fixture.wasm",
    );
    Ok((
        std::fs::read(wasm_path)?,
        WasmFunctionDescriptor {
            name: "echo".to_string(),
            function_type: "query".to_string(),
        },
    ))
}

/// Build the Go guest fixture, if the Go toolchain is available.
fn build_go_guest() -> anyhow::Result<Option<(Vec<u8>, WasmFunctionDescriptor)>> {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/go_guest",
    );
    let output = std::process::Command::new("go")
        .args(["build", "-buildmode=c-shared", "-o", "go_guest.wasm", "."])
        .current_dir(dir)
        .env("GOOS", "wasip1")
        .env("GOARCH", "wasm")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(output.status.success(), "go build failed");
    let wasm_path = format!("{dir}/go_guest.wasm");
    Ok(Some((
        std::fs::read(wasm_path)?,
        WasmFunctionDescriptor {
            name: "echo".to_string(),
            function_type: "query".to_string(),
        },
    )))
}

async fn load_database(rt: &ProdRuntime) -> anyhow::Result<Database<ProdRuntime>> {
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

/// Run the guest's `echo` function end to end against a fresh transaction.
async fn run_echo(
    runner: &WasmRunner,
    wasm: &[u8],
    database: &Database<ProdRuntime>,
    name: &str,
) -> anyhow::Result<()> {
    let tx = database.begin(Identity::Unknown(None)).await?;
    let (_, result) = run_wasm_udf(
        runner,
        wasm,
        WasmInput {
            function_name: name.to_string(),
            args_json: r#"["hello"]"#.to_string(),
        },
        tx,
        ComponentId::Root,
        [7u8; 32],
        UnixTimestamp::from_millis(1_700_000_000_000),
        WasmLimits::default(),
        false,
        None,
    )
    .await?;
    result.result?;
    Ok(())
}

/// The native Rust baseline: the same `echo` workload.
fn native_echo(value: &str) -> String {
    value.to_string()
}

async fn bench<F, Fut>(name: &str, iterations: usize, mut f: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    // Warm up.
    f().await?;
    f().await?;
    let start = Instant::now();
    for _ in 0..iterations {
        f().await?;
    }
    let elapsed = start.elapsed();
    let per_call = elapsed / iterations as u32;
    println!(
        "{name:46} {:>10.1} µs/call  ({iterations} iterations, {:?} total)",
        per_call.as_secs_f64() * 1e6,
        elapsed,
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let iterations = 500;
    let tokio = ProdRuntime::init_tokio()?;
    let rt = ProdRuntime::new(&tokio);
    rt.block_on("udf_bench", async {
        println!("== WASM UDF execution benchmark ({iterations} iterations each) ==\n");

        // Native baseline (no wasm boundary at all).
        let start = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(native_echo(std::hint::black_box("hello")));
        }
        let elapsed = start.elapsed();
        println!(
            "{:46} {:>10.1} µs/call",
            "native Rust (echo)",
            (elapsed / iterations as u32).as_secs_f64() * 1e6,
        );

        let (rust_wasm, rust_descriptor) = build_rust_guest()?;
        let runner = Arc::new(WasmRunner::new()?);
        let database = Arc::new(load_database(&rt).await?);

        // Rust WASM: first call includes module compilation.
        {
            let runner = runner.clone();
            let database = database.clone();
            let wasm = rust_wasm.clone();
            let name = rust_descriptor.name.clone();
            bench("Rust WASM: echo (cold, incl. compile)", 20, move || {
                let runner = runner.clone();
                let database = database.clone();
                let wasm = wasm.clone();
                let name = name.clone();
                async move { run_echo(&runner, &wasm, &database, &name).await }
            })
            .await?;
        }

        // Rust WASM: warm (compiled module cached).
        {
            let runner = runner.clone();
            let database = database.clone();
            let wasm = rust_wasm.clone();
            let name = rust_descriptor.name.clone();
            runner.get_or_compile_module(&wasm, &WasmLimits::default())?;
            bench("Rust WASM: echo (warm)", iterations, move || {
                let runner = runner.clone();
                let database = database.clone();
                let wasm = wasm.clone();
                let name = name.clone();
                async move { run_echo(&runner, &wasm, &database, &name).await }
            })
            .await?;
        }

        // Go WASM: warm.
        if let Some((go_wasm, go_descriptor)) = build_go_guest()? {
            let go_size = go_wasm.len();
            let runner = runner.clone();
            let database = database.clone();
            let wasm = go_wasm.clone();
            let name = go_descriptor.name.clone();
            runner.get_or_compile_module(&wasm, &WasmLimits::default())?;
            bench("Go WASM: echo (warm)", iterations, move || {
                let runner = runner.clone();
                let database = database.clone();
                let wasm = wasm.clone();
                let name = name.clone();
                async move { run_echo(&runner, &wasm, &database, &name).await }
            })
            .await?;
            println!("\nGo guest wasm size: {go_size} bytes (native Go toolchain)");
        } else {
            println!("\nGo toolchain not found; skipping the Go benchmark");
        }

        println!();
        println!("All benchmarks include: begin tx, module lookup, wasmtime instantiate,");
        println!("host function setup, guest execution, result parse, tx teardown.");
        Ok(())
    })
}
