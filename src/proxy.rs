use crate::config::{Config, normalize_request_host};
use crate::lifecycle::{
    IdleMonitor, RequestAdmission, RequestGate, ServiceLease, ServiceLifecycle,
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
use std::sync::Arc;
use std::time::Duration;

const UNKNOWN_HOST_BODY: &[u8] = b"unknown host\n";
const SHUTTING_DOWN_BODY: &[u8] = b"gateway shutting down\n";
const LOADING_HTML_BODY: &[u8] = b"<!doctype html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"5\"><title>Loading</title></head><body>Service is starting.</body></html>\n";
const LOADING_JSON_BODY: &[u8] = b"{\"status\":\"loading\",\"retry_after\":5}\n";

pub struct RousedProxy {
    listener_port: u16,
    routes: HashMap<String, Arc<ServiceRoute>>,
    idle_monitor: IdleMonitor,
    request_gate: Arc<RequestGate>,
}

#[derive(Clone)]
pub struct GatewayShutdownHandle {
    request_gate: Arc<RequestGate>,
}

impl GatewayShutdownHandle {
    pub fn begin_shutdown(&self) {
        self.request_gate.close();
    }
}

pub struct RequestContext {
    route: Option<Arc<ServiceRoute>>,
    lease: Option<ServiceLease>,
    admission: Option<RequestAdmission>,
    rejected_for_shutdown: bool,
    websocket: bool,
}

struct ServiceRoute {
    wake: Arc<WakeTarget>,
    lifecycle: Arc<ServiceLifecycle>,
}

impl RousedProxy {
    pub fn new(config: &Config) -> Self {
        let user_id = current_user_id();
        let request_gate = RequestGate::new();
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
            listener_port: config.listen().port(),
            routes,
            idle_monitor: IdleMonitor::new(lifecycles, Arc::clone(&request_gate)),
            request_gate,
        }
    }

    pub fn idle_monitor(
        &self,
    ) -> impl pingora::services::background::BackgroundService + Clone + use<> {
        self.idle_monitor.clone()
    }

    pub fn shutdown_handle(&self) -> GatewayShutdownHandle {
        GatewayShutdownHandle {
            request_gate: Arc::clone(&self.request_gate),
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
        }
    }

    async fn request_filter(&self, session: &mut Session, context: &mut Self::CTX) -> Result<bool> {
        if context.rejected_for_shutdown {
            write_shutting_down(session).await?;
            return Ok(true);
        }
        context.websocket = is_valid_websocket_request(session.req_header());
        let route = session
            .req_header()
            .headers
            .get(HOST)
            .and_then(|host| host.to_str().ok())
            .and_then(|host| normalize_request_host(host, self.listener_port))
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
