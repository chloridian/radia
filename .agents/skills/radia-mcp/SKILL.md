---
name: radia-mcp
description: Launch and connect to Radia's opt-in local loopback MCP server (--mcp).
---

# Viewer Local MCP Server

## Starting the viewer with MCP enabled

By default, Radia binds no network listener. Pass `--mcp` (fixed default loopback port `47100`) or `--mcp-port <port>` (override loopback port) to start the local MCP HTTP server on `127.0.0.1`:

```sh
cargo run --bin viewer -- --mcp
cargo run --bin viewer -- --mcp-port 4040
```

Startup prints `MCP server listening on http://127.0.0.1:<port>`. The default endpoint URL is `http://127.0.0.1:47100/mcp` (served via `rmcp`'s stateful Streamable HTTP transport with SSE, capped at 8 live sessions with an idle timeout). Clients register the server under the name `radia`. Disconnecting an MCP client leaves the viewer running.

Tool catalog and usage guidance are provided directly by the server's `initialize` `instructions` field.
