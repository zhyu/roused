mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{
    ProxyProcess, RawServer, connect, read_exact_with_prefix, read_request, read_request_head,
    read_response, read_response_head, run_roused_to_exit, send_request, write_response,
};

const CHANNEL_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn routes_two_hosts_and_preserves_http_semantics() {
    let (alpha_sender, alpha_receiver) = mpsc::channel();
    let alpha = RawServer::spawn(move |mut stream| {
        let request = read_request(&mut stream).expect("read alpha request");
        let (status, body): (&str, &[u8]) = if request.method == "POST" {
            ("207 Multi-Status", &request.body)
        } else {
            ("206 Partial Content", b"alpha fixture")
        };
        write_response(
            &mut stream,
            status,
            [
                ("X-Fixture", "alpha"),
                ("X-Origin-End-To-End", "origin-alpha"),
                ("Connection", "close"),
            ],
            body,
        )
        .expect("write alpha response");
        alpha_sender.send(request).expect("record alpha request");
    });

    let (beta_sender, beta_receiver) = mpsc::channel();
    let beta = RawServer::spawn(move |mut stream| {
        let request = read_request(&mut stream).expect("read beta request");
        write_response(
            &mut stream,
            "200 OK",
            [("X-Fixture", "beta"), ("Connection", "close")],
            b"beta fixture",
        )
        .expect("write beta response");
        beta_sender.send(request).expect("record beta request");
    });

    let alpha_address = alpha.address();
    let beta_address = beta.address();
    let proxy = ProxyProcess::spawn(|listen| {
        proxy_configuration(
            listen,
            &[
                ("alpha.apps.test", alpha_address, "net.test.alpha"),
                ("beta.apps.test", beta_address, "net.test.beta"),
            ],
        )
    });

    let original_host = format!("AlPhA.ApPs.TeSt.:{}", proxy.address().port());
    let normalized_request = format!(
        "GET /items/list?kind=all&limit=2 HTTP/1.1\r\nHost: {original_host}\r\nX-Client-End-To-End: client-alpha\r\n\r\n"
    );
    let normalized_response = send_request(proxy.address(), normalized_request.as_bytes())
        .expect("request normalized alpha host");
    assert_eq!(normalized_response.status, 206);
    assert_eq!(normalized_response.body, b"alpha fixture");
    assert_eq!(normalized_response.header("x-fixture"), Some("alpha"));
    assert_eq!(
        normalized_response.header("x-origin-end-to-end"),
        Some("origin-alpha")
    );
    let observed_get = alpha_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("alpha fixture observed GET");
    assert_eq!(observed_get.method, "GET");
    assert_eq!(observed_get.target, "/items/list?kind=all&limit=2");
    assert_eq!(observed_get.header("host"), Some(original_host.as_str()));
    assert_eq!(
        observed_get.header("x-client-end-to-end"),
        Some("client-alpha")
    );
    assert!(observed_get.body.is_empty());

    let mixed_case_response = send_request(
        proxy.address(),
        b"GET /mixed-case-only HTTP/1.1\r\nHost: aLpHa.ApPs.TeSt\r\n\r\n",
    )
    .expect("request mixed-case alpha host");
    assert_eq!(mixed_case_response.status, 206);
    let observed_mixed_case = alpha_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("alpha fixture observed mixed-case host");
    assert_eq!(observed_mixed_case.target, "/mixed-case-only");
    assert_eq!(observed_mixed_case.header("host"), Some("aLpHa.ApPs.TeSt"));

    let terminal_dot_response = send_request(
        proxy.address(),
        b"GET /terminal-dot-only HTTP/1.1\r\nHost: alpha.apps.test.\r\n\r\n",
    )
    .expect("request terminal-dot alpha host");
    assert_eq!(terminal_dot_response.status, 206);
    let observed_terminal_dot = alpha_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("alpha fixture observed terminal-dot host");
    assert_eq!(observed_terminal_dot.target, "/terminal-dot-only");
    assert_eq!(
        observed_terminal_dot.header("host"),
        Some("alpha.apps.test.")
    );

    let beta_response = send_request(
        proxy.address(),
        b"GET /from-beta HTTP/1.1\r\nHost: beta.apps.test\r\n\r\n",
    )
    .expect("request beta host");
    assert_eq!(beta_response.status, 200);
    assert_eq!(beta_response.body, b"beta fixture");
    assert_eq!(beta_response.header("x-fixture"), Some("beta"));
    let observed_beta = beta_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("beta fixture observed GET");
    assert_eq!(observed_beta.method, "GET");
    assert_eq!(observed_beta.target, "/from-beta");
    assert_eq!(observed_beta.header("host"), Some("beta.apps.test"));

    let post_body = b"opaque post body forwarded once";
    let post_request = format!(
        "POST /submit/jobs?mode=opaque HTTP/1.1\r\nHost: alpha.apps.test\r\nContent-Type: application/octet-stream\r\nX-Client-End-To-End: client-post\r\nContent-Length: {}\r\n\r\n",
        post_body.len()
    );
    let mut post_wire = post_request.into_bytes();
    post_wire.extend_from_slice(post_body);
    let post_response = send_request(proxy.address(), &post_wire).expect("send POST through proxy");
    assert_eq!(post_response.status, 207);
    assert_eq!(post_response.body, post_body);
    assert_eq!(post_response.header("x-fixture"), Some("alpha"));
    let observed_post = alpha_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("alpha fixture observed POST");
    assert_eq!(observed_post.method, "POST");
    assert_eq!(observed_post.target, "/submit/jobs?mode=opaque");
    assert_eq!(
        observed_post.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(
        observed_post.header("x-client-end-to-end"),
        Some("client-post")
    );
    assert_eq!(observed_post.body, post_body);

    let unknown = send_request(
        proxy.address(),
        b"GET /anything HTTP/1.1\r\nHost: missing.apps.test\r\n\r\n",
    )
    .expect("request unknown host");
    assert_eq!(unknown.status, 421);
    assert_eq!(
        unknown.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(unknown.header("cache-control"), Some("no-store"));
    assert_eq!(unknown.header("content-length"), Some("13"));
    assert_eq!(unknown.body, b"unknown host\n");
}

#[test]
fn preserves_authentication_and_removes_bounded_hop_headers() {
    let basic_password = runtime_token("basic-password");
    let basic_authorization = format!(
        "Basic {}",
        base64(format!("fixture:{basic_password}").as_bytes())
    );
    let bearer_authorization = format!("Bearer {}", runtime_token("bearer"));
    let cookie = format!("session={}; preference=opaque", runtime_token("cookie"));
    let proxy_authorization = format!("Basic {}", base64(runtime_token("proxy-auth").as_bytes()));
    let query_token = runtime_token("query");
    let body_token = runtime_token("body");
    let end_to_end_request = runtime_token("request-field");
    let authentication_info = format!("nextnonce=\"{}\"", runtime_token("nextnonce"));
    let first_set_cookie = format!("session={}; Path=/; HttpOnly", runtime_token("set-cookie"));
    let second_set_cookie = format!("secure={}; Path=/; Secure", runtime_token("secure-cookie"));
    let end_to_end_response = runtime_token("response-field");

    let fixture_basic = basic_authorization.clone();
    let fixture_bearer = bearer_authorization.clone();
    let fixture_cookie = cookie.clone();
    let fixture_authentication_info = authentication_info.clone();
    let fixture_first_cookie = first_set_cookie.clone();
    let fixture_second_cookie = second_set_cookie.clone();
    let fixture_response_field = end_to_end_response.clone();
    let (request_sender, request_receiver) = mpsc::channel();
    let auth_fixture = RawServer::spawn(move |mut stream| {
        let request = read_request(&mut stream).expect("read authentication request");
        if request.target.starts_with("/auth/basic") {
            let accepted = request.header("authorization") == Some(fixture_basic.as_str())
                && request.header("cookie") == Some(fixture_cookie.as_str())
                && !request.has_header("proxy-authorization")
                && !request.has_header("x-request-hop");
            if accepted {
                write_response(
                    &mut stream,
                    "401 Unauthorized",
                    [
                        ("WWW-Authenticate", "Basic realm=\"fixture\""),
                        ("Authentication-Info", fixture_authentication_info.as_str()),
                        ("Set-Cookie", fixture_first_cookie.as_str()),
                        ("Set-Cookie", fixture_second_cookie.as_str()),
                        ("Location", "/login?return=%2Fauth%2Fbasic"),
                        ("X-Origin-End-To-End", fixture_response_field.as_str()),
                        ("Connection", "close, X-Response-Hop"),
                        ("X-Response-Hop", "remove-me"),
                        ("Proxy-Connection", "remove-me"),
                        ("Keep-Alive", "timeout=5"),
                        ("Proxy-Authenticate", "remove-me"),
                        ("TE", "trailers"),
                        ("Trailer", "X-Response-Trailer"),
                        ("Upgrade", "h2c"),
                    ],
                    b"authentication required",
                )
                .expect("write authentication challenge");
            } else {
                write_response(
                    &mut stream,
                    "500 Fixture-Rejected",
                    [("Connection", "close")],
                    b"fixture rejected headers",
                )
                .expect("write fixture rejection");
            }
        } else {
            let accepted = request.header("authorization") == Some(fixture_bearer.as_str())
                && request.header("cookie") == Some(fixture_cookie.as_str())
                && !request.has_header("proxy-authorization");
            let status = if accepted {
                "200 OK"
            } else {
                "500 Fixture-Rejected"
            };
            write_response(
                &mut stream,
                status,
                [("Connection", "close")],
                b"bearer accepted",
            )
            .expect("write bearer response");
        }
        request_sender
            .send(request)
            .expect("record authentication request");
    });

    let auth_address = auth_fixture.address();
    let proxy = ProxyProcess::spawn(|listen| {
        proxy_configuration(listen, &[("auth.apps.test", auth_address, "net.test.auth")])
    });

    let basic_target = format!("/auth/basic?access_token={query_token}");
    let basic_body = format!("body-token={body_token}");
    let basic_head = format!(
        "POST {basic_target} HTTP/1.1\r\nHost: AuTh.ApPs.TeSt\r\nAuthorization: {basic_authorization}\r\nCookie: {cookie}\r\nProxy-Authorization: {proxy_authorization}\r\nConnection: X-Request-Hop\r\nX-Request-Hop: remove-me\r\nProxy-Connection: keep-alive\r\nKeep-Alive: timeout=5\r\nProxy-Authenticate: remove-me\r\nTE: trailers\r\nTrailer: X-Request-Trailer\r\nUpgrade: h2c\r\nX-Client-End-To-End: {end_to_end_request}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    let basic_wire = format!(
        "{basic_head}{:X}\r\n{basic_body}\r\n0\r\n\r\n",
        basic_body.len()
    );
    let basic_response = send_request(proxy.address(), basic_wire.as_bytes())
        .expect("send Basic authentication request");
    assert_eq!(basic_response.status, 401);
    assert_eq!(
        basic_response.header("www-authenticate"),
        Some("Basic realm=\"fixture\"")
    );
    assert!(
        basic_response.header("authentication-info") == Some(authentication_info.as_str()),
        "Authentication-Info was not preserved"
    );
    assert_eq!(
        basic_response.header("location"),
        Some("/login?return=%2Fauth%2Fbasic")
    );
    assert!(
        basic_response.header("x-origin-end-to-end") == Some(end_to_end_response.as_str()),
        "end-to-end response header was not preserved"
    );
    let set_cookies = basic_response
        .header_values("set-cookie")
        .collect::<Vec<_>>();
    assert_eq!(
        set_cookies.len(),
        2,
        "Set-Cookie fields were combined or lost"
    );
    assert!(
        set_cookies.iter().any(|value| **value == first_set_cookie),
        "first Set-Cookie field was not preserved"
    );
    assert!(
        set_cookies.iter().any(|value| **value == second_set_cookie),
        "second Set-Cookie field was not preserved"
    );
    for removed in [
        "x-response-hop",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "te",
        "trailer",
        "upgrade",
    ] {
        assert!(
            !basic_response.has_header(removed),
            "bounded response field {removed} was forwarded"
        );
    }
    assert!(
        !basic_response.header_values("connection").any(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("x-response-hop"))
        }),
        "a Connection-named response field was forwarded"
    );

    let observed_basic = request_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("fixture observed Basic request");
    assert_eq!(observed_basic.method, "POST");
    assert!(
        observed_basic.target == basic_target,
        "query authentication material was not preserved"
    );
    assert_eq!(observed_basic.header("host"), Some("AuTh.ApPs.TeSt"));
    assert!(
        observed_basic.header("authorization") == Some(basic_authorization.as_str()),
        "Basic Authorization was not preserved"
    );
    assert!(
        observed_basic.header("cookie") == Some(cookie.as_str()),
        "Cookie was not preserved"
    );
    assert!(
        observed_basic.header("x-client-end-to-end") == Some(end_to_end_request.as_str()),
        "end-to-end request header was not preserved"
    );
    assert!(
        observed_basic.body == basic_body.as_bytes(),
        "body authentication material was not preserved"
    );
    for removed in [
        "proxy-authorization",
        "connection",
        "x-request-hop",
        "proxy-connection",
        "keep-alive",
        "proxy-authenticate",
        "te",
        "trailer",
        "upgrade",
    ] {
        assert!(
            !observed_basic.has_header(removed),
            "bounded request field {removed} was forwarded"
        );
    }
    assert_eq!(
        observed_basic.header("transfer-encoding"),
        Some("chunked"),
        "Pingora did not regenerate upstream request framing"
    );

    let bearer_request = format!(
        "GET /auth/bearer HTTP/1.1\r\nHost: auth.apps.test\r\nAuthorization: {bearer_authorization}\r\nCookie: {cookie}\r\nProxy-Authorization: {proxy_authorization}\r\n\r\n"
    );
    let bearer_response = send_request(proxy.address(), bearer_request.as_bytes())
        .expect("send Bearer authentication request");
    assert_eq!(bearer_response.status, 200);
    assert_eq!(bearer_response.body, b"bearer accepted");
    let observed_bearer = request_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("fixture observed Bearer request");
    assert!(
        observed_bearer.header("authorization") == Some(bearer_authorization.as_str()),
        "Bearer Authorization was not preserved"
    );
    assert!(
        observed_bearer.header("cookie") == Some(cookie.as_str()),
        "Bearer request Cookie was not preserved"
    );
    assert!(!observed_bearer.has_header("proxy-authorization"));
}

#[test]
fn streams_request_and_response_bodies_before_completion() {
    let request_prefix = b"request-prefix-arrives-first/".to_vec();
    let request_suffix = b"request-suffix-arrives-later".to_vec();
    // Pingora's HTTP/1 writer batches small writes. Each half is deliberately
    // larger than that transport buffer so observing the first half still
    // proves the proxy did not wait for the complete response.
    let response_prefix = vec![b'R'; 128 * 1024];
    let response_suffix = vec![b'S'; 128 * 1024];

    let fixture_request_prefix = request_prefix.clone();
    let fixture_request_suffix = request_suffix.clone();
    let fixture_response_prefix = response_prefix.clone();
    let fixture_response_suffix = response_suffix.clone();
    let (prefix_seen_sender, prefix_seen_receiver) = mpsc::channel();
    let (request_body_sender, request_body_receiver) = mpsc::channel();
    let (release_response_sender, release_response_receiver) = mpsc::channel();
    let streaming_fixture = RawServer::spawn(move |mut stream| {
        let head = read_request_head(&mut stream).expect("read streaming request head");
        if head.target == "/stream-request" {
            let mut body = read_exact_with_prefix(
                &mut stream,
                head.buffered_body,
                fixture_request_prefix.len(),
            )
            .expect("read first request segment");
            prefix_seen_sender
                .send(())
                .expect("signal first request segment");
            let suffix =
                read_exact_with_prefix(&mut stream, Vec::new(), fixture_request_suffix.len())
                    .expect("read second request segment");
            body.extend_from_slice(&suffix);
            request_body_sender
                .send(body)
                .expect("record streamed request body");
            write_response(
                &mut stream,
                "200 OK",
                [("Connection", "close")],
                b"request streamed",
            )
            .expect("write request streaming response");
        } else {
            let expected_length = fixture_response_prefix.len() + fixture_response_suffix.len();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {expected_length}\r\nConnection: close\r\n\r\n"
            )
            .expect("write streaming response head");
            stream
                .write_all(&fixture_response_prefix)
                .expect("write first response segment");
            stream.flush().expect("flush first response segment");
            release_response_receiver
                .recv_timeout(CHANNEL_TIMEOUT)
                .expect("client releases second response segment");
            stream
                .write_all(&fixture_response_suffix)
                .expect("write second response segment");
            stream.flush().expect("flush second response segment");
        }
    });

    let fixture_address = streaming_fixture.address();
    let proxy = ProxyProcess::spawn(|listen| {
        proxy_configuration(
            listen,
            &[("stream.apps.test", fixture_address, "net.test.stream")],
        )
    });

    let total_request_length = request_prefix.len() + request_suffix.len();
    let mut request_client = connect(proxy.address()).expect("connect request streaming client");
    write!(
        request_client,
        "POST /stream-request HTTP/1.1\r\nHost: stream.apps.test\r\nContent-Length: {total_request_length}\r\n\r\n"
    )
    .expect("write request streaming head");
    request_client
        .write_all(&request_prefix)
        .expect("write first request segment");
    request_client.flush().expect("flush first request segment");
    prefix_seen_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("upstream received request prefix before request completion");
    request_client
        .write_all(&request_suffix)
        .expect("write second request segment");
    request_client
        .flush()
        .expect("flush second request segment");
    let request_response =
        read_response(&mut request_client).expect("read request stream response");
    assert_eq!(request_response.status, 200);
    let observed_body = request_body_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("fixture recorded streamed request");
    let mut expected_request_body = request_prefix;
    expected_request_body.extend_from_slice(&request_suffix);
    assert_eq!(observed_body, expected_request_body);

    let mut response_client = connect(proxy.address()).expect("connect response streaming client");
    response_client
        .write_all(b"GET /stream-response HTTP/1.1\r\nHost: stream.apps.test\r\n\r\n")
        .expect("write response streaming request");
    response_client
        .flush()
        .expect("flush response streaming request");
    let response_head =
        read_response_head(&mut response_client).expect("read streaming response head");
    assert_eq!(response_head.status, 200);
    let observed_prefix = read_exact_with_prefix(
        &mut response_client,
        response_head.buffered_body,
        response_prefix.len(),
    )
    .expect("proxy delivered response prefix before upstream completion");
    assert!(
        observed_prefix == response_prefix,
        "the streamed response prefix changed"
    );
    release_response_sender
        .send(())
        .expect("release response suffix");
    let observed_suffix =
        read_exact_with_prefix(&mut response_client, Vec::new(), response_suffix.len())
            .expect("read response suffix");
    assert!(
        observed_suffix == response_suffix,
        "the streamed response suffix changed"
    );
}

#[test]
fn submits_a_post_exactly_once_when_upstream_drops_after_submission() {
    let submissions = Arc::new(AtomicUsize::new(0));
    let fixture_submissions = Arc::clone(&submissions);
    let (submitted_sender, submitted_receiver) = mpsc::channel();
    let dropping_fixture = RawServer::spawn(move |mut stream| {
        let Ok(mut request) = read_request(&mut stream) else {
            return;
        };
        if request.target == "/prime-keepalive" {
            write_response(&mut stream, "200 OK", [("X-Prime", "ready")], b"primed")
                .expect("write keepalive priming response");
            request = read_request(&mut stream).expect("read POST on reused upstream connection");
        }
        if request.method == "POST" {
            fixture_submissions.fetch_add(1, Ordering::SeqCst);
            let _ = submitted_sender.send(());
        }
        // Deliberately close a reused connection without a response after
        // consuming the POST. Pingora classifies this path as retryable.
    });

    let fixture_address = dropping_fixture.address();
    let proxy = ProxyProcess::spawn(|listen| {
        proxy_configuration(
            listen,
            &[("single.apps.test", fixture_address, "net.test.single")],
        )
    });
    let mut client = connect(proxy.address()).expect("connect retry-cap client");
    client
        .write_all(b"GET /prime-keepalive HTTP/1.1\r\nHost: single.apps.test\r\n\r\n")
        .expect("write keepalive priming request");
    client.flush().expect("flush keepalive priming request");
    let priming_response =
        read_response(&mut client).expect("prime an upstream keepalive connection");
    assert_eq!(priming_response.status, 200);
    assert_eq!(priming_response.body, b"primed");

    let body = b"one upstream submission";
    let head = format!(
        "POST /json-rpc HTTP/1.1\r\nHost: single.apps.test\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    client
        .write_all(&request)
        .expect("write POST on keepalive client");
    client.flush().expect("flush POST on keepalive client");
    let response = read_response(&mut client).expect("read proxy failure response");
    assert!(response.status >= 500);
    submitted_receiver
        .recv_timeout(CHANNEL_TIMEOUT)
        .expect("fixture received POST submission");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        submissions.load(Ordering::SeqCst),
        1,
        "a POST was submitted upstream more than once"
    );
}

#[test]
fn cookie_websocket_echo_survives_more_than_sixty_idle_seconds() {
    let cookie = format!("session={}", runtime_token("websocket-cookie"));
    let proxy_authorization = format!(
        "Basic {}",
        base64(runtime_token("ws-proxy-auth").as_bytes())
    );
    let fixture_cookie = cookie.clone();
    let (handshake_sender, handshake_receiver) = mpsc::channel();
    let websocket_fixture = RawServer::spawn(move |mut stream| {
        let request = read_request(&mut stream).expect("read WebSocket handshake");
        let accepted = request.header("cookie") == Some(fixture_cookie.as_str())
            && !request.has_header("proxy-authorization")
            && !request.has_header("x-websocket-hop")
            && request
                .header("connection")
                .is_some_and(|value| contains_header_token(value, "upgrade"))
            && request
                .header("upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        handshake_sender
            .send(accepted)
            .expect("record WebSocket handshake");
        if !accepted {
            write_response(
                &mut stream,
                "403 Forbidden",
                [("Connection", "close")],
                b"handshake rejected",
            )
            .expect("reject WebSocket handshake");
            return;
        }
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
            )
            .expect("write WebSocket handshake");
        stream.flush().expect("flush WebSocket handshake");
        stream
            .set_read_timeout(Some(Duration::from_secs(75)))
            .expect("extend WebSocket fixture read timeout");

        let (opcode, first_message) = read_websocket_frame(&mut stream).expect("read first frame");
        assert_eq!(opcode, 0x1);
        write_websocket_frame(&mut stream, 0x1, &first_message, None).expect("echo first frame");

        let (opcode, second_message) =
            read_websocket_frame(&mut stream).expect("read frame after idle period");
        assert_eq!(opcode, 0x1);
        write_websocket_frame(&mut stream, 0x1, &second_message, None)
            .expect("echo frame after idle period");

        if let Ok((0x8, close_body)) = read_websocket_frame(&mut stream) {
            let _ = write_websocket_frame(&mut stream, 0x8, &close_body, None);
        }
    });

    let fixture_address = websocket_fixture.address();
    let launchd_label = format!(
        "com.openai.roused.test.websocket.{}",
        runtime_token("lease")
    );
    let proxy = ProxyProcess::spawn_with_stderr_capture(move |listen| {
        format!(
            "listen = \"{listen}\"\n\n[[services]]\nhost = \"ws.apps.test\"\nupstream = \"{fixture_address}\"\nlaunchd_label = \"{launchd_label}\"\nidle_timeout_seconds = 1\n"
        )
    });
    let original_host = format!("Ws.ApPs.TeSt.:{}", proxy.address().port());
    let handshake = format!(
        "GET /socket/echo HTTP/1.1\r\nHost: {original_host}\r\nUpgrade: websocket\r\nConnection: Upgrade, X-WebSocket-Hop\r\nX-WebSocket-Hop: remove-me\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nCookie: {cookie}\r\nProxy-Authorization: {proxy_authorization}\r\n\r\n"
    );
    let mut client = connect(proxy.address()).expect("connect WebSocket client");
    client
        .write_all(handshake.as_bytes())
        .expect("write WebSocket handshake");
    client.flush().expect("flush WebSocket handshake");
    let response = read_response_head(&mut client).expect("read WebSocket handshake response");
    assert_eq!(response.status, 101);
    assert!(response.buffered_body.is_empty());
    assert_eq!(
        response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
            .map(|(_, value)| value.as_str()),
        Some("websocket")
    );
    assert!(
        handshake_receiver
            .recv_timeout(CHANNEL_TIMEOUT)
            .expect("fixture checked WebSocket authentication"),
        "fixture did not receive the required Cookie or hop headers leaked"
    );

    write_websocket_frame(
        &mut client,
        0x1,
        b"before idle",
        Some([0x17, 0x29, 0x3b, 0x4d]),
    )
    .expect("write first WebSocket frame");
    let (opcode, echoed) = read_websocket_frame(&mut client).expect("read first echo");
    assert_eq!(opcode, 0x1);
    assert_eq!(echoed, b"before idle");

    let idle_started = Instant::now();
    thread::sleep(Duration::from_secs(61));
    assert!(idle_started.elapsed() > Duration::from_secs(60));
    assert!(
        !proxy.stderr_contents().contains("launchctl stop started"),
        "an open WebSocket lease allowed an idle stop"
    );
    write_websocket_frame(
        &mut client,
        0x1,
        b"after idle",
        Some([0x51, 0x63, 0x75, 0x87]),
    )
    .expect("write WebSocket frame after idle period");
    let (opcode, echoed) = read_websocket_frame(&mut client).expect("read echo after idle period");
    assert_eq!(opcode, 0x1);
    assert_eq!(echoed, b"after idle");
    write_websocket_frame(
        &mut client,
        0x8,
        &[0x03, 0xe8],
        Some([0x91, 0xa3, 0xb5, 0xc7]),
    )
    .expect("close WebSocket");
    let (opcode, _) = read_websocket_frame(&mut client).expect("read WebSocket close response");
    assert_eq!(opcode, 0x8);
    drop(client);
    wait_for_proxy_log(&proxy, "launchctl stop started");
}

#[test]
fn invalid_configurations_exit_before_the_listener_can_bind() {
    let listener_guard = TcpListener::bind("127.0.0.1:0").expect("reserve guarded listener");
    let listen = listener_guard
        .local_addr()
        .expect("guarded listener address");
    let first_service = valid_service("alpha.apps.test", "127.0.0.1:19001", "net.test.alpha");
    let second_service = valid_service("beta.apps.test", "127.0.0.1:19002", "net.test.beta");

    let cases = vec![
        (
            "unknown top-level field",
            format!("listen = \"{listen}\"\nunknown = true\n{first_service}"),
        ),
        (
            "unknown service field",
            format!(
                "listen = \"{listen}\"\n{}\nunknown = true\n",
                first_service.trim_end()
            ),
        ),
        (
            "invalid listener",
            format!("listen = \"localhost:8080\"\n{first_service}"),
        ),
        (
            "invalid host",
            format!(
                "listen = \"{listen}\"\n{}",
                first_service.replace("alpha.apps.test", "bad host")
            ),
        ),
        (
            "non-loopback upstream in later entry",
            format!(
                "listen = \"{listen}\"\n{first_service}{}",
                second_service.replace("127.0.0.1:19002", "192.0.2.10:19002")
            ),
        ),
        (
            "duplicate normalized host",
            format!(
                "listen = \"{listen}\"\n{first_service}{}",
                second_service.replace("beta.apps.test", "ALPHA.APPS.TEST.")
            ),
        ),
        (
            "duplicate label",
            format!(
                "listen = \"{listen}\"\n{first_service}{}",
                second_service.replace("net.test.beta", "net.test.alpha")
            ),
        ),
        (
            "duplicate upstream",
            format!(
                "listen = \"{listen}\"\n{first_service}{}",
                second_service.replace("127.0.0.1:19002", "127.0.0.1:19001")
            ),
        ),
        (
            "invalid launchd label",
            format!(
                "listen = \"{listen}\"\n{}",
                first_service.replace("net.test.alpha", "bad/label")
            ),
        ),
        (
            "zero idle timeout",
            format!(
                "listen = \"{listen}\"\n{}\nidle_timeout_seconds = 0\n",
                first_service.trim_end()
            ),
        ),
        (
            "empty stop command",
            format!(
                "listen = \"{listen}\"\n{}\ncan_stop_command = []\n",
                first_service.trim_end()
            ),
        ),
        (
            "relative stop command",
            format!(
                "listen = \"{listen}\"\n{}\ncan_stop_command = [\"relative\"]\n",
                first_service.trim_end()
            ),
        ),
    ];

    for (name, configuration) in cases {
        let (status, stderr) = run_roused_to_exit(&configuration);
        assert!(!status.success(), "{name} was accepted");
        assert!(
            stderr.contains("invalid configuration")
                || stderr.contains("invalid TOML configuration"),
            "{name} reached listener setup instead of configuration rejection"
        );
    }

    assert_eq!(
        listener_guard
            .local_addr()
            .expect("listener guard remains bound"),
        listen
    );
}

fn proxy_configuration(listen: SocketAddr, services: &[(&str, SocketAddr, &str)]) -> String {
    let mut configuration = format!("listen = \"{listen}\"\n");
    for (host, upstream, label) in services {
        configuration.push_str(&format!(
            "\n[[services]]\nhost = \"{host}\"\nupstream = \"{upstream}\"\nlaunchd_label = \"{label}\"\n"
        ));
    }
    configuration
}

fn valid_service(host: &str, upstream: &str, label: &str) -> String {
    format!(
        "\n[[services]]\nhost = \"{host}\"\nupstream = \"{upstream}\"\nlaunchd_label = \"{label}\"\n"
    )
}

fn runtime_token(label: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{label}-{}-{timestamp}-{sequence}", std::process::id())
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn contains_header_token(value: &str, expected: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case(expected))
}

fn wait_for_proxy_log(proxy: &ProxyProcess, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
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

fn write_websocket_frame(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
    mask: Option<[u8; 4]>,
) -> std::io::Result<()> {
    assert!(
        payload.len() <= 125,
        "test WebSocket frames must stay small"
    );
    stream.write_all(&[0x80 | opcode])?;
    stream.write_all(&[(if mask.is_some() { 0x80 } else { 0 }) | payload.len() as u8])?;
    if let Some(mask) = mask {
        stream.write_all(&mask)?;
        let masked = payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4])
            .collect::<Vec<_>>();
        stream.write_all(&masked)?;
    } else {
        stream.write_all(payload)?;
    }
    stream.flush()
}

fn read_websocket_frame(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0; 2];
    stream.read_exact(&mut head)?;
    if head[0] & 0x80 == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fragmented test WebSocket frame",
        ));
    }
    let opcode = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let length = usize::from(head[1] & 0x7f);
    if length > 125 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "large test WebSocket frame",
        ));
    }
    let mut mask = [0; 4];
    if masked {
        stream.read_exact(&mut mask)?;
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((opcode, payload))
}
