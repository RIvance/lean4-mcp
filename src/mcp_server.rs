use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::jsonrpc::{self, JsonRpcResponse};
use crate::lean_client::LeanClient;

/// Manages multiple LeanClient instances, one per lake workspace (or standalone).
/// Clients are lazily spawned on first use.
///
/// Workspace keys:
///   - `""` (empty string) → standalone mode (`lean --server`)
///   - `"/absolute/path/to/lake/project"` → lake mode (`lake serve` in that dir)
struct ClientManager {
    /// Map from canonical lake workspace path → LeanClient.
    /// Empty string key = standalone (no lake project).
    clients: Mutex<HashMap<String, Arc<LeanClient>>>,
    /// Map from document URI → workspace key, so we know which client owns each URI.
    uri_to_workspace: Mutex<HashMap<String, String>>,
    /// Default lake workspace from CLI `--lake-project` or `LAKE_PROJECT` env var.
    /// Used when the agent doesn't specify `lake_workspace` and auto-detection fails.
    default_lake_root: Option<String>,
}

impl ClientManager {
    fn new(default_lake_root: Option<String>) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            uri_to_workspace: Mutex::new(HashMap::new()),
            default_lake_root,
        }
    }

    /// Get or lazily spawn a LeanClient for the given workspace key.
    ///
    /// - `workspace_key = ""` → standalone mode
    /// - `workspace_key = "/path/to/project"` → lake project mode
    async fn get_or_spawn(&self, workspace_key: &str) -> Result<Arc<LeanClient>> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(workspace_key) {
            return Ok(client.clone());
        }

        let lake_root = if workspace_key.is_empty() {
            None
        } else {
            Some(workspace_key.to_string())
        };

        info!(
            "Spawning new Lean server for workspace: {}",
            if workspace_key.is_empty() {
                "(standalone)"
            } else {
                workspace_key
            }
        );

        let client = Arc::new(LeanClient::spawn(lake_root).await?);
        clients.insert(workspace_key.to_string(), client.clone());
        Ok(client)
    }

    /// Resolve a workspace key for opening a file.
    ///
    /// Priority:
    /// 1. Explicit `lake_workspace` argument from the agent
    /// 2. Auto-detect by searching for lakefile.lean / lakefile.toml upward from the file path
    /// 3. CLI default (`--lake-project` / `LAKE_PROJECT`)
    /// 4. Standalone mode (empty string)
    fn resolve_workspace(&self, file_path: Option<&str>, explicit_workspace: Option<&str>) -> String {
        // 1. Explicit workspace
        if let Some(ws) = explicit_workspace {
            let ws = ws.trim();
            if !ws.is_empty() {
                return self.canonicalize_path(ws);
            }
        }

        // 2. Auto-detect from file path
        if let Some(path) = file_path {
            if let Some(detected) = Self::detect_lake_workspace(path) {
                info!("Auto-detected lake workspace for {path}: {detected}");
                return detected;
            }
        }

        // 3. CLI default
        if let Some(ref default) = self.default_lake_root {
            return default.clone();
        }

        // 4. Standalone
        String::new()
    }

    /// Search upward from a file path for `lakefile.lean`, `lakefile.toml`, or `lakefile`.
    fn detect_lake_workspace(file_path: &str) -> Option<String> {
        let path = std::path::Path::new(file_path);
        let mut dir = if path.is_file() || !path.exists() {
            path.parent()?
        } else {
            path
        };

        loop {
            for lakefile_name in &["lakefile.lean", "lakefile.toml", "lakefile"] {
                if dir.join(lakefile_name).exists() {
                    return std::fs::canonicalize(dir)
                        .ok()
                        .map(|p| p.to_string_lossy().to_string());
                }
            }
            match dir.parent() {
                Some(parent) if parent != dir => {
                    dir = parent;
                }
                _ => break,
            }
        }
        None
    }

    fn canonicalize_path(&self, path: &str) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| std::path::PathBuf::from(path))
            .to_string_lossy()
            .to_string()
    }

    /// Register a URI → workspace mapping.
    async fn register_uri(&self, uri: &str, workspace_key: &str) {
        let mut map = self.uri_to_workspace.lock().await;
        map.insert(uri.to_string(), workspace_key.to_string());
    }

    /// Unregister a URI → workspace mapping.
    async fn unregister_uri(&self, uri: &str) {
        let mut map = self.uri_to_workspace.lock().await;
        map.remove(uri);
    }

    /// Look up which client owns a given URI.
    async fn client_for_uri(&self, uri: &str) -> Result<Arc<LeanClient>> {
        let workspace_key = {
            let map = self.uri_to_workspace.lock().await;
            map.get(uri).cloned()
        };

        match workspace_key {
            Some(key) => self.get_or_spawn(&key).await,
            None => {
                anyhow::bail!(
                    "Document not open: {uri}. Use open_file or open_document first."
                )
            }
        }
    }

    /// List all open documents across all clients, with their workspace info.
    async fn list_all_documents(&self) -> Vec<(String, String)> {
        let map = self.uri_to_workspace.lock().await;
        map.iter()
            .map(|(uri, ws)| (uri.clone(), ws.clone()))
            .collect()
    }
}

/// The MCP Server reads JSON-RPC requests from stdin and writes responses to stdout.
/// It delegates Lean-specific operations to the appropriate `LeanClient` via `ClientManager`.
pub struct McpServer {
    manager: ClientManager,
}

impl McpServer {
    pub fn new(default_lake_root: Option<String>) -> Self {
        Self {
            manager: ClientManager::new(default_lake_root),
        }
    }

    /// Run the MCP server loop: read from stdin, dispatch, write to stdout.
    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        info!("MCP server listening on stdin...");

        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }

            debug!("← mcp: {trimmed}");

            let parsed: Value = match serde_json::from_str(&trimmed) {
                Ok(value) => value,
                Err(error) => {
                    let error_response = JsonRpcResponse::error(
                        Value::Null,
                        jsonrpc::PARSE_ERROR,
                        format!("Parse error: {error}"),
                    );
                    let response_json = serde_json::to_string(&error_response)?;
                    debug!("→ mcp: {response_json}");
                    stdout.write_all(response_json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    continue;
                }
            };

            // Extract the request id and method
            let request_id = parsed.get("id").cloned().unwrap_or(Value::Null);
            let method = parsed
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let params = parsed.get("params").cloned();

            let response = match method {
                "initialize" => self.handle_initialize(request_id.clone(), params).await,
                "notifications/initialized" | "notifications/cancelled" => {
                    // Client notifications; no response expected
                    debug!("Client sent {method}");
                    continue;
                }
                "tools/list" => self.handle_tools_list(request_id.clone()).await,
                "tools/call" => self.handle_tools_call(request_id.clone(), params).await,
                "resources/list" => {
                    // We don't expose any resources
                    Ok(JsonRpcResponse::success(request_id.clone(), json!({ "resources": [] })))
                }
                "resources/templates/list" => {
                    Ok(JsonRpcResponse::success(request_id.clone(), json!({ "resourceTemplates": [] })))
                }
                "prompts/list" => {
                    // We don't expose any prompts
                    Ok(JsonRpcResponse::success(request_id.clone(), json!({ "prompts": [] })))
                }
                "ping" => Ok(JsonRpcResponse::success(request_id.clone(), json!({}))),
                other => {
                    warn!("Unknown MCP method: {other}");
                    Ok(JsonRpcResponse::error(
                        request_id.clone(),
                        jsonrpc::METHOD_NOT_FOUND,
                        format!("Method not found: {other}"),
                    ))
                }
            };

            match response {
                Ok(resp) => {
                    let response_json = serde_json::to_string(&resp)?;
                    debug!("→ mcp: {response_json}");
                    stdout.write_all(response_json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
                Err(error) => {
                    error!("Error handling MCP request: {error}");
                    let error_response = JsonRpcResponse::error(
                        request_id,
                        jsonrpc::INTERNAL_ERROR,
                        format!("Internal error: {error}"),
                    );
                    let response_json = serde_json::to_string(&error_response)?;
                    stdout.write_all(response_json.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }

        info!("MCP stdin closed, shutting down");
        Ok(())
    }

    // MCP protocol handlers

    /// Handle MCP `initialize` request.
    async fn handle_initialize(
        &self,
        request_id: Value,
        _params: Option<Value>,
    ) -> Result<JsonRpcResponse> {
        info!("MCP initialize request received");
        Ok(JsonRpcResponse::success(
            request_id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "lean4-mcp-proxy",
                    "version": "0.2.0"
                }
            }),
        ))
    }

    /// Handle MCP `tools/list` request.
    async fn handle_tools_list(&self, request_id: Value) -> Result<JsonRpcResponse> {
        let tools = json!({
            "tools": [
                {
                    "name": "open_file",
                    "description": "Open a Lean 4 source file from disk in the language server by its absolute file path. The file content is read automatically from disk. This MUST be called before any other operations (get_goal_state, get_diagnostics, etc.) on a file. After opening, Lean will begin type-checking the file — use get_diagnostics with a timeout to wait for results. If the file belongs to a Lake project (has a lakefile.lean in a parent directory), the workspace is auto-detected. You can also pass lake_workspace explicitly.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute path to the .lean file on disk (e.g. /home/user/project/MyFile.lean)"
                            },
                            "lake_workspace": {
                                "type": "string",
                                "description": "Optional. Absolute path to the Lake project root directory (the folder containing lakefile.lean/lakefile.toml). If omitted, the workspace is auto-detected by searching for lakefile.lean upward from the file path. If no lakefile is found, standalone mode (lean --server) is used."
                            }
                        },
                        "required": ["path"]
                    }
                },
                {
                    "name": "open_document",
                    "description": "Open a Lean 4 source file by providing a file URI and full text content directly. Use this when you have the content in memory (e.g. a new unsaved file). For files already on disk, prefer open_file instead. Supports optional lake_workspace parameter for Lake project files.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI (e.g. file:///home/user/project/MyFile.lean)"
                            },
                            "text": {
                                "type": "string",
                                "description": "The full text content of the file"
                            },
                            "lake_workspace": {
                                "type": "string",
                                "description": "Optional. Absolute path to the Lake project root directory. If omitted, auto-detected from the URI's file path."
                            }
                        },
                        "required": ["uri", "text"]
                    }
                },
                {
                    "name": "get_diagnostics",
                    "description": "Get all compilation diagnostics (errors, warnings, info) for an open Lean 4 document. Waits for Lean to finish checking the file (up to timeout_ms). Returns formatted diagnostics with severity, location, and message. If no diagnostics are found, the file compiled successfully. Call this after open_file or after editing to check for errors.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI (e.g. file:///path/to/file.lean)"
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Max milliseconds to wait for Lean to finish checking (default: 60000). For files importing Mathlib, use 180000 or more."
                            }
                        },
                        "required": ["uri"]
                    }
                },
                {
                    "name": "get_goal_state",
                    "description": "Query the Lean 4 proof state (tactic goals) at a cursor position. Returns hypotheses and target type at that position. Place the cursor inside a `by` tactic block (e.g. on a `sorry` or tactic name) to see what needs to be proved. Line and character are 0-indexed.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI"
                            },
                            "line": {
                                "type": "integer",
                                "description": "Cursor line (0-indexed)"
                            },
                            "character": {
                                "type": "integer",
                                "description": "Cursor character (0-indexed)"
                            }
                        },
                        "required": ["uri", "line", "character"]
                    }
                },
                {
                    "name": "replace_document",
                    "description": "Replace the entire content of an already-open Lean 4 document with new text. The file on disk is also updated. Use this to rewrite a proof or change the file content. After replacing, call get_diagnostics to check the result.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI"
                            },
                            "newText": {
                                "type": "string",
                                "description": "The new full text content of the file"
                            }
                        },
                        "required": ["uri", "newText"]
                    }
                },
                {
                    "name": "apply_edit",
                    "description": "Apply a targeted text edit to an already-open Lean 4 document. Specify a range (start/end line and character, 0-indexed) and the replacement text. The file on disk is also updated. Useful for replacing a single tactic or definition without rewriting the whole file.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI"
                            },
                            "startLine": {
                                "type": "integer",
                                "description": "Start line (0-indexed)"
                            },
                            "startCharacter": {
                                "type": "integer",
                                "description": "Start character (0-indexed)"
                            },
                            "endLine": {
                                "type": "integer",
                                "description": "End line (0-indexed)"
                            },
                            "endCharacter": {
                                "type": "integer",
                                "description": "End character (0-indexed)"
                            },
                            "newText": {
                                "type": "string",
                                "description": "The text to replace the range with"
                            }
                        },
                        "required": ["uri", "startLine", "startCharacter", "endLine", "endCharacter", "newText"]
                    }
                },
                {
                    "name": "get_document_text",
                    "description": "Get the current text content of an open Lean 4 document as tracked by the language server. Useful to see the current state after edits.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI"
                            }
                        },
                        "required": ["uri"]
                    }
                },
                {
                    "name": "close_document",
                    "description": "Close a previously opened Lean 4 document. Frees resources in both the MCP server and Lean language server. Call when done working with a file.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI of the document to close"
                            }
                        },
                        "required": ["uri"]
                    }
                },
                {
                    "name": "file_status",
                    "description": "Check whether Lean is still processing an open document or has finished checking it. Returns 'checked' when safe to query diagnostics/goals, or 'processing' if Lean is still working.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "uri": {
                                "type": "string",
                                "description": "The file URI"
                            }
                        },
                        "required": ["uri"]
                    }
                },
                {
                    "name": "list_documents",
                    "description": "List all currently open Lean 4 documents across all workspaces, with their processing status and which workspace they belong to.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                }
            ]
        });

        Ok(JsonRpcResponse::success(request_id, tools))
    }

    /// Handle MCP `tools/call` request.
    async fn handle_tools_call(
        &self,
        request_id: Value,
        params: Option<Value>,
    ) -> Result<JsonRpcResponse> {
        let params = params.context("Missing params in tools/call")?;
        let tool_name = params
            .get("name")
            .and_then(|n| n.as_str())
            .context("Missing tool name")?;
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        info!("Tool call: {tool_name}");

        let result = match tool_name {
            "open_file" => self.tool_open_file(&arguments).await,
            "open_document" => self.tool_open_document(&arguments).await,
            "apply_edit" => self.tool_apply_edit(&arguments).await,
            "replace_document" => self.tool_replace_document(&arguments).await,
            "get_goal_state" => self.tool_get_goal_state(&arguments).await,
            "get_diagnostics" => self.tool_get_diagnostics(&arguments).await,
            "get_document_text" => self.tool_get_document_text(&arguments).await,
            "close_document" => self.tool_close_document(&arguments).await,
            "file_status" => self.tool_file_status(&arguments).await,
            "list_documents" => self.tool_list_documents().await,
            other => Err(anyhow::anyhow!("Unknown tool: {other}")),
        };

        match result {
            Ok(content) => Ok(JsonRpcResponse::success(
                request_id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": content
                    }]
                }),
            )),
            Err(error) => Ok(JsonRpcResponse::success(
                request_id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Error: {error}")
                    }],
                    "isError": true
                }),
            )),
        }
    }

    // Tool implementations

    async fn tool_open_file(&self, arguments: &Value) -> Result<String> {
        let path = arguments
            .get("path")
            .and_then(|p| p.as_str())
            .context("Missing 'path' parameter")?;
        let explicit_workspace = arguments
            .get("lake_workspace")
            .and_then(|w| w.as_str());

        // Resolve to absolute path
        let abs_path = if path.starts_with('/') {
            path.to_string()
        } else {
            std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string())
        };

        // Read the file content from disk
        let text = tokio::fs::read_to_string(&abs_path)
            .await
            .context(format!("Failed to read file: {abs_path}"))?;

        let uri = crate::lean_client::file_path_to_uri(&abs_path);

        // Resolve which workspace this file belongs to
        let workspace_key = self
            .manager
            .resolve_workspace(Some(&abs_path), explicit_workspace);

        let workspace_label = if workspace_key.is_empty() {
            "standalone".to_string()
        } else {
            workspace_key.clone()
        };

        // Get or spawn the appropriate Lean client
        let client = self.manager.get_or_spawn(&workspace_key).await?;

        // If already open in this client, replace content
        if client.is_document_open(&uri).await {
            client.replace_document(&uri, &text).await?;
            return Ok(format!(
                "Document {uri} was already open; content refreshed from disk. (workspace: {workspace_label})"
            ));
        }

        client.open_document(&uri, &text).await?;
        self.manager.register_uri(&uri, &workspace_key).await;
        Ok(format!(
            "Document {uri} opened successfully ({} bytes). (workspace: {workspace_label})",
            text.len()
        ))
    }

    async fn tool_open_document(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;
        let text = arguments
            .get("text")
            .and_then(|t| t.as_str())
            .context("Missing 'text' parameter")?;
        let explicit_workspace = arguments
            .get("lake_workspace")
            .and_then(|w| w.as_str());

        // Try to extract a file path from the URI for workspace detection
        let file_path = crate::lean_client::uri_to_file_path(uri);
        let workspace_key = self
            .manager
            .resolve_workspace(file_path.as_deref(), explicit_workspace);

        let workspace_label = if workspace_key.is_empty() {
            "standalone".to_string()
        } else {
            workspace_key.clone()
        };

        let client = self.manager.get_or_spawn(&workspace_key).await?;

        // If already open, replace content
        if client.is_document_open(uri).await {
            client.replace_document(uri, text).await?;
            return Ok(format!(
                "Document {uri} was already open; content replaced. (workspace: {workspace_label})"
            ));
        }

        client.open_document(uri, text).await?;
        self.manager.register_uri(uri, &workspace_key).await;
        Ok(format!(
            "Document {uri} opened successfully. (workspace: {workspace_label})"
        ))
    }

    async fn tool_apply_edit(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;
        let start_line = arguments
            .get("startLine")
            .and_then(|v| v.as_u64())
            .context("Missing 'startLine' parameter")?;
        let start_character = arguments
            .get("startCharacter")
            .and_then(|v| v.as_u64())
            .context("Missing 'startCharacter' parameter")?;
        let end_line = arguments
            .get("endLine")
            .and_then(|v| v.as_u64())
            .context("Missing 'endLine' parameter")?;
        let end_character = arguments
            .get("endCharacter")
            .and_then(|v| v.as_u64())
            .context("Missing 'endCharacter' parameter")?;
        let new_text = arguments
            .get("newText")
            .and_then(|t| t.as_str())
            .context("Missing 'newText' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;
        let resulting_text = client
            .apply_edit(uri, start_line, start_character, end_line, end_character, new_text)
            .await?;

        let line_count = resulting_text.lines().count();
        Ok(format!(
            "Edit applied to {uri}. Document now has {line_count} lines."
        ))
    }

    async fn tool_replace_document(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;
        let new_text = arguments
            .get("newText")
            .and_then(|t| t.as_str())
            .context("Missing 'newText' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;
        let resulting_text = client.replace_document(uri, new_text).await?;
        let line_count = resulting_text.lines().count();
        Ok(format!(
            "Document {uri} replaced. Document now has {line_count} lines."
        ))
    }

    async fn tool_get_goal_state(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;
        let line = arguments
            .get("line")
            .and_then(|v| v.as_u64())
            .context("Missing 'line' parameter")?;
        let character = arguments
            .get("character")
            .and_then(|v| v.as_u64())
            .context("Missing 'character' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;
        client.get_goal_state(uri, line, character).await
    }

    async fn tool_get_diagnostics(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;
        let timeout_ms = arguments
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(60000);

        let client = self.manager.client_for_uri(uri).await?;
        let diagnostics = client.wait_for_diagnostics(uri, timeout_ms).await;

        if diagnostics.is_empty() {
            return Ok("No diagnostics (errors/warnings) found.".to_string());
        }

        let formatted: Vec<String> = diagnostics
            .iter()
            .map(|diag| {
                let severity = match diag.severity {
                    Some(1) => "error",
                    Some(2) => "warning",
                    Some(3) => "info",
                    Some(4) => "hint",
                    _ => "unknown",
                };
                format!(
                    "[{severity}] {}:{}-{}:{}: {}",
                    diag.range.start.line,
                    diag.range.start.character,
                    diag.range.end.line,
                    diag.range.end.character,
                    diag.message
                )
            })
            .collect();

        Ok(formatted.join("\n"))
    }

    async fn tool_get_document_text(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;
        let text = client
            .get_document_text(uri)
            .await
            .context(format!("Document not open: {uri}"))?;
        Ok(text)
    }

    async fn tool_close_document(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;
        client.close_document(uri).await?;
        self.manager.unregister_uri(uri).await;
        Ok(format!("Document {uri} closed."))
    }

    async fn tool_file_status(&self, arguments: &Value) -> Result<String> {
        let uri = arguments
            .get("uri")
            .and_then(|u| u.as_str())
            .context("Missing 'uri' parameter")?;

        let client = self.manager.client_for_uri(uri).await?;

        if !client.is_document_open(uri).await {
            return Err(anyhow::anyhow!("Document not open: {uri}"));
        }

        let status = client.get_file_progress(uri).await;
        match status {
            Some(crate::lean_client::FileCheckStatus::Checked) => {
                Ok("checked — Lean has finished processing this file.".to_string())
            }
            Some(crate::lean_client::FileCheckStatus::Processing) => {
                Ok("processing — Lean is still checking this file.".to_string())
            }
            None => {
                Ok("unknown — no file progress information available yet.".to_string())
            }
        }
    }

    async fn tool_list_documents(&self) -> Result<String> {
        let docs = self.manager.list_all_documents().await;
        if docs.is_empty() {
            return Ok("No documents are currently open.".to_string());
        }

        let mut lines = Vec::new();
        for (uri, workspace_key) in &docs {
            let status_str = if let Ok(client) = self.manager.get_or_spawn(workspace_key).await {
                match client.get_file_progress(uri).await {
                    Some(crate::lean_client::FileCheckStatus::Checked) => "checked",
                    Some(crate::lean_client::FileCheckStatus::Processing) => "processing",
                    None => "unknown",
                }
            } else {
                "error"
            };

            let ws_label = if workspace_key.is_empty() {
                "standalone"
            } else {
                workspace_key.as_str()
            };

            lines.push(format!("  [{status_str}] {uri} (workspace: {ws_label})"));
        }

        Ok(format!(
            "Open documents ({}):\n{}",
            docs.len(),
            lines.join("\n")
        ))
    }
}
