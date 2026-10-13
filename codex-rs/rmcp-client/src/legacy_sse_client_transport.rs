use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use futures::Stream;
use futures::StreamExt;
use futures::stream::try_unfold;
use reqwest::Url;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderMap;
use rmcp::service::RoleClient;
use rmcp::service::RxJsonRpcMessage;
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use sse_stream::Sse;
use sse_stream::SseStream;
use thiserror::Error;
use tokio::time;
use tracing::debug;
use tracing::warn;

use crate::remote_header_auth::RemoteHeaderAuth;
use crate::remote_header_auth::RemoteHeaderAuthError;

const LEGACY_SSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
static NEXT_LEGACY_SSE_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

type BoxedSseStream = Pin<Box<dyn Stream<Item = Result<Sse, sse_stream::Error>> + Send>>;

pub(crate) struct LegacySseClientTransport {
    endpoint: String,
    stream: BoxedSseStream,
    pending_messages: VecDeque<RxJsonRpcMessage<RoleClient>>,
    http_client: Arc<dyn HttpClient>,
    default_headers: HeaderMap,
    auth: RemoteHeaderAuth,
}

#[derive(Debug, Error)]
pub(crate) enum LegacySseClientTransportError {
    #[error("Auth required")]
    AuthRequired,
    #[error(transparent)]
    RemoteAuth(#[from] RemoteHeaderAuthError),
    #[error("failed to open SSE stream: {0}")]
    OpenStream(String),
    #[error("failed to send SSE request: {0}")]
    SendRequest(String),
    #[error("failed to serialize SSE payload: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("unexpected end of SSE stream before receiving an endpoint event")]
    MissingEndpoint,
    #[error("invalid SSE endpoint: {0}")]
    InvalidEndpoint(String),
}

impl LegacySseClientTransport {
    pub(crate) async fn connect(
        url: &str,
        default_headers: HeaderMap,
        auth: RemoteHeaderAuth,
        http_client: Arc<dyn HttpClient>,
        connect_timeout: Duration,
    ) -> Result<Self, LegacySseClientTransportError> {
        let base_url = Url::parse(url)
            .map_err(|error| LegacySseClientTransportError::InvalidEndpoint(error.to_string()))?;
        let mut stream = Self::open_stream(
            url,
            &default_headers,
            &auth,
            Arc::clone(&http_client),
            connect_timeout,
        )
        .await?;
        let mut pending_messages = VecDeque::new();
        let endpoint = loop {
            let Some(event) = stream.next().await else {
                return Err(LegacySseClientTransportError::MissingEndpoint);
            };
            let event = event
                .map_err(|error| LegacySseClientTransportError::OpenStream(error.to_string()))?;
            if event.event.as_deref() == Some("endpoint") {
                let endpoint = event.data.as_deref().ok_or_else(|| {
                    LegacySseClientTransportError::InvalidEndpoint(
                        "missing endpoint data".to_string(),
                    )
                })?;
                let endpoint = base_url.join(endpoint).map_err(|error| {
                    LegacySseClientTransportError::InvalidEndpoint(error.to_string())
                })?;
                if endpoint.origin() != base_url.origin() {
                    return Err(LegacySseClientTransportError::InvalidEndpoint(
                        "endpoint origin does not match connection origin".to_string(),
                    ));
                }
                break endpoint.to_string();
            }
            if let Some(message) = parse_sse_message(event) {
                pending_messages.push_back(message);
            }
        };

        Ok(Self {
            endpoint,
            stream,
            pending_messages,
            http_client,
            default_headers,
            auth,
        })
    }

    async fn open_stream(
        url: &str,
        default_headers: &HeaderMap,
        auth: &RemoteHeaderAuth,
        http_client: Arc<dyn HttpClient>,
        connect_timeout: Duration,
    ) -> Result<BoxedSseStream, LegacySseClientTransportError> {
        match Self::open_stream_once(
            url,
            default_headers,
            auth,
            Arc::clone(&http_client),
            connect_timeout,
        )
        .await
        {
            Ok(stream) => Ok(stream),
            Err(LegacySseClientTransportError::AuthRequired) => {
                if auth.retry_after_unauthorized().await? {
                    Self::open_stream_once(url, default_headers, auth, http_client, connect_timeout)
                        .await
                } else {
                    Err(LegacySseClientTransportError::AuthRequired)
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn open_stream_once(
        url: &str,
        default_headers: &HeaderMap,
        auth: &RemoteHeaderAuth,
        http_client: Arc<dyn HttpClient>,
        connect_timeout: Duration,
    ) -> Result<BoxedSseStream, LegacySseClientTransportError> {
        let mut headers = build_request_headers(default_headers, auth).await?;
        headers.push(HttpHeader {
            name: ACCEPT.as_str().to_string(),
            value: "text/event-stream".to_string(),
            value_env_var: None,
        });
        let (response, body_stream) = time::timeout(
            connect_timeout,
            http_client.http_request_stream(HttpRequestParams {
                method: "GET".to_string(),
                url: url.to_string(),
                headers,
                body: None,
                timeout_ms: None,
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: next_request_id("legacy-sse-connect"),
                stream_response: true,
            }),
        )
        .await
        .map_err(|_| {
            LegacySseClientTransportError::OpenStream(format!(
                "timed out after {connect_timeout:?} while opening event stream"
            ))
        })?
        .map_err(|error| LegacySseClientTransportError::OpenStream(error.to_string()))?;
        if response.status == 401 {
            return Err(LegacySseClientTransportError::AuthRequired);
        }
        if !(200..300).contains(&response.status) {
            return Err(LegacySseClientTransportError::OpenStream(format!(
                "HTTP {} while opening event stream",
                response.status
            )));
        }

        let byte_stream = try_unfold(body_stream, |mut body_stream| async move {
            match body_stream.recv().await {
                Ok(Some(chunk)) => Ok(Some((Bytes::from(chunk), body_stream))),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            }
        });
        Ok(Box::pin(SseStream::from_bytes_stream(byte_stream)))
    }

    async fn send_once(
        endpoint: &str,
        http_client: Arc<dyn HttpClient>,
        default_headers: HeaderMap,
        auth: RemoteHeaderAuth,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> Result<(), LegacySseClientTransportError> {
        let mut headers = build_request_headers(&default_headers, &auth).await?;
        headers.push(HttpHeader {
            name: CONTENT_TYPE.as_str().to_string(),
            value: "application/json".to_string(),
            value_env_var: None,
        });
        let body = serde_json::to_vec(&message)?;
        let response = http_client
            .http_request(HttpRequestParams {
                method: "POST".to_string(),
                url: endpoint.to_string(),
                headers,
                body: Some(body.into()),
                timeout_ms: Some(timeout_ms(LEGACY_SSE_REQUEST_TIMEOUT)),
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: next_request_id("legacy-sse-post"),
                stream_response: false,
            })
            .await
            .map_err(|error| LegacySseClientTransportError::SendRequest(error.to_string()))?;
        if response.status == 401 {
            return Err(LegacySseClientTransportError::AuthRequired);
        }
        if !(200..300).contains(&response.status) {
            let body = String::from_utf8_lossy(&response.body.into_inner()).into_owned();
            return Err(LegacySseClientTransportError::SendRequest(format!(
                "HTTP {}: {body}",
                response.status
            )));
        }
        Ok(())
    }
}

impl Transport<RoleClient> for LegacySseClientTransport {
    type Error = LegacySseClientTransportError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let endpoint = self.endpoint.clone();
        let http_client = Arc::clone(&self.http_client);
        let default_headers = self.default_headers.clone();
        let auth = self.auth.clone();
        let retry_item = item.clone();
        async move {
            match LegacySseClientTransport::send_once(
                &endpoint,
                Arc::clone(&http_client),
                default_headers.clone(),
                auth.clone(),
                item,
            )
            .await
            {
                Ok(()) => Ok(()),
                Err(LegacySseClientTransportError::AuthRequired) => {
                    if auth.retry_after_unauthorized().await? {
                        LegacySseClientTransport::send_once(
                            &endpoint,
                            http_client,
                            default_headers,
                            auth,
                            retry_item,
                        )
                        .await
                    } else {
                        Err(LegacySseClientTransportError::AuthRequired)
                    }
                }
                Err(error) => Err(error),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        if let Some(message) = self.pending_messages.pop_front() {
            return Some(message);
        }

        loop {
            let event = match self.stream.next().await {
                Some(Ok(event)) => event,
                Some(Err(error)) => {
                    warn!("legacy SSE stream error: {error}");
                    return None;
                }
                None => return None,
            };
            if event.event.as_deref() == Some("endpoint") {
                continue;
            }
            if let Some(message) = parse_sse_message(event) {
                return Some(message);
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn parse_sse_message(event: Sse) -> Option<RxJsonRpcMessage<RoleClient>> {
    let payload = event.data?;
    if payload.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(&payload) {
        Ok(message) => Some(message),
        Err(error) => {
            debug!("failed to parse legacy SSE payload as JSON-RPC: {error}");
            None
        }
    }
}

async fn build_request_headers(
    default_headers: &HeaderMap,
    auth: &RemoteHeaderAuth,
) -> Result<Vec<HttpHeader>, LegacySseClientTransportError> {
    let mut headers = default_headers.clone();
    auth.apply_headers(&mut headers).await?;
    headers
        .iter()
        .map(|(name, value)| {
            Ok(HttpHeader {
                name: name.as_str().to_string(),
                value: value
                    .to_str()
                    .map_err(|error| {
                        LegacySseClientTransportError::SendRequest(format!(
                            "invalid header value for `{name}`: {error}"
                        ))
                    })?
                    .to_string(),
                value_env_var: None,
            })
        })
        .collect()
}

fn next_request_id(prefix: &str) -> String {
    let id = NEXT_LEGACY_SSE_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{id}")
}

fn timeout_ms(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}
