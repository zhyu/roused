use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEAD_BYTES: usize = 64 * 1024;

pub struct RawServer {
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RawServer {
    pub fn spawn<F>(mut handler: F) -> Self
    where
        F: FnMut(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
        let address = listener.local_addr().expect("fixture listener address");
        listener
            .set_nonblocking(true)
            .expect("make fixture listener nonblocking");
        let stopping = Arc::new(AtomicBool::new(false));
        let thread_stopping = Arc::clone(&stopping);
        let thread = thread::spawn(move || {
            while !thread_stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("make fixture connection blocking");
                        stream
                            .set_read_timeout(Some(IO_TIMEOUT))
                            .expect("set fixture read timeout");
                        stream
                            .set_write_timeout(Some(IO_TIMEOUT))
                            .expect("set fixture write timeout");
                        handler(stream);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        Self {
            address,
            stopping,
            thread: Some(thread),
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && let Err(payload) = thread.join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(payload);
        }
    }
}

pub struct ProxyProcess {
    address: SocketAddr,
    child: Child,
    _configuration_directory: tempfile::TempDir,
}

impl ProxyProcess {
    pub fn spawn(make_configuration: impl FnOnce(SocketAddr) -> String) -> Self {
        let address = unused_loopback_address();
        let configuration = make_configuration(address);
        let configuration_directory =
            tempfile::tempdir().expect("create proxy configuration directory");
        let configuration_path = configuration_directory.path().join("roused.toml");
        fs::write(&configuration_path, configuration).expect("write proxy configuration");

        let mut child = Command::new(env!("CARGO_BIN_EXE_roused"))
            .arg(configuration_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start roused");
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = child.try_wait().expect("poll roused startup") {
                panic!("roused exited during startup with {status}");
            }
            if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "roused did not start in time");
            thread::sleep(Duration::from_millis(20));
        }

        Self {
            address,
            child,
            _configuration_directory: configuration_directory,
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn unused_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback address");
    listener.local_addr().expect("reserved loopback address")
}

pub fn run_roused_to_exit(configuration: &str) -> (ExitStatus, String) {
    let configuration_directory =
        tempfile::tempdir().expect("create invalid configuration directory");
    let configuration_path = configuration_directory.path().join("roused.toml");
    fs::write(&configuration_path, configuration).expect("write invalid configuration");
    let mut child = Command::new(env!("CARGO_BIN_EXE_roused"))
        .arg(configuration_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start roused with invalid configuration");
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll invalid roused process") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("roused did not reject an invalid configuration in time");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("capture roused stderr")
        .read_to_string(&mut stderr)
        .expect("read roused stderr");
    (status, stderr)
}

pub struct RawRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

pub struct RawRequestHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub buffered_body: Vec<u8>,
}

pub fn read_request(stream: &mut TcpStream) -> io::Result<RawRequest> {
    let head = read_request_head(stream)?;
    let body = read_message_body(stream, &head.headers, head.buffered_body)?;
    Ok(RawRequest {
        method: head.method,
        target: head.target,
        headers: head.headers,
        body,
    })
}

pub fn read_request_head(stream: &mut TcpStream) -> io::Result<RawRequestHead> {
    let (head, buffered_body) = read_head_bytes(stream)?;
    let head = str::from_utf8(&head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 HTTP request head"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_owned();
    let target = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing request target"))?
        .to_owned();
    let _version = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version"))?;
    if request_parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed request line",
        ));
    }
    Ok(RawRequestHead {
        method,
        target,
        headers: parse_headers(lines)?,
        buffered_body,
    })
}

pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers
            .iter()
            .filter(move |(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

pub struct RawResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub buffered_body: Vec<u8>,
}

pub fn send_request(address: SocketAddr, request: &[u8]) -> io::Result<RawResponse> {
    let mut stream = connect(address)?;
    stream.write_all(request)?;
    stream.flush()?;
    read_response(&mut stream)
}

pub fn connect(address: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub fn read_response(stream: &mut TcpStream) -> io::Result<RawResponse> {
    let head = read_response_head(stream)?;
    let body = read_message_body(stream, &head.headers, head.buffered_body)?;
    Ok(RawResponse {
        status: head.status,
        headers: head.headers,
        body,
    })
}

pub fn read_response_head(stream: &mut TcpStream) -> io::Result<RawResponseHead> {
    let (head, buffered_body) = read_head_bytes(stream)?;
    let head = str::from_utf8(&head)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 HTTP response head"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing status line"))?;
    let mut status_parts = status_line.split_whitespace();
    let version = status_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP version"))?;
    if !version.starts_with("HTTP/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "malformed HTTP version",
        ));
    }
    let status = status_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing status"))?
        .parse::<u16>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid status"))?;
    Ok(RawResponseHead {
        status,
        headers: parse_headers(lines)?,
        buffered_body,
    })
}

pub fn read_exact_with_prefix(
    stream: &mut TcpStream,
    prefix: Vec<u8>,
    length: usize,
) -> io::Result<Vec<u8>> {
    let mut body = vec![0; length];
    let mut reader = Cursor::new(prefix).chain(stream);
    reader.read_exact(&mut body)?;
    Ok(body)
}

pub fn write_response<'a>(
    stream: &mut TcpStream,
    status: &str,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    body: &[u8],
) -> io::Result<()> {
    let headers = headers.into_iter().collect::<Vec<_>>();
    write!(stream, "HTTP/1.1 {status}\r\n")?;
    for (name, value) in &headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
    {
        write!(stream, "Content-Length: {}\r\n", body.len())?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(body)?;
    stream.flush()
}

fn parse_headers<'a>(lines: impl Iterator<Item = &'a str>) -> io::Result<Vec<(String, String)>> {
    lines
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line.split_once(':').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP header")
            })?;
            Ok((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn read_head_bytes(stream: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP head",
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let buffered_body = bytes.split_off(index + 4);
            bytes.truncate(index);
            return Ok((bytes, buffered_body));
        }
        if bytes.len() > MAX_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP head is too large",
            ));
        }
    }
}

fn read_message_body(
    stream: &mut TcpStream,
    headers: &[(String, String)],
    buffered_body: Vec<u8>,
) -> io::Result<Vec<u8>> {
    if header_values(headers, "transfer-encoding").any(|value| {
        value
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
    }) {
        return read_chunked_body(stream, buffered_body);
    }
    if let Some(length) = header_values(headers, "content-length").next() {
        let length = length
            .parse::<usize>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid content length"))?;
        return read_exact_with_prefix(stream, buffered_body, length);
    }
    Ok(Vec::new())
}

fn header_values<'a>(
    headers: &'a [(String, String)],
    name: &'a str,
) -> impl Iterator<Item = &'a str> {
    headers
        .iter()
        .filter(move |(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn read_chunked_body(stream: &mut TcpStream, prefix: Vec<u8>) -> io::Result<Vec<u8>> {
    let mut reader = BufReader::new(Cursor::new(prefix).chain(stream));
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing chunk size"))?;
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                if trailer == "\r\n" || trailer == "\n" || trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        let previous_length = body.len();
        body.resize(previous_length + size, 0);
        reader.read_exact(&mut body[previous_length..])?;
        let mut terminator = [0; 2];
        reader.read_exact(&mut terminator)?;
        if terminator != *b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid chunk terminator",
            ));
        }
    }
}
