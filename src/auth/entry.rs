use std::future::Future;
use std::pin::Pin;

use aws_lc_rs::aead::{AES_256_GCM, Aad, NONCE_LEN, Nonce, RandomizedNonceKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, TimeDelta, Utc};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    access::OrganizationRole,
    audit::{self, AuditEventKind, AuditEventResult, AuditEventTarget},
    invitations::accept_pending_invitation_for_email,
    organization_timezone::OrganizationTimezone,
};

use super::{
    PasswordService,
    master_key::{MasterKey, MasterKeyError},
    normalize_email_address,
    session::{IssuedSession, SessionStore},
    setup::{SetupToken, SetupTokenStore},
    time::format_timestamp,
};

const ATTEMPT_LIFETIME: TimeDelta = TimeDelta::minutes(10);
const MAX_CLIENT_SECRET_BYTES: usize = 512;

#[derive(Debug, Error)]
pub enum EntryError {
    #[error("installation setup is not available")]
    SetupUnavailable,
    #[error("installation login is not configured")]
    LoginUnavailable,
    #[error("Muira identity was rejected")]
    IdentityRejected,
    #[error("entry persistence failed")]
    Database(#[from] sqlx::Error),
    #[error("entry secret could not be protected")]
    Secret(#[from] MasterKeyError),
    #[error("password setup failed")]
    Password(#[from] super::PasswordError),
    #[error("session setup failed")]
    Session(#[from] super::session::SessionError),
    #[error("invitation acceptance failed")]
    Invitation(#[from] crate::invitations::OrganizationInvitationError),
    #[error("audit setup failed")]
    Audit(#[from] crate::audit::AuditEventError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedMuiraIdentity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
}

pub struct MuiraExchangeRequest {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Zeroizing<String>,
    pub redirect_uri: String,
    pub code: String,
    pub verifier: String,
    pub nonce: String,
}

pub type MuiraExchangeFuture =
    Pin<Box<dyn Future<Output = Result<VerifiedMuiraIdentity, EntryError>> + Send>>;

pub trait MuiraCodeExchange: Send + Sync {
    fn exchange(&self, request: MuiraExchangeRequest) -> MuiraExchangeFuture;
}

pub struct DisabledMuiraExchange;

impl MuiraCodeExchange for DisabledMuiraExchange {
    fn exchange(&self, _request: MuiraExchangeRequest) -> MuiraExchangeFuture {
        Box::pin(async { Err(EntryError::LoginUnavailable) })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LoginSource {
    Puxiri,
    Muira,
}

pub struct SetupStatus {
    pub setup_required: bool,
    pub login: Option<LoginSource>,
}

pub struct MuiraAuthorization {
    pub url: String,
}

pub struct BuiltinSetup<'a> {
    pub organization_name: &'a str,
    pub locale: &'a str,
    pub timezone: &'a str,
    pub display_name: &'a str,
    pub email: &'a str,
    pub password: String,
}

pub struct MuiraInstallation<'a> {
    pub organization_name: &'a str,
    pub locale: &'a str,
    pub timezone: &'a str,
    pub display_name: &'a str,
    pub email: &'a str,
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub redirect_uri: &'a str,
}

struct OwnerInsert<'a> {
    organization_name: &'a str,
    locale: &'a str,
    timezone: &'a str,
    display_name: &'a str,
    email: &'a str,
    email_key: &'a str,
    password_hash: Option<&'a str>,
    now: DateTime<Utc>,
}

pub enum MuiraLoginOutcome {
    Session(IssuedSession),
    InvitationRequired,
}

pub struct InstallationEntry {
    pool: sqlx::SqlitePool,
    master_key: MasterKey,
    passwords: PasswordService,
    sessions: SessionStore,
}

impl InstallationEntry {
    pub fn new(
        pool: sqlx::SqlitePool,
        master_key: MasterKey,
        passwords: PasswordService,
        sessions: SessionStore,
    ) -> Self {
        Self {
            pool,
            master_key,
            passwords,
            sessions,
        }
    }

    pub async fn status(&self) -> Result<SetupStatus, EntryError> {
        let row = sqlx::query(
            "SELECT installation_state.setup_completed_at, installation_login.source \
             FROM installation_state \
             LEFT JOIN installation_login ON installation_login.singleton = installation_state.singleton \
             WHERE installation_state.singleton = 1",
        )
        .fetch_one(&self.pool)
        .await?;
        let setup_required = row
            .try_get::<Option<String>, _>("setup_completed_at")?
            .is_none();
        let login = match row.try_get::<Option<String>, _>("source")?.as_deref() {
            Some("puxiri") => Some(LoginSource::Puxiri),
            Some("muira") => Some(LoginSource::Muira),
            Some(_) => return Err(EntryError::LoginUnavailable),
            None => None,
        };
        Ok(SetupStatus {
            setup_required,
            login,
        })
    }

    pub async fn complete_puxiri_setup(
        &self,
        token: &SetupToken,
        setup: BuiltinSetup<'_>,
        now: DateTime<Utc>,
    ) -> Result<IssuedSession, EntryError> {
        let BuiltinSetup {
            organization_name,
            locale,
            timezone,
            display_name,
            email,
            password,
        } = setup;
        let organization_name = required_text(organization_name)?;
        let locale = required_locale(locale)?;
        let timezone = required_timezone(timezone)?;
        let display_name = required_text(display_name)?;
        let email = normalize_email_address(email).map_err(|_| EntryError::SetupUnavailable)?;
        let password_hash = self.passwords.hash(password).await?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        SetupTokenStore::consume_in_transaction(&mut transaction, &token.digest(), now)
            .await
            .map_err(|_| EntryError::SetupUnavailable)?;
        let user_id = insert_owner(
            &mut transaction,
            OwnerInsert {
                organization_name: &organization_name,
                locale: &locale,
                timezone: &timezone,
                display_name: &display_name,
                email: &email.address,
                email_key: &email.key,
                password_hash: Some(&password_hash),
                now,
            },
        )
        .await?;
        sqlx::query("INSERT INTO installation_login (singleton, source) VALUES (1, 'puxiri')")
            .execute(&mut *transaction)
            .await?;
        let session = self
            .sessions
            .issue_for_active_user_in_transaction(&mut transaction, &user_id, now)
            .await?;
        transaction.commit().await?;
        Ok(session)
    }

    pub async fn begin_muira_setup(
        &self,
        token: &SetupToken,
        installation: MuiraInstallation<'_>,
        now: DateTime<Utc>,
    ) -> Result<MuiraAuthorization, EntryError> {
        let MuiraInstallation {
            organization_name,
            locale,
            timezone,
            display_name,
            email,
            issuer,
            client_id,
            client_secret,
            redirect_uri,
        } = installation;
        let organization_name = required_text(organization_name)?;
        let locale = required_locale(locale)?;
        let timezone = required_timezone(timezone)?;
        let display_name = required_text(display_name)?;
        let email = normalize_email_address(email).map_err(|_| EntryError::SetupUnavailable)?;
        let issuer = normalize_issuer(issuer)?;
        let client_id = required_text(client_id)?;
        let redirect_uri = required_text(redirect_uri)?;
        if !SetupTokenStore::new(self.pool.clone())
            .is_current(token)
            .await
            .map_err(|_| EntryError::SetupUnavailable)?
        {
            return Err(EntryError::SetupUnavailable);
        }
        let envelope = encrypt_client_secret(&self.master_key, client_secret)?;
        let attempt = new_attempt().map_err(|_| EntryError::SetupUnavailable)?;
        let expires_at = now
            .checked_add_signed(ATTEMPT_LIFETIME)
            .ok_or(EntryError::SetupUnavailable)?;
        sqlx::query(
            "INSERT INTO muira_login_attempts ( \
                state_hash, purpose, nonce, verifier, organization_name, locale, timezone_name, \
                display_name, owner_email, owner_email_key, setup_token_hash, \
                client_secret_envelope, issuer, client_id, expires_at, created_at \
             ) VALUES (?, 'setup', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(attempt.state_hash.to_vec())
        .bind(&attempt.nonce)
        .bind(attempt.verifier.as_str())
        .bind(&organization_name)
        .bind(&locale)
        .bind(&timezone)
        .bind(&display_name)
        .bind(&email.address)
        .bind(&email.key)
        .bind(token.digest().to_vec())
        .bind(envelope.as_slice())
        .bind(&issuer)
        .bind(&client_id)
        .bind(format_timestamp(expires_at))
        .bind(format_timestamp(now))
        .execute(&self.pool)
        .await?;
        Ok(MuiraAuthorization {
            url: authorize_url(
                &issuer,
                &client_id,
                &redirect_uri,
                &attempt.state,
                &attempt.nonce,
                &attempt.challenge,
            ),
        })
    }

    pub async fn begin_muira_login(
        &self,
        redirect_uri: &str,
        now: DateTime<Utc>,
    ) -> Result<MuiraAuthorization, EntryError> {
        let redirect_uri = required_text(redirect_uri)?;
        let config = load_muira_config(&self.pool).await?;
        let attempt = new_attempt().map_err(|_| EntryError::LoginUnavailable)?;
        let expires_at = now
            .checked_add_signed(ATTEMPT_LIFETIME)
            .ok_or(EntryError::LoginUnavailable)?;
        sqlx::query(
            "INSERT INTO muira_login_attempts ( \
                state_hash, purpose, nonce, verifier, expires_at, created_at \
             ) VALUES (?, 'login', ?, ?, ?, ?)",
        )
        .bind(attempt.state_hash.to_vec())
        .bind(&attempt.nonce)
        .bind(attempt.verifier.as_str())
        .bind(format_timestamp(expires_at))
        .bind(format_timestamp(now))
        .execute(&self.pool)
        .await?;
        Ok(MuiraAuthorization {
            url: authorize_url(
                &config.issuer,
                &config.client_id,
                &redirect_uri,
                &attempt.state,
                &attempt.nonce,
                &attempt.challenge,
            ),
        })
    }

    /// Checks the installation password credential. A missing account still
    /// spends one password hash so the response time does not reveal it.
    pub async fn login_puxiri(
        &self,
        email: &str,
        password: String,
        now: DateTime<Utc>,
    ) -> Result<IssuedSession, EntryError> {
        let email = normalize_email_address(email).map_err(|_| EntryError::LoginUnavailable)?;
        let source = sqlx::query("SELECT source FROM installation_login WHERE singleton = 1")
            .fetch_optional(&self.pool)
            .await?;
        let configured = match source {
            Some(row) => row.try_get::<String, _>("source")? == "puxiri",
            None => false,
        };
        if !configured {
            self.passwords.hash(password).await?;
            return Err(EntryError::LoginUnavailable);
        }
        let account = sqlx::query(
            "SELECT users.id, password_credentials.password_hash \
             FROM user_emails \
             JOIN users ON users.id = user_emails.user_id \
             JOIN password_credentials ON password_credentials.user_id = users.id \
             JOIN organization_members \
               ON organization_members.organization_id = 1 \
              AND organization_members.user_id = users.id \
             WHERE user_emails.address_key = ? \
               AND user_emails.confirmed_at IS NOT NULL \
               AND users.state = 'active'",
        )
        .bind(&email.key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(account) = account else {
            self.passwords.hash(password).await?;
            return Err(EntryError::LoginUnavailable);
        };
        let user_id: String = account.try_get("id")?;
        let password_hash: String = account.try_get("password_hash")?;
        if !self.passwords.verify(password, password_hash).await? {
            return Err(EntryError::LoginUnavailable);
        }
        Ok(self.sessions.issue(&user_id, now).await?)
    }

    pub async fn finish_muira_callback(
        &self,
        state: &str,
        identity: &VerifiedMuiraIdentity,
        now: DateTime<Utc>,
    ) -> Result<MuiraLoginOutcome, EntryError> {
        let state_hash = sha256(state.as_bytes());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let attempt = sqlx::query(
            "SELECT purpose, nonce, organization_name, locale, timezone_name, display_name, \
                    owner_email, owner_email_key, setup_token_hash, client_secret_envelope, \
                    issuer, client_id, expires_at \
             FROM muira_login_attempts WHERE state_hash = ?",
        )
        .bind(state_hash.to_vec())
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(attempt) = attempt else {
            transaction.rollback().await?;
            return Err(EntryError::IdentityRejected);
        };
        let expires_at: String = attempt.try_get("expires_at")?;
        if expires_at <= format_timestamp(now) {
            sqlx::query("DELETE FROM muira_login_attempts WHERE state_hash = ?")
                .bind(state_hash.to_vec())
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            return Err(EntryError::IdentityRejected);
        }
        let purpose: String = attempt.try_get("purpose")?;
        let outcome = if purpose == "setup" {
            let issuer: String = attempt.try_get("issuer")?;
            if identity.issuer != issuer || !identity.email_verified {
                return Err(close_rejected_attempt(transaction, &state_hash).await);
            }
            let Some(raw_email) = identity.email.as_deref() else {
                return Err(close_rejected_attempt(transaction, &state_hash).await);
            };
            let email = match normalize_email_address(raw_email) {
                Ok(email) => email,
                Err(_) => return Err(close_rejected_attempt(transaction, &state_hash).await),
            };
            let expected_key: String = attempt.try_get("owner_email_key")?;
            if email.key != expected_key {
                return Err(close_rejected_attempt(transaction, &state_hash).await);
            }
            let setup_token_hash: Vec<u8> = attempt.try_get("setup_token_hash")?;
            if SetupTokenStore::consume_in_transaction(&mut transaction, &setup_token_hash, now)
                .await
                .is_err()
            {
                transaction.rollback().await?;
                return Err(EntryError::SetupUnavailable);
            }
            let organization_name: String = attempt.try_get("organization_name")?;
            let locale: String = attempt.try_get("locale")?;
            let timezone: String = attempt.try_get("timezone_name")?;
            let display_name: String = attempt.try_get("display_name")?;
            let user_id = insert_owner(
                &mut transaction,
                OwnerInsert {
                    organization_name: &organization_name,
                    locale: &locale,
                    timezone: &timezone,
                    display_name: &display_name,
                    email: &email.address,
                    email_key: &email.key,
                    password_hash: None,
                    now,
                },
            )
            .await?;
            insert_muira_identity(
                &mut transaction,
                &user_id,
                &identity.issuer,
                &identity.subject,
                now,
            )
            .await?;
            let envelope: Vec<u8> = attempt.try_get("client_secret_envelope")?;
            sqlx::query(
                "INSERT INTO installation_login \
                 (singleton, source, muira_issuer, muira_client_id, muira_client_secret_envelope) \
                 VALUES (1, 'muira', ?, ?, ?)",
            )
            .bind(&issuer)
            .bind(attempt.try_get::<String, _>("client_id")?)
            .bind(envelope)
            .execute(&mut *transaction)
            .await?;
            MuiraLoginOutcome::Session(
                self.sessions
                    .issue_for_active_user_in_transaction(&mut transaction, &user_id, now)
                    .await?,
            )
        } else {
            let config = load_muira_config_in_transaction(&mut transaction).await?;
            if identity.issuer != config.issuer {
                return Err(close_rejected_attempt(transaction, &state_hash).await);
            }
            let existing = sqlx::query(
                "SELECT users.id, \
                        EXISTS(SELECT 1 FROM organization_members \
                               WHERE organization_members.user_id = users.id) AS member \
                 FROM auth_identities \
                 JOIN users ON users.id = auth_identities.user_id \
                 WHERE auth_identities.provider = 'muira' \
                   AND auth_identities.issuer = ? AND auth_identities.subject = ? \
                   AND users.state = 'active'",
            )
            .bind(&identity.issuer)
            .bind(&identity.subject)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(existing) = existing {
                let member: i64 = existing.try_get("member")?;
                if member != 1 {
                    return finish_without_access(transaction, &state_hash).await;
                }
                let user_id: String = existing.try_get("id")?;
                MuiraLoginOutcome::Session(
                    self.sessions
                        .issue_for_active_user_in_transaction(&mut transaction, &user_id, now)
                        .await?,
                )
            } else if identity.email_verified {
                let Some(raw_email) = identity.email.as_deref() else {
                    return finish_without_access(transaction, &state_hash).await;
                };
                let email = match normalize_email_address(raw_email) {
                    Ok(email) => email,
                    Err(_) => return Err(close_rejected_attempt(transaction, &state_hash).await),
                };
                let user_id = Uuid::now_v7().to_string();
                let now_text = format_timestamp(now);
                sqlx::query(
                    "INSERT INTO users (id, display_name, state, created_at, updated_at) \
                     VALUES (?, ?, 'active', ?, ?)",
                )
                .bind(&user_id)
                .bind(email.address.as_str())
                .bind(&now_text)
                .bind(&now_text)
                .execute(&mut *transaction)
                .await?;
                let accepted = accept_pending_invitation_for_email(
                    &mut transaction,
                    &user_id,
                    &email.key,
                    now,
                )
                .await?;
                if !accepted {
                    transaction.rollback().await?;
                    sqlx::query("DELETE FROM muira_login_attempts WHERE state_hash = ?")
                        .bind(state_hash.to_vec())
                        .execute(&self.pool)
                        .await?;
                    return Ok(MuiraLoginOutcome::InvitationRequired);
                }
                sqlx::query(
                    "INSERT INTO user_emails \
                     (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
                     VALUES (?, ?, ?, ?, 1, ?, ?)",
                )
                .bind(Uuid::now_v7().to_string())
                .bind(&user_id)
                .bind(&email.address)
                .bind(&email.key)
                .bind(&now_text)
                .bind(&now_text)
                .execute(&mut *transaction)
                .await?;
                insert_muira_identity(
                    &mut transaction,
                    &user_id,
                    &identity.issuer,
                    &identity.subject,
                    now,
                )
                .await?;
                MuiraLoginOutcome::Session(
                    self.sessions
                        .issue_for_active_user_in_transaction(&mut transaction, &user_id, now)
                        .await?,
                )
            } else {
                return finish_without_access(transaction, &state_hash).await;
            }
        };
        sqlx::query("DELETE FROM muira_login_attempts WHERE state_hash = ?")
            .bind(state_hash.to_vec())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(outcome)
    }

    pub(crate) async fn exchange_materials(
        &self,
        state: &str,
    ) -> Result<MuiraCallbackMaterial, EntryError> {
        let state_hash = sha256(state.as_bytes());
        let row = sqlx::query(
            "SELECT attempts.purpose, attempts.verifier, attempts.nonce, attempts.issuer, \
                    attempts.client_id, attempts.client_secret_envelope, \
                    login.muira_issuer, login.muira_client_id, login.muira_client_secret_envelope \
             FROM muira_login_attempts AS attempts \
             LEFT JOIN installation_login AS login ON login.singleton = 1 \
             WHERE attempts.state_hash = ?",
        )
        .bind(state_hash.to_vec())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(EntryError::IdentityRejected)?;
        let purpose: String = row.try_get("purpose")?;
        let verifier = Zeroizing::new(row.try_get("verifier")?);
        let nonce: String = row.try_get("nonce")?;
        if purpose == "setup" {
            let issuer: String = row.try_get("issuer")?;
            let client_id: String = row.try_get("client_id")?;
            let envelope: Vec<u8> = row.try_get("client_secret_envelope")?;
            let client_secret = decrypt_client_secret(&self.master_key, &envelope)?;
            return Ok(MuiraCallbackMaterial {
                issuer,
                client_id,
                client_secret,
                verifier,
                nonce,
            });
        }
        let issuer: String = row
            .try_get::<Option<String>, _>("muira_issuer")?
            .ok_or(EntryError::LoginUnavailable)?;
        let client_id: String = row
            .try_get::<Option<String>, _>("muira_client_id")?
            .ok_or(EntryError::LoginUnavailable)?;
        let envelope: Vec<u8> = row
            .try_get::<Option<Vec<u8>>, _>("muira_client_secret_envelope")?
            .ok_or(EntryError::LoginUnavailable)?;
        let client_secret = decrypt_client_secret(&self.master_key, &envelope)?;
        Ok(MuiraCallbackMaterial {
            issuer,
            client_id,
            client_secret,
            verifier,
            nonce,
        })
    }
}

pub(crate) struct MuiraCallbackMaterial {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Zeroizing<String>,
    pub verifier: Zeroizing<String>,
    pub nonce: String,
}

struct AttemptSecrets {
    state: String,
    state_hash: [u8; 32],
    nonce: String,
    verifier: Zeroizing<String>,
    challenge: String,
}

fn new_attempt() -> Result<AttemptSecrets, super::SessionTokenError> {
    let state_token = super::SessionToken::generate()?;
    let state = state_token.expose().to_owned();
    let mut verifier_bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut verifier_bytes);
    let verifier = Zeroizing::new(URL_SAFE_NO_PAD.encode(verifier_bytes));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut nonce_bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut nonce_bytes);
    Ok(AttemptSecrets {
        state_hash: sha256(state.as_bytes()),
        state,
        nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
        verifier,
        challenge,
    })
}

fn authorize_url(
    issuer: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    challenge: &str,
) -> String {
    let mut url = format!("{issuer}/oauth/authorize?response_type=code&scope=openid%20email");
    for (name, value) in [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ] {
        url.push('&');
        url.push_str(name);
        url.push('=');
        url.push_str(&urlencoding_minimal(value));
    }
    url
}

fn urlencoding_minimal(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn sha256(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

fn required_text(value: &str) -> Result<String, EntryError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(EntryError::SetupUnavailable);
    }
    Ok(value.to_owned())
}

fn required_locale(value: &str) -> Result<String, EntryError> {
    if matches!(value, "pt-BR" | "en") {
        Ok(value.to_owned())
    } else {
        Err(EntryError::SetupUnavailable)
    }
}

fn required_timezone(value: &str) -> Result<String, EntryError> {
    OrganizationTimezone::parse(value).map_err(|_| EntryError::SetupUnavailable)?;
    Ok(value.to_owned())
}

fn normalize_issuer(value: &str) -> Result<String, EntryError> {
    let value = value.trim().trim_end_matches('/');
    if !value.starts_with("https://") || value.contains([' ', '?']) {
        return Err(EntryError::SetupUnavailable);
    }
    Ok(value.to_owned())
}

struct MuiraConfig {
    issuer: String,
    client_id: String,
}

async fn load_muira_config(pool: &sqlx::SqlitePool) -> Result<MuiraConfig, EntryError> {
    let mut connection = pool.acquire().await?;
    load_muira_config_in_transaction(&mut connection).await
}

async fn load_muira_config_in_transaction(
    executor: &mut sqlx::SqliteConnection,
) -> Result<MuiraConfig, EntryError> {
    let row = sqlx::query(
        "SELECT source, muira_issuer, muira_client_id FROM installation_login WHERE singleton = 1",
    )
    .fetch_optional(&mut *executor)
    .await?;
    let Some(row) = row else {
        return Err(EntryError::LoginUnavailable);
    };
    if row.try_get::<String, _>("source")? != "muira" {
        return Err(EntryError::LoginUnavailable);
    }
    Ok(MuiraConfig {
        issuer: row
            .try_get::<Option<String>, _>("muira_issuer")?
            .ok_or(EntryError::LoginUnavailable)?,
        client_id: row
            .try_get::<Option<String>, _>("muira_client_id")?
            .ok_or(EntryError::LoginUnavailable)?,
    })
}

async fn insert_owner(
    transaction: &mut Transaction<'_, Sqlite>,
    owner: OwnerInsert<'_>,
) -> Result<String, EntryError> {
    let OwnerInsert {
        organization_name,
        locale,
        timezone,
        display_name,
        email,
        email_key,
        password_hash,
        now,
    } = owner;
    let now_text = format_timestamp(now);
    let user_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO organizations (id, name, locale, timezone, created_at, updated_at) \
         VALUES (1, ?, ?, ?, ?, ?)",
    )
    .bind(organization_name)
    .bind(locale)
    .bind(timezone)
    .bind(&now_text)
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO users (id, display_name, state, created_at, updated_at) \
         VALUES (?, ?, 'active', ?, ?)",
    )
    .bind(&user_id)
    .bind(display_name)
    .bind(&now_text)
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;
    let email_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO user_emails \
         (id, user_id, address, address_key, is_primary, confirmed_at, created_at) \
         VALUES (?, ?, ?, ?, 1, ?, ?)",
    )
    .bind(&email_id)
    .bind(&user_id)
    .bind(email)
    .bind(email_key)
    .bind(&now_text)
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO organization_members (organization_id, user_id, role, created_at) \
         VALUES (1, ?, 'owner', ?)",
    )
    .bind(&user_id)
    .bind(&now_text)
    .execute(&mut **transaction)
    .await?;
    sqlx::query("INSERT INTO installation_admins (user_id, created_at) VALUES (?, ?)")
        .bind(&user_id)
        .bind(&now_text)
        .execute(&mut **transaction)
        .await?;
    if let Some(password_hash) = password_hash {
        sqlx::query(
            "INSERT INTO password_credentials (user_id, password_hash, changed_at) VALUES (?, ?, ?)",
        )
        .bind(&user_id)
        .bind(password_hash)
        .bind(&now_text)
        .execute(&mut **transaction)
        .await?;
    }
    audit::append_in_transaction(
        transaction,
        Some(&user_id),
        AuditEventKind::EmailAddressAdded,
        AuditEventResult::Success,
        &AuditEventTarget::email_address(&email_id),
        now,
    )
    .await?;
    audit::append_in_transaction(
        transaction,
        Some(&user_id),
        AuditEventKind::EmailAddressConfirmed,
        AuditEventResult::Success,
        &AuditEventTarget::email_address(&email_id),
        now,
    )
    .await?;
    audit::append_in_transaction(
        transaction,
        Some(&user_id),
        AuditEventKind::OrganizationMemberAdded,
        AuditEventResult::Success,
        &AuditEventTarget::organization_membership(&user_id, None, Some(OrganizationRole::Owner)),
        now,
    )
    .await?;
    Ok(user_id)
}

async fn insert_muira_identity(
    transaction: &mut Transaction<'_, Sqlite>,
    user_id: &str,
    issuer: &str,
    subject: &str,
    now: DateTime<Utc>,
) -> Result<(), EntryError> {
    sqlx::query(
        "INSERT INTO auth_identities (id, user_id, provider, issuer, subject, created_at) \
         VALUES (?, ?, 'muira', ?, ?, ?)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(user_id)
    .bind(issuer)
    .bind(subject)
    .bind(format_timestamp(now))
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

fn encrypt_client_secret(master_key: &MasterKey, secret: &str) -> Result<Vec<u8>, EntryError> {
    if secret.is_empty() || secret.len() > MAX_CLIENT_SECRET_BYTES {
        return Err(EntryError::SetupUnavailable);
    }
    let key_material = master_key.derive_muira_client_secret_key()?;
    let cipher = RandomizedNonceKey::new(&AES_256_GCM, key_material.as_slice())
        .map_err(|_| EntryError::Secret(MasterKeyError::SubkeyDerivationFailed))?;
    let key_id = master_key.key_id();
    let mut ciphertext = Zeroizing::new(secret.as_bytes().to_vec());
    let nonce = cipher
        .seal_in_place_append_tag(Aad::from(secret_aad(&key_id)), &mut *ciphertext)
        .map_err(|_| EntryError::Secret(MasterKeyError::SubkeyDerivationFailed))?;
    let mut envelope = Vec::with_capacity(1 + 16 + NONCE_LEN + ciphertext.len());
    envelope.push(1);
    envelope.extend_from_slice(&key_id);
    envelope.extend_from_slice(nonce.as_ref());
    envelope.extend_from_slice(ciphertext.as_slice());
    Ok(envelope)
}

fn decrypt_client_secret(
    master_key: &MasterKey,
    envelope: &[u8],
) -> Result<Zeroizing<String>, EntryError> {
    if envelope.first() != Some(&1) || envelope.len() < 1 + 16 + NONCE_LEN + 16 {
        return Err(EntryError::LoginUnavailable);
    }
    let key_id = master_key.key_id();
    if envelope[1..17] != key_id {
        return Err(EntryError::LoginUnavailable);
    }
    let key_material = master_key.derive_muira_client_secret_key()?;
    let cipher = RandomizedNonceKey::new(&AES_256_GCM, key_material.as_slice())
        .map_err(|_| EntryError::Secret(MasterKeyError::SubkeyDerivationFailed))?;
    let nonce_bytes: &[u8; NONCE_LEN] = envelope[17..17 + NONCE_LEN]
        .try_into()
        .map_err(|_| EntryError::LoginUnavailable)?;
    let nonce =
        Nonce::try_assume_unique_for_key(nonce_bytes).map_err(|_| EntryError::LoginUnavailable)?;
    let mut plaintext = Zeroizing::new(envelope[17 + NONCE_LEN..].to_vec());
    let opened = cipher
        .open_in_place(nonce, Aad::from(secret_aad(&key_id)), &mut plaintext)
        .map_err(|_| EntryError::LoginUnavailable)?;
    let secret = String::from_utf8(opened.to_vec()).map_err(|_| EntryError::LoginUnavailable)?;
    Ok(Zeroizing::new(secret))
}

fn secret_aad(key_id: &[u8; 16]) -> Vec<u8> {
    let mut aad = b"puxiri muira client secret envelope v1\0".to_vec();
    aad.extend_from_slice(key_id);
    aad
}

async fn close_rejected_attempt(
    mut transaction: Transaction<'_, Sqlite>,
    state_hash: &[u8],
) -> EntryError {
    if let Err(error) = sqlx::query("DELETE FROM muira_login_attempts WHERE state_hash = ?")
        .bind(state_hash.to_vec())
        .execute(&mut *transaction)
        .await
    {
        let _ = transaction.rollback().await;
        return EntryError::Database(error);
    }
    if let Err(error) = transaction.commit().await {
        return EntryError::Database(error);
    }
    EntryError::IdentityRejected
}

async fn finish_without_access(
    mut transaction: Transaction<'_, Sqlite>,
    state_hash: &[u8],
) -> Result<MuiraLoginOutcome, EntryError> {
    sqlx::query("DELETE FROM muira_login_attempts WHERE state_hash = ?")
        .bind(state_hash.to_vec())
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(MuiraLoginOutcome::InvitationRequired)
}
