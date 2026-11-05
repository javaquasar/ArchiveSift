use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .first()
        .is_some_and(|a| matches!(a.as_str(), "bundle" | "keygen" | "verify"))
    {
        return package_command(&args).map_err(|e| e.to_string().into());
    }
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
    let schema = if args.len() == 5 {
        let path = std::path::Path::new(&args[4])
            .parent()
            .ok_or("template has no directory")?
            .join("settings.schema.json");
        if path.is_file() {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(archive_sift_scraper_api::settings_schema::MAX_SCHEMA_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > archive_sift_scraper_api::settings_schema::MAX_SCHEMA_BYTES {
                return Err("settings schema exceeds 64 KiB".into());
            }
            Some(bytes)
        } else {
            None
        }
    } else {
        Some(include_bytes!("../../../plugins/demo-parser/settings.schema.json").to_vec())
    };
    if let Some(bytes) = &schema {
        let compiled = archive_sift_scraper_api::settings_schema::SettingsSchema::compile(
            serde_json::from_slice(bytes)?,
        )?;
        compiled.validate_plugin_identity(manifest["id"].as_str().ok_or("missing plugin ID")?)?;
    }
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
    let schema_path = directory.join("settings.schema.json");
    if let Some(bytes) = schema {
        std::fs::write(schema_path, bytes)?;
    } else if schema_path.exists() {
        std::fs::remove_file(schema_path)?;
    }
    let self_test = if args.len() == 5 {
        std::path::Path::new(&args[4])
            .parent()
            .map(|p| p.join("self-test.json"))
            .filter(|p| p.is_file())
            .map(std::fs::read)
            .transpose()?
    } else {
        Some(include_bytes!("../../../plugins/demo-parser/self-test.json").to_vec())
    };
    if let Some(bytes) = self_test {
        std::fs::write(directory.join("self-test.json"), bytes)?;
    } else if directory.join("self-test.json").exists() {
        std::fs::remove_file(directory.join("self-test.json"))?;
    }
    println!(
        "Created disabled extension package: {}",
        directory.display()
    );
    Ok(())
}

fn package_command(args: &[String]) -> archive_sift_extension_package::Result<()> {
    use archive_sift_extension_package::{encode, generate_key, Package};
    use std::{collections::BTreeMap, fs::OpenOptions, io::Write};
    match args.first().map(String::as_str) {
        Some("keygen") if args.len()==2 => {
            let key=zeroize::Zeroizing::new(generate_key()?);
            let mut options=OpenOptions::new(); options.write(true).create_new(true);
            #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
            let mut file=options.open(&args[1])?; file.write_all(&key)?;
            println!("Publisher key created. Keep this file private and outside the repository."); Ok(())
        },
        Some("verify") if args.len()==2 => {
            let p=Package::read(std::path::Path::new(&args[1]))?;
            println!("SHA-256: {}\nPublisher: {}",p.artifact_digest,p.signer.as_deref().unwrap_or("unsigned")); Ok(())
        },
        Some("bundle") if matches!(args.len(),3|5) => {
            if args.len()==5 && args[3]!="--signing-key" {return Err("expected --signing-key <private PKCS8 file>".into());}
            let directory=std::path::Path::new(&args[1]);
            let manifest:serde_json::Value=serde_json::from_slice(&std::fs::read(directory.join("plugin.json"))?)?;
            let entry=manifest["entry"].as_str().ok_or("missing entry")?;
            if entry.contains(['\\',':']) || std::path::Path::new(entry).components().any(|c|!matches!(c,std::path::Component::Normal(_))) {return Err("invalid module entry path".into());}
            let mut files=BTreeMap::new();
            for name in ["plugin.json",entry,"settings.schema.json","account.json","credential-bindings.json","release.json","self-test.json"] {
                let path=directory.join(name);if path.is_file(){
                    if path.metadata()?.len()>archive_sift_extension_package::MAX_PACKAGE as u64 {return Err("file exceeds size limit".into());}
                    files.insert(name.to_owned(),std::fs::read(path)?);
                }
            }
            let key=if args.len()==5 {Some(zeroize::Zeroizing::new(std::fs::read(&args[4])?))}
                else if let Ok(value)=std::env::var("ARCHIVESIFT_SIGNING_KEY") {let value=zeroize::Zeroizing::new(value);Some(zeroize::Zeroizing::new(archive_sift_extension_package::unhex(value.trim())?))}else{None};
            let bytes=encode(&files,key.as_deref().map(|k|k.as_slice()))?;
            std::fs::write(&args[2],bytes)?;
            println!("Created {} .asift-plugin package",if key.is_some(){"signed"}else{"unsigned local"});Ok(())
        },
        _=>Err("usage: bundle <directory> <output.asift-plugin> [--signing-key <private-key>] | keygen <private-key> | verify <package>".into()),
    }
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
