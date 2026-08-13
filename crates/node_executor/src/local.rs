use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use common::log_lines::LogLine;
use errors::ErrorMetadata;
use futures::{
    select_biased,
    FutureExt,
};
use futures_async_stream::try_stream;
use isolate::bundled_js::node_executor_file;
use rand::Rng;
use reqwest::Client;
use serde_json::Value as JsonValue;
use tempfile::TempDir;
use tokio::{
    process::{
        Child,
        Command as TokioCommand,
    },
    sync::{
        mpsc,
        Mutex,
    },
};

use crate::{
    executor::{
        ExecutorRequest,
        InvokeResponse,
        NodeExecutor,
        ARGS_TOO_LARGE_RESPONSE_MESSAGE,
        EXECUTE_TIMEOUT_RESPONSE_JSON,
    },
    handle_node_executor_stream,
    NodeExecutorStreamPart,
};

const NVMRC_VERSION: &str = include_str!("../../../.nvmrc");
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_millis(100);
const MAX_HEALTH_CHECK_ATTEMPTS: u32 = 50;

/// Environment variables passed through to the spawned Node.js executor
/// process. The executor runs untrusted user code with unrestricted access
/// to `fs`, `child_process`, and `process`, so anything else from the
/// backend's environment (notably `CONVEX_*` instance secrets) would be
/// readable by that code. This mirrors the allowlist the executor exposes
/// to user actions, plus `HOME` (npm uses it for its cache and config) and
/// minus `PWD` (the child's working directory is controlled instead).
const NODE_EXECUTOR_ENV_ALLOWLIST: &[&str] = &["HOME", "LANG", "NODE_PATH", "PATH", "TZ", "UTC"];

pub struct LocalNodeExecutor {
    inner: Arc<Mutex<Option<InnerLocalNodeExecutor>>>,
    config: LocalNodeExecutorConfig,
}

struct LocalNodeExecutorConfig {
    node_process_timeout: Duration,
    /// Overrides the initial callback retry backoff in the spawned node
    /// process (read by syscalls.ts at module load). Tests zero this so
    /// callbacks retrying against an unreachable backend settle within test
    /// timeouts.
    callback_initial_backoff: Option<Duration>,
}

struct InnerLocalNodeExecutor {
    _source_dir: TempDir,
    client: reqwest::Client,
    _server_handle: Child,
}

impl InnerLocalNodeExecutor {
    async fn new(config: &LocalNodeExecutorConfig) -> anyhow::Result<Self> {
        tracing::info!("Initializing inner local node executor");
        // Create a single temp directory for both source files and Node.js temp files
        let source_dir = TempDir::new()?;
        let (source, source_map) =
            node_executor_file("local.cjs").expect("local.cjs not generated!");
        let source_map = source_map.context("Missing local.cjs.map")?;
        let source_path = source_dir.path().join("local.cjs");
        let source_map_path = source_dir.path().join("local.cjs.map");
        fs::write(&source_path, source.as_bytes())?;
        fs::write(source_map_path, source_map.as_bytes())?;
        tracing::info!(
            "Using local node executor. Source: {}",
            source_path.to_str().expect("Path is not UTF-8 string?"),
        );

        let socket_path = if cfg!(unix) {
            source_dir.path().join(".executor.sock")
        } else if cfg!(windows) {
            PathBuf::from(format!(
                r"\\.\pipe\cvx-node-executor-{:016x}",
                rand::rng().random::<u64>()
            ))
        } else {
            panic!("not supported");
        };
        let server_handle =
            Self::start_node_with_listener(config, &source_path, &source_dir, &socket_path).await?;
        // Don't keep idle connections in the pool. The Node HTTP server closes
        // idle keep-alive connections after its (default 5s) `keepAliveTimeout`,
        // but hyper's pool would hold one much longer and reuse it right as the
        // server closes it, surfacing as a spurious "connection reset by peer".
        // Opening a fresh connection per request is cheap over a local socket.
        let mut client_builder = Client::builder().pool_max_idle_per_host(0);
        #[cfg(unix)]
        {
            client_builder = client_builder.unix_socket(socket_path);
        }
        #[cfg(windows)]
        {
            client_builder = client_builder.windows_named_pipe(socket_path);
        }
        let client = client_builder.build()?;

        // Wait for the Node process to be ready to handle HTTP requests.
        for _ in 0..MAX_HEALTH_CHECK_ATTEMPTS {
            if Self::check_server_health(&client).await? {
                return Ok(Self {
                    _source_dir: source_dir,
                    client,
                    _server_handle: server_handle,
                });
            }
            tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
        }
        anyhow::bail!("Node executor server failed to start and become healthy")
    }

    async fn check_node_version(node_path: &str) -> anyhow::Result<()> {
        let cmd = TokioCommand::new(node_path)
            .arg("--version")
            .output()
            .await?;
        let version = String::from_utf8_lossy(&cmd.stdout);

        if !version.starts_with("v20.")
            && !version.starts_with("v22.")
            && !version.starts_with("v24.")
        {
            anyhow::bail!(ErrorMetadata::bad_request(
                "DeploymentNotConfiguredForNodeActions",
                "Deployment is not configured to deploy \"use node\" actions. \
                 Node.js v20, 22, or 24 is not installed. \
                 Install a supported Node.js version with nvm (https://github.com/nvm-sh/nvm) \
                 to deploy Node.js actions."
            ))
        }
        Ok(())
    }

    async fn check_server_health(client: &Client) -> anyhow::Result<bool> {
        match client
            .get("http://localhost/health".to_string())
            .timeout(Duration::from_secs(1))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => Ok(true),
            _ => Ok(false),
        }
    }

    /// Spawns the Node.js executor server that runs untrusted user code.
    /// The child inherits none of the backend's environment or working
    /// directory: user code has unrestricted access to `fs`,
    /// `child_process`, and `process`, so secrets in the backend's
    /// environment would otherwise be readable by deployed actions. Only an
    /// allowlisted subset of the environment is passed through, the working
    /// directory is set to the executor's own temp dir, and the child is
    /// hardened with `no_new_privs` and resource limits (see
    /// `harden_child_process`).
    async fn start_node_with_listener(
        config: &LocalNodeExecutorConfig,
        source_path: &PathBuf,
        temp_dir: &TempDir,
        socket_path: &PathBuf,
    ) -> anyhow::Result<Child> {
        let preferred_node_version = NVMRC_VERSION.trim();

        // Look for node in a few places.
        let possible_path = home::home_dir()
            .unwrap()
            .join(".nvm")
            .join(format!("versions/node/v{preferred_node_version}/bin/node"));
        let node_path = if possible_path.exists() {
            possible_path.to_string_lossy().to_string()
        } else {
            "node".to_string()
        };
        Self::check_node_version(&node_path).await?;

        let mut cmd = TokioCommand::new(node_path);
        cmd.env_clear();
        for name in NODE_EXECUTOR_ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        // Point the child at the executor's own temp dir for both its
        // working directory and temp files instead of inheriting the
        // backend's.
        cmd.env("TMPDIR", temp_dir.path())
            .current_dir(temp_dir.path());
        cmd.arg(source_path)
            .arg("--ipc-path")
            .arg(socket_path)
            .arg("--tempdir")
            .arg(temp_dir.path())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // Safety: the closure runs in the child between `fork` and `exec`
            // and only performs async-signal-safe `libc` calls (see
            // `harden_child_process`).
            unsafe {
                cmd.pre_exec(harden_child_process);
            }
        }
        if let Some(backoff) = config.callback_initial_backoff {
            cmd.env(
                "CALLBACK_INITIAL_BACKOFF_MS",
                backoff.as_millis().to_string(),
            );
        }

        let child = cmd.spawn()?;

        Ok(child)
    }
}

/// Applies Unix hardening to the spawned Node.js executor process, which
/// runs untrusted user code: it can't gain new privileges through setuid
/// binaries (`PR_SET_NO_NEW_PRIVS`) and its resource limits are capped so a
/// runaway or compromised executor can't exhaust the host's file
/// descriptors, memory, or CPU. Each limit is set to the tighter of its cap
/// and the child's inherited hard limit, with soft == hard so the child
/// can't raise it. `RLIMIT_DATA` (Linux only: macOS rejects `setrlimit` on
/// it) bounds memory rather than `RLIMIT_AS`, which also counts virtual
/// reservations such as V8's sandbox and would fail to start under it.
///
/// # Safety
/// This runs in the child between `fork` and `exec`, where only
/// async-signal-safe operations are allowed: it only performs `libc`
/// syscalls with no allocation or locking, and every pointer passed to them
/// points to a local value initialized on the stack.
#[cfg(unix)]
fn harden_child_process() -> std::io::Result<()> {
    unsafe {
        // Safety: `prctl` is async-signal-safe and takes no pointers here.
        #[cfg(target_os = "linux")]
        {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        // Safety: `getrlimit` and `setrlimit` are async-signal-safe and only
        // operate on a local `rlimit` value.
        let cpu_cap = (24 * 60 * 60) as libc::rlim_t;
        #[cfg(target_os = "linux")]
        let memory_cap = (8 * 1024 * 1024 * 1024u64) as libc::rlim_t;
        let nofile_cap = 4096u64 as libc::rlim_t;
        for (resource, cap) in [
            (libc::RLIMIT_CPU, cpu_cap),
            // `RLIMIT_DATA` is Linux-only: macOS rejects `setrlimit` on
            // `RLIMIT_DATA`/`RLIMIT_AS` outright and doesn't enforce them.
            #[cfg(target_os = "linux")]
            (libc::RLIMIT_DATA, memory_cap),
            (libc::RLIMIT_NOFILE, nofile_cap),
        ] {
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if libc::getrlimit(resource, limit.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut limit = limit.assume_init();
            let hard = limit.rlim_max.min(cap);
            if hard == 0 {
                continue;
            }
            limit.rlim_cur = hard;
            limit.rlim_max = hard;
            if libc::setrlimit(resource, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

impl LocalNodeExecutor {
    pub async fn new(node_process_timeout: Duration) -> anyhow::Result<Self> {
        let executor = Self {
            inner: Arc::new(Mutex::new(None)),
            config: LocalNodeExecutorConfig {
                node_process_timeout,
                callback_initial_backoff: None,
            },
        };

        Ok(executor)
    }

    #[try_stream(ok = NodeExecutorStreamPart, error = anyhow::Error)]
    async fn response_stream(config: &LocalNodeExecutorConfig, mut response: reqwest::Response) {
        let mut timeout_future = Box::pin(tokio::time::sleep(config.node_process_timeout));
        let timeout_future = &mut timeout_future;
        loop {
            let process_chunk = async {
                select_biased! {
                    chunk = response.chunk().fuse() => {
                        let chunk = chunk?;
                        match chunk {
                            Some(chunk) => {
                                anyhow::Ok(NodeExecutorStreamPart::Chunk(chunk))
                            }
                            None => {
                                anyhow::Ok(NodeExecutorStreamPart::InvokeComplete(Ok(())))
                            }
                        }
                    },
                    _ = timeout_future.fuse() => {
                        anyhow::Ok(NodeExecutorStreamPart::InvokeComplete(Err(InvokeResponse {
                            response: EXECUTE_TIMEOUT_RESPONSE_JSON.clone(),
                            aws_request_id: None,
                        })))
                    },
                }
            };
            let part = process_chunk.await?;
            if let NodeExecutorStreamPart::InvokeComplete(_) = part {
                yield part;
                break;
            } else {
                yield part;
            }
        }
    }
}

#[async_trait]
impl NodeExecutor for LocalNodeExecutor {
    fn enable(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn invoke(
        &self,
        request: ExecutorRequest,
        log_line_sender: mpsc::UnboundedSender<LogLine>,
    ) -> anyhow::Result<InvokeResponse> {
        let client = {
            let mut inner = self.inner.lock().await;
            if inner.is_none() {
                *inner = Some(
                    InnerLocalNodeExecutor::new(&self.config)
                        .await
                        .context("Failed to create inner local node executor")?,
                )
            }
            let inner = inner.as_ref().unwrap();
            inner.client.clone()
        };
        let request_json = JsonValue::try_from(request)?;

        let response_result = client
            .post("http://localhost/invoke".to_string())
            .json(&request_json)
            .timeout(self.config.node_process_timeout)
            .send()
            .await;
        let response = match response_result {
            Ok(response) => response,
            Err(e) => {
                if e.is_timeout() {
                    return Ok(InvokeResponse {
                        response: EXECUTE_TIMEOUT_RESPONSE_JSON.clone(),
                        aws_request_id: None,
                    });
                } else if e.is_connect() {
                    // Connection error likely means the Node server crashed (e.g., OOM).
                    // Drop the dead server so it will be restarted on next invoke.
                    tracing::warn!("Node server connection failed, dropping server: {e}");
                    self.inner.lock().await.take();
                    return Err(anyhow::anyhow!(e).context("Node server request failed"));
                } else {
                    return Err(anyhow::anyhow!(e).context("Node server request failed"));
                }
            },
        };

        if let Err(e) = response.error_for_status_ref() {
            if e.status() == Some(reqwest::StatusCode::PAYLOAD_TOO_LARGE) {
                return Err(
                    anyhow::anyhow!(e.without_url()).context(ErrorMetadata::bad_request(
                        "ArgsTooLarge",
                        ARGS_TOO_LARGE_RESPONSE_MESSAGE,
                    )),
                );
            }
            let error = response.text().await?;
            anyhow::bail!("Node executor server returned error: {}", error);
        }
        let stream = Self::response_stream(&self.config, response);
        let stream = Box::pin(stream);
        let result = handle_node_executor_stream(log_line_sender, stream).await?;
        match result {
            Ok(payload) => {
                if payload
                    .get("exitingProcess")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    // Drop the server if it claims to be exiting.
                    self.inner.lock().await.take();
                }
                Ok(InvokeResponse {
                    response: payload,
                    aws_request_id: None,
                })
            },
            Err(e) => Ok(e),
        }
    }

    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn executor_child_gets_scrubbed_env_and_controlled_cwd() -> anyhow::Result<()> {
        if TokioCommand::new("node")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("node not available; skipping");
            return Ok(());
        }
        // Safety: the test process env is not read concurrently.
        unsafe {
            std::env::set_var("CONVEX_MARKER_SECRET", "super-secret-value");
            std::env::set_var("MARKER_KEY", "another-secret");
        }
        let config = LocalNodeExecutorConfig {
            node_process_timeout: Duration::from_secs(10),
            callback_initial_backoff: None,
        };
        let inner = InnerLocalNodeExecutor::new(&config).await?;
        let pid = inner._server_handle.id().expect("child has no pid on unix");
        let env = child_environment(pid);
        let env = env.as_deref().unwrap_or_default();
        assert!(
            !env.iter().any(|e| e.starts_with("CONVEX_MARKER_SECRET=")),
            "backend env leaked to node child: {env:?}"
        );
        assert!(
            !env.iter().any(|e| e.starts_with("MARKER_KEY=")),
            "backend env leaked to node child: {env:?}"
        );
        assert!(env.iter().any(|e| e.starts_with("PATH=")));
        assert!(env.iter().any(|e| e.starts_with("TMPDIR=")));
        assert!(env.iter().any(|e| e.starts_with("HOME=")));
        assert!(!env
            .iter()
            .any(|e| e.starts_with("CALLBACK_INITIAL_BACKOFF_MS=")));
        let cwd = child_cwd(pid).ok().flatten();
        assert_eq!(
            cwd.as_deref()
                .map(std::path::Path::canonicalize)
                .transpose()?
                .as_deref(),
            Some(inner._source_dir.path().canonicalize()?.as_path()),
            "node child should run with the executor temp dir as its cwd"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn child_environment(pid: u32) -> anyhow::Result<Vec<String>> {
        let environ = std::fs::read(format!("/proc/{pid}/environ"))?;
        Ok(environ
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect())
    }

    #[cfg(target_os = "linux")]
    fn child_cwd(pid: u32) -> anyhow::Result<Option<std::path::PathBuf>> {
        Ok(Some(std::fs::read_link(format!("/proc/{pid}/cwd"))?))
    }

    #[cfg(target_os = "macos")]
    fn child_environment(pid: u32) -> anyhow::Result<Vec<String>> {
        let out = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-wwE", "-o", "command="])
            .output()?;
        let out = String::from_utf8_lossy(&out.stdout);
        Ok(out
            .split(' ')
            .map(|e| e.trim().to_owned())
            .filter(|e| e.contains('='))
            .collect())
    }

    #[cfg(target_os = "macos")]
    fn child_cwd(pid: u32) -> anyhow::Result<Option<std::path::PathBuf>> {
        let out = std::process::Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()?;
        let out = String::from_utf8_lossy(&out.stdout);
        Ok(out
            .lines()
            .find_map(|l| l.strip_prefix('n'))
            .map(Into::into))
    }
}
