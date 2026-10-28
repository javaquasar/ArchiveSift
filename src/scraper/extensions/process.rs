use std::path::{Path, PathBuf};
use std::process::Stdio;

use archive_sift_scraper_api::{to_json_bounded, Action, Request, RpcRequest, RpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::manifest::{Limits, PluginManifest};
use crate::AppResult;

pub struct ProcessSession {
    // kill_on_drop terminates a timed-out or cancelled direct child.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    limits: Limits,
}

impl ProcessSession {
    pub async fn spawn(manifest: &PluginManifest, directory: &Path) -> AppResult<Self> {
        if !manifest.trusted {
            return Err("native process extension requires trusted=true".into());
        }
        // Re-check the executable immediately before each launch.
        let verification = manifest.clone();
        let package = directory.to_path_buf();
        let entry =
            tokio::task::spawn_blocking(move || verification.verified_process_entry(&package))
                .await??;
        let mut command = Command::new(&entry);
        command
            .current_dir(directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or("missing extension stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("missing extension stdout")?);
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
            limits: manifest.limits.clone(),
        })
    }

    pub async fn execute(&mut self, params: Request) -> AppResult<Action> {
        let id = self.next_id;
        self.next_id += 1;
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "execute".into(),
            params,
        };
        let mut bytes = to_json_bounded(&request, 8 * 1024 * 1024 - 1)
            .map_err(|_| "process extension request exceeds protocol limit")?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        let mut line = Vec::new();
        (&mut self.stdout)
            .take(self.limits.max_output_bytes as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if line.len() > self.limits.max_output_bytes || !line.ends_with(b"\n") {
            return Err(
                "process extension returned an oversized or incomplete RPC response".into(),
            );
        }
        parse_response(&line, id)
    }
}

fn parse_response(line: &[u8], id: u64) -> AppResult<Action> {
    let response: RpcResponse = serde_json::from_slice(line)?;
    if response.jsonrpc != "2.0" || response.id != id {
        return Err("process extension RPC version or response ID mismatch".into());
    }
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => {
            Err(format!("process extension error {}: {}", error.code, error.message).into())
        }
        _ => Err("process extension must return exactly one result or error".into()),
    }
}

pub(crate) struct ProcessBackend {
    pub directory: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_rejects_wrong_ids_versions_ambiguous_results_and_unframed_logs() {
        let good = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"Listing":{"urls":[],"next_page_url":null}}});
        assert!(matches!(
            parse_response(&serde_json::to_vec(&good).unwrap(), 1).unwrap(),
            Action::Listing(_)
        ));
        assert!(parse_response(&serde_json::to_vec(&good).unwrap(), 2).is_err());
        let mut wrong = good.clone();
        wrong["jsonrpc"] = "1.0".into();
        assert!(parse_response(&serde_json::to_vec(&wrong).unwrap(), 1).is_err());
        let mut ambiguous = good;
        ambiguous["error"] = serde_json::json!({"code":-32000,"message":"failure"});
        assert!(parse_response(&serde_json::to_vec(&ambiguous).unwrap(), 1).is_err());
        for bytes in [
            b"log line".as_slice(),
            br#"{"jsonrpc":"2.0","id":1}"#,
            br#"{"jsonrpc":"2.0","id":1,"result":null,"error":null}"#,
        ] {
            assert!(parse_response(bytes, 1).is_err());
        }
        let error =
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"intentional failure"}}"#;
        assert!(parse_response(error, 1)
            .unwrap_err()
            .to_string()
            .contains("intentional failure"));
    }
}
