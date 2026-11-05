//! Versioned packages and local grants. No publisher file is rewritten on activation.
mod diagnostics;
#[cfg(test)]
mod tests;
pub mod updates;
use super::{
    manifest::{PluginManifest, RuntimeKind},
    settings::PluginSettings,
};
use crate::{
    scraper::{RegistryProvider, ScraperRegistry},
    AppResult,
};
use archive_sift_extension_package::{digest, Package};
pub use diagnostics::{CheckReport, CheckState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

const STATE: &str = "extensions-state.json";
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalState {
    pub schema: u32,
    pub generation: u64,
    pub plugins: BTreeMap<String, Installed>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Installed {
    pub active: String,
    pub previous: Option<String>,
    pub enabled: bool,
    pub trusted_native: bool,
    pub versions: BTreeMap<String, InstalledVersion>,
    pub source: Option<updates::UpdateSource>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstalledVersion {
    pub version: String,
    pub digest: String,
    pub publisher: Option<String>,
    pub origins: Vec<String>,
    pub settings: Value,
    pub native_approved: bool,
}

pub struct PreparedPackage {
    pub manifest: PluginManifest,
    pub settings: PluginSettings,
    pub package: Package,
    directory: PathBuf,
    archive: Vec<u8>,
}
impl Drop for PreparedPackage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[derive(Clone)]
pub struct ExtensionManager {
    root: PathBuf,
    provider: RegistryProvider,
    mutation: Arc<Mutex<()>>,
}
pub fn global_manager() -> &'static ExtensionManager {
    static MANAGER: OnceLock<ExtensionManager> = OnceLock::new();
    MANAGER.get_or_init(|| {
        ExtensionManager::new(
            super::default_plugin_directory(),
            crate::scraper::global_registry().clone(),
        )
    })
}

pub(super) fn bounded_read(path: &Path, limit: usize) -> AppResult<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err("extension file exceeds size limit".into());
    }
    Ok(bytes)
}
fn read_state(root: &Path) -> AppResult<LocalState> {
    if !root.join(STATE).exists() {
        return Ok(LocalState {
            schema: 1,
            ..Default::default()
        });
    }
    let state: LocalState = serde_json::from_slice(&bounded_read(&root.join(STATE), 1024 * 1024)?)?;
    validate_state(&state)?;
    Ok(state)
}
fn validate_state(state: &LocalState) -> AppResult<()> {
    if state.schema != 1 || state.plugins.len() > 256 {
        return Err("unsupported extension state".into());
    }
    for (id, plugin) in &state.plugins {
        if !safe_component(id)
            || plugin.versions.len() > 128
            || !plugin.versions.contains_key(&plugin.active)
            || plugin
                .previous
                .as_ref()
                .is_some_and(|p| !plugin.versions.contains_key(p))
        {
            return Err("invalid extension state identity".into());
        }
        for (key, version) in &plugin.versions {
            if !safe_component(key)
                || version.digest.len() != 64
                || !version.digest.bytes().all(|c| c.is_ascii_hexdigit())
                || semver::Version::parse(&version.version).is_err()
                || version.origins.len() > 64
                || serde_json::to_vec(&version.settings)?.len() > 16 * 1024
            {
                return Err("invalid extension version state".into());
            }
        }
    }
    if serde_json::to_vec(state)?.len() > 1024 * 1024 {
        return Err("extension state exceeds 1 MiB".into());
    }
    Ok(())
}
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 192
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}
fn version_dir(root: &Path, id: &str, key: &str) -> PathBuf {
    root.join("packages").join(id).join(key)
}
fn check_managed_path(root: &Path, path: &Path) -> AppResult<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "package path is outside the plugins directory")?;
    let mut current = root.to_owned();
    for part in relative.components() {
        current.push(part);
        if let Ok(metadata) = std::fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                return Err("managed package paths cannot contain symbolic links".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("managed package paths cannot contain reparse points".into());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn package_entries(root: &Path) -> AppResult<Vec<(PathBuf, AppResult<PluginManifest>)>> {
    entries_for_state(root, &read_state(root)?)
}
fn entries_for_state(
    root: &Path,
    state: &LocalState,
) -> AppResult<Vec<(PathBuf, AppResult<PluginManifest>)>> {
    let mut entries = vec![];
    if root.exists() {
        let mut paths = std::fs::read_dir(root)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
                    && p.is_dir()
                    && p.join("plugin.json").is_file()
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let manifest = super::read_manifest(&path);
            if manifest
                .as_ref()
                .is_ok_and(|m| state.plugins.contains_key(&m.id))
            {
                continue;
            }
            entries.push((path, manifest));
        }
    }
    for (id, plugin) in &state.plugins {
        let path = version_dir(root, id, &plugin.active);
        let result = (|| -> AppResult<PluginManifest> {
            check_managed_path(root, &path.join("package.asift-plugin"))?;
            let version = plugin
                .versions
                .get(&plugin.active)
                .ok_or("active version is missing")?;
            let package = Package::read(&path.join("package.asift-plugin"))?;
            if package.artifact_digest != version.digest || package.signer != version.publisher {
                return Err("installed package identity changed".into());
            }
            for (name, bytes) in &package.files {
                let target = path.join(name);
                if target.symlink_metadata()?.file_type().is_symlink()
                    || digest(&bounded_read(&target, bytes.len())?) != digest(bytes)
                {
                    return Err("installed package payload changed".into());
                }
            }
            let mut manifest = super::read_manifest(&path)?;
            if manifest.id != *id
                || manifest.version != version.version
                || manifest.network_origins != version.origins
            {
                return Err("installed manifest or permissions changed".into());
            }
            manifest.enabled = plugin.enabled;
            manifest.trusted = plugin.trusted_native;
            Ok(manifest)
        })();
        entries.push((path, result));
    }
    Ok(entries)
}

impl ExtensionManager {
    fn lock_mutation(&self) -> AppResult<MutationGuard<'_>> {
        let thread = self
            .mutation
            .lock()
            .map_err(|_| "extension manager lock failed")?;
        std::fs::create_dir_all(&self.root)?;
        let file = lock_file(&self.root)?;
        file.try_lock().map_err(|_| {
            "another ArchiveSift instance is changing extensions; retry when it finishes"
        })?;
        Ok(MutationGuard {
            _file: file,
            _thread: thread,
        })
    }
    pub fn new(root: PathBuf, provider: RegistryProvider) -> Self {
        Self {
            root,
            provider,
            mutation: Arc::new(Mutex::new(())),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn state(&self) -> AppResult<LocalState> {
        read_state(&self.root)
    }
    pub fn snapshot(&self) -> ScraperRegistry {
        self.provider.snapshot()
    }
    pub fn prepare(&self, path: &Path) -> AppResult<PreparedPackage> {
        self.prepare_bytes(bounded_read(
            path,
            archive_sift_extension_package::MAX_PACKAGE,
        )?)
    }
    pub fn prepare_bytes(&self, archive: Vec<u8>) -> AppResult<PreparedPackage> {
        let package = Package::decode(&archive)?;
        let manifest: PluginManifest = serde_json::from_slice(&package.files["plugin.json"])?;
        manifest.validate()?;
        std::fs::create_dir_all(&self.root)?;
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| "cannot create staging directory")?;
        let directory = self.root.join(format!(
            ".staging-{}",
            archive_sift_extension_package::hex(&random)
        ));
        std::fs::create_dir(&directory)?;
        // Own the directory before writing so every failure cleans up only this staging tree.
        let mut prepared = PreparedPackage {
            manifest: manifest.clone(),
            settings: PluginSettings::from_directory(&directory, &manifest)?,
            package,
            directory,
            archive,
        };
        for (name, bytes) in &prepared.package.files {
            let path = prepared.directory.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        #[cfg(unix)]
        if manifest.runtime == RuntimeKind::Process {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                prepared.directory.join(&manifest.entry),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        prepared.settings = PluginSettings::from_directory(&prepared.directory, &manifest)?;
        // Reference declarations are parsed here; values stay in the OS store.
        if let Some(bytes) = prepared.package.files.get("credential-bindings.json") {
            super::credentials::Credentials::parse(&manifest, bytes)?;
        }
        super::accounts::AccountProvider::for_manifest(&prepared.directory, &manifest)?;
        Ok(prepared)
    }

    /// Explicit approval is tied to the exact reviewed artifact and publisher key.
    pub fn install(
        &self,
        prepared: &PreparedPackage,
        approve_publisher: Option<&str>,
        allow_unsigned: bool,
        trust_native: bool,
        settings: Option<&Value>,
    ) -> AppResult<()> {
        match prepared.package.signer.as_deref() {
            Some(key) if approve_publisher == Some(key) => {}
            None if allow_unsigned => {}
            _ => return Err("publisher approval is required".into()),
        }
        if prepared.manifest.runtime == RuntimeKind::Process && !trust_native {
            return Err("native process approval is required".into());
        }
        // Missing settings are reported by diagnostics, never prevent installing disabled packages.
        let _guard = self.lock_mutation()?;
        let mut state = self.state()?;
        let id = &prepared.manifest.id;
        let key = format!(
            "{}--{}",
            prepared.manifest.version,
            &prepared.package.artifact_digest[..16]
        );
        let existing = state.plugins.get(id);
        if let Some(existing) = existing {
            if existing.versions.values().any(|v| {
                v.version == prepared.manifest.version
                    && v.digest != prepared.package.artifact_digest
            }) {
                return Err(
                    "a different artifact already uses this version; publish a new version".into(),
                );
            }
        }
        let target = version_dir(&self.root, id, &key);
        check_managed_path(&self.root, &target)?;
        std::fs::create_dir_all(target.parent().ok_or("missing package directory")?)?;
        if !target.exists() {
            // Copy from immutable in-memory bytes, never re-read the selected path.
            let mut random = [0; 8];
            getrandom::fill(&mut random).map_err(|_| "staging failed")?;
            let stage = target.with_file_name(format!(
                ".install-{}",
                archive_sift_extension_package::hex(&random)
            ));
            std::fs::create_dir(&stage)?;
            let result = (|| -> AppResult<()> {
                for (name, bytes) in &prepared.package.files {
                    let path = stage.join(name);
                    if let Some(p) = path.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    std::fs::write(path, bytes)?;
                }
                std::fs::write(stage.join("package.asift-plugin"), &prepared.archive)?;
                #[cfg(unix)]
                if prepared.manifest.runtime == RuntimeKind::Process {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        stage.join(&prepared.manifest.entry),
                        std::fs::Permissions::from_mode(0o700),
                    )?;
                }
                std::fs::rename(&stage, &target)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_dir_all(&stage);
            }
            result?;
        }
        let plugin = state.plugins.entry(id.clone()).or_default();
        if plugin.active != key && !plugin.active.is_empty() {
            plugin.previous = Some(plugin.active.clone());
        }
        if let Some(active) = plugin.versions.get_mut(&plugin.active) {
            active.settings = settings.cloned().unwrap_or_else(|| serde_json::json!({}));
        }
        plugin.versions.insert(
            key.clone(),
            InstalledVersion {
                version: prepared.manifest.version.clone(),
                digest: prepared.package.artifact_digest.clone(),
                publisher: prepared.package.signer.clone(),
                origins: prepared.manifest.network_origins.clone(),
                settings: settings.cloned().unwrap_or_else(|| serde_json::json!({})),
                native_approved: trust_native,
            },
        );
        plugin.active = key;
        plugin.enabled = true;
        plugin.trusted_native = trust_native;
        self.commit(state, Some(id))
    }
    fn registry_for(&self, state: &LocalState) -> AppResult<ScraperRegistry> {
        let mut registry = ScraperRegistry::with_public_scrapers();
        let entries = entries_for_state(&self.root, state)?;
        registry.extension_statuses = Arc::new(super::discover_entries(&mut registry, entries));
        Ok(registry)
    }
    fn commit(&self, mut state: LocalState, required: Option<&str>) -> AppResult<()> {
        validate_state(&state)?;
        let registry = self.registry_for(&state)?;
        if let Some(id) = required {
            if !registry
                .extension_statuses()
                .iter()
                .any(|s| s.id == id && s.loaded)
            {
                return Err(registry
                    .extension_statuses()
                    .iter()
                    .find(|s| s.id == id)
                    .map_or("extension validation failed".to_owned(), |s| {
                        s.message.clone()
                    })
                    .into());
            }
        }
        // Do not activate a candidate that disables another previously loaded route.
        for status in self
            .snapshot()
            .extension_statuses()
            .iter()
            .filter(|s| s.loaded)
        {
            if state.plugins.get(&status.id).is_some_and(|p| !p.enabled) {
                continue;
            }
            if !registry
                .extension_statuses()
                .iter()
                .any(|s| s.id == status.id && s.loaded)
            {
                return Err("candidate conflicts with an active extension".into());
            }
        }
        state.generation += 1;
        crate::persistence::atomic_write(
            &self.root.join(STATE),
            &serde_json::to_vec_pretty(&state)?,
        )?;
        self.provider.publish(registry);
        Ok(())
    }
    pub fn set_enabled(&self, id: &str, enabled: bool) -> AppResult<()> {
        let _guard = self.lock_mutation()?;
        let mut state = self.state()?;
        state
            .plugins
            .get_mut(id)
            .ok_or("import the legacy package before changing local state")?
            .enabled = enabled;
        self.commit(state, enabled.then_some(id))
    }
    pub fn rollback_settings(&self, id: &str) -> AppResult<Value> {
        let state = self.state()?;
        let plugin = state.plugins.get(id).ok_or("unknown extension")?;
        Ok(plugin
            .versions
            .get(plugin.previous.as_ref().ok_or("no previous version")?)
            .ok_or("previous version is missing")?
            .settings
            .clone())
    }
    pub fn rollback(&self, id: &str, settings: Option<&Value>) -> AppResult<()> {
        let _guard = self.lock_mutation()?;
        let mut state = self.state()?;
        let plugin = state.plugins.get_mut(id).ok_or("unknown extension")?;
        let previous = plugin.previous.clone().ok_or("no previous version")?;
        plugin.trusted_native = plugin
            .versions
            .get(&previous)
            .ok_or("previous version is missing")?
            .native_approved;
        let dir = version_dir(&self.root, id, &previous);
        let manifest = super::read_manifest(&dir)?;
        PluginSettings::from_directory(&dir, &manifest)?.effective(settings)?;
        plugin.previous = Some(std::mem::replace(&mut plugin.active, previous));
        self.commit(state, Some(id))
    }
    pub fn version_settings(&self, id: &str, version: &str) -> AppResult<Value> {
        Ok(self
            .state()?
            .plugins
            .get(id)
            .and_then(|p| p.versions.get(version))
            .ok_or("unknown installed version")?
            .settings
            .clone())
    }
    /// A journal closes the crash window between the local pointer and the
    /// profile document. Only this extension's settings are changed.
    pub fn switch_and_save(
        &self,
        id: &str,
        version: &str,
        current: &crate::app::AppConfig,
        config_path: &Path,
    ) -> AppResult<crate::app::AppConfig> {
        let _guard = self.lock_mutation()?;
        let _switch = SWITCH_LOCK
            .lock()
            .map_err(|_| "extension switch lock failed")?;
        recover_switch_locked(&self.root)?;
        let old_config = bounded_read(config_path, 1024 * 1024)?;
        // Use the same migrations/normalization as the open desktop profile.
        // Recovery above has completed before a new journal can be created.
        let saved = crate::app::AppConfig::load_from(config_path)?;
        if serde_json::to_value(&saved)? != serde_json::to_value(current)? {
            return Err("configuration changed; reload it before switching versions".into());
        }
        let mut state = self.state()?;
        let plugin = state.plugins.get_mut(id).ok_or("unknown extension")?;
        if plugin.active == version {
            return Err("this version is already active".into());
        }
        let stored = plugin
            .versions
            .get(version)
            .ok_or("unknown installed version")?
            .settings
            .clone();
        plugin.trusted_native = plugin
            .versions
            .get(version)
            .ok_or("unknown installed version")?
            .native_approved;
        plugin.enabled = true;
        if let Some(active) = plugin.versions.get_mut(&plugin.active) {
            active.settings = current
                .extension_settings
                .get(id)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
        }
        plugin.previous = Some(std::mem::replace(&mut plugin.active, version.to_owned()));
        let directory = version_dir(&self.root, id, version);
        let manifest = super::read_manifest(&directory)?;
        let target_settings = PluginSettings::from_directory(&directory, &manifest)?;
        let settings = target_settings.effective(Some(&stored))?;
        let mut next = current.clone();
        next.extension_settings.insert(id.to_owned(), settings);
        let mut catalog = super::settings::SettingsCatalog::discover(&self.root);
        catalog.plugins.insert(id.to_owned(), target_settings);
        let next =
            crate::services::configuration::validate(&serde_json::to_value(&next)?, &catalog)
                .map_err(|_| {
                    "restored configuration does not match the selected extension schema"
                })?;
        // Compile and check conflicts before writing either file.
        let registry = self.registry_for(&state)?;
        if !registry
            .extension_statuses()
            .iter()
            .any(|s| s.id == id && s.loaded)
        {
            return Err("selected version failed validation".into());
        }
        state.generation += 1;
        validate_state(&state)?;
        let config_path = std::fs::canonicalize(config_path)?;
        let journal = SwitchJournal {
            next_state: state.clone(),
            config_path,
            old_config,
            new_config: serde_json::to_vec_pretty(&next)?,
        };
        crate::persistence::atomic_write(
            &self.root.join("switch-journal.json"),
            &serde_json::to_vec(&journal)?,
        )?;
        crate::persistence::atomic_write(&journal.config_path, &journal.new_config)?;
        if let Err(e) = crate::persistence::atomic_write(
            &self.root.join(STATE),
            &serde_json::to_vec_pretty(&state)?,
        ) {
            let _ = recover_switch_locked(&self.root);
            return Err(e.into());
        }
        self.provider
            .publish_with_settings(registry, next.extension_settings.clone());
        // If cleanup fails startup recovery is idempotent and finishes the same switch.
        let _ = std::fs::remove_file(self.root.join("switch-journal.json"));
        Ok(next)
    }
    pub fn reload(&self) -> AppResult<()> {
        let _guard = self.lock_mutation()?;
        let registry = self.registry_for(&self.state()?)?;
        self.provider.publish(registry);
        Ok(())
    }
    pub fn set_source(&self, id: &str, source: Option<updates::UpdateSource>) -> AppResult<()> {
        if let Some(s) = &source {
            s.validate()?;
        }
        let _guard = self.lock_mutation()?;
        let mut state = self.state()?;
        state.plugins.get_mut(id).ok_or("unknown extension")?.source = source;
        self.commit(state, None)
    }
    pub fn import_legacy(&self, directory: &Path) -> AppResult<PreparedPackage> {
        let manifest = super::read_manifest(directory)?;
        let mut files = BTreeMap::new();
        for name in [
            "plugin.json",
            manifest.entry.as_str(),
            "settings.schema.json",
            "account.json",
            "credential-bindings.json",
            "release.json",
            "self-test.json",
        ] {
            let path = directory.join(name);
            if path.is_file() {
                files.insert(
                    name.to_owned(),
                    bounded_read(&path, super::manifest::MAX_MODULE_BYTES as usize)?,
                );
            }
        }
        self.prepare_bytes(archive_sift_extension_package::encode(&files, None)?)
    }
}

struct MutationGuard<'a> {
    _file: std::fs::File,
    _thread: std::sync::MutexGuard<'a, ()>,
}
fn lock_file(root: &Path) -> AppResult<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".extension-manager.lock"))?)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchJournal {
    next_state: LocalState,
    config_path: PathBuf,
    old_config: Vec<u8>,
    new_config: Vec<u8>,
}
static SWITCH_LOCK: Mutex<()> = Mutex::new(());
pub(crate) fn recover_for_config(root: &Path, config: &Path) -> AppResult<()> {
    let path = root.join("switch-journal.json");
    if !path.exists() {
        return Ok(());
    }
    let journal: SwitchJournal = serde_json::from_slice(&bounded_read(&path, 12 * 1024 * 1024)?)?;
    if std::fs::canonicalize(config).ok().as_ref() != Some(&journal.config_path) {
        return Ok(());
    }
    let _file = lock_file(root)?;
    _file.lock()?;
    let _guard = SWITCH_LOCK
        .lock()
        .map_err(|_| "extension switch lock failed")?;
    recover_switch_locked(root)
}
fn recover_switch_locked(root: &Path) -> AppResult<()> {
    let path = root.join("switch-journal.json");
    if !path.exists() {
        return Ok(());
    }
    let journal: SwitchJournal = serde_json::from_slice(&bounded_read(&path, 12 * 1024 * 1024)?)?;
    if !journal.config_path.is_absolute()
        || journal.old_config.len() > 1024 * 1024
        || journal.new_config.len() > 1024 * 1024
    {
        return Err("invalid pending extension switch".into());
    }
    let current = bounded_read(&journal.config_path, 1024 * 1024)?;
    if current != journal.old_config && current != journal.new_config {
        return Err("configuration was modified during an interrupted version switch; review switch-journal.json".into());
    }
    let state = read_state(root)?;
    let committed = state.generation == journal.next_state.generation
        && serde_json::to_value(&state)? == serde_json::to_value(&journal.next_state)?;
    crate::persistence::atomic_write(
        &journal.config_path,
        if committed {
            &journal.new_config
        } else {
            &journal.old_config
        },
    )?;
    std::fs::remove_file(path)?;
    Ok(())
}
