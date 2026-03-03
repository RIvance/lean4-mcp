use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tracing::{debug, error, info, warn};

use crate::jsonrpc::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

/// Encode a JSON-RPC message with LSP Content-Length header framing.
fn encode_lsp_message(body: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut message = header.into_bytes();
    message.extend_from_slice(body);
    message
}

/// Diagnostic types
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspDiagnostic {
    pub range: LspRange,
    pub severity: Option<i64>,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LspPosition {
    pub line: u64,
    pub character: u64,
}

/// Tracks whether Lean has finished processing a file.
/// Lean sends `$/lean/fileProgress` notifications with `processing: [...]`.
/// When the processing array is empty, the file is fully checked.
#[derive(Debug, Clone, PartialEq)]
pub enum FileCheckStatus {
    /// Lean is still processing the file.
    Processing,
    /// Lean has finished checking (processing array was empty).
    Checked,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DocumentState {
    pub uri: String,
    pub version: i64,
    pub text: String,
    pub rpc_session_id: Option<String>,
}

/// Manages communication with the `lean --server` child process.
pub struct LeanClient {
    /// Channel to send raw bytes to Lean's stdin.
    stdin_sender: mpsc::Sender<Vec<u8>>,
    /// Pending requests awaiting responses, keyed by JSON-RPC id.
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>,
    /// Cached diagnostics per URI.
    diagnostics_cache: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
    /// File check status per URI (tracks $/lean/fileProgress).
    file_progress: Arc<Mutex<HashMap<String, FileCheckStatus>>>,
    /// Notifier that fires whenever file progress changes.
    file_progress_notify: Arc<Notify>,
    /// Notifier that fires whenever diagnostics are updated.
    diagnostics_notify: Arc<Notify>,
    /// Document states per URI.
    documents: Arc<Mutex<HashMap<String, DocumentState>>>,
    /// Monotonically increasing request id counter.
    next_request_id: Arc<Mutex<i64>>,
    /// Handle to the child process.
    _child: Arc<Mutex<Child>>,
    /// The lake project root directory, if any.
    lake_root: Option<String>,
}

impl LeanClient {
    /// Spawn the Lean 4 language server and wire up the communication channels.
    ///
    /// If `lake_root` is provided, we use `lake serve` (which sets up `LEAN_PATH`
    /// and all lake environment variables automatically).
    /// Otherwise, we use `lean --server` directly.
    ///
    /// The binary is resolved in this order:
    /// 1. `LEAN_BIN` environment variable (overrides everything)
    /// 2. `lake serve` (if `lake_root` is set) — this is the recommended way
    ///    for lake projects as it sets LEAN_PATH, picks the right toolchain, etc.
    /// 3. `lean --server` on PATH (standalone mode)
    pub async fn spawn(lake_root: Option<String>) -> Result<Self> {
        if let Some(ref root) = lake_root {
            info!("Lake project root: {root}");
        }

        let use_lake_serve = lake_root.is_some()
            && std::env::var("LEAN_BIN").is_err();

        let mut cmd = if use_lake_serve {
            // Use `lake serve` which automatically sets LEAN_PATH, finds the
            // correct toolchain, and starts `lean --server` with project settings.
            let lake_bin = std::env::var("LAKE_BIN").unwrap_or_else(|_| "lake".to_string());
            info!("Using `{lake_bin} serve` for lake project");
            let mut c = Command::new(&lake_bin);
            c.arg("serve");
            c
        } else {
            // Standalone mode: use lean --server directly
            let lean_bin = std::env::var("LEAN_BIN").unwrap_or_else(|_| "lean".to_string());
            info!("Using `{lean_bin} --server`");
            let mut c = Command::new(&lean_bin);
            c.arg("--server");
            c
        };

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // If we have a lake project, set the working directory so that
        // lake/lean can discover the lakefile, toolchain, and built packages.
        if let Some(ref root) = lake_root {
            cmd.current_dir(root);
        }

        let spawn_desc = format!("{:?}", cmd.as_std());
        let mut child = cmd.spawn().context(format!(
            "Failed to spawn: {spawn_desc}. Is Lean 4 / Lake installed and on PATH?"
        ))?;

        let child_stdin = child.stdin.take().expect("Failed to capture lean stdin");
        let child_stdout = child.stdout.take().expect("Failed to capture lean stdout");
        let child_stderr = child.stderr.take().expect("Failed to capture lean stderr");

        // Channel for sending messages to Lean's stdin
        let (stdin_sender, mut stdin_receiver) = mpsc::channel::<Vec<u8>>(256);

        // Spawn a task that writes to Lean's stdin
        let mut child_stdin = child_stdin;
        tokio::spawn(async move {
            while let Some(data) = stdin_receiver.recv().await {
                if let Err(error) = child_stdin.write_all(&data).await {
                    error!("Failed to write to lean stdin: {error}");
                    break;
                }
                if let Err(error) = child_stdin.flush().await {
                    error!("Failed to flush lean stdin: {error}");
                    break;
                }
            }
            debug!("Lean stdin writer task exiting");
        });

        // Forward Lean's stderr to our stderr for debugging
        tokio::spawn(async move {
            let reader = BufReader::new(child_stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!("[lean stderr] {line}");
            }
        });

        let pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics_cache: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let file_progress: Arc<Mutex<HashMap<String, FileCheckStatus>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let file_progress_notify = Arc::new(Notify::new());
        let diagnostics_notify = Arc::new(Notify::new());

        // Spawn a task that reads from Lean's stdout (LSP framed messages)
        let pending_for_reader = pending_requests.clone();
        let diagnostics_for_reader = diagnostics_cache.clone();
        let file_progress_for_reader = file_progress.clone();
        let file_progress_notify_for_reader = file_progress_notify.clone();
        let diagnostics_notify_for_reader = diagnostics_notify.clone();
        let stdin_for_reader = stdin_sender.clone();
        tokio::spawn(async move {
            if let Err(error) =
                Self::read_lean_stdout(
                    child_stdout,
                    pending_for_reader,
                    diagnostics_for_reader,
                    file_progress_for_reader,
                    file_progress_notify_for_reader,
                    diagnostics_notify_for_reader,
                    stdin_for_reader,
                )
                    .await
            {
                error!("Lean stdout reader failed: {error}");
            }
            debug!("Lean stdout reader task exiting");
        });

        let client = LeanClient {
            stdin_sender,
            pending_requests,
            diagnostics_cache,
            file_progress,
            file_progress_notify,
            diagnostics_notify,
            documents: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: Arc::new(Mutex::new(1)),
            _child: Arc::new(Mutex::new(child)),
            lake_root: lake_root.clone(),
        };

        // Perform LSP initialization handshake
        client.initialize().await?;

        Ok(client)
    }

    /// Read LSP-framed messages from Lean's stdout, dispatching responses and
    /// caching diagnostic notifications.
    async fn read_lean_stdout(
        stdout: tokio::process::ChildStdout,
        pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>,
        diagnostics_cache: Arc<Mutex<HashMap<String, Vec<LspDiagnostic>>>>,
        file_progress: Arc<Mutex<HashMap<String, FileCheckStatus>>>,
        file_progress_notify: Arc<Notify>,
        diagnostics_notify: Arc<Notify>,
        stdin_sender: mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        let mut reader = BufReader::new(stdout);

        loop {
            // Read headers until we find Content-Length
            let mut content_length: Option<usize> = None;
            loop {
                let mut header_line = String::new();
                let bytes_read = reader.read_line(&mut header_line).await?;
                if bytes_read == 0 {
                    info!("Lean stdout closed (EOF)");
                    return Ok(());
                }
                let trimmed = header_line.trim();
                if trimmed.is_empty() {
                    // End of headers
                    break;
                }
                if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                    content_length = Some(value.trim().parse::<usize>()?);
                }
                // Ignore other headers (e.g. Content-Type)
            }

            let content_length = match content_length {
                Some(length) => length,
                None => {
                    warn!("No Content-Length header found in Lean message");
                    continue;
                }
            };

            // Read the body
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await?;
            let body_str = String::from_utf8_lossy(&body);
            debug!("← lean: {body_str}");

            let parsed: Value = match serde_json::from_str(&body_str) {
                Ok(value) => value,
                Err(error) => {
                    warn!("Failed to parse Lean message as JSON: {error}");
                    continue;
                }
            };

            // Determine if this is a response (has id + result/error) or notification/request (has method)
            let has_id = parsed.get("id").is_some()
                && parsed.get("id") != Some(&Value::Null);
            let has_result_or_error =
                parsed.get("result").is_some() || parsed.get("error").is_some();
            let has_method = parsed.get("method").is_some();

            if has_id && has_result_or_error {
                // This is a response to one of our requests
                let response: JsonRpcResponse = match serde_json::from_value(parsed) {
                    Ok(response) => response,
                    Err(error) => {
                        warn!("Failed to deserialize response: {error}");
                        continue;
                    }
                };
                let id_string = match &response.id {
                    Value::Number(number) => number.to_string(),
                    Value::String(string) => string.clone(),
                    other => other.to_string(),
                };
                let mut pending = pending_requests.lock().await;
                if let Some(sender) = pending.remove(&id_string) {
                    let _ = sender.send(response);
                } else {
                    debug!("Received response for unknown id: {id_string}");
                }
            } else if has_id && has_method {
                // This is a server-initiated request from Lean (e.g., client/registerCapability)
                // We must send a response back to acknowledge it
                let request_id = parsed.get("id").cloned().unwrap_or(Value::Null);
                let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
                debug!("Lean server request: {method} (id={request_id})");

                // Send an empty success response for all server-initiated requests
                let response = JsonRpcResponse::success(request_id, Value::Null);
                let response_body = serde_json::to_string(&response)
                    .unwrap_or_default();
                let framed = encode_lsp_message(response_body.as_bytes());
                if let Err(error) = stdin_sender.send(framed).await {
                    warn!("Failed to respond to Lean server request: {error}");
                }
            } else if let Some(method) = parsed.get("method").and_then(|m| m.as_str()) {
                // This is a notification (no id)
                match method {
                    "textDocument/publishDiagnostics" => {
                        if let Some(params) = parsed.get("params") {
                            let uri = params
                                .get("uri")
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string();
                            let diags: Vec<LspDiagnostic> = params
                                .get("diagnostics")
                                .and_then(|d| serde_json::from_value(d.clone()).ok())
                                .unwrap_or_default();
                            debug!("Caching {} diagnostics for {uri}", diags.len());
                            let mut cache = diagnostics_cache.lock().await;
                            cache.insert(uri, diags);
                            diagnostics_notify.notify_waiters();
                        }
                    }
                    "$/lean/fileProgress" => {
                        if let Some(params) = parsed.get("params") {
                            let uri = params
                                .get("textDocument")
                                .and_then(|td| td.get("uri"))
                                .and_then(|u| u.as_str())
                                .unwrap_or("")
                                .to_string();
                            let processing = params
                                .get("processing")
                                .and_then(|p| p.as_array())
                                .map(|arr| arr.len())
                                .unwrap_or(0);

                            let status = if processing == 0 {
                                FileCheckStatus::Checked
                            } else {
                                FileCheckStatus::Processing
                            };

                            debug!(
                                "File progress for {uri}: {status:?} (processing regions: {processing})"
                            );

                            let mut fp = file_progress.lock().await;
                            fp.insert(uri, status);
                            file_progress_notify.notify_waiters();
                        }
                    }
                    other => {
                        debug!("Unhandled notification from Lean: {other}");
                    }
                }
            } else {
                debug!("Unclassified message from Lean: {body_str}");
            }
        }
    }

    /// Allocate the next request ID.
    async fn allocate_request_id(&self) -> i64 {
        let mut counter = self.next_request_id.lock().await;
        let id = *counter;
        *counter += 1;
        id
    }

    /// Send a JSON-RPC request to Lean and await the response.
    /// Uses 120s timeout by default (Mathlib files can be slow).
    pub async fn send_request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        self.send_request_with_timeout(method, params, 120).await
    }

    /// Send a request with an explicit timeout in seconds.
    pub async fn send_request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_secs: u64,
    ) -> Result<JsonRpcResponse> {
        let request_id = self.allocate_request_id().await;
        let request = JsonRpcRequest::new(request_id, method, params);

        let body = serde_json::to_string(&request)?;
        debug!("→ lean: {body}");
        let framed = encode_lsp_message(body.as_bytes());

        let id_string = request_id.to_string();
        let (response_sender, response_receiver) = oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id_string.clone(), response_sender);
        }

        self.stdin_sender
            .send(framed)
            .await
            .context("Failed to send to lean stdin channel")?;

        match tokio::time::timeout(
            tokio::time::Duration::from_secs(timeout_secs),
            response_receiver,
        )
        .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                anyhow::bail!("Response channel closed before receiving response for {method}")
            }
            Err(_) => {
                // Timeout - clean up the pending entry
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id_string);
                anyhow::bail!(
                    "Timed out waiting for Lean response to {method} ({timeout_secs}s)"
                )
            }
        }
    }

    /// Send a JSON-RPC notification to Lean (no response expected).
    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let body = serde_json::to_string(&notification)?;
        debug!("→ lean (notification): {body}");
        let framed = encode_lsp_message(body.as_bytes());
        self.stdin_sender
            .send(framed)
            .await
            .context("Failed to send notification to lean stdin channel")?;
        Ok(())
    }

    /// Perform the LSP initialization handshake with Lean.
    async fn initialize(&self) -> Result<()> {
        info!("Initializing LSP connection with Lean...");

        // If we have a lake project root, send it as rootUri so the Lean server
        // can discover the lake-manifest, lakefile, and built packages.
        let root_uri: Value = match &self.lake_root {
            Some(root) => {
                let uri = file_path_to_uri(root);
                info!("LSP rootUri: {uri}");
                Value::String(uri)
            }
            None => Value::Null,
        };

        let init_params = json!({
            "processId": std::process::id(),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "relatedInformation": true
                    }
                }
            },
            "rootUri": root_uri,
            "initializationOptions": {}
        });

        let response = self
            .send_request_with_timeout("initialize", Some(init_params), 60)
            .await?;

        if let Some(error) = &response.error {
            anyhow::bail!("Lean initialization failed: {}", error.message);
        }
        info!("Lean initialization response received");

        // Send 'initialized' notification
        self.send_notification("initialized", Some(json!({})))
            .await?;
        info!("LSP initialization handshake complete");

        Ok(())
    }

    /// Open a document in Lean. Sends `textDocument/didOpen`.
    pub async fn open_document(&self, uri: &str, text: &str) -> Result<()> {
        let language_id = "lean4";
        let version = 1i64;

        // Mark file as processing before we send the notification
        {
            let mut fp = self.file_progress.lock().await;
            fp.insert(uri.to_string(), FileCheckStatus::Processing);
        }

        // Clear any stale diagnostics for this URI
        {
            let mut cache = self.diagnostics_cache.lock().await;
            cache.remove(uri);
        }

        self.send_notification(
            "textDocument/didOpen",
            Some(json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": text
                }
            })),
        )
        .await?;

        let document_state = DocumentState {
            uri: uri.to_string(),
            version,
            text: text.to_string(),
            rpc_session_id: None,
        };

        {
            let mut documents = self.documents.lock().await;
            documents.insert(uri.to_string(), document_state);
        }

        // Establish an RPC session for this document
        self.ensure_rpc_session(uri).await?;

        info!("Document opened: {uri}");
        Ok(())
    }

    /// Close a document in Lean. Sends `textDocument/didClose`.
    pub async fn close_document(&self, uri: &str) -> Result<()> {
        // Check if document is actually open
        {
            let documents = self.documents.lock().await;
            if !documents.contains_key(uri) {
                anyhow::bail!("Document not open: {uri}");
            }
        }

        self.send_notification(
            "textDocument/didClose",
            Some(json!({
                "textDocument": {
                    "uri": uri
                }
            })),
        )
        .await?;

        // Clean up local state
        {
            let mut documents = self.documents.lock().await;
            documents.remove(uri);
        }
        {
            let mut cache = self.diagnostics_cache.lock().await;
            cache.remove(uri);
        }
        {
            let mut fp = self.file_progress.lock().await;
            fp.remove(uri);
        }

        info!("Document closed: {uri}");
        Ok(())
    }

    /// Ensure an RPC session is established for the given URI.
    async fn ensure_rpc_session(&self, uri: &str) -> Result<String> {
        {
            let documents = self.documents.lock().await;
            if let Some(doc) = documents.get(uri) {
                if let Some(ref session_id) = doc.rpc_session_id {
                    return Ok(session_id.clone());
                }
            }
        }

        info!("Establishing RPC session for {uri}...");
        let response = self
            .send_request(
                "$/lean/rpc/connect",
                Some(json!({
                    "uri": uri
                })),
            )
            .await?;

        if let Some(error) = &response.error {
            anyhow::bail!("RPC connect failed for {uri}: {}", error.message);
        }

        let session_id = response
            .result
            .as_ref()
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .context("No sessionId in RPC connect response")?
            .to_string();

        {
            let mut documents = self.documents.lock().await;
            if let Some(doc) = documents.get_mut(uri) {
                doc.rpc_session_id = Some(session_id.clone());
            }
        }

        info!("RPC session established for {uri}: {session_id}");
        Ok(session_id)
    }

    /// Apply an edit to a document. Sends `textDocument/didChange` and updates
    /// the physical file on disk.
    pub async fn apply_edit(
        &self,
        uri: &str,
        start_line: u64,
        start_character: u64,
        end_line: u64,
        end_character: u64,
        new_text: &str,
    ) -> Result<String> {
        let new_version;
        let resulting_text;

        {
            let mut documents = self.documents.lock().await;
            let document = documents
                .get_mut(uri)
                .context(format!("Document not open: {uri}"))?;

            // Apply the edit to our in-memory text
            let old_text = &document.text;
            let lines: Vec<&str> = old_text.split('\n').collect();

            // Validate the range
            if start_line as usize > lines.len() || end_line as usize > lines.len() {
                anyhow::bail!(
                    "Edit range out of bounds: lines {start_line}-{end_line}, document has {} lines",
                    lines.len()
                );
            }

            // Convert line/character positions to byte offsets
            let start_offset = Self::position_to_offset(&lines, start_line, start_character);
            let end_offset = Self::position_to_offset(&lines, end_line, end_character);

            if start_offset > end_offset {
                anyhow::bail!(
                    "Invalid edit range: start offset ({start_offset}) > end offset ({end_offset})"
                );
            }

            let mut new_full_text = String::new();
            new_full_text.push_str(&old_text[..start_offset]);
            new_full_text.push_str(new_text);
            new_full_text.push_str(&old_text[end_offset..]);

            document.version += 1;
            document.text = new_full_text.clone();
            new_version = document.version;
            resulting_text = new_full_text;
        }

        // Mark file as processing after edit
        {
            let mut fp = self.file_progress.lock().await;
            fp.insert(uri.to_string(), FileCheckStatus::Processing);
        }

        // Clear stale diagnostics
        {
            let mut cache = self.diagnostics_cache.lock().await;
            cache.remove(uri);
        }

        // Send didChange to Lean
        self.send_notification(
            "textDocument/didChange",
            Some(json!({
                "textDocument": {
                    "uri": uri,
                    "version": new_version
                },
                "contentChanges": [{
                    "range": {
                        "start": { "line": start_line, "character": start_character },
                        "end": { "line": end_line, "character": end_character }
                    },
                    "text": new_text
                }]
            })),
        )
        .await?;

        // Update the physical file on disk
        if let Some(file_path) = uri_to_file_path(uri) {
            if let Err(error) = tokio::fs::write(&file_path, &resulting_text).await {
                warn!("Failed to write file to disk at {file_path}: {error}");
            }
        }

        info!("Edit applied to {uri}, version now {new_version}");
        Ok(resulting_text)
    }

    /// Replace the entire document text. This is a convenience wrapper over
    /// `apply_edit` that replaces everything.
    pub async fn replace_document(&self, uri: &str, new_text: &str) -> Result<String> {
        let (end_line, end_character) = {
            let documents = self.documents.lock().await;
            let document = documents
                .get(uri)
                .context(format!("Document not open: {uri}"))?;
            let lines: Vec<&str> = document.text.split('\n').collect();
            let last_line = if lines.is_empty() { 0 } else { lines.len() - 1 };
            let last_char = lines.last().map(|l| l.len()).unwrap_or(0);
            (last_line as u64, last_char as u64)
        };

        self.apply_edit(uri, 0, 0, end_line, end_character, new_text)
            .await
    }

    /// Get the proof goal state at a specific position.
    pub async fn get_goal_state(
        &self,
        uri: &str,
        line: u64,
        character: u64,
    ) -> Result<String> {
        // Ensure RPC session exists
        let _session_id = self.ensure_rpc_session(uri).await?;

        // Use $/lean/plainGoal which returns goals in plain text format
        let response = self
            .send_request(
                "$/lean/plainGoal",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                })),
            )
            .await?;

        if let Some(error) = &response.error {
            // Common: no goal at this position
            return Ok(format!("No goal at this position: {}", error.message));
        }

        match response.result {
            Some(result) => {
                if result.is_null() {
                    return Ok("No goal at this position.".to_string());
                }
                // The result has "goals" array and possibly "rendered" field
                if let Some(rendered) = result.get("rendered").and_then(|r| r.as_str()) {
                    return Ok(rendered.to_string());
                }
                if let Some(goals) = result.get("goals").and_then(|g| g.as_array()) {
                    if goals.is_empty() {
                        return Ok("No goals.".to_string());
                    }
                    let formatted: Vec<String> = goals
                        .iter()
                        .enumerate()
                        .map(|(index, goal)| {
                            let goal_string = goal.to_string();
                            let goal_str = goal.as_str().unwrap_or(&goal_string);
                            format!("Goal {}:\n{}", index + 1, goal_str)
                        })
                        .collect();
                    return Ok(formatted.join("\n\n"));
                }
                // Fallback: return the raw JSON
                Ok(serde_json::to_string_pretty(&result)?)
            }
            None => Ok("No result from Lean.".to_string()),
        }
    }

    /// Get cached diagnostics for a document.
    pub async fn get_diagnostics(&self, uri: &str) -> Vec<LspDiagnostic> {
        let cache = self.diagnostics_cache.lock().await;
        cache.get(uri).cloned().unwrap_or_default()
    }

    /// Get the file checking status for a document.
    pub async fn get_file_progress(&self, uri: &str) -> Option<FileCheckStatus> {
        let fp = self.file_progress.lock().await;
        fp.get(uri).cloned()
    }

    /// Wait until Lean has finished checking the file (based on $/lean/fileProgress).
    /// Falls back to polling diagnostics if progress info never arrives.
    /// Returns the diagnostics once the file is fully checked or timeout.
    pub async fn wait_for_diagnostics(&self, uri: &str, timeout_ms: u64) -> Vec<LspDiagnostic> {
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);

        // Give Lean a moment to start processing
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

        loop {
            // Check if file is fully checked via fileProgress
            let status = self.get_file_progress(uri).await;
            if status == Some(FileCheckStatus::Checked) {
                // File is fully checked — grab the latest diagnostics
                // Give a tiny delay for the final diagnostics to arrive
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                return self.get_diagnostics(uri).await;
            }

            // Check timeout
            if tokio::time::Instant::now() >= deadline {
                debug!("wait_for_diagnostics timed out after {timeout_ms}ms for {uri}");
                return self.get_diagnostics(uri).await;
            }

            // Wait for either a progress update or a diagnostics update
            let remaining = deadline - tokio::time::Instant::now();
            tokio::select! {
                _ = self.file_progress_notify.notified() => {
                    // Progress changed, loop and check again
                }
                _ = self.diagnostics_notify.notified() => {
                    // Diagnostics updated, loop and check if file is done
                }
                _ = tokio::time::sleep(remaining) => {
                    // Timeout
                    debug!("wait_for_diagnostics final timeout for {uri}");
                    return self.get_diagnostics(uri).await;
                }
            }
        }
    }

    /// Get the current document text.
    pub async fn get_document_text(&self, uri: &str) -> Option<String> {
        let documents = self.documents.lock().await;
        documents.get(uri).map(|d| d.text.clone())
    }

    /// Check if a document is open.
    pub async fn is_document_open(&self, uri: &str) -> bool {
        let documents = self.documents.lock().await;
        documents.contains_key(uri)
    }

    // pub async fn list_open_documents(&self) -> Vec<String> {
    //     let documents = self.documents.lock().await;
    //     documents.keys().cloned().collect()
    // }

    // pub fn get_lake_root(&self) -> Option<&str> {
    //     self.lake_root.as_deref()
    // }

    /// Convert a line/character position to a byte offset in the text.
    fn position_to_offset(lines: &[&str], line: u64, character: u64) -> usize {
        let mut offset = 0;
        for (index, line_text) in lines.iter().enumerate() {
            if index == line as usize {
                // Add character offset within this line, clamped to line length
                offset += (character as usize).min(line_text.len());
                return offset;
            }
            // +1 for the newline character that was removed by split
            offset += line_text.len() + 1;
        }
        offset
    }
}

/// Convert a file:// URI to a local file path.
pub fn uri_to_file_path(uri: &str) -> Option<String> {
    if let Some(path) = uri.strip_prefix("file://") {
        // Handle percent-encoded paths
        Some(percent_decode(path))
    } else {
        None
    }
}

/// Convert a local file path to a file:// URI.
pub fn file_path_to_uri(path: &str) -> String {
    // For absolute paths: file:// + /home/... = file:///home/...
    // For relative paths (shouldn't happen): file:///relative/...
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

/// Basic percent-decoding for URIs (handles spaces and common special chars).
fn percent_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let hex = [hi, lo];
            if let Ok(decoded) = u8::from_str_radix(
                std::str::from_utf8(&hex).unwrap_or("00"),
                16,
            ) {
                result.push(decoded as char);
            } else {
                result.push('%');
                result.push(hi as char);
                result.push(lo as char);
            }
        } else {
            result.push(byte as char);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_to_offset_first_line() {
        let text = "hello\nworld\n";
        let lines: Vec<&str> = text.split('\n').collect();
        // Line 0, char 0 -> offset 0
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 0), 0);
        // Line 0, char 3 -> offset 3
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 3), 3);
        // Line 0, char 5 -> offset 5 (end of "hello")
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 5), 5);
    }

    #[test]
    fn test_position_to_offset_second_line() {
        let text = "hello\nworld\n";
        let lines: Vec<&str> = text.split('\n').collect();
        // Line 1, char 0 -> offset 6 (after "hello\n")
        assert_eq!(LeanClient::position_to_offset(&lines, 1, 0), 6);
        // Line 1, char 3 -> offset 9
        assert_eq!(LeanClient::position_to_offset(&lines, 1, 3), 9);
        // Line 1, char 5 -> offset 11 (end of "world")
        assert_eq!(LeanClient::position_to_offset(&lines, 1, 5), 11);
    }

    #[test]
    fn test_position_to_offset_clamped_character() {
        let text = "hi\nworld\n";
        let lines: Vec<&str> = text.split('\n').collect();
        // Line 0, char 100 -> clamped to end of "hi" = offset 2
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 100), 2);
    }

    #[test]
    fn test_position_to_offset_empty_text() {
        let text = "";
        let lines: Vec<&str> = text.split('\n').collect();
        // Line 0, char 0 on empty string
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 0), 0);
    }

    #[test]
    fn test_position_to_offset_multiline() {
        let text = "def hello : String := \"world\"\n\n#check hello\n";
        let lines: Vec<&str> = text.split('\n').collect();
        // Line 0, char 0
        assert_eq!(LeanClient::position_to_offset(&lines, 0, 0), 0);
        // Line 1, char 0 (empty line after first)
        assert_eq!(LeanClient::position_to_offset(&lines, 1, 0), 30);
        // Line 2, char 0
        assert_eq!(LeanClient::position_to_offset(&lines, 2, 0), 31);
        // Line 2, char 6 -> "#check" ends at char 6
        assert_eq!(LeanClient::position_to_offset(&lines, 2, 6), 37);
    }

    #[test]
    fn test_uri_to_file_path() {
        assert_eq!(
            uri_to_file_path("file:///tmp/test.lean"),
            Some("/tmp/test.lean".to_string())
        );
        assert_eq!(uri_to_file_path("http://example.com"), None);
        assert_eq!(uri_to_file_path("not-a-uri"), None);
    }

    #[test]
    fn test_uri_to_file_path_percent_encoded() {
        assert_eq!(
            uri_to_file_path("file:///tmp/my%20file.lean"),
            Some("/tmp/my file.lean".to_string())
        );
    }

    #[test]
    fn test_encode_lsp_message() {
        let body = b"{\"test\": true}";
        let encoded = encode_lsp_message(body);
        let expected = format!("Content-Length: {}\r\n\r\n{{\"test\": true}}", body.len());
        assert_eq!(encoded, expected.as_bytes());
    }

    #[test]
    fn test_file_path_to_uri() {
        assert_eq!(
            file_path_to_uri("/tmp/test.lean"),
            "file:///tmp/test.lean"
        );
        // Relative path (shouldn't normally happen after canonicalization)
        assert_eq!(
            file_path_to_uri("relative/path.lean"),
            "file:///relative/path.lean"
        );
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("no%2Fslash"), "no/slash");
        assert_eq!(percent_decode("plain"), "plain");
    }
}
