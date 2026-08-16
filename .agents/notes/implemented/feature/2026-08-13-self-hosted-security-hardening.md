# Agent Note: Self-hosted security hardening

Status: implemented

## Problem

Self-hosted deployments exposed several forgeable or overly permissive surfaces: client-driven upload tokens were plaintext JSON carrying absolute storage paths, so any token holder could mint a token for any path; UDF `fetch` connected to private/loopback/link-local ranges, enabling SSRF against the host network; the local backend bound to all interfaces, accepted the well-known dev secret from any origin, served `/metrics` without authentication, and echoed raw paths into logs; MySQL connections defaulted to cleartext; and the wasm and node executors lacked resource caps and environment scrubbing.

## Decision

Harden every surface found in the audit, defaulting to the secure posture with explicit opt-out env vars:

- **Storage tokens**: client-driven upload and part tokens are AEAD-encrypted capabilities via `keybroker` `RandomEncryptor` instead of forgeable plaintext JSON. `LocalDirStorage` canonicalizes and strip-prefixes every client-derived path; `objectKey` rejects `..`, `.`, and empty components; per-part upload caps apply.
- **UDF fetch SSRF guard**: `crates/common/src/http/fetch.rs` installs an IPv4 + IPv6 denylist (private, loopback, link-local, CGNAT, metadata, multicast) with a DNS-rebind-safe resolve-validate-connect sequence through a custom reqwest resolver. Opt-out: `CONVEX_ALLOW_PRIVATE_FETCH_IPS`. When `--convex-http-proxy` is configured, screening delegates to the proxy.
- **Local backend defaults**: bind `127.0.0.1` by default; refuse the well-known dev secret on non-loopback binds unless `CONVEX_ALLOW_INSECURE_DEV_SECRET` is set; `/metrics` requires an admin key unless `CONVEX_ALLOW_UNAUTHENTICATED_METRICS` is set; redact-logs-to-client defaults on; the `adminKey` query parameter is removed (header only); admin keys support `--expires-in` with expiry enforced; CORS and WebSocket `Origin` are restricted to `--allowed-origins`; decompressed push bodies are capped.
- **Wasm sandbox caps**: `wasm_runner` caps the WasmGC heap reservation to the linear-memory budget (wasmtime `StoreLimits` does not bound GC allocations), applies a wall-clock timeout to compile + instantiate behind a bounded compile semaphore on `spawn_blocking`, bound-checks guest reads before allocating (a hostile length cannot force a 2 GiB host allocation), and enumerates the exact WASI p1 / `env` host function surface at validation time.
- **node_executor**: spawned processes get an env allowlist, a controlled CWD, and `no_new_privs` + rlimits on Linux.
- **MySQL**: TLS required by default; `MYSQL_TLS_REQUIRED=false` opts out.
- **CLI**: `convex backend` zip downloads are verified against published sha256 checksums; the precompile workflow publishes checksum assets.
- **Isolate/mysql**: V8-callback unwraps that could abort the process are removed.

## Alternatives considered

- **Allowlist instead of denylist for SSRF**: a denylist matches how self-hosters actually deploy (private databases, internal services behind the backend) while still blocking the dangerous ranges; an allowlist would break legitimate private-network integrations by default. The proxy configuration is the allowlist path when one is wanted.
- **Signed plaintext tokens instead of AEAD**: signing preserves the token contents, which still leak the storage layout; encryption hides the capability structure entirely and matches the existing keybroker infrastructure.
- **MySQL TLS opt-in**: TLS-on-by-default breaks existing cleartext deployments at upgrade; the explicit `MYSQL_TLS_REQUIRED=false` opt-out makes the break visible and deliberate rather than silent.

## Consequences

- The default secure posture changes behavior for existing deployments: non-loopback binds, unauthenticated `/metrics`, the `adminKey` query parameter, and cleartext MySQL all require an explicit opt-out or flag after this change.
- Storage token format changed incompatibly; clients re-upload to obtain tokens.
- UDFs calling private-network `fetch` fail with a clear SSRF-guard error unless `CONVEX_ALLOW_PRIVATE_FETCH_IPS` is set or a proxy is configured.
- Env-var reference lives in `self-hosted/advanced/knobs.md`; wasm sandbox details live in `docs/wasm.md`.
