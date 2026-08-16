# Advanced Configuration and Tuning

There is a large number of detailed configuration options in
[knobs.rs](/crates/common/src/knobs.rs). These options are configurable via
environment variables. In order to tune your Convex instance at scale for your
workload, you may need to adjust these knobs. You will have to set these
environment variables by adding them to your `docker-compose.yml` file. Commonly
overriden knobs are listed in the `env` section of the
[`docker-compose.yml`](../docker/docker-compose.yml)

## `APPLICATION_MAX_CONCURRENT_*` knobs

You can increase the max concurrency on your self-hosted instance with these
environment variables. Note that increasing concurrency will increase load on
your system and after a certain threshold, performance will degrade. You will
have to tune parameters based on your own hardware and workload.

## `HTTP_SERVER_MAX_CONCURRENT_REQUESTS`

Bounding the number of concurrent HTTP requests accepted by the backend's HTTP
server. Lower this to protect memory under load spikes; raise it for
throughput-bound workloads. Passed through in the
[`docker-compose.yml`](../docker/docker-compose.yml) `env` section.

## Security knobs

The following environment variables control the security posture of a
self-hosted deployment. The defaults are the secure posture; set these only when
you understand the exposure.

| Environment variable                   | Default | Effect                                                                                                                        |
| -------------------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `CONVEX_ALLOW_PRIVATE_FETCH_IPS`       | unset   | When set, UDF `fetch` requests to private/loopback/link-local/metadata IP ranges are allowed (the SSRF denylist is disabled). |
| `CONVEX_ALLOW_INSECURE_DEV_SECRET`     | unset   | When set, the backend accepts the well-known dev admin secret on non-loopback binds.                                          |
| `CONVEX_ALLOW_UNAUTHENTICATED_METRICS` | unset   | When set, `/metrics` is served without an admin key.                                                                          |
| `MYSQL_TLS_REQUIRED`                   | `true`  | Set to `false` to allow cleartext MySQL connections.                                                                          |

Related flags on the local backend: `--disable-js-engine` (run wasm functions
only; see [wasm deployment modes](../../docs/wasm.md#deployment-modes)),
`--expires-in` (admin keys expire after the given duration), and
`--allowed-origins` (restrict CORS and WebSocket `Origin`).
