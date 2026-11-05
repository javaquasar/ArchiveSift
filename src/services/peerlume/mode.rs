//! Protocol-1 mode configuration. Never start a custom-mode torrent before its
//! mode application has been confirmed by the PeerLume engine.
use super::{Error, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadMode {
    #[default]
    Normal,
    NoProgress,
    #[serde(rename = "upload_ratio_1_5")]
    UploadRatio,
    HideDownloaded,
    FreezeAfter,
}
impl DownloadMode {
    pub const ALL: [Self; 5] = [
        Self::Normal,
        Self::NoProgress,
        Self::UploadRatio,
        Self::HideDownloaded,
        Self::FreezeAfter,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::NoProgress => "No progress reporting",
            Self::UploadRatio => "Reported upload multiplier",
            Self::HideDownloaded => "Hide downloaded bytes",
            Self::FreezeAfter => "Freeze metrics after threshold",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::Normal => "Download and report actual transfer metrics.",
            Self::NoProgress => "Download normally; report zero uploaded/downloaded and the full torrent size remaining, without a completed event.",
            Self::UploadRatio => "Download normally; report uploaded bytes as downloaded bytes multiplied by a random value in the chosen range for each announce.",
            Self::HideDownloaded => "Download normally; report zero downloaded bytes while upload and remaining bytes stay actual.",
            Self::FreezeAfter => "Report actual metrics until the received-byte threshold, then keep that metrics snapshot. PeerLume resets the snapshot when the mode is reapplied or the owner restarts.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadRatio {
    pub min_milli: u32,
    pub max_milli: u32,
}
impl Default for UploadRatio {
    fn default() -> Self {
        Self {
            min_milli: 1500,
            max_milli: 2500,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModeConfig {
    pub mode: DownloadMode,
    pub freeze_after_bytes: u64,
    pub upload_ratio: UploadRatio,
}
impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            mode: DownloadMode::Normal,
            freeze_after_bytes: 1024 * 1024 * 1024,
            upload_ratio: UploadRatio::default(),
        }
    }
}
impl ModeConfig {
    pub fn validate(&self) -> Result<()> {
        if (self.mode == DownloadMode::FreezeAfter
            && (self.freeze_after_bytes == 0 || self.freeze_after_bytes > i64::MAX as u64))
            || self.upload_ratio.min_milli == 0
            || self.upload_ratio.min_milli > self.upload_ratio.max_milli
        {
            return Err(Error::ModeConfiguration);
        }
        Ok(())
    }
    pub fn custom(&self) -> bool {
        self.mode != DownloadMode::Normal
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupState {
    #[default]
    AwaitingAdd,
    ApplyingMode,
    Starting,
    Complete,
    Failed,
    ReconciliationRequired,
}
impl SetupState {
    pub fn pending(self) -> bool {
        matches!(
            self,
            Self::AwaitingAdd | Self::ApplyingMode | Self::Starting
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MutationIntent {
    pub expected_revision: u64,
    pub idempotency_key: String,
    pub deadline_unix_ms: u64,
    pub operation_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModeSetup {
    pub state: SetupState,
    pub mode_request: Option<MutationIntent>,
    pub start_request: Option<MutationIntent>,
}
