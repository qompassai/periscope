//! The Neovim broker: owns (or attaches to) a Neovim instance and
//! speaks msgpack-RPC to it.
//!
//! Ownership contract:
//! - Managed mode spawns `nvim --headless --listen <sock>` in the
//!   workspace with the user's own config; this broker owns the child
//!   and reaps it on shutdown.
//! - Attach mode connects to an already-running editor's socket; the
//!   broker never signals that process.
//!
//! Trust note: the peer on this socket is the user's own editor, a
//! local trusted process, so inbound message sizes are not separately
//! capped here; the untrusted boundary (the MCP client) is bounded in
//! the front end. Writes are serialized through one mutex and are
//! never held across an `.await`.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rmpv::Value as MpValue;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::codec;
use crate::error::{Error, Result};

/// Budget for the socket connect + spawn wait, in milliseconds.
const CONNECT_DEADLINE_MS: u64 = 10_000;
/// Budget for a single handshake round trip, in milliseconds.
const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;
/// Budget for the managed editor to finish startup (VimEnter).
const READY_DEADLINE_MS: u64 = 20_000;
/// Grace period for the managed child to exit before it is killed.
const SHUTDOWN_GRACE_MS: u64 = 2_000;
/// Poll interval while waiting for the socket or for startup.
const POLL_INTERVAL_MS: u64 = 50;
/// Longest Neovim error text forwarded to a caller.
const RPC_ERROR_TEXT_MAX: usize = 512;

/// How the brokered editor was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrokerMode {
    /// Connected to an editor the user already had running.
    Attached,
    /// Spawned by this process; owned and reaped by it.
    Managed,
}

impl BrokerMode {
    /// Stable string for status reporting.
    pub fn as_str(self) -> &'static str {
        match self {
            BrokerMode::Attached => "attached",
            BrokerMode::Managed => "managed",
        }
    }
}

/// Static configuration for one broker.
#[derive(Clone, Debug)]
pub struct BrokerConfig {
    /// Socket of a running editor (attach mode) when present.
    pub attach_socket: Option<PathBuf>,
    /// Neovim executable for managed mode.
    pub nvim_bin: PathBuf,
    /// Working directory the editor serves.
    pub workspace: PathBuf,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<MpValue>>>>>;

/// One live broker connection. Create with [`NvimBroker::connect`],
/// finish with [`NvimBroker::shutdown`].
pub struct NvimBroker {
    child: Option<tokio::process::Child>,
    mode: BrokerMode,
    next_id: AtomicU64,
    pending: Pending,
    socket_path: Option<PathBuf>,
    writer: Arc<Mutex<UnixStream>>,
    pub workspace: PathBuf,
}

/// Lock recovering from poisoning: critical sections in this module
/// never panic by construction, and a poisoned lock must not wedge
/// every later request behind one bad message.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl NvimBroker {
    /// Connect to (or spawn) the editor and complete the handshake.
    ///
    /// Failure modes: the editor binary missing or exiting during
    /// startup, the socket never appearing, or a failed handshake all
    /// return typed errors; a managed child that fails to start is
    /// reaped before the error propagates.
    pub async fn connect(config: BrokerConfig) -> Result<NvimBroker> {
        let (mode, socket_path, child) = match &config.attach_socket {
            Some(path) => (BrokerMode::Attached, path.clone(), None),
            None => {
                let (path, child) = spawn_editor(&config).await?;
                (BrokerMode::Managed, path, Some(child))
            }
        };
        let stream = match wait_for_socket(&socket_path).await {
            Ok(stream) => stream,
            Err(error) => {
                if mode == BrokerMode::Managed {
                    // Best-effort reap: the caller never sees this child.
                    if let Some(mut child) = child {
                        let _ = child.start_kill();
                    }
                    let _ = std::fs::remove_file(&socket_path);
                }
                return Err(error);
            }
        };
        let reader_stream = stream.try_clone().map_err(|error| Error::Io {
            context: "cloning the editor socket",
            detail: error.to_string(),
        })?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(stream));
        spawn_reader(reader_stream, Arc::clone(&pending), Arc::clone(&writer));
        let broker = NvimBroker {
            child,
            mode,
            next_id: AtomicU64::new(1),
            pending,
            socket_path: (mode == BrokerMode::Managed).then_some(socket_path),
            writer,
            workspace: config.workspace,
        };
        broker.handshake().await?;
        if mode == BrokerMode::Managed {
            broker.wait_ready().await?;
        }
        Ok(broker)
    }

    /// Whether this broker spawned (and therefore owns) the editor.
    pub fn mode(&self) -> BrokerMode {
        self.mode
    }

    /// One msgpack-RPC request with a hard deadline.
    ///
    /// On timeout the pending entry is removed before returning, so a
    /// late response is dropped by the reader instead of completing a
    /// caller that no longer exists.
    pub async fn request(&self, method: &str, params: MpValue, timeout_ms: u64) -> Result<MpValue> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        let message = MpValue::Array(vec![
            MpValue::from(0u64),
            MpValue::from(id),
            MpValue::from(method),
            params,
        ]);
        let mut bytes = Vec::with_capacity(256);
        rmpv::encode::write_value(&mut bytes, &message).map_err(|error| {
            lock(&self.pending).remove(&id);
            Error::Io {
                context: "encoding an editor request",
                detail: error.to_string(),
            }
        })?;
        {
            let mut writer = lock(&self.writer);
            if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                lock(&self.pending).remove(&id);
                return Err(Error::Io {
                    context: "writing to the editor socket",
                    detail: error.to_string(),
                });
            }
        }
        match timeout(Duration::from_millis(timeout_ms), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::BrokerClosed),
            Err(_) => {
                lock(&self.pending).remove(&id);
                Err(Error::Timeout {
                    operation: "an editor request",
                    budget_ms: timeout_ms,
                })
            }
        }
    }

    /// Execute a Lua chunk in the editor and return its result as JSON.
    pub async fn exec_lua(
        &self,
        code: &str,
        args: Vec<JsonValue>,
        timeout_ms: u64,
    ) -> Result<JsonValue> {
        let mp_args: Vec<MpValue> = args.iter().map(codec::json_to_mp).collect();
        let params = MpValue::Array(vec![MpValue::from(code), MpValue::Array(mp_args)]);
        let result = self.request("nvim_exec_lua", params, timeout_ms).await?;
        Ok(codec::mp_to_json(&result))
    }

    /// Shut the broker down. Managed editors are asked to quit, then
    /// killed after a grace period and reaped; attached editors are
    /// only disconnected from.
    pub async fn shutdown(mut self) {
        if self.mode == BrokerMode::Managed {
            let _ = self
                .request(
                    "nvim_command",
                    MpValue::Array(vec![MpValue::from("qa!")]),
                    1_000,
                )
                .await;
        }
        if let Ok(stream) = self.writer.try_lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        fail_all_pending(&self.pending);
        if let Some(mut child) = self.child.take() {
            let exited = timeout(Duration::from_millis(SHUTDOWN_GRACE_MS), child.wait()).await;
            if exited.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }

    async fn handshake(&self) -> Result<()> {
        let result = self
            .request(
                "nvim_eval",
                MpValue::Array(vec![MpValue::from("1")]),
                HANDSHAKE_TIMEOUT_MS,
            )
            .await?;
        if result.as_u64() != Some(1) {
            return Err(Error::BrokerRpc(
                "unexpected handshake response".to_string(),
            ));
        }
        Ok(())
    }

    async fn wait_ready(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(READY_DEADLINE_MS);
        loop {
            // Neovim has no v:vimentered; has('vim_starting') is the
            // supported probe — it reads 1 during startup and flips
            // to 0 once VimEnter has completed.
            let value = self
                .exec_lua(
                    "return vim.fn.has('vim_starting')",
                    Vec::new(),
                    HANDSHAKE_TIMEOUT_MS,
                )
                .await?;
            if value.as_i64() == Some(0) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout {
                    operation: "editor startup (VimEnter)",
                    budget_ms: READY_DEADLINE_MS,
                });
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        }
    }
}

/// Spawn the managed editor and return its socket path and child.
///
/// The child's stdout is discarded and its stderr goes to a per-run
/// log beside the socket: MCP owns this process's stdout, and editor
/// noise must never reach the protocol stream.
async fn spawn_editor(config: &BrokerConfig) -> Result<(PathBuf, tokio::process::Child)> {
    static SPAWN_SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SPAWN_SEQUENCE.fetch_add(1, Ordering::SeqCst);
    let stem = format!("periscope-{}-{}", std::process::id(), sequence);
    let socket_path = std::env::temp_dir().join(format!("{stem}.sock"));
    let log_path = std::env::temp_dir().join(format!("{stem}.log"));
    let log_file = std::fs::File::create(&log_path).map_err(|error| Error::Io {
        context: "creating the editor log file",
        detail: error.to_string(),
    })?;
    let mut command = tokio::process::Command::new(&config.nvim_bin);
    command
        .args(["--headless", "--listen"])
        .arg(&socket_path)
        .args(["-n", "-i", "NONE", "--cmd", "set noexrc"])
        .current_dir(&config.workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));
    let child = command.spawn().map_err(|error| Error::Io {
        context: "spawning neovim (is it on PATH?)",
        detail: error.to_string(),
    })?;
    Ok((socket_path, child))
}

/// Poll for the editor's socket and connect to it. A socket whose
/// parent directory does not exist can never appear, so that case
/// fails immediately instead of burning the whole deadline.
async fn wait_for_socket(path: &Path) -> Result<UnixStream> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return Err(Error::Io {
            context: "connecting to the editor socket",
            detail: format!("socket directory does not exist: {}", parent.display()),
        });
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(CONNECT_DEADLINE_MS);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::Io {
                        context: "connecting to the editor socket",
                        detail: error.to_string(),
                    });
                }
                tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
        }
    }
}

/// The reader thread: demultiplexes msgpack-RPC messages from the
/// editor. Responses complete their pending callers; requests from
/// the editor are answered as unsupported; notifications are dropped.
/// On EOF or a framing error every pending caller fails closed.
fn spawn_reader(stream: UnixStream, pending: Pending, writer: Arc<Mutex<UnixStream>>) {
    std::thread::spawn(move || {
        let mut buffered = std::io::BufReader::new(stream);
        while let Ok(message) = rmpv::decode::read_value(&mut buffered) {
            handle_message(&message, &pending, &writer);
        }
        fail_all_pending(&pending);
    });
}

fn handle_message(message: &MpValue, pending: &Pending, writer: &Arc<Mutex<UnixStream>>) {
    let Some(parts) = message.as_array() else {
        return;
    };
    let Some(kind) = parts.first().and_then(MpValue::as_u64) else {
        return;
    };
    match kind {
        1 => {
            // Response: [1, id, error, result]
            let (Some(id), Some(error), Some(result)) = (
                parts.get(1).and_then(MpValue::as_u64),
                parts.get(2),
                parts.get(3),
            ) else {
                return;
            };
            if let Some(sender) = lock(pending).remove(&id) {
                let outcome = if error.is_nil() {
                    Ok(result.clone())
                } else {
                    Err(Error::BrokerRpc(describe_rpc_error(error)))
                };
                let _ = sender.send(outcome);
            }
        }
        0 => {
            // Request from the editor: this broker serves no methods.
            if let Some(id) = parts.get(1).and_then(MpValue::as_u64) {
                let reply = MpValue::Array(vec![
                    MpValue::from(1u64),
                    MpValue::from(id),
                    MpValue::Array(vec![
                        MpValue::from(0u64),
                        MpValue::from("periscope: unsupported request"),
                    ]),
                    MpValue::Nil,
                ]);
                let mut bytes = Vec::with_capacity(128);
                if rmpv::encode::write_value(&mut bytes, &reply).is_ok() {
                    let mut stream = lock(writer);
                    let _ = stream.write_all(&bytes).and_then(|()| stream.flush());
                }
            }
        }
        _ => {}
    }
}

fn fail_all_pending(pending: &Pending) {
    let drained: Vec<_> = lock(pending).drain().collect();
    for (_, sender) in drained {
        let _ = sender.send(Err(Error::BrokerClosed));
    }
}

/// Render a Neovim RPC error compactly: `[code, "message"]` arrays
/// yield their message; anything else is debug-formatted and capped.
fn describe_rpc_error(error: &MpValue) -> String {
    let text = match error {
        MpValue::Array(parts) => parts
            .get(1)
            .and_then(|part| part.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{error:?}")),
        MpValue::String(text) => text.as_str().unwrap_or_default().to_owned(),
        _ => format!("{error:?}"),
    };
    text.chars().take(RPC_ERROR_TEXT_MAX).collect()
}
