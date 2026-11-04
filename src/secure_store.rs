//! OS-owned extension secrets. Values never enter configuration or plugin DTOs.
use std::fmt;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Secret(pub(crate) Zeroizing<Vec<u8>>);

impl Secret {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Exposes bytes to trusted host-side storage adapters. Never log this value.
    pub fn expose_secret(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, target: &str) -> Result<Option<Secret>, StoreError>;
    fn set(&self, target: &str, secret: &Secret) -> Result<(), StoreError>;
    fn delete(&self, target: &str) -> Result<(), StoreError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreError;

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cannot access the system password store. On Linux, install secret-tool and unlock the desktop keyring.")
    }
}
impl std::error::Error for StoreError {}

pub struct SystemSecretStore;

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::{HRESULT, PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn missing(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
    }

    impl SecretStore for SystemSecretStore {
        fn get(&self, target: &str) -> Result<Option<Secret>, StoreError> {
            let target = wide(target);
            let mut pointer: *mut CREDENTIALW = std::ptr::null_mut();
            unsafe {
                match CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0, &mut pointer) {
                    Err(error) if missing(&error) => return Ok(None),
                    Err(_) => return Err(StoreError),
                    Ok(()) => {}
                }
                let credential = &*pointer;
                let size = credential.CredentialBlobSize as usize;
                let result = if size == 0 || size > 8192 || credential.CredentialBlob.is_null() {
                    Err(StoreError)
                } else {
                    Ok(Some(Secret::new(
                        std::slice::from_raw_parts(credential.CredentialBlob, size).to_vec(),
                    )))
                };
                CredFree(pointer.cast());
                result
            }
        }

        fn set(&self, target: &str, secret: &Secret) -> Result<(), StoreError> {
            if secret.bytes().is_empty() || secret.bytes().len() > 2560 {
                return Err(StoreError);
            }
            let mut target = wide(target);
            let mut username = wide("ArchiveSift");
            let credential = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                UserName: PWSTR(username.as_mut_ptr()),
                CredentialBlobSize: secret.bytes().len() as u32,
                CredentialBlob: secret.bytes().as_ptr() as *mut u8,
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                ..Default::default()
            };
            unsafe { CredWriteW(&credential, 0).map_err(|_| StoreError) }
        }

        fn delete(&self, target: &str) -> Result<(), StoreError> {
            let target = wide(target);
            match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, 0) } {
                Ok(()) => Ok(()),
                Err(error) if missing(&error) => Ok(()),
                Err(_) => Err(StoreError),
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn command(operation: &str, target: &str) -> Command {
        let mut command = Command::new("secret-tool");
        command.arg(operation);
        if operation == "store" {
            command.args(["--label", "ArchiveSift extension credential"]);
        }
        command.args(["application", "ArchiveSift", "target", target]);
        command
    }

    impl SecretStore for SystemSecretStore {
        fn get(&self, target: &str) -> Result<Option<Secret>, StoreError> {
            let output = command("lookup", target).output().map_err(|_| StoreError)?;
            let mut bytes = Zeroizing::new(output.stdout);
            if !output.status.success() {
                return if output.status.code() == Some(1) && bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(StoreError)
                };
            }
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            Ok(Some(Secret::new(std::mem::take(&mut *bytes))))
        }
        fn set(&self, target: &str, secret: &Secret) -> Result<(), StoreError> {
            let mut child = command("store", target)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| StoreError)?;
            let write = child
                .stdin
                .take()
                .ok_or(StoreError)?
                .write_all(secret.bytes());
            let status = child.wait().map_err(|_| StoreError)?;
            if write.is_ok() && status.success() {
                Ok(())
            } else {
                Err(StoreError)
            }
        }
        fn delete(&self, target: &str) -> Result<(), StoreError> {
            let output = command("clear", target).output().map_err(|_| StoreError)?;
            if output.status.success() || output.status.code() == Some(1) {
                Ok(())
            } else {
                Err(StoreError)
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
impl SecretStore for SystemSecretStore {
    fn get(&self, _: &str) -> Result<Option<Secret>, StoreError> {
        Err(StoreError)
    }
    fn set(&self, _: &str, _: &Secret) -> Result<(), StoreError> {
        Err(StoreError)
    }
    fn delete(&self, _: &str) -> Result<(), StoreError> {
        Err(StoreError)
    }
}
