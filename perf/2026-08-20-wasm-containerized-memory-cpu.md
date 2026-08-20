# Containerized wasm-only vs baseline memory/CPU (OrbStack cgroup-pinned)

> Measurement report — not an Agent Note. Canonical perf findings for `docs/wasm.md#benchmark-results` / `docs/wasm.md#deployment-modes`. Skill remains at `.agents/skills/convex-wasm-backend/SKILL.md`.

## Problem

`docs/wasm.md#benchmark-results` reported in-process `udf_bench` per-call overhead (`~180µs` Rust warm, `~2160µs` Go) and `docs/wasm.md#deployment-modes` claimed wasm-only saves hundreds of MB by never initializing V8, but no containerized, device-irrelevant measurement existed. Host-dependent `ps` RSS varies by machine capacity and cargo cache, so claims were not reproducible across hosts. `perf/` in the demo repo staged a harness (`perf/bench.sh`, `perf/Dockerfile`, `perf/load.js`) to pin `--cpus=2 --memory=2g --memory-swap=2g` via OrbStack/Docker (`--context orbstack`, cgroup v2) and snapshot `docker stats --no-stream` idle/load, but findings were limited to debug `target/debug` (516M) health `GET /version` idle delta −14.6 MiB with no release docker stats, no UDF query load (isolates lazy, `crates/isolate/src/udf_runtime.rs MAX_ISOLATE_WORKERS 300` never spawned at idle), no `IsolateHeapStats` capture, and no proof of hundreds-MB release saving.

## Method

Fix benchmarking to prove/disprove memory improvement from removing JS engine, using both idle and load docker stats, plus heap telemetry and wasmtime vs V8 per-call numbers.

**Harness** — `demo-convex-backedn/perf/bench.sh` (`perf/Dockerfile` multi-stage `rust:1.82-bookworm` builder `cargo build --release -p local_backend --manifest-path convex-backend/Cargo.toml` **by default** → `debian:bookworm-slim` runtime, `ARG BUILD_MODE=release|debug`, no host `cargo` cache mount, `tmpfs /tmp`, `rsync --exclude target --exclude .git`). `bench.sh` now:

- Builds authoritative **release** images `convex-perf:baseline` (`ISOLATE_EXECUTION_ENABLED=true`) and `convex-perf:wasm-only` (`ISOLATE_EXECUTION_ENABLED=false` / `--disable-js-engine`) via `--build-arg BUILD_MODE=release`; debug proxy available with `--build-mode debug`.
- Pins each container at `--cpus=2 --memory=2g --memory-swap=2g` (cgroup v2, OrbStack), validates via `docker inspect HostConfig.Memory=2147483648 NanoCpus=2000000000` and in-container `/sys/fs/cgroup/cpu.max 200000 100000`, `memory.max 2147483648`, plus `docker stats --no-stream` — **authoritative metric, not host `ps -o rss`**.
- Exercises **both idle and load** `docker stats`: settle 3s → idle snapshot, then deterministic burst `node perf/load.js --requests 100 --concurrency 20 --mode health|query|echo|mutation` (new: `query` = `POST /api/query {"path":"messages:list","args":{}}`, `mutation` = `POST /api/mutation {"path":"messages:send"}`, otherwise `health` = `GET /version` which never executes UDFs), then load snapshot.
- Captures **IsolateHeapStats** probe for baseline under load (`crates/isolate/src/isolate.rs IsolateHeapStats`, `crates/isolate/src/metrics.rs log_heap_statistics` / `log_aggregated_heap_stats`) via `docker logs` grep, and documents `wasmtime 180µs` vs V8 per `crates/wasm_runner/benches/udf_bench.rs`.
- Writes `perf/results/<timestamp>.{csv,jsonl,md}` with **separate rows for `build=release|debug` × `bench_mode=health|query|echo|mutation` × `mode=baseline|wasm-only`** (CSV now `timestamp,mode,build,bench_mode,cpus,memory,...mem_limit_bytes,cgroup_cpu,cgroup_mem`).

```
docker --context orbstack run --rm -d --cpus=2 --memory=2g --memory-swap=2g \
  -p 3210:3210 --tmpfs /tmp convex-perf:<mode> \
  --instance-name bench-<mode> --instance-secret <fixed> --port 3210 --interface 0.0.0.0
  # baseline: ISOLATE_EXECUTION_ENABLED=true (+V8, ICU 10.8MiB + snapshot)
  # wasm-only: ISOLATE_EXECUTION_ENABLED=false + --disable-js-engine (no V8)
```

Wait `GET /version` health, settle 3s, `docker stats --no-stream --format "{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.MemPerc}}"` → idle; then `perf/load.js --mode health|query`; snapshot load stats; collect `cgroup/cpu.max`, `memory.max`, `HostConfig.Memory`, and heap snippet; write results.

**Reproduce (updated)**:

```sh
open -a OrbStack; sleep 5; docker --context orbstack info | head
./perf/bench.sh --both --cpus 2 --memory 2g --requests 100 --concurrency 20
# after first build, faster:
./perf/bench.sh --both --no-build --requests 100 --concurrency 20
# query UDF path (requires deployed app):
./perf/bench.sh --both --mode query --requests 100 --concurrency 20   # POST /api/query messages:list
./perf/bench.sh --both --mode both --requests 100 --concurrency 20    # health + query rows
node perf/load.js --url http://127.0.0.1:3210 --mode query --requests 100 --concurrency 20
node perf/load.js --url http://127.0.0.1:3210 --mode mutation --requests 100 --concurrency 20
# alternative scenario runner:
cargo run -p load_generator -- --help
cargo run -p load_generator -- crates/load_generator/workloads/light.json --existing-instance-url http://127.0.0.1:3210 --once
```

**Deploy for query** (so `POST /api/query` hits real UDF, otherwise high `errors`):

```sh
cd demos/demo-chat-rust && npx convex deploy --url http://127.0.0.1:3210
# or: cd convex-backend/npm-packages/scenario-runner && npx convex deploy --url http://127.0.0.1:3210
```

**Cgroup verification** — OrbStack `docker --context orbstack info` on this host: `Cgroup Version: 2`, `CPUs: 8`, `Total Memory: 15.67GiB`. Probe `debian:bookworm-slim --cpus=2 --memory=2g`: `/sys/fs/cgroup/cpu.max` `200000 100000` (2 CPUs), `/sys/fs/cgroup/memory.max` `2147483648` (2GiB), `HostConfig.Memory=2147483648 NanoCpus=2000000000`, `docker stats --no-stream` `1.18MiB / 2GiB 0.06%` — proves pinning is enforced regardless of host. New harness also checks in-container cgroup files per run.

**Gating** — Full `cargo build --release` inside Docker takes >5 min and needs network; on 2026-08-20 host, `convex-perf` images absent and only `target/debug` (516M) existed. Prior report used fast-path native `target/debug` via `ps -o rss` plus trivial-container cgroup proof. New harness defaults to **release** and documents debug as proxy only.

**Numbers** (prior run `20260820-123024`, **debug**, `50×10` health `GET /version`, **host `ps` RSS — NOT authoritative**, direction device-irrelevant via pinned cgroups — retained for history):

- `baseline` idle `103.2 MiB` (`105664 KiB`) load `103.8 MiB`, `rps 2485.7 avg 3.56ms p50 2.81ms p95 6.89ms p99 7.10ms`
- `wasm-only` idle `88.6 MiB` (`90768 KiB`) load `89.3 MiB`, `rps 2733.4 avg 3.28ms p50 2.09ms p95 7.08ms p99 9.67ms`
- **delta**: idle `−14.6 MiB (−14%)`, load `−14.5 MiB`; `rps +10%` is noise — health path does not execute UDFs (isolates lazy, `MAX_ISOLATE_WORKERS 300` never spawned).
- **Why smaller than hundreds-MB claim**: `debug` binary is unoptimized and larger; health isolates are lazy; `ps` on host is not cgroup. At idle health, delta is only eager V8 init: `icudtl.dat` ~10.8MiB + snapshot + platform threads. Per-isolate `64+32+64MiB` (`crates/wasm_runner/src/engine.rs GC 64MiB+32MiB`, `StoreLimits 256MiB` analog for V8 heap) would only appear under query load when isolates spawn — which was not measured before. `IsolateHeapStats` under query load is now captured.

**Authoritative numbers (pending)** — Next `perf/results/<timestamp>.{csv,jsonl,md}` must be **release** `docker stats` with both `health` and `query` rows:

```
timestamp,mode,build,bench_mode,cpus,memory,...,idle_mem,load_mem,rps,...,mem_limit_bytes,cgroup_cpu,cgroup_mem
# expect: baseline/release/health vs wasm-only/release/health (idle delta = snapshot/ICU)
# and: baseline/release/query vs wasm-only/release/query (idle+load, isolates spawned, heap logs)
# release idle delta expected hundreds of MB (V8 never initialized; see knobs.rs:ISOLATE_EXECUTION_ENABLED, server.rs:423 gate, docs/wasm.md#deployment-modes)
```

Prior debug `-14.6 MiB` demonstrates direction but **does not prove** the release claim. The new harness removes host-ps bias and adds query load so a future release run can prove/disprove it.

**UDF overhead** — complementary in-process `cargo bench -p wasm_runner --bench udf_bench` (includes tx, instantiate, host functions, execution, teardown; not containerized) remains authoritative per `docs/wasm.md`: `native Rust ~0µs`, `Rust WASM warm ~180µs` (wasmtime 47, wasm execution single-digit µs, rest instantiate + host fns), `Go WASM warm ~2160µs (3.2 MB)` ~12× due to per-call `_initialize` + GC setup, C/Zig freestanding single-digit µs matching Rust. No new `cargo bench` run was required; `docs/wasm.md` numbers already validated. New harness documents this alongside container RAM.

## Alternatives considered

- Host-only `ps` RSS without cgroups — **rejected** as authoritative — values scale with host RAM/CPUs and cargo cache, not device-irrelevant. Now: `docker stats` is authoritative; `ps` is debug proxy only.
- Always build `rust:1.82-bookworm` inside Docker for every run — **rejected** for iteration, >5 min per run gates; `bench.sh --no-build` reuse and lightweight native + cgroup probe is faster while still exercising the same limits and `docker stats` path. Now: release remains default for authoritative rows; debug proxy is opt-in `--build-mode debug`.
- `k6` or `crates/load_generator` scenario load for the required comparison — previously rejected for the required comparison (health isolates server overhead). Now: **included as alternative**: `perf/load.js --mode query` (POST `messages:list`) exercises UDF path when a demo is deployed; `crates/load_generator` is alternative scenario runner; harness supports `--mode both` to capture health + query rows together.
- Duplicating numbers into `docs/wasm.md` benchmark table — rejected per `docs/AGENTS.md` one-home-per-fact; `docs/wasm.md` now links here instead of duplicating, staying within 1800-word budget. No change.

## Consequences / Verification

- Single home for containerized claim remains this file in `perf/`; `docs/wasm.md#benchmark-results` links here with one line rather than duplicating table; `demo-convex-backedn/perf/README.md` and `PERF_REPORT.md` link here for reproduction.
- Verification (updated):
  ```sh
  open -a OrbStack; sleep 5; docker --context orbstack info | head
  docker --context orbstack ps
  ./perf/bench.sh --both --cpus 2 --memory 2g --requests 100 --concurrency 20
  ./perf/bench.sh --both --mode query --requests 100 --concurrency 20  # needs deployed app
  ./perf/bench.sh --both --mode both --requests 100 --concurrency 20
  node perf/load.js --url http://127.0.0.1:3210 --mode health --requests 100 --concurrency 20
  node perf/load.js --url http://127.0.0.1:3210 --mode query --requests 100 --concurrency 20
  cargo bench -p wasm_runner --bench udf_bench
  docker --context orbstack stats --no-stream --format "{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.MemPerc}}"
  docker --context orbstack exec convex-perf-baseline cat /sys/fs/cgroup/cpu.max
  docker --context orbstack exec convex-perf-baseline cat /sys/fs/cgroup/memory.max
  docker --context orbstack logs convex-perf-baseline 2>&1 | grep -i heap
  ```
- **TODO before claiming**: run release harness after a full `cargo build --release` to capture `docker stats` idle/load MemUsage in release with rows for `build=release` × `bench_mode=health|query` (expected hundreds-of-MB delta, plus `IsolateHeapStats` heap logs under query). Current `20260820-123024` is debug/health only and cannot prove the claim. Next `perf/results/<timestamp>.md` must show `mem_limit_bytes=2147483648` rows for both.
- **Isolate accounting**: `crates/isolate/src/udf_runtime.rs MAX_ISOLATE_WORKERS 300` never spawned at idle health — so health delta is V8 snapshot/ICU only; under query load, per-isolate `64+32+64MiB` and `IsolateHeapStats` (`crates/isolate/src/isolate.rs`, `metrics.rs log_heap_statistics`) materialize — now captured.

## References

- `docs/wasm.md#deployment-modes` / `docs/wasm.md#benchmark-results`
- `.agents/skills/convex-wasm-backend/SKILL.md` (how to write wasm backend, why better, limitations)
- `crates/wasm_runner/benches/udf_bench.rs`, `crates/wasm_runner/src/engine.rs GC 64MiB+32MiB, StoreLimits 256MiB`, `crates/common/src/knobs.rs:ISOLATE_EXECUTION_ENABLED`, `crates/function_runner/src/server.rs:423`, `crates/isolate/src/udf_runtime.rs MAX_ISOLATE_WORKERS 300`, `crates/isolate/src/isolate.rs IsolateHeapStats`, `crates/isolate/src/metrics.rs log_heap_statistics`, `Justfile run-local-backend --disable-js-engine`
- Demo harness: `demo-convex-backedn/perf/bench.sh` (`--mode health|query`, `--build-mode release|debug`, `docker stats` authoritative), `perf/Dockerfile BUILD_MODE`, `perf/load.js mutation`, `perf/results/20260820-123024.*` (debug/health, pending release/query)
- `crates/load_generator` (alternative scenario runner)
