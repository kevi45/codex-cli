use std::sync::Arc;

use codex_api::SharedAuthProvider;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationManager;
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Clone)]
pub(crate) enum RemoteHeaderAuth {
    None,
    Shared(SharedAuthProvider),
    Bearer(String),
    OAuth(Arc<Mutex<AuthorizationManager>>),
}

#[derive(Debug, Error)]
pub(crate) enum RemoteHeaderAuthError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("invalid authorization header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),
}

impl RemoteHeaderAuth {
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager access must remain serialized through its mutex"
    )]
    pub(crate) async fn apply_headers(
        &self,
        headers: &mut HeaderMap,
    ) -> Result<(), RemoteHeaderAuthError> {
        match self {
            Self::None => Ok(()),
            Self::Shared(auth_provider) => {
                auth_provider.add_auth_headers(headers);
                Ok(())
            }
            Self::Bearer(token) => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
                Ok(())
            }
            Self::OAuth(manager) => {
                let token = manager.lock().await.get_access_token().await?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
                Ok(())
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "AuthorizationManager refresh must remain serialized through its mutex"
    )]
    pub(crate) async fn retry_after_unauthorized(&self) -> Result<bool, RemoteHeaderAuthError> {
        match self {
            Self::OAuth(manager) => {
                manager.lock().await.refresh_token().await?;
                Ok(true)
            }
            Self::None | Self::Shared(_) | Self::Bearer(_) => Ok(false),
        }
    }
}
