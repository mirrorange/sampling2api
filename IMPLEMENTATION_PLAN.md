## Stage 1: Protocol Foundation
**Goal**: Initialize the Rust project and define the Anthropic Messages <-> MCP Sampling protocol models and conversion boundaries.
**Success Criteria**: The crate builds, conversion helpers exist for request/response translation, and unit tests cover core mapping rules and validation failures.
**Tests**: `cargo test protocol`
**Status**: Complete

## Stage 2: Stdio Bridge
**Goal**: Implement the shared peer registry and non-streaming `/v1/messages` bridge backed by MCP sampling over stdio.
**Success Criteria**: A connected MCP client can initialize, register, and serve non-streaming Anthropic-compatible requests via HTTP.
**Tests**: `cargo test stdio_bridge`
**Status**: Complete

## Stage 3: Streamable HTTP Bridge
**Goal**: Add Streamable HTTP MCP transport support and session-aware peer selection for the Messages API.
**Success Criteria**: The service can host `/mcp` and `/v1/messages` together, register HTTP MCP sessions, and route requests to the correct peer.
**Tests**: `cargo test http_bridge`
**Status**: In Progress

## Stage 4: Streaming and Polish
**Goal**: Convert completed MCP sampling responses into Anthropic-style SSE event streams and finalize CLI/documentation.
**Success Criteria**: `stream: true` returns a valid SSE response sequence, tests cover text and tool-use streaming, and the implementation plan is completed then removed.
**Tests**: `cargo test streaming`
**Status**: Not Started
