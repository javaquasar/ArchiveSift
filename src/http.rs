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

pub(crate) fn shared_request_limiter() -> &'static RequestLimiter {
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
    fetch_response_using(url, shared_http_client()).await
}

/// One restricted pool is reused for every image in a content download.
pub(crate) struct OriginRestrictedClient {
    client: Client,
    origins: Vec<String>,
}

impl OriginRestrictedClient {
    pub(crate) fn new(origins: &[String]) -> AppResult<Self> {
        let permissions = origins.to_vec();
        let origins = origins.to_vec();
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many image redirects");
                }
                let target = attempt.url();
                if !target.username().is_empty()
                    || target.password().is_some()
                    || !origins.contains(&target.origin().ascii_serialization())
                {
                    return attempt
                        .error("image redirect is outside the extension network permissions");
                }
                attempt.follow()
            }))
            .build()?;
        Ok(Self {
            client,
            origins: permissions,
        })
    }

    pub(crate) async fn fetch(&self, url: &str) -> AppResult<LimitedHttpResponse> {
        let parsed = reqwest::Url::parse(url)?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || !self
                .origins
                .contains(&parsed.origin().ascii_serialization())
        {
            return Err("image URL is outside the extension network permissions".into());
        }
        fetch_response_using(url, &self.client).await
    }
}

async fn fetch_response_using(url: &str, client: &Client) -> AppResult<LimitedHttpResponse> {
    let permit = shared_request_limiter().acquire().await;
    let response = client
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

pub(crate) fn decode_body(body: Vec<u8>) -> String {
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
    async fn image_redirect_cannot_escape_the_declared_origin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", source.local_addr().unwrap());
        let location = format!("http://{}/private.png", target.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let mut request = [0; 2048];
            stream.read(&mut request).await.unwrap();
            let response = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = OriginRestrictedClient::new(&[origin.clone()]).unwrap();
        assert!(client.fetch(&format!("{origin}/image.png")).await.is_err());
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
        assert!(client
            .fetch("https://ungranted.invalid/image.png")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn image_client_follows_a_redirect_inside_the_allowed_origin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in [302, 200] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 2048];
                stream.read(&mut request).await.unwrap();
                let response = if status == 302 {
                    "HTTP/1.1 302 Found\r\nLocation: /final.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\npng"
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = OriginRestrictedClient::new(&[origin.clone()]).unwrap();
        let limited = client.fetch(&format!("{origin}/image.png")).await.unwrap();
        let (response, _permit) = limited.into_parts();
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"png");
        server.await.unwrap();
    }

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
