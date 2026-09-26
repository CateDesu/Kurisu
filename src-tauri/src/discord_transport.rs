use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use discord_rich_presence::activity::Activity;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
type Socket = tokio::net::UnixStream;
#[cfg(windows)]
type Socket = tokio::net::windows::named_pipe::NamedPipeClient;

static NONCE: AtomicU64 = AtomicU64::new(0);
const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub(super) struct Client {
    socket: Socket,
}

impl Client {
    pub(super) async fn connect(paths: &[PathBuf], id: &str) -> io::Result<Self> {
        for path in paths {
            #[cfg(unix)]
            let socket = Socket::connect(path).await;
            #[cfg(windows)]
            let socket = tokio::net::windows::named_pipe::ClientOptions::new().open(path);
            let Ok(socket) = socket else { continue };
            let mut client = Self { socket };
            client.send(0, &json!({"v": 1, "client_id": id})).await?;
            let mut header = [0u8; 8];
            client.socket.read_exact(&mut header).await?;
            let opcode = u32::from_le_bytes(header[..4].try_into().unwrap());
            let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            if opcode != 1 || size > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Discord handshake",
                ));
            }
            let mut body = vec![0; size];
            client.socket.read_exact(&mut body).await?;
            let reply: Value = serde_json::from_slice(&body)?;
            if reply["evt"] != "READY" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Discord was not ready",
                ));
            }
            return Ok(client);
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Discord IPC unavailable",
        ))
    }

    async fn send(&mut self, opcode: u32, payload: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(payload)?;
        self.socket.write_all(&opcode.to_le_bytes()).await?;
        self.socket
            .write_all(&(body.len() as u32).to_le_bytes())
            .await?;
        self.socket.write_all(&body).await?;
        self.socket.flush().await
    }

    pub(super) async fn set_activity(&mut self, activity: Activity<'_>) -> io::Result<()> {
        self.send(1, &json!({
            "cmd": "SET_ACTIVITY",
            "args": {"pid": std::process::id(), "activity": activity},
            "nonce": format!("{}-{}", std::process::id(), NONCE.fetch_add(1, Ordering::Relaxed)),
        })).await
    }
}

pub(super) fn socket_paths() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        let mut paths = Vec::new();
        for key in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
            let Some(value) = std::env::var_os(key) else {
                continue;
            };
            let mut base = PathBuf::from(value);
            if key == "XDG_RUNTIME_DIR" && std::env::var_os("SNAP").is_some() {
                if let Some(parent) = base.parent() {
                    base = parent.to_path_buf();
                }
            }
            for i in 0..10 {
                for subpath in [
                    "",
                    "app/com.discordapp.Discord",
                    "app/dev.vencord.Vesktop",
                    ".flatpak/com.discordapp.Discord/xdg-run",
                    ".flatpak/dev.vencord.Vesktop/xdg-run",
                    "snap.discord-canary",
                    "snap.discord",
                ] {
                    paths.push(base.join(subpath).join(format!("discord-ipc-{i}")));
                }
            }
        }
        paths
    }
    #[cfg(windows)]
    {
        (0..10)
            .map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}")))
            .collect()
    }
}
