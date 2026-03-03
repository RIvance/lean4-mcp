mod jsonrpc;
mod lean_client;
mod mcp_server;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::mcp_server::McpServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging to stderr (MUST NOT go to stdout, which is the MCP JSON channel)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("lean4-mcp-proxy starting...");

    // Determine the default lake project root:
    //   1. --lake-project <path> CLI argument
    //   2. LAKE_PROJECT environment variable
    //   3. None (standalone mode, or agent specifies per-file)
    let args: Vec<String> = std::env::args().collect();
    let lake_root = if let Some(pos) = args.iter().position(|a| a == "--lake-project") {
        args.get(pos + 1).cloned()
    } else {
        std::env::var("LAKE_PROJECT").ok()
    };

    // Canonicalize the lake project path to an absolute path
    let lake_root = lake_root.map(|p| {
        std::fs::canonicalize(&p)
            .unwrap_or_else(|_| std::path::PathBuf::from(&p))
            .to_string_lossy()
            .to_string()
    });

    if let Some(ref root) = lake_root {
        info!("Default lake project: {root}");
    } else {
        info!("No default lake project specified (will auto-detect or use standalone mode)");
    }

    // Create and run the MCP server.
    // Lean server instances are spawned lazily when files are opened.
    let mcp_server = McpServer::new(lake_root);
    mcp_server.run().await?;

    info!("lean4-mcp-proxy shutting down");
    Ok(())
}
