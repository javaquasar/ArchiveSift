//! Versioned data contract shared by WASM components and process extensions.
//! This crate has no dependencies on the desktop app, HTTP, or persistence.

use serde::{Deserialize, Serialize};

mod json_budget;
mod wit_conversion;

pub use json_budget::{check_json_size, to_json_bounded};

pub const API_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Content,
    Listing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub api_version: u32,
    pub operation: Operation,
    pub url: String,
    pub response: Option<HttpResponse>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<Header>,
    pub body: Option<String>,
    /// Opaque continuation data returned with the HTTP response.
    pub state: Option<String>,
}

impl FetchRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: None,
            state: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribute {
    pub kind: String,
    pub label: String,
    pub value: String,
    pub source_url: Option<String>,
    pub is_female: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub url: String,
    pub preview_url: Option<String>,
    pub alt_text: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    pub title: String,
    pub images: Vec<Image>,
    pub attributes: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredUrl {
    pub url: String,
    pub operation: Operation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub urls: Vec<DiscoveredUrl>,
    pub next_page_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Fetch(FetchRequest),
    Content(Content),
    Listing(Listing),
}

/// JSON-RPC 2.0 is used only on process stdio. WASM uses the typed WIT world.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Request,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

/// Runs a trusted native extension. Stdout is exclusively a protocol channel.
pub fn serve_stdio(handler: impl Fn(Request) -> Result<Action, String>) -> std::io::Result<()> {
    use std::io::{BufRead, Read, Write};
    let input = std::io::stdin();
    let mut output = std::io::stdout().lock();
    let mut reader = input.lock();
    loop {
        let mut line = String::new();
        // Bound input even if a caller does not implement the host contract.
        let count = reader
            .by_ref()
            .take(8 * 1024 * 1024 + 1)
            .read_line(&mut line)?;
        if count == 0 {
            return Ok(());
        }
        if count > 8 * 1024 * 1024 || !line.ends_with('\n') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "oversized RPC request",
            ));
        }
        let request: RpcRequest = serde_json::from_str(&line)?;
        let result = if request.jsonrpc != "2.0" || request.method != "execute" {
            Err("unsupported RPC method or version".to_string())
        } else if request.params.api_version != API_VERSION {
            Err("unsupported scraper API version".to_string())
        } else {
            handler(request.params)
        };
        let (result, error) = match result {
            Ok(action) => (Some(action), None),
            Err(message) => (
                None,
                Some(RpcError {
                    code: -32000,
                    message,
                }),
            ),
        };
        serde_json::to_writer(
            &mut output,
            &RpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result,
                error,
            },
        )?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip_preserves_continuation_and_metadata() {
        let request = Request {
            api_version: API_VERSION,
            operation: Operation::Listing,
            url: "https://example.test/list".into(),
            response: Some(HttpResponse {
                url: "https://example.test/api".into(),
                status: 200,
                content_type: Some("application/json".into()),
                body: "{}".into(),
            }),
            state: Some("cursor-2".into()),
        };
        assert_eq!(
            serde_json::from_slice::<Request>(&serde_json::to_vec(&request).unwrap()).unwrap(),
            request
        );
        let action = Action::Content(Content {
            title: "Example".into(),
            images: vec![Image {
                url: "image.png".into(),
                preview_url: None,
                alt_text: Some("test".into()),
                width: Some(400),
                height: Some(300),
            }],
            attributes: vec![Attribute {
                kind: "description".into(),
                label: "Description".into(),
                value: "Full description".into(),
                source_url: None,
                is_female: None,
            }],
        });
        assert_eq!(
            serde_json::from_slice::<Action>(&serde_json::to_vec(&action).unwrap()).unwrap(),
            action
        );
    }
}
