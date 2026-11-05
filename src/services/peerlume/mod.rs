//! Reviewed, durable handoff to an already running PeerLume owner.
pub mod client;
mod config;
mod files;
mod journal;
mod mode;
mod setup;

use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppConfig;
use crate::db::Database;
use client::{call, discover, ControlTransport};
pub use config::{relative_destination, PeerLumeConfig};
use journal::Journal;
pub use mode::{DownloadMode, ModeConfig, ModeSetup, SetupState, UploadRatio};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Destination,
    Disabled,
    TokenMissing,
    SecretStore,
    Transport,
    Timeout,
    ResponseTooLarge,
    Protocol,
    OwnerChanged,
    PermissionDenied,
    NotRunning,
    Busy,
    PathDenied,
    Source,
    Staging,
    Persistence,
    ReconciliationRequired,
    Remote,
    ModeConfiguration,
    ReportingPermission,
    ModeMetadata,
    SetupExpired,
    ModeChanged,
}
impl Error {
    fn remote(code: &str) -> Self {
        match code {
            "unauthorized" | "permission_denied" => Self::PermissionDenied,
            "disabled" | "not_running" => Self::NotRunning,
            "busy" => Self::Busy,
            "conflict" => Self::OwnerChanged,
            "path_denied" => Self::PathDenied,
            "reconciliation_required" | "unknown_operation" => Self::ReconciliationRequired,
            "incompatible_protocol" | "invalid_request" => Self::Protocol,
            _ => Self::Remote,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration => "Choose an absolute PeerLume control executable and an existing profile directory.",
            Self::Destination => "Choose an existing relative destination inside PeerLume's download root, with normal path components only.",
            Self::Disabled => "Enable the PeerLume integration in Settings first.",
            Self::TokenMissing => "Save a valid dedicated PeerLume control token in Settings.",
            Self::SecretStore => "The system credential store is unavailable. Unlock the desktop keyring on Linux.",
            Self::Transport => "Cannot communicate with the PeerLume control CLI. Check its path and selected profile.",
            Self::Timeout => "PeerLume did not respond in time. Refresh the handoff list before trying again.",
            Self::ResponseTooLarge => "PeerLume control output exceeded the configured limit.",
            Self::Protocol => "PeerLume returned an unsupported or invalid control response.",
            Self::OwnerChanged => "PeerLume's owner instance changed. Test the connection and review the torrent again.",
            Self::PermissionDenied => "PeerLume denied access. Enable its control API and grant this client manage permission.",
            Self::NotRunning => "Start PeerLume with this profile and enable its restricted control API.",
            Self::Busy => "PeerLume is busy. Refresh the handoff list before trying again.",
            Self::PathDenied => "PeerLume rejected the path. The relative payload destination must already exist under its granted download root.",
            Self::Source => "A valid local .torrent attachment is required. Magnet-only handoff is not supported by this PeerLume API.",
            Self::Staging => "Cannot safely stage or verify the torrent inside the selected PeerLume profile.",
            Self::Persistence => "Cannot read or save the ArchiveSift handoff journal.",
            Self::ReconciliationRequired => "Delivery is uncertain. Refresh to reconcile; no automatic add retry will be sent.",
            Self::Remote => "PeerLume rejected the operation. Inspect its local application for details.",
            Self::ModeConfiguration => "Choose a nonzero freeze threshold up to 9223372036854775807 bytes and a positive upload multiplier range with minimum no greater than maximum.",
            Self::ReportingPermission => "Custom PeerLume modes require both manage and reporting permission. The torrent will not start in a fallback mode.",
            Self::ModeMetadata => "Custom PeerLume modes require a v1 or hybrid torrent with a nonzero total size.",
            Self::SetupExpired => "The confirmed setup expired. The torrent will not be started automatically; review it in PeerLume.",
            Self::ModeChanged => "The torrent mode changed outside this handoff. It will not be started automatically; review it in PeerLume.",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct InfoHashes {
    pub v1: Option<String>,
    pub v2: Option<String>,
}
impl InfoHashes {
    fn validate(&self) -> Result<()> {
        if self.v1.is_none() && self.v2.is_none() {
            return Err(Error::Protocol);
        }
        for (value, length) in [(&self.v1, 40), (&self.v2, 64)] {
            if value.as_ref().is_some_and(|s| {
                s.len() != length
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            }) {
                return Err(Error::Protocol);
            }
        }
        Ok(())
    }
    fn keys(&self) -> Vec<String> {
        self.v1
            .iter()
            .map(|s| format!("v1:{s}"))
            .chain(self.v2.iter().map(|s| format!("v2:{s}")))
            .collect()
    }
    fn matches(&self, id: &str) -> bool {
        // PeerLume canonical IDs join "v1: <hex>" / "v2: <hex>" with ';'.
        self.keys().iter().any(|hash| {
            id.split(';')
                .any(|part| part.replace(' ', "").eq_ignore_ascii_case(hash))
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Preview {
    pub total_bytes: u64,
    pub file_count: usize,
    pub private: bool,
    pub info_hash_v1: Option<String>,
    pub info_hash_v2: Option<String>,
}
impl Preview {
    fn hashes(&self) -> InfoHashes {
        InfoHashes {
            v1: self.info_hash_v1.clone(),
            v2: self.info_hash_v2.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedTorrent {
    pub page_id: i64,
    pub title: String,
    pub preview: Preview,
    source: String,
    digest: String,
    profile: String,
    instance: String,
    reviewed_at: u64,
}

pub struct Preparation {
    pub torrents: Vec<PreparedTorrent>,
    pub errors: Vec<(i64, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Submitting,
    Accepted,
    Added,
    Linked,
    Failed,
    ReconciliationRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    AwaitingEngine,
    Downloading,
    Paused,
    Seeding,
    Checking,
    VerificationFailed,
    Error,
    Removed,
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handoff {
    pub id: String,
    pub page_id: i64,
    pub title: String,
    pub profile: String,
    pub hashes: InfoHashes,
    pub source: String,
    pub digest: String,
    pub destination: String,
    pub start: bool,
    pub instance: String,
    pub idempotency_key: String,
    pub deadline_unix_ms: u64,
    pub operation_id: Option<String>,
    pub torrent_id: Option<String>,
    pub state: HandoffState,
    pub transfer: Option<TransferState>,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub missing: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub download_mode: ModeConfig,
    #[serde(default)]
    pub mode_setup: Option<ModeSetup>,
    #[serde(default)]
    pub observed_mode: Option<DownloadMode>,
}
impl Handoff {
    pub fn pending_setup(&self) -> bool {
        self.mode_setup
            .as_ref()
            .is_some_and(|setup| setup.state.pending())
    }
}

#[derive(Deserialize)]
struct Operation {
    operation_id: String,
    torrent_id: String,
    command: String,
    state: OperationState,
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum OperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    ReconciliationRequired,
}
#[derive(Deserialize)]
struct Observation {
    id: String,
    state: TransferState,
    downloaded_bytes: u64,
    #[serde(default)]
    command_revision: u64,
    #[serde(default)]
    active_mode: Option<DownloadMode>,
    #[serde(default)]
    mode_state: Option<String>,
    #[serde(default)]
    active_freeze_after_bytes: Option<u64>,
    #[serde(default)]
    active_upload_ratio: Option<UploadRatio>,
}
#[derive(Deserialize)]
struct Page {
    revision: u64,
    controller_available: bool,
    resync_required: bool,
    next_offset: Option<usize>,
    torrents: Vec<Observation>,
}
#[derive(Deserialize)]
struct OperationsPage {
    operations: Vec<Operation>,
    next_offset: Option<usize>,
}

pub struct HandoffService<T> {
    config: AppConfig,
    transport: T,
    profile: String,
    journal: Journal,
}

impl<T: ControlTransport> HandoffService<T> {
    pub fn new(config: AppConfig, transport: T) -> Result<Self> {
        if !config.peerlume.enabled {
            return Err(Error::Disabled);
        }
        config.peerlume.validate()?;
        let profile = config.peerlume.profile_key()?;
        Database::new(&config.database_path)
            .initialize()
            .map_err(|_| Error::Persistence)?;
        let journal = Journal::open(Path::new(&config.database_path))?;
        Ok(Self {
            config,
            transport,
            profile,
            journal,
        })
    }

    pub fn test_connection(&self) -> Result<String> {
        client::discover_for_mode(&self.transport, &self.config.peerlume.download_mode)
    }

    pub fn prepare(&self, page_ids: &[i64]) -> Result<Preparation> {
        if page_ids.is_empty() || page_ids.len() > 100 {
            return Err(Error::Source);
        }
        let instance = discover(&self.transport)?;
        let database = Database::new(&self.config.database_path);
        let mut result = Preparation {
            torrents: vec![],
            errors: vec![],
        };
        let mut seen = std::collections::HashSet::new();
        for &page_id in page_ids {
            if !seen.insert(page_id) {
                continue;
            }
            let card = match database.torrent_card(page_id) {
                Ok(card) => card,
                Err(_) => {
                    result.errors.push((page_id, Error::Source.to_string()));
                    continue;
                }
            };
            let mut count = 0;
            for attachment in &card.attachments {
                if !attachment
                    .local_file_path
                    .to_ascii_lowercase()
                    .ends_with(".torrent")
                {
                    continue;
                }
                count += 1;
                if result.torrents.len() >= 100 {
                    return Err(Error::Source);
                }
                let run = || {
                    let bytes = files::read_archive(
                        &self.config.effective_download_dir(),
                        &attachment.local_file_path,
                    )?;
                    let hashes = files::hashes(&bytes)?;
                    let (source, digest) =
                        files::stage(Path::new(&self.config.peerlume.profile), &bytes)?;
                    let (_, preview): (_, Preview) = call(
                        &self.transport,
                        Some(&instance),
                        json!({"name":"preview","args":{"source":source}}),
                    )?;
                    preview.hashes().validate()?;
                    if preview.hashes() != hashes || preview.file_count == 0 {
                        return Err(Error::Protocol);
                    }
                    Ok(PreparedTorrent {
                        page_id,
                        title: card.content_title().chars().take(512).collect(),
                        preview,
                        source,
                        digest,
                        profile: self.profile.clone(),
                        instance: instance.clone(),
                        reviewed_at: now_ms(),
                    })
                };
                match run() {
                    Ok(torrent) => result.torrents.push(torrent),
                    Err(error) => result.errors.push((page_id, error.to_string())),
                }
            }
            if count == 0 {
                result.errors.push((page_id, Error::Source.to_string()));
            }
        }
        Ok(result)
    }

    /// No hidden retries: the write-ahead reservation is committed before Add.
    pub fn submit(
        &self,
        prepared: &PreparedTorrent,
        destination: &str,
        start: bool,
    ) -> Result<Handoff> {
        self.submit_reviewed(
            prepared,
            destination,
            start,
            false,
            self.config.peerlume.download_mode,
        )
    }

    /// A new user confirmation may retry a *confirmed* failed operation. Never
    /// use this for ambiguous delivery, a missing torrent or an automatic retry.
    pub fn retry_failed(
        &self,
        prepared: &PreparedTorrent,
        destination: &str,
        start: bool,
    ) -> Result<Handoff> {
        self.submit_reviewed(
            prepared,
            destination,
            start,
            true,
            self.config.peerlume.download_mode,
        )
    }

    pub fn submit_with_mode(
        &self,
        prepared: &PreparedTorrent,
        destination: &str,
        start: bool,
        retry_failed: bool,
        mode: ModeConfig,
    ) -> Result<Handoff> {
        self.submit_reviewed(prepared, destination, start, retry_failed, mode)
    }

    fn submit_reviewed(
        &self,
        prepared: &PreparedTorrent,
        destination: &str,
        start: bool,
        retry_failed: bool,
        mode: ModeConfig,
    ) -> Result<Handoff> {
        let destination = relative_destination(destination)?;
        let instance = client::discover_for_mode(&self.transport, &mode)?;
        if mode.custom()
            && (prepared.preview.info_hash_v1.is_none() || prepared.preview.total_bytes == 0)
        {
            return Err(Error::ModeMetadata);
        }
        if prepared.profile != self.profile
            || prepared.instance != instance
            || now_ms().saturating_sub(prepared.reviewed_at) > 15 * 60 * 1000
        {
            return Err(Error::OwnerChanged);
        }
        files::verify(
            Path::new(&self.config.peerlume.profile),
            &prepared.source,
            &prepared.digest,
        )?;
        let snapshot = self.snapshot(&instance)?;
        let existing = snapshot
            .iter()
            .find(|row| prepared.preview.hashes().matches(&row.id));
        let record = Handoff {
            id: random_id()?,
            page_id: prepared.page_id,
            title: prepared.title.clone(),
            profile: self.profile.clone(),
            hashes: prepared.preview.hashes(),
            source: prepared.source.clone(),
            digest: prepared.digest.clone(),
            destination,
            start,
            instance: instance.clone(),
            idempotency_key: random_id()?,
            deadline_unix_ms: now_ms() + 15 * 60 * 1000,
            operation_id: None,
            torrent_id: existing.map(|row| row.id.clone()),
            state: if existing.is_some() {
                HandoffState::Linked
            } else {
                HandoffState::Submitting
            },
            transfer: existing.map(|row| row.state),
            downloaded_bytes: existing.map_or(0, |row| row.downloaded_bytes),
            total_bytes: prepared.preview.total_bytes,
            missing: false,
            error: None,
            download_mode: mode,
            mode_setup: (mode.custom() && existing.is_none()).then(ModeSetup::default),
            observed_mode: existing.and_then(|row| row.active_mode),
        };
        let _lease = self.record_lease(&record.id)?.ok_or(Error::Busy)?;
        let (mut record, reserved) = self.journal.reserve(record, retry_failed)?;
        if !reserved || existing.is_some() {
            return Ok(record);
        }
        let response: Result<(_, Operation)> = call(
            &self.transport,
            Some(&instance),
            json!({"name":"add","args":{
                "source":record.source,"destination":record.destination,"start":record.start && !record.download_mode.custom(),
                "idempotency_key":record.idempotency_key,"deadline_unix_ms":record.deadline_unix_ms
            }}),
        );
        match response {
            Ok((_, operation)) => apply_operation(&mut record, operation)?,
            Err(error) => {
                record.state = HandoffState::ReconciliationRequired;
                record.error = Some(error.to_string());
            }
        }
        self.journal.save(&record)?;
        Ok(record)
    }

    pub fn history(&self) -> Result<Vec<Handoff>> {
        self.journal.list(&self.profile)
    }

    pub fn refresh(&self) -> Result<Vec<Handoff>> {
        let instance = discover(&self.transport)?;
        let snapshot = self.snapshot(&instance)?;
        let mut records = self.history()?;
        let operations = if records.iter().any(|r| {
            r.operation_id.is_none()
                && matches!(
                    r.state,
                    HandoffState::Submitting | HandoffState::ReconciliationRequired
                )
        }) {
            self.operations(&instance)?
        } else {
            Vec::new()
        };
        for record in &mut records {
            let Some(_lease) = self.record_lease(&record.id)? else {
                continue;
            };
            *record = self.journal.get(&record.id)?;
            if record.instance == instance
                && matches!(
                    record.state,
                    HandoffState::Accepted | HandoffState::ReconciliationRequired
                )
            {
                if let Some(id) = &record.operation_id {
                    let operation: Result<(_, Operation)> = call(
                        &self.transport,
                        Some(&instance),
                        json!({"name":"operation","args":{"operation_id":id}}),
                    );
                    match operation {
                        Ok((_, operation)) => apply_operation(record, operation)?,
                        Err(error) => {
                            record.state = HandoffState::ReconciliationRequired;
                            record.error = Some(error.to_string());
                        }
                    }
                }
            }
            if let Some(row) = snapshot.iter().find(|row| record.hashes.matches(&row.id)) {
                record.torrent_id = Some(row.id.clone());
                record.transfer = Some(row.state);
                record.observed_mode = row.active_mode;
                record.downloaded_bytes = row.downloaded_bytes;
                record.missing = false;
                if matches!(
                    record.state,
                    HandoffState::Submitting | HandoffState::ReconciliationRequired
                ) || (record.instance != instance && record.state == HandoffState::Accepted)
                {
                    record.state = HandoffState::Linked;
                    record.error = None;
                }
            } else {
                record.transfer = None;
                record.observed_mode = None;
                record.missing = record.torrent_id.is_some();
                if let Some(operation) = operations
                    .iter()
                    .find(|op| op.command == "add" && record.hashes.matches(&op.torrent_id))
                {
                    record.operation_id = Some(operation.operation_id.clone());
                }
                if record.instance != instance
                    && !matches!(
                        record.state,
                        HandoffState::Added | HandoffState::Linked | HandoffState::Failed
                    )
                {
                    record.state = HandoffState::ReconciliationRequired;
                    record.error = Some(Error::ReconciliationRequired.to_string());
                } else if record.instance == instance
                    && record.operation_id.is_some()
                    && matches!(
                        record.state,
                        HandoffState::Submitting | HandoffState::ReconciliationRequired
                    )
                {
                    let id = record.operation_id.as_ref().unwrap();
                    let operation: Result<(_, Operation)> = call(
                        &self.transport,
                        Some(&instance),
                        json!({"name":"operation","args":{"operation_id":id}}),
                    );
                    match operation {
                        Ok((_, operation)) => apply_operation(record, operation)?,
                        Err(error) => {
                            record.state = HandoffState::ReconciliationRequired;
                            record.error = Some(error.to_string());
                        }
                    }
                } else if record.state == HandoffState::Submitting {
                    record.state = HandoffState::ReconciliationRequired;
                    record.error = Some(Error::ReconciliationRequired.to_string());
                }
            }
            self.journal.save(record)?;
        }
        Ok(records)
    }

    fn operations(&self, instance: &str) -> Result<Vec<Operation>> {
        let mut all = Vec::new();
        let mut offset = 0;
        for _ in 0..128 {
            let (_, page): (_, OperationsPage) = call(
                &self.transport,
                Some(instance),
                json!({"name":"operations","args":{"offset":offset,"limit":128}}),
            )?;
            if page.operations.len() > 128 {
                return Err(Error::Protocol);
            }
            let end = offset + page.operations.len();
            all.extend(page.operations);
            match page.next_offset {
                None => return Ok(all),
                Some(next) if next > offset && next == end => offset = next,
                _ => return Err(Error::Protocol),
            }
        }
        Err(Error::Busy)
    }

    fn snapshot(&self, instance: &str) -> Result<Vec<Observation>> {
        // A moving list cannot be assembled from unrelated revisions. Retry the
        // read from page zero; never retry a mutation as part of this loop.
        for _ in 0..3 {
            let mut revision = None;
            let mut offset = 0;
            let mut all = Vec::new();
            for _ in 0..128 {
                let response: Result<(_, Page)> = call(
                    &self.transport,
                    Some(instance),
                    json!({"name":"list","args":{"offset":offset,"limit":128,"revision":revision,"after_revision":null}}),
                );
                let (_, page) = match response {
                    Err(Error::OwnerChanged) if offset > 0 => break,
                    other => other?,
                };
                if !page.controller_available {
                    return Err(Error::NotRunning);
                }
                if page.resync_required || revision.is_some_and(|rev| rev != page.revision) {
                    break;
                }
                if page.torrents.len() > 128 || page.torrents.iter().any(|row| row.id.len() > 160) {
                    return Err(Error::Protocol);
                }
                revision = Some(page.revision);
                let end = offset + page.torrents.len();
                all.extend(page.torrents);
                match page.next_offset {
                    None => return Ok(all),
                    Some(next) if next == end && next > offset => offset = next,
                    _ => return Err(Error::Protocol),
                }
            }
        }
        Err(Error::Busy)
    }
}

fn apply_operation(record: &mut Handoff, operation: Operation) -> Result<()> {
    if operation.command != "add"
        || operation.operation_id.is_empty()
        || operation.operation_id.len() > 128
        || record
            .operation_id
            .as_ref()
            .is_some_and(|id| id != &operation.operation_id)
        || (!operation.torrent_id.is_empty() && !record.hashes.matches(&operation.torrent_id))
    {
        return Err(Error::Protocol);
    }
    record.operation_id = Some(operation.operation_id);
    if !operation.torrent_id.is_empty() {
        record.torrent_id = Some(operation.torrent_id);
    }
    record.state = match operation.state {
        OperationState::Queued | OperationState::Running => HandoffState::Accepted,
        OperationState::Succeeded => HandoffState::Added,
        OperationState::Failed | OperationState::Cancelled => HandoffState::Failed,
        OperationState::ReconciliationRequired => HandoffState::ReconciliationRequired,
    };
    record.error = operation.error.map(|code| Error::remote(&code).to_string());
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| Error::Persistence)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
pub(crate) mod tests;
