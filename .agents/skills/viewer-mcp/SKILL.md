---
name: viewer-mcp
description: Inspect a running Radia viewer through its opt-in local loopback MCP server (--mcp).
---

# Viewer Local MCP Server

## Starting the viewer with MCP enabled

By default, Radia binds no network listener. Pass `--mcp` (fixed default loopback port `47100`) or `--mcp-port <port>` (override loopback port) to start the local MCP HTTP server on `127.0.0.1`:

```sh
cargo run --bin viewer -- --mcp
cargo run --bin viewer -- --mcp-port 4040
```

Startup prints `MCP server listening on http://127.0.0.1:<port>`. The default endpoint URL is `http://127.0.0.1:47100/` (the server accepts JSON-RPC `POST` requests on any path, including `/` and `/mcp`). Disconnecting an MCP client leaves the viewer running.

## Available read-only tools

| Tool | Answers |
|---|---|
| `world` | Active regions, bounded object lists, or single object properties by UUID (`query`: `summary`, `regions`, `objects`, `object`). |
| `stats` | Frame timing (`fps`, `frame_ms`, `game_ms`, `render_ms`, `gpu_timings_ms`), `render::Stats`, `AssetQueueStats`, and memory stats. |
| `cache` | Object cache write-queue depth/capacity, busy/full drops, handshake state, and asset disk cache hit/miss/eviction counts. |
| `inventory` | Inventory folder fetch counts (`pending`, `in_flight`, `done`, `failed`) and a bounded list of failed folders (UUIDs, retry state, typed error; no items or asset IDs). |
| `network` | Per-region circuit packet counts/rates, unacked/resent counts, throttle settings, `EventQueueGet` state, and capability names + last status (no URLs or session IDs). |
| `changes` | Bounded page of the World change stream (`cursor`, `limit`) returning change kinds (updates, kills, reparents) and `next_cursor`. |
| `textures` | Aggregate texture fetch/decode queue depths by priority, discard-level histogram, GPU texture memory vs budget, and slow in-flight fetch count (>30s); no texture UUIDs. |
| `environment` | Active sky, water, and day-cycle values plus source (`region`, `parcel`, `local_override`) for the agent's region (values only, no asset UUIDs). |
| `camera` | Camera mode, position, direction, agent region name/handle, and region-local position. |
| `logs` | Recent `WARN` and `ERROR` entries from the in-memory ring buffer (sensitive fields and capability URLs omitted). |
| `screenshot` | Non-blocking viewport PNG capture encoded off the GUI thread. |
| `explain_object` | Combined World detail + renderer placement, culling, mesh residency, and per-face PBR/legacy material shading, reason, texture/alpha/omission state for an `object_id` (no asset UUIDs). |
| `pick` | Geometry pick at viewport pixel `(x, y)` returning hit point, entity, object UUID, face slot, and base/emissive texture presence. |
| `wait_for` | Bounded `watch`-channel wait (`world_revision`, `asset_queue_empty`, `texture_event`, max 30s timeout) with zero polling. |

## Bounds and safety

- Strictly bound to `127.0.0.1`.
- Concurrency capped at 4 in-flight requests (`HTTP 429` when saturated).
- Request body capped at 64 KiB (`HTTP 413`); JSON responses capped at 512 KiB; PNG responses capped at 8 MiB.
- Never exposes texture/asset UUIDs, capability URLs, session tokens, or credentials.

## Adding a new tool

1. Create `apps/viewer/src/mcp/tools/<name>.rs` exporting `pub const SPEC: ToolSpec`.
2. Add `mod <name>;` and `<name>::SPEC` to `TOOLS` in `apps/viewer/src/mcp/tools/mod.rs`.
