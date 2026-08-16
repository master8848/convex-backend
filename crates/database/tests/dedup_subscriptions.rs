//! Integration tests for subscription deduplication: identical query
//! subscriptions across clients share a single manager entry, the entry is
//! released when the last client disconnects, and a stale released entry is
//! never reused by a later subscriber.

use std::{
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use common::{
    persistence::Persistence,
    runtime::new_unlimited_rate_limiter,
    shutdown::ShutdownSignal,
    types::{
        TableName,
        Timestamp,
    },
    virtual_system_mapping::VirtualSystemMapping,
};
use database::{
    Database,
    Subscription,
    SystemMetadataModel,
    TableModel,
    Token,
    UserFacingModel,
};
use indexing::index_cache::IndexCache;
use keybroker::Identity;
use runtime::prod::ProdRuntime;
use search::{
    searcher::SearcherStub,
    Searcher,
};
use serde_json::json;
use sqlite::SqlitePersistence;
use tokio::sync::mpsc;
use value::{
    ConvexObject,
    ResolvedDocumentId,
    TableNamespace,
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

/// Insert a user document without UDFs, bypassing authorization.
async fn insert_doc(
    database: &Database<ProdRuntime>,
    table_name: &TableName,
    body: serde_json::Value,
) -> anyhow::Result<ResolvedDocumentId> {
    let mut tx = database.begin(Identity::Unknown(None)).await?;
    let doc_id = SystemMetadataModel::new(&mut tx, TableNamespace::Global)
        .insert_metadata(table_name, ConvexObject::try_from(body)?)
        .await?;
    database
        .commit_with_write_source(tx, database::WriteSource::System("test"))
        .await?;
    Ok(doc_id)
}

/// Replace a user document, writing its by_id index key (no UDFs needed).
async fn replace_doc(
    database: &Database<ProdRuntime>,
    doc_id: ResolvedDocumentId,
    body: serde_json::Value,
) -> anyhow::Result<()> {
    let mut tx = database.begin(Identity::Unknown(None)).await?;
    UserFacingModel::new(&mut tx, TableNamespace::Global)
        .replace(doc_id.developer_id, ConvexObject::try_from(body)?)
        .await?;
    database
        .commit_with_write_source(tx, database::WriteSource::System("test"))
        .await?;
    Ok(())
}

/// Fresh database with a `counters` table containing one document, plus a
/// token that read that document (a by_id point interval, so a write to the
/// same document invalidates the subscription).
async fn setup(
    rt: &ProdRuntime,
) -> anyhow::Result<(Database<ProdRuntime>, TableName, ResolvedDocumentId, Token)> {
    let (persistence, database) = new_database(rt).await?;
    let database = create_table(&database, &persistence, rt, "counters").await?;
    let table_name: TableName = "counters".parse()?;
    let doc_id = insert_doc(&database, &table_name, json!({ "value": "doc1" })).await?;

    let mut tx = database.begin(Identity::Unknown(None)).await?;
    let doc = tx.get(doc_id).await?;
    anyhow::ensure!(doc.is_some(), "doc1 should be readable");
    let (reads, _writes) = tx.into_reads_and_writes();
    let token = Token::new(
        Arc::new(reads.into_read_set()),
        *database.now_ts_for_reads(),
    );
    Ok((database, table_name, doc_id, token))
}

/// Wait for the dedup map to release the shared entry after the last handle
/// is dropped. The release is dispatched through a spawned task, so poll.
async fn wait_for_shared_release(database: &Database<ProdRuntime>) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while database.test_subscriptions_shared_len() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("shared entry should be released after the last handle drops")?;
    Ok(())
}

/// Wait for an invalidation, with a timeout so regressions fail instead of
/// hanging the test binary.
async fn wait_for_invalidation(subscription: &Subscription) -> anyhow::Result<Option<Timestamp>> {
    tokio::time::timeout(
        Duration::from_secs(10),
        subscription.wait_for_invalidation(),
    )
    .await
    .context("timed out waiting for subscription invalidation")
}

/// Two clients with identical read sets share a single dedup entry, and the
/// entry stays alive as long as at least one client holds it.
#[test]
fn test_dedup_shared_lifetime() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let rt = ProdRuntime::new(&tokio);
        let (database, _table_name, doc_id, token) = setup(&rt).await?;

        let sub1 = database.subscribe(token.clone()).await?;
        let sub2 = database.subscribe(token).await?;
        assert_eq!(database.test_subscriptions_shared_len(), 1);

        // One client dropping must not release the shared entry.
        drop(sub1);
        assert_eq!(database.test_subscriptions_shared_len(), 1);

        // A write to the table invalidates the surviving subscription.
        replace_doc(&database, doc_id, json!({ "value": "doc2" })).await?;
        let invalid_ts = wait_for_invalidation(&sub2).await?;
        anyhow::ensure!(
            invalid_ts.is_some(),
            "subscription invalidated without a timestamp"
        );
        anyhow::Ok(())
    })
}

/// The dedup entry is released when the last client handle drops (leak
/// regression: `users` used to never reach zero, pinning the manager entry
/// forever).
#[test]
fn test_release_on_last_drop() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let rt = ProdRuntime::new(&tokio);
        let (database, _table_name, _doc_id, token) = setup(&rt).await?;

        let sub1 = database.subscribe(token.clone()).await?;
        let sub2 = database.subscribe(token).await?;
        assert_eq!(database.test_subscriptions_shared_len(), 1);

        drop(sub1);
        drop(sub2);
        wait_for_shared_release(&database).await?;
        anyhow::Ok(())
    })
}

/// After the last handle drops and the entry is released, a later subscriber
/// with the same read set creates a fresh entry and still receives
/// invalidations (dead-reuse regression: the old code reused the released
/// entry and silently never invalidated).
#[test]
fn test_fresh_entry_after_release() -> anyhow::Result<()> {
    let tokio = ProdRuntime::init_tokio()?;
    tokio.block_on(async {
        let rt = ProdRuntime::new(&tokio);
        let (database, _table_name, doc_id, token) = setup(&rt).await?;

        let sub1 = database.subscribe(token.clone()).await?;
        let sub2 = database.subscribe(token.clone()).await?;
        assert_eq!(database.test_subscriptions_shared_len(), 1);

        drop(sub1);
        drop(sub2);
        wait_for_shared_release(&database).await?;

        // A later subscriber with the same read set must get a fresh entry.
        let sub3 = database.subscribe(token).await?;
        assert_eq!(database.test_subscriptions_shared_len(), 1);

        replace_doc(&database, doc_id, json!({ "value": "doc3" })).await?;
        let invalid_ts = wait_for_invalidation(&sub3).await?;
        anyhow::ensure!(
            invalid_ts.is_some(),
            "subscription invalidated without a timestamp"
        );
        anyhow::Ok(())
    })
}
