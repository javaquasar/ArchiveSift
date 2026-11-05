use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::{
    wire::{
        self, ErrorCode, Request, Response, MAX_CONNECTIONS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
        PROTOCOL_VERSION,
    },
    AgentService,
};
use crate::{app::AppConfig, services::UrlQueueService, ui::UiLog};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    protocol_version: u32,
    instance_id: String,
    address: SocketAddr,
}

pub struct AgentSession {
    service: Arc<AgentService>,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    endpoint: PathBuf,
    _lock: File,
}

struct ClientPermit(Arc<AtomicUsize>);
impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn canonical_profile(config: &Path) -> Result<PathBuf, ErrorCode> {
    if config.is_file() {
        return config.canonicalize().map_err(|_| ErrorCode::NotRunning);
    }
    let parent = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent
        .canonicalize()
        .map_err(|_| ErrorCode::NotRunning)?
        .join(config.file_name().ok_or(ErrorCode::InvalidArguments)?))
}

pub fn endpoint_path(config: &Path) -> Result<PathBuf, ErrorCode> {
    let config = canonical_profile(config)?;
    let mut name = config
        .file_name()
        .ok_or(ErrorCode::InvalidArguments)?
        .to_os_string();
    name.push(".agent-endpoint.json");
    Ok(config.with_file_name(name))
}

fn read_endpoint(path: &Path) -> Result<Endpoint, ErrorCode> {
    let file = File::open(path).map_err(|_| ErrorCode::NotRunning)?;
    let mut bytes = Vec::new();
    file.take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| ErrorCode::NotRunning)?;
    if bytes.len() > 4096 {
        return Err(ErrorCode::NotRunning);
    }
    let endpoint: Endpoint = serde_json::from_slice(&bytes).map_err(|_| ErrorCode::NotRunning)?;
    if endpoint.protocol_version != PROTOCOL_VERSION
        || endpoint.instance_id.len() != 32
        || !endpoint
            .instance_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || endpoint.address.ip() != std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
        || endpoint.address.port() == 0
    {
        return Err(ErrorCode::NotRunning);
    }
    Ok(endpoint)
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl AgentSession {
    /// Creates a disabled session. Only the UI activates it after accepting the
    /// asynchronous startup result; cancelling startup cannot grant access.
    pub fn start(
        config: AppConfig,
        queue: Arc<UrlQueueService>,
        config_path: &Path,
        log: UiLog,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(Self, Zeroizing<String>), ErrorCode> {
        let parent = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(|_| ErrorCode::OperationFailed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if parent
                .metadata()
                .map_err(|_| ErrorCode::OperationFailed)?
                .permissions()
                .mode()
                & 0o022
                != 0
            {
                return Err(ErrorCode::PermissionDenied);
            }
        }
        let endpoint = endpoint_path(config_path)?;
        let lock_path = endpoint.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|_| ErrorCode::OperationFailed)?;
        lock.try_lock().map_err(|_| ErrorCode::Busy)?;
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|_| ErrorCode::OperationFailed)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ErrorCode::OperationFailed)?;
        let (service, token) = AgentService::new(config, queue, log, wake)?;
        let details = Endpoint {
            protocol_version: PROTOCOL_VERSION,
            instance_id: service.instance_id.clone(),
            address: listener
                .local_addr()
                .map_err(|_| ErrorCode::OperationFailed)?,
        };
        let temporary = endpoint.with_extension(format!("{}.tmp", service.instance_id));
        let write = (|| -> std::io::Result<()> {
            let mut file = private_file(&temporary)?;
            file.write_all(&serde_json::to_vec(&details).unwrap())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &endpoint)
        })();
        if write.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(ErrorCode::OperationFailed);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_service = Arc::clone(&service);
        let clients = Arc::new(AtomicUsize::new(0));
        let worker = thread::Builder::new()
            .name("archivesift-agent-listener".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    worker_service.expire_queries();
                    match listener.accept() {
                        Ok((mut stream, peer)) => {
                            if !peer.ip().is_loopback() {
                                continue;
                            }
                            if clients
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                    (n < MAX_CONNECTIONS).then_some(n + 1)
                                })
                                .is_err()
                            {
                                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                                let _ = stream.write_all(&wire::encode(&Response::error(
                                    &worker_service.instance_id,
                                    ErrorCode::Busy,
                                )));
                                continue;
                            }
                            let service = Arc::clone(&worker_service);
                            let clients = Arc::clone(&clients);
                            let permit = ClientPermit(clients);
                            let _ = thread::Builder::new()
                                .name("archivesift-agent-request".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                                    let mut reader = BufReader::new(&mut stream);
                                    let response =
                                        match read_frame(&mut reader, MAX_REQUEST_BYTES, true) {
                                            Ok(Some(bytes)) => {
                                                match serde_json::from_slice::<Request>(&bytes) {
                                                    Ok(request) => service.handle(&request),
                                                    Err(_) => Response::error(
                                                        &service.instance_id,
                                                        ErrorCode::InvalidArguments,
                                                    ),
                                                }
                                            }
                                            Err(error) => {
                                                Response::error(&service.instance_id, error)
                                            }
                                            Ok(None) => return,
                                        };
                                    drop(reader);
                                    let _ = stream.write_all(&wire::encode(&response));
                                });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20))
                        }
                        Err(_) => break,
                    }
                }
                worker_service.disable();
            })
            .map_err(|_| {
                let _ = fs::remove_file(&endpoint);
                ErrorCode::OperationFailed
            })?;
        Ok((
            Self {
                service,
                stop,
                listener: Some(worker),
                endpoint,
                _lock: lock,
            },
            token,
        ))
    }
    pub fn service(&self) -> &Arc<AgentService> {
        &self.service
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        self.service.disable();
        self.stop.store(true, Ordering::Release);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        if read_endpoint(&self.endpoint)
            .is_ok_and(|endpoint| endpoint.instance_id == self.service.instance_id)
        {
            let _ = fs::remove_file(&self.endpoint);
        }
    }
}

pub(crate) fn read_frame(
    reader: &mut impl BufRead,
    maximum: usize,
    newline: bool,
) -> Result<Option<Zeroizing<Vec<u8>>>, ErrorCode> {
    let mut bytes = Zeroizing::new(Vec::new());
    let count = reader
        .take(maximum as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ErrorCode::OperationFailed)?;
    if count == 0 {
        return Ok(None);
    }
    if count > maximum {
        return Err(ErrorCode::TooLarge);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    } else if newline {
        return Err(ErrorCode::InvalidArguments);
    }
    Ok(Some(bytes))
}

pub fn call(
    config: &Path,
    token: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<Response, ErrorCode> {
    let endpoint = read_endpoint(&endpoint_path(config)?)?;
    let request = Request {
        protocol_version: PROTOCOL_VERSION,
        instance_id: endpoint.instance_id.clone(),
        token: token.into(),
        tool: tool.into(),
        arguments,
    };
    let mut bytes =
        Zeroizing::new(serde_json::to_vec(&request).map_err(|_| ErrorCode::InvalidArguments)?);
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(ErrorCode::TooLarge);
    }
    let mut stream = TcpStream::connect_timeout(&endpoint.address, Duration::from_secs(2))
        .map_err(|_| ErrorCode::NotRunning)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|_| ErrorCode::OperationFailed)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| ErrorCode::OperationFailed)?;
    stream
        .write_all(&bytes)
        .map_err(|_| ErrorCode::NotRunning)?;
    let response = read_frame(&mut BufReader::new(stream), MAX_RESPONSE_BYTES, true)?
        .ok_or(ErrorCode::NotRunning)?;
    let response: Response =
        serde_json::from_slice(&response).map_err(|_| ErrorCode::OperationFailed)?;
    if response.protocol_version != PROTOCOL_VERSION || response.instance_id != endpoint.instance_id
    {
        return Err(ErrorCode::NotRunning);
    }
    Ok(response)
}
