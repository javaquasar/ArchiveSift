//! Bounded, portable ZIP packages. Signatures cover every payload file and its name.
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read, Write},
    path::Path,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const MAX_PACKAGE: usize = 40 * 1024 * 1024;
const SIGNATURE: &str = "package-signature.json";
const DOMAIN: &[u8] = b"ArchiveSift package v1\n";

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub fn unhex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) || value.len() > 256 {
        return Err("invalid hexadecimal value".into());
    }
    (0..value.len())
        .step_by(2)
        .map(|i| {
            value
                .get(i..i + 2)
                .ok_or_else(|| "invalid hexadecimal value".into())
                .and_then(|s| Ok(u8::from_str_radix(s, 16)?))
        })
        .collect()
}
pub fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    schema: u32,
    files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signature {
    index: Index,
    public_key: String,
    signature: String,
}

#[derive(Clone)]
pub struct Package {
    pub files: BTreeMap<String, Vec<u8>>,
    /// Full public key, not a display-name assertion. Trust is granted locally.
    pub signer: Option<String>,
    pub artifact_digest: String,
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && !name.contains(['\\', ':'])
        && name
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.ends_with([' ', '.']))
}
fn message(index: &Index) -> Result<Vec<u8>> {
    let mut bytes = DOMAIN.to_vec();
    bytes.extend(serde_json::to_vec(index)?);
    Ok(bytes)
}

impl Package {
    pub fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_PACKAGE as u64 + 1)
            .read_to_end(&mut bytes)?;
        Self::decode(&bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PACKAGE {
            return Err("package exceeds 40 MiB".into());
        }
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
        if zip.len() > 24 {
            return Err("too many package files".into());
        }
        let mut files = BTreeMap::new();
        let mut names = BTreeSet::new();
        let mut total = 0;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i)?;
            let name = file.name().to_owned();
            if !safe_name(&name)
                || !names.insert(name.to_ascii_lowercase())
                || file.is_dir()
                || file
                    .unix_mode()
                    .is_some_and(|m| m & 0o170000 != 0 && m & 0o170000 != 0o100000)
            {
                return Err("unsafe, duplicate or non-regular package path".into());
            }
            let limit = if name.ends_with(".wasm") || name.ends_with(".exe") || name == "adapter" {
                32 * 1024 * 1024
            } else if name == "self-test.json" {
                1024 * 1024
            } else {
                64 * 1024
            };
            if file.size() > limit as u64 {
                return Err("package entry exceeds size limit".into());
            }
            let mut data = Vec::new();
            file.by_ref()
                .take(limit as u64 + 1)
                .read_to_end(&mut data)?;
            total += data.len();
            if data.len() > limit || total > MAX_PACKAGE {
                return Err("expanded package exceeds size limit".into());
            }
            files.insert(name, data);
        }
        let signer = if let Some(bytes) = files.remove(SIGNATURE) {
            let signed: Signature = serde_json::from_slice(&bytes)?;
            let expected = files
                .iter()
                .map(|(n, b)| (n.clone(), digest(b)))
                .collect::<BTreeMap<_, _>>();
            if signed.index.schema != 1 || signed.index.files != expected {
                return Err("signed file index mismatch".into());
            }
            let key = unhex(&signed.public_key)?;
            let sig = unhex(&signed.signature)?;
            signature::UnparsedPublicKey::new(&signature::ED25519, &key)
                .verify(&message(&signed.index)?, &sig)
                .map_err(|_| "invalid publisher signature")?;
            Some(hex(&key))
        } else {
            None
        };
        let manifest: serde_json::Value =
            serde_json::from_slice(files.get("plugin.json").ok_or("missing plugin.json")?)?;
        let entry = manifest["entry"].as_str().ok_or("missing module entry")?;
        if !safe_name(entry)
            || digest(files.get(entry).ok_or("missing module")?)
                != manifest["sha256"]
                    .as_str()
                    .ok_or("missing module checksum")?
        {
            return Err("module checksum mismatch".into());
        }
        let allowed = [
            "plugin.json",
            "settings.schema.json",
            "release.json",
            "account.json",
            "credential-bindings.json",
            "self-test.json",
            entry,
        ];
        if files.keys().any(|n| !allowed.contains(&n.as_str())) {
            return Err("unexpected package payload".into());
        }
        Ok(Self {
            files,
            signer,
            artifact_digest: digest(bytes),
        })
    }
}

/// The key is PKCS#8 Ed25519 data. Callers own its storage and redaction.
pub fn encode(files: &BTreeMap<String, Vec<u8>>, signing_key: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut files = files.clone();
    if files.contains_key(SIGNATURE) || files.keys().any(|n| !safe_name(n)) {
        return Err("invalid payload paths".into());
    }
    if let Some(key) = signing_key {
        let key = signature::Ed25519KeyPair::from_pkcs8(key).map_err(|_| "invalid signing key")?;
        let index = Index {
            schema: 1,
            files: files.iter().map(|(n, b)| (n.clone(), digest(b))).collect(),
        };
        let signature = hex(key.sign(&message(&index)?).as_ref());
        files.insert(
            SIGNATURE.into(),
            serde_json::to_vec(&Signature {
                index,
                public_key: hex(key.public_key().as_ref()),
                signature,
            })?,
        );
    }
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in files {
        writer.start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o600),
        )?;
        writer.write_all(&bytes)?;
    }
    let bytes = writer.finish()?.into_inner();
    Package::decode(&bytes)?;
    Ok(bytes)
}

pub fn generate_key() -> Result<Vec<u8>> {
    Ok(
        signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| "key generation failed")?
            .as_ref()
            .to_vec(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_symlinks_large_metadata_and_too_many_files_before_extraction() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink(
                "plugin.json",
                "outside",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        assert!(Package::decode(&writer.finish().unwrap().into_inner()).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "plugin.json",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(&vec![b' '; 65537]).unwrap();
        assert!(Package::decode(&writer.finish().unwrap().into_inner()).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for i in 0..25 {
            writer
                .start_file(
                    format!("{i}.json"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"{}").unwrap();
        }
        assert!(Package::decode(&writer.finish().unwrap().into_inner()).is_err());
    }
    fn files() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            ("adapter.wasm".into(), b"module".to_vec()),
            (
                "plugin.json".into(),
                serde_json::to_vec(
                    &serde_json::json!({"entry":"adapter.wasm","sha256":digest(b"module")}),
                )
                .unwrap(),
            ),
        ])
    }
    #[test]
    fn signed_round_trip_and_tamper_rejection() {
        let key = generate_key().unwrap();
        let bytes = encode(&files(), Some(&key)).unwrap();
        let package = Package::decode(&bytes).unwrap();
        assert!(package.signer.is_some());
        let mut changed = package.files.clone();
        changed.insert("account.json".into(), b"{}".to_vec());
        // Carry the original signature into a modified archive.
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let mut sig = Vec::new();
        archive
            .by_name(SIGNATURE)
            .unwrap()
            .read_to_end(&mut sig)
            .unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        changed.insert(SIGNATURE.into(), sig);
        for (n, b) in changed {
            writer
                .start_file(n, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&b).unwrap();
        }
        assert!(Package::decode(&writer.finish().unwrap().into_inner()).is_err());
    }
    #[test]
    fn refuses_traversal_duplicate_case_and_unknown_payload() {
        for names in [
            vec!["../escape"],
            vec!["plugin.json", "PLUGIN.JSON"],
            vec!["/absolute"],
            vec!["a\\b"],
            vec!["x:y"],
        ] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for n in names {
                writer
                    .start_file(n, zip::write::SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"{}").unwrap();
            }
            assert!(Package::decode(&writer.finish().unwrap().into_inner()).is_err());
        }
        let mut f = files();
        f.insert("unwanted.json".into(), b"{}".to_vec());
        assert!(encode(&f, None).is_err());
    }
}
