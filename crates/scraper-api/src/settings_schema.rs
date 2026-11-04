//! Bounded, offline JSON Schema subset shared by the host and package tools.
//! References and executable/remote extensions are deliberately unsupported.
use serde_json::{Map, Value};

pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}
impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

#[derive(Clone, Debug)]
pub struct SettingsSchema(Value);
impl SettingsSchema {
    pub fn compile(value: Value) -> Result<Self, String> {
        if serde_json::to_vec(&value)
            .map_err(|_| "invalid schema")?
            .len()
            > MAX_SCHEMA_BYTES
        {
            return Err("schema exceeds 64 KiB".into());
        }
        check_schema(&value, 0)?;
        let schema = Self(value);
        check_defaults(&schema.0)?;
        Ok(schema)
    }
    pub fn value(&self) -> &Value {
        &self.0
    }
    pub fn validate_plugin_identity(&self, id: &str) -> Result<(), String> {
        if self.0["type"] != "object" {
            return Err("plugin settings must describe an object".into());
        }
        fn walk(node: &Value, id: &str) -> Result<(), String> {
            if node["format"] == "secret-reference" {
                let target = node["const"]
                    .as_str()
                    .ok_or("secret references must declare a fixed system-store target")?;
                if !target.starts_with(&format!("ArchiveSift/{id}/")) || target.contains("..") {
                    return Err("secret reference exceeds the plugin namespace".into());
                }
            }
            if let Some(properties) = node["properties"].as_object() {
                for child in properties.values() {
                    walk(child, id)?;
                }
            }
            for key in ["items", "additionalProperties"] {
                if node[key].is_object() {
                    walk(&node[key], id)?;
                }
            }
            Ok(())
        }
        walk(&self.0, id)
    }
    pub fn validate(&self, value: &Value) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        validate_node(&self.0, value, "", &mut issues, 0);
        issues
    }
    pub fn with_defaults(&self, value: &Value) -> Value {
        materialize(&self.0, value)
    }
    pub fn defaults(&self) -> Value {
        materialize(&self.0, &Value::Object(Map::new()))
    }
    pub fn at_path(&self, path: &[String]) -> Option<&Value> {
        let mut node = &self.0;
        for key in path {
            node = if node["type"] == "array" {
                node.get("items")?
            } else {
                node.get("properties")
                    .and_then(|p| p.get(key))
                    .or_else(|| node.get("additionalProperties").filter(|v| v.is_object()))?
            };
        }
        Some(node)
    }
}

fn check_schema(schema: &Value, depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("schema nesting exceeds 16 levels".into());
    }
    let object = schema.as_object().ok_or("schema nodes must be objects")?;
    const ALLOWED: &[&str] = &[
        "$schema",
        "title",
        "description",
        "default",
        "examples",
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "uniqueItems",
        "format",
        "x-archivesift-delivery",
    ];
    if object.keys().any(|k| !ALLOWED.contains(&k.as_str())) {
        return Err("unsupported schema keyword (references are not allowed)".into());
    }
    if !object.contains_key("type") {
        return Err("every schema node must declare its type".into());
    }
    let types = types(schema);
    if let Some(values) = schema["type"].as_array() {
        if values.len() != types.len()
            || values
                .iter()
                .enumerate()
                .any(|(i, v)| values[..i].contains(v))
        {
            return Err("invalid schema type list".into());
        }
    }
    if types.is_empty()
        || types.iter().any(|t| {
            !matches!(
                *t,
                "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
            )
        })
    {
        return Err("invalid schema type".into());
    }
    for key in [
        "title",
        "description",
        "$schema",
        "format",
        "x-archivesift-delivery",
    ] {
        if object.get(key).is_some_and(|v| !v.is_string()) {
            return Err("schema metadata must be strings".into());
        }
    }
    if schema
        .get("x-archivesift-delivery")
        .is_some_and(|v| depth != 0 || v != "state-v1")
    {
        return Err("unsupported settings delivery".into());
    }
    if schema
        .get("format")
        .is_some_and(|v| !matches!(v.as_str(), Some("uri" | "secret-reference")))
    {
        return Err("unsupported string format".into());
    }
    if schema.get("format").is_some() && !types.contains(&"string") {
        return Err("string formats require string type".into());
    }
    for (min, max) in [
        ("minimum", "maximum"),
        ("minLength", "maxLength"),
        ("minItems", "maxItems"),
    ] {
        for key in [min, max] {
            if let Some(v) = object.get(key) {
                if if key == "minimum" || key == "maximum" {
                    v.as_f64().is_none()
                } else {
                    v.as_u64().is_none()
                } {
                    return Err("invalid schema bound".into());
                }
            }
        }
        if let (Some(a), Some(b)) = (schema[min].as_f64(), schema[max].as_f64()) {
            if a > b {
                return Err("schema minimum exceeds maximum".into());
            }
        }
    }
    if schema.get("uniqueItems").is_some_and(|v| !v.is_boolean()) {
        return Err("uniqueItems must be boolean".into());
    }
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 256)
            .ok_or("invalid enum")?;
        if values
            .iter()
            .enumerate()
            .any(|(i, v)| values[..i].contains(v))
        {
            return Err("duplicate enum value".into());
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .filter(|p| p.len() <= 256)
            .ok_or("invalid properties")?;
        if !types.contains(&"object") {
            return Err("properties require object type".into());
        }
        for (key, child) in properties {
            if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
                return Err("invalid property name".into());
            }
            let lower = key.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "password" | "passwd" | "api_key" | "apikey" | "secret" | "token" | "cookie"
            ) && child["format"] != "secret-reference"
                && !matches!(child["type"].as_str(), Some("object" | "array"))
            {
                return Err("credential properties must use secret-reference format".into());
            }
            check_schema(child, depth + 1)?;
        }
    }
    if let Some(required) = object.get("required") {
        let required = required.as_array().ok_or("required must be an array")?;
        if required.len() > 256 {
            return Err("too many required properties".into());
        }
        for name in required {
            if name
                .as_str()
                .is_none_or(|s| schema["properties"].get(s).is_none())
            {
                return Err("required refers to an undeclared property".into());
            }
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        if !additional.is_boolean() {
            check_schema(additional, depth + 1)?;
        }
    }
    if let Some(items) = object.get("items") {
        check_schema(items, depth + 1)?;
    }
    if types.contains(&"array") && !object.contains_key("items") {
        return Err("arrays must declare items".into());
    }
    Ok(())
}

fn types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t],
        Value::Array(t) => t.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}
fn issue(issues: &mut Vec<ValidationIssue>, path: &str, message: impl Into<String>) {
    if issues.len() < 64 {
        issues.push(ValidationIssue {
            path: if path.is_empty() {
                "/".into()
            } else {
                path.into()
            },
            message: message.into(),
        });
    }
}
pub fn pointer_child(path: &str, key: &str) -> String {
    format!("{}/{}", path, key.replace('~', "~0").replace('/', "~1"))
}

fn validate_node(
    schema: &Value,
    value: &Value,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
    depth: usize,
) {
    if issues.len() >= 64 {
        return;
    }
    if depth > 32 {
        issue(issues, path, "settings nesting exceeds 32 levels");
        return;
    }
    let types = types(schema);
    let valid = types.iter().any(|t| match *t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    });
    if !valid {
        issue(issues, path, format!("expected {}", types.join(" or ")));
        return;
    }
    if schema.get("const").is_some_and(|v| v != value) {
        issue(issues, path, "value must match the declared constant");
    }
    if schema["enum"]
        .as_array()
        .is_some_and(|allowed| !allowed.contains(value))
    {
        issue(issues, path, "value is not one of the declared choices");
    }
    if let Some(number) = value.as_f64() {
        if schema["minimum"].as_f64().is_some_and(|m| number < m) {
            issue(issues, path, "number is below the minimum");
        }
        if schema["maximum"].as_f64().is_some_and(|m| number > m) {
            issue(issues, path, "number exceeds the maximum");
        }
    }
    if let Some(text) = value.as_str() {
        let len = text.chars().count() as u64;
        if schema["minLength"].as_u64().is_some_and(|m| len < m) {
            issue(issues, path, "string is too short");
        }
        if schema["maxLength"].as_u64().is_some_and(|m| len > m) {
            issue(issues, path, "string is too long");
        }
        if schema["format"] == "secret-reference"
            && (text.len() > 256
                || !text.starts_with("ArchiveSift/")
                || text.chars().any(char::is_control))
        {
            issue(
                issues,
                path,
                "expected a system password store reference, never a secret value",
            );
        }
        if schema["format"] == "uri"
            && (url::Url::parse(text).is_err() || text.chars().any(char::is_whitespace))
        {
            issue(issues, path, "expected an absolute URI");
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    issue(
                        issues,
                        &pointer_child(path, key),
                        "required property is missing",
                    );
                }
            }
        }
        for (key, value) in object {
            let next = pointer_child(path, key);
            if let Some(child) = schema["properties"].get(key) {
                validate_node(child, value, &next, issues, depth + 1);
            } else if schema["additionalProperties"] == false {
                issue(issues, &next, "unknown property");
            } else if schema["additionalProperties"].is_object() {
                validate_node(
                    &schema["additionalProperties"],
                    value,
                    &next,
                    issues,
                    depth + 1,
                );
            }
        }
    }
    if let Some(items) = value.as_array() {
        if schema["minItems"]
            .as_u64()
            .is_some_and(|m| items.len() < (m as usize))
        {
            issue(issues, path, "array has too few items");
        }
        if schema["maxItems"]
            .as_u64()
            .is_some_and(|m| items.len() > (m as usize))
        {
            issue(issues, path, "array has too many items");
        }
        for (i, item) in items.iter().enumerate() {
            if schema["uniqueItems"] == true && items[..i].contains(item) {
                issue(
                    issues,
                    &pointer_child(path, &i.to_string()),
                    "duplicate array item",
                );
            }
            validate_node(
                &schema["items"],
                item,
                &pointer_child(path, &i.to_string()),
                issues,
                depth + 1,
            );
        }
    }
}

fn materialize(schema: &Value, value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        if let Some(properties) = schema["properties"].as_object() {
            for (key, child) in properties {
                if let Some(existing) = object.get_mut(key) {
                    *existing = materialize(child, existing);
                } else if let Some(default) = child.get("default").or_else(|| child.get("const")) {
                    object.insert(key.clone(), materialize(child, default));
                }
            }
        }
        if schema["additionalProperties"].is_object() {
            for (key, value) in object.iter_mut() {
                if schema["properties"].get(key).is_none() {
                    *value = materialize(&schema["additionalProperties"], value);
                }
            }
        }
    }
    if let Some(items) = value.as_array_mut() {
        for item in items {
            *item = materialize(&schema["items"], item);
        }
    }
    value
}
fn check_defaults(schema: &Value) -> Result<(), String> {
    if let Some(default) = schema.get("default") {
        let mut errors = vec![];
        validate_node(schema, default, "", &mut errors, 0);
        if !errors.is_empty() {
            return Err("schema default fails its own validation".into());
        }
    }
    if let Some(properties) = schema["properties"].as_object() {
        for child in properties.values() {
            check_defaults(child)?;
        }
    }
    for key in ["items", "additionalProperties"] {
        if schema[key].is_object() {
            check_defaults(&schema[key])?;
        }
    }
    Ok(())
}

/// Opt-in envelope over the existing opaque state field; WIT 1.0 stays unchanged.
pub fn settings_state(settings: &Value, state: Option<String>) -> Result<String, String> {
    let text = serde_json::to_string(
        &serde_json::json!({"$archivesift":1,"settings":settings,"state":state}),
    )
    .map_err(|_| "cannot encode settings")?;
    if text.len() > MAX_SETTINGS_BYTES {
        return Err("configured continuation exceeds 64 KiB".into());
    }
    Ok(text)
}
pub fn unpack_settings(input: &mut crate::Request) -> Result<Value, String> {
    let Some(state) = input.state.as_deref() else {
        return Ok(serde_json::json!({}));
    };
    let Ok(value) = serde_json::from_str::<Value>(state) else {
        return Ok(serde_json::json!({}));
    };
    if value.get("$archivesift").is_none() {
        return Ok(serde_json::json!({}));
    }
    if value["$archivesift"] != 1
        || !value["settings"].is_object()
        || (!value["state"].is_null() && !value["state"].is_string())
    {
        return Err("invalid settings envelope".into());
    }
    input.state = value["state"].as_str().map(String::from);
    Ok(value["settings"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn example() -> Value {
        json!({"type":"object","additionalProperties":false,"properties":{"quality":{"type":"string","enum":["preview","original"],"default":"original"},"limit":{"type":"integer","minimum":1,"maximum":20,"default":10},"labels":{"type":"array","items":{"type":"string","minLength":2},"maxItems":2,"uniqueItems":true}},"required":["quality","limit"]})
    }

    #[test]
    fn defaults_required_fields_types_and_unknown_properties_are_consistent() {
        let schema = SettingsSchema::compile(example()).unwrap();
        assert_eq!(schema.defaults(), json!({"quality":"original","limit":10}));
        assert_eq!(schema.validate(&json!({})).len(), 2);
        assert!(schema.validate(&schema.defaults()).is_empty());
        for value in [
            json!({"quality":"other","limit":1}),
            json!({"quality":"original","limit":21}),
            json!({"quality":"original","limit":1.5}),
            json!({"quality":"original","limit":true}),
            json!({"quality":"original","limit":1,"typo":true}),
        ] {
            assert!(!schema.validate(&value).is_empty());
        }
    }
    #[test]
    fn nested_arrays_use_pointer_paths_and_unicode_scalar_lengths() {
        let schema = SettingsSchema::compile(example()).unwrap();
        let errors =
            schema.validate(&json!({"quality":"original","limit":1,"labels":["я","я","third"]}));
        assert!(errors.iter().any(|e| e.path == "/labels/0"));
        assert!(errors.iter().any(|e| e.message == "duplicate array item"));
        assert!(schema
            .validate(&json!({"quality":"original","limit":1,"labels":["ёж"]}))
            .is_empty());
        assert_eq!(pointer_child("/x", "a/b~c"), "/x/a~1b~0c");
    }
    #[test]
    fn unsupported_schema_features_and_bad_defaults_fail_closed() {
        for value in [
            json!({"$ref":"https://example.test/schema","type":"object"}),
            json!({"type":"array"}),
            json!({"type":["string",null]}),
            json!({"type":["string","string"]}),
            json!({"type":"integer","minimum":5,"maximum":1}),
            json!({"type":"integer","maximum":5,"default":10}),
            json!({"type":"object","properties":{"password":{"type":"string"}}}),
            json!({"type":"object","required":["missing"]}),
        ] {
            assert!(SettingsSchema::compile(value).is_err());
        }
        assert!(SettingsSchema::compile(
            json!({"type":"string","description":"a".repeat(MAX_SCHEMA_BYTES)})
        )
        .is_err());
        let mut nested = json!({"type":"string"});
        for _ in 0..20 {
            nested = json!({"type":"array","items":nested});
        }
        assert!(SettingsSchema::compile(nested).is_err());
    }
    #[test]
    fn secret_references_are_constant_and_scoped_to_the_owning_plugin() {
        let make = |target: &str| json!({"type":"object","properties":{"api_key_ref":{"type":"string","format":"secret-reference","const":target,"default":target}}});
        let schema = SettingsSchema::compile(make("ArchiveSift/example.parser/api")).unwrap();
        assert!(schema.validate_plugin_identity("example.parser").is_ok());
        assert!(schema.validate_plugin_identity("other.parser").is_err());
        assert!(
            SettingsSchema::compile(make("ArchiveSift/example.parser/../other"))
                .unwrap()
                .validate_plugin_identity("example.parser")
                .is_err()
        );
        let errors = schema.validate(&json!({"api_key_ref":"fixture-sensitive-material"}));
        assert!(!errors.is_empty());
        assert!(!format!("{errors:?}").contains("fixture-sensitive-material"));
    }

    #[test]
    fn formats_apply_to_strings_and_credential_named_namespaces_can_contain_objects() {
        let schema=SettingsSchema::compile(json!({"type":"object","properties":{"token":{"type":"object","properties":{"enabled":{"type":"boolean"}}}}})).unwrap();
        assert!(schema
            .validate(&json!({"token":{"enabled":true}}))
            .is_empty());
        assert!(SettingsSchema::compile(json!({"type":"object","format":"uri"})).is_err());
        let uri = SettingsSchema::compile(json!({"type":"string","format":"uri"})).unwrap();
        assert!(uri.validate(&json!("https://example.test/path")).is_empty());
        assert!(uri.validate(&json!("urn:example:resource")).is_empty());
        assert!(!uri.validate(&json!("relative/path")).is_empty());
    }
    #[test]
    fn settings_envelope_keeps_opaque_continuations_and_supports_legacy_requests() {
        let settings = json!({"limit":3});
        let state = "opaque guest continuation".to_string();
        let mut request = crate::Request {
            api_version: crate::API_VERSION,
            operation: crate::Operation::Content,
            url: "https://example.test/".into(),
            response: None,
            state: Some(settings_state(&settings, Some(state.clone())).unwrap()),
        };
        assert_eq!(unpack_settings(&mut request).unwrap(), settings);
        assert_eq!(request.state, Some(state.clone()));
        assert_eq!(unpack_settings(&mut request).unwrap(), json!({}));
        assert_eq!(request.state, Some(state));
        request.state = Some("{\"$archivesift\":2}".into());
        assert!(unpack_settings(&mut request).is_err());
        assert!(settings_state(&json!({"big":"x".repeat(MAX_SETTINGS_BYTES)}), None).is_err());
    }
}
