use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use rand_core::{OsRng, RngCore};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

const TOTP_SECRET_BYTES: usize = 20;
const MAX_WINDOW_STEPS: u8 = 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TotpError {
    #[error("TOTP policy is invalid")]
    InvalidPolicy,
    #[error("TOTP secret is malformed or has an unsupported length")]
    InvalidSecret,
    #[error("secure TOTP secret generation failed")]
    RandomnessFailed,
}

#[derive(Clone, Copy)]
pub struct TotpPolicy {
    step_seconds: u32,
    digits: u8,
    past_window_steps: u8,
    future_window_steps: u8,
}

impl TotpPolicy {
    pub fn new(
        step_seconds: u32,
        digits: u8,
        past_window_steps: u8,
        future_window_steps: u8,
    ) -> Result<Self, TotpError> {
        if step_seconds == 0
            || !matches!(digits, 6..=8)
            || past_window_steps > MAX_WINDOW_STEPS
            || future_window_steps > MAX_WINDOW_STEPS
        {
            return Err(TotpError::InvalidPolicy);
        }
        Ok(Self {
            step_seconds,
            digits,
            past_window_steps,
            future_window_steps,
        })
    }

    /// RFC 6238's 30-second time step with interoperable six-digit codes and
    /// one step of tolerance in either direction for ordinary clock drift.
    pub fn authenticator_default() -> Self {
        Self {
            step_seconds: 30,
            digits: 6,
            past_window_steps: 1,
            future_window_steps: 1,
        }
    }
}

pub struct TotpSecret(Zeroizing<[u8; TOTP_SECRET_BYTES]>);

impl TotpSecret {
    pub fn generate() -> Result<Self, TotpError> {
        let mut secret = Zeroizing::new([0_u8; TOTP_SECRET_BYTES]);
        OsRng
            .try_fill_bytes(&mut *secret)
            .map_err(|_| TotpError::RandomnessFailed)?;
        Ok(Self(secret))
    }

    pub fn parse_base32(value: &str) -> Result<Self, TotpError> {
        let decoded = Zeroizing::new(
            base32::decode(base32::Alphabet::Rfc4648 { padding: false }, value)
                .ok_or(TotpError::InvalidSecret)?,
        );
        let canonical = Zeroizing::new(base32::encode(
            base32::Alphabet::Rfc4648 { padding: false },
            &decoded,
        ));
        if decoded.len() != TOTP_SECRET_BYTES || canonical.as_str() != value {
            return Err(TotpError::InvalidSecret);
        }
        let secret = decoded
            .as_slice()
            .try_into()
            .map_err(|_| TotpError::InvalidSecret)?;
        Ok(Self(Zeroizing::new(secret)))
    }

    /// Reveals enrollment material. Callers must display it once over the
    /// authenticated setup flow and must not log, cache, or persist this value.
    pub fn reveal_for_enrollment(&self) -> Zeroizing<String> {
        Zeroizing::new(base32::encode(
            base32::Alphabet::Rfc4648 { padding: false },
            self.0.as_slice(),
        ))
    }

    /// Returns the matched counter only for a correctly shaped code in the
    /// configured time window and strictly after the last accepted counter.
    /// The caller must persist the returned counter atomically with the
    /// protected action so a TOTP cannot be replayed.
    pub fn matching_step(
        &self,
        code: &str,
        now: DateTime<Utc>,
        last_accepted_step: Option<u64>,
        policy: TotpPolicy,
    ) -> Option<u64> {
        if code.len() != usize::from(policy.digits)
            || !code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }

        let timestamp = u64::try_from(now.timestamp()).ok()?;
        let current_step = timestamp / u64::from(policy.step_seconds);
        let first_step = current_step.saturating_sub(u64::from(policy.past_window_steps));
        let last_step = current_step.checked_add(u64::from(policy.future_window_steps))?;
        let mut matched_step = None;

        for step in first_step..=last_step {
            if last_accepted_step.is_some_and(|last| step <= last) {
                continue;
            }
            let expected = self.code_for_step(step, policy.digits);
            if bool::from(expected.as_bytes().ct_eq(code.as_bytes())) {
                matched_step = Some(matched_step.map_or(step, |matched: u64| matched.max(step)));
            }
        }
        matched_step
    }

    fn code_for_step(&self, step: u64, digits: u8) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(self.0.as_slice())
            .expect("HMAC-SHA-1 accepts keys of any length");
        mac.update(&step.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = usize::from(digest[19] & 0x0f);
        let binary = u32::from_be_bytes([
            digest[offset],
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ]) & 0x7fff_ffff;
        let modulus = 10_u32.pow(u32::from(digits));
        format!("{:0width$}", binary % modulus, width = usize::from(digits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn secret_from_ascii() -> TotpSecret {
        TotpSecret(Zeroizing::new(*b"12345678901234567890"))
    }

    fn at(unix_seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(unix_seconds, 0).single().unwrap()
    }

    #[test]
    fn matches_rfc_6238_sha1_test_vectors_including_year_2603() {
        let secret = secret_from_ascii();
        let policy = TotpPolicy::new(30, 8, 0, 0).unwrap();
        for (timestamp, expected) in [
            (59, "94287082"),
            (1_111_111_109, "07081804"),
            (1_111_111_111, "14050471"),
            (1_234_567_890, "89005924"),
            (2_000_000_000, "69279037"),
            (20_000_000_000, "65353130"),
        ] {
            let now = at(timestamp);
            let step = u64::try_from(timestamp).unwrap() / 30;
            assert_eq!(
                secret.matching_step(expected, now, None, policy),
                Some(step)
            );
        }
    }

    #[test]
    fn enrollment_secret_round_trips_and_rejects_noncanonical_or_wrong_length_input() {
        let generated = TotpSecret::generate().unwrap();
        let encoded = generated.reveal_for_enrollment();
        assert_eq!(encoded.len(), 32);
        assert!(TotpSecret::parse_base32(encoded.as_str()).is_ok());
        assert_eq!(
            TotpSecret::parse_base32(&encoded.to_ascii_lowercase())
                .err()
                .unwrap(),
            TotpError::InvalidSecret
        );
        assert_eq!(
            TotpSecret::parse_base32("MFRGGZDFMZTWQ2LK").err().unwrap(),
            TotpError::InvalidSecret
        );
    }

    #[test]
    fn matching_code_must_be_numeric_in_window_and_not_replayed() {
        let secret = secret_from_ascii();
        let policy = TotpPolicy::new(30, 8, 1, 1).unwrap();
        let now = at(59);
        assert_eq!(secret.matching_step("94287082", now, None, policy), Some(1));
        assert_eq!(secret.matching_step("94287082", now, Some(1), policy), None);
        assert_eq!(secret.matching_step("9428708x", now, None, policy), None);
        assert_eq!(secret.matching_step("9428708", now, None, policy), None);
        assert_eq!(secret.matching_step("94287082", at(-1), None, policy), None);
    }

    #[test]
    fn authenticator_profile_accepts_six_digit_codes_with_one_step_drift() {
        let secret = secret_from_ascii();
        let policy = TotpPolicy::authenticator_default();
        assert_eq!(
            secret.matching_step("287082", at(59), None, policy),
            Some(1)
        );
        assert_eq!(
            secret.matching_step("287082", at(60), None, policy),
            Some(1)
        );
    }

    #[test]
    fn policy_allows_only_supported_digits_and_one_step_of_clock_tolerance() {
        assert!(TotpPolicy::new(0, 6, 0, 0).is_err());
        assert!(TotpPolicy::new(30, 5, 0, 0).is_err());
        assert!(TotpPolicy::new(30, 9, 0, 0).is_err());
        assert!(TotpPolicy::new(30, 6, 2, 0).is_err());
        assert!(TotpPolicy::new(30, 6, 0, 2).is_err());
        assert_eq!(TotpPolicy::authenticator_default().step_seconds, 30);
    }
}
