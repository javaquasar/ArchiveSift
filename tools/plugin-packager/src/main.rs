use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !matches!(args.len(), 4 | 5)
        || !matches!(args[0].as_str(), "wasm" | "process")
        || (args.len() == 5 && args[3] != "--manifest")
    {
        return Err("usage: archive-sift-plugin-packager <wasm|process> <input> <output-directory> <origin> OR <wasm|process> <input> <output-directory> --manifest <template.json>".into());
    }
    let directory = PathBuf::from(&args[2]);
    let input = std::fs::read(&args[1])?;
    let (entry, bytes) = if args[0] == "wasm" {
        (
            "adapter.wasm",
            wit_component::ComponentEncoder::default()
                .module(&input)?
                .validate(true)
                .encode()?,
        )
    } else {
        (
            if cfg!(windows) {
                "adapter.exe"
            } else {
                "adapter"
            },
            input,
        )
    };
    let manifest = if args.len() == 5 {
        let template = std::fs::read(&args[4])?;
        manifest_from_template(&template, &args[0], entry, &bytes)?
    } else {
        let origin = url::Url::parse(&args[3])?;
        serde_json::json!({
            "schema": 1, "id": format!("demo.{}", args[0]), "version": "0.1.0", "api": "1.0",
            "display_name": format!("Demo {} extension", args[0]), "source_key": format!("demo-{}", args[0]),
            "runtime": args[0], "entry": entry, "sha256": format!("{:x}", Sha256::digest(&bytes)),
            "enabled": false, "trusted": false,
            "routes": [
                {"host": origin.host_str().ok_or("origin requires host")?, "path_prefix": "/galleries/", "kind": "content"},
                {"host": origin.host_str().unwrap(), "path_prefix": "/channels/", "kind": "listing"},
                {"host": origin.host_str().unwrap(), "path_prefix": "/models/", "kind": "listing"}
            ],
            "network_origins": [origin.origin().ascii_serialization()], "allow_http": origin.scheme() == "http"
        })
    };
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join(entry), &bytes)?;
    #[cfg(unix)]
    if args[0] == "process" {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            directory.join(entry),
            std::fs::Permissions::from_mode(0o700),
        )?;
    }
    std::fs::write(
        directory.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!(
        "Created disabled extension package: {}",
        directory.display()
    );
    Ok(())
}

fn manifest_from_template(
    template: &[u8],
    runtime: &str,
    entry: &str,
    bytes: &[u8],
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    if template.len() > 64 * 1024 {
        return Err("manifest template exceeds 64 KiB".into());
    }
    let mut value: serde_json::Value = serde_json::from_slice(template)?;
    let object = value
        .as_object_mut()
        .ok_or("manifest template must be an object")?;
    let allowed = [
        "schema",
        "id",
        "version",
        "api",
        "display_name",
        "source_key",
        "runtime",
        "entry",
        "sha256",
        "enabled",
        "trusted",
        "routes",
        "network_origins",
        "allow_http",
        "limits",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unknown manifest template field".into());
    }
    for field in ["id", "version", "display_name", "source_key"] {
        if object
            .get(field)
            .and_then(|v| v.as_str())
            .is_none_or(|v| v.trim().is_empty())
        {
            return Err(format!("missing manifest identity field: {field}").into());
        }
    }
    if object.get("schema").and_then(|v| v.as_u64()) != Some(1)
        || object.get("api").and_then(|v| v.as_str()) != Some("1.0")
        || object
            .get("routes")
            .and_then(|v| v.as_array())
            .is_none_or(|v| v.is_empty())
        || object
            .get("network_origins")
            .and_then(|v| v.as_array())
            .is_none_or(|v| v.is_empty())
    {
        return Err("unsupported or incomplete manifest template".into());
    }
    for (field, replacement) in [
        ("runtime", serde_json::json!(runtime)),
        ("entry", serde_json::json!(entry)),
        (
            "sha256",
            serde_json::json!(format!("{:x}", Sha256::digest(bytes))),
        ),
        ("enabled", serde_json::json!(false)),
        ("trusted", serde_json::json!(false)),
    ] {
        object.insert(field.into(), replacement);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn template() -> serde_json::Value {
        serde_json::json!({"schema":1,"api":"1.0","id":"example.parser","version":"0.2.1","display_name":"Example","source_key":"example","routes":[{"host":"example.test","path_prefix":"/items/","kind":"content"}],"network_origins":["https://example.test"],"enabled":true,"trusted":true})
    }
    #[test]
    fn packages_arbitrary_identity_but_never_enables_or_trusts_it() {
        let result = manifest_from_template(
            &serde_json::to_vec(&template()).unwrap(),
            "wasm",
            "adapter.wasm",
            b"component",
        )
        .unwrap();
        assert_eq!(result["id"], "example.parser");
        assert_eq!(result["version"], "0.2.1");
        assert_eq!(
            result["sha256"],
            format!("{:x}", Sha256::digest(b"component"))
        );
        assert_eq!(result["enabled"], false);
        assert_eq!(result["trusted"], false);
    }
    #[test]
    fn rejects_incomplete_unknown_and_oversized_templates() {
        for field in ["id", "routes", "network_origins", "api"] {
            let mut value = template();
            value.as_object_mut().unwrap().remove(field);
            assert!(manifest_from_template(
                &serde_json::to_vec(&value).unwrap(),
                "wasm",
                "adapter.wasm",
                b"module"
            )
            .is_err());
        }
        let mut value = template();
        value["secret"] = "not-permitted".into();
        assert!(manifest_from_template(
            &serde_json::to_vec(&value).unwrap(),
            "wasm",
            "adapter.wasm",
            b"module"
        )
        .is_err());
        assert!(
            manifest_from_template(&vec![b' '; 65537], "wasm", "adapter.wasm", b"module").is_err()
        );
    }
}
