use super::*;
use crate::secure_store::{SecretStore, SystemSecretStore};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckState {
    Ready,
    Disabled,
    MissingSettings,
    AuthorizationRequired,
    Incompatible,
    Conflict,
    Failed,
}
#[derive(Clone, Debug)]
pub struct CheckReport {
    pub state: CheckState,
    pub checks: Vec<String>,
}

impl ExtensionManager {
    pub async fn check(&self, id: &str, values: Option<&Value>) -> CheckReport {
        let entries = match package_entries(&self.root) {
            Ok(e) => e,
            Err(e) => return report(CheckState::Failed, e.to_string()),
        };
        for (directory, manifest) in entries {
            if let Ok(manifest) = manifest {
                if manifest.id == id {
                    return check_directory(&directory, manifest, values, true).await;
                }
            }
        }
        report(CheckState::Failed, "Extension not found")
    }
    pub async fn check_prepared(
        &self,
        prepared: &PreparedPackage,
        values: Option<&Value>,
    ) -> CheckReport {
        let mut manifest = prepared.manifest.clone();
        manifest.enabled = true;
        // A native preview validates metadata and hashes without executing a process.
        manifest.trusted = false;
        let result = check_directory(&prepared.directory, manifest, values, false).await;
        if matches!(
            result.state,
            CheckState::Ready | CheckState::MissingSettings | CheckState::AuthorizationRequired
        ) {
            if let Ok(entries) = package_entries(&self.root) {
                for (_, other) in entries {
                    if let Ok(other) = other {
                        if other.enabled
                            && other.id != prepared.manifest.id
                            && (other.source_key == prepared.manifest.source_key
                                || other.routes.iter().any(|a| {
                                    prepared.manifest.routes.iter().any(|b| {
                                        a.host == b.host
                                            && (super::super::manifest::path_matches(
                                                &a.path_prefix,
                                                &b.path_prefix,
                                            ) || super::super::manifest::path_matches(
                                                &b.path_prefix,
                                                &a.path_prefix,
                                            ))
                                    })
                                }))
                        {
                            return report(
                                CheckState::Conflict,
                                "Routes or source key conflict with an active extension",
                            );
                        }
                    }
                }
            }
        }
        result
    }
}
fn report(state: CheckState, message: impl Into<String>) -> CheckReport {
    CheckReport {
        state,
        checks: vec![message.into()],
    }
}
async fn check_directory(
    directory: &Path,
    manifest: PluginManifest,
    values: Option<&Value>,
    check_account: bool,
) -> CheckReport {
    if !manifest.enabled {
        return report(CheckState::Disabled, "Disabled in local state");
    }
    if let Err(e) = manifest.validate() {
        return report(CheckState::Incompatible, e.to_string());
    }
    let mut checks = vec!["Manifest and scraper API 1.0: compatible".into()];
    let result = (|| -> AppResult<Option<Arc<super::super::wasm::WasmBackend>>> {
        let (_, module) = manifest.verified_entry(directory)?;
        match manifest.runtime {
            RuntimeKind::Wasm => Ok(Some(Arc::new(super::super::wasm::WasmBackend::new(
                &module,
                manifest.limits.clone(),
            )?))),
            RuntimeKind::Process => Ok(None),
        }
    })();
    let backend = match result {
        Ok(b) => b,
        Err(e) => return report(CheckState::Incompatible, e.to_string()),
    };
    checks.push(
        if backend.is_some() {
            "WASM compiled and WIT imports/exports checked"
        } else {
            "Native executable hash checked; diagnostic does not execute native code"
        }
        .into(),
    );
    if let Some(backend) = backend {
        let directory = directory.to_owned();
        match tokio::task::spawn_blocking(move || backend.check_offline_test(&directory)).await {
            Ok(Ok(true)) => {
                checks.push("Offline parser self-test: passed (no network requests)".into())
            }
            Ok(Ok(false)) => {
                checks.push("No offline parsing fixture supplied by this publisher".into())
            }
            Ok(Err(e)) => return report(CheckState::Failed, e.to_string()),
            Err(_) => return report(CheckState::Failed, "Parser self-test worker failed"),
        }
    }
    let settings = match PluginSettings::from_directory(directory, &manifest) {
        Ok(s) => s,
        Err(e) => return report(CheckState::Failed, e.to_string()),
    };
    if let Err(e) = settings.effective(values) {
        checks.push(e.to_string());
        return CheckReport {
            state: CheckState::MissingSettings,
            checks,
        };
    }
    checks.push("Required settings: valid".into());
    if check_account {
        if let Ok(Some(account)) =
            super::super::accounts::AccountProvider::from_directory(directory)
        {
            if !matches!(
                SystemSecretStore.get(&account.session_target()),
                Ok(Some(_))
            ) {
                checks.push("Sign in using Settings → Accounts".into());
                return CheckReport {
                    state: CheckState::AuthorizationRequired,
                    checks,
                };
            }
            checks.push(
                "Stored session found; its validity is verified on the next authenticated request"
                    .into(),
            );
        }
    }
    CheckReport {
        state: CheckState::Ready,
        checks,
    }
}
