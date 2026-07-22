mod support;

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{
    ProxyProcess, RawResponse, RawResponseHead, connect, read_request, read_response,
    read_response_head, send_request, unused_loopback_address, write_response,
};

const HTML_BODY: &[u8] = b"<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"5\"><title>Loading</title></head><body>Service is starting.</body></html>\n";
const JSON_BODY: &[u8] = b"{\"status\":\"loading\",\"retry_after\":5}\n";
const COOLDOWN: Duration = Duration::from_secs(5);
const WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const ADDRESS_ENV: &str = "ROUSED_M2_FIXTURE_ADDRESS";
const GATE_ENV: &str = "ROUSED_M2_FIXTURE_GATE";
const START_LOG_ENV: &str = "ROUSED_M2_FIXTURE_START_LOG";
const REQUEST_LOG_ENV: &str = "ROUSED_M2_FIXTURE_REQUEST_LOG";

#[test]
fn wakes_one_disposable_user_launch_agent_without_replaying_cold_requests() {
    let mut fixture = LaunchAgentFixture::new();
    let upstream = fixture.address;
    let label = fixture.label.clone();
    let proxy = ProxyProcess::spawn_with_stderr_capture(|listen| {
        format!(
            "listen = \"{listen}\"\n\n[[services]]\nhost = \"wake.apps.test\"\nupstream = \"{upstream}\"\nlaunchd_label = \"{label}\"\n"
        )
    });

    let failed_attempt = send_request(
        proxy.address(),
        b"GET /before-bootstrap HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: application/json\r\n\r\n",
    )
    .expect("send request before fixture bootstrap");
    assert_json_loading(&failed_attempt);
    wait_for_proxy_log(&proxy, "launchctl kickstart failed");
    assert_timestamped_log_entry(
        &proxy.stderr_contents(),
        "roused::wake",
        "launchctl kickstart failed",
    );
    assert_eq!(launch_attempts(&proxy), 1);

    fixture.bootstrap();
    for _ in 0..3 {
        let refresh = send_request(
            proxy.address(),
            b"GET /failed-cooldown HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: application/json\r\n\r\n",
        )
        .expect("refresh during failed-attempt cooldown");
        assert_json_loading(&refresh);
    }
    assert_eq!(launch_attempts(&proxy), 1);
    assert_eq!(file_lines(&fixture.start_log), Vec::<String>::new());

    thread::sleep(COOLDOWN + Duration::from_millis(250));
    let attempts_before_cold_batch = launch_attempts(&proxy);
    let proxy_address = proxy.address();
    let barrier = Arc::new(Barrier::new(20));
    let mut requests = Vec::new();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for index in 0..20 {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                let request = format!(
                    "GET /cold/{index} HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: text/html\r\n\r\n"
                );
                send_request(proxy_address, request.as_bytes())
                    .expect("send concurrent cold request")
            }));
        }
        for handle in handles {
            requests.push(handle.join().expect("join concurrent cold request"));
        }
    });
    for response in &requests {
        assert_html_loading(response);
    }

    wait_for_file_lines(&fixture.start_log, 1);
    wait_for_proxy_log(&proxy, "launchctl kickstart completed");
    let successful_attempt_finished = Instant::now();
    assert_eq!(
        launch_attempts(&proxy),
        attempts_before_cold_batch + 1,
        "twenty concurrent cold requests invoked more than one kickstart"
    );
    assert_eq!(file_lines(&fixture.start_log), ["started"]);

    let html_refresh = send_request(
        proxy.address(),
        b"GET /html-refresh HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: text/html,application/xhtml+xml\r\n\r\n",
    )
    .expect("send HTML refresh");
    assert_html_loading(&html_refresh);

    let mut head_client = connect(proxy.address()).expect("connect HEAD client");
    head_client
        .write_all(b"HEAD /html-head HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: text/html\r\n\r\n")
        .expect("write cold HEAD request");
    head_client.flush().expect("flush cold HEAD request");
    let head_response = read_response_head(&mut head_client).expect("read cold HEAD response");
    assert_html_loading_head(&head_response);
    let mut after_head = [0; 1];
    assert_eq!(
        head_client
            .read(&mut after_head)
            .expect("read after HEAD response"),
        0,
        "cold HEAD connection remained reusable"
    );

    let api_refresh = send_request(
        proxy.address(),
        b"GET /api-refresh HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: application/json,*/*\r\n\r\n",
    )
    .expect("send API refresh");
    assert_json_loading(&api_refresh);

    let post_body = b"cold body must not be replayed";
    let post_head = format!(
        "POST /cold-post HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: text/html\r\nContent-Length: {}\r\n\r\n",
        post_body.len()
    );
    let mut post_request = post_head.into_bytes();
    post_request.extend_from_slice(post_body);
    let post_response =
        send_request(proxy.address(), &post_request).expect("send complete cold POST");
    assert_json_loading(&post_response);

    assert_unread_body_closes_connection(proxy.address());
    for _ in 0..3 {
        let refresh = send_request(
            proxy.address(),
            b"GET /successful-cooldown HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: application/json\r\n\r\n",
        )
        .expect("refresh during successful-attempt cooldown");
        assert_json_loading(&refresh);
    }
    assert!(
        successful_attempt_finished.elapsed() < COOLDOWN,
        "loading-response checks exceeded the fixed cooldown"
    );
    assert_eq!(
        launch_attempts(&proxy),
        attempts_before_cold_batch + 1,
        "refreshes during cooldown invoked launchctl again"
    );
    assert_eq!(file_lines(&fixture.request_log), Vec::<String>::new());

    fixture.release_startup_gate();
    wait_for_listener(upstream);
    let later_retry = send_request(
        proxy.address(),
        b"GET /later-retry HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: application/json\r\n\r\n",
    )
    .expect("send later retry after fixture becomes ready");
    assert_eq!(later_retry.status, 200);
    assert_eq!(later_retry.header("content-type"), Some("text/plain"));
    assert_eq!(later_retry.body, b"awake fixture\n");
    assert!(
        successful_attempt_finished.elapsed() < COOLDOWN,
        "later retry did not prove readiness overrides the cooldown"
    );
    wait_for_file_lines(&fixture.request_log, 1);
    assert_eq!(file_lines(&fixture.request_log), ["GET /later-retry 0"]);
    assert_eq!(launch_attempts(&proxy), attempts_before_cold_batch + 1);

    fixture.bootout();
}

#[test]
#[ignore = "entry point for the disposable current-user LaunchAgent fixture"]
fn launch_agent_child_entry() {
    let Ok(address) = env::var(ADDRESS_ENV) else {
        return;
    };
    let address = address
        .parse::<SocketAddr>()
        .expect("parse LaunchAgent fixture address");
    let gate = PathBuf::from(env::var_os(GATE_ENV).expect("fixture gate path"));
    let start_log = PathBuf::from(env::var_os(START_LOG_ENV).expect("fixture start log path"));
    let request_log =
        PathBuf::from(env::var_os(REQUEST_LOG_ENV).expect("fixture request log path"));
    append_line(&start_log, "started");

    let deadline = Instant::now() + Duration::from_secs(30);
    while !gate.exists() {
        assert!(Instant::now() < deadline, "fixture startup gate timed out");
        thread::sleep(Duration::from_millis(10));
    }

    let listener = TcpListener::bind(address).expect("bind LaunchAgent fixture listener");
    loop {
        let (mut stream, _) = listener.accept().expect("accept fixture connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set fixture write timeout");
        let mut first_byte = [0; 1];
        match stream.peek(&mut first_byte) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => panic!("fixture peek failed: {error}"),
        }
        let request = read_request(&mut stream).expect("read proxied fixture request");
        append_line(
            &request_log,
            &format!(
                "{} {} {}",
                request.method,
                request.target,
                request.body.len()
            ),
        );
        write_response(
            &mut stream,
            "200 OK",
            [("Content-Type", "text/plain"), ("Connection", "close")],
            b"awake fixture\n",
        )
        .expect("write awake fixture response");
    }
}

struct LaunchAgentFixture {
    _directory: tempfile::TempDir,
    label: String,
    user_id: u32,
    address: SocketAddr,
    plist: PathBuf,
    gate: PathBuf,
    start_log: PathBuf,
    request_log: PathBuf,
    bootstrapped: bool,
}

impl LaunchAgentFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create LaunchAgent fixture directory");
        let user_id = directory
            .path()
            .metadata()
            .expect("read fixture directory metadata")
            .uid();
        let address = unused_loopback_address();
        let label = format!(
            "com.openai.roused.test.wake.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos()
        );
        let plist = directory.path().join("wake-fixture.plist");
        let gate = directory.path().join("release-startup");
        let start_log = directory.path().join("starts.log");
        let request_log = directory.path().join("requests.log");
        let stdout_log = directory.path().join("launchagent.stdout");
        let stderr_log = directory.path().join("launchagent.stderr");
        let executable = env::current_exe().expect("resolve integration-test executable");
        let contents = launch_agent_plist(
            &label,
            &executable,
            address,
            &gate,
            &start_log,
            &request_log,
            &stdout_log,
            &stderr_log,
        );
        fs::write(&plist, contents).expect("write disposable LaunchAgent plist");

        Self {
            _directory: directory,
            label,
            user_id,
            address,
            plist,
            gate,
            start_log,
            request_log,
            bootstrapped: false,
        }
    }

    fn bootstrap(&mut self) {
        let domain = format!("gui/{}", self.user_id);
        let output = Command::new("/bin/launchctl")
            .args(["bootstrap", domain.as_str()])
            .arg(&self.plist)
            .output()
            .expect("run current-user launchctl bootstrap");
        assert!(
            output.status.success(),
            "current-user bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.bootstrapped = true;
    }

    fn release_startup_gate(&self) {
        fs::write(&self.gate, b"release\n").expect("release fixture startup gate");
    }

    fn bootout(&mut self) {
        if !self.bootstrapped {
            return;
        }
        let target = format!("gui/{}/{}", self.user_id, self.label);
        let output = Command::new("/bin/launchctl")
            .args(["bootout", target.as_str()])
            .output()
            .expect("run current-user launchctl bootout");
        assert!(
            output.status.success(),
            "current-user bootout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.bootstrapped = false;
    }
}

impl Drop for LaunchAgentFixture {
    fn drop(&mut self) {
        if self.bootstrapped {
            let target = format!("gui/{}/{}", self.user_id, self.label);
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", target.as_str()])
                .status();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_agent_plist(
    label: &str,
    executable: &Path,
    address: SocketAddr,
    gate: &Path,
    start_log: &Path,
    request_log: &Path,
    stdout_log: &Path,
    stderr_log: &Path,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--ignored</string>\n    <string>--exact</string>\n    <string>launch_agent_child_entry</string>\n    <string>--nocapture</string>\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>{ADDRESS_ENV}</key><string>{address}</string>\n    <key>{GATE_ENV}</key><string>{}</string>\n    <key>{START_LOG_ENV}</key><string>{}</string>\n    <key>{REQUEST_LOG_ENV}</key><string>{}</string>\n  </dict>\n  <key>RunAtLoad</key><false/>\n  <key>KeepAlive</key><false/>\n  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n</dict>\n</plist>\n",
        xml_escape(label),
        xml_escape(&executable.display().to_string()),
        xml_escape(&gate.display().to_string()),
        xml_escape(&start_log.display().to_string()),
        xml_escape(&request_log.display().to_string()),
        xml_escape(&stdout_log.display().to_string()),
        xml_escape(&stderr_log.display().to_string()),
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn assert_html_loading(response: &RawResponse) {
    assert_loading_headers(
        response.status,
        &response.headers,
        "text/html; charset=utf-8",
        HTML_BODY.len(),
    );
    assert_eq!(response.body, HTML_BODY);
}

fn assert_html_loading_head(response: &RawResponseHead) {
    assert_loading_headers(
        response.status,
        &response.headers,
        "text/html; charset=utf-8",
        HTML_BODY.len(),
    );
    assert!(response.buffered_body.is_empty());
}

fn assert_json_loading(response: &RawResponse) {
    assert_loading_headers(
        response.status,
        &response.headers,
        "application/json",
        JSON_BODY.len(),
    );
    assert_eq!(response.body, JSON_BODY);
}

fn assert_loading_headers(
    status: u16,
    headers: &[(String, String)],
    content_type: &str,
    content_length: usize,
) {
    assert_eq!(status, 503);
    assert_eq!(header(headers, "content-type"), Some(content_type));
    assert_eq!(header(headers, "retry-after"), Some("5"));
    assert_eq!(header(headers, "cache-control"), Some("no-store"));
    let content_length = content_length.to_string();
    assert_eq!(
        header(headers, "content-length"),
        Some(content_length.as_str())
    );
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn assert_unread_body_closes_connection(address: SocketAddr) {
    let mut client = connect(address).expect("connect partial-body client");
    client
        .write_all(
            b"POST /partial-body HTTP/1.1\r\nHost: wake.apps.test\r\nContent-Length: 100\r\n\r\npartial",
        )
        .expect("write partial cold request body");
    client.flush().expect("flush partial cold request body");
    let response = read_response(&mut client).expect("read partial-body loading response");
    assert_json_loading(&response);

    let second_request =
        b"GET /must-not-reuse HTTP/1.1\r\nHost: wake.apps.test\r\nAccept: text/html\r\n\r\n";
    if client.write_all(second_request).is_ok() {
        let mut byte = [0; 1];
        match client.read(&mut byte) {
            Ok(0) | Err(_) => {}
            Ok(_) => panic!("downstream connection was reused with unread request bytes"),
        }
    }
}

fn wait_for_proxy_log(proxy: &ProxyProcess, needle: &str) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if proxy.stderr_contents().contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "proxy log did not contain {needle}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_timestamped_log_entry(stderr: &str, target: &str, message: &str) {
    let line = stderr
        .lines()
        .find(|line| line.contains(message))
        .unwrap_or_else(|| panic!("missing log entry containing {message}: {stderr}"));
    let (header, _) = line
        .strip_prefix('[')
        .and_then(|line| line.split_once("] "))
        .unwrap_or_else(|| panic!("log entry has no formatted header: {line}"));
    let mut fields = header.split_ascii_whitespace();
    let timestamp = fields
        .next()
        .unwrap_or_else(|| panic!("log entry has no timestamp: {line}"));
    assert_utc_timestamp(timestamp);
    assert_eq!(fields.next(), Some("WARN"), "unexpected log level: {line}");
    assert_eq!(fields.next(), Some(target), "unexpected log target: {line}");
    assert_eq!(fields.next(), None, "unexpected log header: {line}");
}

fn assert_utc_timestamp(timestamp: &str) {
    assert_eq!(timestamp.len(), 20, "unexpected timestamp: {timestamp}");
    assert!(
        timestamp
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                10 => byte == b'T',
                13 | 16 => byte == b':',
                19 => byte == b'Z',
                _ => byte.is_ascii_digit(),
            }),
        "unexpected timestamp: {timestamp}"
    );
}

fn launch_attempts(proxy: &ProxyProcess) -> usize {
    proxy
        .stderr_contents()
        .matches("launchctl kickstart started for configured service")
        .count()
}

fn wait_for_file_lines(path: &Path, expected: usize) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if file_lines(path).len() >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} did not reach {expected} lines",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn file_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_for_listener(address: SocketAddr) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture listener did not become ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn append_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open fixture log");
    writeln!(file, "{line}").expect("write fixture log");
}
