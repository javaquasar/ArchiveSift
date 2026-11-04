//! Declarative settings discovery. Reading a schema never executes guest code.
use super::manifest::PluginManifest;
use crate::AppResult;
use archive_sift_scraper_api::settings_schema::{
    SettingsSchema, ValidationIssue, MAX_SCHEMA_BYTES,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct PluginSettings {
    pub id: String,
    pub version: String,
    pub enabled: bool,
    pub schema: SettingsSchema,
    pub directory: PathBuf,
    pub declared: bool,
}
impl PluginSettings {
    pub fn from_directory(directory: &Path, manifest: &PluginManifest) -> AppResult<Self> {
        let path = directory.join("settings.schema.json");
        let file = match std::fs::File::open(&path) {
            Ok(file) => Some(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let declared = file.is_some();
        let schema = if let Some(file) = file {
            let mut bytes = Vec::new();
            file.take(MAX_SCHEMA_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_SCHEMA_BYTES {
                return Err("plugin settings schema exceeds 64 KiB".into());
            }
            SettingsSchema::compile(serde_json::from_slice(&bytes)?)?
        } else {
            SettingsSchema::compile(
                json!({"type":"object","additionalProperties":false,"properties":{}}),
            )?
        };
        if schema.value()["type"] != "object" {
            return Err("plugin settings schema must describe an object".into());
        }
        schema.validate_plugin_identity(&manifest.id)?;
        Ok(Self {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            enabled: manifest.enabled,
            schema,
            directory: directory.to_owned(),
            declared,
        })
    }
    pub fn effective(&self, values: Option<&Value>) -> AppResult<Value> {
        let value = self.schema.with_defaults(values.unwrap_or(&json!({})));
        if serde_json::to_vec(&value)?.len() > 16 * 1024 {
            return Err("plugin settings exceed 16 KiB".into());
        }
        let issues = self.schema.validate(&value);
        if !issues.is_empty() {
            return Err(format!(
                "invalid settings for {}: {}",
                self.id,
                issues
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            )
            .into());
        }
        Ok(value)
    }
    pub fn guest_settings(&self, values: &Value) -> Value {
        strip_secrets(self.schema.value(), values)
    }
}

fn strip_secrets(schema: &Value, values: &Value) -> Value {
    if schema["format"] == "secret-reference" {
        return Value::Null;
    }
    if let Some(object) = values.as_object() {
        return Value::Object(
            object
                .iter()
                .filter_map(|(k, v)| {
                    let child = schema["properties"]
                        .get(k)
                        .unwrap_or(&schema["additionalProperties"]);
                    (child["format"] != "secret-reference")
                        .then(|| (k.clone(), strip_secrets(child, v)))
                })
                .collect(),
        );
    }
    if let Some(items) = values.as_array() {
        return Value::Array(
            items
                .iter()
                .map(|v| strip_secrets(&schema["items"], v))
                .collect(),
        );
    }
    values.clone()
}

#[derive(Clone, Default)]
pub struct SettingsCatalog {
    pub plugins: BTreeMap<String, PluginSettings>,
    pub errors: Vec<String>,
}
impl SettingsCatalog {
    pub fn secret_targets(&self) -> Vec<String> {
        fn collect(schema: &Value, targets: &mut Vec<String>) {
            if schema["format"] == "secret-reference" {
                if let Some(value) = schema["const"].as_str() {
                    if !targets.iter().any(|t| t == value) {
                        targets.push(value.into());
                    }
                }
            }
            if let Some(properties) = schema["properties"].as_object() {
                for node in properties.values() {
                    collect(node, targets);
                }
            }
            for key in ["items", "additionalProperties"] {
                if schema[key].is_object() {
                    collect(&schema[key], targets);
                }
            }
        }
        let mut targets = vec![];
        for plugin in self.plugins.values() {
            collect(plugin.schema.value(), &mut targets);
        }
        targets
    }

    pub fn store_secret(
        &self,
        target: &str,
        value: &crate::secure_store::Secret,
        store: &dyn crate::secure_store::SecretStore,
    ) -> AppResult<()> {
        if !self.secret_targets().iter().any(|t| t == target) {
            return Err("credential target is not declared by an installed plugin".into());
        }
        if value.expose_secret().is_empty() || value.expose_secret().len() > 8192 {
            return Err("credential must contain 1–8192 bytes".into());
        }
        store.set(target, value)?;
        Ok(())
    }
    pub fn installed() -> Self {
        Self::discover(&super::default_plugin_directory())
    }
    pub fn discover(directory: &Path) -> Self {
        let mut catalog = Self::default();
        let entries = match std::fs::read_dir(directory) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return catalog,
            Err(_) => {
                catalog
                    .errors
                    .push("Cannot read the plugins directory".into());
                return catalog;
            }
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.join("plugin.json").is_file())
            .collect::<Vec<_>>();
        paths.sort();
        for directory in paths {
            let result = (|| -> AppResult<PluginSettings> {
                let manifest = super::read_manifest(&directory)?;
                PluginSettings::from_directory(&directory, &manifest)
            })();
            match result {
                Ok(plugin) if !catalog.plugins.contains_key(&plugin.id) => {
                    catalog.plugins.insert(plugin.id.clone(), plugin);
                }
                Ok(_) => catalog
                    .errors
                    .push("Duplicate plugin identity in settings catalog".into()),
                Err(_) => catalog.errors.push(format!(
                    "Invalid settings declaration in {}",
                    directory.display()
                )),
            }
        }
        catalog
    }
    pub fn validate_values(&self, values: &BTreeMap<String, Value>) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        for message in &self.errors {
            issues.push(ValidationIssue {
                path: "/extension_settings".into(),
                message: message.clone(),
            });
        }
        for (id, plugin) in &self.plugins {
            if plugin.enabled || values.contains_key(id) {
                let settings = plugin
                    .schema
                    .with_defaults(values.get(id).unwrap_or(&json!({})));
                for mut issue in plugin.schema.validate(&settings) {
                    issue.path = format!("/extension_settings/{}{}", id, issue.path);
                    issues.push(issue);
                }
            }
        }
        issues
    }
}
