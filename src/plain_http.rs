use async_trait::async_trait;
use pingora::apps::ServerApp;
use pingora::protocols::Stream;
use pingora::proxy::HttpProxy;
use pingora::server::ShutdownWatch;
use roused::proxy::RousedProxy;
use std::sync::Arc;
use std::time::Duration;

const TLS_PREFIX_TIMEOUT: Duration = Duration::from_secs(60);

/// Rejects TLS handshakes before delegating raw TCP streams to Pingora's HTTP parser.
pub struct PlainHttpApp {
    proxy: Arc<HttpProxy<RousedProxy>>,
}

impl PlainHttpApp {
    pub fn new(proxy: HttpProxy<RousedProxy>) -> Self {
        Self {
            proxy: Arc::new(proxy),
        }
    }
}

#[async_trait]
impl ServerApp for PlainHttpApp {
    async fn process_new(
        self: &Arc<Self>,
        mut stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        if *shutdown.borrow() {
            return None;
        }

        let mut prefix = [0_u8; 3];
        let mut shutdown = shutdown.clone();
        let peeked = tokio::select! {
            biased;
            _ = shutdown.changed() => return None,
            result = tokio::time::timeout(TLS_PREFIX_TIMEOUT, stream.try_peek(&mut prefix)) => result,
        };

        match peeked {
            // 0x16 is a TLS handshake record and 0x03 is the TLS version-family major byte.
            Ok(Ok(true)) if prefix[0] == 0x16 && prefix[1] == 0x03 => None,
            Ok(Ok(_)) => ServerApp::process_new(&self.proxy, stream, &shutdown).await,
            Ok(Err(_)) | Err(_) => None,
        }
    }

    async fn cleanup(&self) {
        ServerApp::cleanup(self.proxy.as_ref()).await;
    }
}
