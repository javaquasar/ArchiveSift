use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

use super::{Error, PeerLumeConfig, Result};
use crate::secure_store::{Secret, SecretStore, SystemSecretStore};

pub const MAX_RESPONSE: usize = 256 * 1024 + 1;
const MAX_REQUEST: usize = 64 * 1024;
const MAX_STDERR: usize = 16 * 1024;

/// Only the CLI transport sees credentials. Test transports do not need secrets.
pub trait ControlTransport: Send + Sync {
    fn request(&self, instance: Option<&str>, method: Value) -> Result<Reply>;
}

#[derive(Deserialize)]
pub struct Reply {
    pub protocol_version: u32,
    pub instance_id: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl Reply {
    pub fn value(self, instance: Option<&str>) -> Result<(String, Value)> {
        if self.protocol_version != 1 {
            return Err(Error::Protocol);
        }
        if let Some(code) = self.error {
            return Err(Error::remote(&code));
        }
        if self.instance_id.is_empty()
            || self.instance_id.len() > 128
            || instance.is_some_and(|id| id != self.instance_id)
        {
            return Err(Error::OwnerChanged);
        }
        Ok((self.instance_id, self.result.ok_or(Error::Protocol)?))
    }
}

pub struct CliTransport {
    executable: PathBuf,
    profile: PathBuf,
    token: Secret,
    timeout: Duration,
}

impl CliTransport {
    pub fn connect(config: &PeerLumeConfig) -> Result<Self> {
        if !config.enabled {
            return Err(Error::Disabled);
        }
        config.validate()?;
        let token = SystemSecretStore
            .get(&config.secret_target()?)
            .map_err(|_| Error::SecretStore)?
            .ok_or(Error::TokenMissing)?;
        Self::new(config, token, Duration::from_secs(15))
    }

    pub fn new(config: &PeerLumeConfig, token: Secret, timeout: Duration) -> Result<Self> {
        config.validate()?;
        validate_token(token.expose_secret())?;
        let executable = PathBuf::from(&config.executable)
            .canonicalize()
            .map_err(|_| Error::Configuration)?;
        let profile = PathBuf::from(&config.profile)
            .canonicalize()
            .map_err(|_| Error::Configuration)?;
        if !executable.is_file() || !profile.is_dir() {
            return Err(Error::Configuration);
        }
        Ok(Self {
            executable,
            profile,
            token,
            timeout,
        })
    }
}

pub fn validate_token(bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > 128 || !bytes.iter().all(|b| b.is_ascii_graphic()) {
        return Err(Error::TokenMissing);
    }
    Ok(())
}

async fn read_limited(reader: impl AsyncRead + Unpin, max: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut result = Zeroizing::new(Vec::new());
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut result)
        .await
        .map_err(|_| Error::Transport)?;
    if result.len() > max {
        return Err(Error::ResponseTooLarge);
    }
    Ok(result)
}

impl ControlTransport for CliTransport {
    fn request(&self, instance: Option<&str>, method: Value) -> Result<Reply> {
        let mut input = serde_json::to_vec(&json!({"instance_id":instance,"method":method}))
            .map_err(|_| Error::Protocol)?;
        input.push(b'\n');
        if input.len() > MAX_REQUEST {
            return Err(Error::Protocol);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Transport)?;
        runtime.block_on(async {
            let mut command = tokio::process::Command::new(&self.executable);
            command
                .arg("request")
                .arg("--profile")
                .arg(&self.profile)
                .current_dir(&self.profile)
                .env(
                    "PEERLUME_CONTROL_TOKEN",
                    std::str::from_utf8(self.token.expose_secret())
                        .map_err(|_| Error::TokenMissing)?,
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
            let mut child = command.spawn().map_err(|_| Error::Transport)?;
            let mut stdin = child.stdin.take().ok_or(Error::Transport)?;
            let stdout = child.stdout.take().ok_or(Error::Transport)?;
            let stderr = child.stderr.take().ok_or(Error::Transport)?;
            let run = async {
                let write = async {
                    stdin
                        .write_all(&input)
                        .await
                        .map_err(|_| Error::Transport)?;
                    drop(stdin);
                    Ok(())
                };
                let wait = async { child.wait().await.map_err(|_| Error::Transport) };
                let (_, out, _, status) = tokio::try_join!(
                    write,
                    read_limited(stdout, MAX_RESPONSE),
                    read_limited(stderr, MAX_STDERR),
                    wait
                )?;
                let reply: Reply = serde_json::from_slice(&out).map_err(|_| Error::Protocol)?;
                if !status.success() && reply.error.is_none() {
                    return Err(Error::Transport);
                }
                Ok(reply)
            };
            match tokio::time::timeout(self.timeout, run).await {
                Ok(Ok(reply)) => Ok(reply),
                outcome => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    match outcome {
                        Err(_) => Err(Error::Timeout),
                        Ok(Err(e)) => Err(e),
                        _ => unreachable!(),
                    }
                }
            }
        })
    }
}

pub(super) fn call<T: serde::de::DeserializeOwned>(
    transport: &dyn ControlTransport,
    instance: Option<&str>,
    method: Value,
) -> Result<(String, T)> {
    let (id, value) = transport.request(instance, method)?.value(instance)?;
    let result = serde_json::from_value(value).map_err(|_| Error::Protocol)?;
    Ok((id, result))
}

#[derive(Deserialize)]
pub struct Discovery {
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
    pub grants: Grants,
}
#[derive(Deserialize)]
pub struct Grants {
    pub manage: bool,
}

pub fn discover(transport: &dyn ControlTransport) -> Result<String> {
    let (id, discovery): (_, Discovery) = call(transport, None, json!({"name":"discover"}))?;
    if discovery.protocol_version != 1 {
        return Err(Error::Protocol);
    }
    if !discovery.grants.manage {
        return Err(Error::PermissionDenied);
    }
    for required in ["preview", "add", "operations", "list"] {
        if !discovery.capabilities.iter().any(|cap| cap == required) {
            return Err(Error::Protocol);
        }
    }
    Ok(id)
}
