use std::path::PathBuf;

use sha2::{Digest, Sha256};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 4 || !matches!(args[0].as_str(), "wasm" | "process") {
        return Err("usage: archive-sift-plugin-packager <wasm|process> <input> <output-directory> <origin>".into());
    }
    let origin = url::Url::parse(&args[3])?;
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
    let manifest = serde_json::json!({
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
    });
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
