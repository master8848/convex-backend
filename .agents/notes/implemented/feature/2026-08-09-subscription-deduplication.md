# Agent Note: Subscription deduplication across clients

Status: implemented

## Problem

Multiple clients watching the same query each held their own subscription manager entry: one `ReadSet` and one interval-map footprint per client, even when the read set, refreshed timestamp, and system flag were identical. The cost scaled with client count instead of distinct query count.

## Decision

Identical subscriptions (same read set, refreshed timestamp, and system flag) share a single subscription manager entry. Each client gets its own handle that shares the validity and watch state, so an invalidation event fans out to all handles. The shared entry is released when the last client handle drops and is evicted from the dedup map so a later subscriber creates a fresh subscription; an invalidated subscription releases its handles the same way.

Two bugs in the initial implementation are fixed in the shipped state: the user counter was initialized to 1 and never reached the release condition, leaking manager entries forever; and released entries stayed in the dedup map, so a later subscriber could silently reuse a dead subscription and never receive invalidation events.

## Alternatives considered

- **Value-keyed dedup by query text only**: identical query text with different execution state (refresh timestamps, system flags) would collide; the key is the full subscription identity, not the source text.
- **Global query result cache**: shares results but not the reactive watch state, which is the expensive footprint; dedup at the subscription layer removes the manager cost without changing result semantics.

## Consequences

- Subscriber-visible semantics are unchanged: each client still receives its own invalidation events and refreshes; only the shared machinery is deduplicated.
- The two lifecycle bugs mean the correctness of release and eviction is part of the contract: a handle must never outlive its shared entry, and a stale entry must never be reused.
