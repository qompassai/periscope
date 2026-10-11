//! Shared test fixture: a fake editor speaking just enough
//! msgpack-RPC to stand in for Neovim.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use periscope::broker::BrokerConfig;
use rmpv::Value as MpValue;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique socket path for one fake editor.
pub fn temp_socket(tag: &str) -> PathBuf {
    let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "periscope-test-{tag}-{}-{sequence}.sock",
        std::process::id()
    ))
}

/// A broker config that attaches to the given (fake) editor socket.
pub fn config_for(socket: PathBuf) -> BrokerConfig {
    BrokerConfig {
        attach_socket: Some(socket),
        nvim_bin: PathBuf::from("nvim"),
        workspace: std::env::temp_dir(),
    }
}

/// How the fake editor behaves after the handshake. Not every test
/// binary constructs every variant.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
pub enum Behavior {
    /// Answer everything per protocol.
    Normal,
    /// Answer the handshake, then go silent forever.
    SilentAfterHandshake,
    /// Answer the handshake, then close the connection.
    DieAfterHandshake,
}

/// Spawn the fake editor on a socket. It answers `nvim_eval` with 1,
/// echoes `nvim_exec_lua` arguments back under an `echo` key, and
/// turns any code containing `BOOM` into an RPC error.
pub fn spawn_fake_editor(socket: PathBuf, behavior: Behavior) {
    let listener = UnixListener::bind(&socket).expect("bind fake editor socket");
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut handshaken = false;
        loop {
            let message = match rmpv::decode::read_value(&mut reader) {
                Ok(message) => message,
                Err(_) => return,
            };
            let parts = message.as_array().expect("request is an array").clone();
            let id = parts.get(1).and_then(MpValue::as_u64).unwrap_or(0);
            let method = parts.get(2).and_then(MpValue::as_str).unwrap_or("");
            if handshaken && behavior != Behavior::Normal {
                match behavior {
                    Behavior::SilentAfterHandshake => {
                        std::thread::sleep(std::time::Duration::from_secs(30));
                        return;
                    }
                    Behavior::DieAfterHandshake => return,
                    Behavior::Normal => unreachable!(),
                }
            }
            let (error, result) = if method == "nvim_eval" {
                handshaken = true;
                (MpValue::Nil, MpValue::from(1u64))
            } else if method == "nvim_exec_lua" {
                let params = parts.get(3).and_then(MpValue::as_array).expect("params");
                let code = params.first().and_then(MpValue::as_str).unwrap_or("");
                if code.contains("BOOM") {
                    (
                        MpValue::Array(vec![MpValue::from(1u64), MpValue::from("lua exploded")]),
                        MpValue::Nil,
                    )
                } else {
                    let args = params.get(1).cloned().unwrap_or(MpValue::Nil);
                    (
                        MpValue::Nil,
                        MpValue::Map(vec![(MpValue::from("echo"), args)]),
                    )
                }
            } else {
                (MpValue::Nil, MpValue::Nil)
            };
            let reply = MpValue::Array(vec![MpValue::from(1u64), MpValue::from(id), error, result]);
            let mut bytes = Vec::new();
            rmpv::encode::write_value(&mut bytes, &reply).expect("encode reply");
            use std::io::Write;
            let mut writer: UnixStream = stream.try_clone().expect("clone writer");
            if writer.write_all(&bytes).is_err() {
                return;
            }
        }
    });
}
