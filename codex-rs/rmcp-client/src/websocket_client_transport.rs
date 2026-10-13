use std::sync::Arc;
use std::time::Duration;

use codex_http_client::maybe_build_rustls_client_config_with_custom_ca;
use futures::SinkExt;
use futures::StreamExt;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use reqwest::header::HeaderMap;
use rmcp::service::RoleClient;
use rmcp::service::RxJsonRpcMessage;
use rmcp::service::TxJsonRpcMessage;
use rmcp::transport::Transport;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::debug;

use crate::remote_header_auth::RemoteHeaderAuth;
use crate::remote_header_auth::RemoteHeaderAuthError;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = SplitSink<WsStream, Message>;
type WsRead = SplitStream<WsStream>;

pub(crate) struct WebSocketClientTransport {
    read: WsRead,
    write: Arc<Mutex<WsWrite>>,
}

#[derive(Debug, Error)]
pub(crate) enum WebSocketClientTransportError {
    #[error(transparent)]
    RemoteAuth(#[from] RemoteHeaderAuthError),
    #[error("failed to build websocket request: {0}")]
    RequestBuild(String),
    #[error("failed to connect websocket: {0}")]
    Connect(String),
    #[error("timed out connecting websocket after {0:?}")]
    ConnectTimeout(Duration),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Tungstenite(#[from] TungsteniteError),
}

impl WebSocketClientTransport {
    pub(crate) async fn connect(
        url: &str,
        default_headers: HeaderMap,
        auth: RemoteHeaderAuth,
        connect_timeout: Duration,
    ) -> Result<Self, WebSocketClientTransportError> {
        let mut request = url
            .into_client_request()
            .map_err(|error| WebSocketClientTransportError::RequestBuild(error.to_string()))?;
        request.headers_mut().extend(default_headers.clone());
        auth.apply_headers(request.headers_mut()).await?;

        let connector = maybe_build_rustls_client_config_with_custom_ca()
            .map_err(|error| WebSocketClientTransportError::Connect(error.to_string()))?
            .map(tokio_tungstenite::Connector::Rustls);
        let connect_future = connect_async_tls_with_config(
            request, /*config*/ None, /*disable_nagle*/ false, connector,
        );
        let (stream, _) = timeout(connect_timeout, connect_future)
            .await
            .map_err(|_| WebSocketClientTransportError::ConnectTimeout(connect_timeout))?
            .map_err(|error| WebSocketClientTransportError::Connect(error.to_string()))?;
        let (write, read) = stream.split();
        Ok(Self {
            read,
            write: Arc::new(Mutex::new(write)),
        })
    }
}

impl Transport<RoleClient> for WebSocketClientTransport {
    type Error = WebSocketClientTransportError;

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "websocket writes must be serialized through the shared sink mutex"
    )]
    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let write = Arc::clone(&self.write);
        async move {
            let payload = serde_json::to_string(&item)?;
            let mut write = write.lock().await;
            write.send(Message::Text(payload.into())).await?;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            match self.read.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(text.as_ref()) {
                        Ok(message) => return Some(message),
                        Err(error) => {
                            debug!("failed to parse websocket JSON-RPC payload: {error}");
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    debug!("ignoring websocket binary frame from MCP server");
                }
                Some(Ok(Message::Ping(_)))
                | Some(Ok(Message::Pong(_)))
                | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) => return None,
                Some(Err(error)) => {
                    debug!("websocket receive error: {error}");
                    return None;
                }
                None => return None,
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "websocket close must be serialized through the shared sink mutex"
    )]
    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut write = self.write.lock().await;
        write.close().await?;
        Ok(())
    }
}
