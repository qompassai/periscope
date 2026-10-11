#![forbid(unsafe_code)]
//! Periscope entry point.
//!
//! Usage: `periscope [--workspace DIR] [--attach SOCK] [--nvim-bin PATH]`
//!
//! Speaks MCP on stdio. All diagnostics go to stderr; stdout carries
//! protocol traffic only. Configuration errors (a missing workspace)
//! fail fast with exit code 2 before the protocol starts.

use std::path::PathBuf;
use std::process::ExitCode;

use periscope::broker::BrokerConfig;

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
    runtime.block_on(periscope::mcp::run(
        tokio::io::stdin(),
        tokio::io::stdout(),
        config,
    ));
    ExitCode::SUCCESS
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
