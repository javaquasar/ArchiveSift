//! Sources are local user configuration, never a publisher-controlled executable.
use super::{ExtensionManager, PreparedPackage};
use crate::{
    secure_store::{SecretStore, SystemSecretStore},
    AppResult,
};
use archive_sift_extension_package::{Package, MAX_PACKAGE};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UpdateSource {
    Directory {
        path: PathBuf,
    },
    Github {
        repository: String,
        asset_prefix: String,
    },
}
impl UpdateSource {
    pub fn validate(&self) -> AppResult<()> {
        match self {
            Self::Directory { path } if path.is_absolute() => Ok(()),
            Self::Github {
                repository,
                asset_prefix,
            } if repository.split('/').count() == 2
                && repository.split('/').all(|s| {
                    !s.is_empty()
                        && s != "."
                        && s != ".."
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                })
                && !asset_prefix.is_empty()
                && asset_prefix.len() < 128
                && asset_prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) =>
            {
                Ok(())
            }
            _ => Err("invalid extension update source".into()),
        }
    }
    pub fn credential_target(&self) -> Option<String> {
        match self {
            Self::Github { repository, .. } => Some(crate::secure_store::host_secret_target(
                &format!("github/{repository}"),
            )),
            _ => None,
        }
    }
}

impl ExtensionManager {
    pub async fn check_update(&self, id: &str) -> AppResult<PreparedPackage> {
        let state = self.state()?;
        let installed = state.plugins.get(id).ok_or("unknown extension")?;
        let active = installed
            .versions
            .get(&installed.active)
            .ok_or("missing active version")?;
        let publisher = active
            .publisher
            .as_deref()
            .ok_or("import a signed publisher version before using update sources")?;
        let source = installed
            .source
            .as_ref()
            .ok_or("configure an update source first")?;
        source.validate()?;
        let current = semver::Version::parse(&active.version)?;
        let mut best: Option<Candidate> = None;
        match source {
            UpdateSource::Directory { path } => {
                let mut paths = std::fs::read_dir(path)?
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "asift-plugin"))
                    .collect::<Vec<_>>();
                paths.sort();
                if paths.len() > 128 {
                    return Err("update directory contains more than 128 packages".into());
                }
                for path in paths {
                    if let Ok(p) = Package::read(&path) {
                        if let Some(version) =
                            candidate_version(&p, id, publisher, &current, best.as_ref())
                        {
                            best = Some(Candidate {
                                version,
                                digest: p.artifact_digest,
                                path: Some(path),
                                bytes: None,
                            });
                        }
                    }
                }
            }
            UpdateSource::Github {
                repository,
                asset_prefix,
            } => {
                let token = SystemSecretStore
                    .get(
                        &source
                            .credential_target()
                            .ok_or("missing credential target")?,
                    )?
                    .ok_or("GitHub credential is missing; save it in the system password store")?;
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(45))
                    .user_agent(concat!("ArchiveSift/", env!("CARGO_PKG_VERSION")))
                    .build()?;
                let token = String::from_utf8(token.expose_secret().to_vec())
                    .map(zeroize::Zeroizing::new)
                    .map_err(|_| "invalid GitHub credential")?;
                let url = format!("https://api.github.com/repos/{repository}/releases?per_page=50");
                let bytes = fetch(&client, &url, Some(&token), 2 * 1024 * 1024).await?;
                let releases: Vec<Release> = serde_json::from_slice(&bytes)
                    .map_err(|_| "invalid GitHub releases response")?;
                let assets = releases
                    .into_iter()
                    .filter(|r| !r.draft && !r.prerelease)
                    .flat_map(|r| r.assets)
                    .filter(|a| {
                        a.name.starts_with(asset_prefix) && a.name.ends_with(".asift-plugin")
                    })
                    .take(8);
                for asset in assets {
                    // Construct the API URL from a numeric ID. Never send credentials to an asset-supplied URL.
                    let bytes = fetch(
                        &client,
                        &format!(
                            "https://api.github.com/repos/{repository}/releases/assets/{}",
                            asset.id
                        ),
                        Some(&token),
                        MAX_PACKAGE,
                    )
                    .await?;
                    let p = Package::decode(&bytes)?;
                    if let Some(version) =
                        candidate_version(&p, id, publisher, &current, best.as_ref())
                    {
                        best = Some(Candidate {
                            version,
                            digest: p.artifact_digest,
                            path: None,
                            bytes: Some(bytes),
                        });
                    }
                }
            }
        }
        let Candidate {
            digest: expected,
            path,
            bytes,
            ..
        } = best.ok_or("no newer package signed by this publisher was found")?;
        let prepared = match (path, bytes) {
            (Some(p), _) => self.prepare(&p)?,
            (_, Some(b)) => self.prepare_bytes(b)?,
            _ => unreachable!(),
        };
        if prepared.package.artifact_digest != expected
            || prepared.package.signer.as_deref() != Some(publisher)
        {
            return Err("update source changed during verification".into());
        }
        Ok(prepared)
    }
}

struct Candidate {
    version: semver::Version,
    digest: String,
    path: Option<PathBuf>,
    bytes: Option<Vec<u8>>,
}
fn candidate_version(
    package: &Package,
    id: &str,
    publisher: &str,
    current: &semver::Version,
    best: Option<&Candidate>,
) -> Option<semver::Version> {
    if package.signer.as_deref() != Some(publisher) {
        return None;
    }
    let manifest: super::PluginManifest =
        serde_json::from_slice(package.files.get("plugin.json")?).ok()?;
    manifest.validate().ok()?;
    let version = semver::Version::parse(&manifest.version).ok()?;
    (manifest.id == id
        && version.pre.is_empty()
        && &version > current
        && best.is_none_or(|b| version > b.version))
    .then_some(version)
}

#[derive(Deserialize)]
struct Release {
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    id: u64,
    name: String,
}

fn allowed_download(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    limit: usize,
) -> AppResult<Vec<u8>> {
    let mut url = reqwest::Url::parse(url)?;
    for _ in 0..=5 {
        if !allowed_download(&url) {
            return Err("unapproved GitHub download destination".into());
        }
        let request = download_request(client, &url, token, limit);
        let response = request.send().await.map_err(|_| "GitHub download failed")?;
        if response.status().is_redirection() {
            url = url.join(
                response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or("redirect without a location")?
                    .to_str()?,
            )?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "GitHub request failed (HTTP {})",
                response.status().as_u16()
            )
            .into());
        }
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err("GitHub response exceeds size limit".into());
        }
        let mut bytes = vec![];
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "GitHub response interrupted")?;
            if bytes.len() + chunk.len() > limit {
                return Err("GitHub response exceeds size limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    Err("too many GitHub redirects".into())
}

fn download_request(
    client: &reqwest::Client,
    url: &reqwest::Url,
    token: Option<&str>,
    limit: usize,
) -> reqwest::RequestBuilder {
    let mut request = client.get(url.clone()).header(
        "Accept",
        if limit == MAX_PACKAGE {
            "application/octet-stream"
        } else {
            "application/vnd.github+json"
        },
    );
    if url.host_str() == Some("api.github.com") {
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_credentials_are_sensitive_and_never_attached_to_asset_cdn_requests() {
        let client = reqwest::Client::new();
        let api = download_request(
            &client,
            &reqwest::Url::parse("https://api.github.com/repos/owner/example/releases").unwrap(),
            Some("unit-only"),
            1024,
        )
        .build()
        .unwrap();
        assert!(api.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
        let cdn = download_request(
            &client,
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/asset").unwrap(),
            Some("unit-only"),
            MAX_PACKAGE,
        )
        .build()
        .unwrap();
        assert!(!cdn.headers().contains_key(reqwest::header::AUTHORIZATION));
    }
    #[test]
    fn rejects_arbitrary_sources_and_redirect_destinations() {
        for repo in ["../x", "owner/repo/extra", "owner/repo?token=x", "/repo"] {
            assert!(UpdateSource::Github {
                repository: repo.into(),
                asset_prefix: "plugin-".into()
            }
            .validate()
            .is_err());
        }
        for u in [
            "http://api.github.com/x",
            "https://api.github.com.evil.test/x",
            "https://user@api.github.com/x",
            "https://127.0.0.1/x",
            "https://github.com:444/x",
        ] {
            assert!(!allowed_download(&reqwest::Url::parse(u).unwrap()));
        }
        assert!(allowed_download(
            &reqwest::Url::parse("https://release-assets.githubusercontent.com/x").unwrap()
        ));
    }
}
