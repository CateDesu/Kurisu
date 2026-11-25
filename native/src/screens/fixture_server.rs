use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use super::workspace_tests::FixtureDir;

pub(super) fn child_gate(flag: &str) -> Option<PathBuf> {
    let gate = PathBuf::from(std::env::var_os(flag)?);
    fs::write(gate.with_extension("entered"), []).unwrap();
    Some(gate)
}

pub(super) struct Request {
    pub stream: TcpStream,
    pub target: String,
    pub authorization: String,
    body: Vec<u8>,
}

impl Request {
    fn read(stream: TcpStream) -> io::Result<Option<Self>> {
        // Windows can inherit nonblocking mode from the listener.
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = BufReader::new(&stream);
        let mut headers = String::new();
        loop {
            let start = headers.len();
            if reader.read_line(&mut headers)? == 0 {
                return Ok(None);
            }
            if &headers[start..] == "\r\n" {
                break;
            }
        }
        let mut lines = headers.lines();
        let target = lines
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        let mut length = 0;
        let mut authorization = String::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (key, value) = line.split_once(':').expect("invalid fixture header");
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            } else if key.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_owned();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        Ok(Some(Self {
            stream,
            target,
            authorization,
            body,
        }))
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    pub fn reply_json(&mut self, status: u16, body: serde_json::Value) -> io::Result<()> {
        let body = body.to_string();
        write!(
            self.stream,
            "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }
}

pub(super) struct FixtureChild {
    pub gate: PathBuf,
    process: Child,
    deadline: Instant,
}

impl FixtureChild {
    fn running(&mut self) -> bool {
        if self.process.try_wait().unwrap().is_some() {
            return false;
        }
        assert!(Instant::now() < self.deadline, "fixture child timed out");
        true
    }

    pub fn wait_for_gate(&mut self) {
        while !self.gate.exists() {
            assert!(self.running(), "child exited before opening the gate");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

pub(super) fn serve(
    test_name: &str,
    child_flag: &str,
    endpoint: &str,
    mut respond: impl FnMut(Request, &mut FixtureChild),
) {
    assert_eq!(
        thread::current().name(),
        Some(test_name),
        "wrong fixture test"
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let path = std::env::temp_dir().join(format!(
        "kurisu-http-{}-{}",
        std::process::id(),
        address.port()
    ));
    fs::create_dir(&path).unwrap();
    let dir = FixtureDir(path);
    let gate = dir.0.join("gate");
    let log_path = dir.0.join("child.log");
    // A verbose child must be able to finish while the parent serves requests.
    let log = File::create(&log_path).unwrap();
    let process = Command::new(std::env::current_exe().unwrap())
        .args([test_name, "--exact", "--test-threads=1", "--nocapture"])
        .env(child_flag, &gate)
        .env("TMPDIR", &dir.0)
        .env("TMP", &dir.0)
        .env("TEMP", &dir.0)
        .env(endpoint, format!("http://{address}"))
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut child = FixtureChild {
        gate,
        process,
        deadline: Instant::now() + Duration::from_secs(15),
    };
    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop {
            let running = child.running();
            let (stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if !running {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            };
            match Request::read(stream) {
                Ok(Some(request)) => respond(request, &mut child),
                Ok(None) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(error) => panic!("reading fixture request: {error}"),
            }
        }
    }));
    if served.is_err() {
        let _ = child.process.kill();
    }
    let status = child.process.wait().unwrap();
    if !status.success() || served.is_err() {
        eprintln!(
            "{test_name} child {status}\n{}",
            String::from_utf8_lossy(&fs::read(log_path).unwrap())
        );
    }
    if let Err(panic) = served {
        std::panic::resume_unwind(panic);
    }
    assert!(status.success(), "{test_name} child {status}");
    assert!(
        child.gate.with_extension("entered").is_file(),
        "child skipped {test_name}"
    );
}

#[test]
fn cancelled_requests_leave_the_next_fixture_response_available() {
    const CHILD: &str = "KURISU_HTTP_CANCELLATION_CHILD";
    const ENDPOINT: &str = "KURISU_HTTP_CANCELLATION_URL";
    if child_gate(CHILD).is_some() {
        let origin = std::env::var(ENDPOINT).unwrap();
        let address = origin.strip_prefix("http://").unwrap();
        for prefix in [
            b"".as_slice(),
            b"GET /partial HTTP/1.1\r\n",
            b"POST /partial HTTP/1.1\r\nContent-Length: 4\r\n\r\nnu",
        ] {
            TcpStream::connect(address)
                .unwrap()
                .write_all(prefix)
                .unwrap();
        }
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .write_all(b"POST /complete HTTP/1.1\r\nContent-Length: 4\r\n\r\ntrue")
            .unwrap();
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        assert!(reply.ends_with("\r\n\r\ntrue"));
        return;
    }
    let mut requests = 0;
    serve(
        thread::current().name().unwrap(),
        CHILD,
        ENDPOINT,
        |mut request, _| {
            assert_eq!(request.target, "/complete");
            assert_eq!(request.json(), true);
            requests += 1;
            request
                .reply_json(200, serde_json::Value::Bool(true))
                .unwrap();
        },
    );
    assert_eq!(requests, 1);
}
