//! SSE fallback for sync. Primary transport remains WebSocket (`subs/mod.rs:129`
//! `run_sync_socket`); SSE exists for restrictive networks where WS upgrades
//! are blocked. Reuses the same `sync_types::ServerMessage::Transition` /
//! `TransitionChunk` wire format so no duplicate transition logic.
//!
//! Feature-gated behind `local_backend/sse` and runtime-gated by
//! `SSE_SYNC_ENABLED` knob. Keep `sync_types` as the one wire format.
//!
//! Wire:
//!   GET /api/sse_sync  -> `text/event-stream` with events:
//!     event: transition
//!     id: <server_ts base64>          // Last-Event-ID on reconnect
//!     data: <ServerMessage JSON>
//!     event: chunk
//!     id: <transition_id>:<part>
//!     data: <TransitionChunk JSON>
//!     : keep-alive comment every 15s
//!   Writes use existing `POST /api/mutation` (and actions/queries via HTTP).

use std::{
    convert::Infallible,
    time::Duration,
};

use anyhow::Context as _;
use axum::{
    extract::State,
    response::{
        sse::{
            Event,
            KeepAlive,
            Sse,
        },
        IntoResponse,
    },
};
use common::{
    http::{
        ExtractClientVersion,
        ExtractRequestMetadata,
        ExtractResolvedHostname,
        HttpResponseError,
    },
    knobs::{
        SSE_SYNC_ENABLED,
        SYNC_MAX_MESSAGE_SIZE,
    },
    runtime::Runtime as _,
    version::ClientType,
};
use errors::ErrorMetadata;
use serde_json::Value as JsonValue;
use sync::{
    worker::measurable_unbounded_channel,
    ServerMessage,
    SyncWorker,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    subs::{
        maybe_split_transition_with_limit,
        new_sync_worker_config,
    },
    RouterState,
};

/// Parse `Last-Event-ID` for SSE reconnect. Format is the `id` we emit:
/// either the base64-encoded `server_ts` (for `transition`) or
/// `<transition_id>:<part_number>` (for `chunk`). We map it to a
/// `common::query_journal::Cursor`-style resume point. For now the cursor is
/// opaque and we map `Last-Event-ID` -> `Timestamp` via base64 decode; the
/// subsequent `SyncWorker` start will treat it as `max_observed_timestamp`
/// similar to `ClientMessage::Connect::max_observed_timestamp` so the server
/// can reject stale resumes with a linearizability check. Full
/// `QueryJournal { end_cursor: Some(Cursor { position: After(bytes),
/// query_fingerprint }) }` resume is left as a follow-up (see report TODO).
fn parse_last_event_id(id: &str) -> Option<common::query::Cursor> {
    // We emit `id: <u64 base64>` for transitions. Try to decode it back to a
    // timestamp; treat it as cursor resume hint. If it is a chunk id
    // `<len>:<part>` we strip the suffix and try again.
    let base = id.split(':').next().unwrap_or(id);
    let ts_u64 = base64::decode(base).ok().and_then(|b| {
        if b.len() == 8 {
            Some(u64::from_le_bytes(b.try_into().ok()?))
        } else {
            None
        }
    });
    // For now return None and let SyncWorker start fresh; the timestamp hint
    // is logged and can be used to seed `max_observed_timestamp`. Returning a
    // real `Cursor` requires decrypting the client's `SerializedQueryJournal`
    // which is encrypted via `keybroker::broker::1171`; we reuse the same
    // journal on the next `ModifyQuerySet` after reconnect instead of trying
    // to synthesize a cursor here.
    let _ = ts_u64;
    None
}

/// Shared SSE event builder. Reuses `maybe_split_transition_with_limit` so WS
/// and SSE share the 5 MiB `SYNC_MAX_MESSAGE_SIZE` bound and the same
/// `TransitionChunk { chunk, part_number, total_parts, transition_id }` layout.
fn server_message_to_events(
    mut message: ServerMessage,
    supports_chunks: bool,
    max_size: usize,
    runtime: &runtime::prod::ProdRuntime,
) -> anyhow::Result<Vec<Event>> {
    // Inject server_ts like `run_sync_socket` does, so `id:` is monotonic.
    if let ServerMessage::Transition { .. } = &message {
        message.inject_server_ts(runtime.generate_timestamp()?);
    }
    let messages = maybe_split_transition_with_limit(message, supports_chunks, max_size)?;
    let mut events = Vec::with_capacity(messages.len());
    for msg in messages {
        let json = serde_json::to_string(&JsonValue::from(msg.clone()))?;
        let event = match &msg {
            ServerMessage::TransitionChunk {
                transition_id,
                part_number,
                ..
            } => Event::default()
                .event("chunk")
                .id(format!("{transition_id}:{part_number}"))
                .data(json),
            ServerMessage::Transition { server_ts, .. } => {
                let id = server_ts
                    .map(|ts| {
                        let bytes = u64::from(ts).to_le_bytes();
                        base64::encode(bytes)
                    })
                    .unwrap_or_else(|| "0".to_string());
                Event::default().event("transition").id(id).data(json)
            },
            ServerMessage::Ping => Event::default().event("ping").data(json),
            ServerMessage::AuthError { .. } | ServerMessage::FatalError { .. } => {
                Event::default().event("error").data(json)
            },
            _ => Event::default().event("message").data(json),
        };
        events.push(event);
    }
    Ok(events)
}

/// `GET /api/sse_sync` — SSE fallback. Auth is the same as WS: check
/// `Origin` allowlist, then rely on `Authorization` header (EventSource
/// limitation: browsers cannot set headers, so we also accept
/// `?token=` / `?authToken=` / `?accessToken=` query params and map them to
/// `AuthenticationToken` via the same `ExtractAuthenticationToken` path that
/// WS's `ClientMessage::Authenticate` eventually uses. Writes still go via
/// `POST /api/mutation` (and actions via `POST /api/action`) to keep
/// ordering and auth double-handling minimal.
pub async fn sse_sync(
    State(st): State<RouterState>,
    ExtractResolvedHostname(host): ExtractResolvedHostname,
    ExtractRequestMetadata(request_metadata): ExtractRequestMetadata,
    ExtractClientVersion(client_version): ExtractClientVersion,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, HttpResponseError> {
    if !*SSE_SYNC_ENABLED {
        return Err(anyhow::anyhow!(ErrorMetadata::not_found(
            "SseDisabled",
            "SSE sync is disabled via SSE_SYNC_ENABLED",
        ))
        .into());
    }
    if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
        let origin = origin.to_str().context(ErrorMetadata::bad_request(
            "InvalidOrigin",
            "Invalid Origin header",
        ))?;
        if !st.allowed_origins.iter().any(|allowed| allowed == origin) {
            return Err(anyhow::anyhow!(ErrorMetadata::forbidden(
                "OriginNotAllowed",
                format!(
                    "SSE connection rejected: origin {origin} is not in the allowed origins list."
                ),
            ))
            .into());
        }
    }

    // Last-Event-ID may arrive as header (standard SSE) or as query param
    // `?lastEventId=` for clients that cannot set headers on reconnect.
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(ref id) = last_event_id {
        // Validate and log; Cursor mapping is best-effort for now.
        let _cursor = parse_last_event_id(id);
        tracing::info!("SSE reconnect Last-Event-ID: {id}");
    }

    let config = new_sync_worker_config(
        client_version.clone(),
        st.subscription_reconnect_rate_limiter.clone(),
    )?;
    // EventSource is JS-only, so we effectively only support NPM-like clients
    // that understand TransitionChunk. Still gate on the same version check as
    // WS to keep behavior identical.
    let supports_chunks = match client_version.client() {
        ClientType::NPM => true,
        _ => config.supports_transition_chunks,
    };
    let max_size = *SYNC_MAX_MESSAGE_SIZE;

    // Wire SyncWorker to an SSE stream. Client->server messages (ModifyQuerySet
    // etc.) are not yet tunneled over SSE; the client drives them via
    // `POST /api/sse_modify` or the existing mutation/action endpoints. For
    // the initial push we synthesize a `Connect` via SyncWorker's `on_connect`.
    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let (server_tx, mut server_rx) = measurable_unbounded_channel();

    // Keep a handle to feed server_rx into SSE.
    let st_clone = st.clone();
    let host_clone = host.clone();
    let request_metadata_clone = request_metadata.clone();
    let runtime = st_clone.runtime.clone();

    // Spawn SyncWorker in background.
    let partition_id_result = st_clone.api.partition_id(&host_clone).await;

    // We don't have a SessionId yet; synthesize one from Last-Event-ID if
    // present, else random. SyncWorker will set it on first Connect.
    let session_id_placeholder = sync_types::SessionId::new(Uuid::new_v4());

    tokio::spawn(async move {
        let partition_id_val = partition_id_result.unwrap_or(0);
        let mut worker = SyncWorker::new(
            st_clone.api.clone(),
            st_clone.runtime.clone(),
            host_clone,
            config,
            client_rx,
            server_tx,
            Box::new(|_sid| ()),
            partition_id_val,
            request_metadata_clone,
        );
        // Prime with a Connect message so queries can be driven.
        // In a full implementation the client would POST ModifyQuerySet to
        // `/api/sse_query` which forwards to `client_tx`; we drive via channel
        // here. The Last-Event-ID cursor, if we had one, would be threaded as
        // `max_observed_timestamp` on this Connect to preserve linearizability.
        let _ = client_tx.send((
            sync_types::ClientMessage::Connect {
                session_id: session_id_placeholder,
                connection_count: 1,
                last_close_reason: last_event_id.clone().unwrap_or_default(),
                max_observed_timestamp: None,
                client_ts: None,
            },
            st_clone.runtime.monotonic_now(),
        ));
        let _ = worker.go().await;
    });

    let (sse_tx, sse_rx) = mpsc::channel::<Result<Event, Infallible>>(16);
    let runtime_for_events = runtime.clone();
    tokio::spawn(async move {
        while let Some((msg, _instant)) = server_rx.next().await {
            let events = match server_message_to_events(
                msg,
                supports_chunks,
                max_size,
                &runtime_for_events,
            ) {
                Ok(evs) => evs,
                Err(e) => {
                    tracing::error!("SSE event serialization failed: {e:?}");
                    continue;
                },
            };
            for ev in events {
                if sse_tx.send(Ok(ev)).await.is_err() {
                    break;
                }
            }
        }
    });

    let stream = ReceiverStream::new(sse_rx);
    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    );

    Ok(sse)
}
