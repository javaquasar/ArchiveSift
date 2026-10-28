use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use reqwest::{Client, Response};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::HttpClientError;
use crate::AppResult;

const USER_AGENT: &str = "ArchiveSift/0.1";
const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_GLOBAL_HTTP_REQUESTS: usize = 32;
const MAX_HTML_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// A cloneable concurrency gate shared by every HTTP request type.
#[derive(Clone, Debug)]
pub(crate) struct RequestLimiter {
    permits: Arc<Semaphore>,
}

impl RequestLimiter {
    pub(crate) fn new(max_requests: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_requests.max(1))),
        }
    }

    pub(crate) async fn acquire(&self) -> OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("HTTP request limiter is never closed")
    }
}

pub(crate) struct LimitedHttpResponse {
    response: Response,
    permit: OwnedSemaphorePermit,
}

impl LimitedHttpResponse {
    pub(crate) fn into_parts(self) -> (Response, OwnedSemaphorePermit) {
        (self.response, self.permit)
    }
}

fn shared_request_limiter() -> &'static RequestLimiter {
    static LIMITER: OnceLock<RequestLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| RequestLimiter::new(MAX_GLOBAL_HTTP_REQUESTS))
}

pub fn shared_http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();

    CLIENT.get_or_init(|| {
        Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .expect("shared HTTP client should be constructible")
    })
}

pub(crate) async fn fetch_response(url: &str) -> AppResult<LimitedHttpResponse> {
    let permit = shared_request_limiter().acquire().await;
    let response = shared_http_client()
        .get(url)
        .send()
        .await
        .map_err(|source| HttpClientError::RequestFailed {
            url: url.to_string(),
            source,
        })?
        .error_for_status()
        .map_err(|source| HttpClientError::RequestFailed {
            url: url.to_string(),
            source,
        })?;
    Ok(LimitedHttpResponse { response, permit })
}

pub async fn fetch_text(url: &str) -> AppResult<String> {
    let limited = fetch_response(url).await?;
    let (response, _permit) = limited.into_parts();
    read_bounded_text(url, response).await
}

async fn read_bounded_text(url: &str, response: Response) -> AppResult<String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTML_RESPONSE_BYTES as u64)
    {
        return Err(HttpClientError::ResponseTooLarge {
            url: url.to_string(),
            max_bytes: MAX_HTML_RESPONSE_BYTES,
        }
        .into());
    }

    let initial_capacity = response
        .content_length()
        .unwrap_or_default()
        .min(MAX_HTML_RESPONSE_BYTES as u64) as usize;
    let mut body = Vec::with_capacity(initial_capacity);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| HttpClientError::RequestFailed {
            url: url.to_string(),
            source,
        })?;
        if !append_bounded(&mut body, &chunk, MAX_HTML_RESPONSE_BYTES) {
            return Err(HttpClientError::ResponseTooLarge {
                url: url.to_string(),
                max_bytes: MAX_HTML_RESPONSE_BYTES,
            }
            .into());
        }
    }
    Ok(decode_body(body))
}

fn decode_body(body: Vec<u8>) -> String {
    match String::from_utf8(body) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> bool {
    let Some(new_len) = target.len().checked_add(chunk.len()) else {
        return false;
    };
    if new_len > max_bytes {
        return false;
    }
    target.extend_from_slice(chunk);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_limiter_enforces_capacity_and_releases_permits() {
        let limiter = RequestLimiter::new(2);
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;

        assert!(limiter.permits.clone().try_acquire_owned().is_err());

        drop(first);
        let replacement = limiter
            .permits
            .clone()
            .try_acquire_owned()
            .expect("dropping a permit should make capacity available");

        drop(second);
        drop(replacement);
    }

    #[tokio::test]
    async fn request_limiter_treats_zero_as_one() {
        let limiter = RequestLimiter::new(0);
        let only = limiter.acquire().await;
        assert!(limiter.permits.clone().try_acquire_owned().is_err());
        drop(only);
    }

    #[test]
    fn bounded_body_rejects_crossing_chunk_without_retaining_it() {
        let mut body = b"1234".to_vec();
        assert!(append_bounded(&mut body, b"56", 6));
        assert_eq!(body, b"123456");
        assert!(!append_bounded(&mut body, b"7", 6));
        assert_eq!(body, b"123456");
    }

    #[test]
    fn bounded_body_handles_length_overflow() {
        let mut body = Vec::new();
        assert!(!append_bounded(&mut body, b"x", 0));
        assert!(body.is_empty());
    }

    #[test]
    fn valid_utf8_body_reuses_the_streaming_buffer() {
        let body = b"bounded response".to_vec();
        let allocation = body.as_ptr();

        let text = decode_body(body);

        assert_eq!(text, "bounded response");
        assert_eq!(text.as_ptr(), allocation);
    }
}
