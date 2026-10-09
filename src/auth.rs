use std::sync::{Arc, OnceLock};

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::{Error as PasswordHashError, SaltString},
};
use rand_core::OsRng;
use thiserror::Error;
use tokio::{sync::Semaphore, task};
use zeroize::Zeroizing;

const PASSWORD_MIN_CHARS: usize = 10;
const PASSWORD_MAX_CHARS: usize = 128;
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_LANES: u32 = 1;
const ARGON2_OUTPUT_BYTES: usize = 32;
const ARGON2_MAX_ACTIVE: usize = 2;
const MAX_STORED_PASSWORD_HASH_BYTES: usize = 256;
const COMMON_PASSWORDS: &[&str] = &[
    "1234567890",
    "123456789a",
    "123456789q",
    "adminadmin",
    "changeme123",
    "iloveyou123",
    "letmein123",
    "passw0rd123",
    "password",
    "password1",
    "password123",
    "qwerty12345",
    "welcome123",
];

static PASSWORD_KDF_ADMISSION: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddress {
    pub address: String,
    pub key: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmailAddressError {
    #[error("email address is empty")]
    Empty,
    #[error("email address must contain one non-empty local part and domain")]
    Malformed,
    #[error("email address contains whitespace or control characters")]
    WhitespaceOrControl,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PasswordError {
    #[error("password does not meet the local password policy")]
    PolicyRejected,
    #[error("password hashing capacity is busy")]
    Busy,
    #[error("stored password hash is malformed or outside the supported profile")]
    InvalidStoredHash,
    #[error("password hashing worker failed")]
    WorkerFailed,
    #[error("password hashing failed")]
    HashingFailed,
}

#[derive(Clone)]
pub struct PasswordService {
    active_hashes: Arc<Semaphore>,
}

impl Default for PasswordService {
    fn default() -> Self {
        Self {
            active_hashes: PASSWORD_KDF_ADMISSION
                .get_or_init(|| Arc::new(Semaphore::new(ARGON2_MAX_ACTIVE)))
                .clone(),
        }
    }
}

impl PasswordService {
    pub async fn hash(&self, password: String) -> Result<String, PasswordError> {
        let password = Zeroizing::new(password);
        validate_password(password.as_str())?;
        let permit = self.try_acquire()?;

        task::spawn_blocking(move || {
            let _permit = permit;
            hash_password_blocking(password.as_str())
        })
        .await
        .map_err(|_| PasswordError::WorkerFailed)?
    }

    pub async fn verify(
        &self,
        password: String,
        encoded_hash: String,
    ) -> Result<bool, PasswordError> {
        let password = Zeroizing::new(password);
        let encoded_hash = Zeroizing::new(encoded_hash);
        if password.chars().count() > PASSWORD_MAX_CHARS
            || encoded_hash.len() > MAX_STORED_PASSWORD_HASH_BYTES
        {
            return if encoded_hash.len() > MAX_STORED_PASSWORD_HASH_BYTES {
                Err(PasswordError::InvalidStoredHash)
            } else {
                Ok(false)
            };
        }
        let parsed_hash = PasswordHash::new(encoded_hash.as_str())
            .map_err(|_| PasswordError::InvalidStoredHash)?;
        if !uses_supported_profile(&parsed_hash) {
            return Err(PasswordError::InvalidStoredHash);
        }

        let permit = self.try_acquire()?;
        task::spawn_blocking(move || {
            let _permit = permit;
            verify_password_blocking(password.as_str(), encoded_hash.as_str())
        })
        .await
        .map_err(|_| PasswordError::WorkerFailed)?
    }

    fn try_acquire(&self) -> Result<tokio::sync::OwnedSemaphorePermit, PasswordError> {
        self.active_hashes
            .clone()
            .try_acquire_owned()
            .map_err(|_| PasswordError::Busy)
    }
}

fn validate_password(password: &str) -> Result<(), PasswordError> {
    let character_count = password.chars().count();
    if !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&character_count) {
        return Err(PasswordError::PolicyRejected);
    }
    let folded_password = password.to_ascii_lowercase();
    if COMMON_PASSWORDS.contains(&folded_password.as_str()) {
        return Err(PasswordError::PolicyRejected);
    }
    Ok(())
}

fn password_hasher() -> Result<Argon2<'static>, PasswordError> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_LANES,
        Some(ARGON2_OUTPUT_BYTES),
    )
    .map_err(|_| PasswordError::HashingFailed)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}

fn hash_password_blocking(password: &str) -> Result<String, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    password_hasher()?
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| PasswordError::HashingFailed)
}

fn verify_password_blocking(password: &str, encoded_hash: &str) -> Result<bool, PasswordError> {
    let parsed_hash =
        PasswordHash::new(encoded_hash).map_err(|_| PasswordError::InvalidStoredHash)?;
    if !uses_supported_profile(&parsed_hash) {
        return Err(PasswordError::InvalidStoredHash);
    }
    match password_hasher()?.verify_password(password.as_bytes(), &parsed_hash) {
        Ok(()) => Ok(true),
        Err(PasswordHashError::Password) => Ok(false),
        Err(_) => Err(PasswordError::HashingFailed),
    }
}

fn uses_supported_profile(hash: &PasswordHash<'_>) -> bool {
    hash.algorithm.as_str() == "argon2id"
        && hash.version == Some(Version::V0x13 as u32)
        && hash.params.iter().count() == 3
        && hash.params.get_decimal("m") == Some(ARGON2_MEMORY_KIB)
        && hash.params.get_decimal("t") == Some(ARGON2_ITERATIONS)
        && hash.params.get_decimal("p") == Some(ARGON2_LANES)
        && hash.salt.as_ref().is_some_and(|salt| {
            let mut salt_bytes = [0_u8; 16];
            salt.decode_b64(&mut salt_bytes)
                .is_ok_and(|decoded| decoded.len() == 16)
        })
        && hash
            .hash
            .as_ref()
            .is_some_and(|output| output.as_bytes().len() == ARGON2_OUTPUT_BYTES)
}

pub fn normalize_email_address(value: &str) -> Result<EmailAddress, EmailAddressError> {
    let address = value.trim();
    if address.is_empty() {
        return Err(EmailAddressError::Empty);
    }
    if address.chars().any(char::is_whitespace) || address.chars().any(char::is_control) {
        return Err(EmailAddressError::WhitespaceOrControl);
    }
    let mut parts = address.split('@');
    let Some(local_part) = parts.next() else {
        return Err(EmailAddressError::Malformed);
    };
    let Some(domain) = parts.next() else {
        return Err(EmailAddressError::Malformed);
    };
    if local_part.is_empty() || domain.is_empty() || parts.next().is_some() {
        return Err(EmailAddressError::Malformed);
    }

    Ok(EmailAddress {
        address: address.to_owned(),
        key: address.to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_outer_whitespace_and_folds_ascii_case_without_aliasing_dots_or_plus_tags() {
        let normalized = normalize_email_address(" Alice.Example+tag@EXAMPLE.COM ").unwrap();
        assert_eq!(normalized.address, "Alice.Example+tag@EXAMPLE.COM");
        assert_eq!(normalized.key, "alice.example+tag@example.com");

        let dotted = normalize_email_address("aliceexample+tag@example.com").unwrap();
        let untagged = normalize_email_address("alice.example@example.com").unwrap();
        assert_ne!(normalized.key, dotted.key);
        assert_ne!(normalized.key, untagged.key);
    }

    #[test]
    fn rejects_empty_malformed_and_embedded_whitespace_addresses() {
        assert_eq!(
            normalize_email_address("  ").unwrap_err(),
            EmailAddressError::Empty
        );
        assert_eq!(
            normalize_email_address("local-only").unwrap_err(),
            EmailAddressError::Malformed
        );
        assert_eq!(
            normalize_email_address("a@@example.com").unwrap_err(),
            EmailAddressError::Malformed
        );
        assert_eq!(
            normalize_email_address("a b@example.com").unwrap_err(),
            EmailAddressError::WhitespaceOrControl
        );
    }

    #[tokio::test]
    async fn password_service_hashes_and_verifies_the_bounded_argon2id_profile() {
        let service = PasswordService::default();
        let password = String::from("a deliberately strong passphrase");
        let encoded_hash = service.hash(password.clone()).await.unwrap();
        let parsed_hash = PasswordHash::new(&encoded_hash).unwrap();

        assert!(uses_supported_profile(&parsed_hash));
        assert!(!encoded_hash.contains(&password));
        assert!(
            service
                .verify(password, encoded_hash.clone())
                .await
                .unwrap()
        );
        assert!(
            !service
                .verify(String::from("a different passphrase"), encoded_hash)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn password_policy_rejects_short_and_common_values_before_kdf() {
        let service = PasswordService::default();
        assert_eq!(
            service.hash(String::from("short")).await,
            Err(PasswordError::PolicyRejected)
        );
        assert_eq!(
            service.hash(String::from("PASSWORD123")).await,
            Err(PasswordError::PolicyRejected)
        );
        assert_eq!(
            service.hash("é".repeat(129)).await,
            Err(PasswordError::PolicyRejected)
        );
    }

    #[tokio::test]
    async fn verification_rejects_hostile_or_unknown_hash_profiles_before_kdf() {
        let service = PasswordService::default();
        let hostile_hash =
            String::from("$argon2id$v=19$m=4294967295,t=4294967295,p=1$c2FsdA$aGFzaA");
        assert_eq!(
            service
                .verify(String::from("some password"), hostile_hash)
                .await,
            Err(PasswordError::InvalidStoredHash)
        );
        assert_eq!(
            service
                .verify(
                    String::from("some password"),
                    String::from("not-a-PHC-hash")
                )
                .await,
            Err(PasswordError::InvalidStoredHash)
        );
        let undersized_salt = format!(
            "$argon2id$v=19$m={ARGON2_MEMORY_KIB},t={ARGON2_ITERATIONS},p={ARGON2_LANES}$c2FsdA${}",
            "A".repeat(43)
        );
        assert_eq!(
            service
                .verify(String::from("some password"), undersized_salt)
                .await,
            Err(PasswordError::InvalidStoredHash)
        );
    }

    #[test]
    fn kdf_admission_fails_fast_at_the_configured_concurrency_limit() {
        let service = PasswordService {
            active_hashes: Arc::new(Semaphore::new(ARGON2_MAX_ACTIVE)),
        };
        let first = service.try_acquire().unwrap();
        let second = service.try_acquire().unwrap();
        assert_eq!(service.try_acquire().err(), Some(PasswordError::Busy));
        drop(first);
        assert!(service.try_acquire().is_ok());
        drop(second);
    }
}
