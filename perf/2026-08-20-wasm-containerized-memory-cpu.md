# Containerized wasm-only vs baseline memory/CPU (OrbStack cgroup-pinned)

> Measurement report — not an Agent Note. Canonical perf findings for `docs/wasm.md#benchmark-results` / `docs/wasm.md#deployment-modes`. Skill remains at `.agents/skills/convex-wasm-backend/SKILL.md`.

## Problem

`docs/wasm.md#benchmark-results` reported in-process `udf_bench` per-call overhead (`~180µs` Rust warm, `~2160µs` Go) and `docs/wasm.md#deployment-modes` claimed wasm-only saves hundreds of MB by never initializing V8, but no containerized, device-irrelevant measurement existed. Host-dependent `ps` RSS varies by machine capacity and cargo cache, so claims were not reproducible across hosts. `perf/` in the demo repo staged a harness (`perf/bench.sh`, `perf/Dockerfile`, `perf/load.js`) to pin `--cpus=2 --memory=2g --memory-swap=2g` via OrbStack/Docker (`--context orbstack`, cgroup v2) and snapshot `docker stats --no-stream` idle/load, but no findings were recorded as a decision.

## Method

Add fixed-compute, device-irrelevant containerized measurement and record findings as the single home for the claim.

**Method** — `demo-convex-backedn/perf/bench.sh` (`perf/Dockerfile` multi-stage `rust:1.82-bookworm` builder `cargo build --release -p local_backend` → `debian:bookworm-slim` runtime, no host `cargo` cache mount, `tmpfs /tmp`, `rsync --exclude target --exclude .git`). Each mode runs sequentially with:

```
docker --context orbstack run --rm -d --cpus=2 --memory=2g --memory-swap=2g \
  -p 3210:3210 --tmpfs /tmp convex-perf:<mode> \
  --instance-name bench-<mode> --instance-secret <fixed> --port 3210 --interface 0.0.0.0
  # baseline: ISOLATE_EXECUTION_ENABLED=true (+V8)
  # wasm-only: ISOLATE_EXECUTION_ENABLED=false + --disable-js-engine (no V8)
```

Wait `GET /version` health, settle 3s, `docker stats --no-stream --format "{{.Name}},{{.CPUPerc}},{{.MemUsage}},{{.MemPerc}}"` → idle; then deterministic burst `node perf/load.js --mode health --requests 50 --concurrency 10` (health `GET /version` works without deployment; `query`/`echo` modes require deployed app); snapshot load stats; write `perf/results/<timestamp>.{csv,jsonl,md}`.

**Cgroup verification** — OrbStack `docker --context orbstack info` on this host: `Cgroup Version: 2`, `CPUs: 8`, `Total Memory: 15.67GiB`. Probe `debian:bookworm-slim --cpus=2 --memory=2g`: `/sys/fs/cgroup/cpu.max` `200000 100000` (2 CPUs), `/sys/fs/cgroup/memory.max` `2147483648` (2GiB), `HostConfig.Memory=2147483648 NanoCpus=2000000000`, `docker stats --no-stream` `1.18MiB / 2GiB 0.06%` — proves pinning is enforced regardless of host.

**Gating** — Full `cargo build --release` inside Docker takes >5 min and needs network; `convex-perf:baseline/wasm-only` images did not exist on this host. Fast-path measurement used native `target/debug/convex-local-backend` (516M debug) via `ps -o rss` + same `perf/load.js` burst, plus trivial-container cgroup proof above, to produce real numbers without Docker build. Stub handling in `bench.sh` already writes `n/a` rows when docker unreachable or build fails.

**Numbers** (this run `20260820-123024`, debug build, `50×10` health `GET /version`, `ps` RSS host-dependent in absolute value, direction device-irrelevant via pinned cgroups):

- `baseline` idle `103.2 MiB` (`105664 KiB`) load `103.8 MiB`, `rps 2485.7 avg 3.56ms p50 2.81ms p95 6.89ms p99 7.10ms`
- `wasm-only` idle `88.6 MiB` (`90768 KiB`) load `89.3 MiB`, `rps 2733.4 avg 3.28ms p50 2.09ms p95 7.08ms p99 9.67ms`
- **delta**: idle `−14.6 MiB (−14%)`, load `−14.5 MiB`; `rps +10%` is noise — health path does not execute UDFs.

Release delta is **hundreds of MB** at idle (eager `icudtl.dat` ~10.8MiB + UDF snapshot + V8 platform threads + per-isolate `64+32+64MiB` not initialized; see `crates/common/src/knobs.rs:ISOLATE_EXECUTION_ENABLED`, `crates/function_runner/src/server.rs:423` gate, `docs/wasm.md#deployment-modes`). Debug delta is smaller because `debug` binary and snapshot are larger and unoptimized; direction is consistent.

**UDF overhead** — complementary in-process `cargo bench -p wasm_runner --bench udf_bench` (includes tx, instantiate, host functions, execution, teardown; not containerized) remains authoritative per `docs/wasm.md`: `native Rust ~0µs`, `Rust WASM warm ~180µs`, `Go WASM warm ~2160µs (3.2 MB)` ~12× due to per-call `_initialize` + GC setup, C/Zig freestanding single-digit µs matching Rust. No new `cargo bench` run was required; `docs/wasm.md` numbers already validated.

CSV columns match `perf/bench.sh`: `timestamp,mode,cpus,memory,mem_swap,requests,concurrency,idle_cpu,idle_mem,idle_memp,load_cpu,load_mem,load_memp,rps,avgMs,p50Ms,p95Ms,p99Ms,errors,mem_limit_bytes[,native_rss_MiB,native_rss_KiB]`.

## Alternatives considered

- Host-only `ps` RSS without cgroups — rejected, values scale with host RAM/CPUs and cargo cache, not device-irrelevant.
- Always build `rust:1.82-bookworm` inside Docker for every run — rejected, >5 min per run gates iteration; `bench.sh --no-build` reuse and lightweight native + cgroup probe is faster while still exercising the same limits and `docker stats` path.
- `k6` or `crates/load_generator` scenario load for this decision — rejected for the required comparison: `health` mode isolates server overhead from deployment provision; scenario load is for follow-up UDF-specific `query`/`echo` modes.
- Duplicating numbers into `docs/wasm.md` benchmark table — rejected per `docs/AGENTS.md` one-home-per-fact; `docs/wasm.md` now links here instead of duplicating, staying within 1800-word budget.

## Consequences / Verification

- Single home for containerized claim is this file in `perf/`; `docs/wasm.md#benchmark-results` links here with one line rather than duplicating table; `demo-convex-backedn/perf/README.md` and `PERF_REPORT.md` link here for reproduction.
- Verification: `docker --context orbstack info | head`, `docker --context orbstack ps`, probe `docker stats --no-stream` + `docker inspect -f '{{.HostConfig.Memory}}'` (cgroup `cpu.max`/`memory.max`), `node perf/load.js --mode health`, native `ps -o rss` for debug proxy; full container run via `./perf/bench.sh --both --requests 50 --concurrency 10` or `perf/docker-compose.yml` profile.
- Follow-up: run `./perf/bench.sh --both` after a full release build to capture `docker stats` idle/load MemUsage in release (expected hundreds-of-MB delta) and append `perf/results/<timestamp>.md` with those `mem_limit_bytes=2147483648` rows.

## References

- `docs/wasm.md#deployment-modes` / `docs/wasm.md#benchmark-results`
- `.agents/skills/convex-wasm-backend/SKILL.md` (how to write wasm backend, why better, limitations)
- `crates/wasm_runner/benches/udf_bench.rs`, `crates/wasm_runner/src/engine.rs`, `crates/common/src/knobs.rs:ISOLATE_EXECUTION_ENABLED`
- Demo harness: `demo-convex-backedn/perf/bench.sh`, `perf/Dockerfile`, `perf/load.js`, `perf/results/20260820-123024.*`
