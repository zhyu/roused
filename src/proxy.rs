use crate::config::{Config, normalize_request_host};
use crate::lifecycle::{
    IdleMonitor, RequestAdmission, RequestGate, ServiceLease, ServiceLifecycle, ShutdownCoordinator,
};
use crate::wake::{WakeTarget, current_user_id};
use async_trait::async_trait;
use bytes::Bytes;
use http::header::{
    ACCEPT, CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, RETRY_AFTER, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use http::{HeaderMap, Method};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::{Error, HttpPeer, InternalError, Result};
use pingora::proxy::{ProxyHttp, Session};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

const UNKNOWN_HOST_BODY: &[u8] = b"unknown host\n";
const SHUTTING_DOWN_BODY: &[u8] = b"gateway shutting down\n";
const LOADING_HTML_BODY: &[u8] = b"<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"5\"><title>Loading</title></head><body>Service is starting.</body></html>\n";
const LOADING_JSON_BODY: &[u8] = b"{\"status\":\"loading\",\"retry_after\":5}\n";

pub struct RousedProxy {
    plain_listener_port: u16,
    routes: HashMap<String, Arc<ServiceRoute>>,
    idle_monitor: IdleMonitor,
    request_gate: Arc<RequestGate>,
    shutdown: Arc<ShutdownCoordinator>,
}

#[derive(Clone)]
pub struct GatewayShutdownHandle {
    shutdown: Arc<ShutdownCoordinator>,
}

impl GatewayShutdownHandle {
    pub fn begin_shutdown(&self) {
        self.shutdown.request();
    }

    pub async fn wait_for_cleanup(&self, max_wait: Duration) -> bool {
        self.shutdown.wait_for_completion(max_wait).await
    }
}

pub struct RequestContext {
    route: Option<Arc<ServiceRoute>>,
    lease: Option<ServiceLease>,
    admission: Option<RequestAdmission>,
    rejected_for_shutdown: bool,
    websocket: bool,
    listener_port: u16,
    tls: bool,
    peer_ip: Option<IpAddr>,
}

struct ServiceRoute {
    wake: Arc<WakeTarget>,
    lifecycle: Arc<ServiceLifecycle>,
}

impl RousedProxy {
    pub fn new(config: &Config) -> Self {
        let user_id = current_user_id();
        let request_gate = RequestGate::new();
        let shutdown = ShutdownCoordinator::new(Arc::clone(&request_gate));
        let mut lifecycles = Vec::new();
        let routes = config
            .services()
            .map(|service| {
                let wake = WakeTarget::new(
                    service.upstream(),
                    service.launchd_label().to_owned(),
                    user_id,
                );
                let lifecycle = ServiceLifecycle::new(
                    service.launchd_label().to_owned(),
                    user_id,
                    Duration::from_secs(service.idle_timeout_seconds()),
                    service.can_stop_command().map(<[String]>::to_vec),
                    Arc::clone(&wake),
                );
                lifecycles.push(Arc::clone(&lifecycle));
                (
                    service.host().to_owned(),
                    Arc::new(ServiceRoute { wake, lifecycle }),
                )
            })
            .collect();
        Self {
            plain_listener_port: config.listen().port(),
            routes,
            idle_monitor: IdleMonitor::new(
                lifecycles,
                Arc::clone(&request_gate),
                Arc::clone(&shutdown),
            ),
            request_gate,
            shutdown,
        }
    }

    pub fn idle_monitor(
        &self,
    ) -> impl pingora::services::background::BackgroundService + Clone + use<> {
        self.idle_monitor.clone()
    }

    pub fn shutdown_handle(&self) -> GatewayShutdownHandle {
        GatewayShutdownHandle {
            shutdown: Arc::clone(&self.shutdown),
        }
    }
}

#[async_trait]
impl ProxyHttp for RousedProxy {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        let admission = self.request_gate.admit();
        RequestContext {
            route: None,
            lease: None,
            rejected_for_shutdown: admission.is_none(),
            admission,
            websocket: false,
            listener_port: self.plain_listener_port,
            tls: false,
            peer_ip: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, context: &mut Self::CTX) -> Result<bool> {
        if context.rejected_for_shutdown {
            write_shutting_down(session).await?;
            return Ok(true);
        }
        context.websocket = is_valid_websocket_request(session.req_header());
        let (local_port, tls, peer_ip) = request_transport(session);
        context.listener_port = local_port.unwrap_or(self.plain_listener_port);
        context.tls = tls;
        context.peer_ip = peer_ip;

        let route = session
            .req_header()
            .headers
            .get(HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(|host| normalize_request_host(host, context.listener_port))
            .and_then(|host| self.routes.get(&host).cloned());

        let Some(route) = route else {
            drop(context.admission.take());
            write_unknown_host(session).await?;
            return Ok(true);
        };
        route.lifecycle.note_request_arrival();

        if route.wake.is_ready().await {
            context.route = Some(route);
            return Ok(false);
        }

        route.wake.request_launch();
        drop(context.admission.take());
        write_loading_response(session).await?;
        Ok(true)
    }

    async fn proxy_upstream_filter(
        &self,
        _session: &mut Session,
        context: &mut Self::CTX,
    ) -> Result<bool> {
        let route = context.route.as_ref().ok_or_else(|| {
            Error::explain(InternalError, "route missing before upstream proxying")
        })?;
        if context.lease.is_some() {
            return Err(Error::explain(
                InternalError,
                "service lease acquired more than once",
            ));
        }
        context.lease = Some(route.lifecycle.acquire());
        drop(context.admission.take());
        Ok(true)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        context: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let upstream = context
            .route
            .as_ref()
            .map(|route| route.wake.upstream())
            .ok_or_else(|| {
                Error::explain(InternalError, "route missing after request filtering")
            })?;
        Ok(Box::new(HttpPeer::new(upstream, false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut RequestHeader,
        context: &mut Self::CTX,
    ) -> Result<()> {
        strip_request_hop_headers(upstream_request, context.websocket);
        apply_forwarded_headers(upstream_request, context)?;

        // Pingora reads the downstream body incrementally, but after removing the
        // inbound framing header it needs explicit H1 framing for the upstream.
        if !session.is_body_empty() && !upstream_request.headers.contains_key(CONTENT_LENGTH) {
            upstream_request.insert_header(TRANSFER_ENCODING, "chunked")?;
        }
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        context: &mut Self::CTX,
    ) -> Result<()> {
        let upgraded = context.websocket && is_valid_websocket_response(upstream_response);
        strip_response_hop_headers(upstream_response, upgraded);
        if upgraded {
            session.as_downstream_mut().set_read_timeout(None);
        }
        Ok(())
    }

    async fn logging(
        &self,
        _session: &mut Session,
        _error: Option<&Error>,
        context: &mut Self::CTX,
    ) {
        drop(context.lease.take());
        drop(context.admission.take());
    }

    fn request_summary(&self, _session: &Session, _context: &Self::CTX) -> String {
        // Pingora's default includes Host. Keep all request headers, query
        // credentials, and body tokens out of supported logs.
        "proxied request".to_owned()
    }
}

async fn write_shutting_down(session: &mut Session) -> Result<()> {
    let head = session.req_header().method == Method::HEAD;
    let mut response = ResponseHeader::build(503, Some(3))?;
    response.insert_header("Content-Type", "text/plain; charset=utf-8")?;
    response.insert_header("Cache-Control", "no-store")?;
    response.insert_header(CONTENT_LENGTH, SHUTTING_DOWN_BODY.len().to_string())?;

    session.as_downstream_mut().set_keepalive(None);
    session
        .write_response_header(Box::new(response), head)
        .await?;
    if head {
        Ok(())
    } else {
        session
            .write_response_body(Some(Bytes::from_static(SHUTTING_DOWN_BODY)), true)
            .await
    }
}

async fn write_unknown_host(session: &mut Session) -> Result<()> {
    let mut response = ResponseHeader::build(421, Some(4))?;
    response.insert_header("Content-Type", "text/plain; charset=utf-8")?;
    response.insert_header("Cache-Control", "no-store")?;
    response.insert_header(CONTENT_LENGTH, UNKNOWN_HOST_BODY.len().to_string())?;

    // Do not reuse a connection whose request body was not consumed.
    session.as_downstream_mut().set_keepalive(None);
    session
        .write_response_header(Box::new(response), false)
        .await?;
    session
        .write_response_body(Some(Bytes::from_static(UNKNOWN_HOST_BODY)), true)
        .await
}

async fn write_loading_response(session: &mut Session) -> Result<()> {
    let html = prefers_html_loading(session.req_header());
    let head = session.req_header().method == Method::HEAD;
    let (content_type, body) = if html {
        ("text/html; charset=utf-8", LOADING_HTML_BODY)
    } else {
        ("application/json", LOADING_JSON_BODY)
    };

    let mut response = ResponseHeader::build(503, Some(5))?;
    response.insert_header(CONTENT_TYPE, content_type)?;
    response.insert_header(RETRY_AFTER, "5")?;
    response.insert_header(CACHE_CONTROL, "no-store")?;
    response.insert_header(CONTENT_LENGTH, body.len().to_string())?;

    // The cold request is deliberately not consumed or replayed. Closing the
    // downstream connection prevents unread body bytes from becoming a later
    // request on the same HTTP/1.1 connection.
    session.as_downstream_mut().set_keepalive(None);
    session
        .write_response_header(Box::new(response), head)
        .await?;
    if head {
        Ok(())
    } else {
        session
            .write_response_body(Some(Bytes::from_static(body)), true)
            .await
    }
}

fn apply_forwarded_headers(request: &mut RequestHeader, context: &RequestContext) -> Result<()> {
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-port",
        "x-forwarded-prefix",
        "x-forwarded-proto",
        "x-real-ip",
    ] {
        request.remove_header(name);
    }

    if !context.tls {
        return Ok(());
    }

    request.insert_header("X-Forwarded-Proto", "https")?;
    if let Some(peer_ip) = context.peer_ip {
        let peer_ip = peer_ip.to_string();
        request.insert_header("X-Forwarded-For", peer_ip.as_str())?;
        request.insert_header("X-Real-IP", peer_ip.as_str())?;
    }
    if let Some(host) = request
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    {
        request.insert_header("X-Forwarded-Host", host.as_str())?;
    }
    request.insert_header("X-Forwarded-Port", context.listener_port.to_string())?;
    Ok(())
}

fn request_transport(session: &Session) -> (Option<u16>, bool, Option<IpAddr>) {
    let Some(digest) = session.digest() else {
        return (None, false, None);
    };
    let socket = digest.socket_digest.as_ref();
    let local_port = socket
        .and_then(|socket| socket.local_addr())
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.port());
    let peer_ip = socket
        .and_then(|socket| socket.peer_addr())
        .and_then(|addr| addr.as_inet())
        .map(|addr| addr.ip());
    (local_port, digest.ssl_digest.is_some(), peer_ip)
}

fn prefers_html_loading(request: &RequestHeader) -> bool {
    if request.method != Method::GET && request.method != Method::HEAD {
        return false;
    }

    request.headers.get_all(ACCEPT).iter().any(|value| {
        let Ok(value) = value.to_str() else {
            return false;
        };
        value.split(',').any(html_media_range_is_accepted)
    })
}

fn html_media_range_is_accepted(media_range: &str) -> bool {
    let mut parts = media_range.split(';');
    if !parts
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"))
    {
        return false;
    }

    parts
        .filter_map(|parameter| parameter.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
        .is_none_or(|(_, value)| {
            value
                .trim()
                .parse::<f32>()
                .is_ok_and(|quality| quality > 0.0 && quality <= 1.0)
        })
}

fn is_valid_websocket_request(request: &RequestHeader) -> bool {
    request.version == http::Version::HTTP_11
        && request
            .headers
            .get_all(UPGRADE)
            .iter()
            .any(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        && header_contains_token(&request.headers, CONNECTION, b"upgrade")
}

fn is_valid_websocket_response(response: &ResponseHeader) -> bool {
    response.status.as_u16() == 101
        && response
            .headers
            .get_all(UPGRADE)
            .iter()
            .any(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
        && header_contains_token(&response.headers, CONNECTION, b"upgrade")
}

fn header_contains_token(headers: &HeaderMap, name: HeaderName, expected: &[u8]) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .as_bytes()
            .split(|byte| *byte == b',')
            .map(trim_ascii_whitespace)
            .any(|token| token.eq_ignore_ascii_case(expected))
    })
}

fn connection_header_names(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|byte| *byte == b','))
        .map(trim_ascii_whitespace)
        .filter_map(|name| HeaderName::from_bytes(name).ok())
        .collect()
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn strip_request_hop_headers(request: &mut RequestHeader, retain_upgrade: bool) {
    for name in connection_header_names(&request.headers) {
        if !(retain_upgrade && name == UPGRADE) {
            request.remove_header(&name);
        }
    }
    request.remove_header(&PROXY_AUTHORIZATION);
    strip_request_bounded_headers(request, retain_upgrade);
    if retain_upgrade {
        request
            .insert_header(CONNECTION, "Upgrade")
            .expect("static Connection header is valid");
        request
            .insert_header(UPGRADE, "websocket")
            .expect("static Upgrade header is valid");
    }
}

fn strip_request_bounded_headers(request: &mut RequestHeader, retain_upgrade: bool) {
    if !retain_upgrade {
        request.remove_header(&CONNECTION);
        request.remove_header(&UPGRADE);
    }
    for name in [
        "proxy-connection",
        "keep-alive",
        PROXY_AUTHENTICATE.as_str(),
        TE.as_str(),
        TRAILER.as_str(),
        TRANSFER_ENCODING.as_str(),
    ] {
        request.remove_header(name);
    }
}

fn strip_response_hop_headers(response: &mut ResponseHeader, retain_upgrade: bool) {
    for name in connection_header_names(&response.headers) {
        if !(retain_upgrade && name == UPGRADE) {
            response.remove_header(&name);
        }
    }
    if !retain_upgrade {
        response.remove_header(&CONNECTION);
        response.remove_header(&UPGRADE);
    }
    for name in [
        "proxy-connection",
        "keep-alive",
        PROXY_AUTHENTICATE.as_str(),
        TE.as_str(),
        TRAILER.as_str(),
        TRANSFER_ENCODING.as_str(),
    ] {
        response.remove_header(name);
    }
    if retain_upgrade {
        response
            .insert_header(CONNECTION, "Upgrade")
            .expect("static Connection header is valid");
        response
            .insert_header(UPGRADE, "websocket")
            .expect("static Upgrade header is valid");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_forwarded_headers_replace_spoofed_values() {
        let mut request = RequestHeader::build("GET", b"/", Some(24)).unwrap();
        request.insert_header(HOST, "stash.apps.test:8443").unwrap();
        request
            .insert_header("Forwarded", "for=203.0.113.9")
            .unwrap();
        request.insert_header("X-Forwarded-Proto", "http").unwrap();
        request
            .insert_header("X-Forwarded-For", "203.0.113.9")
            .unwrap();
        request.insert_header("X-Real-IP", "203.0.113.9").unwrap();
        let context = RequestContext {
            route: None,
            lease: None,
            admission: None,
            rejected_for_shutdown: false,
            websocket: false,
            listener_port: 8443,
            tls: true,
            peer_ip: Some("192.0.2.10".parse().unwrap()),
        };

        apply_forwarded_headers(&mut request, &context).unwrap();

        assert_eq!(request.headers[HOST], "stash.apps.test:8443");
        assert_eq!(request.headers["x-forwarded-proto"], "https");
        assert_eq!(request.headers["x-forwarded-for"], "192.0.2.10");
        assert_eq!(request.headers["x-real-ip"], "192.0.2.10");
        assert_eq!(request.headers["x-forwarded-host"], "stash.apps.test:8443");
        assert_eq!(request.headers["x-forwarded-port"], "8443");
        assert!(!request.headers.contains_key("forwarded"));
    }

    #[test]
    fn plaintext_forwarded_headers_are_removed() {
        let mut request = RequestHeader::build("GET", b"/", Some(24)).unwrap();
        request.insert_header(HOST, "stash.apps.test:8080").unwrap();
        request.insert_header("X-Forwarded-Proto", "https").unwrap();
        request
            .insert_header("X-Forwarded-For", "203.0.113.9")
            .unwrap();
        request.insert_header("X-Real-IP", "203.0.113.9").unwrap();
        let context = RequestContext {
            route: None,
            lease: None,
            admission: None,
            rejected_for_shutdown: false,
            websocket: false,
            listener_port: 8080,
            tls: false,
            peer_ip: Some("192.0.2.10".parse().unwrap()),
        };

        apply_forwarded_headers(&mut request, &context).unwrap();

        assert_eq!(request.headers[HOST], "stash.apps.test:8080");
        for name in [
            "x-forwarded-proto",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-port",
            "x-real-ip",
        ] {
            assert!(!request.headers.contains_key(name), "{name}");
        }
    }

    #[test]
    fn ordinary_request_removes_bounded_and_connection_named_headers() {
        let proxy_credential = format!("runtime-proxy-{}", std::process::id());
        let origin_credential = format!("runtime-origin-{}", std::process::id());
        let mut request = RequestHeader::build("GET", b"/", Some(16)).unwrap();
        request
            .insert_header(CONNECTION, "keep-alive, x-hop")
            .unwrap();
        request.insert_header("x-hop", "remove").unwrap();
        request.insert_header("keep-alive", "timeout=5").unwrap();
        request
            .insert_header(PROXY_AUTHORIZATION, proxy_credential.as_str())
            .unwrap();
        request
            .insert_header("authorization", origin_credential.as_str())
            .unwrap();

        strip_request_hop_headers(&mut request, false);

        for name in [
            CONNECTION,
            PROXY_AUTHORIZATION,
            HeaderName::from_static("x-hop"),
        ] {
            assert!(!request.headers.contains_key(name));
        }
        assert!(request.headers.contains_key("authorization"));
    }

    #[test]
    fn websocket_retains_only_upgrade_hop_headers() {
        let mut request = RequestHeader::build("GET", b"/socket", Some(16)).unwrap();
        request.set_version(http::Version::HTTP_11);
        request.insert_header(CONNECTION, "Upgrade, x-hop").unwrap();
        request.insert_header(UPGRADE, "websocket").unwrap();
        request.insert_header("x-hop", "remove").unwrap();
        assert!(is_valid_websocket_request(&request));

        strip_request_hop_headers(&mut request, true);

        assert_eq!(request.headers[CONNECTION], "Upgrade");
        assert_eq!(request.headers[UPGRADE], "websocket");
        assert!(!request.headers.contains_key("x-hop"));
    }

    #[test]
    fn websocket_response_requires_and_normalizes_upgrade_headers() {
        let mut response = ResponseHeader::build(101, Some(8)).unwrap();
        response
            .insert_header(CONNECTION, "Upgrade, Connection, x-hop")
            .unwrap();
        response.insert_header(UPGRADE, "WebSocket").unwrap();
        response.insert_header("x-hop", "remove").unwrap();
        assert!(is_valid_websocket_response(&response));

        strip_response_hop_headers(&mut response, true);

        assert_eq!(response.headers[CONNECTION], "Upgrade");
        assert_eq!(response.headers[UPGRADE], "websocket");
        assert!(!response.headers.contains_key("x-hop"));
    }

    #[test]
    fn response_preserves_repeated_end_to_end_fields() {
        let first_cookie = format!("one={}", std::process::id());
        let second_cookie = format!("two={}; Secure", std::process::id());
        let mut response = ResponseHeader::build(401, Some(16)).unwrap();
        response
            .append_header("set-cookie", first_cookie.as_str())
            .unwrap();
        response
            .append_header("set-cookie", second_cookie.as_str())
            .unwrap();
        response.insert_header("www-authenticate", "Basic").unwrap();
        response.insert_header(CONNECTION, "x-hop").unwrap();
        response.insert_header("x-hop", "remove").unwrap();

        strip_response_hop_headers(&mut response, false);

        assert_eq!(response.headers.get_all("set-cookie").iter().count(), 2);
        assert!(response.headers.contains_key("www-authenticate"));
        assert!(!response.headers.contains_key("x-hop"));
    }

    #[test]
    fn loading_html_requires_a_safe_method_and_explicit_html_acceptance() {
        let mut get = RequestHeader::build("GET", b"/", Some(4)).unwrap();
        get.insert_header(ACCEPT, "application/json, text/html;q=0.5")
            .unwrap();
        assert!(prefers_html_loading(&get));

        let mut head = RequestHeader::build("HEAD", b"/", Some(4)).unwrap();
        head.insert_header(ACCEPT, "TEXT/HTML").unwrap();
        assert!(prefers_html_loading(&head));

        let mut api_get = RequestHeader::build("GET", b"/", Some(4)).unwrap();
        api_get
            .insert_header(ACCEPT, "application/json, */*")
            .unwrap();
        assert!(!prefers_html_loading(&api_get));

        let mut rejected_html = RequestHeader::build("GET", b"/", Some(4)).unwrap();
        rejected_html
            .insert_header(ACCEPT, "text/html;q=0")
            .unwrap();
        assert!(!prefers_html_loading(&rejected_html));

        let mut post = RequestHeader::build("POST", b"/", Some(4)).unwrap();
        post.insert_header(ACCEPT, "text/html").unwrap();
        assert!(!prefers_html_loading(&post));
    }
}
