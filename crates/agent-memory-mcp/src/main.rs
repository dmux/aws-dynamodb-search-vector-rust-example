//! Local MCP server exposing the semantic memory layer over stdio.
//!
//! # stdout is not yours
//!
//! stdout carries the MCP JSON-RPC stream. A stray `println!` corrupts the
//! protocol and the failure looks nothing like its cause, so every diagnostic
//! here goes to stderr — including the tracing subscriber, which is configured
//! for stderr explicitly rather than by default.

mod github;
mod tools;

use std::sync::Arc;

use agent_memory_client::RemoteMemoryService;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;
use tracing_subscriber::EnvFilter;

use crate::tools::MemoryTools;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        // Not a preference: stdout belongs to JSON-RPC.
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Attribution only, and entirely optional: no `gh`, or `gh` logged out,
    // still yields a working server. The GitHub token is never read or sent —
    // see `github` for why.
    let github_login = github::detect_login().await;
    let github_repo = github::detect_repo().await;
    if let Some(login) = &github_login {
        tracing::info!(github_login = %login, github_repo = ?github_repo, "attributing memories to GitHub user");
    }

    let service = RemoteMemoryService::from_env(github_login, github_repo).await?;
    tracing::info!("agent-memory MCP server ready on stdio");

    let server = MemoryTools::new(Arc::new(service)).serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}
