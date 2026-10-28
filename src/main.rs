#![forbid(unsafe_code)]
//! Periscope entry point.
//!
//! Usage: `periscope [--workspace DIR] [--attach SOCK] [--nvim-bin PATH]`
//!
//! Speaks MCP on stdio. All diagnostics go to stderr; stdout carries
//! protocol traffic only. Configuration errors (a missing workspace)
//! fail fast with exit code 2 before the protocol starts.
//!
//! As of 0.2.0 (WS2 of the MCP standardization program, decision
//! D-113) this server runs on rmcp, the official Rust MCP SDK, and
//! speaks the modern protocol era only (2026-07-28): sessions open
//! with `server/discover`, and every request carries its protocol
//! version in `_meta` — there is no `initialize` handshake. Legacy
//! contact is refused at the stdio front door (see `guard`) with
//! the typed `-32022` unsupported-version error, naming 2026-07-28
//! as the one supported version. The broker, the tool surface, and
//! the attention board are unchanged.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use periscope::broker::BrokerConfig;
use periscope::guard;
use periscope::server::PeriscopeServer;
use rmcp::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::sync::Mutex;

/// Duplex buffer between the era guard and the SDK service. Request
/// lines can reach the 4 MiB line bound; the SDK consumes
/// incrementally, so this only needs to cover response bursts.
const DUPLEX_BUFFER_BYTES: usize = 1024 * 1024;

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("periscope: {message}");
            eprintln!("usage: periscope [--workspace DIR] [--attach SOCK] [--nvim-bin PATH]");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("periscope: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("periscope: serve loop ended: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Wire the pieces together: the guard owns stdio, the SDK service
/// sits behind an in-process duplex, and a pump copies its responses
/// back to stdout through the same lock the guard writes with. When
/// the serve ends (stdin EOF), the broker is shut down — the
/// baseline front end's exit contract.
async fn run(config: BrokerConfig) -> std::io::Result<()> {
    let (client_side, server_side) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, client_write) = tokio::io::split(client_side);
    let stdout: guard::SharedStdout = Arc::new(Mutex::new(tokio::io::stdout()));

    let ordering = guard::Ordering::new();
    let handler = PeriscopeServer::new(config);
    let serve_handler = handler.clone();
    let serve = tokio::spawn(async move {
        let transport = AsyncRwTransport::new_server(server_read, server_write);
        match serve_handler.serve(transport).await {
            Ok(running) => running
                .waiting()
                .await
                .map(|_| ())
                .map_err(|e| std::io::Error::other(e.to_string())),
            // The client closed the connection before selecting a
            // lifecycle (a probe, a spawn check, a client that only
            // ever wanted the refusal): the baseline treated bare
            // EOF as a clean end of session, and so do we.
            Err(rmcp::service::ServerInitializeError::ConnectionClosed(_)) => Ok(()),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let mut pump = tokio::spawn(guard::pump(
        client_read,
        Arc::clone(&stdout),
        Arc::clone(&ordering),
    ));

    // When stdin reaches EOF the guard shuts the duplex write side
    // down, which ends the SDK service; the pump ends when the
    // service's transport drops. The pump wait is bounded as a
    // backstop: a response still in flight when the service ends
    // gets its chance to drain, but a wedged pump cannot hang exit.
    let guard_result = guard::run(tokio::io::stdin(), client_write, stdout, ordering).await;
    let serve_result = match serve.await {
        Ok(result) => result,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    };
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
    handler.shutdown_broker().await;
    guard_result.and(serve_result)
}

/// Parse the (deliberately tiny) CLI into a broker configuration.
fn parse_args() -> Result<BrokerConfig, String> {
    let mut workspace: Option<PathBuf> = None;
    let mut attach_socket: Option<PathBuf> = None;
    let mut nvim_bin: PathBuf = PathBuf::from("nvim");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut take_value = |flag: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg.as_str() {
            "--workspace" => workspace = Some(PathBuf::from(take_value("--workspace")?)),
            "--attach" => {
                attach_socket = Some(PathBuf::from(take_value("--attach")?));
            }
            "--nvim-bin" => nvim_bin = PathBuf::from(take_value("--nvim-bin")?),
            "--help" | "-h" => {
                return Err("periscope speaks MCP on stdio; it has no interactive mode".to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let workspace = match workspace {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|error| format!("cannot determine the working directory: {error}"))?,
    };
    let workspace = std::fs::canonicalize(&workspace)
        .map_err(|error| format!("workspace does not resolve: {error}"))?;
    if !workspace.is_dir() {
        return Err(format!(
            "workspace is not a directory: {}",
            workspace.display()
        ));
    }
    Ok(BrokerConfig {
        attach_socket,
        nvim_bin,
        workspace,
    })
}
