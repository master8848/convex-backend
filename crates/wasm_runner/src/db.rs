//! Database host functions, mirroring the isolate's database syscalls.
//!
//! Each host function receives a JSON object of arguments and returns a
//! packed `(offset, len)` pair into host-managed call data holding the JSON
//! envelope `{"ok": <value>}` or `{"err": <message>}`.

use std::sync::Arc;

use anyhow::Context;
use common::{
    components::ComponentId,
    document::DeveloperDocument,
    query::{
        Order,
        Query,
    },
    runtime::Runtime,
    types::WriteTimestamp,
    version::Version,
};
use database::{
    query::{
        DeveloperQuery,
        TableFilter,
    },
    Transaction,
    UserFacingModel,
};
use serde::Deserialize;
use serde_json::{
    json,
    Value as JsonValue,
};
use value::{
    DeveloperDocumentId,
    PendingValue,
    TableName,
    TableNamespace,
};

use crate::abi::{
    pack_result,
    DB_ERROR,
};

/// State shared between the store and the async database host functions.
pub(crate) struct DbShared<RT: Runtime> {
    pub tx: tokio::sync::Mutex<Transaction<RT>>,
    pub call_data: Arc<std::sync::Mutex<Vec<u8>>>,
    pub component: ComponentId,
    pub max_call_data: usize,
}

/// Checks if the underlying table and the request's expectation for the table
/// line up.
fn system_table_guard(name: &TableName, expect_system_table: bool) -> anyhow::Result<()> {
    if expect_system_table && !name.is_system() {
        anyhow::bail!("SystemTableError: user tables cannot be accessed with db.system");
    } else if !expect_system_table && name.is_system() {
        anyhow::bail!("SystemTableError: system tables can only be accessed with db.system");
    }
    Ok(())
}

fn check_table_name(provided: &Option<String>, actual: &TableName) -> anyhow::Result<()> {
    if let Some(provided) = provided {
        let provided: TableName = provided.parse().context("table")?;
        anyhow::ensure!(
            provided == *actual,
            "table {} does not match the table for the given id, which is {}",
            provided,
            actual,
        );
    }
    Ok(())
}

/// Serialize a developer document to its internal JSON representation,
/// resolving staged writes when the document was modified in this
/// transaction.
fn developer_document_to_json<RT: Runtime>(
    tx: &mut Transaction<RT>,
    namespace: TableNamespace,
    document: &DeveloperDocument,
    ts: WriteTimestamp,
) -> anyhow::Result<JsonValue> {
    if ts == WriteTimestamp::Pending {
        let id = document.id();
        if tx
            .table_mapping()
            .namespace(namespace)
            .table_number_exists()(id.table())
        {
            let id = tx.resolve_developer_id(&id, namespace)?;
            if let Some(update) = tx.pending_write(&id)
                && !update.is_resolved()
            {
                return update
                    .new_document_internal_json()
                    .context("Staged write for a returned document has no new document");
            }
        }
    }
    Ok(document.to_internal_json())
}

/// Append bytes to host-managed call data, returning the `(offset, len)`
/// packed into an `i64`, or `DB_ERROR` if the call-data limit would be
/// exceeded.
async fn write_to_call_data(shared: &DbShared<impl Runtime>, bytes: Vec<u8>) -> i64 {
    let mut call_data = shared.call_data.lock().unwrap_or_else(|e| e.into_inner());
    let offset = call_data.len();
    if offset
        .checked_add(bytes.len())
        .is_none_or(|total| total > shared.max_call_data)
    {
        return DB_ERROR;
    }
    call_data.extend_from_slice(&bytes);
    pack_result(
        u32::try_from(offset).unwrap_or(u32::MAX),
        u32::try_from(bytes.len()).unwrap_or(u32::MAX),
    )
}

/// Serialize a DB operation's result into the `{"ok" | "err"}` envelope and
/// write it to call data.
async fn write_result<RT: Runtime>(
    shared: &DbShared<RT>,
    result: anyhow::Result<JsonValue>,
) -> i64 {
    let envelope = match result {
        Ok(value) => json!({ "ok": value }),
        Err(e) => json!({ "err": e.to_string() }),
    };
    let bytes = match serde_json::to_vec(&envelope) {
        Ok(bytes) => bytes,
        Err(_) => return DB_ERROR,
    };
    write_to_call_data(shared, bytes).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetArgs {
    #[serde(default)]
    table: Option<String>,
    id: String,
    #[serde(default)]
    version: Option<String>,
}

/// `db.get(table, id)` -> document or null.
pub(crate) async fn db_get<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: GetArgs = serde_json::from_slice(args).context("db.get")?;
        let id = DeveloperDocumentId::decode(&args.id).context("db.get: invalid id")?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let namespace = component.into();
        let table_name =
            match tx.resolve_idv6(id, namespace, TableFilter::ExcludePrivateSystemTables) {
                Ok(table_name) => {
                    check_table_name(&args.table, &table_name)?;
                    system_table_guard(&table_name, false)?;
                    Some(table_name)
                },
                // Get on a non-existent table should return null.
                Err(_) => None,
            };
        let Some(table_name) = table_name else {
            return Ok(JsonValue::Null);
        };
        let version = parse_version(args.version)?;
        let query = Query::get(table_name, id);
        let mut query = DeveloperQuery::new_with_version(
            &mut tx,
            namespace,
            query,
            version,
            TableFilter::ExcludePrivateSystemTables,
        )?;
        match query.next_with_ts(&mut tx, Some(1)).await? {
            Some((document, ts)) => developer_document_to_json(&mut tx, namespace, &document, ts),
            None => Ok(JsonValue::Null),
        }
    }
    .await;
    write_result(shared, result).await
}

fn parse_version(version: Option<String>) -> anyhow::Result<Option<Version>> {
    version
        .map(|v| v.parse())
        .transpose()
        .context("db.get: invalid version")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableArgs {
    table: String,
}

/// `db.count(table)` -> number.
pub(crate) async fn db_count<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: TableArgs = serde_json::from_slice(args).context("db.count")?;
        let table: TableName = args.table.parse().context("db.count: invalid table")?;
        system_table_guard(&table, false)?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let count = tx
            .count(component.into(), &table)
            .await
            .context("db.count")?;
        let Some(count) = count else {
            anyhow::bail!("db.count: table count unavailable while bootstrapping");
        };
        let count = u32::try_from(count).context("db.count: count too large")?;
        Ok(JsonValue::from(f64::from(count)))
    }
    .await;
    write_result(shared, result).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InsertArgs {
    table: String,
    value: JsonValue,
}

/// `db.insert(table, value)` -> `{_id: string}`.
pub(crate) async fn db_insert<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: InsertArgs = serde_json::from_slice(args).context("db.insert")?;
        let value =
            PendingValue::from_uncommitted_json(args.value).context("db.insert: invalid value")?;
        if !value.is_object() {
            anyhow::bail!("db.insert: value must be an object");
        }
        let table: TableName = args.table.parse().context("db.insert: invalid table")?;
        system_table_guard(&table, false)?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let document_id = UserFacingModel::new(&mut tx, component.into())
            .insert(table, value)
            .await
            .context("db.insert")?;
        Ok(json!({ "_id": document_id.encode() }))
    }
    .await;
    write_result(shared, result).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateArgs {
    #[serde(default)]
    table: Option<String>,
    id: String,
    value: JsonValue,
}

/// `db.replace(id, value)` -> replaced document.
pub(crate) async fn db_replace<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: UpdateArgs = serde_json::from_slice(args).context("db.replace")?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let namespace = component.into();
        let id = DeveloperDocumentId::decode(&args.id).context("db.replace: invalid id")?;
        let table_name = tx
            .resolve_idv6(id, namespace, TableFilter::ExcludePrivateSystemTables)
            .context("db.replace: id")?;
        check_table_name(&args.table, &table_name)?;
        system_table_guard(&table_name, false)?;
        let value =
            PendingValue::from_uncommitted_json(args.value).context("db.replace: invalid value")?;
        if !value.is_object() {
            anyhow::bail!("db.replace: value must be an object");
        }
        let document = UserFacingModel::new(&mut tx, namespace)
            .replace(id, value)
            .await
            .context("db.replace")?;
        Ok(document.to_uncommitted_internal_json())
    }
    .await;
    write_result(shared, result).await
}

/// `db.patch(id, value)` -> patched document.
pub(crate) async fn db_patch<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: UpdateArgs = serde_json::from_slice(args).context("db.patch")?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let namespace = component.into();
        let id = DeveloperDocumentId::decode(&args.id).context("db.patch: invalid id")?;
        let table_name = tx
            .resolve_idv6(id, namespace, TableFilter::ExcludePrivateSystemTables)
            .context("db.patch: id")?;
        check_table_name(&args.table, &table_name)?;
        system_table_guard(&table_name, false)?;
        let value = database::PatchValue::from_uncommitted_json(args.value)
            .context("db.patch: invalid value")?;
        let document = UserFacingModel::new(&mut tx, namespace)
            .patch(id, value)
            .await
            .context("db.patch")?;
        Ok(document.to_uncommitted_internal_json())
    }
    .await;
    write_result(shared, result).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteArgs {
    #[serde(default)]
    table: Option<String>,
    id: String,
}

/// `db.delete(id)` -> deleted document.
pub(crate) async fn db_delete<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: DeleteArgs = serde_json::from_slice(args).context("db.delete")?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let namespace = component.into();
        let id = DeveloperDocumentId::decode(&args.id).context("db.delete: invalid id")?;
        let table_name = tx
            .resolve_idv6(id, namespace, TableFilter::ExcludePrivateSystemTables)
            .context("db.delete: id")?;
        check_table_name(&args.table, &table_name)?;
        system_table_guard(&table_name, false)?;
        let document = UserFacingModel::new(&mut tx, namespace)
            .delete(id)
            .await
            .context("db.delete")?;
        Ok(document.to_internal_json())
    }
    .await;
    write_result(shared, result).await
}

/// `db.query(table)` -> array of documents (full table scan).
///
/// Filters, indexes, ordering, and pagination are not yet supported: the
/// guest SDK's `db.query` returns all documents in the table. See
/// `docs/wasm.md` for the full list of limitations.
pub(crate) async fn db_query<RT: Runtime>(shared: &DbShared<RT>, args: &[u8]) -> i64 {
    let result = async {
        let args: TableArgs = serde_json::from_slice(args).context("db.query")?;
        let table: TableName = args.table.parse().context("db.query: invalid table")?;
        system_table_guard(&table, false)?;
        let component = shared.component;
        let mut tx = shared.tx.lock().await;
        let namespace = component.into();
        let mut query = DeveloperQuery::new_with_version(
            &mut tx,
            namespace,
            Query::full_table_scan(table, Order::Asc),
            None,
            TableFilter::ExcludePrivateSystemTables,
        )?;
        let mut documents = Vec::new();
        loop {
            let Some((document, ts)) = query.next_with_ts(&mut tx, Some(128)).await? else {
                break;
            };
            documents.push(developer_document_to_json(
                &mut tx, namespace, &document, ts,
            )?);
        }
        Ok(JsonValue::Array(documents))
    }
    .await;
    write_result(shared, result).await
}
