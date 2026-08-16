# Agent Note: Wire HTTP_SERVER_MAX_CONCURRENT_REQUESTS and fix beacon defaults

Status: implemented

## Problem

`HTTP_SERVER_MAX_CONCURRENT_REQUESTS` existed as a knob but was unused, so self-hosters could not bound HTTP server concurrency. The beacon (usage reporting) was off by default without a documented opt-in, and `docker-compose.yml` did not pass through the new env vars.

## Decision

The knob now gates the HTTP server's concurrent request limit. `docker-compose.yml` passes through `HTTP_SERVER_MAX_CONCURRENT_REQUESTS`, and the beacon is enabled by default with `CONVEX_ENABLE_BEACON=1` as the documented opt-out context. The beacon docs are corrected to match the shipped default.

## Alternatives considered

- **Middleware semaphore instead of a server-level limit**: a per-request middleware would still accept connections and buffer them; the server-level limit bounds connection acceptance, which is what protects memory under load.
- **Leave the knob unused**: a documented knob that does nothing is worse than no knob — operators tune against it and get no effect.

## Consequences

- Operators bound HTTP concurrency with one env var; the value is plumbed from `docker-compose.yml` through to the server.
- Env-var reference lives in `self-hosted/advanced/knobs.md`.
