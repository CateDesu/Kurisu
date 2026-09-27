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
    incoming: Vec<u8>,
}

impl Client {
    pub(super) async fn connect(paths: &[PathBuf], id: &str) -> io::Result<Self> {
        let mut last_error = io::Error::new(io::ErrorKind::NotFound, "Discord IPC unavailable");
        for path in paths {
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                Self::connect_one(path, id),
            )
            .await
            {
                Ok(Ok(client)) => return Ok(client),
                Ok(Err(error)) => last_error = error,
                Err(_) => {
                    last_error =
                        io::Error::new(io::ErrorKind::TimedOut, "Discord handshake timed out")
                }
            }
        }
        Err(last_error)
    }

    async fn connect_one(path: &std::path::Path, id: &str) -> io::Result<Self> {
        #[cfg(unix)]
        let socket = Socket::connect(path).await?;
        #[cfg(windows)]
        let socket = tokio::net::windows::named_pipe::ClientOptions::new().open(path)?;
        let mut client = Self {
            socket,
            incoming: Vec::new(),
        };
        client.send(0, &json!({"v": 1, "client_id": id})).await?;
        let (opcode, body) = client.read_frame(true).await?;
        if opcode != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Discord handshake",
            ));
        }
        let reply: Value = serde_json::from_slice(&body)?;
        if reply["evt"] != "READY" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Discord was not ready",
            ));
        }
        Ok(client)
    }

    async fn send(&mut self, opcode: u32, payload: &Value) -> io::Result<()> {
        let body = serde_json::to_vec(payload)?;
        self.send_bytes(opcode, &body).await
    }

    async fn send_bytes(&mut self, opcode: u32, body: &[u8]) -> io::Result<()> {
        self.socket.write_all(&opcode.to_le_bytes()).await?;
        self.socket
            .write_all(&(body.len() as u32).to_le_bytes())
            .await?;
        self.socket.write_all(body).await?;
        self.socket.flush().await
    }

    async fn read_frame(&mut self, handshake: bool) -> io::Result<(u32, Vec<u8>)> {
        loop {
            if self.incoming.len() >= 8 {
                let opcode = u32::from_le_bytes(self.incoming[..4].try_into().unwrap());
                let size = u32::from_le_bytes(self.incoming[4..8].try_into().unwrap()) as usize;
                if (handshake && opcode != 1)
                    || !matches!(opcode, 1 | 3 | 4)
                    || size > MAX_FRAME_BYTES
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid Discord frame",
                    ));
                }
                if self.incoming.len() >= 8 + size {
                    let body = self.incoming[8..8 + size].to_vec();
                    self.incoming.drain(..8 + size);
                    return Ok((opcode, body));
                }
            }
            let mut chunk = [0; 4096];
            let read = self.socket.read(&mut chunk).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Discord disconnected",
                ));
            }
            self.incoming.extend_from_slice(&chunk[..read]);
        }
    }

    pub(super) async fn receive(&mut self) -> io::Result<Option<Vec<u8>>> {
        let (opcode, body) = self.read_frame(false).await?;
        if opcode == 3 {
            return Ok(Some(body));
        }
        if opcode == 1 {
            let payload: Value = serde_json::from_slice(&body)?;
            if payload["evt"] == "ERROR" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Discord rejected activity",
                ));
            }
        }
        Ok(None)
    }

    pub(super) async fn pong(&mut self, body: &[u8]) -> io::Result<()> {
        self.send_bytes(4, body).await
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
        let mut bases = Vec::new();
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
            bases.push(base);
        }
        socket_paths_from_bases(bases)
    }
    #[cfg(windows)]
    {
        (0..10)
            .map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}")))
            .collect()
    }
}

#[cfg(unix)]
fn socket_paths_from_bases(mut bases: Vec<PathBuf>) -> Vec<PathBuf> {
    bases.push(PathBuf::from("/tmp"));
    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for base in bases {
        if !seen.insert(base.clone()) {
            continue;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn optional_environment_paths_keep_a_unique_temporary_fallback() {
        let fallback = socket_paths_from_bases(vec![]);
        assert!(fallback.contains(&PathBuf::from("/tmp/discord-ipc-0")));
        assert_eq!(
            fallback,
            socket_paths_from_bases(vec![PathBuf::from("/tmp"), PathBuf::from("/tmp")])
        );
    }

    async fn handshake(socket: &mut tokio::net::UnixStream) {
        let mut header = [0; 8];
        socket.read_exact(&mut header).await.unwrap();
        let mut body = vec![0; u32::from_le_bytes(header[4..].try_into().unwrap()) as usize];
        socket.read_exact(&mut body).await.unwrap();
    }

    async fn reply(socket: &mut tokio::net::UnixStream, event: &str) {
        let body = format!("{{\"evt\":\"{event}\"}}");
        socket.write_all(&1u32.to_le_bytes()).await.unwrap();
        socket
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn failed_and_stalled_endpoints_do_not_mask_a_ready_endpoint() {
        for stalled in [false, true] {
            let first_path = std::env::temp_dir().join(format!(
                "kurisu-discord-first-{}-{stalled}",
                std::process::id()
            ));
            let next_path = std::env::temp_dir().join(format!(
                "kurisu-discord-next-{}-{stalled}",
                std::process::id()
            ));
            let first = tokio::net::UnixListener::bind(&first_path).unwrap();
            let next = tokio::net::UnixListener::bind(&next_path).unwrap();
            let bad = tokio::spawn(async move {
                let (mut socket, _) = first.accept().await.unwrap();
                handshake(&mut socket).await;
                if !stalled {
                    reply(&mut socket, "ERROR").await;
                }
                let _ = socket.read(&mut [0]).await;
            });
            let good = tokio::spawn(async move {
                let (mut socket, _) = next.accept().await.unwrap();
                handshake(&mut socket).await;
                reply(&mut socket, "READY").await;
                let _ = socket.read(&mut [0]).await;
            });
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                Client::connect(&[first_path.clone(), next_path.clone()], "test-client"),
            )
            .await;
            bad.abort();
            good.abort();
            std::fs::remove_file(first_path).unwrap();
            std::fs::remove_file(next_path).unwrap();
            assert!(matches!(result, Ok(Ok(_))), "stalled={stalled}");
        }
    }
}
