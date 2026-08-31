//! MPV JSON IPC client, the fallback player source.
//!
//! Bare mpv does not register with the OS media session APIs on either
//! platform. Linux MPRIS support needs the mpv-mpris script and Windows
//! GSMTC only sees mpv.net. What mpv does ship is a JSON IPC socket,
//! enabled with --input-ipc-server, that reports everything the watcher
//! needs: the file path, the title, pause state, and playback position.
//! When the OS media session has nothing playing, the watcher asks the
//! socket instead. Same recognition pipeline either way.
//!
//! One connection per tick. Cheap, and immune to mpv restarting or the
//! socket moving between launches. mpv logs client connects at verbose
//! level only, so polling every 5s never spams a terminal mpv.

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::time::Duration;

/// Whole round trip budget. The per read timeout below bounds a single read,
/// but a peer trickling a garbage line just under that cadence kept the loop
/// alive forever, and on Windows a named pipe peer that accepts but never
/// answers blocks read_line with no timeout at all. One deadline for the
/// entire exchange closes both.
const QUERY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Windows named pipes that missed their deadline once are skipped for the
/// rest of the session. std cannot time out a synchronous pipe read, so the
/// reader thread is abandoned when the deadline hits. Retrying on every 5s
/// tick would leak one thread per tick against a wedged pipe.
#[cfg(windows)]
static POISONED_PIPES: parking_lot::Mutex<std::collections::HashSet<String>> =
    parking_lot::Mutex::new(std::collections::HashSet::new());

/// Per read and write cap so a wedged mpv can't stall the tick. The
/// whole round trip is a few small lines, anything slower than this is
/// a dead connection. Unix sockets only, the Windows named pipe has no
/// timeout support and blocks instead, which is what the deadline and
/// the poison set above are for.
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_millis(500);

/// Properties fetched in one round trip. The index doubles as the IPC
/// request id so answers can be told apart no matter what order they
/// come back in.
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

/// One mpv's state. path is the playing file path or URL and doubles as
/// the per track key since it is unique per file.
pub(crate) struct MpvSnapshot {
    pub playing: bool,
    pub path: String,
    pub media_title: String,
    pub filename: String,
    pub duration_us: i64,
    pub position_us: i64,
}

/// Try each path in order and return the first mpv that answers with a
/// loaded file. A path whose socket is missing is skipped before any
/// connect attempt. On Windows the pipe namespace can't be stat'd, so
/// there the open call itself is the check.
pub(crate) fn probe(paths: &[String]) -> Option<MpvSnapshot> {
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
            return Some(s);
        }
    }
    None
}

/// Unix domain socket. Not linux-only: every unix mpv supports
/// --input-ipc-server the same way, and probe_mpv is simply never
/// called on platforms whose read_now is the no-op stub.
#[cfg(unix)]
fn probe_one(path: &str) -> Option<MpvSnapshot> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let writer = stream.try_clone().ok()?;
    query(
        BufReader::new(stream),
        writer,
        std::time::Instant::now() + QUERY_DEADLINE,
    )
}

/// Windows named pipe. mpv listens with one pipe instance per client so
/// a plain read plus write open is a full duplex connection, but std has no
/// read timeout here: a peer that accepts and never answers parks read_line
/// forever, which killed detection for the whole session and leaked a
/// blocking thread per tick. The open and the query run on a helper thread
/// with a deadline, and a pipe that misses it is poisoned so the abandoned
/// thread is the last one we spend on it.
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

/// Send every get_property in one burst, then read lines until all six
/// answers arrived. mpv pushes event notifications on the same socket
/// unasked, so any line carrying an event key is skipped. A response is
/// matched by request id, and a failed property simply yields none for
/// its slot instead of sinking the round trip. The whole exchange is
/// bounded by `deadline`, so a peer that never answers or keeps feeding
/// garbage lines cannot hold the caller.
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
    let mut answered = 0;
    loop {
        if std::time::Instant::now() >= deadline {
            return None;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None, // mpv closed the connection
            Ok(_) => {}
            Err(_) => return None, // timeout or broken pipe, not worth retrying
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("event").is_some() {
            continue;
        }
        let Some(rid) = v.get("request_id").and_then(|x| x.as_u64()) else {
            continue;
        };
        // Every command went out with an id from PROPS, so a 0 or unknown
        // id is a foreign peer talking and must not consume a slot.
        if rid == 0 || rid as usize >= vals.len() || vals[rid as usize].is_some() {
            continue;
        }
        let ok = v.get("error").and_then(|e| e.as_str()) == Some("success");
        vals[rid as usize] = ok.then(|| v.get("data").cloned()).flatten();
        answered += 1;
        if answered == PROPS.len() {
            return build_snapshot(&vals);
        }
    }
}

/// Assemble the snapshot. Unknown pause state or no loaded file means
/// there is nothing to track, so it is a None rather than a guess.
/// Duration and position degrade to zero the way the MPRIS path does
/// when a player reports no timeline.
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

/// Well known socket locations tried when no explicit path is set. The
/// mpv manual's own example plus the paths common in dotfile setups.
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
    // The top level Duration import is cfg(unix), the live mpv test needs
    // it on Windows too.
    use std::time::Duration;

    /// Run a query against canned socket lines. The sink stands in for
    /// the write half and captures the command burst.
    fn query_lines(lines: &str) -> (Option<MpvSnapshot>, String) {
        let mut sink = Vec::new();
        let snap = query(
            BufReader::new(Cursor::new(lines.to_string())),
            &mut sink,
            std::time::Instant::now() + Duration::from_secs(5),
        );
        (snap, String::from_utf8(sink).unwrap())
    }

    /// A reader that never ends and never says anything useful. Each read
    /// hands back one plausible looking garbage line, the trickle pattern
    /// that kept the old unbounded loop alive forever. The deadline must
    /// cut it off.
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

    fn ok(id: usize, data: &str) -> String {
        format!("{{\"data\":{data},\"request_id\":{id},\"error\":\"success\"}}\n")
    }

    #[test]
    fn parses_a_playing_file() {
        let lines = [
            "{\"event\":\"playback-restart\"}\n".to_string(), // pushed events are skipped
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
        // One get_property line per property, each carrying its request id.
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
        // Streams often carry no duration. time-pos can be briefly
        // unavailable right after a file load. Both come back as errors
        // and the snapshot still works with zeros.
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
        // mpv with --idle answers, but path errors out. Nothing playing.
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
        // Connection dropped after three answers. Never a half snapshot.
        let lines = [ok(IDX_PAUSE, "false"), ok(IDX_PATH, "\"/a.mkv\"")].concat();
        let (snap, _) = query_lines(&lines);
        assert!(snap.is_none());
    }

    #[test]
    fn out_of_order_and_foreign_answers_are_ignored() {
        // A duplicate or unknown request id must not consume a slot or
        // count toward completion. Id 0 included, we never send it.
        let mut lines = String::new();
        lines.push_str(&ok(IDX_FILENAME, "\"f.mkv\""));
        lines.push_str(&ok(99, "\"junk\""));
        lines.push_str(&ok(0, "\"junk\""));
        lines.push_str(&ok(IDX_PAUSE, "false"));
        lines.push_str(&ok(IDX_PATH, "\"/f.mkv\""));
        lines.push_str(&ok(IDX_PATH, "\"/f.mkv\"")); // duplicate, ignored
        lines.push_str(&ok(IDX_TITLE, "\"f\""));
        lines.push_str(&ok(IDX_DURATION, "60"));
        lines.push_str(&ok(IDX_POSITION, "10"));
        let (snap, _) = query_lines(&lines);
        assert!(snap.is_some());
    }

    /// Real mpv over a real socket. Ignored by default so cargo test
    /// stays hermetic. Generates a small silent wav, plays it on loop
    /// with the IPC socket enabled, and runs the same probe the watcher
    /// uses. Run with: cargo test --lib mpv -- --ignored --nocapture
    #[test]
    #[ignore]
    fn probes_a_live_mpv() {
        let dir = std::env::temp_dir().join(format!("kurisu-mpv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        // 8kHz 8-bit mono PCM. 5 seconds of silence.
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
        // The =value form. mpv rejects the space separated form for
        // input-ipc-server. Null outputs keep the test silent, both the
        // wav and the machine's speakers.
        let mut ipc_arg = std::ffi::OsString::from("--input-ipc-server=");
        ipc_arg.push(&sock);
        cmd.arg("--vo=null")
            .arg("--ao=null")
            .arg("--loop-file=inf")
            .arg(&ipc_arg)
            .arg(&wav);
        // cargo runs tests with LD_LIBRARY_PATH pointed at the build
        // dir, and the child would inherit it. mpv must resolve its own
        // libraries from the system paths.
        cmd.env_remove("LD_LIBRARY_PATH");
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("mpv is installed");
        // Wait for the socket to appear.
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
