use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerLumeConfig {
    pub enabled: bool,
    pub executable: String,
    pub profile: String,
    pub destination: String,
    pub start: bool,
    pub download_mode: super::ModeConfig,
}

impl Default for PeerLumeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: String::new(),
            profile: String::new(),
            destination: "incoming".into(),
            start: false,
            download_mode: Default::default(),
        }
    }
}

impl PeerLumeConfig {
    /// Pure validation, also used by the JSON editor. No filesystem or network IO.
    pub fn validate(&self) -> Result<()> {
        self.download_mode.validate()?;
        if !self.enabled {
            return Ok(());
        }
        if !Path::new(&self.executable).is_absolute()
            || !Path::new(&self.profile).is_absolute()
            || [&self.executable, &self.profile]
                .iter()
                .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err(Error::Configuration);
        }
        relative_destination(&self.destination)?;
        Ok(())
    }

    pub fn profile_key(&self) -> Result<String> {
        let path = Path::new(&self.profile)
            .canonicalize()
            .map_err(|_| Error::Configuration)?;
        if !path.is_dir() {
            return Err(Error::Configuration);
        }
        let identity = path.to_str().ok_or(Error::Configuration)?.to_owned();
        #[cfg(windows)]
        let identity = identity.to_lowercase();
        Ok(format!("{:x}", Sha256::digest(identity.as_bytes())))
    }

    pub fn secret_target(&self) -> Result<String> {
        Ok(crate::secure_store::host_secret_target(&format!(
            "peerlume/{}",
            self.profile_key()?
        )))
    }
}

/// The server performs the final download-root containment check.
pub fn relative_destination(value: &str) -> Result<String> {
    let value = value.replace('\\', "/");
    if value.is_empty() || value.len() > 1024 {
        return Err(Error::Destination);
    }
    for part in value.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with([' ', '.'])
            || part
                .chars()
                .any(|c| c.is_control() || ":<>\"|?*".contains(c))
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ["COM", "LPT"].iter().any(|prefix| {
                stem.strip_prefix(prefix)
                    .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
            })
        {
            return Err(Error::Destination);
        }
    }
    Ok(value)
}
