use archive_sift_scraper_api::{FetchRequest, HttpMethod, HttpResponse};
use futures::StreamExt;
use reqwest::{header, Client};

use super::credentials::Credentials;
use super::manifest::PluginManifest;
use crate::AppResult;

pub struct HttpBroker {
    client: Client,
    manifest: PluginManifest,
    credentials: Credentials,
}

impl HttpBroker {
    #[cfg(test)]
    pub fn new(manifest: &PluginManifest) -> AppResult<Self> {
        Self::with_account(manifest, None)
    }

    #[cfg(test)]
    pub fn with_account(
        manifest: &PluginManifest,
        account: Option<&super::accounts::AccountProvider>,
    ) -> AppResult<Self> {
        Self::with_package(manifest, account, None)
    }
    pub fn with_package(
        manifest: &PluginManifest,
        account: Option<&super::accounts::AccountProvider>,
        bindings: Option<&[u8]>,
    ) -> AppResult<Self> {
        let mut credentials = Credentials::load(manifest)?;
        if let Some(bytes) = bindings {
            credentials.include_package(manifest, bytes)?;
        }
        if let Some(account) = account {
            credentials.include_account(account)?;
        }
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("ArchiveSift/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_millis(manifest.limits.timeout_ms))
                .build()?,
            manifest: manifest.clone(),
            credentials,
        })
    }

    async fn response(
        &self,
        request: &FetchRequest,
    ) -> AppResult<crate::http::LimitedHttpResponse> {
        let mut url = self.manifest.allowed_url(&request.url)?;
        if request
            .body
            .as_ref()
            .is_some_and(|body| body.len() > self.manifest.limits.max_response_bytes)
            || request.headers.len() > 32
        {
            return Err("extension HTTP request exceeds its limits".into());
        }
        let mut headers = header::HeaderMap::new();
        for entry in &request.headers {
            let name = header::HeaderName::from_bytes(entry.name.as_bytes())?;
            if matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "connection"
                    | "transfer-encoding"
                    | "proxy-authorization"
                    | "upgrade"
            ) || entry.value.len() > 8192
            {
                return Err("extension requested a forbidden HTTP header".into());
            }
            headers.insert(name, header::HeaderValue::from_str(&entry.value)?);
        }
        let permit = crate::http::shared_request_limiter().acquire().await;
        let mut method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
        };
        let mut body = request.body.clone();
        for _ in 0..=5 {
            // Every redirect is checked before sending headers or body.
            self.manifest.allowed_url(url.as_str())?;
            // Resolve secrets for each exact destination; they never enter guest DTOs.
            let credentials = self.credentials.headers(&url)?;
            let mut builder = self
                .client
                .request(method.clone(), url.clone())
                .headers(headers.clone())
                .headers(credentials);
            if let Some(body) = &body {
                builder = builder.body(body.clone());
            }
            let response = builder.send().await?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .ok_or("HTTP redirect has no Location header")?
                    .to_str()?;
                let target = url.join(location)?;
                self.manifest.allowed_url(target.as_str())?;
                // Credentials and request bodies never cross an origin boundary.
                if target.origin() != url.origin() {
                    return Err("extension HTTP redirect crosses an origin boundary".into());
                }
                if response.status().as_u16() == 303
                    || (method == reqwest::Method::POST
                        && matches!(response.status().as_u16(), 301 | 302))
                {
                    method = reqwest::Method::GET;
                    body = None;
                    headers.remove(header::CONTENT_TYPE);
                }
                url = target;
                continue;
            }
            return Ok(crate::http::LimitedHttpResponse::new(response, permit));
        }
        Err("extension exceeded HTTP redirect limit".into())
    }

    pub async fn fetch(&self, request: &FetchRequest) -> AppResult<HttpResponse> {
        let limited = self.response(request).await?;
        let (response, _permit) = limited.into_parts();
        let url = response.url().to_string();
        if response
            .content_length()
            .is_some_and(|length| length > self.manifest.limits.max_response_bytes as u64)
        {
            return Err("extension HTTP response exceeds its limit".into());
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > self.manifest.limits.max_response_bytes {
                return Err("extension HTTP response exceeds its limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(HttpResponse {
            url,
            status,
            body: crate::http::decode_text_body(bytes, content_type.as_deref()),
            content_type,
        });
    }

    pub async fn attachment(&self, url: &str) -> AppResult<crate::models::AttachmentData> {
        let limited = self.response(&FetchRequest::get(url)).await?;
        crate::http::read_attachment(limited).await
    }
}
