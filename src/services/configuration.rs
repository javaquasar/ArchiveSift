//! Full application document validation and schema composition for the editor.
use crate::app::AppConfig;
use crate::scraper::extensions::settings::SettingsCatalog;
use crate::AppResult;
use archive_sift_scraper_api::settings_schema::{SettingsSchema, ValidationIssue};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path};

pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub fn parse_json(text: &str) -> Result<Value, String> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err("Configuration exceeds 1 MiB".into());
    }
    serde_json::from_str::<UniqueValue>(text)
        .map(|v| v.0)
        .map_err(|e| {
            format!(
                "JSON error at line {}, column {}: {}",
                e.line(),
                e.column(),
                e
            )
        })
}
struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| UniqueValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<UniqueValue>()? {
                    values.push(v.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((k, v)) = map.next_entry::<String, UniqueValue>()? {
                    if values.insert(k, v.0).is_some() {
                        return Err(de::Error::custom("duplicate property"));
                    }
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        d.deserialize_any(JsonVisitor)
    }
}

pub fn schema(catalog: &SettingsCatalog) -> Result<SettingsSchema, String> {
    let mut root = infer(
        &serde_json::to_value(AppConfig::default()).map_err(|_| "Cannot describe configuration")?,
    );
    let properties = root["properties"].as_object_mut().unwrap();
    properties.get_mut("peerlume").unwrap()["description"] = json!("PeerLume local control integration; tokens are stored separately in the OS credential store");
    properties.get_mut("peerlume").unwrap()["properties"]["download_mode"]["properties"]["mode"] = json!({"type":"string","enum":["normal","no_progress","upload_ratio_1_5","hide_downloaded","freeze_after"],"default":"normal","description":"Default PeerLume mode, overridable during review. Custom modes require reporting permission."});
    properties.insert("extension_accounts".into(),json!({"type":"object","additionalProperties":{"type":"object","properties":{"username":{"type":"string","maxLength":512,"default":""},"remember_password":{"type":"boolean","default":false}},"required":["username"],"additionalProperties":false},"properties":{}}));
    properties.insert("extension_settings".into(),json!({"type":"object","properties":{},"additionalProperties":{"type":"object","additionalProperties":true},"required":[]}));
    for (id, plugin) in &catalog.plugins {
        let mut node = plugin.schema.value().clone();
        node.as_object_mut()
            .unwrap()
            .remove("x-archivesift-delivery");
        node.as_object_mut().unwrap().remove("$schema");
        let defaults = plugin.schema.defaults();
        if plugin.schema.validate(&defaults).is_empty() {
            node["default"] = defaults;
        }
        properties["extension_settings"]["properties"][id] = node;
        if plugin.enabled {
            properties.get_mut("extension_settings").unwrap()["required"]
                .as_array_mut()
                .unwrap()
                .push(id.clone().into());
        }
        if plugin.directory.join("account.json").is_file() {
            let account = properties["extension_accounts"]["additionalProperties"].clone();
            properties.get_mut("extension_accounts").unwrap()["properties"][id] = account;
        }
    }
    properties.insert("config_version".into(),json!({"type":"integer","enum":[2],"default":2,"description":"Configuration format version"}));
    properties.insert("input_mode".into(),json!({"type":"string","enum":["ClipboardMonitor","ManualUrl"],"default":"ClipboardMonitor"}));
    properties.insert(
        "preview_open_mode".into(),
        json!({"type":"string","enum":["single_click","double_click"],"default":"double_click"}),
    );
    for (key, min, max) in [
        ("max_concurrent_downloads", 1, 32),
        ("max_parallel_downloads", 1, 16),
        ("search_columns", 1, 10),
    ] {
        properties.get_mut(key).unwrap()["minimum"] = min.into();
        properties.get_mut(key).unwrap()["maximum"] = max.into();
    }
    properties.get_mut("database_path").unwrap()["minLength"] = 1.into();
    properties.insert("thumbnail_folder".into(),json!({"type":["string","null"],"minLength":1,"description":"Null uses the thumbnail folder next to the database"}));
    root["required"] = json!([
        "source_url",
        "download_folder",
        "download_folder_history",
        "clipboard_listener_enabled",
        "database_path",
        "add_plus",
        "max_concurrent_downloads"
    ]);
    SettingsSchema::compile(root)
}

fn infer(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            json!({"type":"object","additionalProperties":false,"properties":map.iter().map(|(k,v)|(k.clone(),infer(v))).collect::<serde_json::Map<_,_>>()})
        }
        Value::Array(_) => {
            json!({"type":"array","items":{"type":"string"},"default":value,"maxItems":1000})
        }
        Value::String(_) => json!({"type":"string","default":value}),
        Value::Bool(_) => json!({"type":"boolean","default":value}),
        Value::Number(n) => {
            if n.is_f64() {
                json!({"type":"number","default":value})
            } else {
                json!({"type":"integer","minimum":0,"default":value})
            }
        }
        Value::Null => json!({"type":"null","default":null}),
    }
}

pub fn document(config: &AppConfig, catalog: &SettingsCatalog) -> Result<Value, String> {
    let mut value = serde_json::to_value(config).map_err(|_| "Cannot serialize configuration")?;
    if value.get("extension_settings").is_none() {
        value["extension_settings"] = json!({});
    }
    if value.get("extension_accounts").is_none() {
        value["extension_accounts"] = json!({});
    }
    Ok(schema(catalog)?.with_defaults(&value))
}

pub fn validate(
    value: &Value,
    catalog: &SettingsCatalog,
) -> Result<AppConfig, Vec<ValidationIssue>> {
    let mut issues = match schema(catalog) {
        Ok(schema) => schema.validate(value),
        Err(message) => {
            return Err(vec![ValidationIssue {
                path: "/".into(),
                message,
            }])
        }
    };
    for message in &catalog.errors {
        issues.push(ValidationIssue {
            path: "/extension_settings".into(),
            message: message.clone(),
        });
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    let config = serde_json::from_value::<AppConfig>(value.clone()).map_err(|_| {
        vec![ValidationIssue {
            path: "/".into(),
            message: "A setting cannot be represented by the application; check numeric ranges"
                .into(),
        }]
    })?;
    let mut add = |path: &str, message: &str| {
        issues.push(ValidationIssue {
            path: path.into(),
            message: message.into(),
        })
    };
    if let Err(error) = config.peerlume.validate() {
        add("/peerlume", &error.to_string());
    }
    for (id, plugin) in &catalog.plugins {
        if plugin.enabled || config.extension_settings.contains_key(id) {
            if let Err(error) = plugin.effective(config.extension_settings.get(id)) {
                add(&format!("/extension_settings/{id}"), &error.to_string());
            }
        }
    }
    if config.request_delay_max_ms < config.request_delay_min_ms {
        add(
            "/request_delay_max_ms",
            "must be at least request_delay_min_ms",
        );
    }
    if config.effective_concurrent_image_downloads() != config.max_concurrent_downloads {
        add(
            "/max_concurrent_downloads",
            "page and image concurrency together must not exceed 32",
        );
    }
    if config.database_path.trim().is_empty() {
        add("/database_path", "choose a database file");
    }
    if config
        .thumbnail_folder
        .as_ref()
        .is_some_and(|s| s.trim().is_empty())
    {
        add("/thumbnail_folder", "choose a folder or null");
    }
    for (name, value) in [
        ("download_folder", Some(config.download_folder.as_str())),
        ("database_path", Some(config.database_path.as_str())),
        ("thumbnail_folder", config.thumbnail_folder.as_deref()),
        ("subfolder", Some(config.subfolder.as_str())),
    ] {
        if value.is_some_and(|v| v.chars().any(char::is_control)) {
            add(
                &format!("/{name}"),
                "path contains control characters; escape Windows backslashes in JSON",
            );
        }
    }
    let subfolder = config.subfolder.replace('\\', "/");
    if subfolder.contains(':')
        || Path::new(&subfolder)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        add(
            "/subfolder",
            "must be a relative folder without parent traversal",
        );
    }
    if config.scrape_layout != config.scrape_layout.normalized() {
        add(
            "/scrape_layout",
            "layout values exceed their ranges or breakpoint order",
        );
    }
    if issues.is_empty() {
        Ok(config)
    } else {
        Err(issues)
    }
}

impl crate::services::SettingsService {
    pub fn apply_configuration(
        &self,
        current: &AppConfig,
        value: &Value,
        catalog: &SettingsCatalog,
        config_path: &Path,
    ) -> AppResult<AppConfig> {
        let candidate = validate(value, catalog).map_err(|issues| {
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        })?;
        if config_path.exists() {
            let disk = AppConfig::load_from(config_path)?;
            if serde_json::to_value(disk)? != serde_json::to_value(current)? {
                return Err("Settings changed on disk; reload the document before saving".into());
            }
        }
        self.apply_storage_settings(
            &candidate,
            &crate::services::StorageSettings::from(&candidate),
            config_path,
        )
    }
}
