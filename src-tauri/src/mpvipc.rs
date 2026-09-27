//! MPV IPC fallback when OS media sessions have no active player.

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::time::Duration;

/// Bound the whole exchange, including peers that trickle incomplete replies.
const QUERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Skip timed out pipes for the session. Retrying would leak another blocked thread each tick.
#[cfg(windows)]
static POISONED_PIPES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Property indices also identify their IPC replies.
const IDX_PAUSE: usize = 1;
const IDX_PATH: usize = 2;
const IDX_TITLE: usize = 3;
const IDX_FILENAME: usize = 4;
const IDX_DURATION: usize = 5;
const IDX_POSITION: usize = 6;
const PROPS: [(&str, usize); 6] = [
    ("pause", IDX_PAUSE),
    ("path", IDX_PATH),
    ("media-title", IDX_TITLE),
    ("filename", IDX_FILENAME),
    ("duration", IDX_DURATION),
    ("time-pos", IDX_POSITION),
];

pub(crate) struct MpvSnapshot {
    pub playing: bool,
    pub path: String,
    pub media_title: String,
    pub filename: String,
    pub duration_us: i64,
    pub position_us: i64,
}

pub(crate) fn probe(paths: &[String]) -> Option<MpvSnapshot> {
    let mut paused = None;
    for p in paths {
        #[cfg(unix)]
        if std::fs::metadata(p).is_err() {
            continue;
        }
        #[cfg(windows)]
        if POISONED_PIPES.lock().contains(p) {
            continue;
        }
        if let Some(s) = probe_one(p) {
            if s.playing {
                return Some(s);
            }
            if paused.is_none() {
                paused = Some(s);
            }
        }
    }
    paused
}

#[cfg(unix)]
fn probe_one(path: &str) -> Option<MpvSnapshot> {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::os::unix::net::UnixStream;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None).ok()?;
    socket
        .connect_timeout(&SockAddr::unix(path).ok()?, IO_TIMEOUT)
        .ok()?;
    let stream: UnixStream = socket.into();
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let writer = stream.try_clone().ok()?;
    query(
        BufReader::new(stream),
        writer,
        std::time::Instant::now() + QUERY_DEADLINE,
    )
}

#[cfg(windows)]
fn probe_one(path: &str) -> Option<MpvSnapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_string();
    std::thread::spawn(move || {
        let snap = (|| {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&p)
                .ok()?;
            let writer = file.try_clone().ok()?;
            query(
                BufReader::new(file),
                writer,
                std::time::Instant::now() + QUERY_DEADLINE,
            )
        })();
        let _ = tx.send(snap);
    });
    match rx.recv_timeout(QUERY_DEADLINE + std::time::Duration::from_secs(1)) {
        Ok(snap) => snap,
        Err(_) => {
            POISONED_PIPES.lock().insert(path.to_string());
            None
        }
    }
}

fn query<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    deadline: std::time::Instant,
) -> Option<MpvSnapshot> {
    let mut out = String::new();
    for (prop, id) in PROPS {
        out.push_str(&format!(
            "{{\"command\":[\"get_property\",\"{prop}\"],\"request_id\":{id}}}\n"
        ));
    }
    writer.write_all(out.as_bytes()).ok()?;
    writer.flush().ok()?;

    let mut vals: Vec<Option<serde_json::Value>> = vec![None; PROPS.len() + 1];
    let mut received = [false; PROPS.len() + 1];
    let mut answered = 0;
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        let line = read_reply(&mut reader, deadline)?;
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("event").is_some() {
            continue;
        }
        let Some(rid) = v.get("request_id").and_then(|x| x.as_u64()) else {
            continue;
        };
        if rid == 0 || rid >= vals.len() as u64 || received[rid as usize] {
            continue;
        }
        received[rid as usize] = true;
        let ok = v.get("error").and_then(|e| e.as_str()) == Some("success");
        vals[rid as usize] = ok.then(|| v.get("data").cloned()).flatten();
        answered += 1;
        if answered == PROPS.len() {
            return build_snapshot(&vals);
        }
    }
}

fn read_reply(reader: &mut impl BufRead, deadline: std::time::Instant) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        let bytes = reader.fill_buf().ok()?;
        if bytes.is_empty() {
            return None;
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let count = newline.map_or(bytes.len(), |n| n + 1);
        if line.len() + count > MAX_REPLY_BYTES {
            return None;
        }
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Some(line);
        }
    }
}

fn build_snapshot(vals: &[Option<serde_json::Value>]) -> Option<MpvSnapshot> {
    let paused = vals[IDX_PAUSE].as_ref().and_then(|v| v.as_bool())?;
    let path = vals[IDX_PATH].as_ref().and_then(|v| v.as_str())?;
    if path.is_empty() {
        return None;
    }
    let str_at = |i: usize| {
        vals[i]
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let secs_to_us = |i: usize| {
        vals[i]
            .as_ref()
            .and_then(|v| v.as_f64())
            .map(|s| (s * 1_000_000.0).round() as i64)
            .unwrap_or(0)
    };
    Some(MpvSnapshot {
        playing: !paused,
        path: path.to_string(),
        media_title: str_at(IDX_TITLE),
        filename: str_at(IDX_FILENAME),
        duration_us: secs_to_us(IDX_DURATION),
        position_us: secs_to_us(IDX_POSITION),
    })
}

pub(crate) fn default_socket_paths() -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    #[cfg(unix)]
    {
        v.push("/tmp/mpvsocket".into());
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            v.push(format!("{}/mpv.sock", xdg.to_string_lossy()));
        }
        if let Some(home) = std::env::var_os("HOME") {
            let home = home.to_string_lossy();
            v.push(format!("{home}/.cache/mpv/socket"));
            v.push(format!("{home}/.config/mpv/socket"));
        }
    }
    #[cfg(windows)]
    {
        v.push(r"\\.\pipe\mpvsocket".into());
        v.push(r"\\.\pipe\mpv-socket".into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::time::Duration;

    fn query_lines(lines: &str) -> (Option<MpvSnapshot>, String) {
        let mut sink = Vec::new();
        let snap = query(
            BufReader::new(Cursor::new(lines.to_string())),
            &mut sink,
            std::time::Instant::now() + Duration::from_secs(5),
        );
        (snap, String::from_utf8(sink).unwrap())
    }

    struct Trickle;
    impl std::io::Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(6);
            buf[..n].copy_from_slice(&b"junk\n\n"[..n]);
            Ok(n)
        }
    }
    impl BufRead for Trickle {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Ok(&b"junk\n"[..])
        }
        fn consume(&mut self, _: usize) {}
    }

    #[test]
    fn an_ever_trickling_peer_hits_the_deadline() {
        let mut sink = Vec::new();
        let started = std::time::Instant::now();
        let snap = query(
            Trickle,
            &mut sink,
            std::time::Instant::now() + Duration::from_millis(200),
        );
        assert!(snap.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline must bound the exchange"
        );
    }

    #[test]
    fn incomplete_lines_still_hit_the_deadline() {
        struct PartialLine {
            chunks: usize,
        }
        impl std::io::Read for PartialLine {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let chunk = self.fill_buf()?;
                if chunk.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = chunk[0];
                self.consume(1);
                Ok(1)
            }
        }
        impl BufRead for PartialLine {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                std::thread::sleep(Duration::from_millis(10));
                Ok(if self.chunks > 0 { b" " } else { b"" })
            }
            fn consume(&mut self, count: usize) {
                self.chunks -= count;
            }
        }
        let started = std::time::Instant::now();
        let snap = query(
            PartialLine { chunks: 100 },
            Vec::new(),
            started + Duration::from_millis(100),
        );
        assert!(snap.is_none());
        assert!(started.elapsed() < Duration::from_millis(800));
    }

    #[test]
    fn oversized_reply_lines_are_rejected() {
        let line = " ".repeat(MAX_REPLY_BYTES + 1) + "\n";
        assert!(query_lines(&line).0.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_full_socket_backlog_cannot_block_the_probe() {
        use socket2::{Domain, SockAddr, Socket, Type};
        let path = std::env::temp_dir().join(format!("kurisu-mpv-backlog-{}", std::process::id()));
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&SockAddr::unix(&path).unwrap()).unwrap();
        listener.listen(1).unwrap();
        let clients = [
            std::os::unix::net::UnixStream::connect(&path).unwrap(),
            std::os::unix::net::UnixStream::connect(&path).unwrap(),
        ];
        let (release, wait) = std::sync::mpsc::channel();
        let closer = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(2));
            drop(listener);
        });
        let started = std::time::Instant::now();
        assert!(probe_one(path.to_str().unwrap()).is_none());
        let elapsed = started.elapsed();
        let _ = release.send(());
        closer.join().unwrap();
        drop(clients);
        std::fs::remove_file(path).unwrap();
        assert!(elapsed < Duration::from_secs(1));
    }

    fn ok(id: usize, data: &str) -> String {
        format!("{{\"data\":{data},\"request_id\":{id},\"error\":\"success\"}}\n")
    }

    #[test]
    fn parses_a_playing_file() {
        let lines = [
            "{\"event\":\"playback-restart\"}\n".to_string(),
            ok(IDX_PAUSE, "false"),
            ok(IDX_PATH, "\"/anime/[Group] Frieren - 05 [1080p].mkv\""),
            ok(IDX_TITLE, "\"[Group] Frieren - 05\""),
            ok(IDX_FILENAME, "\"[Group] Frieren - 05 [1080p].mkv\""),
            ok(IDX_DURATION, "1440.5"),
            ok(IDX_POSITION, "0.25"),
        ]
        .concat();
        let (snap, sent) = query_lines(&lines);
        let s = snap.expect("all six answers arrived");
        assert!(s.playing);
        assert_eq!(s.path, "/anime/[Group] Frieren - 05 [1080p].mkv");
        assert_eq!(s.media_title, "[Group] Frieren - 05");
        assert_eq!(s.duration_us, 1_440_500_000);
        assert_eq!(s.position_us, 250_000);
        for (prop, id) in PROPS {
            assert!(
                sent.contains(&format!(
                    "[\"get_property\",\"{prop}\"],\"request_id\":{id}"
                )),
                "command burst must ask for {prop} with id {id}"
            );
        }
    }

    #[test]
    fn failed_properties_degrade_instead_of_sinking_the_snapshot() {
        let lines = [
            ok(IDX_PAUSE, "true"),
            ok(IDX_PATH, "\"https://example.com/stream\""),
            "{\"request_id\":3,\"error\":\"property unavailable\"}\n".to_string(),
            ok(IDX_FILENAME, "\"stream\""),
            "{\"request_id\":5,\"error\":\"property unavailable\"}\n".to_string(),
            "{\"request_id\":6,\"error\":\"property unavailable\"}\n".to_string(),
        ]
        .concat();
        let (snap, _) = query_lines(&lines);
        let s = snap.expect("a paused stream with no timeline is still a track");
        assert!(!s.playing);
        assert_eq!(s.path, "https://example.com/stream");
        assert_eq!(s.media_title, "");
        assert_eq!(s.duration_us, 0);
        assert_eq!(s.position_us, 0);
    }

    #[test]
    fn an_idle_mpv_with_no_file_is_not_a_track() {
        let lines = [
            ok(IDX_PAUSE, "false"),
            "{\"request_id\":2,\"error\":\"property unavailable\"}\n".to_string(),
            ok(IDX_TITLE, "null"),
            ok(IDX_FILENAME, "null"),
            "{\"request_id\":5,\"error\":\"property unavailable\"}\n".to_string(),
            "{\"request_id\":6,\"error\":\"property unavailable\"}\n".to_string(),
        ]
        .concat();
        let (snap, _) = query_lines(&lines);
        assert!(snap.is_none());
    }

    #[test]
    fn a_truncated_or_garbage_round_trip_is_none() {
        let lines = [ok(IDX_PAUSE, "false"), ok(IDX_PATH, "\"/a.mkv\"")].concat();
        let (snap, _) = query_lines(&lines);
        assert!(snap.is_none());
    }

    #[test]
    fn out_of_order_and_foreign_answers_are_ignored() {
        let mut lines = String::new();
        lines.push_str(&ok(IDX_FILENAME, "\"f.mkv\""));
        lines.push_str(&ok(99, "\"junk\""));
        lines.push_str(&ok(0, "\"junk\""));
        lines.push_str(&ok(IDX_PAUSE, "false"));
        lines.push_str(&ok(IDX_PATH, "\"/f.mkv\""));
        lines.push_str(&ok(IDX_PATH, "\"/f.mkv\""));
        lines.push_str(&ok(IDX_TITLE, "\"f\""));
        lines.push_str(&ok(IDX_DURATION, "60"));
        lines.push_str(&ok(IDX_POSITION, "10"));
        let (snap, _) = query_lines(&lines);
        assert!(snap.is_some());
    }

    #[test]
    fn duplicate_failed_properties_do_not_finish_the_query_early() {
        let failed = "{\"request_id\":3,\"error\":\"property unavailable\"}\n";
        let lines = [
            ok(IDX_PAUSE, "false"),
            ok(IDX_PATH, "\"/f.mkv\""),
            failed.into(),
            failed.into(),
            ok(IDX_FILENAME, "\"f.mkv\""),
            ok(IDX_DURATION, "60"),
            ok(IDX_POSITION, "10"),
        ]
        .concat();
        let snap = query_lines(&lines).0.unwrap();
        assert_eq!(snap.position_us, 10_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn a_playing_socket_outranks_an_earlier_paused_socket() {
        use std::os::unix::net::UnixListener;
        let mut paths = Vec::new();
        let mut servers = Vec::new();
        for paused in [true, false] {
            let path = std::env::temp_dir()
                .join(format!("kurisu-mpv-choice-{}-{paused}", std::process::id()));
            let listener = UnixListener::bind(&path).unwrap();
            paths.push(path.to_str().unwrap().to_string());
            servers.push(std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                for _ in PROPS {
                    reader.read_line(&mut String::new()).unwrap();
                }
                let lines = [
                    ok(IDX_PAUSE, if paused { "true" } else { "false" }),
                    ok(IDX_PATH, "\"/f.mkv\""),
                    ok(IDX_TITLE, "\"f\""),
                    ok(IDX_FILENAME, "\"f.mkv\""),
                    ok(IDX_DURATION, "60"),
                    ok(IDX_POSITION, "10"),
                ]
                .concat();
                (&stream).write_all(lines.as_bytes()).unwrap();
            }));
        }
        let snap = probe(&paths).unwrap();
        for server in servers {
            server.join().unwrap();
        }
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
        assert!(snap.playing);
    }

    /// Requires MPV. Run with cargo test --lib mpv -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probes_a_live_mpv() {
        let dir = std::env::temp_dir().join(format!("kurisu-mpv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        // Five seconds of silence as 8 kHz, 8 bit mono PCM.
        let data_len = 40_000_u32;
        let mut wav_bytes = Vec::new();
        wav_bytes.extend_from_slice(b"RIFF");
        wav_bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav_bytes.extend_from_slice(b"WAVEfmt ");
        wav_bytes.extend_from_slice(&16_u32.to_le_bytes());
        wav_bytes.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        wav_bytes.extend_from_slice(&1_u16.to_le_bytes()); // mono
        wav_bytes.extend_from_slice(&8000_u32.to_le_bytes());
        wav_bytes.extend_from_slice(&8000_u32.to_le_bytes()); // byte rate
        wav_bytes.extend_from_slice(&1_u16.to_le_bytes()); // block align
        wav_bytes.extend_from_slice(&8_u16.to_le_bytes()); // bits
        wav_bytes.extend_from_slice(b"data");
        wav_bytes.extend_from_slice(&data_len.to_le_bytes());
        wav_bytes.extend(std::iter::repeat_n(0_u8, data_len as usize));
        std::fs::write(&wav, wav_bytes).unwrap();

        let sock = dir.join("sock");
        let mut cmd = std::process::Command::new("mpv");
        // MPV requires the socket argument in the =value form.
        let mut ipc_arg = std::ffi::OsString::from("--input-ipc-server=");
        ipc_arg.push(&sock);
        cmd.arg("--vo=null")
            .arg("--ao=null")
            .arg("--loop-file=inf")
            .arg(&ipc_arg)
            .arg(&wav);
        // Do not let Cargo's library path override MPV's system libraries.
        cmd.env_remove("LD_LIBRARY_PATH");
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("mpv is installed");
        let mut appeared = false;
        for _ in 0..30 {
            if sock.exists() {
                appeared = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(appeared, "mpv never opened the IPC socket");

        let snap = probe_one(sock.to_str().unwrap()).expect("mpv answers with a loaded file");
        assert!(snap.playing);
        assert!(snap.path.ends_with("t.wav"));
        assert_eq!(snap.duration_us, 5_000_000);
        assert!(snap.position_us > 0);

        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }
}
