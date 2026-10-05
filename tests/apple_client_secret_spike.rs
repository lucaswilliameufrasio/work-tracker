//! Artificial key material only. No Apple request, product secret storage or
//! proof of Apple ID-token compatibility. ES256 here signs CLIENT credentials.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use openssl::{
    ec::{EcGroup, EcKey},
    ecdsa::EcdsaSig,
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    sign::Verifier,
};
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer},
    pkcs8::DecodePrivateKey,
};
use serde_json::{Value, json};

const NOW: i64 = 1_800_000_000;
const MAX_TTL: i64 = 15_777_000;
struct Config {
    team: String,
    kid: String,
    service: String,
    key: Vec<u8>,
}
// Deliberately no Debug: configuration holds private key bytes.
struct Secret {
    jwt: String,
    expires: i64,
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("jwt", &"[REDACTED]")
            .field("expires", &self.expires)
            .finish()
    }
}
#[derive(Debug, PartialEq, thiserror::Error)]
enum Failure {
    #[error("invalid signing configuration")]
    Config,
    #[error("invalid signing key")]
    Key,
    #[error("invalid lifetime")]
    Lifetime,
}
fn generate(config: &Config, now: i64, ttl: i64) -> Result<Secret, Failure> {
    let identifier = |s: &str| {
        s.len() == 10
            && s.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    };
    if !identifier(&config.team)
        || !identifier(&config.kid)
        || config.service.is_empty()
        || config.service.len() > 256
        || !config
            .service
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(Failure::Config);
    }
    // A shorter operational TTL is recommended; ceiling is not a default.
    if now < 0 || !(1..=MAX_TTL).contains(&ttl) {
        return Err(Failure::Lifetime);
    }
    let expires = now.checked_add(ttl).ok_or(Failure::Lifetime)?;
    if config.key.len() > 8192 {
        return Err(Failure::Key);
    }
    let key = SigningKey::from_pkcs8_der(&config.key).map_err(|_| Failure::Key)?;
    let header = json!({"alg":"ES256","kid":config.kid});
    let claims = json!({"iss":config.team,"sub":config.service,"aud":"https://appleid.apple.com","iat":now,"exp":expires});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature: Signature = key.sign(input.as_bytes());
    Ok(Secret {
        jwt: format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())),
        expires,
    })
}
fn renewal_due(secret: &Secret, now: i64, margin: i64) -> bool {
    // Fail closed if local renewal policy/clock is nonsensical.
    now < 0
        || margin < 0
        || now
            .checked_add(margin)
            .is_none_or(|threshold| threshold >= secret.expires)
}
fn fixture(curve: Nid) -> (Config, PKey<Private>) {
    let key =
        PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(curve).unwrap()).unwrap())
            .unwrap();
    (
        Config {
            team: "TEAM123456".into(),
            kid: "KEY1234567".into(),
            service: "com.example.puxiri.web".into(),
            key: key.private_key_to_pkcs8().unwrap(),
        },
        key,
    )
}
fn decode(segment: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segment).unwrap()).unwrap()
}
fn verify(jwt: &str, key: &PKey<Private>) -> bool {
    let parts: Vec<_> = jwt.split('.').collect();
    let bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
    // JOSE signatures are fixed-width R || S, NOT OpenSSL ASN.1 DER.
    let signature = Signature::from_slice(&bytes).unwrap();
    let der = signature.to_der();
    assert!(EcdsaSig::from_der(der.as_bytes()).is_ok());
    let mut verifier = Verifier::new(MessageDigest::sha256(), key).unwrap();
    verifier
        .update(format!("{}.{}", parts[0], parts[1]).as_bytes())
        .unwrap();
    verifier.verify(der.as_bytes()).unwrap()
}

#[test]
fn es256_pkcs8_key_produces_jose_signature_and_exact_client_claims_verified_independently() {
    let (config, key) = fixture(Nid::X9_62_PRIME256V1);
    let secret = generate(&config, NOW, 3600).unwrap();
    let parts: Vec<_> = secret.jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(decode(parts[0]), json!({"alg":"ES256","kid":config.kid}));
    assert_eq!(
        decode(parts[1]),
        json!({"iss":config.team,"sub":config.service,"aud":"https://appleid.apple.com","iat":NOW,"exp":NOW+3600})
    );
    assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 64);
    assert!(verify(&secret.jwt, &key));
    let (_, other) = fixture(Nid::X9_62_PRIME256V1);
    assert!(!verify(&secret.jwt, &other));
    let tampered = format!(
        "{}.{}.{}",
        parts[0],
        URL_SAFE_NO_PAD.encode(json!({"sub":"another-service"}).to_string()),
        parts[2]
    );
    assert!(!verify(&tampered, &key));
    assert!(!format!("{secret:?}").contains(&secret.jwt));
}

#[test]
fn rejects_wrong_curve_malformed_key_invalid_configuration_and_unsafe_lifetime_without_secret_errors()
 {
    let (mut config, _) = fixture(Nid::SECP384R1);
    assert_eq!(generate(&config, NOW, 3600).unwrap_err(), Failure::Key);
    config.key = b"artificial-invalid-private-key".to_vec();
    assert_eq!(
        generate(&config, NOW, 3600).unwrap_err().to_string(),
        "invalid signing key"
    );
    let (mut config, _) = fixture(Nid::X9_62_PRIME256V1);
    for (now, ttl) in [
        (NOW, 0),
        (NOW, -1),
        (NOW, MAX_TTL + 1),
        (-1, 3600),
        (i64::MAX, 1),
    ] {
        assert_eq!(generate(&config, now, ttl).unwrap_err(), Failure::Lifetime);
    }
    assert!(generate(&config, NOW, MAX_TTL).is_ok());
    config.team = "wrong".into();
    assert_eq!(generate(&config, NOW, 3600).unwrap_err(), Failure::Config);
    config.team = "TEAM123456".into();
    config.kid = "invalid kid".into();
    assert_eq!(generate(&config, NOW, 3600).unwrap_err(), Failure::Config);
    config.kid = "KEY1234567".into();
    config.service = "https://unapproved.invalid".into();
    assert_eq!(generate(&config, NOW, 3600).unwrap_err(), Failure::Config);
}

#[test]
fn renewal_boundary_regenerates_with_current_time_and_rotated_key_without_extending_old_credential()
{
    let (config, key) = fixture(Nid::X9_62_PRIME256V1);
    let old = generate(&config, NOW, 3600).unwrap();
    assert!(!renewal_due(&old, NOW + 3299, 300));
    assert!(renewal_due(&old, NOW + 3300, 300));
    assert!(renewal_due(&old, NOW + 3600, 0));
    assert!(renewal_due(&old, -1, 300));
    assert!(renewal_due(&old, NOW, -1));
    assert!(renewal_due(&old, i64::MAX, 1));
    let (mut rotated, new_key) = fixture(Nid::X9_62_PRIME256V1);
    rotated.kid = "NEW1234567".into();
    let new = generate(&rotated, NOW + 3300, 3600).unwrap();
    assert_eq!(old.expires, NOW + 3600);
    assert_eq!(new.expires, NOW + 6900);
    assert!(verify(&new.jwt, &new_key));
    assert!(!verify(&new.jwt, &key));
    assert!(verify(&old.jwt, &key)); // Local rotation cannot revoke bearer credentials already issued.
}
