<div align="center">

# Sampling2API

**Expose MCP Client Sampling as an Anthropic-compatible Messages API**

[![Rust](https://img.shields.io/badge/Rust-2024_Edition-orange?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/Protocol-MCP-green)](https://modelcontextprotocol.io/)

[English](#english) · [中文](#中文)

</div>

---

## English

### Overview

`sampling2api` is a bridge tool written in Rust that exposes the [MCP (Model Context Protocol)](https://modelcontextprotocol.io/) client's **Sampling** capability as a standard [Anthropic Messages API](https://docs.anthropic.com/en/api/messages) (`POST /v1/messages`).

This allows any external service or tool that already speaks the Anthropic API to seamlessly use the AI capabilities provided by an MCP client — **no external API keys or provider configuration needed**.

### Motivation

In the MCP ecosystem, **Sampling** lets servers request LLM completions from clients. However, most existing tools and libraries are built against well-known HTTP APIs (like Anthropic's). `sampling2api` bridges this gap: it accepts standard Anthropic Messages API requests over HTTP and forwards them as MCP `sampling/createMessage` calls to the connected client.

### Features

- **Anthropic Messages API compatible** — `POST /v1/messages` endpoint with JSON and streaming (SSE) responses
- **Two transport modes**:
  - **Stdio** — connects to an MCP client over stdin/stdout (ideal for subprocess-based setups)
  - **Streamable HTTP** — connects to an MCP client over HTTP (ideal for remote or multi-client scenarios)
- **Tool use support** — full round-trip for tool calls, tool results, and multi-turn tool loops
- **Image support** — base64-encoded image content passthrough
- **Multi-session routing** — in HTTP mode, multiple MCP clients can connect simultaneously, routed via the `x-mcp-session-id` header
- **Health check endpoint** — `GET /health`

### Installation

```bash
# Clone the repository
git clone https://github.com/anthropics/sampling2api.git
cd sampling2api

# Build with Cargo
cargo build --release
```

The binary will be at `target/release/sampling2api`.

### Usage

#### Stdio Mode

Connect to an MCP client over stdin/stdout and expose the API on a local HTTP port:

```bash
sampling2api stdio --listen 127.0.0.1:38080
```

The MCP client launches `sampling2api` as a subprocess (or vice versa). Once connected, send requests to `http://127.0.0.1:38080/v1/messages`.

#### Streamable HTTP Mode

Run as an HTTP server that accepts both MCP client connections and API requests:

```bash
sampling2api http --listen 127.0.0.1:38080 --mcp-path /mcp
```

- MCP clients connect via the Streamable HTTP transport at `http://127.0.0.1:38080/mcp`
- API consumers send requests to `http://127.0.0.1:38080/v1/messages`

When multiple MCP clients are connected, use the `x-mcp-session-id` header to route requests to a specific session.

### API Reference

#### `POST /v1/messages`

Accepts a standard Anthropic Messages API request body:

```json
{
  "model": "claude-sonnet-4-0",
  "max_tokens": 1024,
  "messages": [
    { "role": "user", "content": "Hello, world!" }
  ],
  "stream": false
}
```

- Set `"stream": true` for Server-Sent Events (SSE) streaming responses
- Set `"stream": false` (or omit) for a single JSON response
- The `model` field is passed as a hint to the MCP client's model preferences

**Headers:**

| Header | Required | Description |
|---|---|---|
| `x-mcp-session-id` | No | Route to a specific MCP client session (HTTP mode with multiple clients) |

#### `GET /health`

Returns `ok` — useful for readiness and liveness probes.

### Architecture

```
┌──────────────┐         ┌──────────────────┐         ┌────────────┐
│  API Consumer│  HTTP    │   sampling2api   │   MCP   │ MCP Client │
│  (e.g. app)  │────────▶│                  │────────▶│ (with LLM) │
│              │◀────────│  /v1/messages     │◀────────│            │
└──────────────┘  JSON/  └──────────────────┘ sampling └────────────┘
                   SSE          Bridge         /createMessage
```

1. An API consumer sends an Anthropic-format request to `/v1/messages`
2. `sampling2api` converts it to an MCP `sampling/createMessage` request
3. The MCP client processes the request (invoking its LLM)
4. The result is converted back to an Anthropic Messages API response

### Supported Conversions

| Anthropic Feature | MCP Sampling | Status |
|---|---|---|
| Text messages | ✅ | Supported |
| Image content (base64) | ✅ | Supported |
| System prompt | ✅ | Supported |
| Tool definitions | ✅ | Supported |
| Tool use / Tool results | ✅ | Supported |
| Tool choice (auto/any/none) | ✅ | Supported |
| Tool choice (specific tool) | ❌ | Not representable in MCP |
| Temperature | ✅ | Supported |
| Stop sequences | ✅ | Supported |
| Max tokens | ✅ | Supported |
| Model hints | ✅ | Via model preferences |
| Streaming (SSE) | ✅ | Simulated from full response |
| Metadata passthrough | ✅ | Supported |

### Tech Stack

- **Rust** (2024 edition)
- [rmcp](https://crates.io/crates/rmcp) — MCP protocol implementation
- [axum](https://crates.io/crates/axum) — HTTP framework
- [tokio](https://crates.io/crates/tokio) — Async runtime
- [serde](https://crates.io/crates/serde) / [serde_json](https://crates.io/crates/serde_json) — Serialization

---

## 中文

### 概述

`sampling2api` 是一个用 Rust 编写的桥接工具，它将 [MCP（Model Context Protocol）](https://modelcontextprotocol.io/)客户端的 **Sampling（采样）** 能力暴露为标准的 [Anthropic Messages API](https://docs.anthropic.com/en/api/messages)（`POST /v1/messages`）。

这使得任何已经对接 Anthropic API 的外部服务或工具，都能无缝使用 MCP 客户端提供的 AI 能力——**无需配置外部 API 密钥或提供商信息**。

### 动机

在 MCP 生态中，**Sampling** 允许服务器向客户端请求 LLM 补全。然而，大多数现有工具和库是基于主流 HTTP API（如 Anthropic）构建的。`sampling2api` 弥合了这一鸿沟：它通过 HTTP 接收标准 Anthropic Messages API 请求，并将其转发为 MCP `sampling/createMessage` 调用发送给已连接的客户端。

### 特性

- **兼容 Anthropic Messages API** — `POST /v1/messages` 端点，支持 JSON 和流式（SSE）响应
- **两种传输模式**：
  - **Stdio** — 通过 stdin/stdout 连接 MCP 客户端（适合子进程方式）
  - **Streamable HTTP** — 通过 HTTP 连接 MCP 客户端（适合远程或多客户端场景）
- **工具调用支持** — 完整的工具调用、工具结果和多轮工具循环
- **图像支持** — base64 编码的图像内容透传
- **多会话路由** — HTTP 模式下多个 MCP 客户端可同时连接，通过 `x-mcp-session-id` 头进行路由
- **健康检查端点** — `GET /health`

### 安装

```bash
# 克隆仓库
git clone https://github.com/anthropics/sampling2api.git
cd sampling2api

# 使用 Cargo 构建
cargo build --release
```

编译产物位于 `target/release/sampling2api`。

### 使用方法

#### Stdio 模式

通过 stdin/stdout 连接 MCP 客户端，并在本地 HTTP 端口暴露 API：

```bash
sampling2api stdio --listen 127.0.0.1:38080
```

MCP 客户端将 `sampling2api` 作为子进程启动（或反向亦可）。连接成功后，向 `http://127.0.0.1:38080/v1/messages` 发送请求即可。

#### Streamable HTTP 模式

作为 HTTP 服务器运行，同时接受 MCP 客户端连接和 API 请求：

```bash
sampling2api http --listen 127.0.0.1:38080 --mcp-path /mcp
```

- MCP 客户端通过 Streamable HTTP 传输连接到 `http://127.0.0.1:38080/mcp`
- API 消费者向 `http://127.0.0.1:38080/v1/messages` 发送请求

当多个 MCP 客户端连接时，使用 `x-mcp-session-id` 请求头将请求路由到特定会话。

### API 参考

#### `POST /v1/messages`

接受标准 Anthropic Messages API 请求体：

```json
{
  "model": "claude-sonnet-4-0",
  "max_tokens": 1024,
  "messages": [
    { "role": "user", "content": "Hello, world!" }
  ],
  "stream": false
}
```

- 设置 `"stream": true` 获取 SSE 流式响应
- 设置 `"stream": false`（或省略）获取单次 JSON 响应
- `model` 字段作为模型偏好提示传递给 MCP 客户端

**请求头：**

| 请求头 | 是否必须 | 说明 |
|---|---|---|
| `x-mcp-session-id` | 否 | 路由到特定 MCP 客户端会话（HTTP 模式下多客户端时使用） |

#### `GET /health`

返回 `ok`，可用于就绪和存活探针。

### 架构

```
┌──────────────┐         ┌──────────────────┐         ┌────────────┐
│  API 消费者   │  HTTP    │   sampling2api   │   MCP   │ MCP 客户端  │
│  (如应用程序) │────────▶│                  │────────▶│ (含 LLM)   │
│              │◀────────│  /v1/messages     │◀────────│            │
└──────────────┘  JSON/  └──────────────────┘ sampling └────────────┘
                   SSE          桥接层         /createMessage
```

1. API 消费者向 `/v1/messages` 发送 Anthropic 格式的请求
2. `sampling2api` 将其转换为 MCP `sampling/createMessage` 请求
3. MCP 客户端处理请求（调用其 LLM）
4. 结果被转换回 Anthropic Messages API 响应格式

### 转换支持表

| Anthropic 特性 | MCP Sampling | 状态 |
|---|---|---|
| 文本消息 | ✅ | 支持 |
| 图像内容（base64） | ✅ | 支持 |
| 系统提示词 | ✅ | 支持 |
| 工具定义 | ✅ | 支持 |
| 工具调用 / 工具结果 | ✅ | 支持 |
| 工具选择（auto/any/none） | ✅ | 支持 |
| 工具选择（指定工具） | ❌ | MCP 中无法表示 |
| Temperature | ✅ | 支持 |
| 停止序列 | ✅ | 支持 |
| 最大 token 数 | ✅ | 支持 |
| 模型提示 | ✅ | 通过模型偏好传递 |
| 流式响应（SSE） | ✅ | 基于完整响应模拟 |
| 元数据透传 | ✅ | 支持 |

### 技术栈

- **Rust**（2024 edition）
- [rmcp](https://crates.io/crates/rmcp) — MCP 协议实现
- [axum](https://crates.io/crates/axum) — HTTP 框架
- [tokio](https://crates.io/crates/tokio) — 异步运行时
- [serde](https://crates.io/crates/serde) / [serde_json](https://crates.io/crates/serde_json) — 序列化
