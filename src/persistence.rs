use std::fs;
use std::path::{Path, PathBuf};

use crate::error::PersistenceError;

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), PersistenceError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|source| PersistenceError::CreateParentDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }

    let temp_path = temp_path_for(path);
    fs::write(&temp_path, contents).map_err(|source| PersistenceError::WriteTempFile {
        path: temp_path.clone(),
        source,
    })?;
    fs::rename(&temp_path, path).map_err(|source| PersistenceError::ReplaceFile {
        from: temp_path.clone(),
        to: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "tmp".into());
    file_name.push(".tmp");
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_file_contents() {
        let unique = format!(
            "archivesift_atomic_write_{}.txt",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);

        fs::write(&path, b"old").expect("seed file should be writable");
        atomic_write(&path, b"new").expect("atomic write should succeed");

        let written = fs::read_to_string(&path).expect("written file should be readable");
        assert_eq!(written, "new");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(temp_path_for(&path));
    }
}
