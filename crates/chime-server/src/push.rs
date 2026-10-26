use async_trait::async_trait;
use futures_lite::io::AsyncReadExt;
use isahc::{
    HttpClient,
    config::{Configurable, RedirectPolicy},
};
use std::time::Duration;
use web_push::{WebPushClient, WebPushError, WebPushMessage, request_builder};

const MAX_PROVIDER_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResult {
    Accepted,
    Expired,
    Failed,
}

#[async_trait]
pub trait PushTransport: Send + Sync {
    async fn send(&self, message: WebPushMessage) -> ProviderResult;
}

#[derive(Clone)]
pub struct WebPushTransport {
    client: BoundedWebPushClient,
}

impl WebPushTransport {
    pub fn new() -> anyhow::Result<Self> {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .redirect_policy(RedirectPolicy::None)
            .proxy(None)
            .build()?;
        Ok(Self {
            client: BoundedWebPushClient { client },
        })
    }
}

#[async_trait]
impl PushTransport for WebPushTransport {
    async fn send(&self, message: WebPushMessage) -> ProviderResult {
        classify(self.client.send(message).await)
    }
}

fn classify(result: Result<(), WebPushError>) -> ProviderResult {
    match result {
        Ok(()) => ProviderResult::Accepted,
        Err(WebPushError::EndpointNotFound(_) | WebPushError::EndpointNotValid(_)) => {
            ProviderResult::Expired
        }
        Err(_) => ProviderResult::Failed,
    }
}

#[derive(Clone)]
struct BoundedWebPushClient {
    client: HttpClient,
}

#[async_trait]
impl WebPushClient for BoundedWebPushClient {
    async fn send(&self, message: WebPushMessage) -> Result<(), WebPushError> {
        let request = request_builder::build_request::<isahc::AsyncBody>(message);
        let response = self
            .client
            .send_async(request)
            .await
            .map_err(|_| WebPushError::Unspecified)?;
        let status = response.status();
        let mut body = Vec::new();
        let size = response
            .into_body()
            .take((MAX_PROVIDER_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .await
            .map_err(|error| WebPushError::Io(std::io::Error::other(error)))?;
        if size > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(WebPushError::ResponseTooLarge);
        }
        request_builder::parse_response(status, body)
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderResult, classify};
    use isahc::http::StatusCode;
    use web_push::{WebPushError, request_builder};

    #[test]
    fn only_provider_404_and_410_are_permanent_subscription_expiry() {
        let not_found = request_builder::parse_response(StatusCode::NOT_FOUND, vec![]).unwrap_err();
        let gone = request_builder::parse_response(StatusCode::GONE, vec![]).unwrap_err();
        let unauthorized =
            request_builder::parse_response(StatusCode::UNAUTHORIZED, vec![]).unwrap_err();
        let unavailable =
            request_builder::parse_response(StatusCode::SERVICE_UNAVAILABLE, vec![]).unwrap_err();

        assert_eq!(classify(Err(not_found)), ProviderResult::Expired);
        assert_eq!(classify(Err(gone)), ProviderResult::Expired);
        assert_eq!(classify(Err(unauthorized)), ProviderResult::Failed);
        assert_eq!(classify(Err(unavailable)), ProviderResult::Failed);
        assert_eq!(classify(Ok(())), ProviderResult::Accepted);
        assert_eq!(
            classify(Err(WebPushError::ResponseTooLarge)),
            ProviderResult::Failed
        );
    }
}
