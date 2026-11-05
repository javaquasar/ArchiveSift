use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroize;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_CONNECTIONS: usize = 8;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Disabled,
    Unauthorized,
    PermissionDenied,
    InvalidArguments,
    NotRunning,
    Busy,
    TooLarge,
    LibraryUnavailable,
    DestinationNotConfigured,
    OperationFailed,
}

impl ErrorCode {
    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => "Agent access is disabled in the running application.",
            Self::Unauthorized => {
                "Set a valid ARCHIVESIFT_CONTROL_TOKEN using the agent host's secret facility."
            }
            Self::PermissionDenied => {
                "Queue changes and downloads require a separate permission in Settings."
            }
            Self::InvalidArguments => {
                "The tool name or arguments do not match the advertised schema."
            }
            Self::NotRunning => "No enabled ArchiveSift instance is available for this profile.",
            Self::Busy => "The application is busy. Retry the request later.",
            Self::TooLarge => {
                "The request or response exceeds its limit. Reduce the page size or filters."
            }
            Self::LibraryUnavailable => "The application's selected library is unavailable.",
            Self::DestinationNotConfigured => {
                "Select a download destination in the application before queuing URLs."
            }
            Self::OperationFailed => "The application could not complete this operation.",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub instance_id: String,
    pub token: String,
    pub tool: String,
    pub arguments: Value,
}

impl Drop for Request {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub protocol_version: u32,
    pub instance_id: String,
    pub result: Option<Value>,
    pub error: Option<ErrorCode>,
}

impl Response {
    pub fn error(instance: &str, error: ErrorCode) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            instance_id: instance.into(),
            result: None,
            error: Some(error),
        }
    }
    pub fn success(instance: &str, result: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            instance_id: instance.into(),
            result: Some(result),
            error: None,
        }
    }
}

pub(crate) fn encode(response: &Response) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(response).unwrap_or_default();
    if bytes.len() >= MAX_RESPONSE_BYTES {
        bytes = serde_json::to_vec(&Response::error(&response.instance_id, ErrorCode::TooLarge))
            .unwrap();
    }
    bytes.push(b'\n');
    bytes
}
