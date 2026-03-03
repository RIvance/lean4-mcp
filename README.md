# lean4-mcp-proxy

An MCP (Model Context Protocol) server that proxies between AI agents and the Lean 4 language server. It lets agents open Lean files, check for errors, inspect proof goals, and edit documents through the standard MCP tool-calling interface.

> Inspired by [@Cui Chen](https://github.com/cu1ch3n)'s [vsrocq-mcp](https://github.com/cu1ch3n/vsrocq-mcp).

:warning: **Platform support**: Tested on Linux. Windows and macOS support is not guaranteed.


## Features

- **Open files** from disk or memory, with automatic Lake workspace detection
- **Get diagnostics** (errors, warnings) after Lean finishes type-checking
- **Query proof goals** at any cursor position inside a tactic block
- **Edit documents** — full replacement or targeted range edits


## Tools

| Tool | Description |
|------|-------------|
| `open_file` | Open a `.lean` file from disk by path (auto-detects Lake workspace) |
| `open_document` | Open a file by URI + text content |
| `get_diagnostics` | Get compilation errors/warnings (waits for Lean to finish) |
| `get_goal_state` | Query tactic proof state at a position |
| `replace_document` | Replace entire file content |
| `apply_edit` | Apply a targeted range edit |
| `get_document_text` | Read current document content |
| `close_document` | Close a document and free resources |
| `file_status` | Check if Lean is still processing a file |
| `list_documents` | List all open documents across all workspaces |

## Prerequisites

- [Rust](https://rustup.rs/) (edition 2021+)
- [Lean 4](https://leanprover.github.io/lean4/doc/setup.html) (`lean` on PATH)
- [Lake](https://github.com/leanprover/lake) (`lake` on PATH, included with Lean 4) — only needed for Lake projects

## Build

```sh
cargo build --release
```

The binary is at `target/release/lean4-mcp-proxy`.

## Usage

### VS Code / Cursor

Add to your MCP settings:

```json
{
  "mcpServers": {
    "lean4-mcp": {
      "command": "/path/to/lean4-mcp-proxy",
      "args": []
    }
  }
}
```

Lake workspaces are auto-detected from `lakefile.lean` when you open a file. You can also set a default:

```json
{
  "mcpServers": {
    "lean4-mcp": {
      "command": "/path/to/lean4-mcp-proxy",
      "args": ["--lake-project", "/path/to/your/lake/project"]
    }
  }
}
```

### Environment variables

| Variable | Description |
|----------|-------------|
| `LAKE_PROJECT` | Default Lake project root (alternative to `--lake-project`) |
| `LEAN_BIN` | Custom path to `lean` binary |
| `LAKE_BIN` | Custom path to `lake` binary |
| `RUST_LOG` | Log level (`info`, `debug`, `trace`) — logs go to stderr |

## Protocol

Communicates over **stdio** using newline-delimited JSON-RPC (MCP protocol version `2024-11-05`). The proxy translates MCP tool calls into LSP requests to the Lean 4 language server.
