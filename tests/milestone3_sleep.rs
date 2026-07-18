mod support;

use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{
    ProxyProcess, connect, read_exact_with_prefix, read_request_head, read_response,
    read_response_head, send_request, unused_loopback_address, write_response,
};

const WAIT_TIMEOUT: Duration = Duration::from_secs(12);
const SHORT_IDLE_SECONDS: u64 = 1;
const WAKE_SAFE_IDLE_SECONDS: u64 = 6;
const STREAM_SEGMENT_BYTES: usize = 128 * 1024;

const TARGET_ADDRESS_ENV: &str = "ROUSED_M3_TARGET_ADDRESS";
const TARGET_START_LOG_ENV: &str = "ROUSED_M3_TARGET_START_LOG";
const TARGET_SIGNAL_LOG_ENV: &str = "ROUSED_M3_TARGET_SIGNAL_LOG";
const TARGET_REQUEST_LOG_ENV: &str = "ROUSED_M3_TARGET_REQUEST_LOG";
const TARGET_UPLOAD_GATE_ENV: &str = "ROUSED_M3_TARGET_UPLOAD_GATE";
const TARGET_RESPONSE_GATE_ENV: &str = "ROUSED_M3_TARGET_RESPONSE_GATE";

const CHECK_MODE_ENV: &str = "ROUSED_M3_CHECK_MODE";
const CHECK_LOG_ENV: &str = "ROUSED_M3_CHECK_LOG";
const CHECK_ALLOW_ENV: &str = "ROUSED_M3_CHECK_ALLOW";
const CHECK_RELEASE_ENV: &str = "ROUSED_M3_CHECK_RELEASE";

static TERM_LOG_FD: AtomicI32 = AtomicI32::new(-1);
static UNIQUE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn long_upload_and_response_hold_the_service_lease_until_completion() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn(move |listen| {
        service_configuration(
            listen,
            "sleep.apps.test",
            upstream,
            &label,
            SHORT_IDLE_SECONDS,
            None,
        )
    });

    let upload_prefix = vec![b'U'; STREAM_SEGMENT_BYTES];
    let upload_suffix = vec![b'u'; STREAM_SEGMENT_BYTES];
    let total_upload = upload_prefix.len() + upload_suffix.len();
    let mut upload_client = connect(proxy.address()).expect("connect long-upload client");
    write!(
        upload_client,
        "POST /hold-upload HTTP/1.1\r\nHost: sleep.apps.test\r\nContent-Length: {total_upload}\r\n\r\n"
    )
    .expect("write long-upload request head");
    upload_client
        .write_all(&upload_prefix)
        .expect("write long-upload prefix");
    upload_client.flush().expect("flush long-upload prefix");
    wait_for_log_value(&target.request_log, "upload-prefix");

    thread::sleep(Duration::from_millis(1_300));
    target.assert_running_without_sigterm("long upload");
    fs::write(&target.upload_gate, b"release\n").expect("release long upload");
    upload_client
        .write_all(&upload_suffix)
        .expect("write long-upload suffix");
    upload_client.flush().expect("flush long-upload suffix");
    let upload_response = read_response(&mut upload_client).expect("read long-upload response");
    assert_eq!(upload_response.status, 200);
    assert_eq!(upload_response.body, b"upload complete\n");

    let mut response_client = connect(proxy.address()).expect("connect long-response client");
    response_client
        .write_all(b"GET /hold-response HTTP/1.1\r\nHost: sleep.apps.test\r\n\r\n")
        .expect("write long-response request");
    response_client
        .flush()
        .expect("flush long-response request");
    let response_head = read_response_head(&mut response_client).expect("read long-response head");
    assert_eq!(response_head.status, 200);
    let response_prefix = read_exact_with_prefix(
        &mut response_client,
        response_head.buffered_body,
        STREAM_SEGMENT_BYTES,
    )
    .expect("read long-response prefix");
    assert_eq!(response_prefix, vec![b'R'; STREAM_SEGMENT_BYTES]);

    thread::sleep(Duration::from_millis(1_300));
    target.assert_running_without_sigterm("long response");
    fs::write(&target.response_gate, b"release\n").expect("release long response");
    let response_suffix =
        read_exact_with_prefix(&mut response_client, Vec::new(), STREAM_SEGMENT_BYTES)
            .expect("read long-response suffix");
    assert_eq!(response_suffix, vec![b'r'; STREAM_SEGMENT_BYTES]);
    drop(response_client);

    wait_for_file_lines(&target.signal_log, 1, WAIT_TIMEOUT);
    assert_eq!(file_lines(&target.signal_log), ["SIGTERM"]);
    target.wait_until_stopped();
}

#[test]
fn client_disconnect_releases_a_long_response_lease() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn(move |listen| {
        service_configuration(
            listen,
            "sleep.apps.test",
            upstream,
            &label,
            SHORT_IDLE_SECONDS,
            None,
        )
    });

    let mut client = connect(proxy.address()).expect("connect disconnect client");
    client
        .write_all(b"GET /disconnect-response HTTP/1.1\r\nHost: sleep.apps.test\r\n\r\n")
        .expect("write disconnect request");
    client.flush().expect("flush disconnect request");
    let response = read_response_head(&mut client).expect("read disconnect response head");
    assert_eq!(response.status, 200);
    let prefix = read_exact_with_prefix(&mut client, response.buffered_body, STREAM_SEGMENT_BYTES)
        .expect("read disconnect response prefix");
    assert_eq!(prefix, vec![b'D'; STREAM_SEGMENT_BYTES]);

    thread::sleep(Duration::from_millis(1_300));
    target.assert_running_without_sigterm("connected response client");
    drop(client);

    wait_for_file_lines(&target.signal_log, 1, WAIT_TIMEOUT);
    assert_eq!(file_lines(&target.signal_log), ["SIGTERM"]);
    target.wait_until_stopped();
}

#[test]
fn idle_sigterm_is_followed_by_a_cold_wake_and_successful_retry() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    assert!(target.job_pid().is_none(), "RunAtLoad=false target started");

    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn(move |listen| {
        service_configuration(
            listen,
            "sleep.apps.test",
            upstream,
            &label,
            WAKE_SAFE_IDLE_SECONDS,
            None,
        )
    });

    let first_cold = request_ok(proxy.address(), "/first-cold");
    assert_eq!(first_cold.status, 503);
    target.wait_until_running(1);
    let first_retry = request_ok(proxy.address(), "/first-retry");
    assert_eq!(first_retry.status, 200);
    assert_eq!(first_retry.body, b"awake fixture\n");

    wait_for_file_lines(
        &target.signal_log,
        1,
        Duration::from_secs(WAKE_SAFE_IDLE_SECONDS + 5),
    );
    target.wait_until_stopped();
    assert_eq!(file_lines(&target.signal_log), ["SIGTERM"]);

    let second_cold = request_ok(proxy.address(), "/second-cold");
    assert_eq!(second_cold.status, 503);
    target.wait_until_running(2);
    let second_retry = request_ok(proxy.address(), "/second-retry");
    assert_eq!(second_retry.status, 200);
    assert_eq!(second_retry.body, b"awake fixture\n");
    assert_eq!(file_lines(&target.start_log), ["started", "started"]);
}

#[test]
fn veto_is_rate_limited_until_activity_then_allow_permits_sigterm() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let check_log = target.directory.path().join("state-check.log");
    let allow = target.directory.path().join("allow-stop");
    let environment = checker_environment("state", &check_log, Some(&allow), None);
    let checker = checker_command();
    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn_with_stderr_capture_and_environment(
        move |listen| {
            service_configuration(
                listen,
                "sleep.apps.test",
                upstream,
                &label,
                SHORT_IDLE_SECONDS,
                Some(&checker),
            )
        },
        environment,
    );

    assert_eq!(request_ok(proxy.address(), "/veto").status, 200);
    wait_for_checker_attempts(&check_log, 1);
    wait_for_log_value(&check_log, "veto");
    thread::sleep(Duration::from_secs(2));
    assert_eq!(checker_attempts(&check_log), 1);
    target.assert_running_without_sigterm("vetoed stop check");

    fs::write(&allow, b"allow\n").expect("allow stop check");
    assert_eq!(request_ok(proxy.address(), "/new-activity").status, 200);
    wait_for_checker_attempts(&check_log, 2);
    wait_for_log_value(&check_log, "allowed");
    wait_for_file_lines(&target.signal_log, 1, WAIT_TIMEOUT);
    target.wait_until_stopped();
}

#[test]
fn activity_during_a_stop_check_invalidates_its_allow_result() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let check_log = target.directory.path().join("blocked-check.log");
    let release = target.directory.path().join("release-check");
    let environment = checker_environment("blocked", &check_log, None, Some(&release));
    let checker = checker_command();
    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn_with_stderr_capture_and_environment(
        move |listen| {
            service_configuration(
                listen,
                "sleep.apps.test",
                upstream,
                &label,
                SHORT_IDLE_SECONDS,
                Some(&checker),
            )
        },
        environment,
    );

    assert_eq!(request_ok(proxy.address(), "/before-check").status, 200);
    wait_for_checker_attempts(&check_log, 1);
    assert_eq!(request_ok(proxy.address(), "/during-check").status, 200);
    fs::write(&release, b"release\n").expect("release blocked stop check");
    wait_for_log_value(&check_log, "allowed");

    thread::sleep(Duration::from_millis(400));
    target.assert_running_without_sigterm("activity during stop check");
}

#[test]
fn failing_stop_check_keeps_the_target_running() {
    assert_unsuccessful_checker_keeps_target("failure", "stop check vetoed shutdown");
}

#[test]
fn timed_out_stop_check_keeps_the_target_running() {
    assert_unsuccessful_checker_keeps_target("timeout", "stop check timed out");
}

#[test]
fn missing_stop_check_keeps_the_target_running() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let checker = vec![format!(
        "/definitely/missing/roused-m3-checker-{}",
        unique_token()
    )];
    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn_with_stderr_capture(move |listen| {
        service_configuration(
            listen,
            "sleep.apps.test",
            upstream,
            &label,
            SHORT_IDLE_SECONDS,
            Some(&checker),
        )
    });

    assert_eq!(request_ok(proxy.address(), "/missing-checker").status, 200);
    wait_for_proxy_log(&proxy, "stop check failed");
    target.assert_running_without_sigterm("missing stop checker");
}

#[test]
fn launchd_restarts_the_gateway_and_restart_gives_a_running_target_fresh_grace() {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);
    let target_pid = target.job_pid().expect("running target pid");

    let mut gateway = GatewayLaunchAgentFixture::new(&target, WAKE_SAFE_IDLE_SECONDS);
    gateway.bootstrap();
    gateway.wait_until_running();
    let old_gateway_pid = gateway.job_pid().expect("gateway pid before kill");

    let first = send_request(
        gateway.address,
        b"GET /before-gateway-restart HTTP/1.1\r\nHost: gateway.apps.test\r\n\r\n",
    )
    .expect("request through gateway LaunchAgent");
    assert_eq!(first.status, 200);
    let old_activity = Instant::now();

    thread::sleep(Duration::from_secs(3));
    gateway.kill();
    gateway.wait_for_different_pid(old_gateway_pid);
    let restart_observed = Instant::now();
    let old_deadline = old_activity + Duration::from_secs(WAKE_SAFE_IDLE_SECONDS + 1);
    let fresh_grace_sample = restart_observed + Duration::from_secs(1);
    wait_until(std::cmp::max(old_deadline, fresh_grace_sample));
    assert!(
        restart_observed.elapsed() < Duration::from_secs(WAKE_SAFE_IDLE_SECONDS),
        "gateway restart did not leave a meaningful fresh idle grace period"
    );
    assert_eq!(target.job_pid(), Some(target_pid));
    assert!(file_lines(&target.signal_log).is_empty());

    let after_restart = send_request(
        gateway.address,
        b"GET /after-gateway-restart HTTP/1.1\r\nHost: gateway.apps.test\r\n\r\n",
    )
    .expect("request through restarted gateway");
    assert_eq!(after_restart.status, 200);
    assert_eq!(file_lines(&target.start_log), ["started"]);
    assert!(
        !gateway
            .stderr_contents()
            .contains("launchctl kickstart started"),
        "already-running target was kickstarted after gateway restart"
    );
}

#[test]
fn launch_agent_templates_have_the_required_static_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gateway_path = root.join("packaging/launchd/roused-gateway.plist");
    let target_path = root.join("packaging/launchd/roused-target.plist");
    assert_plist_is_valid(&gateway_path);
    assert_plist_is_valid(&target_path);

    let gateway =
        normalized_xml(&fs::read_to_string(&gateway_path).expect("read gateway template"));
    assert!(gateway.contains("<key>Label</key> <string>net.example.roused</string>"));
    assert!(gateway.contains("<key>RunAtLoad</key> <true/>"));
    assert!(gateway.contains("<key>KeepAlive</key> <true/>"));
    assert!(gateway.contains(
        "<string>/ABSOLUTE/PATH/TO/roused</string> <string>/ABSOLUTE/PATH/TO/roused.toml</string>"
    ));

    let target = normalized_xml(&fs::read_to_string(&target_path).expect("read target template"));
    assert!(target.contains("<key>Label</key> <string>net.example.service</string>"));
    assert!(target.contains("<key>RunAtLoad</key> <false/>"));
    assert!(target.contains("<key>KeepAlive</key> <false/>"));
    assert!(target.contains("<string>/ABSOLUTE/PATH/TO/target-service</string>"));
}

#[test]
#[ignore = "entry point for the disposable Milestone 3 target LaunchAgent"]
fn target_launch_agent_child_entry() {
    let Ok(address) = env::var(TARGET_ADDRESS_ENV) else {
        return;
    };
    let address = address.parse::<SocketAddr>().expect("parse target address");
    let start_log = required_env_path(TARGET_START_LOG_ENV);
    let signal_log = required_env_path(TARGET_SIGNAL_LOG_ENV);
    let request_log = required_env_path(TARGET_REQUEST_LOG_ENV);
    let upload_gate = required_env_path(TARGET_UPLOAD_GATE_ENV);
    let response_gate = required_env_path(TARGET_RESPONSE_GATE_ENV);
    install_sigterm_recorder(&signal_log);
    append_line(&start_log, "started");

    let listener = TcpListener::bind(address).expect("bind target fixture listener");
    loop {
        let (mut stream, _) = listener.accept().expect("accept target connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set target read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .expect("set target write timeout");
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
            Err(error) => panic!("target peek failed: {error}"),
        }

        let head = read_request_head(&mut stream).expect("read target request head");
        append_line(&request_log, &head.target);
        match head.target.as_str() {
            "/hold-upload" => {
                let length = content_length(&head.headers);
                assert_eq!(length, STREAM_SEGMENT_BYTES * 2);
                let prefix =
                    read_exact_with_prefix(&mut stream, head.buffered_body, STREAM_SEGMENT_BYTES)
                        .expect("read upload prefix");
                assert_eq!(prefix, vec![b'U'; STREAM_SEGMENT_BYTES]);
                append_line(&request_log, "upload-prefix");
                wait_for_path(&upload_gate, Duration::from_secs(30));
                let suffix = read_exact_with_prefix(&mut stream, Vec::new(), STREAM_SEGMENT_BYTES)
                    .expect("read upload suffix");
                assert_eq!(suffix, vec![b'u'; STREAM_SEGMENT_BYTES]);
                write_response(
                    &mut stream,
                    "200 OK",
                    [("Connection", "close")],
                    b"upload complete\n",
                )
                .expect("write upload response");
            }
            "/hold-response" => {
                let total = STREAM_SEGMENT_BYTES * 2;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                )
                .expect("write held response head");
                stream
                    .write_all(&vec![b'R'; STREAM_SEGMENT_BYTES])
                    .expect("write held response prefix");
                stream.flush().expect("flush held response prefix");
                append_line(&request_log, "response-prefix");
                wait_for_path(&response_gate, Duration::from_secs(30));
                stream
                    .write_all(&vec![b'r'; STREAM_SEGMENT_BYTES])
                    .expect("write held response suffix");
                stream.flush().expect("flush held response suffix");
            }
            "/disconnect-response" => {
                let total = STREAM_SEGMENT_BYTES + 32 * 1024 * 1024;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                )
                .expect("write disconnect response head");
                stream
                    .write_all(&vec![b'D'; STREAM_SEGMENT_BYTES])
                    .expect("write disconnect response prefix");
                stream.flush().expect("flush disconnect response prefix");
                append_line(&request_log, "disconnect-prefix");
                let chunk = [b'd'; 16 * 1024];
                loop {
                    thread::sleep(Duration::from_millis(25));
                    if stream
                        .write_all(&chunk)
                        .and_then(|()| stream.flush())
                        .is_err()
                    {
                        append_line(&request_log, "upstream-closed");
                        break;
                    }
                }
            }
            _ => {
                write_response(
                    &mut stream,
                    "200 OK",
                    [("Content-Type", "text/plain"), ("Connection", "close")],
                    b"awake fixture\n",
                )
                .expect("write target response");
            }
        }
    }
}

#[test]
#[ignore = "entry point for the direct-argv Milestone 3 stop checker"]
fn can_stop_checker_child_entry() {
    let Ok(mode) = env::var(CHECK_MODE_ENV) else {
        return;
    };
    let log = required_env_path(CHECK_LOG_ENV);
    append_line(&log, "started");
    match mode.as_str() {
        "state" => {
            let allow = required_env_path(CHECK_ALLOW_ENV);
            if allow.exists() {
                append_line(&log, "allowed");
            } else {
                append_line(&log, "veto");
                std::process::exit(1);
            }
        }
        "blocked" => {
            let release = required_env_path(CHECK_RELEASE_ENV);
            wait_for_path(&release, Duration::from_secs(30));
            append_line(&log, "allowed");
        }
        "failure" => panic!("deliberate checker failure"),
        "timeout" => thread::sleep(Duration::from_secs(30)),
        other => panic!("unknown checker mode {other}"),
    }
}

struct TargetLaunchAgentFixture {
    directory: tempfile::TempDir,
    label: String,
    user_id: u32,
    address: SocketAddr,
    plist: PathBuf,
    start_log: PathBuf,
    signal_log: PathBuf,
    request_log: PathBuf,
    upload_gate: PathBuf,
    response_gate: PathBuf,
    bootstrapped: bool,
}

impl TargetLaunchAgentFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("create target fixture directory");
        let user_id = directory
            .path()
            .metadata()
            .expect("read target fixture metadata")
            .uid();
        let label = format!("com.openai.roused.test.sleep.{}", unique_token());
        let address = unused_loopback_address();
        let plist = directory.path().join("target.plist");
        let start_log = directory.path().join("starts.log");
        let signal_log = directory.path().join("signals.log");
        let request_log = directory.path().join("requests.log");
        let upload_gate = directory.path().join("release-upload");
        let response_gate = directory.path().join("release-response");
        let stdout_log = directory.path().join("target.stdout");
        let stderr_log = directory.path().join("target.stderr");
        let executable = env::current_exe().expect("resolve Milestone 3 test executable");
        fs::write(
            &plist,
            target_launch_agent_plist(
                &label,
                &executable,
                address,
                &start_log,
                &signal_log,
                &request_log,
                &upload_gate,
                &response_gate,
                &stdout_log,
                &stderr_log,
            ),
        )
        .expect("write target fixture plist");
        Self {
            directory,
            label,
            user_id,
            address,
            plist,
            start_log,
            signal_log,
            request_log,
            upload_gate,
            response_gate,
            bootstrapped: false,
        }
    }

    fn bootstrap(&mut self) {
        let domain = format!("gui/{}", self.user_id);
        let output = Command::new("/bin/launchctl")
            .args(["bootstrap", domain.as_str()])
            .arg(&self.plist)
            .output()
            .expect("bootstrap target fixture");
        assert_launchctl_success("target bootstrap", &output);
        self.bootstrapped = true;
    }

    fn kickstart(&self) {
        let target = self.launchctl_target();
        let output = Command::new("/bin/launchctl")
            .args(["kickstart", target.as_str()])
            .output()
            .expect("kickstart target fixture");
        assert_launchctl_success("target kickstart", &output);
    }

    fn wait_until_running(&self, expected_starts: usize) {
        wait_for_file_lines(&self.start_log, expected_starts, WAIT_TIMEOUT);
        wait_for_listener(self.address, WAIT_TIMEOUT);
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while self.job_pid().is_none() {
            assert!(Instant::now() < deadline, "target job has no running pid");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_until_stopped(&self) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while self.job_pid().is_some() {
            assert!(Instant::now() < deadline, "target job did not stop");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_running_without_sigterm(&self, context: &str) {
        assert!(
            file_lines(&self.signal_log).is_empty(),
            "target received SIGTERM during {context}"
        );
        assert!(self.job_pid().is_some(), "target stopped during {context}");
    }

    fn job_pid(&self) -> Option<u32> {
        launchctl_job_pid(&self.launchctl_target())
    }

    fn launchctl_target(&self) -> String {
        format!("gui/{}/{}", self.user_id, self.label)
    }
}

impl Drop for TargetLaunchAgentFixture {
    fn drop(&mut self) {
        if self.bootstrapped {
            let target = self.launchctl_target();
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", target.as_str()])
                .status();
        }
    }
}

struct GatewayLaunchAgentFixture {
    _directory: tempfile::TempDir,
    label: String,
    user_id: u32,
    address: SocketAddr,
    plist: PathBuf,
    stderr_log: PathBuf,
    bootstrapped: bool,
}

impl GatewayLaunchAgentFixture {
    fn new(target: &TargetLaunchAgentFixture, idle_timeout_seconds: u64) -> Self {
        let directory = tempfile::tempdir().expect("create gateway fixture directory");
        let user_id = directory
            .path()
            .metadata()
            .expect("read gateway fixture metadata")
            .uid();
        let label = format!("com.openai.roused.test.gateway.{}", unique_token());
        let address = unused_loopback_address();
        let configuration_path = directory.path().join("roused.toml");
        fs::write(
            &configuration_path,
            service_configuration(
                address,
                "gateway.apps.test",
                target.address,
                &target.label,
                idle_timeout_seconds,
                None,
            ),
        )
        .expect("write gateway fixture configuration");

        let plist = directory.path().join("gateway.plist");
        let stdout_log = directory.path().join("gateway.stdout");
        let stderr_log = directory.path().join("gateway.stderr");
        let binary = Path::new(env!("CARGO_BIN_EXE_roused"));
        let template_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/launchd/roused-gateway.plist");
        let template =
            fs::read_to_string(template_path).expect("read gateway LaunchAgent template");
        let rendered = render_gateway_template(
            &template,
            &label,
            binary,
            &configuration_path,
            &stdout_log,
            &stderr_log,
        );
        fs::write(&plist, rendered).expect("write gateway fixture plist");

        Self {
            _directory: directory,
            label,
            user_id,
            address,
            plist,
            stderr_log,
            bootstrapped: false,
        }
    }

    fn bootstrap(&mut self) {
        let domain = format!("gui/{}", self.user_id);
        let output = Command::new("/bin/launchctl")
            .args(["bootstrap", domain.as_str()])
            .arg(&self.plist)
            .output()
            .expect("bootstrap gateway fixture");
        assert_launchctl_success("gateway bootstrap", &output);
        self.bootstrapped = true;
    }

    fn wait_until_running(&self) {
        wait_for_listener(self.address, WAIT_TIMEOUT);
        let deadline = Instant::now() + WAIT_TIMEOUT;
        while self.job_pid().is_none() {
            assert!(Instant::now() < deadline, "gateway job has no running pid");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_different_pid(&self, old_pid: u32) {
        let deadline = Instant::now() + WAIT_TIMEOUT;
        loop {
            if self.job_pid().is_some_and(|pid| pid != old_pid)
                && TcpStream::connect_timeout(&self.address, Duration::from_millis(50)).is_ok()
            {
                return;
            }
            assert!(Instant::now() < deadline, "launchd did not restart gateway");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(&self) {
        let target = self.launchctl_target();
        // This force-kill is only the test stimulus proving gateway KeepAlive
        // restart. Roused target shutdown remains exactly one SIGTERM.
        let output = Command::new("/bin/launchctl")
            .args(["kill", "SIGKILL", target.as_str()])
            .output()
            .expect("kill gateway fixture");
        assert_launchctl_success("gateway kill", &output);
    }

    fn job_pid(&self) -> Option<u32> {
        launchctl_job_pid(&self.launchctl_target())
    }

    fn stderr_contents(&self) -> String {
        fs::read_to_string(&self.stderr_log).unwrap_or_default()
    }

    fn launchctl_target(&self) -> String {
        format!("gui/{}/{}", self.user_id, self.label)
    }
}

impl Drop for GatewayLaunchAgentFixture {
    fn drop(&mut self) {
        if self.bootstrapped {
            let target = self.launchctl_target();
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", target.as_str()])
                .status();
        }
    }
}

fn assert_unsuccessful_checker_keeps_target(mode: &str, expected_log: &str) {
    let mut target = TargetLaunchAgentFixture::new();
    target.bootstrap();
    target.kickstart();
    target.wait_until_running(1);

    let check_log = target.directory.path().join(format!("{mode}-check.log"));
    let environment = checker_environment(mode, &check_log, None, None);
    let checker = checker_command();
    let upstream = target.address;
    let label = target.label.clone();
    let proxy = ProxyProcess::spawn_with_stderr_capture_and_environment(
        move |listen| {
            service_configuration(
                listen,
                "sleep.apps.test",
                upstream,
                &label,
                SHORT_IDLE_SECONDS,
                Some(&checker),
            )
        },
        environment,
    );
    assert_eq!(request_ok(proxy.address(), "/checker-failure").status, 200);
    wait_for_checker_attempts(&check_log, 1);
    wait_for_proxy_log(&proxy, expected_log);
    target.assert_running_without_sigterm(mode);
}

fn request_ok(address: SocketAddr, path: &str) -> support::RawResponse {
    let request = format!("GET {path} HTTP/1.1\r\nHost: sleep.apps.test\r\n\r\n");
    send_request(address, request.as_bytes()).expect("send target request")
}

fn checker_command() -> Vec<String> {
    vec![
        env::current_exe()
            .expect("resolve checker test executable")
            .display()
            .to_string(),
        "--ignored".to_owned(),
        "--exact".to_owned(),
        "can_stop_checker_child_entry".to_owned(),
        "--nocapture".to_owned(),
    ]
}

fn checker_environment(
    mode: &str,
    log: &Path,
    allow: Option<&Path>,
    release: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let mut environment = vec![
        (OsString::from(CHECK_MODE_ENV), OsString::from(mode)),
        (
            OsString::from(CHECK_LOG_ENV),
            log.as_os_str().to_os_string(),
        ),
    ];
    if let Some(allow) = allow {
        environment.push((
            OsString::from(CHECK_ALLOW_ENV),
            allow.as_os_str().to_os_string(),
        ));
    }
    if let Some(release) = release {
        environment.push((
            OsString::from(CHECK_RELEASE_ENV),
            release.as_os_str().to_os_string(),
        ));
    }
    environment
}

fn service_configuration(
    listen: SocketAddr,
    host: &str,
    upstream: SocketAddr,
    launchd_label: &str,
    idle_timeout_seconds: u64,
    checker: Option<&[String]>,
) -> String {
    let mut configuration = format!(
        "listen = {}\n\n[[services]]\nhost = {}\nupstream = {}\nlaunchd_label = {}\nidle_timeout_seconds = {idle_timeout_seconds}\n",
        toml_string(&listen.to_string()),
        toml_string(host),
        toml_string(&upstream.to_string()),
        toml_string(launchd_label),
    );
    if let Some(checker) = checker {
        let checker = checker
            .iter()
            .map(|argument| toml_string(argument))
            .collect::<Vec<_>>()
            .join(", ");
        configuration.push_str(&format!("can_stop_command = [{checker}]\n"));
    }
    configuration
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[allow(clippy::too_many_arguments)]
fn target_launch_agent_plist(
    label: &str,
    executable: &Path,
    address: SocketAddr,
    start_log: &Path,
    signal_log: &Path,
    request_log: &Path,
    upload_gate: &Path,
    response_gate: &Path,
    stdout_log: &Path,
    stderr_log: &Path,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>--ignored</string>\n    <string>--exact</string>\n    <string>target_launch_agent_child_entry</string>\n    <string>--nocapture</string>\n  </array>\n  <key>EnvironmentVariables</key>\n  <dict>\n    <key>{TARGET_ADDRESS_ENV}</key><string>{address}</string>\n    <key>{TARGET_START_LOG_ENV}</key><string>{}</string>\n    <key>{TARGET_SIGNAL_LOG_ENV}</key><string>{}</string>\n    <key>{TARGET_REQUEST_LOG_ENV}</key><string>{}</string>\n    <key>{TARGET_UPLOAD_GATE_ENV}</key><string>{}</string>\n    <key>{TARGET_RESPONSE_GATE_ENV}</key><string>{}</string>\n  </dict>\n  <key>RunAtLoad</key><false/>\n  <key>KeepAlive</key><false/>\n  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n</dict>\n</plist>\n",
        xml_escape(label),
        xml_escape(&executable.display().to_string()),
        xml_escape(&start_log.display().to_string()),
        xml_escape(&signal_log.display().to_string()),
        xml_escape(&request_log.display().to_string()),
        xml_escape(&upload_gate.display().to_string()),
        xml_escape(&response_gate.display().to_string()),
        xml_escape(&stdout_log.display().to_string()),
        xml_escape(&stderr_log.display().to_string()),
    )
}

fn render_gateway_template(
    template: &str,
    label: &str,
    binary: &Path,
    configuration: &Path,
    stdout_log: &Path,
    stderr_log: &Path,
) -> String {
    let rendered = template
        .replace("net.example.roused", &xml_escape(label))
        .replace(
            "/ABSOLUTE/PATH/TO/roused.toml",
            &xml_escape(&configuration.display().to_string()),
        )
        .replace(
            "/ABSOLUTE/PATH/TO/roused",
            &xml_escape(&binary.display().to_string()),
        );
    let logging = format!(
        "  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n",
        xml_escape(&stdout_log.display().to_string()),
        xml_escape(&stderr_log.display().to_string()),
    );
    rendered.replacen("</dict>", &format!("{logging}</dict>"), 1)
}

fn install_sigterm_recorder(path: &Path) {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open SIGTERM log");
    TERM_LOG_FD.store(file.as_raw_fd(), Ordering::Release);
    // SAFETY: the handler has the C ABI and only calls async-signal-safe libc functions.
    let previous = unsafe {
        libc::signal(
            libc::SIGTERM,
            record_sigterm as *const () as libc::sighandler_t,
        )
    };
    assert_ne!(previous, libc::SIG_ERR, "install SIGTERM handler");
    std::mem::forget(file);
}

extern "C" fn record_sigterm(_signal: libc::c_int) {
    let message = b"SIGTERM\n";
    let fd = TERM_LOG_FD.load(Ordering::Acquire);
    if fd >= 0 {
        // SAFETY: fd refers to the pre-opened append-only fixture log and the buffer is valid.
        unsafe {
            libc::write(fd, message.as_ptr().cast(), message.len());
        }
    }
    // SAFETY: immediate process exit is async-signal-safe and avoids running test destructors.
    unsafe { libc::_exit(0) }
}

fn launchctl_job_pid(target: &str) -> Option<u32> {
    let output = Command::new("/bin/launchctl")
        .args(["print", target])
        .output()
        .expect("print fixture launchd job");
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = ")?.parse().ok())
}

fn assert_launchctl_success(operation: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_listener(address: SocketAddr, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "fixture listener did not start");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_file_lines(path: &Path, expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
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

fn wait_for_checker_attempts(path: &Path, expected: usize) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if checker_attempts(path) >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "checker did not run {expected} times"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn checker_attempts(path: &Path) -> usize {
    file_lines(path)
        .iter()
        .filter(|line| line.as_str() == "started")
        .count()
}

fn wait_for_log_value(path: &Path, expected: &str) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if file_lines(path).iter().any(|line| line == expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} did not contain {expected}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_proxy_log(proxy: &ProxyProcess, expected: &str) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if proxy.stderr_contents().contains(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "proxy log did not contain {expected}"
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

fn append_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open fixture log");
    writeln!(file, "{line}").expect("append fixture log");
}

fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until(deadline: Instant) {
    if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        thread::sleep(remaining);
    }
}

fn content_length(headers: &[(String, String)]) -> usize {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .expect("request Content-Length")
        .1
        .parse()
        .expect("numeric Content-Length")
}

fn required_env_path(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("missing {name}")))
}

fn unique_token() -> String {
    format!(
        "{}.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos(),
        UNIQUE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
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

fn normalized_xml(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn assert_plist_is_valid(path: &Path) {
    let output = Command::new("/usr/bin/plutil")
        .args(["-lint"])
        .arg(path)
        .output()
        .expect("run plutil");
    assert!(
        output.status.success(),
        "{} is not a valid plist: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}
