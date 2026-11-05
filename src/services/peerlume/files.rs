use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::{Error, InfoHashes, Result};
use sha2::{Digest, Sha256};

const MAX_METAINFO: usize = 8 * 1024 * 1024;

fn no_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| Error::Staging)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Staging);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(Error::Staging);
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<Vec<u8>> {
    no_link(path)?;
    let file = File::open(path).map_err(|_| Error::Source)?;
    let metadata = file.metadata().map_err(|_| Error::Source)?;
    if !metadata.is_file() || metadata.len() > MAX_METAINFO as u64 {
        return Err(Error::Source);
    }
    let mut bytes = Vec::new();
    file.take(MAX_METAINFO as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Source)?;
    if bytes.is_empty() || bytes.len() > MAX_METAINFO {
        return Err(Error::Source);
    }
    Ok(bytes)
}

pub(super) fn read_archive(root: &Path, relative: &str) -> Result<Vec<u8>> {
    let root = root.canonicalize().map_err(|_| Error::Source)?;
    let path = crate::services::torrent_library::checked_path(&root, relative)
        .map_err(|_| Error::Source)?;
    let mut current = root.clone();
    for component in path
        .strip_prefix(&root)
        .map_err(|_| Error::Source)?
        .components()
    {
        current.push(component);
        no_link(&current).map_err(|_| Error::Source)?;
    }
    read(&path)
}

pub(super) fn hashes(bytes: &[u8]) -> Result<InfoHashes> {
    // Reuse the metainfo parser, then retain identities only. Tracker/passkey
    // query parameters are intentionally never copied to the handoff journal.
    let magnet = zeroize::Zeroizing::new(
        crate::services::attachments::torrent_magnet(bytes).map_err(|_| Error::Source)?,
    );
    let url = url::Url::parse(&magnet).map_err(|_| Error::Source)?;
    let mut hashes = InfoHashes::default();
    for (key, value) in url.query_pairs() {
        if key != "xt" {
            continue;
        }
        if let Some(value) = value.strip_prefix("urn:btih:") {
            hashes.v1 = Some(value.to_owned());
        }
        if let Some(value) = value.strip_prefix("urn:btmh:1220") {
            hashes.v2 = Some(value.to_owned());
        }
    }
    hashes.validate().map_err(|_| Error::Source)?;
    Ok(hashes)
}

fn import_dir(profile: &Path) -> Result<PathBuf> {
    let mut path = profile.canonicalize().map_err(|_| Error::Staging)?;
    for component in ["imports", "archivesift"] {
        path.push(component);
        match fs::create_dir(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(Error::Staging),
        }
        no_link(&path)?;
        if !path.is_dir() {
            return Err(Error::Staging);
        }
    }
    Ok(path)
}

pub(super) fn stage(profile: &Path, bytes: &[u8]) -> Result<(String, String)> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let directory = import_dir(profile)?;
    let name = format!("{digest}.torrent");
    let target = directory.join(&name);
    if target.exists() {
        if read(&target)? != bytes {
            return Err(Error::Staging);
        }
    } else {
        let temp = directory.join(format!(".{}.partial", super::random_id()?));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp).map_err(|_| Error::Staging)?;
            file.write_all(bytes).map_err(|_| Error::Staging)?;
            file.sync_all().map_err(|_| Error::Staging)?;
            drop(file);
            // Publishing by hard link never overwrites an existing file. A
            // competing preparer can win; its bytes still must match exactly.
            match fs::hard_link(&temp, &target) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(_) => return Err(Error::Staging),
            }
            if read(&target)? != bytes {
                return Err(Error::Staging);
            }
            Ok(())
        })();
        let _ = fs::remove_file(&temp);
        result?;
    }
    Ok((format!("archivesift/{name}"), digest))
}

pub(super) fn verify(profile: &Path, source: &str, digest: &str) -> Result<()> {
    // The source must be exactly a path produced by stage, not a caller path.
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || source != format!("archivesift/{digest}.torrent")
    {
        return Err(Error::Staging);
    }
    let path = import_dir(profile)?.join(format!("{digest}.torrent"));
    if format!("{:x}", Sha256::digest(read(&path)?)) != digest {
        return Err(Error::Staging);
    }
    Ok(())
}
