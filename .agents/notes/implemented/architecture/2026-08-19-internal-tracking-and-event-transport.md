# Agent Note: Internal tracking representation and WS/SSE event transport

Status: implemented

## Problem

Live sync must know what changed to invalidate queries efficiently, and must push results to browsers over a transport that scales to many languages. Today the backend records per-query `ReadSet` intervals, invalidates via `SubscriptionManager::advance_log`, and pushes full `QueryUpdated` values over a single WebSocket multiplexing queries, mutations, and auth. A naïve rewrite risks language-specific tracking, excess bandwidth (full values instead of deltas), and duplicated transports.

## Decision

Keep the language-agnostic `Token`/`ReadSet` → `SubscriptionManager` → `SyncWorker` pipeline as the one home for change tracking, and keep WebSocket as the primary sync transport with SSE as an optional fallback only: tracking is `Transaction.reads: ReadSet { indexed: BTreeMap<TabletIndexName,IntervalSet>, search }` in `crates/database/src/transaction.rs:162` merged into `Token` and inserted into `SubscriptionManager { indexed: BTreeMap<TabletIndexName,(IndexedFields,IntervalMap)> }` in `crates/database/src/subscription.rs:587`; invalidation walks `WriteLog.for_each_index` and queries `IntervalMap` in `crates/database/src/subscription.rs:725`; `SyncWorker` re-executes invalidated queries and emits `ServerMessage::Transition { modifications: Vec<StateModification::QueryUpdated { value: JsonPackedValue, journal: SerializedQueryJournal }> }` in `crates/convex/sync_types/src/types/mod.rs:341` via `crates/local_backend/src/subs/mod.rs:129` `run_sync_socket`; `SerializedQueryJournal` stays `Option<String>` encrypted in `crates/common/src/query_journal.rs:22` and `crates/keybroker/src/broker.rs:1171`; deltas, if added, are JSON-Patch (RFC 6902) on packed `ConvexValue` JSON with fallback to full value when patch ratio exceeds a threshold; WS stays bidirectional (`ClientMessage::Connect|ModifyQuerySet|Mutation|Action|Authenticate` in `crates/convex/sync_types/src/types/mod.rs:170`) with `MAX_MESSAGE_SIZE=5_000_000` chunking to `TransitionChunk` in `crates/local_backend/src/subs/mod.rs:373` and `HEARTBEAT_INTERVAL=5s`/`CLIENT_TIMEOUT=120s` in `crates/local_backend/src/subs/mod.rs:95`; SSE, if added later, is `GET /api/sse_sync` `text/event-stream` with `event: transition` reusing `TransitionChunk` fields and `POST /api/mutation` for writes, not a replacement.

## Alternatives considered

- **Per-field tracking in the journal**: track changed fields per document to skip re-execution; rejected — bookkeeping per write exceeds query re-execution cost for typical OLTP workloads and couples tracking to schema.
- **Delta-only transport**: always send JSON-Patch instead of full values; rejected — patch generation and client apply cost exceed savings on small results and complicates language clients that lack patch libraries.
- **SSE as primary, WS dropped**: `text/event-stream` server→client plus separate `POST` for mutations; rejected — loses ordered bidirectional multiplexing, doubles auth handling, needs separate reconnect logic, and compresses worse (base64 +33% for binary) versus `permessage-deflate` on WS.
- **Per-language tracking formats (e.g., Rust `IntervalSet` vs Kotlin structures)**: rejected — `sync_types` is the one wire format; guests remain opaque to `ReadSet`.

## Consequences

- `docs/optimization-notes.md` owns hot-path payload measurements; this note owns why WS stays primary and when JSON-Patch applies; `crates/convex/sync_types` owns the wire `Transition`/`TransitionChunk`/`StateModification` definitions.
- Mixed-language backends remain agnostic: WASM guests exchange `JsonPackedValue` bytes and `SerializedQueryJournal` strings without parsing `IntervalSet`.
- Payload work is additive and measurable: diff threshold, `permessage-deflate` knob `WS_PERMESSAGE_DEFLATE`, and `5 MB` chunking are reused for both transports.
- Verification: `cargo test -p database` `subscription` dedup tests, `cargo test -p convex --test sync_types`, `crates/local_backend` `sync` integration with `maybe_split_transition` coverage, and bandwidth bench on `npm-packages` sync fixtures.
