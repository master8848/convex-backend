use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt::Debug,
    sync::Arc,
};

use anyhow::Context;
use async_trait::async_trait;
use common::{
    auth::AuthConfig,
    bootstrap_model::components::definition::ComponentDefinitionMetadata,
    components::{
        CanonicalizedComponentModulePath,
        ComponentDefinitionPath,
        ComponentName,
        Resource,
    },
    errors::JsError,
    execution_context::ExecutionContext,
    http::{
        fetch::FetchClient,
        RoutedHttpPath,
    },
    knobs::{
        ISOLATE_EXECUTION_ENABLED,
        MAX_ISOLATE_WORKERS,
    },
    log_lines::LogLine,
    persistence::RetentionValidator,
    query_journal::QueryJournal,
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    schemas::DatabaseSchema,
    types::{
        ConvexOrigin,
        DeploymentMetadata,
        IndexId,
        ModuleEnvironment,
        UdfType,
    },
};
use database::{
    BootstrapMetadata,
    TableCountSnapshot,
    Transaction,
    TransactionTextSnapshot,
};
use errors::ErrorMetadata;
use file_storage::TransactionalFileStorage;
use futures::FutureExt;
use indexing::index_reader::IndexReader;
use isolate::{
    client::{
        EnvironmentData,
        IsolateWorker,
    },
    IsolateClient,
};
use keybroker::{
    FunctionRunnerKeyBroker,
    Identity,
};
use model::{
    components::auth::propagate_component_auth,
    config::types::ModuleConfig,
    environment_variables::types::{
        EnvVarName,
        EnvVarValue,
    },
    modules::{
        language::is_wasm_environment,
        module_versions::{
            AnalyzedModule,
            ModuleSource,
            SourceMap,
        },
        ModuleModel,
    },
    source_packages::{
        upload_download::download_package,
        SourcePackageModel,
    },
    udf_config::types::UdfConfig,
};
use rand::Rng;
use storage::{
    Storage,
    StorageUseCase,
};
use sync_types::{
    CanonicalizedModulePath,
    Timestamp,
};
use tokio::sync::{
    mpsc,
    oneshot,
};
use udf::{
    validation::{
        ValidatedHttpPath,
        ValidatedPathAndArgs,
    },
    ActionCallbacks,
    ActionOutcome,
    EvaluateAppDefinitionsResult,
    FunctionOutcome,
    HttpActionRequest as HttpActionRequestInner,
    HttpActionResponseStreamer,
    SyscallTrace,
    UdfOutcome,
};
use usage_tracking::{
    FunctionUsageStats,
    FunctionUsageTracker,
};
use value::{
    identifier::Identifier,
    JsonPackedValue,
    MAX_COMMIT_TS,
};

use super::in_memory_indexes::InMemoryIndexCache;
use crate::{
    module_cache::{
        CodeCache,
        FunctionRunnerModuleLoader,
        ModuleCache,
    },
    FunctionFinalTransaction,
    FunctionWrites,
};

pub struct RunRequestArgs {
    pub key_broker: FunctionRunnerKeyBroker,
    pub index_reader: Arc<dyn IndexReader>,
    pub convex_origin: ConvexOrigin,
    pub bootstrap_metadata: BootstrapMetadata,
    pub table_count_snapshot: Arc<dyn TableCountSnapshot>,
    pub text_index_snapshot: Arc<dyn TransactionTextSnapshot>,
    pub action_callbacks: Arc<dyn ActionCallbacks>,
    pub fetch_client: Arc<dyn FetchClient>,
    pub log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
    pub function_started_sender: Option<oneshot::Sender<()>>,
    pub udf_type: UdfType,
    pub identity: Identity,
    pub existing_writes: FunctionWrites,
    pub default_system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
    pub in_memory_index_last_modified: BTreeMap<IndexId, Timestamp>,
    pub context: ExecutionContext,
    pub subfunctions_in_same_isolate: bool,
    pub deployment: DeploymentMetadata,
}

#[derive(Clone)]
pub struct FunctionMetadata {
    pub path_and_args: ValidatedPathAndArgs,
    pub journal: QueryJournal,
}

pub struct HttpActionMetadata {
    pub http_response_streamer: HttpActionResponseStreamer,
    pub http_module_path: ValidatedHttpPath,
    pub routed_path: RoutedHttpPath,
    pub http_request: HttpActionRequestInner,
}

#[async_trait]
pub trait StorageForDeployment<RT: Runtime>: Debug + Clone + Send + Sync + 'static {
    /// Gets a storage impl for a deployment. Agnostic to what kind of storage -
    /// local or s3, or how it was loaded (e.g. passed directly within backend,
    /// loaded from a transaction created in Funrun)
    async fn storage_for_deployment(
        &self,
        transaction: &mut Transaction<RT>,
        use_case: StorageUseCase,
    ) -> anyhow::Result<Arc<dyn Storage>>;
}

#[derive(Clone, Debug)]
pub struct DeploymentStorage {
    pub files_storage: Arc<dyn Storage>,
    pub modules_storage: Arc<dyn Storage>,
}

#[async_trait]
impl<RT: Runtime> StorageForDeployment<RT> for DeploymentStorage {
    async fn storage_for_deployment(
        &self,
        _transaction: &mut Transaction<RT>,
        use_case: StorageUseCase,
    ) -> anyhow::Result<Arc<dyn Storage>> {
        match use_case {
            StorageUseCase::Files => Ok(self.files_storage.clone()),
            StorageUseCase::Modules => Ok(self.modules_storage.clone()),
            _ => anyhow::bail!("function runner storage does not support {use_case}"),
        }
    }
}

pub struct FunctionRunnerCore<RT: Runtime, S: StorageForDeployment<RT>> {
    rt: RT,
    storage: S,
    index_cache: InMemoryIndexCache<RT>,
    module_cache: ModuleCache<RT>,
    code_cache: CodeCache,
    isolate_client: Option<IsolateClient<RT>>,
    wasm_runner: Arc<wasm_runner::WasmRunner>,
}

impl<RT: Runtime, S: StorageForDeployment<RT>> Clone for FunctionRunnerCore<RT, S> {
    fn clone(&self) -> Self {
        Self {
            rt: self.rt.clone(),
            storage: self.storage.clone(),
            index_cache: self.index_cache.clone(),
            module_cache: self.module_cache.clone(),
            code_cache: self.code_cache.clone(),
            isolate_client: self.isolate_client.clone(),
            wasm_runner: self.wasm_runner.clone(),
        }
    }
}

#[fastrace::trace]
pub async fn validate_run_function_result(
    udf_type: UdfType,
    ts: Timestamp,
    retention_validator: Arc<dyn RetentionValidator>,
) -> anyhow::Result<()> {
    match udf_type {
        // Since queries and mutations have no side effects, we perform the
        // retention check here, when validating the result.
        UdfType::Query | UdfType::Mutation => retention_validator
            .validate_snapshot(ts)
            .await
            .context("Function runner retention check changed"),
        // Since Actions can have side effects, we have to validate their
        // retention while we run them. We can't perform an additional check
        // here since actions can run longer than the retention.
        UdfType::Action | UdfType::HttpAction => Ok(()),
    }
}

impl<RT: Runtime, S: StorageForDeployment<RT>> FunctionRunnerCore<RT, S> {
    pub fn new<W: IsolateWorker<RT>>(
        rt: RT,
        storage: S,
        max_percent_per_client: usize,
        isolate_worker: W,
    ) -> anyhow::Result<Self> {
        Self::_new(
            rt,
            storage,
            max_percent_per_client,
            isolate_worker,
            *ISOLATE_EXECUTION_ENABLED,
        )
    }

    pub fn _new<W: IsolateWorker<RT>>(
        rt: RT,
        storage: S,
        max_percent_per_client: usize,
        isolate_worker: W,
        isolate_execution_enabled: bool,
    ) -> anyhow::Result<Self> {
        // Constructing the isolate client eagerly initializes V8, loads ICU, and
        // builds the UDF runtime snapshot. For wasm-only deployments we skip it
        // entirely so the process never touches the JS engine.
        let isolate_client = if isolate_execution_enabled {
            Some(IsolateClient::new(
                rt.clone(),
                max_percent_per_client,
                *MAX_ISOLATE_WORKERS,
                isolate_worker,
            )?)
        } else {
            None
        };
        let index_cache = InMemoryIndexCache::new(rt.clone());
        let module_cache = ModuleCache::new(rt.clone());
        let code_cache = CodeCache::new();
        let wasm_runner = Arc::new(wasm_runner::WasmRunner::new()?);

        Ok(Self {
            rt,
            storage,
            index_cache,
            module_cache,
            code_cache,
            isolate_client,
            wasm_runner,
        })
    }

    pub fn active_isolate_workers(&self) -> usize {
        self.isolate_client
            .as_ref()
            .map_or(0, |c| c.active_workers())
    }

    pub fn max_isolate_workers(&self) -> usize {
        self.isolate_client.as_ref().map_or(0, |c| c.max_workers())
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        if let Some(isolate_client) = &self.isolate_client {
            isolate_client.shutdown().await?;
        }
        Ok(())
    }

    // Runs a function given the information for the backend as well as arguments
    // to the function itself.
    // NOTE: The caller of this is responsible of checking retention by calling
    // `validate_function_runner_result`. If the retention check fails, we should
    // ignore any results or errors returned by this method.
    #[fastrace::trace]
    pub async fn run_function_no_retention_check(
        &self,
        run_request_args: RunRequestArgs,
        function_metadata: Option<FunctionMetadata>,
        http_action_metadata: Option<HttpActionMetadata>,
    ) -> anyhow::Result<(
        Option<FunctionFinalTransaction>,
        FunctionOutcome,
        FunctionUsageStats,
    )> {
        self.run_function_no_retention_check_inner(
            run_request_args,
            function_metadata,
            http_action_metadata,
        )
        .boxed()
        .await
    }

    pub async fn run_function_no_retention_check_inner(
        &self,
        RunRequestArgs {
            key_broker,
            index_reader,
            convex_origin,
            bootstrap_metadata,
            table_count_snapshot,
            text_index_snapshot,
            action_callbacks,
            fetch_client,
            log_line_sender,
            function_started_sender,
            udf_type,
            identity,
            existing_writes,
            default_system_env_vars,
            in_memory_index_last_modified,
            context,
            subfunctions_in_same_isolate,
            deployment,
        }: RunRequestArgs,
        function_metadata: Option<FunctionMetadata>,
        http_action_metadata: Option<HttpActionMetadata>,
    ) -> anyhow::Result<(
        Option<FunctionFinalTransaction>,
        FunctionOutcome,
        FunctionUsageStats,
    )> {
        let deployment_name = deployment.name.clone();
        let usage_tracker = FunctionUsageTracker::new();
        let mut transaction = self
            .index_cache
            .begin_tx(
                identity.clone(),
                existing_writes,
                index_reader,
                deployment_name.clone(),
                in_memory_index_last_modified,
                bootstrap_metadata,
                table_count_snapshot,
                text_index_snapshot,
                usage_tracker.clone(),
            )
            .await?;
        let storage = self
            .storage
            .storage_for_deployment(&mut transaction, StorageUseCase::Files)
            .await?;
        let file_storage = TransactionalFileStorage::new(self.rt.clone(), storage, convex_origin);
        let modules_storage = self
            .storage
            .storage_for_deployment(&mut transaction, StorageUseCase::Modules)
            .await?;

        let environment_data = EnvironmentData {
            key_broker,
            default_system_env_vars,
            file_storage,
            module_loader: Arc::new(FunctionRunnerModuleLoader {
                deployment_name: deployment_name.clone(),
                cache: self.module_cache.clone(),
                code_cache: self.code_cache.clone(),
                modules_storage: modules_storage.clone(),
            }),
            deployment,
        };

        match udf_type {
            UdfType::Query | UdfType::Mutation => {
                let FunctionMetadata {
                    path_and_args,
                    journal,
                } = function_metadata.context("Missing function metadata for query or mutation")?;
                // Initialize the UDF's RNG from some high-quality entropy. As with
                // `unix_timestamp` below, the UDF is only deterministic modulo this
                // system-generated input.
                let rng_seed = self.rt.rng().random();
                let unix_timestamp = self.rt.unix_timestamp();
                // Route WASM before isolate. Per-env semaphore (64, GC 64+32MiB).
                if is_wasm_environment(
                    &self
                        .module_environment(&mut transaction, path_and_args.path())
                        .await?,
                ) {
                    let _permit = self.wasm_runner.execution_semaphore_for_env(&deployment_name).acquire_owned().await.context("WASM semaphore closed")?;
                    let (tx, outcome) = self
                        .execute_wasm_udf(
                            udf_type,
                            path_and_args,
                            transaction,
                            rng_seed,
                            unix_timestamp,
                            log_line_sender,
                            modules_storage,
                        )
                        .await?;
                    let outcome = match udf_type {
                        UdfType::Query => FunctionOutcome::Query(outcome),
                        UdfType::Mutation => FunctionOutcome::Mutation(outcome),
                        UdfType::Action | UdfType::HttpAction => {
                            anyhow::bail!("WASM functions do not support {udf_type:?} here")
                        },
                    };
                    return Ok((
                        Some(tx.try_into()?),
                        outcome,
                        usage_tracker.gather_user_stats(),
                    ));
                }
                let (tx, outcome) = self
                    .require_isolate_client()?
                    .execute_udf(
                        udf_type,
                        path_and_args,
                        transaction,
                        journal,
                        context,
                        environment_data,
                        rng_seed,
                        unix_timestamp,
                        0,
                        deployment_name,
                        function_started_sender,
                        subfunctions_in_same_isolate,
                    )
                    .await?;
                Ok((
                    Some(tx.try_into()?),
                    outcome,
                    usage_tracker.gather_user_stats(),
                ))
            },
            UdfType::Action => {
                let FunctionMetadata { path_and_args, .. } =
                    function_metadata.context("Missing function metadata for action")?;
                let log_line_sender =
                    log_line_sender.context("Missing log line sender for action")?;
                if is_wasm_environment(
                    &self
                        .module_environment(&mut transaction, path_and_args.path())
                        .await?,
                ) {
                    let _permit = self.wasm_runner.execution_semaphore_for_env(&deployment_name).acquire_owned().await.context("WASM semaphore closed")?;
                    let rng_seed = self.rt.rng().random();
                    let unix_timestamp = self.rt.unix_timestamp();
                    let (path, arguments, npm_version) = path_and_args.consume();
                    let identity = transaction.inert_identity();
                    let wasm_binary = self
                        .fetch_wasm_binary(&mut transaction, &path, modules_storage)
                        .await?;
                    let function_name = path.udf_path.function_name().to_string();
                    let args_json = arguments.get().to_string();
                    let (_, result) = wasm_runner::run_wasm_udf(
                        &self.wasm_runner,
                        &wasm_binary,
                        wasm_runner::WasmInput {
                            function_name,
                            args_json,
                        },
                        transaction,
                        path.component,
                        rng_seed,
                        unix_timestamp,
                        wasm_runner::WasmLimits::default(),
                        /* allow_unresolved_commit_ts */ false,
                        Some(log_line_sender),
                    )
                    .await?;
                    let action_result = result.result.and_then(|packed| {
                        let pending = packed
                            .unpack()
                            .map_err(|e| JsError::from_message(e.to_string()))?;
                        let value = pending
                            .resolve(MAX_COMMIT_TS)
                            .map_err(|e| JsError::from_message(e.to_string()))?
                            .into_owned();
                        Ok(JsonPackedValue::pack(value))
                    });
                    let outcome = ActionOutcome {
                        path: path.for_logging(),
                        arguments,
                        identity,
                        unix_timestamp,
                        result: action_result,
                        syscall_trace: SyscallTrace::new(),
                        udf_server_version: npm_version,
                        user_execution_time: None,
                    };
                    return Ok((
                        None,
                        FunctionOutcome::Action(outcome),
                        usage_tracker.gather_user_stats(),
                    ));
                }
                let outcome = self
                    .require_isolate_client()?
                    .execute_action(
                        path_and_args,
                        transaction,
                        action_callbacks,
                        fetch_client,
                        log_line_sender,
                        context,
                        environment_data,
                        deployment_name,
                        function_started_sender,
                    )
                    .await?;
                Ok((
                    None,
                    FunctionOutcome::Action(outcome),
                    usage_tracker.gather_user_stats(),
                ))
            },
            UdfType::HttpAction => {
                let HttpActionMetadata {
                    http_response_streamer,
                    http_module_path,
                    routed_path,
                    http_request,
                } = http_action_metadata.context("Missing http action metadata")?;
                let log_line_sender =
                    log_line_sender.context("Missing log line sender for http action")?;
                // Set the proper identity for component HTTP actions. Note that for HTTP,
                // the component is both the caller and the callee.
                let component_id = http_module_path.path().component;
                let identity =
                    propagate_component_auth(&identity, component_id, component_id.is_root());
                let outcome = self
                    .require_isolate_client()?
                    .execute_http_action(
                        http_module_path,
                        routed_path,
                        http_request,
                        identity,
                        action_callbacks,
                        fetch_client,
                        log_line_sender,
                        http_response_streamer,
                        transaction,
                        context,
                        environment_data,
                        deployment_name,
                        function_started_sender,
                    )
                    .await?;
                Ok((
                    None,
                    FunctionOutcome::HttpAction(outcome),
                    usage_tracker.gather_user_stats(),
                ))
            },
        }
    }

    /// Fetch the module environment for the module containing the given
    /// function path.
    async fn module_environment(
        &self,
        transaction: &mut Transaction<RT>,
        path: &common::components::ResolvedComponentFunctionPath,
    ) -> anyhow::Result<ModuleEnvironment> {
        let module_path = CanonicalizedComponentModulePath {
            component: path.component,
            module_path: path.udf_path.module().clone(),
        };
        let Some(metadata) = ModuleModel::new(transaction)
            .get_metadata(module_path)
            .await?
        else {
            anyhow::bail!(
                "Trying to execute {:?} but its module is not found",
                path.udf_path
            );
        };
        Ok(metadata.environment)
    }

    /// Fetch the wasm binary for a module and execute a UDF in it.
    ///
    /// The module's source, as stored in the source package, is a base64
    /// encoding of the wasm binary. See `docs/wasm.md` for the deployment
    /// contract.
    async fn fetch_wasm_binary(
        &self,
        transaction: &mut Transaction<RT>,
        path: &common::components::ResolvedComponentFunctionPath,
        modules_storage: Arc<dyn Storage>,
    ) -> anyhow::Result<Vec<u8>> {
        let component = path.component;
        let module_path = CanonicalizedComponentModulePath {
            component,
            module_path: path.udf_path.module().clone(),
        };
        let Some(metadata) = ModuleModel::new(transaction)
            .get_metadata(module_path)
            .await?
        else {
            anyhow::bail!(
                "Trying to execute {:?} but its module is not found",
                path.udf_path
            );
        };
        anyhow::ensure!(
            is_wasm_environment(&metadata.environment),
            "Trying to execute {:?} as a WASM module, but it is bundled for {:?}",
            path.udf_path,
            metadata.environment,
        );
        let source_package = SourcePackageModel::new(transaction, component.into())
            .get(metadata.source_package_id)
            .await?;
        let modules = download_package(modules_storage, &source_package).await?;
        let module_path: CanonicalizedModulePath = path.udf_path.module().clone();
        let module = modules
            .get(&module_path)
            .with_context(|| format!("Module {:?} not found in source package", module_path))?;
        let module_source: &str = module.source.as_ref();
        let wasm_bytes = base64::decode(module_source.trim())
            .context("WASM module source is not valid base64")?;
        Ok(wasm_bytes)
    }

    /// Execute a UDF from a WASM module.
    async fn execute_wasm_udf(
        &self,
        udf_type: UdfType,
        path_and_args: ValidatedPathAndArgs,
        transaction: Transaction<RT>,
        rng_seed: [u8; 32],
        unix_timestamp: UnixTimestamp,
        log_line_sender: Option<mpsc::UnboundedSender<LogLine>>,
        modules_storage: Arc<dyn Storage>,
    ) -> anyhow::Result<(Transaction<RT>, UdfOutcome)> {
        let (path, arguments, npm_version) = path_and_args.consume();
        let identity = transaction.inert_identity();
        let mut transaction = transaction;
        let wasm_binary = self
            .fetch_wasm_binary(&mut transaction, &path, modules_storage)
            .await?;
        let function_name = path.udf_path.function_name().to_string();
        let args_json = arguments.get().to_string();
        let (transaction, result) = wasm_runner::run_wasm_udf(
            &self.wasm_runner,
            &wasm_binary,
            wasm_runner::WasmInput {
                function_name,
                args_json,
            },
            transaction,
            path.component,
            rng_seed,
            unix_timestamp,
            wasm_runner::WasmLimits::default(),
            /* allow_unresolved_commit_ts */ udf_type == UdfType::Mutation,
            log_line_sender,
        )
        .await?;
        let outcome = UdfOutcome {
            path: path.for_logging(),
            arguments,
            identity,
            observed_identity: false,
            rng_seed,
            observed_rng: result.observed_rng,
            unix_timestamp,
            observed_time: result.observed_time,
            log_lines: result.log_lines,
            journal: QueryJournal::new(),
            audit_log_lines: Default::default(),
            result: result.result,
            syscall_trace: SyscallTrace::new(),
            udf_server_version: npm_version,
            memory_in_mb: 0,
            user_execution_time: None,
        };
        Ok((transaction, outcome))
    }

    pub async fn analyze(
        &self,
        udf_config: UdfConfig,
        modules: BTreeMap<CanonicalizedModulePath, ModuleConfig>,
        environment_variables: BTreeMap<EnvVarName, EnvVarValue>,
        deployment_name: String,
    ) -> anyhow::Result<Result<BTreeMap<CanonicalizedModulePath, AnalyzedModule>, JsError>> {
        anyhow::ensure!(
            modules
                .values()
                .all(|m| m.environment == ModuleEnvironment::Isolate),
            "Can only analyze Isolate modules"
        );

        self.require_isolate_client()?
            .analyze(udf_config, modules, environment_variables, deployment_name)
            .await
    }

    #[fastrace::trace]
    pub async fn evaluate_app_definitions(
        &self,
        app_definition: ModuleConfig,
        component_definitions: BTreeMap<ComponentDefinitionPath, ModuleConfig>,
        dependency_graph: BTreeSet<(ComponentDefinitionPath, ComponentDefinitionPath)>,
        user_environment_variables: BTreeMap<EnvVarName, EnvVarValue>,
        system_env_vars: BTreeMap<EnvVarName, EnvVarValue>,
        deployment_name: String,
    ) -> anyhow::Result<EvaluateAppDefinitionsResult> {
        anyhow::ensure!(
            app_definition.environment == ModuleEnvironment::Isolate,
            "Can only evaluate Isolate modules"
        );
        anyhow::ensure!(
            component_definitions
                .values()
                .all(|m| m.environment == ModuleEnvironment::Isolate),
            "Can only evaluate Isolate modules"
        );

        self.require_isolate_client()?
            .evaluate_app_definitions(
                app_definition,
                component_definitions,
                dependency_graph,
                user_environment_variables,
                system_env_vars,
                deployment_name,
            )
            .await
    }

    #[fastrace::trace]
    pub async fn evaluate_component_initializer(
        &self,
        evaluated_definitions: BTreeMap<ComponentDefinitionPath, ComponentDefinitionMetadata>,
        path: ComponentDefinitionPath,
        definition: ModuleConfig,
        args: BTreeMap<Identifier, Resource>,
        name: ComponentName,
        deployment_name: String,
    ) -> anyhow::Result<BTreeMap<Identifier, Resource>> {
        self.require_isolate_client()?
            .evaluate_component_initializer(
                evaluated_definitions,
                path,
                definition,
                args,
                name,
                deployment_name,
            )
            .await
    }

    #[fastrace::trace]
    pub async fn evaluate_schema(
        &self,
        schema_bundle: ModuleSource,
        source_map: Option<SourceMap>,
        rng_seed: [u8; 32],
        unix_timestamp: UnixTimestamp,
        deployment_name: String,
    ) -> anyhow::Result<DatabaseSchema> {
        self.require_isolate_client()?
            .evaluate_schema(
                schema_bundle,
                source_map,
                rng_seed,
                unix_timestamp,
                deployment_name,
            )
            .await
    }

    #[fastrace::trace]
    pub async fn evaluate_auth_config(
        &self,
        auth_config_bundle: ModuleSource,
        source_map: Option<SourceMap>,
        environment_variables: BTreeMap<EnvVarName, EnvVarValue>,
        explanation: &str,
        deployment_name: String,
    ) -> anyhow::Result<AuthConfig> {
        self.require_isolate_client()?
            .evaluate_auth_config(
                auth_config_bundle,
                source_map,
                environment_variables,
                explanation,
                deployment_name,
            )
            .await
    }

    /// Returns the isolate client or a clear error when JS execution is
    /// disabled (wasm-only deployment).
    fn require_isolate_client(&self) -> anyhow::Result<&IsolateClient<RT>> {
        self.isolate_client.as_ref().ok_or_else(|| {
            anyhow::anyhow!(ErrorMetadata::bad_request(
                "JavaScriptExecutionDisabled",
                "This deployment is configured without the JavaScript engine \
                 (ISOLATE_EXECUTION_ENABLED=false). JavaScript functions and module analysis are \
                 unavailable; only WASM functions can run."
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use common::types::ModuleEnvironment;

    use super::*;

    fn is_wasm_only_env(envs: &[ModuleEnvironment]) -> bool {
        !envs.is_empty() && envs.iter().all(is_wasm_environment)
    }

    #[test]
    fn wasm_only_detection() {
        // Mixed: one wasm, one isolate -> not wasm-only
        assert!(!is_wasm_only_env(&[
            ModuleEnvironment::Wasm,
            ModuleEnvironment::Isolate
        ]));
        // Pure wasm: all wasm -> wasm-only
        assert!(is_wasm_only_env(&[ModuleEnvironment::Wasm]));
        assert!(is_wasm_only_env(&[
            ModuleEnvironment::Wasm,
            ModuleEnvironment::Wasm
        ]));
        // Empty -> not wasm-only (conservative)
        assert!(!is_wasm_only_env(&[]));
        // Isolate only -> not wasm-only
        assert!(!is_wasm_only_env(&[ModuleEnvironment::Isolate]));
    }

    #[test]
    fn wasm_only_skips_isolate_creation() {
        use runtime::prod::ProdRuntime;

        #[derive(Clone, Debug)]
        struct DummyStorage;
        #[async_trait::async_trait]
        impl StorageForDeployment<ProdRuntime> for DummyStorage {
            async fn storage_for_deployment(
                &self,
                _transaction: &mut database::Transaction<ProdRuntime>,
                _use_case: storage::StorageUseCase,
            ) -> anyhow::Result<Arc<dyn storage::Storage>> {
                anyhow::bail!("not used in this test")
            }
        }

        let tokio = ProdRuntime::init_tokio().unwrap();
        let rt = ProdRuntime::new(&tokio);
        // Construct a wasm-only core without calling _new(true) which would init V8.
        // This proves the deployment path can be built without touching the JS engine.
        let core: FunctionRunnerCore<ProdRuntime, DummyStorage> = FunctionRunnerCore {
            rt: rt.clone(),
            storage: DummyStorage,
            index_cache: crate::in_memory_indexes::InMemoryIndexCache::new(rt.clone()),
            module_cache: crate::module_cache::ModuleCache::new(rt.clone()),
            code_cache: crate::module_cache::CodeCache::new(),
            isolate_client: None,
            wasm_runner: Arc::new(wasm_runner::WasmRunner::new().unwrap()),
        };
        // Wasm-only deployment: no isolate client, no V8 workers.
        assert!(core.isolate_client.is_none());
        assert_eq!(core.active_isolate_workers(), 0);
        assert_eq!(core.max_isolate_workers(), 0);
        let Err(err) = core.require_isolate_client() else {
            panic!("expected JavaScriptExecutionDisabled error");
        };
        let err_str = err.to_string();
        // ErrorMetadata code is JavaScriptExecutionDisabled; message mentions the knob.
        assert!(
            err_str.contains("ISOLATE_EXECUTION_ENABLED=false"),
            "error should mention knob: {err_str}"
        );
        assert!(
            err_str.contains("without the JavaScript engine"),
            "expected wasm-only error, got: {err_str}"
        );
        // Also verify the ErrorMetadata short_msg when downcastable.
        if let Some(meta) = err.downcast_ref::<errors::ErrorMetadata>() {
            assert_eq!(meta.short_msg, "JavaScriptExecutionDisabled");
        } else {
            // Fallback: check via debug string if not downcastable due to anyhow wrapping.
            assert!(
                format!("{err:?}").contains("JavaScriptExecutionDisabled"),
                "expected code in debug: {err:?}"
            );
        }

        // Verify that _new(false) also produces None without V8 init.
        let dummy_storage = DummyStorage;
        let isolate_worker = isolate::isolate_worker::FunctionRunnerIsolateWorker::new(
            rt.clone(),
            isolate::IsolateConfig::new("test", isolate::ConcurrencyLimiter::unlimited()),
        );
        let core2 =
            FunctionRunnerCore::_new(rt, dummy_storage, 100, isolate_worker, false).unwrap();
        assert!(core2.isolate_client.is_none());
        assert_eq!(core2.active_isolate_workers(), 0);
    }

    #[test]
    fn mixed_deploy_has_isolate() {
        use runtime::prod::ProdRuntime;

        #[derive(Clone, Debug)]
        struct DummyStorage;
        #[async_trait::async_trait]
        impl StorageForDeployment<ProdRuntime> for DummyStorage {
            async fn storage_for_deployment(
                &self,
                _transaction: &mut database::Transaction<ProdRuntime>,
                _use_case: storage::StorageUseCase,
            ) -> anyhow::Result<Arc<dyn storage::Storage>> {
                anyhow::bail!("not used in this test")
            }
        }

        let tokio = ProdRuntime::init_tokio().unwrap();
        let rt = ProdRuntime::new(&tokio);
        let dummy_storage = DummyStorage;
        let isolate_worker = isolate::isolate_worker::FunctionRunnerIsolateWorker::new(
            rt.clone(),
            isolate::IsolateConfig::new("test", isolate::ConcurrencyLimiter::unlimited()),
        );
        let core = FunctionRunnerCore::_new(rt, dummy_storage, 100, isolate_worker, true).unwrap();
        assert!(core.isolate_client.is_some());
        assert_eq!(core.max_isolate_workers(), 300);
        assert!(core.require_isolate_client().is_ok());
        // Explicitly shutdown to avoid leaking V8 workers in test.
        drop(core);
    }
}
