#![forbid(unsafe_code)]
//! periscope-tui entry point.
//!
//! Usage: `periscope-tui [--server PATH] [--workspace DIR]
//! [--attach SOCK] [--line]`
//!
//! The TUI is an MCP stdio client: it spawns the `periscope` server
//! (default: `periscope` on PATH) and speaks NDJSON JSON-RPC to it.
//! Spawning the server does not launch any editor — the server
//! connects its broker lazily on the first tool call, and this
//! console never attaches to the user's interactive Neovim itself.
//! `--workspace` / `--attach` are passed through to the server.
//!
//! With a TTY on stdin this runs the full-screen ratatui loop;
//! without one (or with `--line`) it runs the line mode over stdin,
//! sharing the same execute path.

use std::io::{BufReader, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use periscope_tui::SERVER_ARGS_MAX;
use periscope_tui::app::{App, run_line_mode};
use periscope_tui::facade::McpStdioFacade;
use periscope_tui::panels;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

struct Cli {
    server: PathBuf,
    server_args: Vec<String>,
    force_line: bool,
}

fn parse_args() -> Result<Cli, String> {
    let mut server = PathBuf::from("periscope");
    let mut server_args: Vec<String> = Vec::new();
    let mut force_line = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--server requires a value".to_string())?;
                server = PathBuf::from(value);
            }
            "--workspace" | "--attach" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                if server_args.len() + 2 > SERVER_ARGS_MAX {
                    return Err("too many server arguments".to_string());
                }
                server_args.push(arg);
                server_args.push(value);
            }
            "--line" => force_line = true,
            "--help" | "-h" => {
                return Err("usage: periscope-tui [--server PATH] [--workspace DIR] \
                     [--attach SOCK] [--line]"
                    .to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Cli {
        server,
        server_args,
        force_line,
    })
}

fn main() -> ExitCode {
    let cli = match parse_args() {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("periscope-tui: {message}");
            return ExitCode::from(2);
        }
    };
    let facade = match McpStdioFacade::spawn(cli.server.clone(), &cli.server_args) {
        Ok(facade) => facade,
        Err(error) => {
            eprintln!("periscope-tui: cannot start the periscope server: {error}");
            return ExitCode::from(2);
        }
    };
    let mut app = App::new(facade);
    app.refresh();
    if cli.force_line || !std::io::stdin().is_terminal() {
        let input = BufReader::new(std::io::stdin());
        let mut output = std::io::stdout();
        return match run_line_mode(&mut app, input, &mut output) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("periscope-tui: line mode failed: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match run_fullscreen(&mut app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("periscope-tui: terminal loop failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_fullscreen(app: &mut App<McpStdioFacade>) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let outcome = loop {
        terminal.draw(|frame| panels::draw(frame, app))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
        if app.should_quit() {
            break Ok(());
        }
    };
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    let _ = terminal.show_cursor();
    outcome
}
