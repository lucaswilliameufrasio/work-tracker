use std::{path::Path, sync::Arc};

use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Error)]
pub enum MasterKeyError {
    #[error("master key could not be read or stored safely")]
    Io(#[from] std::io::Error),
    #[error("master key file is not a private regular 32-byte key owned by this process")]
    InvalidFile,
    #[error("secure random master key generation failed")]
    RandomnessFailed,
    #[error("master-key subkey derivation failed")]
    SubkeyDerivationFailed,
    #[error("master key file operations are not supported on this platform")]
    UnsupportedPlatform,
}

#[derive(Clone)]
pub struct MasterKey(Arc<Zeroizing<[u8; 32]>>);

impl MasterKey {
    /// Returns a stable, non-secret identifier for key selection and rotation.
    pub fn key_id(&self) -> [u8; 16] {
        let mut digest = Sha256::new();
        digest.update(b"puxiri master key id v1\0");
        digest.update(self.0.as_slice());
        let digest = digest.finalize();
        let mut key_id = [0_u8; 16];
        key_id.copy_from_slice(&digest[..16]);
        key_id
    }

    pub(crate) fn keyed_digest(&self, namespace: &[u8], value: &[u8]) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.as_slice())
            .expect("HMAC-SHA-256 accepts keys of any length");
        mac.update(b"puxiri keyed identifier v1\0");
        mac.update(&(namespace.len() as u64).to_be_bytes());
        mac.update(namespace);
        mac.update(value);
        mac.finalize().into_bytes().into()
    }

    pub(crate) fn derive_muira_client_secret_key(
        &self,
    ) -> Result<Zeroizing<[u8; 32]>, MasterKeyError> {
        let salt = Salt::new(HKDF_SHA256, b"puxiri muira client secret extraction v1");
        let pseudorandom_key = salt.extract(self.0.as_slice());
        let output_key_material = pseudorandom_key
            .expand(&[b"puxiri muira client secret key v1"], HKDF_SHA256)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        let mut key = Zeroizing::new([0_u8; 32]);
        output_key_material
            .fill(&mut *key)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        Ok(key)
    }

    pub(crate) fn derive_mail_api_key(&self) -> Result<Zeroizing<[u8; 32]>, MasterKeyError> {
        let salt = Salt::new(HKDF_SHA256, b"puxiri mail api key extraction v1");
        let pseudorandom_key = salt.extract(self.0.as_slice());
        let output_key_material = pseudorandom_key
            .expand(&[b"puxiri mail api key v1"], HKDF_SHA256)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        let mut key = Zeroizing::new([0_u8; 32]);
        output_key_material
            .fill(&mut *key)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        Ok(key)
    }

    pub(crate) fn derive_totp_encryption_key(
        &self,
        user_id: &Uuid,
        credential_id: &Uuid,
    ) -> Result<Zeroizing<[u8; 32]>, MasterKeyError> {
        let salt = Salt::new(HKDF_SHA256, b"puxiri totp encryption extraction v1");
        let pseudorandom_key = salt.extract(self.0.as_slice());
        let info = [
            b"puxiri totp encryption key v1".as_slice(),
            user_id.as_bytes(),
            credential_id.as_bytes(),
        ];
        let output_key_material = pseudorandom_key
            .expand(&info, HKDF_SHA256)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        let mut key = Zeroizing::new([0_u8; 32]);
        output_key_material
            .fill(&mut *key)
            .map_err(|_| MasterKeyError::SubkeyDerivationFailed)?;
        Ok(key)
    }
}

pub struct MasterKeyStore;

impl MasterKeyStore {
    #[cfg(unix)]
    pub fn load_or_create(path: &Path) -> Result<MasterKey, MasterKeyError> {
        use std::{
            ffi::OsString,
            fs::{self, File, OpenOptions},
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };

        use rand_core::{OsRng, RngCore};
        use uuid::Uuid;

        if path.as_os_str().is_empty() || path.file_name().is_none() {
            return Err(MasterKeyError::InvalidFile);
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        validate_parent_directory(parent)?;
        if let Some(key) = load_existing(path)? {
            return Ok(key);
        }

        let mut key_bytes = Zeroizing::new([0_u8; 32]);
        OsRng
            .try_fill_bytes(&mut *key_bytes)
            .map_err(|_| MasterKeyError::RandomnessFailed)?;

        let mut temporary_name = OsString::from(".");
        temporary_name.push(path.file_name().ok_or(MasterKeyError::InvalidFile)?);
        temporary_name.push(format!(".{}.tmp", Uuid::now_v7()));
        let temporary_path = parent.join(temporary_name);

        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temporary_path)?;
        let _cleanup = TemporaryKeyFile::new(temporary_path.clone());
        temporary.set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(key_bytes.as_slice())?;
        temporary.sync_all()?;
        drop(temporary);

        match fs::hard_link(&temporary_path, path) {
            Ok(()) => {
                File::open(parent)?.sync_all()?;
                fs::remove_file(&temporary_path)?;
                File::open(parent)?.sync_all()?;
                let key: [u8; 32] = *key_bytes;
                Ok(MasterKey(Arc::new(Zeroizing::new(key))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                fs::remove_file(&temporary_path)?;
                load_existing(path)?.ok_or(MasterKeyError::InvalidFile)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                Err(error.into())
            }
        }
    }

    #[cfg(not(unix))]
    pub fn load_or_create(_path: &Path) -> Result<MasterKey, MasterKeyError> {
        Err(MasterKeyError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn validate_parent_directory(parent: &Path) -> Result<(), MasterKeyError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(parent)?;
    let effective_user = rustix::process::geteuid().as_raw();
    if !metadata.is_dir()
        || (metadata.uid() != effective_user && metadata.uid() != 0)
        || metadata.mode() & 0o022 != 0
    {
        return Err(MasterKeyError::InvalidFile);
    }
    Ok(())
}

#[cfg(unix)]
struct TemporaryKeyFile(std::path::PathBuf);

#[cfg(unix)]
impl TemporaryKeyFile {
    fn new(path: std::path::PathBuf) -> Self {
        Self(path)
    }
}

#[cfg(unix)]
impl Drop for TemporaryKeyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn load_existing(path: &Path) -> Result<Option<MasterKey>, MasterKeyError> {
    use std::{
        fs::OpenOptions,
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };

    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let metadata = file.metadata()?;
    let effective_user = rustix::process::geteuid().as_raw();
    let permissions = metadata.mode() & 0o777;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_user
        || !matches!(permissions, 0o400 | 0o600)
    {
        return Err(MasterKeyError::InvalidFile);
    }

    let mut contents = Zeroizing::new(Vec::with_capacity(33));
    file.by_ref().take(33).read_to_end(&mut contents)?;
    if contents.len() != 32 {
        return Err(MasterKeyError::InvalidFile);
    }
    let key: [u8; 32] = contents
        .as_slice()
        .try_into()
        .map_err(|_| MasterKeyError::InvalidFile)?;
    Ok(Some(MasterKey(Arc::new(Zeroizing::new(key)))))
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn creates_private_key_once_and_reuses_identical_material() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("master.key");

        let first = MasterKeyStore::load_or_create(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap().len(), 32);
        let second = MasterKeyStore::load_or_create(&path).unwrap();
        assert!(first.key_id() == second.key_id());
    }

    #[test]
    fn concurrent_creation_keeps_one_key_and_no_temporary_secret_files() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("master.key");
        let first_path = PathBuf::from(&path);
        let second_path = PathBuf::from(&path);

        let first = std::thread::spawn(move || MasterKeyStore::load_or_create(&first_path));
        let second = std::thread::spawn(move || MasterKeyStore::load_or_create(&second_path));
        let first = first.join().unwrap().unwrap();
        let second = second.join().unwrap().unwrap();

        assert!(first.key_id() == second.key_id());
        let entries = fs::read_dir(directory.path()).unwrap();
        let mut file_count = 0;
        for entry in entries {
            let entry = entry.unwrap();
            assert_eq!(entry.file_name(), "master.key");
            file_count += 1;
        }
        assert_eq!(file_count, 1);
    }

    #[test]
    fn refuses_wrong_length_or_permissive_key_files_without_overwriting() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("master.key");
        fs::write(&path, [7_u8; 31]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            MasterKeyStore::load_or_create(&path),
            Err(MasterKeyError::InvalidFile)
        ));
        assert_eq!(fs::read(&path).unwrap().len(), 31);

        fs::write(&path, [7_u8; 32]).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            MasterKeyStore::load_or_create(&path),
            Err(MasterKeyError::InvalidFile)
        ));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn refuses_group_or_world_writable_parent_directories() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("master.key");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777)).unwrap();

        assert!(matches!(
            MasterKeyStore::load_or_create(&path),
            Err(MasterKeyError::InvalidFile)
        ));
        assert!(!path.exists());

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symbolic_link_key_path() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let target = directory.path().join("target.key");
        let link = directory.path().join("master.key");
        fs::write(&target, [9_u8; 32]).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(MasterKeyStore::load_or_create(&link).is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap().len(), 32);
    }
}
